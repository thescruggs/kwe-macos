// SPDX-License-Identifier: GPL-3.0-or-later
//! Daemon-side live wallpaper apply transactions (BETA_M4a).
//!
//! `wallpaper.apply` is the daemon half of "set this wallpaper on that
//! screen": it validates the request, starts the wallpaper's renderer
//! through the supervisor, waits (bounded) for the renderer to reach a
//! live phase, records the assignment with the previous wallpaper config
//! as `previous`, and finally switches the Plasma desktop's wallpaper
//! plugin through a bounded `qdbus` script call. The transaction completes
//! on PROMOTION (Live or AwaitingAck), not on the display handshake ack —
//! the ack comes later from the live wallpaper bridge (DisplaySession) and
//! is a display concern, not an apply concern. Any step failing rolls back
//! what the transaction already did and answers `apply_failed`.
//!
//! The Plasma switch never embeds wallpaper content. The script builders
//! here are pure functions of validated identity parts (desktop index,
//! plugin name, config group, image path): plugin and config-group members
//! must match the identity charset (`validate_identity_part`), the image
//! path is JS-string-escaped and size-bounded, and the `wallpaper_id` /
//! `content` fields never reach the script at all. `qdbus` is spawned
//! directly (no shell) with the script passed as an argument, bounded to
//! 64 KiB of output, and killed at the probe deadline.
//!
//! Assignments live in `assignments-v1.json` beside `supervisor-v1.json`
//! in the daemon's private state directory (bounded to 16 outputs and
//! 1 MiB, written atomically, quarantined on corruption exactly like the
//! grants store). `wallpaper.restore` reverts the saved `previous` config;
//! when no assignment exists it resets to `org.kde.image` with a stock
//! image path present on this system — the safe-mode contract is that
//! restore never leaves a desktop assigned to a daemon-owned renderer.
//!
//! Live probes: output enumeration reads `kscreen-doctor -o` (connector
//! names, enabled/connected state, geometry) and one `evaluateScript` call
//! that prints per-desktop state (index, id, screen, wallpaper plugin, and
//! the current plugin's `Image` config value). Results are cached for 5 s
//! (`wallpaper.outputs`), never indefinitely, and every probe is bounded.
//! The shell service name defaults to the Plasma 6 name `org.kde.plasmashell`
//! (the Plasma 5-era `org.kde.PlasmaShell` alias does not exist on 6).
//!
//! Scripts must never call `desktopForScreen`: on Plasma 6 a -1 screen
//! argument (an orphaned, screen-less desktop containment) SIGSEGVs the
//! shell (verified on 6.7.4). Desktop lookup is by `desktops()` array index
//! resolved through the connector -> screen -> desktop mapping above.

use std::{
    collections::{BTreeMap, BTreeSet},
    fs::OpenOptions,
    io::{ErrorKind, Read},
    os::unix::fs::OpenOptionsExt,
    os::unix::io::AsRawFd,
    path::{Path, PathBuf},
    process::{Command, ExitStatus, Stdio},
    sync::{Arc, Mutex, MutexGuard, RwLock},
    time::{Duration, Instant},
};

use anyhow::{Result, bail};
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};
use sha2::{Digest, Sha256};

use kwe_core::{Catalog, CatalogItem, ProjectKind};

use crate::persist::{atomic_write, ensure_private_dir, quarantine_invalid_state, unix_seconds};
use crate::playlist_session::{PlaylistApplyLane, foreign_renderer_live};
use crate::supervisor::{
    ContentSpec, RendererKind, ScalingMode, StartSpec, SupervisorHandle, WorkerPhase,
    validate_identity_part,
};

const ASSIGNMENTS_FILE: &str = "assignments-v1.json";
/// Bounded assignment map: one record per output, hard-capped before the
/// 1 MiB byte bound (mirrors the grants store's count bound).
pub(crate) const MAX_ASSIGNED_OUTPUTS: usize = 16;
const MAX_ASSIGNMENT_BYTES: u64 = 1024 * 1024;
/// Wallpaper content paths live on disk; the stored string is advisory.
const MAX_CONTENT_CHARS: usize = 4096;
const MAX_IMAGE_CHARS: usize = 4096;
/// Every probe (qdbus script call, kscreen-doctor) is capped at 64 KiB of
/// stdout/stderr; oversize output is truncated, never buffered unbounded.
const MAX_PROBE_OUTPUT_BYTES: usize = 64 * 1024;
/// Output enumeration freshness window; never cached indefinitely.
const OUTPUT_CACHE_TTL: Duration = Duration::from_secs(5);
/// Promotion poll cadence while waiting for the renderer to go live.
const PROMOTION_POLL: Duration = Duration::from_millis(200);

/// The wallpaper plugin the daemon assigns to desktops it manages.
const KWE_PLUGIN: &str = "org.kde.kwe.wallpaper";
/// The stock plugin restores fall back to when no assignment exists.
const IMAGE_PLUGIN: &str = "org.kde.image";
/// Stock images tried in order for the assignment-less restore; the first
/// present on this system wins. All absent is still a valid restore (the
/// image plugin falls back to its theme default).
const STOCK_IMAGE_CANDIDATES: &[&str] = &[
    "/usr/share/wallpapers/cachyos-wallpapers/Cachy depths 5K.png",
    "/usr/share/wallpapers/cachyos-wallpapers/Abstract.png",
    "/usr/share/wallpapers/Next/contents/images/Next.jpg",
];

/// The wallpaper config that was live on an output before an apply, saved
/// so `wallpaper.restore` can revert it.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct PreviousWallpaper {
    pub wallpaper_plugin: String,
    /// The plugin's wallpaper config group, e.g. `["Wallpaper",
    /// "org.kde.image", "General"]`.
    pub config_group: Vec<String>,
    /// The plugin's `Image` config value (null when the plugin has none).
    pub image: Option<String>,
}

/// One applied wallpaper on one output. `kind`/`content`/dims mirror the
/// StartSpec that rendered it; `previous` is what restore reverts to.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Assignment {
    pub wallpaper_id: String,
    pub kind: RendererKind,
    pub content: String,
    pub width: u32,
    pub height: u32,
    pub fps: u32,
    /// F1: how the picture maps onto the output. Additive: records written
    /// before F1 read back as `aspect`, the only behaviour they had.
    #[serde(default)]
    pub scaling: ScalingMode,
    pub applied_at_unix_seconds: u64,
    pub previous: Option<PreviousWallpaper>,
}

#[derive(Debug, Clone, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
struct PersistedAssignments {
    schema_version: u32,
    #[serde(default)]
    outputs: BTreeMap<String, Assignment>,
}

impl Default for PersistedAssignments {
    fn default() -> Self {
        Self {
            schema_version: 1,
            outputs: BTreeMap::new(),
        }
    }
}

/// Validates an assignment record and its output key. Identity parts use
/// the same charset as every other daemon identity; the renderer kind is
/// restricted to the apply-able kinds (test wallpapers are not assignable).
fn validate_assignment(output_name: &str, assignment: &Assignment) -> Result<()> {
    validate_identity_part("output", output_name)?;
    validate_identity_part("wallpaper_id", &assignment.wallpaper_id)?;
    if assignment.kind == RendererKind::Test {
        bail!("assignment kind must be video, web, or scene");
    }
    if assignment.content.is_empty() || assignment.content.len() > MAX_CONTENT_CHARS {
        bail!("assignment content must be a bounded content path");
    }
    if assignment.width == 0
        || assignment.width > 8192
        || assignment.height == 0
        || assignment.height > 8192
    {
        bail!("assignment dimensions must be in 1..=8192");
    }
    if !(1..=240).contains(&assignment.fps) {
        bail!("assignment fps must be in 1..=240");
    }
    if let Some(previous) = &assignment.previous {
        validate_identity_part("wallpaper_plugin", &previous.wallpaper_plugin)?;
        if previous.config_group.is_empty() || previous.config_group.len() > 4 {
            bail!("assignment config_group must be a bounded config group");
        }
        for part in &previous.config_group {
            validate_identity_part("config_group", part)?;
        }
        if let Some(image) = &previous.image
            && (image.is_empty() || image.len() > MAX_IMAGE_CHARS)
        {
            bail!("assignment image must be a bounded image path");
        }
    }
    Ok(())
}

/// The assignments file on disk plus its in-memory state. Mutations persist
/// atomically and only commit once the write succeeded, mirroring
/// `GrantStore`.
pub struct AssignmentStore {
    path: PathBuf,
    state: PersistedAssignments,
}

impl AssignmentStore {
    /// Opens (or creates) the assignments file in `directory`. A corrupt
    /// file — oversize, unparsable, unsupported schema, an unknown field,
    /// an invalid output name, an invalid record, or more than
    /// `MAX_ASSIGNED_OUTPUTS` records — is quarantined to
    /// `<file>.invalid-<unix_seconds>` and the store starts fresh.
    pub fn open(directory: &Path) -> Result<Self> {
        ensure_private_dir(directory)?;
        let path = directory.join(ASSIGNMENTS_FILE);
        let state = Self::load(&path);
        Ok(Self { path, state })
    }

    /// The stored assignment for one output, if any.
    pub fn get(&self, output: &str) -> Option<&Assignment> {
        self.state.outputs.get(output)
    }

    /// Every stored assignment, bounded by `MAX_ASSIGNED_OUTPUTS`.
    pub fn all(&self) -> &BTreeMap<String, Assignment> {
        &self.state.outputs
    }

    /// Inserts (or replaces) the assignment for `output` and persists it
    /// atomically; the in-memory state only commits after the write.
    pub fn set(&mut self, output: &str, assignment: Assignment) -> Result<()> {
        validate_assignment(output, &assignment)?;
        if !self.state.outputs.contains_key(output)
            && self.state.outputs.len() >= MAX_ASSIGNED_OUTPUTS
        {
            bail!("assignment count exceeds the {MAX_ASSIGNED_OUTPUTS} output safety limit");
        }
        let mut next_state = self.state.clone();
        next_state.outputs.insert(output.to_string(), assignment);
        self.save(&next_state)?;
        self.state = next_state;
        Ok(())
    }

    /// Removes the assignment for `output`, persisting atomically. Returns
    /// whether a record existed.
    pub fn remove(&mut self, output: &str) -> Result<bool> {
        if !self.state.outputs.contains_key(output) {
            return Ok(false);
        }
        let mut next_state = self.state.clone();
        next_state.outputs.remove(output);
        self.save(&next_state)?;
        self.state = next_state;
        Ok(true)
    }

    fn save(&self, state: &PersistedAssignments) -> Result<()> {
        let bytes = serde_json::to_vec_pretty(state)?;
        if bytes.len() as u64 > MAX_ASSIGNMENT_BYTES {
            bail!("assignments state exceeds {MAX_ASSIGNMENT_BYTES} bytes");
        }
        atomic_write(&self.path, &bytes)
    }

    fn load(path: &Path) -> PersistedAssignments {
        let mut file = match OpenOptions::new()
            .read(true)
            .custom_flags(libc::O_CLOEXEC | libc::O_NOFOLLOW)
            .open(path)
        {
            Ok(file) => file,
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
                return PersistedAssignments::default();
            }
            Err(error) => {
                Self::quarantine(path, &format!("open failed: {error}"));
                return PersistedAssignments::default();
            }
        };
        let metadata = match file.metadata() {
            Ok(metadata) => metadata,
            Err(error) => {
                Self::quarantine(path, &format!("metadata failed: {error}"));
                return PersistedAssignments::default();
            }
        };
        if !metadata.is_file() || metadata.len() > MAX_ASSIGNMENT_BYTES {
            Self::quarantine(path, "not a bounded regular file");
            return PersistedAssignments::default();
        }
        let mut bytes = Vec::with_capacity(metadata.len() as usize);
        if let Err(error) = file.read_to_end(&mut bytes) {
            Self::quarantine(path, &format!("read failed: {error}"));
            return PersistedAssignments::default();
        }
        match serde_json::from_slice::<PersistedAssignments>(&bytes) {
            Ok(state)
                if state.schema_version == 1
                    && state.outputs.len() <= MAX_ASSIGNED_OUTPUTS
                    && state.outputs.iter().all(|(name, assignment)| {
                        validate_assignment(name, assignment).is_ok()
                    }) =>
            {
                state
            }
            Ok(_) => {
                Self::quarantine(
                    path,
                    "unsupported schema, unknown field, or invalid assignment",
                );
                PersistedAssignments::default()
            }
            Err(error) => {
                Self::quarantine(path, &format!("parse failed: {error}"));
                PersistedAssignments::default()
            }
        }
    }

    fn quarantine(path: &Path, reason: &str) {
        eprintln!(
            "event=assignments.state_invalid detail=assignments-v1.json is corrupt ({reason}); quarantining and starting fresh"
        );
        quarantine_invalid_state(path);
    }
}

// ---------------------------------------------------------------------------
// Plasma script generation (pure; never embeds wallpaper content)
// ---------------------------------------------------------------------------

/// `wallpaper.apply` switch script: a pure function of the desktop array
/// index and the validated plugin name. The desktop guard makes a stale
/// index fail visibly (`evaluateScript` reports success even on script
/// exceptions, so without the guard a stale index would silently switch
/// nothing); the daemon's post-switch verification probe is the second
/// line of defense.
pub fn apply_script(desktop_index: usize, plugin: &str) -> Result<String, String> {
    validate_identity_part("wallpaper_plugin", plugin).map_err(|error| error.to_string())?;
    Ok(format!(
        "var d = desktops()[{desktop_index}]; if (!d) throw \"no desktop {desktop_index}\"; d.wallpaperPlugin = \"{plugin}\";"
    ))
}

/// `wallpaper.restore` script: re-selects the saved config group, writes the
/// saved `Image` when one exists (a null image writes nothing), and assigns
/// the saved plugin. Every interpolated value is validated (plugin and group
/// members are identity parts; the image is JS-escaped and size-bounded).
pub fn restore_script(
    desktop_index: usize,
    plugin: &str,
    config_group: &[String],
    image: Option<&str>,
) -> Result<String, String> {
    validate_identity_part("wallpaper_plugin", plugin).map_err(|error| error.to_string())?;
    if config_group.is_empty() || config_group.len() > 4 {
        return Err("config_group must be a bounded config group".into());
    }
    let mut group = String::new();
    for (index, part) in config_group.iter().enumerate() {
        validate_identity_part("config_group", part).map_err(|error| error.to_string())?;
        if index > 0 {
            group.push_str(", ");
        }
        group.push('"');
        group.push_str(part);
        group.push('"');
    }
    let mut script = format!(
        "var d = desktops()[{desktop_index}];\nif (!d) throw \"no desktop {desktop_index}\";\n"
    );
    script.push_str(&format!("d.currentConfigGroup = [{group}];\n"));
    if let Some(image) = image {
        if image.is_empty() || image.len() > MAX_IMAGE_CHARS {
            return Err("image must be a bounded image path".into());
        }
        script.push_str(&format!(
            "d.writeConfig(\"Image\", \"{}\");\n",
            escape_js_string(image)
        ));
    }
    script.push_str(&format!("d.wallpaperPlugin = \"{plugin}\";"));
    Ok(script)
}

/// JS string-literal escaping: backslash, double quote, and the line
/// terminators that would otherwise escape a literal. Pure and tested.
fn escape_js_string(value: &str) -> String {
    let mut out = String::with_capacity(value.len());
    for ch in value.chars() {
        match ch {
            '\\' => out.push_str("\\\\"),
            '"' => out.push_str("\\\""),
            '\n' => out.push_str("\\n"),
            '\r' => out.push_str("\\r"),
            '\u{2028}' => out.push_str("\\u2028"),
            '\u{2029}' => out.push_str("\\u2029"),
            _ => out.push(ch),
        }
    }
    out
}

/// The one fixed template for the read-only enumeration probe. The daemon
/// never interpolates live data into it: the desktop loop is a constant,
/// and the connector list is interpolated only after daemon-side identity
/// validation (`screenForConnector` returns -1 for unknown connectors).
///
/// Reads per desktop: array index (the apply script uses the same index),
/// containment id, screen id, wallpaper plugin, and the current plugin's
/// `Image` config value (read by selecting the plugin's config group inside
/// a bounded try/catch and restoring the wrapper's group afterwards).
/// Deliberately never touches `desktopForScreen` (crash hazard).
const DESKTOP_PROBE_TEMPLATE: &str = r#"var d = desktops();
var out = [];
for (var i = 0; i < d.length; i++) {
  var image = null;
  var wp = d[i].wallpaperPlugin;
  if (/^[A-Za-z0-9._-]+$/.test(wp)) {
    var g = d[i].currentConfigGroup;
    try {
      d[i].currentConfigGroup = ["Wallpaper", wp, "General"];
      image = d[i].readConfig("Image");
    } catch (e) { }
    d[i].currentConfigGroup = g;
  }
  out.push({index: i, id: d[i].id, screen: d[i].screen, wp: wp, image: image});
}
var c = {__CONNECTORS__};
print(JSON.stringify({desktops: out, connectors: c}));"#;

