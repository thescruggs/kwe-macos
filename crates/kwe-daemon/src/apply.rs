// SPDX-License-Identifier: Apache-2.0
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
    collections::BTreeMap,
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
use crate::supervisor::{
    ContentSpec, RendererKind, StartSpec, SupervisorHandle, WorkerPhase, validate_identity_part,
};

const ASSIGNMENTS_FILE: &str = "assignments-v1.json";
/// Bounded assignment map: one record per output, hard-capped before the
/// 1 MiB byte bound (mirrors the grants store's count bound).
const MAX_ASSIGNED_OUTPUTS: usize = 16;
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
/// index and the validated plugin name.
pub fn apply_script(desktop_index: usize, plugin: &str) -> Result<String, String> {
    validate_identity_part("wallpaper_plugin", plugin).map_err(|error| error.to_string())?;
    Ok(format!(
        "var d = desktops()[{desktop_index}]; d.wallpaperPlugin = \"{plugin}\";"
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
    let mut script = format!("var d = desktops()[{desktop_index}];\n");
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
/// carry bounded detail.
#[derive(Debug, Clone)]
pub enum ProbeError {
    Unreachable(String),
    Rejected(String),
    TimedOut(String),
    Parse(String),
}

impl std::fmt::Display for ProbeError {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            ProbeError::Unreachable(detail) => write!(formatter, "{detail}"),
            ProbeError::Rejected(detail) => write!(formatter, "{detail}"),
            ProbeError::TimedOut(detail) => write!(formatter, "{detail}"),
            ProbeError::Parse(detail) => write!(formatter, "{detail}"),
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
    pub scripts: std::sync::Mutex<Vec<String>>,
    /// Fails every non-enumeration script (the switch step). Atomic so the
    /// RPC tests can flip it through the shared `Arc`.
    pub reject_scripts: std::sync::atomic::AtomicBool,
}

#[cfg(test)]
impl StubProbe {
    pub fn new(outputs: Vec<SystemOutput>, reply: Option<String>) -> Self {
        Self {
            outputs,
            reply,
            scripts: std::sync::Mutex::new(Vec::new()),
            reject_scripts: std::sync::atomic::AtomicBool::new(false),
        }
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
            match &self.reply {
                Some(reply) => Ok(reply.clone()),
                None => Err(ProbeError::Rejected("stub probe rejected".into())),
            }
        } else if self
            .reject_scripts
            .load(std::sync::atomic::Ordering::SeqCst)
        {
            Err(ProbeError::Unreachable("stub probe unreachable".into()))
        } else {
            Ok(String::new())
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

/// Plasma shell probe via direct `qdbus` invocation (no shell; the script
/// is passed as an argument). The qdbus binary is resolved from PATH on
/// every call — `qdbus` first, `qdbus6` fallback — so the daemon starts
/// fine on systems without either and reports `shell_unreachable` lazily.
pub struct QdbusShellProbe {
    shell_service: String,
    qdbus_binary: Option<PathBuf>,
    kscreen_binary: PathBuf,
    timeout: Duration,
}

impl QdbusShellProbe {
    pub fn new(
        shell_service: String,
        qdbus_binary: Option<PathBuf>,
        kscreen_binary: PathBuf,
        timeout: Duration,
    ) -> Self {
        Self {
            shell_service,
            qdbus_binary,
            kscreen_binary,
            timeout,
        }
    }

    fn qdbus_command(&self) -> Result<PathBuf, ProbeError> {
        if let Some(path) = &self.qdbus_binary {
            return Ok(path.clone());
        }
        find_in_path(std::env::var_os("PATH").as_deref(), &["qdbus", "qdbus6"])
            .ok_or_else(|| ProbeError::Unreachable("qdbus (or qdbus6) is not on PATH".into()))
    }
}

impl ShellProbe for QdbusShellProbe {
    fn evaluate_script(&self, script: &str) -> Result<String, ProbeError> {
        let qdbus = self.qdbus_command()?;
        let mut command = Command::new(qdbus);
        command
            .arg(&self.shell_service)
            .arg("/PlasmaShell")
            .arg("evaluateScript")
            .arg(script);
        let outcome = run_bounded(&mut command, self.timeout)
            .map_err(|error| classify_probe_failure(&error))?;
        if !outcome.status.success() {
            let detail = String::from_utf8_lossy(&outcome.stderr).trim().to_string();
            return Err(ProbeError::Rejected(if detail.is_empty() {
                format!("qdbus exited {}", outcome.status)
            } else {
                detail
            }));
        }
        let stdout = String::from_utf8_lossy(&outcome.stdout);
        Ok(stdout.trim().to_string())
    }

    fn system_outputs(&self) -> Result<Vec<SystemOutput>, ProbeError> {
        let mut command = Command::new(&self.kscreen_binary);
        command.arg("-o");
        let outcome = run_bounded(&mut command, self.timeout)
            .map_err(|error| classify_probe_failure(&format!("kscreen-doctor: {error}")))?;
        if !outcome.status.success() {
            let detail = String::from_utf8_lossy(&outcome.stderr).trim().to_string();
            return Err(ProbeError::Rejected(if detail.is_empty() {
                format!("kscreen-doctor exited {}", outcome.status)
            } else {
                detail
            }));
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
/// without a matching desktop yields a desktop-less output.
fn assemble_outputs(
    system: Vec<SystemOutput>,
    desktops: Vec<ProbeDesktop>,
    connectors: BTreeMap<String, i32>,
) -> Vec<OutputInfo> {
    system
        .into_iter()
        .map(|output| {
            let screen = connectors.get(&output.name).copied().unwrap_or(-1);
            let desktop = desktops.iter().find(|desktop| desktop.screen == screen);
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
    pub kscreen_binary: PathBuf,
    /// Deadline for every probe (enumeration, switch, restore).
    pub probe_timeout: Duration,
    /// Deadline for the renderer to reach a live phase after start.
    pub promotion_timeout: Duration,
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
    /// The Plasma shell or its tooling could not be reached.
    ShellUnreachable(String),
    /// A step of the apply transaction failed (already rolled back).
    Transaction(String),
    /// The restore script could not be executed.
    RestoreFailed(String),
}

impl ApplyError {
    pub fn code(&self) -> &'static str {
        match self {
            ApplyError::Invalid(_) => "invalid_params",
            ApplyError::UnknownWallpaper(_) => "apply_unknown_wallpaper",
            ApplyError::Incompatible(_) => "apply_incompatible",
            ApplyError::OutputMissing(_) => "output_missing",
            ApplyError::Busy => "apply_busy",
            ApplyError::ShellUnreachable(_) => "shell_unreachable",
            ApplyError::Transaction(_) => "apply_failed",
            ApplyError::RestoreFailed(_) => "restore_failed",
        }
    }

    pub fn detail(&self) -> Option<&str> {
        match self {
            ApplyError::Invalid(detail)
            | ApplyError::UnknownWallpaper(detail)
            | ApplyError::Incompatible(detail)
            | ApplyError::OutputMissing(detail)
            | ApplyError::ShellUnreachable(detail)
            | ApplyError::Transaction(detail)
            | ApplyError::RestoreFailed(detail) => Some(detail),
            ApplyError::Busy => None,
        }
    }
}

/// `wallpaper.apply` params (deny_unknown_fields). `kind`/`content` follow
/// the StartSpec rules; the test kind is not assignable.
#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ApplyWallpaperParams {
    pub output: String,
    pub wallpaper_id: String,
    pub kind: RendererKind,
    pub content: PathBuf,
    #[serde(default = "default_apply_width")]
    pub width: u32,
    #[serde(default = "default_apply_height")]
    pub height: u32,
    #[serde(default = "default_apply_fps")]
    pub fps: u32,
}

pub const fn default_apply_width() -> u32 {
    960
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
            config.kscreen_binary,
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
            },
        })
    }

    pub fn handle(&self) -> ApplyHandle {
        self.handle.clone()
    }
}

impl ApplyHandle {
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
        let system = self
            .probe
            .system_outputs()
            .map_err(|error| ApplyError::ShellUnreachable(error.to_string()))?;
        let names: Vec<String> = system.iter().map(|output| output.name.clone()).collect();
        let script = probe_script(&names).map_err(|error| {
            ApplyError::ShellUnreachable(format!("cannot build the enumeration script: {error}"))
        })?;
        let reply = self
            .probe
            .evaluate_script(&script)
            .map_err(|error| ApplyError::ShellUnreachable(error.to_string()))?;
        let reply: ProbeReply = parse_probe_reply(&reply)
            .map_err(|error| ApplyError::ShellUnreachable(error.to_string()))?;
        Ok(assemble_outputs(system, reply.desktops, reply.connectors))
    }

    /// The full live-apply transaction. Completes when the renderer
    /// PROMOTES (Live or AwaitingAck), not when the display ack arrives —
    /// the ack comes later from the wallpaper bridge. Any failure after a
    /// step has side effects rolls back: a failed wallpaper switch stops
    /// the promoted renderer and drops the persisted assignment.
    pub fn apply(&self, params: ApplyWallpaperParams) -> Result<Value, ApplyError> {
        let _guard = self.acquire_apply_lock()?;

        // 1. Validate the request into a supervisor StartSpec (single
        // validation point; content preflight runs here).
        let mut spec = build_apply_spec(&params)?;

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
        spec.content_hash = content_hash_for(&item, &params.content);

        // 3. Fresh enumeration; the output must be live and have a desktop.
        let outputs = self.enumerate_fresh()?;
        let output = outputs
            .iter()
            .find(|info| info.name == params.output)
            .ok_or_else(|| ApplyError::OutputMissing(params.output.clone()))?;
        let desktop_index = output.desktop_index.ok_or_else(|| {
            ApplyError::Transaction(format!("output {} has no desktop containment", output.name))
        })?;
        let previous = PreviousWallpaper {
            wallpaper_plugin: output.wallpaper_plugin.clone().ok_or_else(|| {
                ApplyError::Transaction(format!(
                    "output {} has no wallpaper plugin to save as previous",
                    output.name
                ))
            })?,
            config_group: output.config_group.clone(),
            image: output.image.clone(),
        };

        // 4. Start the renderer and wait (bounded) for OUR promotion.
        let started = self
            .supervisor
            .start(spec.clone())
            .map_err(|error| ApplyError::Transaction(format!("renderer.start failed: {error}")))?;
        if started.phase == WorkerPhase::Quarantined || started.phase == WorkerPhase::RolledBack {
            return Err(ApplyError::Transaction(format!(
                "renderer rejected the start ({})",
                phase_name(&started.phase)
            )));
        }
        self.wait_for_promotion(&spec.wallpaper_id, &spec.content_hash)?;

        // 5. Persist the assignment (previous = the config that was live).
        let assignment = Assignment {
            wallpaper_id: spec.wallpaper_id.clone(),
            kind: spec.kind,
            content: params
                .content
                .to_string_lossy()
                .chars()
                .take(MAX_CONTENT_CHARS)
                .collect(),
            width: spec.width,
            height: spec.height,
            fps: spec.fps,
            applied_at_unix_seconds: unix_seconds(),
            previous: Some(previous),
        };
        {
            let mut store = self
                .store
                .lock()
                .map_err(|_| ApplyError::Transaction("assignment store lock poisoned".into()))?;
            store
                .set(&output.name, assignment.clone())
                .map_err(|error| {
                    ApplyError::Transaction(format!("persist assignment failed: {error}"))
                })?;
        }

        // 6. Switch the Plasma wallpaper config. The script is a pure
        // function of {desktop index, plugin name} — never wallpaper
        // content — and runs through a bounded, shell-less qdbus call.
        let script = apply_script(desktop_index, KWE_PLUGIN)
            .map_err(|error| ApplyError::Transaction(format!("script error: {error}")))?;
        if let Err(error) = self.probe.evaluate_script(&script) {
            // Rollback: the config was not switched, so undo what the
            // transaction did — stop the renderer, drop the assignment.
            let _ = self.supervisor.stop();
            if let Ok(mut store) = self.store.lock() {
                let _ = store.remove(&output.name);
            }
            return Err(match error {
                ProbeError::Unreachable(detail) => ApplyError::ShellUnreachable(detail),
                other => ApplyError::Transaction(format!("wallpaper switch failed: {other}")),
            });
        }

        Ok(json!({ "output": output.name, "applied": assignment }))
    }

    /// Reverts the wallpaper config of one output to its saved `previous`
    /// (or to the stock image plugin when there is no assignment — the
    /// safe-mode contract: restore never leaves a desktop assigned to a
    /// daemon-owned renderer, so it always succeeds on a known output).
    /// The assignment is cleared only after the script ran.
    pub fn restore(&self, output_name: String) -> Result<Value, ApplyError> {
        let outputs = self.enumerate_fresh()?;
        let output = outputs
            .iter()
            .find(|info| info.name == output_name)
            .ok_or_else(|| ApplyError::OutputMissing(output_name.clone()))?;
        let desktop_index = output.desktop_index.ok_or_else(|| {
            ApplyError::Transaction(format!("output {} has no desktop containment", output.name))
        })?;
        let stored = {
            let store = self
                .store
                .lock()
                .map_err(|_| ApplyError::Transaction("assignment store lock poisoned".into()))?;
            store.get(&output_name).cloned()
        };
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
fn build_apply_spec(params: &ApplyWallpaperParams) -> Result<StartSpec, ApplyError> {
    if params.kind == RendererKind::Test {
        return Err(ApplyError::Invalid(
            "wallpaper.apply does not accept the test renderer kind".into(),
        ));
    }
    let content = match params.kind {
        RendererKind::Video => ContentSpec::Video {
            path: params.content.clone(),
        },
        RendererKind::Web => ContentSpec::Web {
            root: params.content.clone(),
        },
        RendererKind::Scene => ContentSpec::Scene {
            path: params.content.clone(),
        },
        RendererKind::Test => unreachable!("test kind rejected above"),
    };
    StartSpec {
        wallpaper_id: params.wallpaper_id.clone(),
        content_hash: "pending".into(),
        width: params.width,
        height: params.height,
        fps: params.fps,
        kind: params.kind,
        content: Some(content),
        test_fault: None,
        stderr_lines: None,
    }
    .into_validated()
    .map_err(|error| ApplyError::Invalid(error.to_string()))
}

/// Stable content identity for the supervisor's quarantine key: the
/// catalog item's project-metadata hash when present (it is stable across
/// rescans), else the SHA-256 of the canonical content path.
fn content_hash_for(item: &CatalogItem, content: &std::path::Path) -> String {
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
        assert_eq!(
            apply_script(1, "org.kde.kwe.wallpaper").unwrap(),
            "var d = desktops()[1]; d.wallpaperPlugin = \"org.kde.kwe.wallpaper\";"
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
             d.currentConfigGroup = [\"Wallpaper\", \"org.kde.image\", \"General\"];\n\
             d.wallpaperPlugin = \"org.kde.image\";"
        );
        assert!(!script.contains("writeConfig"));
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
}