pub fn probe_script(connector_names: &[String]) -> Result<String, String> {
    let mut entries = Vec::new();
    for name in connector_names {
        validate_identity_part("connector", name).map_err(|error| error.to_string())?;
        entries.push(format!("\"{name}\": screenForConnector(\"{name}\")"));
    }
    let connector_map = if entries.is_empty() {
        // An all-disconnected system has no connector names; the map must
        // still be a valid JS object literal.
        "{}".to_string()
    } else {
        // The placeholder excludes the braces so the replacement keeps the
        // JS object literal delimiters (replacing "{CONNECTORS}" would
        // swallow them and emit invalid JavaScript).
        format!("{{{}}}", entries.join(", "))
    };
    Ok(DESKTOP_PROBE_TEMPLATE.replace("{__CONNECTORS__}", &connector_map))
}

// ---------------------------------------------------------------------------
// Bounded process probes
// ---------------------------------------------------------------------------

/// Why a shell probe failed. `Unreachable` means the shell or its tooling
/// could not be reached at all; `Rejected` means it ran and refused; both
/// carry bounded detail. `DisplayUnavailable` is the narrower case where the
/// probe never ran because there is no display server to talk to — the user
/// can fix that one, so it stays distinct from `Unreachable`.
#[derive(Debug, Clone)]
pub enum ProbeError {
    Unreachable(String),
    Rejected(String),
    TimedOut(String),
    Parse(String),
    DisplayUnavailable(String),
}

impl std::fmt::Display for ProbeError {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            ProbeError::Unreachable(detail) => write!(formatter, "{detail}"),
            ProbeError::Rejected(detail) => write!(formatter, "{detail}"),
            ProbeError::TimedOut(detail) => write!(formatter, "{detail}"),
            ProbeError::Parse(detail) => write!(formatter, "{detail}"),
            ProbeError::DisplayUnavailable(detail) => write!(formatter, "{detail}"),
        }
    }
}

/// Injectable shell boundary: `evaluate_script` runs one Plasma wallpaper
/// script through the shell and returns the `print()` buffer, and
/// `system_outputs` lists the connected outputs. The production
/// implementation spawns bounded child processes with no shell involved.
pub trait ShellProbe: Send + Sync {
    fn evaluate_script(&self, script: &str) -> Result<String, ProbeError>;
    fn system_outputs(&self) -> Result<Vec<SystemOutput>, ProbeError>;
}

/// Test double for the shell boundary, shared with the daemon's RPC tests:
/// returns the fixed enumeration data and records every script it was asked
/// to run. Enumeration scripts are distinguished by their fixed template
/// opening (`var d = desktops();` — the apply/restore scripts never start
/// with it, and it is present even for a connector-less system);
/// every other script (apply/restore switches) succeeds silently unless
/// `reject_scripts` is set.
#[cfg(test)]
pub(crate) struct StubProbe {
    pub outputs: Vec<SystemOutput>,
    /// The JSON reply for enumeration scripts (the whole probe reply, as
    /// parsed by `parse_probe_reply`); None fails the probe.
    pub reply: Option<String>,
    /// The enumeration reply used once the probe has seen the kwe switch
    /// script (the live post-switch state: the desktop reports our plugin).
    pub kwe_reply: Option<String>,
    pub scripts: std::sync::Mutex<Vec<String>>,
    /// Fails every non-enumeration script (the switch step). Atomic so the
    /// RPC tests can flip it through the shared `Arc`.
    pub reject_scripts: std::sync::atomic::AtomicBool,
    /// Set when the probe has seen the kwe switch script; enumeration
    /// then reports the post-switch desktop state.
    pub kwe_assigned: std::sync::atomic::AtomicBool,
}

#[cfg(test)]
impl StubProbe {
    pub fn new(outputs: Vec<SystemOutput>, reply: Option<String>) -> Self {
        Self {
            outputs,
            reply,
            kwe_reply: None,
            scripts: std::sync::Mutex::new(Vec::new()),
            reject_scripts: std::sync::atomic::AtomicBool::new(false),
            kwe_assigned: std::sync::atomic::AtomicBool::new(false),
        }
    }

    /// The enumeration reply used after the kwe switch script has run.
    pub fn after_switch(mut self, reply: String) -> Self {
        self.kwe_reply = Some(reply);
        self
    }

    pub fn scripts(&self) -> Vec<String> {
        self.scripts.lock().unwrap().clone()
    }
}

#[cfg(test)]
impl ShellProbe for StubProbe {
    fn evaluate_script(&self, script: &str) -> Result<String, ProbeError> {
        self.scripts.lock().unwrap().push(script.to_string());
        if script.contains("var d = desktops();") {
            let reply = if self.kwe_assigned.load(std::sync::atomic::Ordering::SeqCst) {
                self.kwe_reply.as_ref().or(self.reply.as_ref())
            } else {
                self.reply.as_ref()
            };
            match reply {
                Some(reply) => Ok(reply.clone()),
                None => Err(ProbeError::Rejected("stub probe rejected".into())),
            }
        } else {
            if script.contains("wallpaperPlugin = \"org.kde.kwe.wallpaper\"") {
                self.kwe_assigned
                    .store(true, std::sync::atomic::Ordering::SeqCst);
            }
            if self
                .reject_scripts
                .load(std::sync::atomic::Ordering::SeqCst)
            {
                Err(ProbeError::Unreachable("stub probe unreachable".into()))
            } else {
                Ok(String::new())
            }
        }
    }

    fn system_outputs(&self) -> Result<Vec<SystemOutput>, ProbeError> {
        Ok(self.outputs.clone())
    }
}

/// One output as reported by `kscreen-doctor -o`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SystemOutput {
    pub name: String,
    pub enabled: bool,
    pub connected: bool,
    /// `[x, y, width, height]` when the doctor reported geometry.
    pub geometry: Option<[i32; 4]>,
}

/// The merged live view of one output: system state plus the desktop
/// containment mapped onto it via `screenForConnector` -> `d.screen`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct OutputInfo {
    pub name: String,
    /// KScreen screen id (`screenForConnector`), -1 when unmapped.
    pub screen: i32,
    /// The containment id and `desktops()` array index bound to this
    /// output; null when the screen has no desktop containment.
    pub desktop_id: Option<u64>,
    pub desktop_index: Option<usize>,
    pub geometry: Option<[i32; 4]>,
    pub enabled: bool,
    pub connected: bool,
    pub wallpaper_plugin: Option<String>,
    /// The plugin's wallpaper config group: `["Wallpaper", <plugin>,
    /// "General"]` — the restore target for this output.
    pub config_group: Vec<String>,
    pub image: Option<String>,
}

/// How the daemon reaches the Plasma shell for one `evaluateScript` call.
/// Production evaluates through a direct `qdbus` invocation (no shell; the
/// script is passed as an argument). Integration smokes inject an external
/// command run exactly the same way — `<path> <script>` with the identical
/// bounded child machinery — so the whole Plasma boundary is stubbable
/// without a live session. A stub must answer the read-only enumeration
/// probe with the probe-reply JSON and run the switch/restore scripts as a
/// recorded no-op; the stub for the smoke keeps a switch log and flips its
/// enumeration reply once the kwe switch script has been seen, mirroring
/// what the real shell reports after the switch.
#[derive(Debug, Clone)]
enum ShellEvaluator {
    Qdbus {
        shell_service: String,
        qdbus_binary: Option<PathBuf>,
    },
    External(PathBuf),
}

/// Plasma shell probe via direct `qdbus` invocation (no shell; the script
/// is passed as an argument). The qdbus binary is resolved from PATH on
/// every call — `qdbus` first, `qdbus6` fallback — so the daemon starts
/// fine on systems without either and reports `shell_unreachable` lazily.
/// `--plasma-switch-command` replaces the whole evaluation boundary with an
/// external command for integration tests.
pub struct QdbusShellProbe {
    evaluator: ShellEvaluator,
    kscreen_binary: PathBuf,
    systemctl_binary: Option<PathBuf>,
    ambient: AmbientDisplay,
    /// A display environment recovered from the systemd user manager, kept
    /// so the recovery shell-out runs once rather than per enumeration.
    /// Only SUCCESSFUL recoveries are cached: a daemon that started before
    /// its session must keep asking, and a stale entry is dropped the
    /// moment the enumeration child fails with it (`system_outputs`).
    display_env: Mutex<Option<Vec<(String, String)>>>,
    timeout: Duration,
}

impl QdbusShellProbe {
    pub fn new(
        shell_service: String,
        qdbus_binary: Option<PathBuf>,
        switch_command: Option<PathBuf>,
        kscreen_binary: PathBuf,
        systemctl_binary: Option<PathBuf>,
        timeout: Duration,
    ) -> Self {
        Self {
            evaluator: match switch_command {
                Some(path) => ShellEvaluator::External(path),
                None => ShellEvaluator::Qdbus {
                    shell_service,
                    qdbus_binary,
                },
            },
            kscreen_binary,
            systemctl_binary,
            ambient: AmbientDisplay::FromProcessEnv,
            display_env: Mutex::new(None),
            timeout,
        }
    }

    /// Test-only: pin the ambient answer to "no display". The process
    /// environment of a running test binary cannot be mutated safely, and
    /// the recovery path is exactly what these tests are about.
    #[cfg(test)]
    fn without_ambient_display(mut self) -> Self {
        self.ambient = AmbientDisplay::Absent;
        self
    }

    /// The display variables the enumeration child needs on top of the
    /// daemon's own environment.
    ///
    /// `kscreen-doctor` is a `QGuiApplication`: handed an environment with
    /// no `WAYLAND_DISPLAY` (or `DISPLAY`) it cannot load a Qt platform
    /// plugin and dies on SIGABRT. That is exactly the environment systemd
    /// hands a daemon started at boot, because Plasma imports the session
    /// environment into the user manager only at login — after the unit is
    /// already running. See `docs/bugs/OUTPUTS_EMPTY_AFTER_REBOOT.md`.
    ///
    /// So: inherit unchanged when the daemon already has a display, and
    /// otherwise recover one from the systemd user manager — the same
    /// environment a restart of the unit would have inherited. Resolution
    /// is lazy and per call, exactly like `resolve_qdbus`: the daemon may
    /// legitimately start before any session exists, and enumeration only
    /// ever runs once somebody is logged in and asking.
    ///
    /// `evaluate_script` deliberately does NOT use this. `qdbus` is a
    /// `QCoreApplication` and reaches plasmashell over the session bus with
    /// no display at all; only the KScreen enumeration needs one.
    ///
    /// Never substitute `QT_QPA_PLATFORM=offscreen` for a real display:
    /// measured 2026-08-22, `kscreen-doctor` then HANGS until it is killed
    /// instead of failing fast, turning a clear error into a probe timeout.
    fn display_env(&self) -> Result<Vec<(String, String)>, ProbeError> {
        if self.ambient.present() {
            return Ok(Vec::new());
        }
        if let Ok(cached) = self.display_env.lock()
            && let Some(entries) = cached.as_ref()
        {
            return Ok(entries.clone());
        }
        let entries = self.recover_display_env()?;
        if let Ok(mut cached) = self.display_env.lock() {
            *cached = Some(entries.clone());
        }
        Ok(entries)
    }

    /// Drops a cached recovery so the next enumeration resolves again.
    /// Called when the child failed while running with recovered values —
    /// the session may have restarted under a different display.
    fn forget_display_env(&self) {
        if let Ok(mut cached) = self.display_env.lock() {
            *cached = None;
        }
    }

    /// Reads `systemctl --user show-environment` through the same bounded
    /// child machinery as every other probe and keeps the display keys.
    fn recover_display_env(&self) -> Result<Vec<(String, String)>, ProbeError> {
        let binary = resolve_systemctl(&self.systemctl_binary)?;
        let mut command = Command::new(binary);
        command.arg("--user").arg("show-environment");
        // Deliberately shorter than the probe budget: this recovery is an
        // extra child on the enumeration path, and the probe deadline is
        // already spent twice there (kscreen-doctor, then evaluateScript).
        // Reading the local user manager's environment is a millisecond
        // operation; a systemctl that cannot answer in RECOVERY_TIMEOUT is
        // not going to, and the manager has its own request deadline to
        // respect.
        let budget = self.timeout.min(RECOVERY_TIMEOUT);
        let outcome = run_bounded(&mut command, budget).map_err(|error| {
            ProbeError::DisplayUnavailable(format!(
                "{DISPLAY_UNAVAILABLE_HINT} (systemctl show-environment: {error})"
            ))
        })?;
        if !outcome.status.success() {
            return Err(ProbeError::DisplayUnavailable(format!(
                "{DISPLAY_UNAVAILABLE_HINT} (systemctl show-environment exited {})",
                outcome.status
            )));
        }
        let entries = parse_display_env(&String::from_utf8_lossy(&outcome.stdout));
        if entries.is_empty() {
            return Err(ProbeError::DisplayUnavailable(
                DISPLAY_UNAVAILABLE_HINT.to_string(),
            ));
        }
        Ok(entries)
    }
}

/// How `display_env` learns whether the daemon's own environment already
/// names a display. Production reads the process environment; tests pin the
/// answer, because mutating the environment of a running test binary is a
/// data race, not a fixture.
#[derive(Debug, Clone, Copy)]
enum AmbientDisplay {
    FromProcessEnv,
    #[cfg(test)]
    Absent,
}

impl AmbientDisplay {
    fn present(self) -> bool {
        match self {
            AmbientDisplay::FromProcessEnv => DISPLAY_ENV_KEYS
                .iter()
                .any(|key| std::env::var_os(key).is_some_and(|value| !value.is_empty())),
            #[cfg(test)]
            AmbientDisplay::Absent => false,
        }
    }
}

/// The variables that let a Qt GUI program find its display, most specific
/// first. Both are forwarded when both are known: `kscreen-doctor` picks
/// the platform plugin itself, and guessing for it is what hangs.
const DISPLAY_ENV_KEYS: [&str; 2] = ["WAYLAND_DISPLAY", "DISPLAY"];

/// Deadline for the display-environment recovery child, capped again by
/// the probe timeout so a tighter configured budget still wins.
const RECOVERY_TIMEOUT: Duration = Duration::from_millis(1500);

/// Longest display value accepted from the manager environment. A Wayland
/// display is a socket name or a path to one; anything longer is not a
/// display, it is someone else's data.
const MAX_DISPLAY_VALUE_BYTES: usize = 128;

/// Said to the user, not just logged: this failure has a fix they can run.
const DISPLAY_UNAVAILABLE_HINT: &str = "the wallpaper service cannot reach the display server \
(it started before the desktop session did); run `systemctl --user restart kwe-daemon`";

/// Resolves the systemctl binary: an explicit path wins, else `systemctl`
/// from PATH. Resolved per call for the same reason as `resolve_qdbus`.
fn resolve_systemctl(systemctl_binary: &Option<PathBuf>) -> Result<PathBuf, ProbeError> {
    if let Some(path) = systemctl_binary {
        return Ok(path.clone());
    }
    find_in_path(std::env::var_os("PATH").as_deref(), &["systemctl"]).ok_or_else(|| {
        ProbeError::DisplayUnavailable(format!(
            "{DISPLAY_UNAVAILABLE_HINT} (systemctl is not on PATH)"
        ))
    })
}

/// Picks the display keys out of `systemctl show-environment` output:
/// `KEY=VALUE` per line, unknown keys ignored. systemd shell-quotes values
/// that need it, so one layer of surrounding double quotes is stripped;
/// anything that is not a plain printable token is dropped rather than
/// handed to a child process — the daemon builds its children's inputs, it
/// does not forward whatever it was told.
fn parse_display_env(text: &str) -> Vec<(String, String)> {
    let mut entries = Vec::new();
    for line in text.lines() {
        let Some((key, value)) = line.split_once('=') else {
            continue;
        };
        if !DISPLAY_ENV_KEYS.contains(&key) {
            continue;
        }
        let value = value
            .strip_prefix('"')
            .and_then(|rest| rest.strip_suffix('"'))
            .unwrap_or(value);
        if valid_display_value(value) {
            entries.push((key.to_string(), value.to_string()));
        }
    }
    entries
}

/// A display value is a bounded, non-empty run of printable ASCII with no
/// whitespace and no quoting characters.
fn valid_display_value(value: &str) -> bool {
    !value.is_empty()
        && value.len() <= MAX_DISPLAY_VALUE_BYTES
        && value.bytes().all(|byte| {
            byte.is_ascii_graphic() && !matches!(byte, b'"' | b'\'' | b'`' | b'$' | b'\\')
        })
}

/// Resolves the qdbus binary: an explicit path wins, else `qdbus` then
/// `qdbus6` from PATH.
fn resolve_qdbus(qdbus_binary: &Option<PathBuf>) -> Result<PathBuf, ProbeError> {
    if let Some(path) = qdbus_binary {
        return Ok(path.clone());
    }
    find_in_path(std::env::var_os("PATH").as_deref(), &["qdbus", "qdbus6"])
        .ok_or_else(|| ProbeError::Unreachable("qdbus (or qdbus6) is not on PATH".into()))
}

impl ShellProbe for QdbusShellProbe {
    fn evaluate_script(&self, script: &str) -> Result<String, ProbeError> {
        let outcome = match &self.evaluator {
            ShellEvaluator::Qdbus {
                shell_service,
                qdbus_binary,
            } => {
                let mut command = Command::new(resolve_qdbus(qdbus_binary)?);
                command
                    .arg(shell_service)
                    .arg("/PlasmaShell")
                    .arg("evaluateScript")
                    .arg(script);
                run_bounded(&mut command, self.timeout)
                    .map_err(|error| classify_probe_failure(&error))?
            }
            ShellEvaluator::External(path) => {
                let mut command = Command::new(path);
                command.arg(script);
                run_bounded(&mut command, self.timeout)
                    .map_err(|error| classify_probe_failure(&error))?
            }
        };
        if !outcome.status.success() {
            let detail = String::from_utf8_lossy(&outcome.stderr).trim().to_string();
            return Err(ProbeError::Rejected(if detail.is_empty() {
                format!("evaluateScript exited {}", outcome.status)
            } else {
                detail
            }));
        }
        let stdout = String::from_utf8_lossy(&outcome.stdout);
        Ok(stdout.trim().to_string())
    }

    fn system_outputs(&self) -> Result<Vec<SystemOutput>, ProbeError> {
        let recovered = self.display_env()?;
        let mut command = Command::new(&self.kscreen_binary);
        command.arg("-o");
        for (key, value) in &recovered {
            command.env(key, value);
        }
        // A child that failed while running on RECOVERED values may have
        // been handed a display that no longer exists (the session
        // restarted under a new one). Drop the cache on any failure so the
        // next enumeration resolves again instead of repeating a stale
        // answer; an inherited environment is the daemon's own and is not
        // ours to forget.
        let forget_on_failure = |probe_error| {
            if !recovered.is_empty() {
                self.forget_display_env();
            }
            probe_error
        };
        let outcome = run_bounded(&mut command, self.timeout).map_err(|error| {
            forget_on_failure(classify_probe_failure(&format!("kscreen-doctor: {error}")))
        })?;
        if !outcome.status.success() {
            let detail = String::from_utf8_lossy(&outcome.stderr).trim().to_string();
            return Err(forget_on_failure(ProbeError::Rejected(
                if detail.is_empty() {
                    format!("kscreen-doctor exited {}", outcome.status)
                } else {
                    detail
                },
            )));
        }
        let text = String::from_utf8_lossy(&outcome.stdout);
        Ok(parse_kscreen_doctor(&text))
    }
}

/// Classifies a bounded-run failure: killed at the deadline is a timeout
/// (the shell may still be alive and well; the probe just lost patience),
/// anything else is an unreachable toolchain.
fn classify_probe_failure(detail: &str) -> ProbeError {
    if detail.contains("timed out") {
        ProbeError::TimedOut(detail.to_string())
    } else {
        ProbeError::Unreachable(detail.to_string())
    }
}

/// Result of a bounded child run: exit status plus capped stdout/stderr.
struct RunOutcome {
    status: ExitStatus,
    stdout: Vec<u8>,
    stderr: Vec<u8>,
}

/// Runs `command` with null stdin, pipes stdout/stderr with a hard 64 KiB
/// cap each, and kills it at `timeout`. Both pipes are read nonblocking so
/// a chatty child can never wedge the deadline behind a full pipe.
fn run_bounded(command: &mut Command, timeout: Duration) -> Result<RunOutcome, String> {
    let mut child = command
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .map_err(|error| format!("spawn failed: {error}"))?;
    let mut stdout = child.stdout.take().ok_or("stdout pipe unavailable")?;
    let mut stderr = child.stderr.take().ok_or("stderr pipe unavailable")?;
    set_nonblocking(&stdout)?;
    set_nonblocking(&stderr)?;
    let deadline = Instant::now() + timeout;
    let mut out = Vec::new();
    let mut err = Vec::new();
    loop {
        let status = match child.try_wait() {
            Ok(Some(status)) => Some(status),
            Ok(None) => None,
            Err(error) => return Err(format!("wait failed: {error}")),
        };
        let mut stdout_eof = drain_pipe(&mut stdout, &mut out)?;
        let mut stderr_eof = drain_pipe(&mut stderr, &mut err)?;
        if let Some(status) = status {
            // The child is gone; the writers closed, so EOF on both pipes
            // is guaranteed once the remaining buffered bytes are read.
            let mut waits = 0;
            while !(stdout_eof && stderr_eof) {
                stdout_eof |= drain_pipe(&mut stdout, &mut out)?;
                stderr_eof |= drain_pipe(&mut stderr, &mut err)?;
                if stdout_eof && stderr_eof {
                    break;
                }
                if waits >= 50 {
                    return Err("pipe drain stalled after child exit".into());
                }
                waits += 1;
                std::thread::sleep(Duration::from_millis(1));
            }
            return Ok(RunOutcome {
                status,
                stdout: out,
                stderr: err,
            });
        }
        if Instant::now() >= deadline {
            let _ = child.kill();
            let _ = child.wait();
            return Err(format!(
                "timed out after {} ms and was killed",
                timeout.as_millis()
            ));
        }
        std::thread::sleep(Duration::from_millis(5));
    }
}

/// Drains one nonblocking pipe into a capped buffer; returns whether EOF
/// was reached. Overflow bytes are read and discarded, never buffered.
fn drain_pipe(file: &mut impl Read, buffer: &mut Vec<u8>) -> Result<bool, String> {
    let mut chunk = [0u8; 8192];
    let mut eof = false;
    loop {
        match file.read(&mut chunk) {
            Ok(0) => {
                eof = true;
                break;
            }
            Ok(count) => {
                if buffer.len() < MAX_PROBE_OUTPUT_BYTES {
                    let take = count.min(MAX_PROBE_OUTPUT_BYTES - buffer.len());
                    buffer.extend_from_slice(&chunk[..take]);
                }
            }
            Err(error) if error.kind() == ErrorKind::WouldBlock => break,
            Err(error) => return Err(format!("pipe read failed: {error}")),
        }
    }
    Ok(eof)
}

fn set_nonblocking(file: &impl AsRawFd) -> Result<(), String> {
    let fd = file.as_raw_fd();
    // SAFETY: fcntl with F_GETFL takes a valid open descriptor.
    let flags = unsafe { libc::fcntl(fd, libc::F_GETFL) };
    if flags < 0 {
        return Err(format!(
            "fcntl(F_GETFL) failed: {}",
            std::io::Error::last_os_error()
        ));
    }
    // SAFETY: fcntl with F_SETFL on the same descriptor is safe for the
    // flags value just read.
    let rc = unsafe { libc::fcntl(fd, libc::F_SETFL, flags | libc::O_NONBLOCK) };
    if rc != 0 {
        return Err(format!(
            "fcntl(F_SETFL) failed: {}",
            std::io::Error::last_os_error()
        ));
    }
    Ok(())
}

/// PATH lookup preferring earlier names. Pure so tests can hand it a
/// synthetic PATH; returns the first existing regular file.
fn find_in_path(path_var: Option<&std::ffi::OsStr>, names: &[&str]) -> Option<PathBuf> {
    let path_var = path_var?;
    for directory in std::env::split_paths(path_var) {
        for name in names {
            let candidate = directory.join(name);
            if candidate.is_file() {
                return Some(candidate);
            }
        }
    }
    None
}

/// Parses `kscreen-doctor -o` output (ANSI stripped defensively):
/// `Output: <index> <name> <uuid>` blocks with indented `enabled` /
/// `connected` / `Geometry: x,y WxH` lines. Unknown lines are ignored.
fn parse_kscreen_doctor(text: &str) -> Vec<SystemOutput> {
    let mut outputs = Vec::new();
    let mut current: Option<SystemOutput> = None;
    for raw_line in text.lines() {
        let line = strip_ansi(raw_line);
        let line = line.trim();
        if let Some(rest) = line.strip_prefix("Output: ") {
            if let Some(previous) = current.take() {
                outputs.push(previous);
            }
            let name = rest.split_whitespace().nth(1);
            if let Some(name) = name {
                current = Some(SystemOutput {
                    name: name.to_string(),
                    enabled: true,
                    connected: false,
                    geometry: None,
                });
            }
            continue;
        }
        if let Some(output) = current.as_mut() {
            match line {
                "enabled" => output.enabled = true,
                "disabled" => output.enabled = false,
                "connected" => output.connected = true,
                "disconnected" => output.connected = false,
                _ => {
                    if let Some(geometry) = line.strip_prefix("Geometry: ") {
                        output.geometry = parse_geometry(geometry);
                    }
                }
            }
        }
    }
    if let Some(last) = current {
        outputs.push(last);
    }
    outputs
}

/// `x,y WxH` -> `[x, y, w, h]`; anything unparsable yields None.
fn parse_geometry(text: &str) -> Option<[i32; 4]> {
    let mut parts = text.split_whitespace();
    let position = parts.next()?;
    let size = parts.next()?;
    let (x, y) = position.split_once(',')?;
    let (w, h) = size.split_once('x')?;
    Some([
        x.parse().ok()?,
        y.parse().ok()?,
        w.parse().ok()?,
        h.parse().ok()?,
    ])
}

/// Strips ANSI escape sequences (the doctor colors output on a tty; the
/// daemon's piped runs should not, but defensive stripping is cheap).
/// Handles CSI sequences (ESC [ ... final byte) and OSC sequences
/// (ESC ] ... BEL); anything else after ESC is dropped with the ESC.
fn strip_ansi(line: &str) -> String {
    let mut out = String::with_capacity(line.len());
    let mut chars = line.chars();
    while let Some(ch) = chars.next() {
        if ch != '\u{1b}' {
            out.push(ch);
            continue;
        }
        match chars.next() {
            Some('[') => {
                for next in chars.by_ref() {
                    if next.is_ascii_alphabetic() {
                        break;
                    }
                }
            }
            Some(']') => {
                for next in chars.by_ref() {
                    if next == '\u{7}' {
                        break;
                    }
                }
            }
            _ => {}
        }
    }
    out
}

/// One desktop containment as reported by the enumeration probe script.
#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct ProbeDesktop {
    index: usize,
    id: u64,
    screen: i32,
    wp: String,
    image: Option<String>,
}

/// The whole probe reply: desktops plus the connector -> screen mapping.
#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct ProbeReply {
    desktops: Vec<ProbeDesktop>,
    connectors: BTreeMap<String, i32>,
}

fn parse_probe_reply(text: &str) -> Result<ProbeReply, ProbeError> {
    if text.is_empty() {
        return Err(ProbeError::Parse(
            "empty probe reply (the shell returned no print() output)".into(),
        ));
    }
    serde_json::from_str(text).map_err(|error| {
        ProbeError::Parse(format!("probe reply is not the expected JSON: {error}"))
    })
}

/// Merges the system outputs, the desktop probe, and the connector mapping
/// into the per-output view. Desktops without a screen (orphaned
/// containments) never match a connector and stay excluded; a connector
/// without a matching desktop yields a desktop-less output. A connector
/// that maps to screen -1 (screenForConnector reports unknown) likewise
/// never binds an orphaned desktop — an orphan must not be treated as the
/// target of a real output.
fn assemble_outputs(
    system: Vec<SystemOutput>,
    desktops: Vec<ProbeDesktop>,
    connectors: BTreeMap<String, i32>,
) -> Vec<OutputInfo> {
    system
        .into_iter()
        .map(|output| {
            let screen = connectors.get(&output.name).copied().unwrap_or(-1);
            let desktop = (screen >= 0)
                .then(|| desktops.iter().find(|desktop| desktop.screen == screen))
                .flatten();
            OutputInfo {
                name: output.name,
                screen,
                desktop_id: desktop.map(|desktop| desktop.id),
                desktop_index: desktop.map(|desktop| desktop.index),
                geometry: output.geometry,
                enabled: output.enabled,
                connected: output.connected,
                wallpaper_plugin: desktop.map(|desktop| desktop.wp.clone()),
                config_group: desktop
                    .map(|desktop| vec!["Wallpaper".into(), desktop.wp.clone(), "General".into()])
                    .unwrap_or_default(),
                image: desktop
                    .and_then(|desktop| desktop.image.clone())
                    .filter(|image| !image.is_empty()),
            }
        })
        .collect()
}

// ---------------------------------------------------------------------------
// Apply service
// ---------------------------------------------------------------------------

/// Apply-service configuration assembled by the daemon from CLI arguments.
pub struct ApplyConfig {
    pub state_dir: PathBuf,
    /// Plasma shell D-Bus service name (Plasma 6: `org.kde.plasmashell`).
    pub shell_service: String,
    /// Explicit qdbus binary; None resolves `qdbus` then `qdbus6` on PATH.
    pub qdbus_binary: Option<PathBuf>,
    /// Replace the whole Plasma evaluation boundary with this command,
    /// run as `<path> <script>` (default: qdbus). Integration smokes use a
    /// stub here so no live Plasma is touched; live enablement (M4d) runs
    /// the real qdbus path by leaving it unset.
    pub switch_command: Option<PathBuf>,
    pub kscreen_binary: PathBuf,
    /// Explicit systemctl binary; None resolves `systemctl` on PATH. Used
    /// only to recover a display environment for the enumeration child
    /// (`QdbusShellProbe::display_env`).
    pub systemctl_binary: Option<PathBuf>,
    /// Deadline for every probe (enumeration, switch, restore).
    pub probe_timeout: Duration,
    /// Deadline for the renderer to reach a live phase after start.
    pub promotion_timeout: Duration,
    /// The Wallpaper Engine assets root (S1), forwarded to scene preflight
    /// (`StartSpec::into_validated`) so a scene's model layers can resolve
    /// their material textures before the apply transaction runs.
    pub scene_assets_dir: Option<PathBuf>,
}

/// The `wallpaper.*` API errors. Codes are the wire contract
/// (docs/SUPERVISOR_API_V1.md); detail is bounded and advisory.
#[derive(Debug, Clone)]
pub enum ApplyError {
    /// Params failed the StartSpec validation rules.
    Invalid(String),
    /// No usable catalog item carries this wallpaper id.
    UnknownWallpaper(String),
    /// The catalog item exists but is not the requested kind.
    Incompatible(String),
    /// No enumerated output carries this name.
    OutputMissing(String),
    /// Another apply transaction is in flight.
    Busy,
    /// A user/manager apply took the slot between the playlist session's
    /// verdict and this transaction's lock (the post-lock re-read, Finding
    /// 1): a NON-failure yield the session treats exactly like a foreign
    /// renderer winning the tick.
    Yielded(String),
    /// The Plasma shell or its tooling could not be reached.
    ShellUnreachable(String),
    /// The output enumeration never ran: there is no display server in
    /// reach. Distinct from `ShellUnreachable` because the user has a fix
    /// (BETA B1, `docs/bugs/OUTPUTS_EMPTY_AFTER_REBOOT.md`).
    DisplayUnavailable(String),
    /// A step of the apply transaction failed (already rolled back).
    Transaction(String),
    /// The supervisor refused to start this content because its persisted
    /// failure record is quarantined (three strikes under this build). The
    /// detail is the record's last failure detail; a client that wants to
    /// try anyway re-applies with `retry: true`, which clears exactly this
    /// identity's record first (B4).
    Quarantined(String),
    /// The restore script could not be executed.
    RestoreFailed(String),
}

/// Enumeration failures are `shell_unreachable` as they always were, with
/// one exception carried through intact: "there is no display server" is a
/// state the user can fix, so it keeps its own code all the way to the UI.
fn enumeration_error(error: ProbeError) -> ApplyError {
    match error {
        ProbeError::DisplayUnavailable(detail) => ApplyError::DisplayUnavailable(detail),
        other => ApplyError::ShellUnreachable(other.to_string()),
    }
}

impl ApplyError {
    pub fn code(&self) -> &'static str {
        match self {
            ApplyError::Invalid(_) => "invalid_params",
            ApplyError::UnknownWallpaper(_) => "apply_unknown_wallpaper",
            ApplyError::Incompatible(_) => "apply_incompatible",
            ApplyError::OutputMissing(_) => "output_missing",
            ApplyError::Busy => "apply_busy",
            ApplyError::Yielded(_) => "apply_yielded",
            ApplyError::ShellUnreachable(_) => "shell_unreachable",
            ApplyError::DisplayUnavailable(_) => "display_unavailable",
            ApplyError::Transaction(_) => "apply_failed",
            ApplyError::Quarantined(_) => "apply_quarantined",
            ApplyError::RestoreFailed(_) => "restore_failed",
        }
    }

    pub fn detail(&self) -> Option<&str> {
        match self {
            ApplyError::Invalid(detail)
            | ApplyError::UnknownWallpaper(detail)
            | ApplyError::Incompatible(detail)
            | ApplyError::OutputMissing(detail)
            | ApplyError::Yielded(detail)
            | ApplyError::ShellUnreachable(detail)
            | ApplyError::DisplayUnavailable(detail)
            | ApplyError::Transaction(detail)
            | ApplyError::Quarantined(detail)
            | ApplyError::RestoreFailed(detail) => Some(detail),
            ApplyError::Busy => None,
        }
    }
}

/// `wallpaper.apply` params (deny_unknown_fields). `kind` follows the
/// StartSpec rules; the test kind is not assignable. `content` is
/// optional: when supplied it must match the catalog item's resolved
/// content path (it is verified, never trusted), and when absent the
/// catalog content is used — the renderer always starts with the catalog
/// content.
#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ApplyWallpaperParams {
    pub output: String,
    pub wallpaper_id: String,
    pub kind: RendererKind,
    #[serde(default)]
    pub content: Option<PathBuf>,
    /// Frame canvas size. Omitted (F1): derived from the output's geometry
    /// — the output's own aspect, long edge capped at `MAX_FRAME_EDGE` —
    /// so every scaling mode maps a canvas that already fits the display
    /// instead of upscaling a fixed 960x540. Explicit values are bounded
    /// by the frame protocol / supervisor limits exactly as before.
    #[serde(default)]
    pub width: Option<u32>,
    #[serde(default)]
    pub height: Option<u32>,
    #[serde(default = "default_apply_fps")]
    pub fps: u32,
    /// F1: `aspect` (default) | `fill` | `stretch`; persisted per output.
    #[serde(default)]
    pub scaling: ScalingMode,
    /// Clear this identity's quarantine record before starting (B4): the
    /// user saw `apply_quarantined` with the reason and chose to try
    /// anyway. Routes through the supervisor's `Retry` command — the same
    /// thing `renderer.retry` does — and nothing else changes.
    #[serde(default)]
    pub retry: bool,
}

pub const fn default_apply_width() -> u32 {
    960
}

/// Longest frame-canvas edge the daemon derives from an output (F1). A
/// 2560-wide canvas keeps the web screencast's JPEG decode, libmpv's
/// software render and the Vulkan compositor inside the budgets measured
/// for 960x540 × ~4.5; larger outputs are upscaled by the plugin. Explicit
/// `width`/`height` params are not capped by this (only by the protocol).
pub const MAX_FRAME_EDGE: u32 = 2560;
/// Smallest derived edge: a degenerate/empty geometry never produces a
/// canvas the renderers cannot use.
const MIN_FRAME_EDGE: u32 = 64;

/// The frame canvas for an apply: explicit params win; otherwise the
/// output's geometry (width/height, aspect kept, long edge capped at
/// `MAX_FRAME_EDGE`, never below `MIN_FRAME_EDGE`); otherwise the legacy
/// 960x540. Pure and bounded — its result always passes `FrameSpec` and
/// the supervised mapping cap (2560x2560x4x2 slots < 128 MiB).
pub fn frame_size_for(
    width: Option<u32>,
    height: Option<u32>,
    geometry: Option<[i32; 4]>,
) -> (u32, u32) {
    if let (Some(width), Some(height)) = (width, height) {
        return (width, height);
    }
    let Some([_, _, out_w, out_h]) = geometry else {
        return (
            width.unwrap_or(default_apply_width()),
            height.unwrap_or(default_apply_height()),
        );
    };
    if out_w <= 0 || out_h <= 0 {
        return (
            width.unwrap_or(default_apply_width()),
            height.unwrap_or(default_apply_height()),
        );
    }
    let (mut w, mut h) = (out_w as u64, out_h as u64);
    let long = w.max(h);
    if long > MAX_FRAME_EDGE as u64 {
        // Scale both edges by the same factor, rounding to even pixels
        // (video decoders and JPEG like even dimensions).
        w = (w * MAX_FRAME_EDGE as u64 / long) & !1;
        h = (h * MAX_FRAME_EDGE as u64 / long) & !1;
    }
    let w = (w as u32).clamp(MIN_FRAME_EDGE, MAX_FRAME_EDGE);
    let h = (h as u32).clamp(MIN_FRAME_EDGE, MAX_FRAME_EDGE);
    (width.unwrap_or(w), height.unwrap_or(h))
}

pub const fn default_apply_height() -> u32 {
    540
}

pub const fn default_apply_fps() -> u32 {
    30
}

/// `wallpaper.restore` params.
#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct RestoreWallpaperParams {
    pub output: String,
}

/// The service object; the daemon holds one handle and passes it to the
/// API boundary. The handle is cheaply cloneable; its locks are only
/// contended within one request at a time (the daemon is single-threaded).
#[derive(Clone)]
pub struct ApplyHandle {
    store: Arc<Mutex<AssignmentStore>>,
    outputs_cache: Arc<Mutex<OutputCache>>,
    probe: Arc<dyn ShellProbe>,
    catalog: Arc<RwLock<Catalog>>,
    supervisor: SupervisorHandle,
    apply_lock: Arc<Mutex<()>>,
    promotion_timeout: Duration,
    scene_assets_dir: Option<PathBuf>,
}

/// Output enumeration cache: fresh for `OUTPUT_CACHE_TTL` after a probe,
/// never cached indefinitely (a hotplug must not go unseen forever).
struct OutputCache {
    entries: Vec<OutputInfo>,
    cached_at: Option<Instant>,
}

impl OutputCache {
    fn stale() -> Self {
        Self {
            entries: Vec::new(),
            cached_at: None,
        }
    }

    fn fresh(&self, now: Instant) -> bool {
        self.cached_at
            .is_some_and(|cached_at| now.duration_since(cached_at) < OUTPUT_CACHE_TTL)
    }
}

pub struct ApplyService {
    handle: ApplyHandle,
}

impl ApplyService {
    pub fn new(
        config: ApplyConfig,
        catalog: Arc<RwLock<Catalog>>,
        supervisor: SupervisorHandle,
    ) -> Result<Self> {
        let store = AssignmentStore::open(&config.state_dir)?;
        let probe: Arc<dyn ShellProbe> = Arc::new(QdbusShellProbe::new(
            config.shell_service,
            config.qdbus_binary,
            config.switch_command,
            config.kscreen_binary,
            config.systemctl_binary,
            config.probe_timeout,
        ));
        Ok(Self {
            handle: ApplyHandle {
                store: Arc::new(Mutex::new(store)),
                outputs_cache: Arc::new(Mutex::new(OutputCache::stale())),
                probe,
                catalog,
                supervisor,
                apply_lock: Arc::new(Mutex::new(())),
                promotion_timeout: config.promotion_timeout,
                scene_assets_dir: config.scene_assets_dir,
            },
        })
    }

    pub fn handle(&self) -> ApplyHandle {
        self.handle.clone()
    }
}

impl ApplyHandle {
    /// The configured Wallpaper Engine assets root (S1 review #5): so
    /// entry points other than `wallpaper.apply` (namely the low-level
    /// `renderer.start`/`renderer.retry` RPCs, `main.rs`) can thread the
    /// same assets dir through scene preflight that `spawn_worker`
    /// (`supervisor.rs`) already forwards to the worker unconditionally —
    /// without this, a model-layer scene that resolves and draws fine at
    /// runtime could be needlessly refused at preflight when started
    /// through `renderer.start` instead of `wallpaper.apply`.
    pub fn scene_assets_dir(&self) -> Option<&Path> {
        self.scene_assets_dir.as_deref()
    }

    /// Test-only constructor with an injectable probe (the RPC tests swap
    /// in a stub; the production probe goes through `ApplyService::new`).
    /// Takes an `Arc` so the test can keep its own reference to the stub
    /// and inspect the recorded scripts.
    #[cfg(test)]
    pub(crate) fn for_test(
        store: AssignmentStore,
        probe: Arc<dyn ShellProbe>,
        catalog: Arc<RwLock<Catalog>>,
        supervisor: SupervisorHandle,
        promotion_timeout: Duration,
    ) -> Self {
        Self {
            store: Arc::new(Mutex::new(store)),
            outputs_cache: Arc::new(Mutex::new(OutputCache::stale())),
            probe,
            catalog,
            supervisor,
            apply_lock: Arc::new(Mutex::new(())),
            promotion_timeout,
            scene_assets_dir: None,
        }
    }

    /// The transaction lock: one apply at a time. Returns the guard so a
    /// caller (or test) can hold it for the whole transaction.
    pub(crate) fn acquire_apply_lock(&self) -> Result<MutexGuard<'_, ()>, ApplyError> {
        self.apply_lock.try_lock().map_err(|_| ApplyError::Busy)
    }

    /// Live output enumeration, cached for `OUTPUT_CACHE_TTL`.
    pub fn outputs(&self) -> Result<Value, ApplyError> {
        let mut cache = self
            .outputs_cache
            .lock()
            .map_err(|_| ApplyError::Transaction("outputs cache lock poisoned".into()))?;
        let now = Instant::now();
        if !cache.fresh(now) {
            let entries = self.enumerate_fresh()?;
            cache.entries = entries;
            cache.cached_at = Some(now);
        }
        Ok(json!({ "outputs": &cache.entries }))
    }

    /// Fresh probe of the live outputs; the apply transaction and restore
    /// always re-probe so desktop indices cannot go stale mid-decision.
    pub(crate) fn enumerate_fresh(&self) -> Result<Vec<OutputInfo>, ApplyError> {
        let system = self.probe.system_outputs().map_err(enumeration_error)?;
        let names: Vec<String> = system.iter().map(|output| output.name.clone()).collect();
        let script = probe_script(&names).map_err(|error| {
            ApplyError::ShellUnreachable(format!("cannot build the enumeration script: {error}"))
        })?;
        let reply = self
            .probe
            .evaluate_script(&script)
            .map_err(enumeration_error)?;
        let reply: ProbeReply = parse_probe_reply(&reply).map_err(enumeration_error)?;
        Ok(assemble_outputs(system, reply.desktops, reply.connectors))
    }

    /// The full live-apply transaction. Completes when the renderer
    /// PROMOTES (Live or AwaitingAck) AND the Plasma switch is verified —
    /// not when the display ack arrives (that comes later from the
    /// wallpaper bridge). Everything after `renderer.start` is covered by
    /// one rollback: stop the renderer (only while it is still ours) and
    /// revert the store to the pre-apply record — so a failed re-apply can
    /// never destroy the original wallpaper config.
    pub fn apply(&self, params: ApplyWallpaperParams) -> Result<Value, ApplyError> {
        let _guard = self.acquire_apply_lock()?;

        // 1. Validate the request into a supervisor StartSpec (single
        // validation point; content preflight runs here).
        let mut spec = build_apply_spec(&params, self.scene_assets_dir.as_deref())?;

        // 2. Catalog lookup: the id must be a usable local item of the
        // requested kind.
        let item = self.catalog_item(&params.wallpaper_id)?;
        if renderer_kind_for(&item) != Some(params.kind) {
            return Err(ApplyError::Incompatible(format!(
                "{} is a {} project, not {}",
                params.wallpaper_id,
                project_kind_name(&item.kind),
                params.kind.as_str()
            )));
        }

        // 2b. The renderer runs the CATALOG content (authoritative): the
        // item's resolved entry (or the web content root). Client-supplied
        // content is verified against it and never trusted; absent content
        // means the catalog path is used.
        let content = catalog_content_path(&item, params.kind);
        if let Some(client_content) = &params.content {
            let client_canon = std::fs::canonicalize(client_content).ok();
            let catalog_canon = std::fs::canonicalize(&content).ok();
            if client_canon.as_deref() != catalog_canon.as_deref() {
                return Err(ApplyError::Invalid(format!(
                    "content {} does not match the catalog item's resolved content {}",
                    client_content.display(),
                    content.display()
                )));
            }
        }
        if content.to_string_lossy().chars().count() > MAX_CONTENT_CHARS {
            return Err(ApplyError::Invalid(format!(
                "catalog content path exceeds {MAX_CONTENT_CHARS} characters"
            )));
        }
        spec.content = Some(content_spec_for(params.kind, &content));
        spec = spec
            .into_validated(self.scene_assets_dir.as_deref())
            .map_err(|error| ApplyError::Invalid(error.to_string()))?;
        spec.content_hash = content_hash_for(&item, &content);

        // 3. Fresh enumeration; the output must be live and have a desktop.
        let outputs = self.enumerate_fresh()?;
        let output = outputs
            .iter()
            .find(|info| info.name == params.output)
            .ok_or_else(|| ApplyError::OutputMissing(params.output.clone()))?;
        let desktop_index = output.desktop_index.ok_or_else(|| {
            ApplyError::Transaction(format!("output {} has no desktop containment", output.name))
        })?;
        // 3a. F1: the frame canvas follows the output unless the client
        // pinned it. Pure and bounded (see frame_size_for), so the validated
        // spec stays valid.
        let (width, height) = frame_size_for(params.width, params.height, output.geometry);
        spec.width = width;
        spec.height = height;

        // 3b. The pre-apply wallpaper state. When a stored assignment
        // exists AND the live enumeration already shows our plugin, the
        // stored record's `previous` is carried forward — a re-apply must
        // never overwrite the original wallpaper config with our own.
        let previous = self.previous_for(output)?;

        // The store record that must survive if the transaction fails
        // after starting the renderer (the store is only touched
        // post-start, so this is the rollback target).
        let old_assignment = self.stored_assignment(&output.name)?;

        // 4+. Start the renderer; EVERY failure from here on rolls back.
        let result = self.complete_apply(
            &spec,
            &content,
            &output.name,
            desktop_index,
            previous,
            params.retry,
        );
        if let Err(error) = result {
            self.rollback_after_failure(&spec, &output.name, old_assignment);
            return Err(error);
        }
        result
    }

    /// Rolls back a failed transaction after the renderer started: stop
    /// the renderer — but only while it is still ours; an ownership change
    /// means another thread owns the supervisor now, and stopping it would
    /// kill their renderer — and revert the store to the pre-apply record
    /// (set the old record back, never just remove, so the original
    /// previous survives a failed re-apply).
    fn rollback_after_failure(
        &self,
        spec: &StartSpec,
        output_name: &str,
        old_assignment: Option<Assignment>,
    ) {
        if let Ok(status) = self.supervisor.status() {
            let ours = status.requested_wallpaper_id.as_deref() == Some(&spec.wallpaper_id)
                && status.requested_content_hash.as_deref() == Some(&spec.content_hash);
            if ours {
                let _ = self.supervisor.stop();
            }
        }
        if let Ok(mut store) = self.store.lock() {
            match old_assignment {
                Some(old) => {
                    let _ = store.set(output_name, old);
                }
                None => {
                    let _ = store.remove(output_name);
                }
            }
        }
    }

    /// Resolves the output a playlist applies to. An explicitly configured
    /// output (`--playlist-output`) wins and is validated like every other
    /// output identity; otherwise the last assigned output whose wallpaper
    /// is a member of the active playlist (the display the user already
    /// attached to this playlist through the UI — per-display playlist
    /// intent, docs/UX_DESIGN.md), else the first enabled and connected
    /// output on the bus, else `OutputMissing` (nothing is applied). The
    /// resolution runs on every apply attempt, so an output that returns
    /// to the bus is picked up without a daemon restart.
    pub(crate) fn resolve_playlist_output(
        &self,
        output: Option<&str>,
        playlist_entries: &BTreeSet<String>,
    ) -> Result<String, ApplyError> {
        if let Some(output) = output {
            validate_identity_part("output", output)
                .map_err(|error| ApplyError::Invalid(error.to_string()))?;
            return Ok(output.to_string());
        }
        // The store records the last output a playlist member was assigned
        // to (per-display intent). It is validated against the FRESH
        // enumeration used by the transaction (Finding 3): a hotplugged-away
        // display falls through to the first enabled and connected output
        // instead of failing with output_missing + backoff. The store lock is
        // scoped so no probe runs while it is held.
        let stored = {
            let store = self
                .store
                .lock()
                .map_err(|_| ApplyError::Transaction("assignment store lock poisoned".into()))?;
            store
                .all()
                .iter()
                .find(|(_, assignment)| playlist_entries.contains(&assignment.wallpaper_id))
                .map(|(name, _)| name.clone())
        };
        let outputs = self.enumerate_fresh()?;
        if let Some(stored) = stored
            && outputs.iter().any(|info| info.name == stored)
        {
            return Ok(stored);
        }
        outputs
            .iter()
            .find(|info| info.enabled && info.connected)
            .map(|info| info.name.clone())
            .ok_or_else(|| ApplyError::OutputMissing("no enabled, connected output".into()))
    }

    /// The post-start half of the apply transaction. Every failure here is
    /// rolled back by `apply` (stop the renderer, revert the store).
    fn complete_apply(
        &self,
        spec: &StartSpec,
        content: &std::path::Path,
        output_name: &str,
        desktop_index: usize,
        previous: PreviousWallpaper,
        retry: bool,
    ) -> Result<Value, ApplyError> {
        // 4. Start the renderer and wait (bounded) for OUR promotion. A
        // `retry` apply clears this identity's failure record first (the
        // user saw the quarantine reason and chose to try again).
        let started = if retry {
            self.supervisor.retry(spec.clone())
        } else {
            self.supervisor.start(spec.clone())
        }
        .map_err(|error| ApplyError::Transaction(format!("renderer.start failed: {error}")))?;
        if started.phase == WorkerPhase::Quarantined || started.phase == WorkerPhase::RolledBack {
            // The supervisor answered synchronously with a terminal phase:
            // either the record is quarantined (B4: say why, and under
            // which code, so the client can offer "try anyway") or the
            // spawn itself failed and struck.
            let detail = started
                .last_failure_detail
                .clone()
                .unwrap_or_else(|| phase_name(&started.phase).to_string());
            if started.quarantined {
                return Err(ApplyError::Quarantined(format!(
                    "disabled after {} failures under this build; last failure: {detail}",
                    started.failures
                )));
            }
            return Err(ApplyError::Transaction(format!(
                "renderer rejected the start ({}: {detail})",
                phase_name(&started.phase)
            )));
        }
        self.wait_for_promotion(&spec.wallpaper_id, &spec.content_hash)?;
        // Ownership re-check immediately before the persist/switch steps:
        // a playlist start landing between promotion and persist would
        // otherwise be masked until the (misleading) timeout.
        self.ensure_ours(&spec.wallpaper_id, &spec.content_hash)?;

        // 5. Persist the assignment (previous = the config that was live).
        let assignment = Assignment {
            wallpaper_id: spec.wallpaper_id.clone(),
            kind: spec.kind,
            content: content.to_string_lossy().into_owned(),
            width: spec.width,
            height: spec.height,
            fps: spec.fps,
            scaling: spec.scaling,
            applied_at_unix_seconds: unix_seconds(),
            previous: Some(previous),
        };
        {
            let mut store = self
                .store
                .lock()
                .map_err(|_| ApplyError::Transaction("assignment store lock poisoned".into()))?;
            store
                .set(output_name, assignment.clone())
                .map_err(|error| {
                    ApplyError::Transaction(format!("persist assignment failed: {error}"))
                })?;
        }

        // 6. Switch the Plasma wallpaper config. The script is a pure
        // function of {desktop index, plugin name} — never wallpaper
        // content — and runs through a bounded, shell-less qdbus call.
        // The desktop guard makes a stale index fail visibly (evaluateScript
        // reports success even on script exceptions), and the verification
        // probe below is the second line of defense.
        let script = apply_script(desktop_index, KWE_PLUGIN)
            .map_err(|error| ApplyError::Transaction(format!("script error: {error}")))?;
        if let Err(error) = self.probe.evaluate_script(&script) {
            return Err(match error {
                ProbeError::Unreachable(detail) => ApplyError::ShellUnreachable(detail),
                other => ApplyError::Transaction(format!("wallpaper switch failed: {other}")),
            });
        }

        // 6b. Post-switch verification: the desktop must now report our
        // plugin; fail (and roll back) when it does not.
        self.verify_switch(output_name, desktop_index)?;

        Ok(json!({ "output": output_name, "applied": assignment }))
    }

    /// The wallpaper config to save as `previous` for a new apply: the
    /// live config — unless a stored assignment exists AND the live
    /// enumeration already shows our plugin, in which case the stored
    /// record's `previous` is carried forward so re-applying never
    /// destroys the original wallpaper.
    fn previous_for(&self, output: &OutputInfo) -> Result<PreviousWallpaper, ApplyError> {
        if output.wallpaper_plugin.as_deref() == Some(KWE_PLUGIN)
            && let Some(stored) = self.stored_assignment(&output.name)?
            && let Some(previous) = stored.previous
        {
            return Ok(previous);
        }
        Ok(PreviousWallpaper {
            wallpaper_plugin: output.wallpaper_plugin.clone().ok_or_else(|| {
                ApplyError::Transaction(format!(
                    "output {} has no wallpaper plugin to save as previous",
                    output.name
                ))
            })?,
            config_group: output.config_group.clone(),
            image: output.image.clone(),
        })
    }

    /// The stored assignment for one output, or None.
    fn stored_assignment(&self, output_name: &str) -> Result<Option<Assignment>, ApplyError> {
        let store = self
            .store
            .lock()
            .map_err(|_| ApplyError::Transaction("assignment store lock poisoned".into()))?;
        Ok(store.get(output_name).cloned())
    }

    /// Post-switch verification: re-probe and confirm the output's desktop
    /// really carries our plugin. `evaluateScript` exits 0 even on script
    /// exceptions, so the script alone could silently switch nothing; this
    /// probe (plus the script's desktop guard) makes that fail instead of
    /// reporting success over a no-op.
    fn verify_switch(&self, output_name: &str, desktop_index: usize) -> Result<(), ApplyError> {
        let outputs = self.enumerate_fresh()?;
        let Some(output) = outputs.iter().find(|info| info.name == output_name) else {
            return Err(ApplyError::Transaction(format!(
                "post-switch verification: output {output_name} is gone"
            )));
        };
        if output.wallpaper_plugin.as_deref() != Some(KWE_PLUGIN)
            || output.desktop_index != Some(desktop_index)
        {
            return Err(ApplyError::Transaction(format!(
                "post-switch verification failed: output {} reports plugin {:?} on desktop {:?}, expected {KWE_PLUGIN} on desktop {desktop_index}",
                output_name, output.wallpaper_plugin, output.desktop_index
            )));
        }
        Ok(())
    }

    /// Reverts the wallpaper config of one output to its saved `previous`
    /// (or to the stock image plugin when there is no assignment — the
    /// safe-mode contract: restore never leaves a desktop assigned to a
    /// daemon-owned renderer, and succeeds on any real output with a
    /// desktop containment while the shell is reachable). The assignment
    /// is cleared only after the restore script ran AND the verification
    /// probe confirmed the plugin switch — never on a silently no-op
    /// script, which would destroy the saved `previous`.
    pub fn restore(&self, output_name: String) -> Result<Value, ApplyError> {
        let outputs = self.enumerate_fresh()?;
        let output = outputs
            .iter()
            .find(|info| info.name == output_name)
            .ok_or_else(|| ApplyError::OutputMissing(output_name.clone()))?;
        let desktop_index = output.desktop_index.ok_or_else(|| {
            ApplyError::Transaction(format!("output {} has no desktop containment", output.name))
        })?;
        let stored = self.stored_assignment(&output_name)?;
        let target = restore_target(stored.clone(), stock_image_path());
        let script = restore_script(
            desktop_index,
            &target.wallpaper_plugin,
            &target.config_group,
            target.image.as_deref(),
        )
        .map_err(|error| ApplyError::RestoreFailed(format!("script error: {error}")))?;
        if let Err(error) = self.probe.evaluate_script(&script) {
            return Err(match error {
                ProbeError::Unreachable(detail) => ApplyError::ShellUnreachable(detail),
                other => ApplyError::RestoreFailed(other.to_string()),
            });
        }
        // Post-restore verification: only clear the record once the live
        // plugin matches the restore target.
        self.verify_restore(&output_name, &target.wallpaper_plugin)?;
        if stored.is_some()
            && let Ok(mut store) = self.store.lock()
        {
            let _ = store.remove(&output_name);
        }
        Ok(json!({
            "output": output_name,
            "mode": target.mode,
            "restored": {
                "wallpaper_plugin": target.wallpaper_plugin,
                "config_group": target.config_group,
                "image": target.image,
            },
            "stock_image": stock_image_path(),
        }))
    }

    /// Post-restore verification: the output's desktop must report the
    /// restore target plugin before the assignment is cleared.
    fn verify_restore(&self, output_name: &str, expected_plugin: &str) -> Result<(), ApplyError> {
        let outputs = self.enumerate_fresh()?;
        let Some(output) = outputs.iter().find(|info| info.name == output_name) else {
            return Err(ApplyError::RestoreFailed(format!(
                "post-restore verification: output {output_name} is gone"
            )));
        };
        if output.wallpaper_plugin.as_deref() != Some(expected_plugin) {
            return Err(ApplyError::RestoreFailed(format!(
                "post-restore verification failed: output {} reports plugin {:?}, expected {expected_plugin:?}",
                output_name, output.wallpaper_plugin
            )));
        }
        Ok(())
    }

    /// Every stored assignment.
    pub fn assignments(&self) -> Result<Value, ApplyError> {
        let store = self
            .store
            .lock()
            .map_err(|_| ApplyError::Transaction("assignment store lock poisoned".into()))?;
        serde_json::to_value(PersistedAssignments {
            schema_version: 1,
            outputs: store.all().clone(),
        })
        .map_err(|error| ApplyError::Transaction(format!("serialize assignments: {error}")))
    }

    fn catalog_item(&self, wallpaper_id: &str) -> Result<CatalogItem, ApplyError> {
        let guard = self
            .catalog
            .read()
            .map_err(|_| ApplyError::Transaction("catalog lock poisoned".into()))?;
        let Some(item) = guard
            .items
            .iter()
            .find(|item| item.workshop_id == wallpaper_id)
        else {
            return Err(ApplyError::UnknownWallpaper(wallpaper_id.to_string()));
        };
        let usable = item.kind != ProjectKind::Invalid
            && item.kind != ProjectKind::Unknown
            && matches!(
                item.workshop_state.as_str(),
                "local" | "subscribed_installed"
            );
        if !usable {
            return Err(ApplyError::UnknownWallpaper(wallpaper_id.to_string()));
        }
        Ok(item.clone())
    }

    fn wait_for_promotion(&self, wallpaper_id: &str, content_hash: &str) -> Result<(), ApplyError> {
        let deadline = Instant::now() + self.promotion_timeout;
        loop {
            let status = self.supervisor.status().map_err(|error| {
                ApplyError::Transaction(format!("renderer.status failed: {error}"))
            })?;
            let ours = status.requested_wallpaper_id.as_deref() == Some(wallpaper_id)
                && status.requested_content_hash.as_deref() == Some(content_hash);
            // Fail fast on ownership change: a renderer we did not start
            // is running (the playlist session thread replaced ours);
            // waiting for ours to promote would only mislead with a
            // timeout long after the handoff happened.
            if !ours && !matches!(status.phase, WorkerPhase::Idle) {
                return Err(ApplyError::Transaction(format!(
                    "renderer ownership changed (requested {}:{}, expected {wallpaper_id}:{content_hash})",
                    status.requested_wallpaper_id.as_deref().unwrap_or("none"),
                    status.requested_content_hash.as_deref().unwrap_or("none"),
                )));
            }
            match promotion_verdict(status.phase, ours, status.last_failure_detail.as_deref()) {
                Some(Ok(())) => return Ok(()),
                Some(Err(detail)) => {
                    return Err(ApplyError::Transaction(format!(
                        "renderer did not promote ({detail})"
                    )));
                }
                None => {
                    if Instant::now() >= deadline {
                        return Err(ApplyError::Transaction(format!(
                            "renderer did not promote within {} ms",
                            self.promotion_timeout.as_millis()
                        )));
                    }
                    std::thread::sleep(PROMOTION_POLL);
                }
            }
        }
    }

    /// Fail fast when the supervisor's requested renderer no longer
    /// matches ours (the playlist session can replace our renderer
    /// mid-transaction; the check is repeated immediately before the
    /// persist/switch steps).
    fn ensure_ours(&self, wallpaper_id: &str, content_hash: &str) -> Result<(), ApplyError> {
        let status = self
            .supervisor
            .status()
            .map_err(|error| ApplyError::Transaction(format!("renderer.status failed: {error}")))?;
        let ours = status.requested_wallpaper_id.as_deref() == Some(wallpaper_id)
            && status.requested_content_hash.as_deref() == Some(content_hash);
        if !ours {
            return Err(ApplyError::Transaction(format!(
                "renderer ownership changed (requested {}:{}, expected {wallpaper_id}:{content_hash})",
                status.requested_wallpaper_id.as_deref().unwrap_or("none"),
                status.requested_content_hash.as_deref().unwrap_or("none"),
            )));
        }
        Ok(())
    }
}

/// The playlist session's apply lane: the full M4a apply transaction, with
/// the kind/content/width/height/fps derived FROM THE CATALOG (a playlist
/// entry is applied exactly as the catalog describes it — the RPC-style
/// client-supplied content match rule does not apply), and the output
/// resolved from the session's configured output or the last assignment of
/// this playlist's entries. The transaction, its single-transaction lock,
/// and its rollback are shared with `wallpaper.apply` — an entry change
/// and a user apply can never run concurrently.
impl PlaylistApplyLane for ApplyHandle {
    fn apply_playlist(
        &self,
        output: Option<String>,
        wallpaper_id: String,
        playlist_entries: &BTreeSet<String>,
        applied: Option<&str>,
    ) -> Result<Value, ApplyError> {
        // The lane shares the single apply transaction lock with
        // wallpaper.apply: a user apply in flight wins the slot and the
        // session backs off instead of interleaving start/ensure_ours
        // steps. User-apply precedence comes from the session's verdict
        // (yield while a foreign renderer is live); this lock only closes
        // the mid-transaction race.
        let _guard = self.acquire_apply_lock()?;
        // TOCTOU closure (Finding 1): the session computed its verdict from
        // a supervisor.status() read taken BEFORE the lock. A user apply that
        // completed in that window is now live — re-read the state with the
        // lock held and yield to it instead of displacing a fresh user
        // renderer. The session's own stale renderer (`requested == applied`)
        // is NOT foreign: the entry-change hard cut must still displace it.
        if let Ok(status) = self.supervisor.status()
            && foreign_renderer_live(&status, &wallpaper_id, applied)
        {
            return Err(ApplyError::Yielded(format!(
                "foreign renderer {} is live; the playlist yields",
                status.requested_wallpaper_id.as_deref().unwrap_or("none")
            )));
        }
        let resolved_output = self.resolve_playlist_output(output.as_deref(), playlist_entries)?;

        // Catalog lookup: the single validation point shared with
        // wallpaper.apply. A playlist entry is always applied as the
        // catalog says it is — there is no client-supplied kind/content.
        let item = self.catalog_item(&wallpaper_id)?;
        let kind = renderer_kind_for(&item).ok_or_else(|| {
            ApplyError::Incompatible(format!(
                "{} is not an apply-able project kind",
                item.workshop_id
            ))
        })?;
        let content = catalog_content_path(&item, kind);
        if content.to_string_lossy().chars().count() > MAX_CONTENT_CHARS {
            return Err(ApplyError::Invalid(format!(
                "catalog content path exceeds {MAX_CONTENT_CHARS} characters"
            )));
        }
        let mut spec = StartSpec {
            wallpaper_id: wallpaper_id.clone(),
            content_hash: "pending".into(),
            width: default_apply_width(),
            height: default_apply_height(),
            fps: default_apply_fps(),
            kind,
            content: Some(content_spec_for(kind, &content)),
            test_fault: None,
            stderr_lines: None,
            scaling: ScalingMode::Aspect,
        }
        .into_validated(self.scene_assets_dir.as_deref())
        .map_err(|error| ApplyError::Invalid(error.to_string()))?;
        spec.content_hash = content_hash_for(&item, &content);

        // Fresh enumeration; the resolved output must be live and have a
        // desktop (same rule as wallpaper.apply).
        let outputs = self.enumerate_fresh()?;
        let output_info = outputs
            .iter()
            .find(|info| info.name == resolved_output)
            .ok_or_else(|| ApplyError::OutputMissing(resolved_output.clone()))?;
        let desktop_index = output_info.desktop_index.ok_or_else(|| {
            ApplyError::Transaction(format!(
                "output {} has no desktop containment",
                output_info.name
            ))
        })?;
        let previous = self.previous_for(output_info)?;
        // F1: canvas from the output; the scaling mode is the output's
        // current one (a playlist advance keeps what the user chose there).
        let (width, height) = frame_size_for(None, None, output_info.geometry);
        spec.width = width;
        spec.height = height;

        // 4+. Start the renderer; EVERY failure from here on rolls back
        // exactly like wallpaper.apply (same ownership guard, same store
        // revert).
        let old_assignment = self.stored_assignment(&resolved_output)?;
        spec.scaling = old_assignment
            .as_ref()
            .map(|assignment| assignment.scaling)
            .unwrap_or_default();
        // F2: the output's chosen frame-rate limit survives playlist advances.
        if let Some(fps) = old_assignment
            .as_ref()
            .map(|assignment| assignment.fps)
            .filter(|fps| (1..=240).contains(fps))
        {
            spec.fps = fps;
        }
        let result = self.complete_apply(
            &spec,
            &content,
            &resolved_output,
            desktop_index,
            previous,
            false,
        );
        if let Err(error) = result {
            self.rollback_after_failure(&spec, &resolved_output, old_assignment);
            return Err(error);
        }
        result
    }
}

/// Pure promotion-state verdict: `Ok(())` once OUR renderer is live,
/// `Err(detail)` on a terminal failure, `None` while still transitioning.
/// The wallpaper identity check keeps a concurrently started renderer (the
/// playlist session thread) from being mistaken for ours.
fn promotion_verdict(
    phase: WorkerPhase,
    ours: bool,
    last_failure_detail: Option<&str>,
) -> Option<Result<(), String>> {
    match (phase, ours) {
        (WorkerPhase::Live, true) | (WorkerPhase::AwaitingAck, true) => Some(Ok(())),
        (WorkerPhase::RolledBack | WorkerPhase::Quarantined | WorkerPhase::Stopped, _) => {
            let detail = last_failure_detail
                .map(str::to_string)
                .unwrap_or_else(|| phase_name(&phase).to_string());
            Some(Err(detail))
        }
        _ => None,
    }
}

fn phase_name(phase: &WorkerPhase) -> &'static str {
    match phase {
        WorkerPhase::Idle => "idle",
        WorkerPhase::Starting => "starting",
        WorkerPhase::Canary => "canary",
        WorkerPhase::Live => "live",
        WorkerPhase::Restarting => "restarting",
        WorkerPhase::AwaitingAck => "awaiting_ack",
        WorkerPhase::RolledBack => "rolled_back",
        WorkerPhase::Stopped => "stopped",
        WorkerPhase::Quarantined => "quarantined",
    }
}

/// Builds the validated StartSpec from apply params (with a placeholder
/// content hash; the catalog lookup replaces it once the item is known).
fn build_apply_spec(
    params: &ApplyWallpaperParams,
    assets_dir: Option<&Path>,
) -> Result<StartSpec, ApplyError> {
    if params.kind == RendererKind::Test {
        return Err(ApplyError::Invalid(
            "wallpaper.apply does not accept the test renderer kind".into(),
        ));
    }
    // Content is a boundary input even though the renderer runs the
    // catalog content: a supplied path is validated and bounded here, and
    // matched against the catalog after the lookup (never truncated).
    if let Some(content) = &params.content {
        let len = content.to_string_lossy().chars().count();
        if len == 0 || len > MAX_CONTENT_CHARS {
            return Err(ApplyError::Invalid(format!(
                "content path must be 1..={MAX_CONTENT_CHARS} characters"
            )));
        }
    }
    let Some(content) = params.content.as_ref() else {
        return Ok(StartSpec {
            wallpaper_id: params.wallpaper_id.clone(),
            content_hash: "pending".into(),
            width: params.width.unwrap_or(default_apply_width()),
            height: params.height.unwrap_or(default_apply_height()),
            fps: params.fps,
            kind: params.kind,
            content: None,
            test_fault: None,
            stderr_lines: None,
            scaling: params.scaling,
        });
    };
    let content = match params.kind {
        RendererKind::Video => ContentSpec::Video {
            path: content.clone(),
        },
        RendererKind::Web => ContentSpec::Web {
            root: content.clone(),
        },
        RendererKind::Scene => ContentSpec::Scene {
            path: content.clone(),
        },
        RendererKind::Test => unreachable!("test kind rejected above"),
    };
    StartSpec {
        wallpaper_id: params.wallpaper_id.clone(),
        content_hash: "pending".into(),
        width: params.width.unwrap_or(default_apply_width()),
        height: params.height.unwrap_or(default_apply_height()),
        fps: params.fps,
        kind: params.kind,
        content: Some(content),
        test_fault: None,
        stderr_lines: None,
        scaling: params.scaling,
    }
    .into_validated(assets_dir)
    .map_err(|error| ApplyError::Invalid(error.to_string()))
}

/// The content path the daemon starts the renderer with for a catalog
/// item (the catalog content is authoritative): the item's runnable entry
/// for video/scene (or its scene.json when no entry is declared), and the
/// content root for web (the renderer serves the whole root).
pub(crate) fn catalog_content_path(item: &CatalogItem, kind: RendererKind) -> PathBuf {
    if kind == RendererKind::Web {
        return item.content_root.clone();
    }
    match &item.entry_file {
        Some(entry) => entry.clone(),
        None => item.content_root.join("scene.json"),
    }
}

/// The validated ContentSpec for a kind and a resolved content path.
fn content_spec_for(kind: RendererKind, path: &std::path::Path) -> ContentSpec {
    match kind {
        RendererKind::Video => ContentSpec::Video {
            path: path.to_path_buf(),
        },
        RendererKind::Web => ContentSpec::Web {
            root: path.to_path_buf(),
        },
        RendererKind::Scene => ContentSpec::Scene {
            path: path.to_path_buf(),
        },
        RendererKind::Test => unreachable!("test kind rejected before content resolution"),
    }
}

/// Stable content identity for the supervisor's quarantine key: the
/// catalog item's project-metadata hash when present (it is stable across
/// rescans), else the SHA-256 of the canonical content path.
pub(crate) fn content_hash_for(item: &CatalogItem, content: &std::path::Path) -> String {
    if let Some(hash) = &item.metadata_hash {
        return hash.clone();
    }
    let canonical = std::fs::canonicalize(content).unwrap_or_else(|_| content.to_path_buf());
    hex::encode(Sha256::digest(canonical.as_os_str().as_encoded_bytes()))
}

/// Lowercase display name of a project kind (ProjectKind has no `as_str`;
/// its Debug form is the PascalCase serde name, which is exactly what the
/// wire format spells in snake_case).
fn project_kind_name(kind: &ProjectKind) -> String {
    format!("{kind:?}").to_lowercase()
}

fn renderer_kind_for(item: &CatalogItem) -> Option<RendererKind> {
    match item.kind {
        ProjectKind::Scene => Some(RendererKind::Scene),
        ProjectKind::Video => Some(RendererKind::Video),
        ProjectKind::Web => Some(RendererKind::Web),
        ProjectKind::Unknown | ProjectKind::Invalid => None,
    }
}

/// What a restore should do, decided purely: the saved `previous` when an
/// assignment exists, else the stock image plugin (with a present-on-system
/// image path, or none). Pure so the contract is unit-testable.
struct RestoreTarget {
    wallpaper_plugin: String,
    config_group: Vec<String>,
    image: Option<String>,
    mode: &'static str,
}

fn restore_target(stored: Option<Assignment>, stock_image: Option<String>) -> RestoreTarget {
    if let Some(assignment) = stored
        && let Some(previous) = assignment.previous
    {
        return RestoreTarget {
            wallpaper_plugin: previous.wallpaper_plugin,
            config_group: previous.config_group,
            image: previous.image,
            mode: "assignment",
        };
    }
    RestoreTarget {
        wallpaper_plugin: IMAGE_PLUGIN.into(),
        config_group: vec!["Wallpaper".into(), IMAGE_PLUGIN.into(), "General".into()],
        image: stock_image,
        mode: "stock",
    }
}

/// First stock image present on this system, or None (the plugin still
/// applies its theme default).
fn stock_image_path() -> Option<String> {
    STOCK_IMAGE_CANDIDATES
        .iter()
        .find(|candidate| Path::new(candidate).is_file())
        .map(|candidate| candidate.to_string())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::persist::unix_nanos;

    fn temporary_directory(label: &str) -> PathBuf {
        std::env::temp_dir().join(format!(
            "kwe-apply-{label}-{}-{}",
            std::process::id(),
            unix_nanos()
        ))
    }

    fn invalid_siblings(directory: &Path) -> Vec<String> {
        std::fs::read_dir(directory)
            .unwrap()
            .filter_map(|entry| entry.ok())
            .map(|entry| entry.file_name().to_string_lossy().into_owned())
            .filter(|name| name.starts_with(&format!("{ASSIGNMENTS_FILE}.invalid-")))
            .collect()
    }

    fn sample_assignment() -> Assignment {
        Assignment {
            wallpaper_id: "431960-123".into(),
            kind: RendererKind::Video,
            content: "/media/steam/workshop/content/431960/123/scene.mp4".into(),
            width: 960,
            height: 540,
            fps: 30,
            scaling: ScalingMode::Aspect,
            applied_at_unix_seconds: 1_787_188_979,
            previous: Some(PreviousWallpaper {
                wallpaper_plugin: "org.kde.image".into(),
                config_group: vec!["Wallpaper".into(), "org.kde.image".into(), "General".into()],
                image: Some("file:///usr/share/wallpapers/fallback.png".into()),
            }),
        }
    }

    // -- store ------------------------------------------------------------

    #[test]
    fn store_round_trips_through_the_file() {
        let root = temporary_directory("round-trip");
        let mut store = AssignmentStore::open(&root).unwrap();
        assert!(store.all().is_empty());
        store.set("DP-1", sample_assignment()).unwrap();
        assert_eq!(store.get("DP-1"), Some(&sample_assignment()));
        let mut reopened = AssignmentStore::open(&root).unwrap();
        assert_eq!(reopened.get("DP-1"), Some(&sample_assignment()));
        assert_eq!(reopened.all().len(), 1);
        assert!(reopened.remove("DP-1").unwrap());
        assert!(!reopened.remove("DP-1").unwrap());
        assert!(AssignmentStore::open(&root).unwrap().all().is_empty());
        std::fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn store_rejects_invalid_outputs_records_and_the_17th_output() {
        let root = temporary_directory("bounds");
        let mut store = AssignmentStore::open(&root).unwrap();
        let mut assignment = sample_assignment();
        // Hostile output names are rejected (they could reach a script or
        // a file name).
        for output in [
            "",
            "../escape",
            "bad space",
            "tab\toutput",
            &"x".repeat(129),
        ] {
            let error = format!("{}", store.set(output, sample_assignment()).unwrap_err());
            assert!(
                error.contains("output must be 1..=128"),
                "unexpected error for {output:?}: {error}"
            );
        }
        // Invalid records are rejected, not persisted.
        assignment.wallpaper_id = "../escape".into();
        let error = format!("{}", store.set("DP-1", assignment).unwrap_err());
        assert!(error.contains("wallpaper_id must be 1..=128"), "{error}");
        let mut assignment = sample_assignment();
        assignment.kind = RendererKind::Test;
        let error = format!("{}", store.set("DP-1", assignment).unwrap_err());
        assert!(error.contains("video, web, or scene"), "{error}");
        // The 17th output hits the count bound.
        for index in 0..MAX_ASSIGNED_OUTPUTS {
            store
                .set(&format!("output-{index:02}"), sample_assignment())
                .unwrap();
        }
        let error = format!(
            "{}",
            store.set("output-16", sample_assignment()).unwrap_err()
        );
        assert!(error.contains("safety limit"), "{error}");
        assert_eq!(store.all().len(), MAX_ASSIGNED_OUTPUTS);
        std::fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn a_corrupt_file_is_quarantined_and_the_store_starts_fresh() {
        let root = temporary_directory("corrupt");
        std::fs::create_dir_all(&root).unwrap();
        // Unparsable JSON.
        std::fs::write(root.join(ASSIGNMENTS_FILE), b"{not json").unwrap();
        let store = AssignmentStore::open(&root).unwrap();
        assert!(store.all().is_empty());
        assert_eq!(invalid_siblings(&root).len(), 1);
        // Unknown fields are corrupt.
        std::fs::write(
            root.join(ASSIGNMENTS_FILE),
            r#"{"schema_version":1,"outputs":{"DP-1":{"wallpaper_id":"a","kind":"video","content":"/x","width":960,"height":540,"fps":30,"applied_at_unix_seconds":1,"previous":null,"bogus":1}}}"#,
        )
        .unwrap();
        let store = AssignmentStore::open(&root).unwrap();
        assert!(store.all().is_empty());
        assert_eq!(invalid_siblings(&root).len(), 2);
        // An invalid wallpaper id is corrupt.
        std::fs::write(
            root.join(ASSIGNMENTS_FILE),
            r#"{"schema_version":1,"outputs":{"DP-1":{"wallpaper_id":"../escape","kind":"video","content":"/x","width":960,"height":540,"fps":30,"applied_at_unix_seconds":1,"previous":null}}}"#,
        )
        .unwrap();
        let store = AssignmentStore::open(&root).unwrap();
        assert!(store.all().is_empty());
        assert_eq!(invalid_siblings(&root).len(), 3);
        // A test-kind assignment is corrupt (never assignable).
        std::fs::write(
            root.join(ASSIGNMENTS_FILE),
            r#"{"schema_version":1,"outputs":{"DP-1":{"wallpaper_id":"a","kind":"test","content":"/x","width":960,"height":540,"fps":30,"applied_at_unix_seconds":1,"previous":null}}}"#,
        )
        .unwrap();
        let store = AssignmentStore::open(&root).unwrap();
        assert!(store.all().is_empty());
        assert_eq!(invalid_siblings(&root).len(), 4);
        // Oversized input is quarantined without reading it all.
        std::fs::write(
            root.join(ASSIGNMENTS_FILE),
            vec![b'x'; (MAX_ASSIGNMENT_BYTES + 1) as usize],
        )
        .unwrap();
        let store = AssignmentStore::open(&root).unwrap();
        assert!(store.all().is_empty());
        assert_eq!(invalid_siblings(&root).len(), 5);
        // A wrong schema version is corrupt.
        std::fs::write(
            root.join(ASSIGNMENTS_FILE),
            r#"{"schema_version":2,"outputs":{}}"#,
        )
        .unwrap();
        let store = AssignmentStore::open(&root).unwrap();
        assert!(store.all().is_empty());
        assert_eq!(invalid_siblings(&root).len(), 6);
        std::fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn a_failed_save_leaves_the_previous_file_intact() {
        let root = temporary_directory("atomic");
        let mut store = AssignmentStore::open(&root).unwrap();
        store.set("DP-1", sample_assignment()).unwrap();
        let before = std::fs::read(root.join(ASSIGNMENTS_FILE)).unwrap();
        let oversized = PersistedAssignments {
            schema_version: 1,
            outputs: BTreeMap::from([(
                format!("x-{}", "y".repeat(MAX_ASSIGNMENT_BYTES as usize)),
                sample_assignment(),
            )]),
        };
        let error = format!("{}", store.save(&oversized).unwrap_err());
        assert!(error.contains("exceeds"), "unexpected error: {error}");
        assert_eq!(std::fs::read(root.join(ASSIGNMENTS_FILE)).unwrap(), before);
        // The in-memory state was untouched too.
        assert!(store.get("DP-1").is_some());
        std::fs::remove_dir_all(root).unwrap();
    }

    // -- scripts ----------------------------------------------------------

    #[test]
    fn apply_and_restore_scripts_are_exact_strings() {
        // The desktop guard fails visibly on a stale index (evaluateScript
        // reports success even on script exceptions).
        assert_eq!(
            apply_script(1, "org.kde.kwe.wallpaper").unwrap(),
            "var d = desktops()[1]; if (!d) throw \"no desktop 1\"; d.wallpaperPlugin = \"org.kde.kwe.wallpaper\";"
        );
        assert_eq!(
            restore_script(
                1,
                "org.kde.image",
                &["Wallpaper".into(), "org.kde.image".into(), "General".into()],
                Some("file:///usr/share/wallpapers/cachy.png"),
            )
            .unwrap(),
            "var d = desktops()[1];\n\
             if (!d) throw \"no desktop 1\";\n\
             d.currentConfigGroup = [\"Wallpaper\", \"org.kde.image\", \"General\"];\n\
             d.writeConfig(\"Image\", \"file:///usr/share/wallpapers/cachy.png\");\n\
             d.wallpaperPlugin = \"org.kde.image\";"
        );
        // A null image skips the writeConfig line entirely: the restore is
        // then a live no-op when the plugin already matches.
        let script = restore_script(
            0,
            "org.kde.image",
            &["Wallpaper".into(), "org.kde.image".into(), "General".into()],
            None,
        )
        .unwrap();
        assert_eq!(
            script,
            "var d = desktops()[0];\n\
             if (!d) throw \"no desktop 0\";\n\
             d.currentConfigGroup = [\"Wallpaper\", \"org.kde.image\", \"General\"];\n\
             d.wallpaperPlugin = \"org.kde.image\";"
        );
        assert!(!script.contains("writeConfig"));
    }

    #[test]
    fn scripts_guard_against_a_stale_desktop_index() {
        // A stale desktop index must fail visibly (the throw aborts the
        // script; the daemon's verification probe then catches the no-op).
        let apply = apply_script(7, "org.kde.kwe.wallpaper").unwrap();
        // The guard precedes the assignment, so a stale index aborts the
        // script before anything is switched.
        assert!(apply.starts_with(
            "var d = desktops()[7]; if (!d) throw \"no desktop 7\"; d.wallpaperPlugin"
        ));
        let restore = restore_script(7, "org.kde.image", &["Wallpaper".into()], None).unwrap();
        assert!(restore.contains("if (!d) throw \"no desktop 7\";\n"));
    }

    #[test]
    fn scripts_reject_hostile_plugins_groups_and_images() {
        // Quotes/backslashes/semicolons/spaces/empties never reach a script.
        for plugin in [
            "",
            "org.kde.image\"}; exploit(); {",
            "a\\b",
            "a b",
            "a;b",
            &"x".repeat(129),
        ] {
            let error = apply_script(0, plugin).unwrap_err().to_string();
            assert!(
                error.contains("wallpaper_plugin must be 1..=128"),
                "unexpected error for {plugin:?}: {error}"
            );
            let error = restore_script(0, plugin, &["Wallpaper".into()], None)
                .unwrap_err()
                .to_string();
            assert!(
                error.contains("wallpaper_plugin must be 1..=128"),
                "unexpected error for {plugin:?}: {error}"
            );
        }
        // Empty and hostile config groups are rejected.
        let error = restore_script(0, "org.kde.image", &[], None)
            .unwrap_err()
            .to_string();
        assert!(error.contains("bounded config group"), "{error}");
        let error = restore_script(
            0,
            "org.kde.image",
            &["Wallpaper".into(), "\"}; exploit();".into()],
            None,
        )
        .unwrap_err()
        .to_string();
        assert!(error.contains("config_group must be 1..=128"), "{error}");
        // Hostile image values are escaped (below) or rejected (oversize).
        let error = restore_script(
            0,
            "org.kde.image",
            &["Wallpaper".into()],
            Some(&"x".repeat(MAX_IMAGE_CHARS + 1)),
        )
        .unwrap_err()
        .to_string();
        assert!(error.contains("bounded image path"), "{error}");
    }

    #[test]
    fn restore_script_escapes_hostile_image_values() {
        let hostile = "file:///x\"; process.exit(1); // \\ and \n and \u{2028}";
        let script = restore_script(
            0,
            "org.kde.image",
            &["Wallpaper".into(), "org.kde.image".into(), "General".into()],
            Some(hostile),
        )
        .unwrap();
        let expected = "var d = desktops()[0];\n\
                        if (!d) throw \"no desktop 0\";\n\
                        d.currentConfigGroup = [\"Wallpaper\", \"org.kde.image\", \"General\"];\n\
                        d.writeConfig(\"Image\", \"file:///x\\\"; process.exit(1); // \\\\ and \\n and \\u2028\");\n\
                        d.wallpaperPlugin = \"org.kde.image\";";
        assert_eq!(script, expected);
        // The hostile text cannot break out of the string literal: the
        // quote is escaped, the backslash is doubled, and the line
        // terminators are literal escape sequences. The raw `";` breakout
        // (unescaped quote immediately before a semicolon) and the raw
        // U+2028 never appear in the script.
        assert!(!script.contains("x\";"));
        assert!(!script.contains('\u{2028}'));
        assert!(script.contains("x\\\";"));
    }

    #[test]
    fn probe_script_embeds_only_validated_connectors() {
        let script = probe_script(&["DP-1".into(), "HDMI-A-1".into()]).unwrap();
        assert!(script.starts_with("var d = desktops();"));
        assert!(script.ends_with("print(JSON.stringify({desktops: out, connectors: c}));"));
        assert!(script.contains("var c = {\"DP-1\": screenForConnector(\"DP-1\"), \"HDMI-A-1\": screenForConnector(\"HDMI-A-1\")};"));
        // The wallpaper data is only ever read inside the fixed template;
        // no live value is interpolated into the script.
        assert_eq!(script.matches("wallpaperPlugin").count(), 1);
        // A connector-less system still yields a valid JS object literal.
        let empty = probe_script(&[]).unwrap();
        assert!(empty.contains("var c = {};"));
        assert!(empty.contains("print(JSON.stringify({desktops: out, connectors: c}));"));
        // Hostile connector names (quotes, backslashes) are rejected, so
        // system output names cannot inject into the script.
        for hostile in ["DP\"}; exploit();", "a\\b", "DP-1\"; process.exit(1);"] {
            assert!(probe_script(&[hostile.into()]).is_err());
        }
    }

    // -- parsing and mapping ---------------------------------------------

    #[test]
    fn kscreen_doctor_output_parses() {
        let sample = "Output: 1 DP-1 62b8c814-6503-41cf-a04d-8743a967c99b\n\
                      \tenabled\n\
                      \tconnected\n\
                      \tpriority 1\n\
                      \tGeometry: 0,0 2926x823\n\
                      Output: 2 HDMI-A-1 6e8f4047-91a0-4ba2-9622-4d1f1a24b7e4\n\
                      \tdisabled\n\
                      \tdisconnected\n\
                      \tGeometry: 0,0 1920x1080\n\
                      Output: 3 eDP-1 12345678-1234-1234-1234-123456789012\n\
                      \tenabled\n\
                      \tconnected\n\
                      \tMode: 1920x1080@60.00*\n";
        let outputs = parse_kscreen_doctor(sample);
        assert_eq!(outputs.len(), 3);
        assert_eq!(outputs[0].name, "DP-1");
        assert!(outputs[0].enabled && outputs[0].connected);
        assert_eq!(outputs[0].geometry, Some([0, 0, 2926, 823]));
        assert_eq!(outputs[1].name, "HDMI-A-1");
        assert!(!outputs[1].enabled && !outputs[1].connected);
        assert_eq!(outputs[1].geometry, Some([0, 0, 1920, 1080]));
        assert_eq!(outputs[2].name, "eDP-1");
        assert_eq!(outputs[2].geometry, None);
        // ANSI color escapes from a tty run are stripped.
        let colored = "\u{1b}[1mOutput: 1 DP-1 62b8c814-6503-41cf-a04d-8743a967c99b\u{1b}[0m\n\t\u{1b}[32menabled\u{1b}[0m\n";
        let outputs = parse_kscreen_doctor(colored);
        assert_eq!(outputs.len(), 1);
        assert!(outputs[0].enabled);
        // Garbage lines are ignored.
        assert!(parse_kscreen_doctor("hello\nworld\n").is_empty());
    }

    #[test]
    fn probe_reply_parses_and_rejects_unknown_fields() {
        let reply = r#"{"desktops":[{"index":1,"id":111,"screen":0,"wp":"org.kde.image","image":"file:///x.png"}],"connectors":{"DP-1":0,"HDMI-A-1":1}}"#;
        let parsed = parse_probe_reply(reply).unwrap();
        assert_eq!(parsed.desktops.len(), 1);
        assert_eq!(parsed.desktops[0].index, 1);
        assert_eq!(parsed.desktops[0].id, 111);
        assert_eq!(parsed.connectors["DP-1"], 0);
        // Empty replies (a shell that printed nothing) are rejected.
        assert!(parse_probe_reply("").is_err());
        assert!(parse_probe_reply("   ").is_err());
        // Unknown fields fail closed: a future plasma reply format is not
        // silently accepted.
        assert!(parse_probe_reply(
            r#"{"desktops":[{"index":0,"id":1,"screen":0,"wp":"a","image":null,"bogus":1}],"connectors":{}}"#
        )
        .is_err());
    }

    #[test]
    fn assemble_outputs_maps_connectors_to_desktops_and_skips_orphans() {
        let system = vec![
            SystemOutput {
                name: "DP-1".into(),
                enabled: true,
                connected: true,
                geometry: Some([0, 0, 2926, 823]),
            },
            SystemOutput {
                name: "HDMI-A-1".into(),
                enabled: true,
                connected: true,
                geometry: None,
            },
        ];
        let desktops = vec![
            ProbeDesktop {
                index: 1,
                id: 111,
                screen: 0,
                wp: "org.kde.kwe.wallpaper".into(),
                image: None,
            },
            // Orphaned containment: no connector maps to screen -1.
            ProbeDesktop {
                index: 0,
                id: 105,
                screen: -1,
                wp: "org.kde.image".into(),
                image: Some("file:///old.png".into()),
            },
        ];
        let connectors = BTreeMap::from([("DP-1".into(), 0), ("HDMI-A-1".into(), 2)]);
        let outputs = assemble_outputs(system, desktops, connectors);
        assert_eq!(outputs.len(), 2);
        // DP-1 maps onto desktop 111; the orphan is nowhere.
        assert_eq!(outputs[0].desktop_id, Some(111));
        assert_eq!(outputs[0].desktop_index, Some(1));
        assert_eq!(
            outputs[0].wallpaper_plugin.as_deref(),
            Some("org.kde.kwe.wallpaper")
        );
        assert_eq!(
            outputs[0].config_group,
            vec![
                "Wallpaper".to_string(),
                "org.kde.kwe.wallpaper".to_string(),
                "General".to_string()
            ]
        );
        // HDMI-A-1 has no desktop on screen 2: desktop fields are null and
        // the empty-string image is normalized to None.
        assert_eq!(outputs[1].desktop_id, None);
        assert_eq!(outputs[1].desktop_index, None);
        assert_eq!(outputs[1].image, None);
    }

    #[test]
    fn connector_mapping_to_negative_screen_never_binds_an_orphan() {
        // screenForConnector reports -1 for an unknown/unmapped connector;
        // the orphaned desktop also carries screen -1. It must never match:
        // a real output would otherwise bind the orphan's containment and
        // capture the wrong previous config.
        let system = vec![SystemOutput {
            name: "DP-2".into(),
            enabled: true,
            connected: true,
            geometry: None,
        }];
        let desktops = vec![ProbeDesktop {
            index: 3,
            id: 105,
            screen: -1,
            wp: "org.kde.image".into(),
            image: Some("file:///old.png".into()),
        }];
        let connectors = BTreeMap::from([("DP-2".into(), -1)]);
        let outputs = assemble_outputs(system, desktops, connectors);
        assert_eq!(outputs.len(), 1);
        assert_eq!(outputs[0].screen, -1);
        assert_eq!(outputs[0].desktop_id, None);
        assert_eq!(outputs[0].desktop_index, None);
        assert_eq!(outputs[0].wallpaper_plugin, None);
        assert_eq!(outputs[0].image, None);
    }

    #[test]
    fn catalog_content_path_resolves_entries_roots_and_scene_json() {
        fn item(content_root: &str, entry_file: Option<&str>, kind: ProjectKind) -> CatalogItem {
            CatalogItem {
                workshop_id: "1".into(),
                title: "Synthetic".into(),
                kind,
                compatibility: kwe_core::Compatibility::RendererDependent,
                compatibility_detail: String::new(),
                content_root: PathBuf::from(content_root),
                project_file: PathBuf::from(content_root).join("project.json"),
                entry_file: entry_file.map(PathBuf::from),
                preview_file: None,
                metadata_hash: None,
                tags: Vec::new(),
                requested_permissions: Vec::new(),
                workshop_state: "local".into(),
                workshop_progress: None,
                diagnostics: Vec::new(),
            }
        }
        // A declared entry wins (the runnable scene.json or scene.pkg).
        assert_eq!(
            catalog_content_path(
                &item("/w/1", Some("/w/1/scene.json"), ProjectKind::Scene),
                RendererKind::Scene
            ),
            PathBuf::from("/w/1/scene.json")
        );
        // A scene without a declared entry runs its scene.json.
        assert_eq!(
            catalog_content_path(&item("/w/2", None, ProjectKind::Scene), RendererKind::Scene),
            PathBuf::from("/w/2/scene.json")
        );
        // The web renderer serves the whole content root, not the entry.
        assert_eq!(
            catalog_content_path(
                &item("/w/3", Some("/w/3/index.html"), ProjectKind::Web),
                RendererKind::Web
            ),
            PathBuf::from("/w/3")
        );
        // Video uses its declared media entry.
        assert_eq!(
            catalog_content_path(
                &item("/w/4", Some("/w/4/video.mp4"), ProjectKind::Video),
                RendererKind::Video
            ),
            PathBuf::from("/w/4/video.mp4")
        );
    }

    // -- cache, restore target, promotion verdict, path lookup -----------

    #[test]
    fn output_cache_is_fresh_for_five_seconds_never_longer() {
        let mut cache = OutputCache::stale();
        let now = Instant::now();
        assert!(!cache.fresh(now));
        cache.entries.push(OutputInfo {
            name: "DP-1".into(),
            screen: 0,
            desktop_id: Some(111),
            desktop_index: Some(1),
            geometry: None,
            enabled: true,
            connected: true,
            wallpaper_plugin: Some("org.kde.image".into()),
            config_group: vec!["Wallpaper".into(), "org.kde.image".into(), "General".into()],
            image: None,
        });
        cache.cached_at = Some(now);
        assert!(cache.fresh(now + OUTPUT_CACHE_TTL - Duration::from_millis(1)));
        assert!(!cache.fresh(now + OUTPUT_CACHE_TTL + Duration::from_millis(1)));
        assert!(!cache.fresh(now + Duration::from_secs(3600)));
    }

    #[test]
    fn restore_target_prefers_the_saved_previous_and_falls_back_to_stock() {
        let stored = Some(sample_assignment());
        let target = restore_target(stored, Some("/usr/share/stock.png".into()));
        assert_eq!(target.mode, "assignment");
        assert_eq!(target.wallpaper_plugin, "org.kde.image");
        assert_eq!(
            target.image.as_deref(),
            Some("file:///usr/share/wallpapers/fallback.png")
        );
        // No assignment: the stock image plugin with the first present
        // stock image.
        let target = restore_target(None, Some("/usr/share/stock.png".into()));
        assert_eq!(target.mode, "stock");
        assert_eq!(target.wallpaper_plugin, "org.kde.image");
        assert_eq!(target.image.as_deref(), Some("/usr/share/stock.png"));
        // No stock image either: the plugin still restores (theme default).
        let target = restore_target(None, None);
        assert_eq!(target.mode, "stock");
        assert_eq!(target.image, None);
        // An assignment with previous = None also falls back to stock.
        let target = restore_target(
            Some(Assignment {
                previous: None,
                ..sample_assignment()
            }),
            None,
        );
        assert_eq!(target.mode, "stock");
        assert_eq!(target.wallpaper_plugin, "org.kde.image");
    }

    #[test]
    fn promotion_verdict_covers_every_phase() {
        // Our renderer reaching a live phase completes the transaction.
        assert_eq!(
            promotion_verdict(WorkerPhase::Live, true, None),
            Some(Ok(()))
        );
        assert_eq!(
            promotion_verdict(WorkerPhase::AwaitingAck, true, None),
            Some(Ok(()))
        );
        // Terminal failures report the bounded detail.
        let verdict = promotion_verdict(WorkerPhase::RolledBack, true, Some("corrupt"));
        assert_eq!(verdict, Some(Err("corrupt".into())));
        let verdict = promotion_verdict(WorkerPhase::Quarantined, false, None);
        assert_eq!(verdict, Some(Err("quarantined".into())));
        assert_eq!(
            promotion_verdict(WorkerPhase::Stopped, true, None),
            Some(Err("stopped".into()))
        );
        // Still transitioning, or a different renderer took the slot:
        // keep waiting until the deadline.
        for phase in [
            WorkerPhase::Idle,
            WorkerPhase::Starting,
            WorkerPhase::Canary,
            WorkerPhase::Restarting,
        ] {
            assert_eq!(promotion_verdict(phase, true, None), None);
        }
        assert_eq!(promotion_verdict(WorkerPhase::Live, false, None), None);
        assert_eq!(
            promotion_verdict(WorkerPhase::AwaitingAck, false, None),
            None
        );
    }

    /// Writes an executable stub and returns its path, ready to exec.
    ///
    /// Tests in this binary run on many threads, and a thread that forks
    /// while another is writing an executable leaves the child holding the
    /// write descriptor until it execs — so exec'ing the new stub can fail
    /// with ETXTBSY through no fault of the code under test. Wait that
    /// window out here, with a no-argument warm-up call the stubs are
    /// written to ignore, rather than teaching production code to retry.
    fn write_stub(root: &std::path::Path, name: &str, body: &str) -> PathBuf {
        use std::os::unix::fs::PermissionsExt;
        let path = root.join(name);
        std::fs::write(&path, body).unwrap();
        std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o755)).unwrap();
        for _attempt in 0..200 {
            let status = Command::new(&path)
                .stdin(Stdio::null())
                .stdout(Stdio::null())
                .stderr(Stdio::null())
                .status();
            match status {
                Err(error) if error.raw_os_error() == Some(libc::ETXTBSY) => {
                    std::thread::sleep(Duration::from_millis(5));
                }
                _ => return path,
            }
        }
        panic!("stub {name} stayed busy");
    }

    /// A probe whose systemctl is `stub` and which believes it has no
    /// display of its own — the boot-started daemon of BETA B1.
    fn recovery_probe(stub: PathBuf) -> QdbusShellProbe {
        QdbusShellProbe::new(
            "org.kde.plasmashell".into(),
            None,
            None,
            PathBuf::from("kscreen-doctor"),
            Some(stub),
            Duration::from_secs(5),
        )
        .without_ambient_display()
    }

    #[test]
    fn display_env_parse_keeps_display_keys_and_drops_everything_else() {
        // Real `systemctl --user show-environment` shape, plus the values
        // an attacker-controlled or merely broken manager could carry.
        let text = concat!(
            "DBUS_SESSION_BUS_ADDRESS=unix:path=/run/user/1000/bus\n",
            "LANG=en_US.UTF-8\n",
            "WAYLAND_DISPLAY=wayland-0\n",
            "DISPLAY=:0\n",
            "XDG_SESSION_TYPE=wayland\n",
        );
        assert_eq!(
            parse_display_env(text),
            vec![
                ("WAYLAND_DISPLAY".to_string(), "wayland-0".to_string()),
                ("DISPLAY".to_string(), ":0".to_string()),
            ]
        );
        // systemd shell-quotes values that need it; one layer comes off.
        assert_eq!(
            parse_display_env("WAYLAND_DISPLAY=\"wayland-1\"\n"),
            vec![("WAYLAND_DISPLAY".to_string(), "wayland-1".to_string())]
        );
        // An absolute socket path is a legitimate Wayland display.
        assert_eq!(
            parse_display_env("WAYLAND_DISPLAY=/run/user/1000/wayland-0\n"),
            vec![(
                "WAYLAND_DISPLAY".to_string(),
                "/run/user/1000/wayland-0".to_string()
            )]
        );
        // Rejected: empty, whitespace, quoting characters, control bytes,
        // and anything over the length bound. None of these reach a child.
        for hostile in [
            "WAYLAND_DISPLAY=\n",
            "WAYLAND_DISPLAY=wayland 0\n",
            "WAYLAND_DISPLAY=way\"land\n",
            "WAYLAND_DISPLAY=way'land\n",
            "WAYLAND_DISPLAY=way`land`\n",
            "WAYLAND_DISPLAY=$(id)\n",
            "WAYLAND_DISPLAY=way\\land\n",
        ] {
            assert!(
                parse_display_env(hostile).is_empty(),
                "accepted hostile value: {hostile:?}"
            );
        }
        let long = format!(
            "WAYLAND_DISPLAY={}\n",
            "w".repeat(MAX_DISPLAY_VALUE_BYTES + 1)
        );
        assert!(parse_display_env(&long).is_empty());
        let at_bound = format!("WAYLAND_DISPLAY={}\n", "w".repeat(MAX_DISPLAY_VALUE_BYTES));
        assert_eq!(parse_display_env(&at_bound).len(), 1);
        // Lines that are not KEY=VALUE, and keys we do not want, are noise.
        assert!(parse_display_env("no equals sign here\nPATH=/usr/bin\n").is_empty());
    }

    #[test]
    fn display_env_inherits_when_the_daemon_already_has_one() {
        // Ambient display present: the recovery must not run at all, which
        // a stub that would fail loudly proves.
        let root = temporary_directory("display-inherit");
        std::fs::create_dir_all(&root).unwrap();
        let stub = write_stub(&root, "systemctl.sh", "#!/bin/sh\nexit 9\n");
        let probe = QdbusShellProbe::new(
            "org.kde.plasmashell".into(),
            None,
            None,
            PathBuf::from("kscreen-doctor"),
            Some(stub),
            Duration::from_secs(5),
        );
        // This test binary runs inside a session, so the ambient answer is
        // real; skip rather than assert a lie when it is not.
        if AmbientDisplay::FromProcessEnv.present() {
            assert!(probe.display_env().unwrap().is_empty());
        }
        std::fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn display_env_recovers_from_the_manager_and_caches_only_success() {
        let root = temporary_directory("display-recover");
        std::fs::create_dir_all(&root).unwrap();
        let calls = root.join("calls");
        let stub = write_stub(
            &root,
            "systemctl.sh",
            &format!(
                "#!/bin/sh\n[ \"$1\" = --user ] && echo x >> {}\necho 'WAYLAND_DISPLAY=wayland-7'\n",
                calls.display()
            ),
        );
        let probe = recovery_probe(stub);
        let recovered = probe.display_env().unwrap();
        assert_eq!(
            recovered,
            vec![("WAYLAND_DISPLAY".to_string(), "wayland-7".to_string())]
        );
        // Second call is served from the cache: the stub ran once.
        assert_eq!(probe.display_env().unwrap(), recovered);
        assert_eq!(std::fs::read_to_string(&calls).unwrap().lines().count(), 1);
        // Forgetting sends the next call back to the manager.
        probe.forget_display_env();
        assert_eq!(probe.display_env().unwrap(), recovered);
        assert_eq!(std::fs::read_to_string(&calls).unwrap().lines().count(), 2);
        std::fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn display_env_failure_is_actionable_and_never_cached() {
        let root = temporary_directory("display-fail");
        std::fs::create_dir_all(&root).unwrap();
        let calls = root.join("calls");
        // The manager answers, but with no display in it — a session that
        // has genuinely not started yet.
        let empty = write_stub(
            &root,
            "empty.sh",
            &format!(
                "#!/bin/sh\n[ \"$1\" = --user ] && echo x >> {}\necho 'LANG=en_US.UTF-8'\n",
                calls.display()
            ),
        );
        let probe = recovery_probe(empty);
        for _ in 0..2 {
            let error = probe.display_env().unwrap_err();
            assert!(
                matches!(error, ProbeError::DisplayUnavailable(ref detail)
                    if detail.contains("systemctl --user restart kwe-daemon")),
                "{error}"
            );
        }
        // Called twice: a failure must never be remembered, or a daemon
        // that started before its session would stay broken for good.
        assert_eq!(std::fs::read_to_string(&calls).unwrap().lines().count(), 2);

        // A manager that answers with a failure fails the same way. /bin/false
        // rather than a written stub: no file to race an exec against.
        let error = recovery_probe(PathBuf::from("/bin/false"))
            .display_env()
            .unwrap_err();
        assert!(
            matches!(error, ProbeError::DisplayUnavailable(ref detail)
                if detail.contains("exited") && detail.contains("restart kwe-daemon")),
            "{error}"
        );
        std::fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn display_recovery_stays_inside_the_probe_budget() {
        // The recovery is an EXTRA child on the enumeration path, which
        // already spends the probe deadline twice. A systemctl that will
        // not answer must not extend enumeration past what the manager's
        // own request deadline allows, so the budget is capped.
        let root = temporary_directory("display-budget");
        std::fs::create_dir_all(&root).unwrap();
        // Sleeps only for the real invocation, so write_stub's warm-up
        // call returns at once.
        let slow = write_stub(
            &root,
            "slow.sh",
            "#!/bin/sh\n[ \"$1\" = --user ] && sleep 30\n",
        );
        let probe = QdbusShellProbe::new(
            "org.kde.plasmashell".into(),
            None,
            None,
            PathBuf::from("kscreen-doctor"),
            Some(slow),
            Duration::from_millis(120),
        )
        .without_ambient_display();
        let started = Instant::now();
        let error = probe.display_env().unwrap_err();
        let elapsed = started.elapsed();
        assert!(
            matches!(error, ProbeError::DisplayUnavailable(ref detail)
                if detail.contains("timed out") && detail.contains("restart kwe-daemon")),
            "{error}"
        );
        // The configured probe timeout wins when it is the smaller of the
        // two; either way the wait is bounded, not the stub's 30 seconds.
        assert!(elapsed < Duration::from_secs(2), "waited {elapsed:?}");
        assert!(RECOVERY_TIMEOUT < Duration::from_secs(5));
        std::fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn recovered_display_reaches_the_enumeration_child() {
        // The point of the whole fix: kscreen-doctor must actually be run
        // with the recovered WAYLAND_DISPLAY in its environment.
        let root = temporary_directory("display-child");
        std::fs::create_dir_all(&root).unwrap();
        let seen = root.join("seen");
        let systemctl = write_stub(
            &root,
            "systemctl.sh",
            "#!/bin/sh\necho 'WAYLAND_DISPLAY=wayland-9'\n",
        );
        let kscreen = write_stub(
            &root,
            "kscreen.sh",
            &format!(
                "#!/bin/sh\nprintf '%s' \"$WAYLAND_DISPLAY\" > {}\n\
                 echo 'Output: 1 DP-1 uuid'\necho '\tenabled'\necho '\tconnected'\n",
                seen.display()
            ),
        );
        let probe = QdbusShellProbe::new(
            "org.kde.plasmashell".into(),
            None,
            None,
            kscreen,
            Some(systemctl),
            Duration::from_secs(5),
        )
        .without_ambient_display();
        let outputs = probe.system_outputs().unwrap();
        assert_eq!(outputs.len(), 1);
        assert_eq!(outputs[0].name, "DP-1");
        assert_eq!(std::fs::read_to_string(&seen).unwrap(), "wayland-9");
        std::fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn a_failing_enumeration_forgets_a_recovered_display() {
        // A recovered display can go stale (the session restarted under a
        // new one); a failed child must send the next call back to the
        // manager rather than replaying the stale value forever.
        let root = temporary_directory("display-stale");
        std::fs::create_dir_all(&root).unwrap();
        let calls = root.join("calls");
        let systemctl = write_stub(
            &root,
            "systemctl.sh",
            &format!(
                "#!/bin/sh\n[ \"$1\" = --user ] && echo x >> {}\necho 'WAYLAND_DISPLAY=wayland-0'\n",
                calls.display()
            ),
        );
        let kscreen = write_stub(&root, "kscreen.sh", "#!/bin/sh\necho 'boom' >&2\nexit 1\n");
        let probe = QdbusShellProbe::new(
            "org.kde.plasmashell".into(),
            None,
            None,
            kscreen,
            Some(systemctl),
            Duration::from_secs(5),
        )
        .without_ambient_display();
        for _ in 0..2 {
            assert!(matches!(
                probe.system_outputs().unwrap_err(),
                ProbeError::Rejected(_)
            ));
        }
        assert_eq!(std::fs::read_to_string(&calls).unwrap().lines().count(), 2);
        std::fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn resolve_systemctl_prefers_the_explicit_path() {
        let explicit = PathBuf::from("/somewhere/systemctl");
        assert_eq!(
            resolve_systemctl(&Some(explicit.clone())).unwrap(),
            explicit
        );
        // Without an explicit path it resolves from PATH, the same way
        // resolve_qdbus does; a PATH with no systemctl in it finds none,
        // and that miss is what becomes the actionable DisplayUnavailable.
        let root = temporary_directory("systemctl-path");
        std::fs::create_dir_all(&root).unwrap();
        let empty = std::env::join_paths([&root]).unwrap();
        assert!(find_in_path(Some(empty.as_os_str()), &["systemctl"]).is_none());
        std::fs::write(root.join("systemctl"), b"x").unwrap();
        assert_eq!(
            find_in_path(Some(empty.as_os_str()), &["systemctl"]).unwrap(),
            root.join("systemctl")
        );
        assert!(DISPLAY_UNAVAILABLE_HINT.contains("systemctl --user restart kwe-daemon"));
        std::fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn enumeration_error_keeps_display_unavailable_distinct() {
        // Only this one survives as its own code; everything else stays
        // shell_unreachable exactly as before.
        let mapped = enumeration_error(ProbeError::DisplayUnavailable("no display".into()));
        assert_eq!(mapped.code(), "display_unavailable");
        assert_eq!(mapped.detail(), Some("no display"));
        for other in [
            ProbeError::Unreachable("gone".into()),
            ProbeError::Rejected("nope".into()),
            ProbeError::TimedOut("slow".into()),
            ProbeError::Parse("garbage".into()),
        ] {
            assert_eq!(enumeration_error(other).code(), "shell_unreachable");
        }
    }

    #[test]
    fn find_in_path_resolves_qdbus_then_qdbus6() {
        let root = temporary_directory("path");
        std::fs::create_dir_all(&root).unwrap();
        let first = root.join("first");
        let second = root.join("second");
        std::fs::create_dir_all(&first).unwrap();
        std::fs::create_dir_all(&second).unwrap();
        std::fs::write(first.join("qdbus"), b"x").unwrap();
        std::fs::write(second.join("qdbus"), b"x").unwrap();
        std::fs::write(second.join("qdbus6"), b"x").unwrap();
        let path = std::env::join_paths([&first, &second]).unwrap();
        // qdbus wins over qdbus6 when both exist.
        let resolved = find_in_path(Some(path.as_os_str()), &["qdbus", "qdbus6"]).unwrap();
        assert_eq!(resolved, first.join("qdbus"));
        // qdbus6 is the fallback when qdbus is absent.
        let third = root.join("third");
        std::fs::create_dir_all(&third).unwrap();
        std::fs::write(third.join("qdbus6"), b"x").unwrap();
        let path = std::env::join_paths([&third]).unwrap();
        let resolved = find_in_path(Some(path.as_os_str()), &["qdbus", "qdbus6"]).unwrap();
        assert_eq!(resolved, third.join("qdbus6"));
        // No PATH means no binary.
        assert!(find_in_path(None, &["qdbus"]).is_none());
        assert!(find_in_path(Some(path.as_os_str()), &["nope"]).is_none());
        std::fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn apply_params_defaults_match_the_renderer_defaults() {
        assert_eq!(default_apply_width(), 960);
        assert_eq!(default_apply_height(), 540);
        assert_eq!(default_apply_fps(), 30);
    }

    #[test]
    fn restore_target_contract_documentation_smoke() {
        // The documented safe-mode contract: restore always resolves to a
        // concrete target, never to a daemon-owned renderer.
        let target = restore_target(None, None);
        assert_ne!(target.wallpaper_plugin, KWE_PLUGIN);
        let target = restore_target(Some(sample_assignment()), None);
        assert_ne!(target.wallpaper_plugin, KWE_PLUGIN);
    }

    #[test]
    fn external_evaluator_runs_the_script_as_its_single_argument() {
        // `--plasma-switch-command <path>` replaces the whole evaluation
        // boundary: the command is spawned with the evaluateScript script
        // as its sole argument and its stdout is the probe reply, through
        // the same bounded-run machinery as qdbus.
        let root = temporary_directory("external-evaluator");
        std::fs::create_dir_all(&root).unwrap();
        // write_stub, not a bare write: exec'ing a just-written file races
        // other threads' forks and fails with ETXTBSY (observed here).
        let stub = write_stub(&root, "plasma-stub.sh", "#!/bin/sh\nprintf '%s' \"$1\"\n");
        let probe = QdbusShellProbe::new(
            "org.kde.plasmashell".into(),
            None,
            Some(stub),
            PathBuf::from("kscreen-doctor"),
            None,
            Duration::from_secs(5),
        );
        // Enumeration and switch scripts both pass through the same
        // boundary; the stub chooses by content.
        let script = "var d = desktops(); print(1);";
        assert_eq!(probe.evaluate_script(script).unwrap(), script);
        let switch = "var d = desktops()[1]; d.wallpaperPlugin = \"org.kde.kwe.wallpaper\";";
        assert_eq!(probe.evaluate_script(switch).unwrap(), switch);
        // A failing stub maps onto ProbeError::Rejected with its stderr.
        let failing = write_stub(
            &root,
            "failing-stub.sh",
            "#!/bin/sh\necho 'nope' >&2\nexit 3\n",
        );
        let probe = QdbusShellProbe::new(
            "org.kde.plasmashell".into(),
            None,
            Some(failing),
            PathBuf::from("kscreen-doctor"),
            None,
            Duration::from_secs(5),
        );
        let error = probe.evaluate_script("print(1);").unwrap_err();
        assert!(
            matches!(error, ProbeError::Rejected(ref detail) if detail == "nope"),
            "{error}"
        );
        std::fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn frame_size_follows_the_output_within_the_cap() {
        // Explicit always wins.
        assert_eq!(
            frame_size_for(Some(320), Some(180), Some([0, 0, 2926, 823])),
            (320, 180)
        );
        // No geometry: the legacy canvas (and one explicit edge is kept).
        assert_eq!(frame_size_for(None, None, None), (960, 540));
        assert_eq!(frame_size_for(Some(800), None, None), (800, 540));
        // Small output: exact.
        assert_eq!(
            frame_size_for(None, None, Some([0, 0, 1920, 1080])),
            (1920, 1080)
        );
        // Long edge capped, aspect kept, even pixels.
        assert_eq!(
            frame_size_for(None, None, Some([0, 0, 2926, 823])),
            (2560, 720)
        );
        assert_eq!(
            frame_size_for(None, None, Some([0, 0, 3840, 2160])),
            (2560, 1440)
        );
        let (w, h) = frame_size_for(None, None, Some([0, 0, 1080, 3840]));
        assert_eq!((w, h), (720, 2560));
        // Degenerate geometry falls back; never below the floor.
        assert_eq!(frame_size_for(None, None, Some([0, 0, 0, 823])), (960, 540));
        assert_eq!(
            frame_size_for(None, None, Some([0, 0, 10000, 8])),
            (2560, 64)
        );
        // Every derived canvas passes the frame spec and the supervised cap.
        for geometry in [[0, 0, 7680, 4320], [0, 0, 2926, 823], [0, 0, 640, 480]] {
            let (w, h) = frame_size_for(None, None, Some(geometry));
            let spec = kwe_frame_protocol::FrameSpec::new(w, h).unwrap();
            assert!(spec.file_bytes <= 128 * 1024 * 1024);
        }
    }
}
