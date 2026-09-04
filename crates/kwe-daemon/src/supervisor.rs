// SPDX-License-Identifier: GPL-3.0-or-later
//! Original bounded renderer-process supervisor.
//!
//! Upstream projects in `THIRD_PARTY.yml` informed the process-isolation goal,
//! but this state machine, persistence format, and implementation are original.

use std::{
    collections::{BTreeMap, BTreeSet},
    ffi::OsString,
    fs::{self, OpenOptions},
    io::{self, Read},
    os::unix::{
        fs::{OpenOptionsExt, PermissionsExt},
        io::AsRawFd,
        process::{CommandExt, ExitStatusExt},
    },
    path::{Path, PathBuf},
    process::{Child, ChildStderr, ChildStdin, ChildStdout, Command as ProcessCommand, Stdio},
    sync::{
        atomic::{AtomicU64, Ordering},
        mpsc::{self, Receiver, SyncSender, TrySendError},
    },
    thread::{self, JoinHandle},
    time::{Duration, Instant},
};

use crate::grants::{Grant, GrantPatch, GrantStore};
#[cfg(test)]
use crate::persist::unix_nanos;
use crate::persist::{atomic_write, ensure_private_dir, unix_seconds};
use anyhow::{Context, Result, anyhow, bail};
use kwe_core::{preflight_scene, preflight_video, preflight_web};
use kwe_frame_protocol::{FrameSnapshot, FrameSpec, ProtocolError, SharedFrameReader};
use kwe_input_protocol::{
    AudioFrame, MAX_MESSAGE_BYTES as MAX_INPUT_MESSAGE_BYTES, MediaState, PointerButton,
    PointerMessage, PointerPhase, decode_ack_line, encode_audio_frame, encode_media_state,
    encode_pointer_line,
};
use serde::{Deserialize, Serialize};

const COMMAND_CAPACITY: usize = 16;
const COMMAND_TIMEOUT: Duration = Duration::from_secs(5);
/// Bounded-rate log for grant-gated audio drops: first 5, then every
/// thousandth, so an ungranted wallpaper's capture stream cannot flood the
/// daemon log (mirrors the renderer-less drop counter in main.rs).
const AUDIO_GRANT_DROP_LOG_EVERY: u64 = 1000;
static AUDIO_GRANT_DROP_LOGS: AtomicU64 = AtomicU64::new(0);

fn log_audio_grant_drop() {
    let calls = AUDIO_GRANT_DROP_LOGS.fetch_add(1, Ordering::Relaxed);
    if calls < 5 || calls.is_multiple_of(AUDIO_GRANT_DROP_LOG_EVERY) {
        eprintln!(
            "event=audio.forward.grant_drop detail=wallpaper has no audio grant, frames dropped latest-wins"
        );
    }
}
const POLL_INTERVAL: Duration = Duration::from_millis(40);
const MAX_RECORDS: usize = 256;
/// Renderer contract exit codes that mean "refused", not "crashed" (B4).
const EXIT_BACKEND_REJECT: i32 = 73;
const EXIT_NO_DRAWABLE_CONTENT: i32 = 74;
const MAX_STATE_BYTES: u64 = 1024 * 1024;
const MAX_SUPERVISED_MAPPING_BYTES: u64 = 128 * 1024 * 1024;
const SNAPSHOT_INTERVAL: Duration = Duration::from_secs(30);
const MAX_ACK_BUFFER_BYTES: usize = 1024;
const MAX_ACK_READ_BYTES_PER_TICK: usize = 4096;
/// Bounded diagnostics: 64 complete lines or 16 KiB, whichever binds first.
const STDERR_RING_LINES: usize = 64;
const STDERR_RING_BYTES: usize = 16 * 1024;
/// Per-tick drain budget equals the pipe capacity (64 KiB) so a chatty
/// renderer's write(2) never blocks on our reader and trips frame_timeout;
/// the ring still bounds memory regardless of how much is drained.
const STDERR_READ_BYTES_PER_TICK: usize = 64 * 1024;

/// Renderer binary families the supervisor can launch. Each kind carries its
/// own binary path, startup timeout, and resource budget so the heavy web
/// renderer cannot be throttled by the test renderer's constraints.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Deserialize, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum RendererKind {
    Test,
    Video,
    Web,
    Scene,
}

impl RendererKind {
    pub fn as_str(&self) -> &'static str {
        match self {
            Self::Test => "test",
            Self::Video => "video",
            Self::Web => "web",
            Self::Scene => "scene",
        }
    }
}

/// Validated content handed to a renderer kind through `--content`.
#[derive(Debug, Clone)]
pub enum ContentSpec {
    Video { path: PathBuf },
    Web { root: PathBuf },
    Scene { path: PathBuf },
}

#[derive(Debug, Clone)]
pub struct SupervisorConfig {
    /// Per-kind renderer binaries. A kind may be absent; requesting it to
    /// spawn then fails closed instead of launching the wrong binary.
    pub renderer_paths: BTreeMap<RendererKind, PathBuf>,
    pub runtime_dir: PathBuf,
    pub state_dir: PathBuf,
    /// Per-kind startup deadlines in milliseconds; every kind must be present.
    pub startup_timeout_ms_by_kind: BTreeMap<RendererKind, u64>,
    pub frame_timeout: Duration,
    pub stop_grace: Duration,
    pub restart_delay: Duration,
    pub canary_duration: Duration,
    pub handoff_timeout: Duration,
    pub max_failures: u32,
    /// Web renderers: session-scoped liveness probe interval in ms (the
    /// worker probes the page's renderer main thread; a page wedged after
    /// first paint otherwise looks alive forever behind the keepalive).
    pub web_heartbeat_ms: u64,
    /// Web renderers: consecutive heartbeat failures before exit 73.
    pub web_heartbeat_max_failures: u32,
    /// Per-kind pre-exec resource ceilings; every kind must be present.
    pub resource_limits_by_kind: BTreeMap<RendererKind, RendererResourceLimits>,
    /// The Wallpaper Engine assets root (S1), passed to the scene worker
    /// as `--assets-dir` when set. `None` when not configured/detected —
    /// scene model layers then only resolve against assets the scene
    /// itself carries (pkg entries / its own directory).
    pub scene_assets_dir: Option<PathBuf>,
    /// SR-3b: `kwe-shader-compiler` (SR-3a's killable shader-compile
    /// helper), passed to the scene worker as `--shader-helper` when set
    /// — resolved beside the daemon's own executable
    /// (`default_shader_helper_path`, main.rs), same pattern as
    /// `default_inspector_path`. `None` when `current_exe()` fails; the
    /// flag is simply omitted in that case (the renderer's OWN sibling-
    /// resolution fallback then applies, so this is a belt-and-suspenders
    /// default, not the only path to a working helper). Scene kind only —
    /// no other renderer kind ever compiles a material shader.
    pub shader_helper_path: Option<PathBuf>,
}

#[derive(Debug, Clone, Copy, Serialize)]
pub struct RendererResourceLimits {
    pub address_space_mib: u64,
    pub file_size_mib: u64,
    pub open_files: u64,
    pub processes: u64,
    pub core_dump_bytes: u64,
}

impl RendererResourceLimits {
    fn validate(self) -> Result<Self> {
        // The address-space bound tops out at 256 GiB: chromium 151 needs a
        // ~128 GiB budget (V8 sandbox reservations; docs/BETA_M2.md M2b).
        if !(256..=262_144).contains(&self.address_space_mib)
            || !(129..=1024).contains(&self.file_size_mib)
            || !(32..=4096).contains(&self.open_files)
            || !(64..=32_768).contains(&self.processes)
            || self.core_dump_bytes != 0
        {
            bail!("renderer resource limits are outside their safety bounds");
        }
        Ok(self)
    }
}

impl SupervisorConfig {
    pub fn validate(mut self) -> Result<Self> {
        for (kind, path) in std::mem::take(&mut self.renderer_paths) {
            // Canonicalize whatever binaries exist now; absent kinds stay
            // unresolved and fail closed at spawn time instead of blocking
            // daemon startup on an uninstalled renderer family.
            match fs::canonicalize(&path) {
                Ok(canonical) => {
                    if !canonical.is_file() {
                        bail!(
                            "renderer executable for kind {} is not a regular file: {}",
                            kind.as_str(),
                            canonical.display()
                        );
                    }
                    self.renderer_paths.insert(kind, canonical);
                }
                Err(error) if error.kind() == io::ErrorKind::NotFound => {
                    self.renderer_paths.insert(kind, path);
                }
                Err(error) => {
                    return Err(error).with_context(|| {
                        format!(
                            "resolve renderer executable for kind {}: {}",
                            kind.as_str(),
                            path.display()
                        )
                    });
                }
            }
        }
        if self.frame_timeout.is_zero()
            || self.stop_grace.is_zero()
            || self.canary_duration.is_zero()
            || self.handoff_timeout.is_zero()
            || self.max_failures == 0
            || self.max_failures > 10
        {
            bail!("supervisor deadlines and failure budget must be bounded and non-zero");
        }
        if !(250..=60_000).contains(&self.web_heartbeat_ms)
            || !(1..=10).contains(&self.web_heartbeat_max_failures)
        {
            bail!("web heartbeat interval and failure budget must be bounded");
        }
        for (kind, timeout_ms) in &self.startup_timeout_ms_by_kind {
            if !(100..=30_000).contains(timeout_ms) {
                bail!(
                    "startup timeout for renderer kind {} is outside 100..=30000 ms",
                    kind.as_str()
                );
            }
        }
        for kind in [
            RendererKind::Test,
            RendererKind::Video,
            RendererKind::Web,
            RendererKind::Scene,
        ] {
            if !self.startup_timeout_ms_by_kind.contains_key(&kind) {
                bail!(
                    "missing startup timeout for renderer kind {}",
                    kind.as_str()
                );
            }
            if !self.resource_limits_by_kind.contains_key(&kind) {
                bail!(
                    "missing resource limits for renderer kind {}",
                    kind.as_str()
                );
            }
        }
        for (kind, limits) in &mut self.resource_limits_by_kind {
            *limits = limits
                .validate()
                .with_context(|| format!("resource limits for renderer kind {}", kind.as_str()))?;
        }
        ensure_private_dir(&self.runtime_dir)?;
        ensure_private_dir(&self.state_dir)?;
        Ok(self)
    }

    /// The binary for `kind`, or an error when it was never configured.
    fn renderer_path_for(&self, kind: RendererKind) -> Result<PathBuf> {
        self.renderer_paths
            .get(&kind)
            .cloned()
            .with_context(|| format!("no renderer binary configured for kind {}", kind.as_str()))
    }

    /// Per-kind startup deadline. `validate()` guarantees every kind is
    /// present, so this lookup cannot miss on a validated config.
    fn startup_timeout_for(&self, kind: RendererKind) -> Duration {
        Duration::from_millis(self.startup_timeout_ms_by_kind[&kind])
    }

    /// Per-kind pre-exec resource ceilings, with the same guaranteed presence.
    fn resource_limits_for(&self, kind: RendererKind) -> RendererResourceLimits {
        self.resource_limits_by_kind[&kind]
    }
}

/// How a wallpaper's picture maps onto the output (BETA F1,
/// `docs/backlog/WALLPAPER_SCALING_MODES.md`). The same mode travels to two
/// places: the renderer (content → frame canvas: the video renderer's
/// letterbox/crop/stretch of the clip, the scene renderer's scene-units →
/// canvas mapping) and the Plasma plugin (frame canvas → output item), so
/// when the canvas already has the output's aspect the plugin step is the
/// identity and the renderer step is what the user sees.
#[derive(Debug, Clone, Copy, Default, Deserialize, Serialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum ScalingMode {
    /// Whole picture visible, aspect preserved, letterboxed (the only
    /// behaviour before F1).
    #[default]
    Aspect,
    /// Aspect preserved, scaled to cover, overflow cropped.
    Fill,
    /// Scaled to the exact target, aspect ignored.
    Stretch,
}

impl ScalingMode {
    pub fn as_str(self) -> &'static str {
        match self {
            ScalingMode::Aspect => "aspect",
            ScalingMode::Fill => "fill",
            ScalingMode::Stretch => "stretch",
        }
    }
}

#[derive(Debug, Clone)]
pub struct StartSpec {
    pub wallpaper_id: String,
    pub content_hash: String,
    pub width: u32,
    pub height: u32,
    pub fps: u32,
    pub kind: RendererKind,
    pub content: Option<ContentSpec>,
    pub test_fault: Option<TestFault>,
    /// Development-only: ask the test renderer for this many stderr lines.
    pub stderr_lines: Option<u32>,
    /// F1: passed to every renderer as `--scaling`; not part of the
    /// failure-record identity (a mode change never earns a new budget).
    pub scaling: ScalingMode,
    /// SR-1c: capability ids the apply gate found `required` but only
    /// tolerated-missing (`kwe_core::SCENE_CAPABILITIES_LIMITATION_TOLERATED`),
    /// so the apply proceeded with a degraded scene. Diagnostic only — never
    /// forwarded as a renderer CLI arg, not part of the failure-record
    /// identity, and not persisted into the assignment (SR-1c open risk:
    /// invisible again after a daemon restart until a later slice).
    pub capability_limitations: Vec<String>,
}

impl StartSpec {
    /// `assets_dir`: the configured Wallpaper Engine assets root (S1),
    /// forwarded to `preflight_scene`/`preflight_pkg` so a scene's model
    /// layers can resolve their material textures during preflight, not
    /// just at worker spawn.
    pub fn validate(&self, assets_dir: Option<&Path>) -> Result<()> {
        validate_identity_part("wallpaper_id", &self.wallpaper_id)?;
        validate_identity_part("content_hash", &self.content_hash)?;
        let frame_spec = FrameSpec::new(self.width, self.height)?;
        if frame_spec.file_bytes > MAX_SUPERVISED_MAPPING_BYTES {
            bail!("supervised frame mapping exceeds 128 MiB safety budget");
        }
        if !(1..=240).contains(&self.fps) {
            bail!("fps must be in 1..=240");
        }
        if let Some(fault) = &self.test_fault {
            fault.validate()?;
        }
        if self.stderr_lines.is_some() && self.kind != RendererKind::Test {
            bail!("stderr_lines is only available to the test renderer kind");
        }
        if let Some(count) = self.stderr_lines
            && !(1..=4096).contains(&count)
        {
            bail!("stderr_lines must be in 1..=4096");
        }
        match (&self.kind, &self.content) {
            (RendererKind::Test, None) => {}
            (RendererKind::Video, Some(ContentSpec::Video { path })) => {
                let report = preflight_video(path);
                if !report.safe {
                    bail!(
                        "video preflight rejected {}: {}",
                        path.display(),
                        report.reasons.join("; ")
                    );
                }
            }
            (RendererKind::Web, Some(ContentSpec::Web { root })) => {
                // No permission grants yet: the empty list keeps network
                // disabled per the preflight default.
                let report = preflight_web(root, &[]);
                if !report.safe {
                    bail!(
                        "web preflight rejected {}: {}",
                        root.display(),
                        report.reasons.join("; ")
                    );
                }
            }
            (RendererKind::Scene, Some(ContentSpec::Scene { path })) => {
                let report = preflight_scene(path, assets_dir);
                if !report.safe {
                    bail!(
                        "scene preflight rejected {}: {}",
                        path.display(),
                        report.reasons.join("; ")
                    );
                }
            }
            _ => bail!(
                "renderer kind {} requires matching content (test takes none)",
                self.kind.as_str()
            ),
        }
        Ok(())
    }

    /// Kind-qualified quarantine identity: a failing video renderer must not
    /// quarantine the same id/hash under web or scene, and vice versa.
    fn identity(&self) -> String {
        format!(
            "{}:{}:{}",
            self.wallpaper_id,
            self.content_hash,
            self.kind.as_str()
        )
    }

    /// Pre-M1a record key (no kind). Old supervisor-v1.json records persist
    /// under `id:hash`; lookups fall back to this key so a previously
    /// quarantined identity keeps its quarantine, and the next failure
    /// migrates the record onto the kind-qualified key.
    fn legacy_identity(&self) -> String {
        format!("{}:{}", self.wallpaper_id, self.content_hash)
    }

    /// Validate and return the spec with content paths resolved in place.
    /// Called exactly once per start (in the RPC layer); the supervisor
    /// event loop consumes the result without re-running preflight, which
    /// can read up to 16 MiB of scene/web content. The video content path
    /// is preflighted first (so a symlink or missing entry is rejected)
    /// and then canonicalized into the validated spec so spawn passes the
    /// resolved file rather than the caller-supplied path, which could be
    /// re-pointed between validation and exec.
    pub fn into_validated(mut self, assets_dir: Option<&Path>) -> Result<Self> {
        self.validate(assets_dir)?;
        if self.kind == RendererKind::Video
            && let Some(ContentSpec::Video { path }) = &self.content
        {
            self.content = Some(ContentSpec::Video {
                path: fs::canonicalize(path)
                    .with_context(|| format!("resolve video content {}", path.display()))?,
            });
        }
        Ok(self)
    }
}

#[derive(Debug, Clone)]
pub enum TestFault {
    StartupHang,
    Hang { after: u64 },
    Corrupt { after: u64 },
    Exit { after: u64 },
    IgnoreTermHang { after: u64 },
    MemoryPressure { after: u64, mib: u64 },
}

impl TestFault {
    fn validate(&self) -> Result<()> {
        let after = match self {
            Self::StartupHang => return Ok(()),
            Self::Hang { after }
            | Self::Corrupt { after }
            | Self::Exit { after }
            | Self::IgnoreTermHang { after }
            | Self::MemoryPressure { after, .. } => *after,
        };
        if !(1..=100_000).contains(&after) {
            bail!("fault frame must be in 1..=100000");
        }
        if let Self::MemoryPressure { mib, .. } = self
            && !(1..=4096).contains(mib)
        {
            bail!("memory pressure must be in 1..=4096 MiB");
        }
        Ok(())
    }
}

#[derive(Debug, Clone, Copy, Serialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum WorkerPhase {
    Idle,
    Starting,
    Canary,
    Live,
    Restarting,
    AwaitingAck,
    RolledBack,
    Stopped,
    Quarantined,
}

#[derive(Debug, Clone, Serialize)]
pub struct WorkerStatus {
    pub phase: WorkerPhase,
    pub kind: RendererKind,
    pub wallpaper_id: Option<String>,
    pub content_hash: Option<String>,
    pub pid: Option<u32>,
    pub frame_file: Option<PathBuf>,
    pub last_good_file: Option<PathBuf>,
    pub sequence: u64,
    pub failures: u32,
    pub restart_count: u32,
    pub forced_kill_count: u64,
    pub last_failure: Option<FailureKind>,
    pub last_failure_detail: Option<String>,
    /// True when the requested identity's persisted record is quarantined
    /// (three strikes); the apply lane reports this as `apply_quarantined`
    /// with the record's last detail instead of a bare phase name (B4).
    pub quarantined: bool,
    /// F1: the active worker's scaling mode (the requested one while
    /// nothing is live), for the display plugin's frame → output mapping.
    pub scaling: ScalingMode,
    pub requested_wallpaper_id: Option<String>,
    pub requested_content_hash: Option<String>,
    pub candidate_pid: Option<u32>,
    pub candidate_frame_file: Option<PathBuf>,
    pub candidate_sequence: u64,
    pub previous_pid: Option<u32>,
    pub previous_frame_file: Option<PathBuf>,
    pub display_generation: u64,
    pub awaiting_display_ack: bool,
    pub resource_limits: RendererResourceLimits,
    pub input_sequence: u64,
    pub input_ack_sequence: u64,
    pub input_pending: bool,
    pub input_coalesced: u64,
    pub input_protocol_errors: u64,
    pub pointer_inside: bool,
    pub pointer_x: u16,
    pub pointer_y: u16,
    pub audio_pending: bool,
    pub audio_coalesced: u64,
    /// Frames silently dropped latest-wins because the active worker's
    /// wallpaper lacks the audio grant (BETA_M2c). The capture worker keeps
    /// running — capture is global, grants gate delivery.
    pub audio_grant_dropped: u64,
    pub media_pending: bool,
    pub media_coalesced: u64,
    /// Bounded stderr diagnostics, newest last. Content is advisory only and
    /// is never parsed as a command.
    pub stderr_tail: Vec<String>,
    pub stderr_dropped_bytes: u64,
    /// SR-1c: mirrors the active (else requested) spec's
    /// `capability_limitations` — capabilities the apply gate tolerated as
    /// missing rather than refusing the apply over.
    pub capability_limitations: Vec<String>,
}

#[derive(Debug, Clone, Copy, Deserialize, Serialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum FailureKind {
    StartupTimeout,
    FrameTimeout,
    InvalidFrame,
    ProcessExit,
    LaunchFailed,
    ResourceLimit,
    /// The worker declined to render before its first publish — exit 73
    /// (`backend_reject`: the browser/backend could not boot in this
    /// environment) or 74 (`no_drawable_content`: the scene needs features
    /// this build lacks). A refusal is "cannot run this here/now", not a
    /// crash: it never restarts, never strikes toward quarantine, and its
    /// detail is kept for the apply error (BETA B4,
    /// docs/bugs/APPLY_REJECTED_QUARANTINED.md). The same exit codes from
    /// an ACTIVE worker (web heartbeat exit 73 after first paint) are
    /// runtime failures and strike as `ProcessExit`.
    Refused,
}

#[derive(Debug, Clone, Deserialize, Serialize)]
struct FailureRecord {
    wallpaper_id: String,
    content_hash: String,
    failures: u32,
    quarantined: bool,
    last_failure: FailureKind,
    last_detail: String,
    updated_unix_seconds: u64,
}

#[derive(Debug, Clone, Deserialize, Serialize)]
struct LastGoodRecord {
    wallpaper_id: String,
    content_hash: String,
    width: u32,
    height: u32,
    sequence: u64,
    file: String,
    updated_unix_seconds: u64,
}

#[derive(Debug, Default, Deserialize, Serialize)]
struct PersistedState {
    schema_version: u32,
    #[serde(default)]
    forced_kill_count: u64,
    #[serde(default)]
    records: BTreeMap<String, FailureRecord>,
    last_good: Option<LastGoodRecord>,
    /// Identity of the daemon + renderer binaries that earned `records`
    /// (B4). A failure record describes how THIS build behaved; a new
    /// build (package upgrade) may have fixed the cause, so records from
    /// another build are dropped at load instead of banning content
    /// forever. Additive: an old file without it is treated as "unknown
    /// build" and its records are dropped once.
    #[serde(default)]
    build_id: Option<String>,
}

struct StateStore {
    directory: PathBuf,
    path: PathBuf,
}

impl StateStore {
    fn open(directory: PathBuf) -> Result<(Self, PersistedState)> {
        ensure_private_dir(&directory)?;
        let store = Self {
            path: directory.join("supervisor-v1.json"),
            directory,
        };
        let state = store.load()?;
        Ok((store, state))
    }

    /// `open`, then reconcile the failure records with the running build:
    /// records earned by a different daemon/renderer build are dropped
    /// (logged once with the count) and the file is rewritten under the
    /// current `build_id`. Everything else in the state (last-good frame,
    /// forced-kill count) is kept.
    fn open_for_build(directory: PathBuf, build_id: &str) -> Result<(Self, PersistedState)> {
        let (store, mut state) = Self::open(directory)?;
        if state.build_id.as_deref() != Some(build_id) {
            let dropped = state.records.len();
            let previous = state
                .build_id
                .take()
                .unwrap_or_else(|| "unknown".to_string());
            state.records.clear();
            state.build_id = Some(build_id.to_string());
            if dropped > 0 {
                eprintln!(
                    "event=renderer.quarantine_reset reason=build_changed dropped={dropped} previous_build={} build={build_id}",
                    truncate_detail(&previous)
                );
            }
            store.save(&state)?;
        }
        Ok((store, state))
    }

    fn load(&self) -> Result<PersistedState> {
        let mut file = match OpenOptions::new()
            .read(true)
            .custom_flags(libc::O_CLOEXEC | libc::O_NOFOLLOW)
            .open(&self.path)
        {
            Ok(file) => file,
            Err(error) if error.kind() == io::ErrorKind::NotFound => {
                return Ok(PersistedState {
                    schema_version: 1,
                    ..PersistedState::default()
                });
            }
            Err(error) => return Err(error.into()),
        };
        let metadata = file.metadata()?;
        if !metadata.is_file() || metadata.len() > MAX_STATE_BYTES {
            bail!("supervisor state is not a bounded regular file");
        }
        let mut bytes = Vec::with_capacity(metadata.len() as usize);
        file.read_to_end(&mut bytes)?;
        let state: PersistedState =
            serde_json::from_slice(&bytes).context("parse supervisor state")?;
        if state.schema_version != 1 || state.records.len() > MAX_RECORDS {
            bail!("unsupported or oversized supervisor state");
        }
        Ok(state)
    }

    fn save(&self, state: &PersistedState) -> Result<()> {
        let bytes = serde_json::to_vec_pretty(state)?;
        if bytes.len() as u64 > MAX_STATE_BYTES {
            bail!("supervisor state exceeds {MAX_STATE_BYTES} bytes");
        }
        atomic_write(&self.path, &bytes)
    }

    fn save_last_good(
        &self,
        snapshot: &FrameSnapshot,
        current_file: Option<&str>,
    ) -> Result<PathBuf> {
        let next_file = if current_file == Some("last-good-a.ppm") {
            "last-good-b.ppm"
        } else {
            "last-good-a.ppm"
        };
        let path = self.directory.join(next_file);
        let ppm = encode_ppm(snapshot)?;
        atomic_write(&path, &ppm)?;
        Ok(path)
    }
}

enum ControlCommand {
    Start(StartSpec, mpsc::Sender<Result<WorkerStatus>>),
    Retry(StartSpec, mpsc::Sender<Result<WorkerStatus>>),
    Stop(mpsc::Sender<Result<WorkerStatus>>),
    Status(mpsc::Sender<Result<WorkerStatus>>),
    Acknowledge(u64, mpsc::Sender<Result<WorkerStatus>>),
    PointerInput {
        generation: u64,
        phase: PointerPhase,
        x: f64,
        y: f64,
        button: Option<PointerButton>,
        reply: mpsc::Sender<Result<WorkerStatus>>,
    },
    AudioFrame {
        generation: u64,
        frame: AudioFrame,
        reply: mpsc::Sender<Result<WorkerStatus>>,
    },
    MediaState {
        generation: u64,
        state: MediaState,
        reply: mpsc::Sender<Result<WorkerStatus>>,
    },
    PermissionsGet(String, mpsc::Sender<Result<Grant>>),
    PermissionsSet {
        wallpaper_id: String,
        patch: GrantPatch,
        reply: mpsc::Sender<Result<Grant>>,
    },
    PermissionsList(mpsc::Sender<Result<BTreeMap<String, Grant>>>),
    QuarantinedIds(mpsc::Sender<BTreeSet<String>>),
    Shutdown,
}

#[derive(Debug, Clone)]
pub struct SupervisorHandle {
    sender: SyncSender<ControlCommand>,
}

impl SupervisorHandle {
    pub fn start(&self, spec: StartSpec) -> Result<WorkerStatus> {
        self.request(|reply| ControlCommand::Start(spec, reply))
    }

    pub fn retry(&self, spec: StartSpec) -> Result<WorkerStatus> {
        self.request(|reply| ControlCommand::Retry(spec, reply))
    }

    pub fn stop(&self) -> Result<WorkerStatus> {
        self.request(ControlCommand::Stop)
    }

    pub fn status(&self) -> Result<WorkerStatus> {
        self.request(ControlCommand::Status)
    }

    pub fn acknowledge(&self, generation: u64) -> Result<WorkerStatus> {
        self.request(|reply| ControlCommand::Acknowledge(generation, reply))
    }

    pub fn pointer_input(
        &self,
        generation: u64,
        phase: PointerPhase,
        x: f64,
        y: f64,
        button: Option<PointerButton>,
    ) -> Result<WorkerStatus> {
        self.request(|reply| ControlCommand::PointerInput {
            generation,
            phase,
            x,
            y,
            button,
            reply,
        })
    }

    pub fn audio_frame(&self, generation: u64, frame: AudioFrame) -> Result<WorkerStatus> {
        self.request(|reply| ControlCommand::AudioFrame {
            generation,
            frame,
            reply,
        })
    }

    pub fn media_state(&self, generation: u64, state: MediaState) -> Result<WorkerStatus> {
        self.request(|reply| ControlCommand::MediaState {
            generation,
            state,
            reply,
        })
    }

    /// Returns the effective grant record for a wallpaper (documented
    /// defaults when none exists).
    pub fn permissions_get(&self, wallpaper_id: String) -> Result<Grant> {
        self.request_value(|reply| ControlCommand::PermissionsGet(wallpaper_id, reply))
    }

    /// Patches the stored grant record and persists it atomically; returns
    /// the new effective record.
    pub fn permissions_set(&self, wallpaper_id: String, patch: GrantPatch) -> Result<Grant> {
        self.request_value(|reply| ControlCommand::PermissionsSet {
            wallpaper_id,
            patch,
            reply,
        })
    }

    /// Every stored grant record (bounded by `MAX_GRANTS`).
    pub fn permissions_list(&self) -> Result<BTreeMap<String, Grant>> {
        self.request_value(ControlCommand::PermissionsList)
    }

    /// Returns the wallpaper IDs with at least one quarantined failure record.
    /// Used by the playlist session to skip quarantined content. The caller
    /// chooses the deadline so frequent pollers can fall back to a cached
    /// value on timeout.
    pub fn try_quarantined_ids(&self, timeout: Duration) -> Result<BTreeSet<String>> {
        let (sender, receiver) = mpsc::channel();
        match self.sender.try_send(ControlCommand::QuarantinedIds(sender)) {
            Ok(()) => {}
            Err(TrySendError::Full(_)) => bail!("supervisor command queue is full"),
            Err(TrySendError::Disconnected(_)) => bail!("supervisor is unavailable"),
        }
        receiver
            .recv_timeout(timeout)
            .map_err(|_| anyhow!("supervisor command timed out"))
    }

    fn request(
        &self,
        make: impl FnOnce(mpsc::Sender<Result<WorkerStatus>>) -> ControlCommand,
    ) -> Result<WorkerStatus> {
        self.request_value(make)
    }

    /// Generic reply-channel round trip, shared by every command: enqueue
    /// the command (bounded queue, bounded timeout), then wait for the
    /// supervisor thread's reply.
    fn request_value<T>(
        &self,
        make: impl FnOnce(mpsc::Sender<Result<T>>) -> ControlCommand,
    ) -> Result<T> {
        let (sender, receiver) = mpsc::channel();
        match self.sender.try_send(make(sender)) {
            Ok(()) => {}
            Err(TrySendError::Full(_)) => bail!("supervisor command queue is full"),
            Err(TrySendError::Disconnected(_)) => bail!("supervisor is unavailable"),
        }
        receiver
            .recv_timeout(COMMAND_TIMEOUT)
            .map_err(|_| anyhow!("supervisor command timed out"))?
    }
}

pub struct SupervisorService {
    handle: SupervisorHandle,
    thread: Option<JoinHandle<()>>,
}

impl SupervisorService {
    pub fn start(config: SupervisorConfig) -> Result<Self> {
        let config = config.validate()?;
        let build_id = build_identity(&config);
        let (store, state) = StateStore::open_for_build(config.state_dir.clone(), &build_id)?;
        let grant_store = GrantStore::open(&config.state_dir)?;
        let (sender, receiver) = mpsc::sync_channel(COMMAND_CAPACITY);
        let thread = thread::Builder::new()
            .name("kwe-renderer-supervisor".into())
            .spawn(move || {
                SupervisorRuntime::new(config, store, state, grant_store).run(receiver)
            })?;
        Ok(Self {
            handle: SupervisorHandle { sender },
            thread: Some(thread),
        })
    }

    pub fn handle(&self) -> SupervisorHandle {
        self.handle.clone()
    }
}

impl Drop for SupervisorService {
    fn drop(&mut self) {
        let _ = self.handle.sender.send(ControlCommand::Shutdown);
        if let Some(thread) = self.thread.take() {
            let _ = thread.join();
        }
    }
}

struct ActiveWorker {
    spec: StartSpec,
    child: Child,
    home_path: PathBuf,
    frame_path: PathBuf,
    reader: Option<SharedFrameReader>,
    started: Instant,
    last_progress: Instant,
    last_snapshot_saved: Option<Instant>,
    sequence: u64,
    input: ChildStdin,
    input_ack: ChildStdout,
    input_ack_buffer: Vec<u8>,
    input_sequence: u64,
    input_ack_sequence: u64,
    pending_input: Option<Vec<u8>>,
    input_coalesced: u64,
    input_protocol_errors: u64,
    pointer_inside: bool,
    pointer_x: u16,
    pointer_y: u16,
    // One pending frame per stream; a newer message replaces the older one
    // instead of queuing, exactly like pointer input.
    pending_audio: Option<Vec<u8>>,
    audio_coalesced: u64,
    pending_media: Option<Vec<u8>>,
    media_coalesced: u64,
    stderr: ChildStderr,
    stderr_ring: StderrRing,
}

struct PendingRestart {
    spec: StartSpec,
    at: Instant,
}

struct RetiredWorker {
    worker: ActiveWorker,
    generation: u64,
    deadline: Instant,
    promotion_snapshot: FrameSnapshot,
}

struct SupervisorRuntime {
    config: SupervisorConfig,
    store: StateStore,
    persisted: PersistedState,
    /// Daemon-owned per-wallpaper permission grants (BETA_M2c): the network
    /// grant gates `--allow-network` on web launches and the audio grant
    /// gates `audio.forward` delivery. Grants re-read per spawn, so a
    /// revocation takes effect on the next `renderer.start`.
    grant_store: GrantStore,
    /// Lifetime count of frames dropped because the active wallpaper lacks
    /// the audio grant; surfaced through `WorkerStatus::audio_grant_dropped`.
    audio_grant_dropped: u64,
    active: Option<ActiveWorker>,
    candidate: Option<ActiveWorker>,
    retired: Option<RetiredWorker>,
    pending: Option<PendingRestart>,
    requested: Option<StartSpec>,
    phase: WorkerPhase,
    restart_count: u32,
    last_failure: Option<(FailureKind, String)>,
    launch_serial: u64,
    display_generation: u64,
}

impl SupervisorRuntime {
    fn new(
        config: SupervisorConfig,
        store: StateStore,
        persisted: PersistedState,
        grant_store: GrantStore,
    ) -> Self {
        Self {
            config,
            store,
            persisted,
            grant_store,
            audio_grant_dropped: 0,
            active: None,
            candidate: None,
            retired: None,
            pending: None,
            requested: None,
            phase: WorkerPhase::Idle,
            restart_count: 0,
            last_failure: None,
            launch_serial: 0,
            display_generation: 0,
        }
    }

    fn run(mut self, receiver: Receiver<ControlCommand>) {
        loop {
            match receiver.recv_timeout(POLL_INTERVAL) {
                Ok(ControlCommand::Start(spec, reply)) => {
                    let result = self.start_selected(spec, false);
                    let _ = reply.send(result);
                }
                Ok(ControlCommand::Retry(spec, reply)) => {
                    let result = self.start_selected(spec, true);
                    let _ = reply.send(result);
                }
                Ok(ControlCommand::Stop(reply)) => {
                    let result = self.stop_selected();
                    let _ = reply.send(result);
                }
                Ok(ControlCommand::Status(reply)) => {
                    let _ = reply.send(Ok(self.status()));
                }
                Ok(ControlCommand::Acknowledge(generation, reply)) => {
                    let result = self.acknowledge_display(generation);
                    let _ = reply.send(result);
                }
                Ok(ControlCommand::PointerInput {
                    generation,
                    phase,
                    x,
                    y,
                    button,
                    reply,
                }) => {
                    let result = self.forward_pointer_input(generation, phase, x, y, button);
                    let _ = reply.send(result);
                }
                Ok(ControlCommand::AudioFrame {
                    generation,
                    frame,
                    reply,
                }) => {
                    let result = self.forward_audio_frame(generation, frame);
                    let _ = reply.send(result);
                }
                Ok(ControlCommand::MediaState {
                    generation,
                    state,
                    reply,
                }) => {
                    let result = self.forward_media_state(generation, state);
                    let _ = reply.send(result);
                }
                Ok(ControlCommand::PermissionsGet(wallpaper_id, reply)) => {
                    let _ = reply.send(Ok(self.grant_store.grant(&wallpaper_id)));
                }
                Ok(ControlCommand::PermissionsSet {
                    wallpaper_id,
                    patch,
                    reply,
                }) => {
                    let result = self.grant_store.set(&wallpaper_id, patch);
                    let _ = reply.send(result);
                }
                Ok(ControlCommand::PermissionsList(reply)) => {
                    let _ = reply.send(Ok(self.grant_store.all().clone()));
                }
                Ok(ControlCommand::QuarantinedIds(reply)) => {
                    let ids = self
                        .persisted
                        .records
                        .values()
                        .filter(|record| record.quarantined)
                        .map(|record| record.wallpaper_id.clone())
                        .collect();
                    let _ = reply.send(ids);
                }
                Ok(ControlCommand::Shutdown) | Err(mpsc::RecvTimeoutError::Disconnected) => {
                    self.stop_all(false);
                    return;
                }
                Err(mpsc::RecvTimeoutError::Timeout) => {}
            }
            self.tick();
        }
    }

    fn start_selected(&mut self, spec: StartSpec, clear_failure: bool) -> Result<WorkerStatus> {
        // The RPC layer validated this spec once (StartSpec::into_validated);
        // preflight must not run again here because it blocks the single
        // supervisor thread on up to 16 MiB of scene/web content reads.
        if self.retired.is_some() {
            bail!("display handoff is still awaiting acknowledgement");
        }
        self.stop_candidate(false);
        self.pending = None;
        self.restart_count = 0;
        self.last_failure = None;
        let identity = spec.identity();
        let legacy = spec.legacy_identity();
        if clear_failure {
            self.persisted.records.remove(&identity);
            self.persisted.records.remove(&legacy);
            self.store.save(&self.persisted)?;
        } else if let Some(record) = self
            .persisted
            .records
            .get(&identity)
            .or_else(|| self.persisted.records.get(&legacy))
            .filter(|record| record.quarantined)
            .cloned()
        {
            // Surface WHY it is quarantined: the record's last failure and
            // detail ride along in status() so the apply error can name the
            // cause instead of the bare phase (B4).
            self.last_failure = Some((record.last_failure, record.last_detail));
            self.requested = Some(spec);
            self.phase = if self.active.is_some() {
                WorkerPhase::RolledBack
            } else {
                WorkerPhase::Quarantined
            };
            return Ok(self.status());
        }
        self.requested = Some(spec.clone());
        match self.spawn_worker(spec.clone()) {
            Ok(worker) => {
                self.candidate = Some(worker);
                self.phase = if self.active.is_some() {
                    WorkerPhase::Canary
                } else {
                    WorkerPhase::Starting
                };
            }
            Err(error) => {
                self.handle_candidate_failure(FailureKind::LaunchFailed, error.to_string(), spec);
            }
        }
        Ok(self.status())
    }

    fn stop_selected(&mut self) -> Result<WorkerStatus> {
        self.pending = None;
        self.stop_all(true);
        self.phase = WorkerPhase::Stopped;
        self.store.save(&self.persisted)?;
        Ok(self.status())
    }

    fn acknowledge_display(&mut self, generation: u64) -> Result<WorkerStatus> {
        let expected = self
            .retired
            .as_ref()
            .map_or(self.display_generation, |retired| retired.generation);
        if generation != expected {
            bail!("display generation mismatch: current is {}", expected);
        }
        if let Some(retired) = self.retired.take() {
            let Some(spec) = self.active.as_ref().map(|worker| worker.spec.clone()) else {
                self.retired = Some(retired);
                bail!("cannot acknowledge a handoff without an active renderer");
            };
            if let Err(error) = self.persist_last_good(&spec, &retired.promotion_snapshot) {
                self.retired = Some(retired);
                return Err(error.context("persist promoted fallback before acknowledgement"));
            }
            self.stop_worker(retired.worker, true);
            self.clear_success_record(&spec);
        }
        if self.active.is_some() && self.candidate.is_none() {
            self.phase = WorkerPhase::Live;
        }
        Ok(self.status())
    }

    fn spawn_worker(&mut self, spec: StartSpec) -> Result<ActiveWorker> {
        self.launch_serial = self.launch_serial.wrapping_add(1);
        // Fails closed when this kind has no configured binary; the launch
        // never falls back to another kind's renderer.
        let renderer_path = self.config.renderer_path_for(spec.kind)?;
        let frame_path = self.config.runtime_dir.join(format!(
            "frame-{}-{}.bin",
            std::process::id(),
            self.launch_serial
        ));
        // Private per-worker HOME, chmod 0700: Chromium-style web renderers
        // take a profile lock under $HOME, so a shared HOME would let the
        // canary and the active worker contend on it during handoff.
        let home_dir = self
            .config
            .runtime_dir
            .join(format!("home-{}", self.launch_serial));
        fs::create_dir_all(&home_dir)
            .with_context(|| format!("create renderer home {}", home_dir.display()))?;
        fs::set_permissions(&home_dir, fs::Permissions::from_mode(0o700))
            .with_context(|| format!("protect renderer home {}", home_dir.display()))?;
        let mut command = ProcessCommand::new(&renderer_path);
        command
            .arg("--output")
            .arg(&frame_path)
            .arg("--width")
            .arg(spec.width.to_string())
            .arg("--height")
            .arg(spec.height.to_string())
            .arg("--fps")
            .arg(spec.fps.to_string())
            .arg("--scaling")
            .arg(spec.scaling.as_str());
        if let Some(content) = &spec.content {
            let path = match content {
                ContentSpec::Video { path } | ContentSpec::Scene { path } => path,
                ContentSpec::Web { root } => root,
            };
            command.arg("--content").arg(path);
        }
        if spec.kind == RendererKind::Scene
            && let Some(assets_dir) = &self.config.scene_assets_dir
        {
            command.arg("--assets-dir").arg(assets_dir);
        }
        // SR-3b: the killable shader-compile helper, scene kind only.
        // Absent -> the flag is simply omitted (the renderer's own
        // sibling-resolution fallback still applies; see
        // `shader_helper::ShaderHelper::new`, kwe-scene-renderer).
        if spec.kind == RendererKind::Scene
            && let Some(shader_helper) = &self.config.shader_helper_path
        {
            command.arg("--shader-helper").arg(shader_helper);
        }
        if let Some(count) = spec.stderr_lines {
            command.arg("--stderr-lines").arg(count.to_string());
        }
        if spec.kind == RendererKind::Web {
            // The worker's session-scoped liveness heartbeat (a page wedged
            // after first paint otherwise looks alive forever behind the
            // keepalive re-publication).
            command
                .arg("--web-heartbeat-ms")
                .arg(self.config.web_heartbeat_ms.to_string())
                .arg("--web-heartbeat-max-failures")
                .arg(self.config.web_heartbeat_max_failures.to_string());
            // BETA_M2c: the per-wallpaper network grant is the only path to
            // --allow-network (the M2b per-request test hook is removed).
            // The record is re-read at every spawn, so a revocation makes
            // the next launch build the bwrap sandbox with --unshare-net
            // again; every ungranted worker keeps the netns isolation.
            if self.grant_store.grant(&spec.wallpaper_id).network {
                command.arg("--allow-network");
            }
        }
        command
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .env_clear()
            .envs(env_allowlist(spec.kind, &home_dir));
        if let Some(fault) = &spec.test_fault {
            match fault {
                TestFault::StartupHang => {
                    command.arg("--startup-hang");
                }
                TestFault::Hang { after } => {
                    command.arg("--hang-after").arg(after.to_string());
                }
                TestFault::Corrupt { after } => {
                    command.arg("--corrupt-after").arg(after.to_string());
                }
                TestFault::Exit { after } => {
                    command.arg("--exit-after").arg(after.to_string());
                }
                TestFault::IgnoreTermHang { after } => {
                    command
                        .arg("--hang-after")
                        .arg(after.to_string())
                        .arg("--ignore-term");
                }
                TestFault::MemoryPressure { after, mib } => {
                    command
                        .arg("--memory-pressure-after")
                        .arg(after.to_string())
                        .arg("--memory-pressure-mib")
                        .arg(mib.to_string());
                }
            }
        }
        let expected_parent = i32::try_from(std::process::id()).context("daemon pid overflow")?;
        let resource_limits = self.config.resource_limits_for(spec.kind);
        // SAFETY: this closure runs in the child after fork and before exec. It
        // calls only async-signal-safe libc functions and does not allocate.
        unsafe {
            command.pre_exec(move || {
                if libc::setpgid(0, 0) != 0 {
                    return Err(io::Error::last_os_error());
                }
                // Parent-death signal, parent-pid check, no-new-privs
                // (Linux); parent-pid check only on macOS, where the
                // worker-side kqueue guard covers parent death.
                kwe_platform::child_pre_exec(expected_parent, libc::SIGKILL)?;
                apply_resource_limits(resource_limits)?;
                Ok(())
            });
        }
        let mut child = match command.spawn() {
            Ok(child) => child,
            Err(error) => {
                cleanup_renderer_home(&home_dir);
                return Err(error)
                    .with_context(|| format!("launch renderer {}", renderer_path.display()));
            }
        };
        let channels = (|| -> Result<(ChildStdin, ChildStdout, ChildStderr)> {
            let input = child
                .stdin
                .take()
                .context("renderer input pipe unavailable")?;
            let input_ack = child
                .stdout
                .take()
                .context("renderer input acknowledgement pipe unavailable")?;
            let stderr = child
                .stderr
                .take()
                .context("renderer diagnostic pipe unavailable")?;
            set_nonblocking(input.as_raw_fd()).context("configure renderer input pipe")?;
            set_nonblocking(input_ack.as_raw_fd())
                .context("configure renderer acknowledgement pipe")?;
            set_nonblocking(stderr.as_raw_fd()).context("configure renderer diagnostic pipe")?;
            Ok((input, input_ack, stderr))
        })();
        let (input, input_ack, stderr) = match channels {
            Ok(channels) => channels,
            Err(error) => {
                terminate_and_reap(&mut child, self.config.stop_grace);
                cleanup_renderer_home(&home_dir);
                return Err(error);
            }
        };
        let now = Instant::now();
        Ok(ActiveWorker {
            spec,
            child,
            home_path: home_dir,
            frame_path,
            reader: None,
            started: now,
            last_progress: now,
            last_snapshot_saved: None,
            sequence: 0,
            input,
            input_ack,
            input_ack_buffer: Vec::with_capacity(MAX_ACK_BUFFER_BYTES),
            input_sequence: 0,
            input_ack_sequence: 0,
            pending_input: None,
            input_coalesced: 0,
            input_protocol_errors: 0,
            pointer_inside: false,
            pointer_x: 0,
            pointer_y: 0,
            pending_audio: None,
            audio_coalesced: 0,
            pending_media: None,
            media_coalesced: 0,
            stderr,
            stderr_ring: StderrRing::default(),
        })
    }

    fn forward_pointer_input(
        &mut self,
        generation: u64,
        phase: PointerPhase,
        x: f64,
        y: f64,
        button: Option<PointerButton>,
    ) -> Result<WorkerStatus> {
        if generation == 0 || generation != self.display_generation {
            bail!("pointer input display generation is stale or invalid");
        }
        let worker = self
            .active
            .as_mut()
            .context("no promoted renderer is available for pointer input")?;
        let sequence = worker
            .input_sequence
            .checked_add(1)
            .context("pointer input sequence exhausted")?;
        let message = match button {
            Some(button) => PointerMessage::button_event(sequence, phase, button, x, y)?,
            None => PointerMessage::from_normalized(sequence, phase, x, y)?,
        };
        let bytes = encode_pointer_line(&message)?;
        queue_control_message(
            &worker.input,
            bytes,
            &mut worker.pending_input,
            &mut worker.input_coalesced,
        )?;
        worker.input_sequence = sequence;
        worker.pointer_inside = phase != PointerPhase::Leave;
        worker.pointer_x = message.x;
        worker.pointer_y = message.y;
        Ok(self.status())
    }

    fn forward_audio_frame(&mut self, generation: u64, frame: AudioFrame) -> Result<WorkerStatus> {
        if generation == 0 || generation != self.display_generation {
            bail!("audio frame display generation is stale or invalid");
        }
        let worker = self
            .active
            .as_mut()
            .context("no promoted renderer is available for audio forwarding")?;
        // BETA_M2c: the audio grant gates delivery, not capture — the
        // capture worker keeps running (capture is global) and frames for a
        // wallpaper without the audio grant are dropped silently latest-wins,
        // counted in status and logged at a bounded rate.
        if !self.grant_store.grant(&worker.spec.wallpaper_id).audio {
            self.audio_grant_dropped = self.audio_grant_dropped.saturating_add(1);
            log_audio_grant_drop();
            return Ok(self.status());
        }
        let bytes = encode_audio_frame(&frame)?;
        queue_control_message(
            &worker.input,
            bytes,
            &mut worker.pending_audio,
            &mut worker.audio_coalesced,
        )?;
        // The wire sequence carries the display generation by design; raise
        // the acceptance ceiling so the worker's echoed ack passes below
        // (mirrors the pointer path's bookkeeping).
        raise_ack_ceiling(worker, generation);
        Ok(self.status())
    }

    fn forward_media_state(&mut self, generation: u64, state: MediaState) -> Result<WorkerStatus> {
        if generation == 0 || generation != self.display_generation {
            bail!("media state display generation is stale or invalid");
        }
        let worker = self
            .active
            .as_mut()
            .context("no promoted renderer is available for media state")?;
        let bytes = encode_media_state(&state)?;
        queue_control_message(
            &worker.input,
            bytes,
            &mut worker.pending_media,
            &mut worker.media_coalesced,
        )?;
        // The wire sequence carries the display generation by design; raise
        // the acceptance ceiling so the worker's echoed ack passes below
        // (mirrors the pointer path's bookkeeping).
        raise_ack_ceiling(worker, generation);
        Ok(self.status())
    }

    fn service_active_input(&mut self) {
        let Some(worker) = self.active.as_mut() else {
            return;
        };
        drain_input_acks(worker);
        flush_pending(
            &worker.input,
            &mut worker.pending_input,
            &mut worker.input_protocol_errors,
            "renderer.input_write_error",
        );
        flush_pending(
            &worker.input,
            &mut worker.pending_audio,
            &mut worker.input_protocol_errors,
            "renderer.audio_write_error",
        );
        flush_pending(
            &worker.input,
            &mut worker.pending_media,
            &mut worker.input_protocol_errors,
            "renderer.media_write_error",
        );
    }

    fn tick(&mut self) {
        self.service_active_input();
        if self
            .retired
            .as_ref()
            .is_some_and(|retired| Instant::now() >= retired.deadline)
        {
            self.complete_handoff_timeout();
        }

        if self.candidate.is_none()
            && let Some(pending) = self.pending.take()
        {
            if Instant::now() < pending.at {
                self.pending = Some(pending);
            } else {
                match self.spawn_worker(pending.spec.clone()) {
                    Ok(worker) => {
                        self.candidate = Some(worker);
                        self.phase = if self.active.is_some() {
                            WorkerPhase::Canary
                        } else {
                            WorkerPhase::Starting
                        };
                    }
                    Err(error) => self.handle_candidate_failure(
                        FailureKind::LaunchFailed,
                        error.to_string(),
                        pending.spec,
                    ),
                }
            }
        }

        let active_observation = self
            .active
            .as_mut()
            .and_then(|worker| inspect_worker(worker, &self.config));
        if let Some(observation) = active_observation {
            match observation {
                WorkerObservation::Progress(snapshot) => {
                    let should_save = self.retired.is_none()
                        && self.active.as_ref().is_some_and(|worker| {
                            worker.last_snapshot_saved.is_none_or(|saved| {
                                Instant::now().duration_since(saved) >= SNAPSHOT_INTERVAL
                            })
                        });
                    if should_save {
                        if let Some(worker) = self.active.as_mut() {
                            worker.last_snapshot_saved = Some(Instant::now());
                        }
                        let spec = self.active.as_ref().map(|worker| worker.spec.clone());
                        if let Some(spec) = spec
                            && let Err(error) = self.persist_last_good(&spec, &snapshot)
                        {
                            eprintln!("event=renderer.snapshot_error detail={error}");
                        }
                    }
                }
                WorkerObservation::Failure(kind, detail) => {
                    self.handle_active_failure(kind, detail);
                }
            }
        }

        let candidate_observation = self
            .candidate
            .as_mut()
            .and_then(|worker| inspect_worker(worker, &self.config));
        if let Some(observation) = candidate_observation {
            match observation {
                WorkerObservation::Progress(snapshot) => {
                    let ready = self.candidate.as_ref().is_some_and(|worker| {
                        worker.sequence >= 3
                            && Instant::now().duration_since(worker.started)
                                >= self.config.canary_duration
                    });
                    if ready {
                        self.promote_candidate(snapshot);
                    } else if self.active.is_some() {
                        self.phase = WorkerPhase::Canary;
                    }
                }
                WorkerObservation::Failure(kind, detail) => {
                    if let Some(spec) = self.candidate.as_ref().map(|worker| worker.spec.clone()) {
                        self.stop_candidate(false);
                        self.handle_candidate_failure(kind, detail, spec);
                    }
                }
            }
        }
    }

    fn promote_candidate(&mut self, snapshot: FrameSnapshot) {
        let Some(mut promoted) = self.candidate.take() else {
            return;
        };
        promoted.last_snapshot_saved = Some(Instant::now());
        let promoted_spec = promoted.spec.clone();
        let previous = self.active.take();
        if previous.is_none()
            && let Err(error) = self.persist_last_good(&promoted_spec, &snapshot)
        {
            self.stop_worker(promoted, false);
            self.handle_candidate_failure(
                FailureKind::InvalidFrame,
                format!("persist_last_good:{error}"),
                promoted_spec,
            );
            return;
        }
        self.active = Some(promoted);
        self.display_generation = self.display_generation.wrapping_add(1).max(1);
        self.pending = None;
        self.restart_count = 0;
        if let Some(worker) = previous {
            self.retired = Some(RetiredWorker {
                worker,
                generation: self.display_generation,
                deadline: Instant::now() + self.config.handoff_timeout,
                promotion_snapshot: snapshot,
            });
            self.phase = WorkerPhase::AwaitingAck;
        } else {
            self.clear_success_record(&promoted_spec);
            self.phase = WorkerPhase::Live;
        }
        eprintln!(
            "event=renderer.promoted generation={} wallpaper_id={} content_hash={}",
            self.display_generation,
            self.active
                .as_ref()
                .map(|worker| worker.spec.wallpaper_id.as_str())
                .unwrap_or("unknown"),
            self.active
                .as_ref()
                .map(|worker| worker.spec.content_hash.as_str())
                .unwrap_or("unknown")
        );
    }

    fn handle_active_failure(&mut self, kind: FailureKind, detail: String) {
        let Some(worker) = self.active.take() else {
            return;
        };
        // A worker that already published and then exits 73/74 (web
        // heartbeat exit after first paint, say) failed at runtime: that is
        // a strike like any other exit, not a refusal.
        let kind = if kind == FailureKind::Refused {
            FailureKind::ProcessExit
        } else {
            kind
        };
        let spec = worker.spec.clone();
        self.stop_worker(worker, false);
        if let Some(retired) = self.retired.take() {
            self.active = Some(retired.worker);
            self.display_generation = self.display_generation.wrapping_add(1).max(1);
            self.requested = Some(spec.clone());
            let quarantined = self.record_failure(kind, &detail, &spec);
            if quarantined {
                self.phase = WorkerPhase::RolledBack;
                self.pending = None;
            } else {
                self.restart_count = self.restart_count.saturating_add(1);
                self.phase = WorkerPhase::Restarting;
                self.pending = Some(PendingRestart {
                    spec,
                    at: Instant::now() + self.config.restart_delay,
                });
            }
            eprintln!(
                "event=renderer.rollback generation={} detail={}",
                self.display_generation,
                truncate_detail(&detail)
            );
            return;
        }
        let quarantined = self.record_failure(kind, &detail, &spec);
        if self.candidate.is_some() {
            self.phase = WorkerPhase::Starting;
        } else if quarantined {
            self.requested = Some(spec);
            self.phase = WorkerPhase::Quarantined;
        } else {
            self.requested = Some(spec.clone());
            self.restart_count = self.restart_count.saturating_add(1);
            self.phase = WorkerPhase::Restarting;
            self.pending = Some(PendingRestart {
                spec,
                at: Instant::now() + self.config.restart_delay,
            });
        }
    }

    fn handle_candidate_failure(&mut self, kind: FailureKind, detail: String, spec: StartSpec) {
        if kind == FailureKind::Refused {
            // Refusal: the worker told us this content cannot run in this
            // environment/build. Retrying would only repeat the answer, and
            // three repeats used to ban the content (B4). Stop here, keep
            // the detail for status()/apply, strike nothing, persist
            // nothing; an active wallpaper stays on screen (RolledBack).
            self.last_failure = Some((kind, truncate_detail(&detail)));
            self.pending = None;
            self.requested = Some(spec.clone());
            self.phase = if self.active.is_some() {
                WorkerPhase::RolledBack
            } else {
                WorkerPhase::Stopped
            };
            eprintln!(
                "event=renderer.refused wallpaper_id={} content_hash={} detail={}",
                spec.wallpaper_id,
                spec.content_hash,
                truncate_detail(&detail)
            );
            return;
        }
        let quarantined = self.record_failure(kind, &detail, &spec);
        if quarantined {
            self.phase = if self.active.is_some() {
                WorkerPhase::RolledBack
            } else {
                WorkerPhase::Quarantined
            };
            self.pending = None;
            eprintln!(
                "event=renderer.quarantined wallpaper_id={} content_hash={}",
                spec.wallpaper_id, spec.content_hash
            );
        } else {
            self.restart_count = self.restart_count.saturating_add(1);
            self.phase = WorkerPhase::Restarting;
            self.pending = Some(PendingRestart {
                spec,
                at: Instant::now() + self.config.restart_delay,
            });
        }
    }

    fn record_failure(&mut self, kind: FailureKind, detail: &str, spec: &StartSpec) -> bool {
        let identity = spec.identity();
        // Migrate a pre-M1a `id:hash` record onto the kind-qualified key so
        // its failure history (and quarantine state) carries over instead of
        // restarting; from here on each kind is tracked independently.
        if let Some(record) = self.persisted.records.remove(&spec.legacy_identity())
            && !self.persisted.records.contains_key(&identity)
        {
            self.persisted.records.insert(identity.clone(), record);
        }
        if !self.persisted.records.contains_key(&identity)
            && self.persisted.records.len() >= MAX_RECORDS
            && let Some(oldest) = self
                .persisted
                .records
                .iter()
                .min_by_key(|(_, record)| record.updated_unix_seconds)
                .map(|(key, _)| key.clone())
        {
            self.persisted.records.remove(&oldest);
        }
        let record = self
            .persisted
            .records
            .entry(identity)
            .or_insert_with(|| FailureRecord {
                wallpaper_id: spec.wallpaper_id.clone(),
                content_hash: spec.content_hash.clone(),
                failures: 0,
                quarantined: false,
                last_failure: kind,
                last_detail: String::new(),
                updated_unix_seconds: 0,
            });
        record.failures = record.failures.saturating_add(1);
        record.quarantined = record.failures >= self.config.max_failures;
        record.last_failure = kind;
        record.last_detail = truncate_detail(detail);
        record.updated_unix_seconds = unix_seconds();
        let quarantined = record.quarantined;
        self.last_failure = Some((kind, record.last_detail.clone()));
        if let Err(error) = self.store.save(&self.persisted) {
            eprintln!("event=renderer.state_save_error detail={error}");
        }
        quarantined
    }

    fn complete_handoff_timeout(&mut self) {
        let Some(retired) = self.retired.take() else {
            return;
        };
        let Some(spec) = self.active.as_ref().map(|worker| worker.spec.clone()) else {
            self.active = Some(retired.worker);
            self.phase = WorkerPhase::RolledBack;
            return;
        };
        match self.persist_last_good(&spec, &retired.promotion_snapshot) {
            Ok(()) => {
                self.stop_worker(retired.worker, true);
                self.clear_success_record(&spec);
                self.phase = WorkerPhase::Live;
                eprintln!(
                    "event=renderer.handoff_timeout generation={} action=commit",
                    self.display_generation
                );
            }
            Err(error) => {
                let detail = format!("persist_last_good:{error}");
                if let Some(failed) = self.active.take() {
                    self.stop_worker(failed, false);
                }
                self.active = Some(retired.worker);
                self.display_generation = self.display_generation.wrapping_add(1).max(1);
                self.handle_candidate_failure(FailureKind::InvalidFrame, detail, spec);
            }
        }
    }

    fn clear_success_record(&mut self, spec: &StartSpec) {
        self.persisted.records.remove(&spec.identity());
        self.persisted.records.remove(&spec.legacy_identity());
        self.last_failure = None;
        if let Err(error) = self.store.save(&self.persisted) {
            eprintln!("event=renderer.state_save_error detail={error}");
        }
    }

    fn stop_worker(&mut self, mut worker: ActiveWorker, count_forced: bool) {
        let forced = terminate_and_reap(&mut worker.child, self.config.stop_grace);
        if forced && count_forced {
            self.persisted.forced_kill_count = self.persisted.forced_kill_count.saturating_add(1);
        }
        if let Err(error) = fs::remove_file(&worker.frame_path)
            && error.kind() != io::ErrorKind::NotFound
        {
            eprintln!("event=renderer.frame_cleanup_error detail={error}");
        }
        cleanup_renderer_home(&worker.home_path);
    }

    fn stop_candidate(&mut self, count_forced: bool) {
        if let Some(worker) = self.candidate.take() {
            self.stop_worker(worker, count_forced);
        }
    }

    fn stop_all(&mut self, count_forced: bool) {
        self.stop_candidate(count_forced);
        if let Some(retired) = self.retired.take() {
            self.stop_worker(retired.worker, count_forced);
        }
        if let Some(worker) = self.active.take() {
            self.stop_worker(worker, count_forced);
        }
    }

    fn persist_last_good(&mut self, spec: &StartSpec, snapshot: &FrameSnapshot) -> Result<()> {
        let previous = self.persisted.last_good.clone();
        let path = self.store.save_last_good(
            snapshot,
            previous.as_ref().map(|record| record.file.as_str()),
        )?;
        self.persisted.last_good = Some(LastGoodRecord {
            wallpaper_id: spec.wallpaper_id.clone(),
            content_hash: spec.content_hash.clone(),
            width: snapshot.spec.width,
            height: snapshot.spec.height,
            sequence: snapshot.sequence,
            file: path
                .file_name()
                .and_then(|name| name.to_str())
                .unwrap_or("last-good-a.ppm")
                .to_string(),
            updated_unix_seconds: unix_seconds(),
        });
        if let Err(error) = self.store.save(&self.persisted) {
            self.persisted.last_good = previous;
            return Err(error);
        }
        Ok(())
    }

    fn status(&self) -> WorkerStatus {
        let active = self.active.as_ref();
        let requested = self.requested.as_ref();
        let candidate = self.candidate.as_ref();
        let record = requested.and_then(|spec| {
            self.persisted
                .records
                .get(&spec.identity())
                .or_else(|| self.persisted.records.get(&spec.legacy_identity()))
        });
        // The active worker is the supervised process, so its kind names the
        // limits that were actually applied; idle/quarantined falls back to
        // the requested kind so the reported budget matches what will be
        // applied, then to the test budget as the former single policy.
        let status_kind = active
            .map(|worker| worker.spec.kind)
            .or_else(|| requested.map(|spec| spec.kind))
            .unwrap_or(RendererKind::Test);
        WorkerStatus {
            phase: self.phase,
            kind: status_kind,
            wallpaper_id: active.map(|worker| worker.spec.wallpaper_id.clone()),
            content_hash: active.map(|worker| worker.spec.content_hash.clone()),
            pid: active.map(|worker| worker.child.id()),
            frame_file: active.map(|worker| worker.frame_path.clone()),
            last_good_file: self
                .persisted
                .last_good
                .as_ref()
                .map(|record| self.store.directory.join(&record.file)),
            sequence: active.map_or(0, |worker| worker.sequence),
            failures: record.map_or(0, |record| record.failures),
            restart_count: self.restart_count,
            forced_kill_count: self.persisted.forced_kill_count,
            last_failure: self.last_failure.as_ref().map(|(kind, _)| *kind),
            last_failure_detail: self.last_failure.as_ref().map(|(_, detail)| detail.clone()),
            quarantined: record.is_some_and(|record| record.quarantined),
            scaling: active
                .map(|worker| worker.spec.scaling)
                .or_else(|| requested.map(|spec| spec.scaling))
                .unwrap_or_default(),
            requested_wallpaper_id: requested.map(|spec| spec.wallpaper_id.clone()),
            requested_content_hash: requested.map(|spec| spec.content_hash.clone()),
            candidate_pid: candidate.map(|worker| worker.child.id()),
            candidate_frame_file: candidate.map(|worker| worker.frame_path.clone()),
            candidate_sequence: candidate.map_or(0, |worker| worker.sequence),
            previous_pid: self
                .retired
                .as_ref()
                .map(|retired| retired.worker.child.id()),
            previous_frame_file: self
                .retired
                .as_ref()
                .map(|retired| retired.worker.frame_path.clone()),
            display_generation: self.display_generation,
            awaiting_display_ack: self.retired.is_some(),
            resource_limits: self.config.resource_limits_for(status_kind),
            input_sequence: active.map_or(0, |worker| worker.input_sequence),
            input_ack_sequence: active.map_or(0, |worker| worker.input_ack_sequence),
            input_pending: active.is_some_and(|worker| worker.pending_input.is_some()),
            input_coalesced: active.map_or(0, |worker| worker.input_coalesced),
            input_protocol_errors: active.map_or(0, |worker| worker.input_protocol_errors),
            pointer_inside: active.is_some_and(|worker| worker.pointer_inside),
            pointer_x: active.map_or(0, |worker| worker.pointer_x),
            pointer_y: active.map_or(0, |worker| worker.pointer_y),
            audio_pending: active.is_some_and(|worker| worker.pending_audio.is_some()),
            audio_coalesced: active.map_or(0, |worker| worker.audio_coalesced),
            audio_grant_dropped: self.audio_grant_dropped,
            media_pending: active.is_some_and(|worker| worker.pending_media.is_some()),
            media_coalesced: active.map_or(0, |worker| worker.media_coalesced),
            stderr_tail: active.map_or_else(Vec::new, |worker| worker.stderr_ring.tail.clone()),
            stderr_dropped_bytes: active.map_or(0, |worker| worker.stderr_ring.dropped_bytes),
            capability_limitations: active
                .map(|worker| worker.spec.capability_limitations.clone())
                .or_else(|| requested.map(|spec| spec.capability_limitations.clone()))
                .unwrap_or_default(),
        }
    }
}

enum WorkerObservation {
    Progress(FrameSnapshot),
    Failure(FailureKind, String),
}

fn inspect_worker(
    worker: &mut ActiveWorker,
    config: &SupervisorConfig,
) -> Option<WorkerObservation> {
    match worker.child.try_wait() {
        Ok(Some(status)) => {
            // Drain the full diagnostic pipe once the child is gone so the
            // final stderr lands in the ring before the caller reaps it.
            drain_stderr(worker, usize::MAX);
            // Any worker exiting 71 declares a resource limit (memory
            // denied): the test renderer's memory-pressure fault and the
            // scene worker's QuickJS heap-cap hit both use it. The mapping
            // is unconditional — 71 is a resource-limit contract, not a
            // test-fault signal (the fault-flag gating was test-era).
            if status.code() == Some(71) {
                return Some(WorkerObservation::Failure(
                    FailureKind::ResourceLimit,
                    "memory_allocation_denied".to_string(),
                ));
            }
            if status.signal() == Some(libc::SIGXFSZ) {
                return Some(WorkerObservation::Failure(
                    FailureKind::ResourceLimit,
                    "file_size_limit_exceeded".to_string(),
                ));
            }
            let detail = if let Some(code) = status.code() {
                format!("exit_code_{code}")
            } else if let Some(signal) = status.signal() {
                format!("signal_{signal}")
            } else {
                "unknown_exit".to_string()
            };
            // Fold the drained ring tail into the detail so crash diagnostics
            // survive the worker drop and reach status()/quarantine records.
            let detail = append_stderr_tail(&detail, worker);
            // 73 (backend_reject) and 74 (no_drawable_content) are the
            // renderer contract's refusal codes; the candidate/active
            // handlers decide whether they strike (B4).
            let kind = match status.code() {
                Some(EXIT_BACKEND_REJECT | EXIT_NO_DRAWABLE_CONTENT) => FailureKind::Refused,
                _ => FailureKind::ProcessExit,
            };
            return Some(WorkerObservation::Failure(kind, detail));
        }
        Ok(None) => {}
        Err(error) => {
            drain_stderr(worker, STDERR_READ_BYTES_PER_TICK);
            return Some(WorkerObservation::Failure(
                FailureKind::ProcessExit,
                append_stderr_tail(&format!("wait_error:{error}"), worker),
            ));
        }
    }

    drain_stderr(worker, STDERR_READ_BYTES_PER_TICK);
    // macOS (MP-9): Darwin refuses RLIMIT_AS, so the address-space budget is
    // enforced here as a resident-set watchdog on every tick; a worker over
    // budget is a ResourceLimit failure (killed, struck, restarted like any
    // other). Linux keeps the kernel rlimit and never reaches this branch.
    if !kwe_platform::address_space_limit_enforced() {
        let mib = 1024_u64 * 1024;
        let budget = config
            .resource_limits_for(worker.spec.kind)
            .address_space_mib
            .saturating_mul(mib);
        if let Ok(pid) = i32::try_from(worker.child.id())
            && let Some(resident) = kwe_platform::resident_set_bytes(pid)
            && resident > budget
        {
            return Some(WorkerObservation::Failure(
                FailureKind::ResourceLimit,
                format!(
                    "resident_set_exceeded:{}MiB>{}MiB",
                    resident / mib,
                    budget / mib
                ),
            ));
        }
    }
    let now = Instant::now();
    if worker.reader.is_none() {
        match SharedFrameReader::open(&worker.frame_path) {
            Ok(reader) => worker.reader = Some(reader),
            Err(ProtocolError::Io(error)) if error.kind() == io::ErrorKind::NotFound => {}
            Err(_) => {}
        }
    }
    if let Some(reader) = &worker.reader {
        match reader.snapshot() {
            Ok(snapshot) if snapshot.sequence > worker.sequence => {
                worker.sequence = snapshot.sequence;
                worker.last_progress = now;
                return Some(WorkerObservation::Progress(snapshot));
            }
            Ok(_) | Err(ProtocolError::Busy) => {}
            Err(error) if worker.sequence == 0 => {
                if now.duration_since(worker.started)
                    >= config.startup_timeout_for(worker.spec.kind)
                {
                    return Some(WorkerObservation::Failure(
                        FailureKind::StartupTimeout,
                        error.to_string(),
                    ));
                }
            }
            Err(error) => {
                return Some(WorkerObservation::Failure(
                    FailureKind::InvalidFrame,
                    error.to_string(),
                ));
            }
        }
    }
    if worker.sequence == 0
        && now.duration_since(worker.started) >= config.startup_timeout_for(worker.spec.kind)
    {
        return Some(WorkerObservation::Failure(
            FailureKind::StartupTimeout,
            "no_valid_frame".into(),
        ));
    }
    if worker.sequence > 0 && now.duration_since(worker.last_progress) >= config.frame_timeout {
        return Some(WorkerObservation::Failure(
            FailureKind::FrameTimeout,
            "frame_sequence_stalled".into(),
        ));
    }
    None
}

enum PipeWrite {
    Written,
    WouldBlock,
}

fn try_write_input(input: &ChildStdin, bytes: &[u8]) -> io::Result<PipeWrite> {
    if bytes.is_empty() || bytes.len() > MAX_INPUT_MESSAGE_BYTES {
        return Err(io::Error::new(
            io::ErrorKind::InvalidInput,
            "renderer input message is outside its safety bound",
        ));
    }
    // Linux guarantees writes no larger than PIPE_BUF are atomic. Protocol
    // messages are capped far below that value, so a nonblocking write either
    // commits the complete message or reports backpressure.
    let written = unsafe {
        libc::write(
            input.as_raw_fd(),
            bytes.as_ptr().cast::<libc::c_void>(),
            bytes.len(),
        )
    };
    if written == bytes.len() as isize {
        return Ok(PipeWrite::Written);
    }
    if written < 0 {
        let error = io::Error::last_os_error();
        if error.kind() == io::ErrorKind::WouldBlock {
            return Ok(PipeWrite::WouldBlock);
        }
        return Err(error);
    }
    Err(io::Error::new(
        io::ErrorKind::WriteZero,
        "renderer input pipe accepted a partial atomic message",
    ))
}

/// Raise the ack acceptance ceiling without ever lowering it. Audio and
/// media wire sequences carry the display generation (a promotion counter
/// that routinely sits below the pointer sequence), so a plain assignment
/// would DECREASE the ceiling and reject in-flight pointer acks as protocol
/// errors. The pointer path increments the sequence per message; this helper
/// only ever lifts it.
fn raise_ack_ceiling(worker: &mut ActiveWorker, sequence: u64) {
    worker.input_sequence = worker.input_sequence.max(sequence);
}

fn drain_input_acks(worker: &mut ActiveWorker) {
    let mut total = 0_usize;
    while total < MAX_ACK_READ_BYTES_PER_TICK {
        let mut chunk = [0_u8; 512];
        let read = unsafe {
            libc::read(
                worker.input_ack.as_raw_fd(),
                chunk.as_mut_ptr().cast::<libc::c_void>(),
                chunk.len(),
            )
        };
        if read == 0 {
            break;
        }
        if read < 0 {
            let error = io::Error::last_os_error();
            if error.kind() != io::ErrorKind::WouldBlock {
                worker.input_protocol_errors = worker.input_protocol_errors.saturating_add(1);
            }
            break;
        }
        let read = read as usize;
        total += read;
        worker.input_ack_buffer.extend_from_slice(&chunk[..read]);
        while let Some(newline) = worker
            .input_ack_buffer
            .iter()
            .position(|byte| *byte == b'\n')
        {
            let line: Vec<u8> = worker.input_ack_buffer.drain(..=newline).collect();
            match decode_ack_line(&line) {
                // Last-wins acceptance: media/audio messages legitimately
                // repeat a sequence (the wire sequence is the display
                // generation), so strict monotonicity would reject valid
                // acks. The ceiling check still rejects stale echoes.
                Ok(ack) if ack.sequence <= worker.input_sequence => {
                    worker.input_ack_sequence = ack.sequence;
                }
                Ok(_) | Err(_) => {
                    worker.input_protocol_errors = worker.input_protocol_errors.saturating_add(1);
                }
            }
        }
        if worker.input_ack_buffer.len() > MAX_ACK_BUFFER_BYTES {
            worker.input_ack_buffer.clear();
            worker.input_protocol_errors = worker.input_protocol_errors.saturating_add(1);
        }
    }
}

/// Latest-wins queueing onto the active worker's control pipe: a newer
/// message replaces an unsent one instead of forming an unbounded queue.
fn queue_control_message(
    input: &ChildStdin,
    bytes: Vec<u8>,
    pending: &mut Option<Vec<u8>>,
    coalesced: &mut u64,
) -> Result<()> {
    if pending.take().is_some() {
        *coalesced = coalesced.saturating_add(1);
    }
    match try_write_input(input, &bytes)? {
        PipeWrite::Written => {}
        PipeWrite::WouldBlock => *pending = Some(bytes),
    }
    Ok(())
}

/// One bounded write attempt per tick for a previously backed-up control
/// message. `event` names the diagnostics stream for error logging.
fn flush_pending(
    input: &ChildStdin,
    pending: &mut Option<Vec<u8>>,
    protocol_errors: &mut u64,
    event: &str,
) {
    let Some(bytes) = pending.take() else {
        return;
    };
    match try_write_input(input, &bytes) {
        Ok(PipeWrite::Written) => {}
        Ok(PipeWrite::WouldBlock) => *pending = Some(bytes),
        Err(error) => {
            *protocol_errors = protocol_errors.saturating_add(1);
            eprintln!("event={event} detail={error}");
        }
    }
}

/// Per-kind environment allowlist. Every worker gets its own private HOME
/// (created per launch under the daemon runtime dir — web renderers hold a
/// profile lock in $HOME, so a shared HOME would make concurrent workers
/// contend during canary handoff) and a fixed PATH; Web additionally
/// inherits the daemon's XDG_RUNTIME_DIR for the future network/permission
/// grant lanes. Note that the path is a host path: /run is not bound inside
/// the bwrap root, so the directory does not exist inside the sandbox and
/// Chromium treats the variable as unset, falling back to its tmpfs
/// profile — a harmless no-op today, kept so the value rides along once
/// grants bind it in (M2c). It is deliberately not granted to the
/// video/scene/test kinds.
pub(crate) fn env_allowlist(kind: RendererKind, home: &Path) -> Vec<(String, String)> {
    env_allowlist_with_runtime(kind, home, std::env::var_os("XDG_RUNTIME_DIR"))
}

/// Every renderer HOME is a daemon-created 0700 directory. Remove it after
/// reaping the child so scene VideoLayer staging is cleaned even when the
/// worker exits through process::exit before Rust destructors can run.
///
/// Reused by `inspect::run_inspection` for the inspector's own per-launch
/// HOME dir — same daemon-created-0700-dir, same must-remove-on-every-exit
/// contract.
pub(crate) fn cleanup_renderer_home(home: &Path) {
    match fs::symlink_metadata(home) {
        Ok(meta) if meta.is_dir() && !meta.file_type().is_symlink() => {
            if let Err(error) = fs::remove_dir_all(home) {
                eprintln!(
                    "event=renderer.home_cleanup_error path={} detail={error}",
                    home.display()
                );
            }
        }
        Ok(_) => eprintln!(
            "event=renderer.home_cleanup_refused path={} detail=not-plain-directory",
            home.display()
        ),
        Err(error) if error.kind() == io::ErrorKind::NotFound => {}
        Err(error) => eprintln!(
            "event=renderer.home_cleanup_error path={} detail={error}",
            home.display()
        ),
    }
}

/// The workers' PATH. Linux: system paths only. macOS: Homebrew's bin
/// directories first — `ffmpeg` (audio capture) and other helpers live
/// there and nowhere on the system default path.
const WORKER_PATH: &str = if cfg!(target_os = "macos") {
    "/opt/homebrew/bin:/usr/local/bin:/usr/bin:/usr/sbin:/bin"
} else {
    "/usr/bin:/usr/sbin:/bin"
};

fn env_allowlist_with_runtime(
    kind: RendererKind,
    home: &Path,
    runtime_dir: Option<OsString>,
) -> Vec<(String, String)> {
    let mut entries = vec![
        ("HOME".to_string(), home.to_string_lossy().into_owned()),
        ("PATH".to_string(), WORKER_PATH.to_string()),
    ];
    if kind == RendererKind::Web
        && let Some(runtime) = runtime_dir
    {
        entries.push((
            "XDG_RUNTIME_DIR".to_string(),
            runtime.to_string_lossy().into_owned(),
        ));
    }
    macos_env_passthrough(&mut entries);
    entries
}

/// macOS (MP-3): the variables a worker needs to find Homebrew dylibs
/// (`libvulkan`, `libmpv`) and the MoltenVK ICD, copied from the daemon's
/// own environment when present. The LaunchAgent sets them
/// (packaging/macos/org.kde.kwe.daemon.plist.in). `TMPDIR` is the per-user
/// secure temp dir every macOS process expects. No-op on Linux.
#[cfg(target_os = "macos")]
fn macos_env_passthrough(entries: &mut Vec<(String, String)>) {
    for name in [
        "DYLD_FALLBACK_LIBRARY_PATH",
        "VK_ICD_FILENAMES",
        "VK_DRIVER_FILES",
        "TMPDIR",
        // Web renderer (MP-5b): browser binary override and the
        // sandbox-exec kill switch.
        "KWE_CHROMIUM",
        "KWE_WEB_SANDBOX",
        // Audio worker (MP-6): AVFoundation device and ffmpeg override.
        "KWE_AUDIO_DEVICE",
        "KWE_FFMPEG",
    ] {
        if let Some(value) = std::env::var_os(name) {
            entries.push((name.to_string(), value.to_string_lossy().into_owned()));
        }
    }
}

#[cfg(not(target_os = "macos"))]
fn macos_env_passthrough(_entries: &mut [(String, String)]) {}

/// Bounded ring of worker stderr diagnostics, newest last. Oldest lines are
/// evicted (their bytes counted as dropped) whenever the 64-line or 16 KiB
/// budget binds; a single unterminated line that passes the byte budget is
/// dropped whole. Contents are advisory only and never parsed as commands.
/// Note `tail_bytes` counts post-newline-strip, lossy-conversion bytes, so it
/// is a diagnostics counter, not a wire-accurate mirror of the pipe: the
/// memory bound it enforces is unaffected by that accounting.
#[derive(Debug, Default)]
struct StderrRing {
    tail: Vec<String>,
    tail_bytes: usize,
    dropped_bytes: u64,
    pending: Vec<u8>,
}

impl StderrRing {
    fn push_bytes(&mut self, bytes: &[u8]) {
        self.pending.extend_from_slice(bytes);
        while let Some(newline) = self.pending.iter().position(|byte| *byte == b'\n') {
            let line: Vec<u8> = self.pending.drain(..=newline).collect();
            self.push_line(line);
        }
        self.enforce_budget();
    }

    fn push_line(&mut self, mut line: Vec<u8>) {
        line.pop(); // strip the trailing newline; the entry keeps its bytes
        let text = String::from_utf8_lossy(&line).into_owned();
        self.tail_bytes += text.len();
        self.tail.push(text);
        self.enforce_budget();
    }

    fn enforce_budget(&mut self) {
        while self.tail.len() > STDERR_RING_LINES
            || self.tail_bytes.saturating_add(self.pending.len()) > STDERR_RING_BYTES
        {
            if self.tail.is_empty() {
                // The unterminated line alone exceeds the whole budget.
                self.dropped_bytes = self.dropped_bytes.saturating_add(self.pending.len() as u64);
                self.pending.clear();
                break;
            }
            let evicted = self.tail.remove(0);
            self.tail_bytes -= evicted.len();
            self.dropped_bytes = self.dropped_bytes.saturating_add(evicted.len() as u64);
        }
    }
}

/// Fold the drained stderr ring tail into a failure detail: bounded to the
/// last 8 lines and the shared 256-char detail budget, newest line last.
/// Called on the exit path, where the ring would otherwise die with the
/// dropped ActiveWorker before status() could ever surface it.
fn append_stderr_tail(detail: &str, worker: &ActiveWorker) -> String {
    if worker.stderr_ring.tail.is_empty() {
        return detail.to_string();
    }
    let tail: Vec<&str> = worker
        .stderr_ring
        .tail
        .iter()
        .rev()
        .take(8)
        .rev()
        .map(String::as_str)
        .collect();
    format!("{detail} stderr=[{}]", truncate_detail(&tail.join(" | ")))
}

/// Nonblocking drain of worker stderr into the bounded ring. `budget` limits
/// bytes consumed per tick; `usize::MAX` drains everything once after exit.
fn drain_stderr(worker: &mut ActiveWorker, budget: usize) {
    let mut total = 0_usize;
    while total < budget {
        let mut chunk = [0_u8; 512];
        let read = unsafe {
            libc::read(
                worker.stderr.as_raw_fd(),
                chunk.as_mut_ptr().cast::<libc::c_void>(),
                chunk.len(),
            )
        };
        if read == 0 {
            break;
        }
        if read < 0 {
            if io::Error::last_os_error().kind() != io::ErrorKind::WouldBlock {
                // Best-effort diagnostics; a failed pipe only stops the tail.
                eprintln!("event=renderer.stderr_read_error");
            }
            break;
        }
        let read = read as usize;
        total += read;
        worker.stderr_ring.push_bytes(&chunk[..read]);
    }
}

pub(crate) fn set_nonblocking(descriptor: libc::c_int) -> io::Result<()> {
    let flags = unsafe { libc::fcntl(descriptor, libc::F_GETFL) };
    if flags < 0 {
        return Err(io::Error::last_os_error());
    }
    if unsafe { libc::fcntl(descriptor, libc::F_SETFL, flags | libc::O_NONBLOCK) } < 0 {
        return Err(io::Error::last_os_error());
    }
    Ok(())
}

/// Identity of the build whose failures the persisted records describe:
/// the daemon's own version and executable (size + mtime) plus every
/// configured renderer binary's size + mtime, in kind order. Package
/// upgrades rewrite these files (new mtime, usually new size); the cargo
/// version alone is not enough because alpha package releases bump
/// `pkgrel` without it. Missing binaries are recorded as such so a later
/// install changes the identity too. Pure metadata — no hashing of
/// multi-megabyte binaries on every start.
pub(crate) fn build_identity(config: &SupervisorConfig) -> String {
    fn stamp(path: &Path) -> String {
        match fs::metadata(path) {
            Ok(metadata) => {
                let mtime = metadata
                    .modified()
                    .ok()
                    .and_then(|time| time.duration_since(std::time::UNIX_EPOCH).ok())
                    .map_or(0, |duration| duration.as_secs());
                format!("{}:{}", metadata.len(), mtime)
            }
            Err(_) => "missing".to_string(),
        }
    }
    let mut parts = vec![format!(
        "daemon={}:{}",
        env!("CARGO_PKG_VERSION"),
        std::env::current_exe()
            .map(|exe| stamp(&exe))
            .unwrap_or_else(|_| "unknown".to_string())
    )];
    for (kind, path) in &config.renderer_paths {
        parts.push(format!("{}={}", kind.as_str(), stamp(path)));
    }
    parts.join(";")
}

pub(crate) fn apply_resource_limits(limits: RendererResourceLimits) -> io::Result<()> {
    let mib = 1024_u64 * 1024;
    // Darwin refuses RLIMIT_AS outright (setrlimit -> EINVAL, measured on
    // the macos-14 runner) and would not enforce it anyway; the resident-set
    // watchdog (plan MP-9) is the macOS substitute. Linux enforces it.
    if kwe_platform::address_space_limit_enforced() {
        set_resource_limit("RLIMIT_AS", kwe_platform::RLIMIT_AS, limits.address_space_mib * mib)?;
    }
    set_resource_limit("RLIMIT_FSIZE", kwe_platform::RLIMIT_FSIZE, limits.file_size_mib * mib)?;
    set_resource_limit("RLIMIT_NOFILE", kwe_platform::RLIMIT_NOFILE, limits.open_files)?;
    set_resource_limit("RLIMIT_NPROC", kwe_platform::RLIMIT_NPROC, limits.processes)?;
    set_resource_limit("RLIMIT_CORE", kwe_platform::RLIMIT_CORE, limits.core_dump_bytes)?;
    Ok(())
}

/// Runs between fork and exec. The error must stay a RAW OS error: std
/// carries a failing pre_exec closure back to the parent as errno only
/// (anything else becomes EINVAL), so the parent's `last_failure_detail`
/// keeps the true cause (EPERM, EINVAL, ...). Which RESOURCE failed is
/// answered by `kwe_platform`'s per-step containment test, not here.
fn set_resource_limit(
    _name: &'static str,
    resource: kwe_platform::RlimitResource,
    value: u64,
) -> io::Result<()> {
    let value = libc::rlim_t::try_from(value)
        .map_err(|_| io::Error::new(io::ErrorKind::InvalidInput, "resource limit overflow"))?;
    let limit = libc::rlimit {
        rlim_cur: value,
        rlim_max: value,
    };
    // SAFETY: `limit` is a valid immutable rlimit structure and `resource` is
    // one of the constants selected by `apply_resource_limits`.
    if unsafe { libc::setrlimit(resource, &limit) } != 0 {
        return Err(io::Error::last_os_error());
    }
    Ok(())
}

fn terminate_and_reap(child: &mut Child, grace: Duration) -> bool {
    if child.try_wait().ok().flatten().is_some() {
        return false;
    }
    let pid = child.id();
    signal_process_group(pid, libc::SIGTERM);
    let deadline = Instant::now() + grace;
    while Instant::now() < deadline {
        if child.try_wait().ok().flatten().is_some() {
            return false;
        }
        thread::sleep(Duration::from_millis(10));
    }
    signal_process_group(pid, libc::SIGKILL);
    let _ = child.kill();
    let _ = child.wait();
    true
}

pub(crate) fn signal_process_group(pid: u32, signal: libc::c_int) {
    if let Ok(pid) = i32::try_from(pid) {
        // SAFETY: the child is placed in a process group whose id equals its
        // pid before exec. A negative pid restricts delivery to that group.
        unsafe {
            libc::kill(-pid, signal);
        }
    }
}

fn encode_ppm(snapshot: &FrameSnapshot) -> Result<Vec<u8>> {
    let expected = snapshot.spec.pixel_bytes();
    if snapshot.pixels.len() != expected {
        bail!("snapshot pixel length changed during fallback encoding");
    }
    let header = format!(
        "P6\n{} {}\n255\n",
        snapshot.spec.width, snapshot.spec.height
    );
    let rgb_bytes = expected
        .checked_div(4)
        .and_then(|pixels| pixels.checked_mul(3))
        .context("fallback image size overflow")?;
    let mut output = Vec::with_capacity(header.len() + rgb_bytes);
    output.extend_from_slice(header.as_bytes());
    for pixel in snapshot.pixels.chunks_exact(4) {
        output.extend_from_slice(&[pixel[2], pixel[1], pixel[0]]);
    }
    Ok(output)
}

pub(crate) fn validate_identity_part(name: &str, value: &str) -> Result<()> {
    if value.is_empty()
        || value.len() > 128
        || !value
            .bytes()
            .all(|byte| byte.is_ascii_alphanumeric() || matches!(byte, b'.' | b'_' | b'-'))
    {
        bail!("{name} must be 1..=128 ASCII letters, digits, '.', '_', or '-'");
    }
    Ok(())
}

fn truncate_detail(detail: &str) -> String {
    detail.chars().take(256).collect()
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::process::{Command, Stdio};

    use kwe_input_protocol::{InputAck, encode_ack_line};

    fn temporary_directory(label: &str) -> PathBuf {
        std::env::temp_dir().join(format!(
            "kwe-supervisor-{label}-{}-{}",
            std::process::id(),
            unix_nanos()
        ))
    }

    #[test]
    fn validates_bounded_identity_and_render_settings() {
        let valid = StartSpec {
            wallpaper_id: "431960-123".into(),
            content_hash: "abc123".into(),
            width: 1920,
            height: 1080,
            fps: 60,
            kind: RendererKind::Test,
            content: None,
            test_fault: None,
            stderr_lines: None,
            scaling: ScalingMode::Aspect,
            capability_limitations: Vec::new(),
        };
        assert!(valid.validate(None).is_ok());
        let mut invalid = valid.clone();
        invalid.wallpaper_id = "../escape".into();
        assert!(invalid.validate(None).is_err());
        invalid = valid.clone();
        invalid.fps = 0;
        assert!(invalid.validate(None).is_err());
        invalid = valid.clone();
        invalid.width = 8192;
        invalid.height = 8192;
        assert!(invalid.validate(None).is_err());
        let mut invalid_scene = StartSpec {
            wallpaper_id: "431960-123".into(),
            content_hash: "abc123".into(),
            width: 1920,
            height: 1080,
            fps: 60,
            kind: RendererKind::Scene,
            content: Some(ContentSpec::Scene {
                path: std::env::temp_dir().join("kwe-missing-scene.json"),
            }),
            test_fault: None,
            stderr_lines: None,
            scaling: ScalingMode::Aspect,
            capability_limitations: Vec::new(),
        };
        assert!(invalid_scene.validate(None).is_err());
        invalid_scene.kind = RendererKind::Test;
        invalid_scene.content = None;
        assert!(invalid_scene.validate(None).is_ok());
    }

    #[test]
    fn rejects_mismatched_kind_content_and_dev_only_stderr_lines() {
        let base = StartSpec {
            wallpaper_id: "431960-123".into(),
            content_hash: "abc123".into(),
            width: 960,
            height: 540,
            fps: 30,
            kind: RendererKind::Test,
            content: None,
            test_fault: None,
            stderr_lines: None,
            scaling: ScalingMode::Aspect,
            capability_limitations: Vec::new(),
        };
        // Test takes no content.
        let mut mismatched = base.clone();
        mismatched.content = Some(ContentSpec::Video {
            path: std::env::temp_dir().join("kwe-any.mp4"),
        });
        assert!(mismatched.validate(None).is_err());
        // Video requires video content.
        mismatched.kind = RendererKind::Video;
        assert!(mismatched.validate(None).is_err());
        // Missing video file fails the static video preflight.
        let missing_video = StartSpec {
            kind: RendererKind::Video,
            content: Some(ContentSpec::Video {
                path: std::env::temp_dir().join("kwe-missing-video.mp4"),
            }),
            ..base.clone()
        };
        assert!(missing_video.validate(None).is_err());
        // A symlinked video path is rejected before it could resolve.
        let root = temporary_directory("video-symlink");
        fs::create_dir_all(&root).unwrap();
        fs::write(root.join("real.mp4"), b"not a real video").unwrap();
        std::os::unix::fs::symlink(root.join("real.mp4"), root.join("link.mp4")).unwrap();
        let symlink_video = StartSpec {
            kind: RendererKind::Video,
            content: Some(ContentSpec::Video {
                path: root.join("link.mp4"),
            }),
            ..base.clone()
        };
        assert!(symlink_video.validate(None).is_err());
        // A disallowed extension is rejected at validation with the
        // preflight reason surfaced in the error.
        let bad_extension = root.join("garbage.bin");
        fs::write(&bad_extension, b"not a real video").unwrap();
        let bad_ext_video = StartSpec {
            kind: RendererKind::Video,
            content: Some(ContentSpec::Video {
                path: bad_extension,
            }),
            ..base.clone()
        };
        let error = format!("{}", bad_ext_video.validate(None).unwrap_err());
        assert!(
            error.contains("video preflight rejected")
                && error.contains("unsupported video extension"),
            "unexpected error: {error}"
        );
        fs::remove_dir_all(root).unwrap();
        // stderr_lines is a test-renderer dev helper.
        let mut dev_only = base.clone();
        dev_only.stderr_lines = Some(10);
        assert!(dev_only.validate(None).is_ok());
        dev_only.kind = RendererKind::Scene;
        assert!(dev_only.validate(None).is_err());
        dev_only.kind = RendererKind::Test;
        dev_only.stderr_lines = Some(0);
        assert!(dev_only.validate(None).is_err());
    }

    #[test]
    fn validates_renderer_resource_policy() {
        let valid = RendererResourceLimits {
            address_space_mib: 4096,
            file_size_mib: 160,
            open_files: 256,
            processes: 1024,
            core_dump_bytes: 0,
        };
        assert!(valid.validate().is_ok());
        assert!(
            RendererResourceLimits {
                address_space_mib: 128,
                ..valid
            }
            .validate()
            .is_err()
        );
        // The web kind's 128 GiB default and the 256 GiB bound top must pass
        // validation; the old 64 GiB cap would reject the web default.
        assert!(
            RendererResourceLimits {
                address_space_mib: 131_072,
                ..valid
            }
            .validate()
            .is_ok()
        );
        assert!(
            RendererResourceLimits {
                address_space_mib: 262_144,
                ..valid
            }
            .validate()
            .is_ok()
        );
        assert!(
            RendererResourceLimits {
                address_space_mib: 262_145,
                ..valid
            }
            .validate()
            .is_err()
        );
        assert!(
            RendererResourceLimits {
                core_dump_bytes: 1,
                ..valid
            }
            .validate()
            .is_err()
        );
    }

    #[test]
    fn persists_bounded_quarantine_state() {
        let directory = temporary_directory("state");
        let (store, mut state) = StateStore::open(directory.clone()).unwrap();
        state.records.insert(
            "wallpaper:hash".into(),
            FailureRecord {
                wallpaper_id: "wallpaper".into(),
                content_hash: "hash".into(),
                failures: 3,
                quarantined: true,
                last_failure: FailureKind::FrameTimeout,
                last_detail: "frame_sequence_stalled".into(),
                updated_unix_seconds: 1,
            },
        );
        store.save(&state).unwrap();
        let loaded = store.load().unwrap();
        assert!(loaded.records["wallpaper:hash"].quarantined);
        assert_eq!(loaded.records["wallpaper:hash"].failures, 3);
        fs::remove_dir_all(directory).unwrap();
    }

    #[test]
    fn encodes_bgra_snapshot_as_portable_rgb_fallback() {
        let snapshot = FrameSnapshot {
            spec: FrameSpec::new(2, 1).unwrap(),
            sequence: 7,
            producer_state: kwe_frame_protocol::ProducerState::Running,
            pixels: vec![10, 20, 30, 255, 40, 50, 60, 255],
        };
        let ppm = encode_ppm(&snapshot).unwrap();
        assert!(ppm.starts_with(b"P6\n2 1\n255\n"));
        assert!(ppm.ends_with(&[30, 20, 10, 60, 50, 40]));
    }

    #[test]
    fn alternates_bounded_last_good_slots() {
        let directory = temporary_directory("fallback-slots");
        let (store, _) = StateStore::open(directory.clone()).unwrap();
        let snapshot = FrameSnapshot {
            spec: FrameSpec::new(1, 1).unwrap(),
            sequence: 1,
            producer_state: kwe_frame_protocol::ProducerState::Running,
            pixels: vec![1, 2, 3, 255],
        };
        let first = store.save_last_good(&snapshot, None).unwrap();
        let second = store
            .save_last_good(&snapshot, first.file_name().and_then(|name| name.to_str()))
            .unwrap();
        assert_eq!(first.file_name().unwrap(), "last-good-a.ppm");
        assert_eq!(second.file_name().unwrap(), "last-good-b.ppm");
        assert_eq!(fs::read_dir(&directory).unwrap().count(), 2);
        fs::remove_dir_all(directory).unwrap();
    }

    #[test]
    fn refuses_symlinked_state_directory() {
        use std::os::unix::fs::symlink;

        let root = temporary_directory("symlink");
        let real = root.join("real");
        let link = root.join("link");
        fs::create_dir_all(&real).unwrap();
        symlink(&real, &link).unwrap();
        assert!(ensure_private_dir(&link).is_err());
        fs::remove_dir_all(root).unwrap();
    }

    fn validated_config(root: &Path) -> SupervisorConfig {
        let limits = RendererResourceLimits {
            address_space_mib: 4096,
            file_size_mib: 160,
            open_files: 256,
            processes: 1024,
            core_dump_bytes: 0,
        };
        SupervisorConfig {
            renderer_paths: BTreeMap::from([(
                RendererKind::Test,
                std::env::current_exe().unwrap(),
            )]),
            runtime_dir: root.join("runtime"),
            state_dir: root.join("state"),
            startup_timeout_ms_by_kind: BTreeMap::from([
                (RendererKind::Test, 3000),
                (RendererKind::Video, 6000),
                (RendererKind::Web, 10_000),
                (RendererKind::Scene, 3000),
            ]),
            frame_timeout: Duration::from_secs(2),
            stop_grace: Duration::from_millis(500),
            restart_delay: Duration::from_millis(250),
            canary_duration: Duration::from_secs(1),
            handoff_timeout: Duration::from_secs(5),
            max_failures: 3,
            web_heartbeat_ms: 5000,
            web_heartbeat_max_failures: 3,
            resource_limits_by_kind: BTreeMap::from([
                (RendererKind::Test, limits),
                (RendererKind::Video, limits),
                (
                    RendererKind::Web,
                    RendererResourceLimits {
                        address_space_mib: 16_384,
                        open_files: 1024,
                        ..limits
                    },
                ),
                (RendererKind::Scene, limits),
            ]),
            scene_assets_dir: None,
            shader_helper_path: None,
        }
        .validate()
        .unwrap()
    }

    #[test]
    fn env_allowlist_keeps_home_and_path_and_grants_runtime_only_to_web() {
        let home = Path::new("/run/kwe/home-7");
        let expected_base = vec![
            ("HOME".to_string(), home.to_string_lossy().into_owned()),
            ("PATH".to_string(), WORKER_PATH.to_string()),
        ];
        for kind in [RendererKind::Test, RendererKind::Video, RendererKind::Scene] {
            let entries = env_allowlist_with_runtime(kind, home, Some("/run/user/1000".into()));
            // HOME and PATH lead; macOS appends its dyld/Vulkan passthrough
            // after them, which is why this is a prefix comparison.
            assert_eq!(&entries[..2], &expected_base[..]);
            assert!(
                entries.iter().all(|(name, _)| name != "XDG_RUNTIME_DIR"),
                "kind {} must not inherit XDG_RUNTIME_DIR",
                kind.as_str()
            );
        }
        let web_without = env_allowlist_with_runtime(RendererKind::Web, home, None);
        assert_eq!(&web_without[..2], &expected_base[..]);
        assert!(web_without.iter().all(|(name, _)| name != "XDG_RUNTIME_DIR"));
        let web =
            env_allowlist_with_runtime(RendererKind::Web, home, Some("/run/user/1000".into()));
        assert_eq!(&web[..2], &expected_base[..]);
        assert_eq!(
            web.iter().filter(|(name, _)| name == "XDG_RUNTIME_DIR").count(),
            1
        );
        assert!(web.contains(&("XDG_RUNTIME_DIR".to_string(), "/run/user/1000".to_string())));
    }

    /// macOS: the resident-set watchdog stands in for RLIMIT_AS. A renderer
    /// that grows past its address-space budget is observed as a
    /// ResourceLimit failure within a few ticks.
    #[cfg(target_os = "macos")]
    #[test]
    fn resident_set_over_budget_is_a_resource_limit_failure() {
        use std::os::unix::fs::PermissionsExt;

        let root = temporary_directory("rss-watchdog");
        fs::create_dir_all(&root).unwrap();
        let script = root.join("hog-renderer");
        fs::write(
            &script,
            "#!/usr/bin/env python3\nimport time\nblock = bytearray(192 * 1024 * 1024)\nfor i in range(0, len(block), 4096):\n    block[i] = 1\ntime.sleep(30)\n",
        )
        .unwrap();
        fs::set_permissions(&script, fs::Permissions::from_mode(0o755)).unwrap();
        let mut config = validated_config(&root);
        config.renderer_paths = BTreeMap::from([(RendererKind::Test, script.clone())]);
        for limits in config.resource_limits_by_kind.values_mut() {
            limits.address_space_mib = 64;
        }
        let config = config.validate().unwrap();
        let (store, state) = StateStore::open(root.join("state")).unwrap();
        let mut runtime = SupervisorRuntime::new(
            config,
            store,
            state,
            GrantStore::open(&root.join("state")).unwrap(),
        );
        let spec = StartSpec {
            wallpaper_id: "431960-rss".into(),
            content_hash: "rss".into(),
            width: 160,
            height: 90,
            fps: 30,
            kind: RendererKind::Test,
            content: None,
            test_fault: None,
            stderr_lines: None,
            scaling: ScalingMode::Aspect,
            capability_limitations: Vec::new(),
        };
        let mut worker = runtime.spawn_worker(spec).unwrap();
        let mut observation = None;
        for _ in 0..600 {
            if let Some(found) = inspect_worker(&mut worker, &runtime.config) {
                observation = Some(found);
                break;
            }
            thread::sleep(Duration::from_millis(10));
        }
        let _ = worker.child.kill();
        let _ = worker.child.wait();
        match observation {
            Some(WorkerObservation::Failure(FailureKind::ResourceLimit, detail)) => {
                assert!(detail.contains("resident_set_exceeded"), "{detail}");
            }
            other => panic!("expected a ResourceLimit observation, got {other:?}"),
        }
        fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn spawn_creates_a_private_home_per_launch_and_surfaces_exit_stderr_in_failure() {
        use std::os::unix::fs::PermissionsExt;

        let root = temporary_directory("stderr-exit");
        fs::create_dir_all(&root).unwrap();
        // A renderer that writes diagnostics and dies: the failure detail
        // must carry the stderr lines that were drained at exit.
        let script = root.join("stderr-renderer");
        fs::write(
            &script,
            "#!/bin/sh\necho diagnostic-line-1 >&2\necho diagnostic-line-2 >&2\nexit 3\n",
        )
        .unwrap();
        fs::set_permissions(&script, fs::Permissions::from_mode(0o755)).unwrap();
        let mut config = validated_config(&root);
        config.renderer_paths = BTreeMap::from([(RendererKind::Test, script.clone())]);
        let config = config.validate().unwrap();
        let (store, state) = StateStore::open(root.join("state")).unwrap();
        let mut runtime = SupervisorRuntime::new(
            config,
            store,
            state,
            GrantStore::open(&root.join("state")).unwrap(),
        );
        let spec = StartSpec {
            wallpaper_id: "431960-123".into(),
            content_hash: "abc123".into(),
            width: 160,
            height: 90,
            fps: 30,
            kind: RendererKind::Test,
            content: None,
            test_fault: None,
            stderr_lines: None,
            scaling: ScalingMode::Aspect,
            capability_limitations: Vec::new(),
        };
        let mut worker = runtime.spawn_worker(spec).unwrap();
        // Each launch gets its own 0700 HOME under the daemon runtime dir.
        let home = root.join("runtime").join("home-1");
        let mode = fs::metadata(&home).unwrap().permissions().mode() & 0o777;
        assert_eq!(
            mode,
            0o700,
            "renderer home must be private: {}",
            home.display()
        );
        let mut observation = None;
        for _ in 0..200 {
            if let Some(found) = inspect_worker(&mut worker, &runtime.config) {
                observation = Some(found);
                break;
            }
            thread::sleep(Duration::from_millis(10));
        }
        let (kind, detail) = match observation {
            Some(WorkerObservation::Failure(kind, detail)) => (kind, detail),
            _ => panic!("expected an exit failure observation"),
        };
        assert_eq!(kind, FailureKind::ProcessExit);
        assert!(
            detail.contains("exit_code_3"),
            "unexpected detail: {detail}"
        );
        assert!(
            detail.contains("diagnostic-line-1") && detail.contains("diagnostic-line-2"),
            "exit stderr must be folded into the failure detail: {detail}"
        );
        fs::remove_dir_all(root).unwrap();
    }

    /// SR-3b: `--shader-helper <path>` is passed for `RendererKind::Scene`
    /// only — a Video-kind spawn (or any other kind) never gets it, even
    /// with `shader_helper_path` configured. Proven by spawning a REAL
    /// (fake) renderer that dumps its own argv to a file, the same
    /// technique the other `spawn_worker` tests in this module use for
    /// exit/stderr behavior.
    #[test]
    fn shader_helper_flag_is_passed_for_scene_kind_only() {
        use std::os::unix::fs::PermissionsExt;

        // Each kind gets its own isolated root/runtime -- spawning two
        // kinds against the same runtime would engage canary/handoff
        // machinery unrelated to this test's own question (which argv a
        // fresh spawn gets), so this stays a single spawn per runtime,
        // matching every other `spawn_worker` test in this module.
        fn run_one(root: &Path, kind: RendererKind, shader_helper_path: Option<PathBuf>) -> String {
            fs::create_dir_all(root).unwrap();
            let argv_dump = root.join("argv.txt");
            let script = root.join("argv-dump-renderer");
            fs::write(
                &script,
                format!("#!/bin/sh\necho \"$@\" > {}\nexit 3\n", argv_dump.display()),
            )
            .unwrap();
            fs::set_permissions(&script, fs::Permissions::from_mode(0o755)).unwrap();

            let mut config = validated_config(root);
            config.renderer_paths = BTreeMap::from([(kind, script)]);
            config.shader_helper_path = shader_helper_path;
            let config = config.validate().unwrap();
            let (store, state) = StateStore::open(root.join("state")).unwrap();
            let mut runtime = SupervisorRuntime::new(
                config,
                store,
                state,
                GrantStore::open(&root.join("state")).unwrap(),
            );
            let spec = StartSpec {
                wallpaper_id: "431960-123".into(),
                content_hash: "abc123".into(),
                width: 160,
                height: 90,
                fps: 30,
                kind,
                content: None,
                test_fault: None,
                stderr_lines: None,
                scaling: ScalingMode::Aspect,
                capability_limitations: Vec::new(),
            };
            let _worker = runtime.spawn_worker(spec).unwrap();
            for _ in 0..200 {
                if let Ok(contents) = fs::read_to_string(&argv_dump) {
                    return contents;
                }
                thread::sleep(Duration::from_millis(10));
            }
            panic!("timed out waiting for {} to appear", argv_dump.display());
        }

        let scene_root = temporary_directory("shader-helper-flag-scene");
        let helper_path = scene_root.join("kwe-shader-compiler");
        fs::create_dir_all(&scene_root).unwrap();
        fs::write(&helper_path, b"x").unwrap();
        let scene_argv = run_one(&scene_root, RendererKind::Scene, Some(helper_path.clone()));
        assert!(
            scene_argv.contains(&format!("--shader-helper {}", helper_path.display())),
            "scene kind must get --shader-helper: {scene_argv}"
        );
        fs::remove_dir_all(&scene_root).unwrap();

        let video_root = temporary_directory("shader-helper-flag-video");
        let video_argv = run_one(&video_root, RendererKind::Video, Some(helper_path));
        assert!(
            !video_argv.contains("--shader-helper"),
            "video kind must never get --shader-helper even when configured: {video_argv}"
        );
        fs::remove_dir_all(&video_root).unwrap();
    }

    #[test]
    fn identity_is_kind_qualified_and_migrates_legacy_records() {
        let root = temporary_directory("identity");
        fs::create_dir_all(&root).unwrap();
        let (store, state) = StateStore::open(root.join("state")).unwrap();
        let mut runtime = SupervisorRuntime::new(
            validated_config(&root),
            store,
            state,
            GrantStore::open(&root.join("state")).unwrap(),
        );
        let base = StartSpec {
            wallpaper_id: "431960-123".into(),
            content_hash: "abc123".into(),
            width: 960,
            height: 540,
            fps: 30,
            kind: RendererKind::Test,
            content: None,
            test_fault: None,
            stderr_lines: None,
            scaling: ScalingMode::Aspect,
            capability_limitations: Vec::new(),
        };
        let video = StartSpec {
            kind: RendererKind::Video,
            content: Some(ContentSpec::Video {
                path: std::env::temp_dir().join("kwe-any.mp4"),
            }),
            ..base.clone()
        };
        assert_eq!(base.identity(), "431960-123:abc123:test");
        assert_eq!(video.identity(), "431960-123:abc123:video");
        assert_eq!(base.legacy_identity(), "431960-123:abc123");
        assert_ne!(base.identity(), video.identity());
        // A pre-M1a id:hash record (as persisted in old supervisor-v1.json
        // files) migrates onto the kind-qualified key, carrying its history.
        runtime.persisted.records.insert(
            base.legacy_identity(),
            FailureRecord {
                wallpaper_id: "431960-123".into(),
                content_hash: "abc123".into(),
                failures: 2,
                quarantined: false,
                last_failure: FailureKind::ProcessExit,
                last_detail: "legacy".into(),
                updated_unix_seconds: 1,
            },
        );
        let quarantined = runtime.record_failure(FailureKind::ProcessExit, "boom", &base);
        assert!(quarantined, "inherited failures must still quarantine");
        assert_eq!(runtime.persisted.records.len(), 1);
        let record = runtime
            .persisted
            .records
            .get(&base.identity())
            .expect("record must live under the kind-qualified key");
        assert_eq!(record.failures, 3);
        assert!(
            !runtime
                .persisted
                .records
                .contains_key(&base.legacy_identity())
        );
        fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn into_validated_canonicalizes_the_video_content_path() {
        let root = temporary_directory("video-canonical");
        fs::create_dir_all(&root).unwrap();
        let real = root.join("real.mp4");
        fs::write(&real, b"not a real video").unwrap();
        let spec = StartSpec {
            wallpaper_id: "431960-123".into(),
            content_hash: "abc123".into(),
            width: 960,
            height: 540,
            fps: 30,
            kind: RendererKind::Video,
            content: Some(ContentSpec::Video { path: real.clone() }),
            test_fault: None,
            stderr_lines: None,
            scaling: ScalingMode::Aspect,
            capability_limitations: Vec::new(),
        };
        let validated = spec.into_validated(None).unwrap();
        let path = match validated.content.expect("video content kept") {
            ContentSpec::Video { path } => path,
            _ => panic!("expected video content"),
        };
        assert_eq!(path, fs::canonicalize(&real).unwrap());
        fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn stderr_ring_evicts_oldest_lines_and_counts_dropped_bytes() {
        let mut ring = StderrRing::default();
        for index in 0..80 {
            ring.push_bytes(format!("line-{index}\n").as_bytes());
        }
        assert_eq!(ring.tail.len(), STDERR_RING_LINES);
        assert_eq!(ring.tail.first().unwrap(), "line-16");
        assert_eq!(ring.tail.last().unwrap(), "line-79");
        // Evicted lines 0..10 are 6 bytes, lines 10..16 are 7 bytes.
        assert_eq!(ring.dropped_bytes, 10 * 6 + 6 * 7);
    }

    #[test]
    fn stderr_ring_binds_on_bytes_before_lines() {
        let mut ring = StderrRing::default();
        for _ in 0..8 {
            ring.push_bytes(&vec![b'a'; 3000]);
            ring.push_bytes(b"\n");
        }
        // 6 * 3000 would exceed 16 KiB, so the oldest line is evicted each
        // time until 5 lines (15000 bytes) remain.
        assert_eq!(ring.tail.len(), 5);
        assert_eq!(ring.dropped_bytes, 3 * 3000);
    }

    #[test]
    fn stderr_ring_joins_lines_split_across_reads() {
        let mut ring = StderrRing::default();
        ring.push_bytes(b"first-half");
        assert!(ring.tail.is_empty());
        ring.push_bytes(b"-joined\nsecond\n");
        assert_eq!(ring.tail, vec!["first-half-joined", "second"]);
        assert_eq!(ring.dropped_bytes, 0);
    }

    #[test]
    fn stderr_ring_drops_an_unterminated_line_that_passes_the_budget() {
        let mut ring = StderrRing::default();
        ring.push_bytes(&vec![b'x'; 10 * 1024]);
        assert_eq!(ring.pending.len(), 10 * 1024);
        ring.push_bytes(&vec![b'y'; 10 * 1024]);
        assert!(ring.tail.is_empty());
        assert!(ring.pending.is_empty());
        assert_eq!(ring.dropped_bytes, 20 * 1024);
        ring.push_bytes(b"tail\n");
        assert_eq!(ring.tail, vec!["tail"]);
    }

    #[test]
    fn spawn_resolves_the_kind_specific_binary_and_fails_closed_when_missing() {
        let root = temporary_directory("kinds");
        fs::create_dir_all(&root).unwrap();
        let test_bin = root.join("test-renderer");
        let video_bin = root.join("video-renderer");
        fs::write(&test_bin, b"x").unwrap();
        fs::write(&video_bin, b"x").unwrap();
        let mut config = validated_config(&root);
        config.renderer_paths = BTreeMap::from([
            (RendererKind::Test, test_bin.clone()),
            (RendererKind::Video, video_bin.clone()),
        ]);
        let config = config.validate().unwrap();
        assert_eq!(
            config.renderer_path_for(RendererKind::Test).unwrap(),
            fs::canonicalize(&test_bin).unwrap()
        );
        assert_eq!(
            config.renderer_path_for(RendererKind::Video).unwrap(),
            fs::canonicalize(&video_bin).unwrap()
        );
        let error = config.renderer_path_for(RendererKind::Web).unwrap_err();
        assert!(
            format!("{error}").contains("no renderer binary configured for kind web"),
            "unexpected error: {error}"
        );
        let (store, state) = StateStore::open(root.join("state")).unwrap();
        let mut runtime = SupervisorRuntime::new(
            config,
            store,
            state,
            GrantStore::open(&root.join("state")).unwrap(),
        );
        let spec = StartSpec {
            wallpaper_id: "431960-123".into(),
            content_hash: "abc123".into(),
            width: 960,
            height: 540,
            fps: 30,
            kind: RendererKind::Web,
            content: Some(ContentSpec::Web { root: root.clone() }),
            test_fault: None,
            stderr_lines: None,
            scaling: ScalingMode::Aspect,
            capability_limitations: Vec::new(),
        };
        let error = runtime
            .spawn_worker(spec)
            .err()
            .expect("spawning a web worker without a configured binary must fail");
        assert!(
            format!("{error}").contains("no renderer binary configured for kind web"),
            "unexpected error: {error}"
        );
        fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn queued_audio_commands_coalesce_and_only_the_latest_reaches_the_worker() {
        use std::os::fd::OwnedFd;
        use std::os::unix::io::FromRawFd;

        let mut fds = [0_i32; 2];
        // SAFETY: fds is a valid writable pair buffer for a fresh pipe.
        assert_eq!(unsafe { libc::pipe(fds.as_mut_ptr()) }, 0);
        set_nonblocking(fds[1]).unwrap();
        let input = ChildStdin::from(unsafe { OwnedFd::from_raw_fd(fds[1]) });
        let _read_end = unsafe { fs::File::from_raw_fd(fds[0]) };
        // Fill the pipe so every control message hits backpressure.
        let filler = vec![b'f'; 64 * 1024];
        // SAFETY: filler is a valid readable buffer and the write end is open.
        let written =
            unsafe { libc::write(fds[1], filler.as_ptr().cast::<libc::c_void>(), filler.len()) };
        assert_eq!(written, filler.len() as isize);
        let first =
            encode_audio_frame(&AudioFrame::new(1, vec![0.25; 64], vec![0.25; 64]).unwrap())
                .unwrap();
        let second =
            encode_audio_frame(&AudioFrame::new(2, vec![0.75; 64], vec![0.75; 64]).unwrap())
                .unwrap();
        let mut pending = None;
        let mut coalesced = 0_u64;
        queue_control_message(&input, first, &mut pending, &mut coalesced).unwrap();
        assert!(pending.is_some());
        queue_control_message(&input, second.clone(), &mut pending, &mut coalesced).unwrap();
        assert_eq!(coalesced, 1);
        assert_eq!(pending.as_ref().unwrap(), &second);
        // Make room, then the per-tick flush must deliver only the latest.
        let mut drain_buffer = vec![0_u8; 64 * 1024];
        let drained = unsafe {
            libc::read(
                fds[0],
                drain_buffer.as_mut_ptr().cast::<libc::c_void>(),
                drain_buffer.len(),
            )
        };
        assert_eq!(drained, filler.len() as isize);
        let mut errors = 0_u64;
        flush_pending(&input, &mut pending, &mut errors, "test");
        assert!(pending.is_none());
        let mut received = vec![0_u8; second.len()];
        let read = unsafe {
            libc::read(
                fds[0],
                received.as_mut_ptr().cast::<libc::c_void>(),
                received.len(),
            )
        };
        assert_eq!(read, second.len() as isize);
        assert_eq!(received, second);
        assert_eq!(errors, 0);
    }

    #[test]
    fn ack_ceiling_never_decreases_so_in_flight_pointer_acks_pass() {
        use std::os::fd::OwnedFd;
        use std::os::unix::io::FromRawFd;

        // Synthetic worker: every pipe end is real so drain_input_acks
        // exercises the actual read loop, but no process participates.
        let mut ack_fds = [0_i32; 2];
        // SAFETY: ack_fds is a valid writable pair buffer for a fresh pipe.
        assert_eq!(unsafe { libc::pipe(ack_fds.as_mut_ptr()) }, 0);
        set_nonblocking(ack_fds[0]).unwrap();
        let input_ack = ChildStdout::from(unsafe { OwnedFd::from_raw_fd(ack_fds[0]) });
        let ack_writer = unsafe { fs::File::from_raw_fd(ack_fds[1]) };

        let mut input_fds = [0_i32; 2];
        // SAFETY: input_fds is a valid writable pair buffer for a fresh pipe.
        assert_eq!(unsafe { libc::pipe(input_fds.as_mut_ptr()) }, 0);
        let input = ChildStdin::from(unsafe { OwnedFd::from_raw_fd(input_fds[1]) });
        let _input_read_end = unsafe { fs::File::from_raw_fd(input_fds[0]) };

        let mut child = Command::new("true").spawn().unwrap();
        let _ = child.wait();
        let mut stderr_command = Command::new("true");
        stderr_command.stderr(Stdio::piped());
        let mut stderr_child = stderr_command.spawn().unwrap();
        let _ = stderr_child.wait();
        let stderr = stderr_child.stderr.take().unwrap();

        let mut worker = ActiveWorker {
            spec: StartSpec {
                wallpaper_id: "431960-123".into(),
                content_hash: "abc123".into(),
                width: 1920,
                height: 1080,
                fps: 60,
                kind: RendererKind::Test,
                content: None,
                test_fault: None,
                stderr_lines: None,
                scaling: ScalingMode::Aspect,
                capability_limitations: Vec::new(),
            },
            child,
            home_path: PathBuf::new(),
            frame_path: PathBuf::new(),
            reader: None,
            started: Instant::now(),
            last_progress: Instant::now(),
            last_snapshot_saved: None,
            sequence: 0,
            input,
            input_ack,
            input_ack_buffer: Vec::new(),
            input_sequence: 0,
            input_ack_sequence: 0,
            pending_input: None,
            input_coalesced: 0,
            input_protocol_errors: 0,
            pointer_inside: false,
            pointer_x: 0,
            pointer_y: 0,
            pending_audio: None,
            audio_coalesced: 0,
            pending_media: None,
            media_coalesced: 0,
            stderr,
            stderr_ring: StderrRing::default(),
        };
        let write_ack = |sequence: u64, writer: &fs::File| {
            let line = encode_ack_line(&InputAck::new(sequence).unwrap()).unwrap();
            // SAFETY: line is a valid readable buffer and the pipe write end
            // is open; the daemon treats a short write as a protocol error.
            let written = unsafe {
                libc::write(
                    writer.as_raw_fd(),
                    line.as_ptr().cast::<libc::c_void>(),
                    line.len(),
                )
            };
            assert_eq!(written, line.len() as isize);
        };

        // The pointer path advances the sequence per message.
        for _ in 0..5 {
            worker.input_sequence = worker.input_sequence.checked_add(1).unwrap();
        }
        // An audio frame carries the display generation (1), far below the
        // pointer sequence; the ceiling must NOT drop to it.
        raise_ack_ceiling(&mut worker, 1);
        assert_eq!(worker.input_sequence, 5);
        // The in-flight ack for pointer message 5 passes the raised ceiling.
        write_ack(5, &ack_writer);
        drain_input_acks(&mut worker);
        assert_eq!(worker.input_ack_sequence, 5);
        assert_eq!(worker.input_protocol_errors, 0);

        // Counterfactual: the pre-fix plain assignment (`input_sequence =
        // generation`) would reject the very same ack as a protocol error.
        worker.input_ack_sequence = 0;
        worker.input_protocol_errors = 0;
        worker.input_sequence = 1; // old buggy assignment
        write_ack(5, &ack_writer);
        drain_input_acks(&mut worker);
        assert_eq!(worker.input_ack_sequence, 0);
        assert_eq!(worker.input_protocol_errors, 1);
    }

    #[test]
    fn the_network_grant_appends_allow_network_and_revocation_removes_it() {
        use std::os::unix::fs::PermissionsExt;

        let root = temporary_directory("network-grant");
        fs::create_dir_all(&root).unwrap();
        // A fake web renderer that records its argv; HOME is the only
        // per-launch writable env the supervisor allowlist passes.
        let script = root.join("web-renderer");
        fs::write(
            &script,
            "#!/bin/sh\nprintf '%s\\n' \"$@\" > \"$HOME/argv.txt\"\nexit 0\n",
        )
        .unwrap();
        fs::set_permissions(&script, fs::Permissions::from_mode(0o755)).unwrap();
        let mut config = validated_config(&root);
        config.renderer_paths = BTreeMap::from([(RendererKind::Web, script.clone())]);
        let config = config.validate().unwrap();
        let mut grant_store = GrantStore::open(&root.join("state")).unwrap();
        grant_store
            .set(
                "431960-123",
                GrantPatch {
                    network: Some(true),
                    ..GrantPatch::default()
                },
            )
            .unwrap();
        let (store, state) = StateStore::open(root.join("state")).unwrap();
        let mut runtime = SupervisorRuntime::new(config, store, state, grant_store);
        let spec = StartSpec {
            wallpaper_id: "431960-123".into(),
            content_hash: "abc123".into(),
            width: 160,
            height: 90,
            fps: 30,
            kind: RendererKind::Web,
            content: Some(ContentSpec::Web { root: root.clone() }),
            test_fault: None,
            stderr_lines: None,
            scaling: ScalingMode::Aspect,
            capability_limitations: Vec::new(),
        };
        // The fake renderer records its argv asynchronously; poll for it
        // within a bounded window (the script writes before it exits).
        let read_argv = |home: &Path| {
            let path = home.join("argv.txt");
            for _ in 0..200 {
                if let Ok(argv) = fs::read_to_string(&path) {
                    return argv;
                }
                thread::sleep(Duration::from_millis(10));
            }
            panic!(
                "fake renderer never recorded its argv at {}",
                path.display()
            );
        };
        // Granted: the spawned web worker's argv carries --allow-network.
        let mut worker = runtime.spawn_worker(spec.clone()).unwrap();
        let argv = read_argv(&root.join("runtime/home-1"));
        assert!(
            argv.contains("--allow-network"),
            "granted web worker argv must carry --allow-network: {argv}"
        );
        let _ = inspect_worker(&mut worker, &runtime.config);
        // Revocation: the next spawn re-reads the store and drops the flag,
        // so the bwrap sandbox gets --unshare-net again (the M2b negative).
        runtime
            .grant_store
            .set(
                "431960-123",
                GrantPatch {
                    network: Some(false),
                    ..GrantPatch::default()
                },
            )
            .unwrap();
        let mut worker = runtime.spawn_worker(spec).unwrap();
        let argv = read_argv(&root.join("runtime/home-2"));
        assert!(
            !argv.contains("--allow-network"),
            "revoked web worker argv must not carry --allow-network: {argv}"
        );
        let _ = inspect_worker(&mut worker, &runtime.config);
        fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn audio_frames_drop_latest_wins_without_the_audio_grant_and_deliver_with_it() {
        use std::os::fd::OwnedFd;
        use std::os::unix::io::FromRawFd;

        let root = temporary_directory("audio-grant");
        fs::create_dir_all(&root).unwrap();
        let (store, state) = StateStore::open(root.join("state")).unwrap();
        let mut runtime = SupervisorRuntime::new(
            validated_config(&root),
            store,
            state,
            GrantStore::open(&root.join("state")).unwrap(),
        );
        runtime.display_generation = 1;
        // Synthetic worker with real pipe ends (mirrors the ack-ceiling test).
        let mut input_fds = [0_i32; 2];
        // SAFETY: input_fds is a valid writable pair buffer for a fresh pipe.
        assert_eq!(unsafe { libc::pipe(input_fds.as_mut_ptr()) }, 0);
        let input = ChildStdin::from(unsafe { OwnedFd::from_raw_fd(input_fds[1]) });
        let _input_read_end = unsafe { fs::File::from_raw_fd(input_fds[0]) };
        let mut ack_fds = [0_i32; 2];
        // SAFETY: ack_fds is a valid writable pair buffer for a fresh pipe.
        assert_eq!(unsafe { libc::pipe(ack_fds.as_mut_ptr()) }, 0);
        set_nonblocking(ack_fds[0]).unwrap();
        let input_ack = ChildStdout::from(unsafe { OwnedFd::from_raw_fd(ack_fds[0]) });
        let _ack_writer = unsafe { fs::File::from_raw_fd(ack_fds[1]) };
        let mut child = Command::new("true").spawn().unwrap();
        let _ = child.wait();
        let mut stderr_command = Command::new("true");
        stderr_command.stderr(Stdio::piped());
        let mut stderr_child = stderr_command.spawn().unwrap();
        let _ = stderr_child.wait();
        let stderr = stderr_child.stderr.take().unwrap();
        let worker = ActiveWorker {
            spec: StartSpec {
                wallpaper_id: "431960-123".into(),
                content_hash: "abc123".into(),
                width: 1920,
                height: 1080,
                fps: 60,
                kind: RendererKind::Test,
                content: None,
                test_fault: None,
                stderr_lines: None,
                scaling: ScalingMode::Aspect,
                capability_limitations: Vec::new(),
            },
            child,
            home_path: PathBuf::new(),
            frame_path: PathBuf::new(),
            reader: None,
            started: Instant::now(),
            last_progress: Instant::now(),
            last_snapshot_saved: None,
            sequence: 0,
            input,
            input_ack,
            input_ack_buffer: Vec::new(),
            input_sequence: 0,
            input_ack_sequence: 0,
            pending_input: None,
            input_coalesced: 0,
            input_protocol_errors: 0,
            pointer_inside: false,
            pointer_x: 0,
            pointer_y: 0,
            pending_audio: None,
            audio_coalesced: 0,
            pending_media: None,
            media_coalesced: 0,
            stderr,
            stderr_ring: StderrRing::default(),
        };
        runtime.active = Some(worker);
        // No record yet: the defaults (audio off) gate delivery.
        let frame = AudioFrame::new(1, vec![0.5; 16], vec![0.5; 16]).unwrap();
        let status = runtime.forward_audio_frame(1, frame.clone()).unwrap();
        assert_eq!(
            status.audio_grant_dropped, 1,
            "the first ungranted frame must count a grant drop"
        );
        assert!(
            runtime.active.as_ref().unwrap().pending_audio.is_none(),
            "an ungranted frame must never reach the worker pipe"
        );
        assert_eq!(runtime.active.as_ref().unwrap().audio_coalesced, 0);
        // Grant audio: the next frame must be delivered to the worker.
        runtime
            .grant_store
            .set(
                "431960-123",
                GrantPatch {
                    audio: Some(true),
                    ..GrantPatch::default()
                },
            )
            .unwrap();
        let status = runtime.forward_audio_frame(1, frame.clone()).unwrap();
        assert_eq!(
            status.audio_grant_dropped, 1,
            "a granted frame must not count another drop"
        );
        let expected = encode_audio_frame(&frame).unwrap();
        let mut received = vec![0_u8; expected.len()];
        let read = unsafe {
            libc::read(
                input_fds[0],
                received.as_mut_ptr().cast::<libc::c_void>(),
                received.len(),
            )
        };
        assert_eq!(read, expected.len() as isize);
        assert_eq!(&received[..read as usize], &expected[..]);
        fs::remove_dir_all(root).unwrap();
    }

    /// B4: a script renderer for refusal/strike tests; `body` is the shell
    /// body after the shebang.
    fn script_renderer(root: &Path, name: &str, body: &str) -> PathBuf {
        fs::create_dir_all(root).unwrap();
        let script = root.join(name);
        fs::write(&script, format!("#!/bin/sh\n{body}\n")).unwrap();
        fs::set_permissions(&script, fs::Permissions::from_mode(0o755)).unwrap();
        script
    }

    fn script_runtime(root: &Path, script: PathBuf) -> SupervisorRuntime {
        let mut config = validated_config(root);
        config.renderer_paths = BTreeMap::from([(RendererKind::Test, script)]);
        let config = config.validate().unwrap();
        let (store, state) = StateStore::open(root.join("state")).unwrap();
        SupervisorRuntime::new(
            config,
            store,
            state,
            GrantStore::open(&root.join("state")).unwrap(),
        )
    }

    fn test_spec() -> StartSpec {
        StartSpec {
            wallpaper_id: "431960-123".into(),
            content_hash: "abc123".into(),
            width: 160,
            height: 90,
            fps: 30,
            kind: RendererKind::Test,
            content: None,
            test_fault: None,
            stderr_lines: None,
            scaling: ScalingMode::Aspect,
            capability_limitations: Vec::new(),
        }
    }

    /// Tick until the candidate is gone (it exited) or `limit` elapses.
    fn tick_until_candidate_settles(runtime: &mut SupervisorRuntime, limit: Duration) {
        let deadline = Instant::now() + limit;
        while runtime.candidate.is_some() && Instant::now() < deadline {
            runtime.tick();
            std::thread::sleep(Duration::from_millis(20));
        }
    }

    #[test]
    fn refusal_exit_codes_do_not_strike_restart_or_persist() {
        for (code, name) in [(73, "backend-reject"), (74, "no-drawable")] {
            let root = temporary_directory(&format!("refused-{name}"));
            let script = script_renderer(
                &root,
                "refuse.sh",
                &format!("echo event=renderer.refusal.{name} >&2\nexit {code}"),
            );
            let mut runtime = script_runtime(&root, script);
            let status = runtime.start_selected(test_spec(), false).unwrap();
            assert_eq!(status.phase, WorkerPhase::Starting);
            tick_until_candidate_settles(&mut runtime, Duration::from_secs(5));
            assert!(
                runtime.candidate.is_none(),
                "exit {code} worker must be reaped"
            );
            let status = runtime.status();
            assert_eq!(
                status.phase,
                WorkerPhase::Stopped,
                "exit {code}: no restart, no quarantine"
            );
            assert_eq!(status.last_failure, Some(FailureKind::Refused));
            let detail = status.last_failure_detail.unwrap();
            assert!(detail.starts_with(&format!("exit_code_{code}")), "{detail}");
            assert!(
                detail.contains(&format!("renderer.refusal.{name}")),
                "{detail}"
            );
            assert!(
                runtime.pending.is_none(),
                "a refusal never schedules a restart"
            );
            assert!(
                runtime.persisted.records.is_empty(),
                "a refusal is not a strike"
            );
            assert!(!status.quarantined);
            assert_eq!(status.failures, 0);
        }
    }

    #[test]
    fn ordinary_exit_still_strikes_and_restarts() {
        let root = temporary_directory("strike-exit-3");
        let script = script_renderer(&root, "crash.sh", "exit 3");
        let mut runtime = script_runtime(&root, script);
        runtime.start_selected(test_spec(), false).unwrap();
        tick_until_candidate_settles(&mut runtime, Duration::from_secs(5));
        let status = runtime.status();
        assert_eq!(status.phase, WorkerPhase::Restarting);
        assert_eq!(status.last_failure, Some(FailureKind::ProcessExit));
        assert_eq!(status.failures, 1);
        assert!(runtime.pending.is_some());
    }

    #[test]
    fn refusal_from_an_active_worker_is_a_runtime_strike() {
        // A worker that already published and then exits 73 (web heartbeat
        // after first paint) failed at runtime: it strikes like any exit.
        let root = temporary_directory("active-refused");
        let script = script_renderer(&root, "sleep.sh", "sleep 30");
        let mut runtime = script_runtime(&root, script);
        let worker = runtime.spawn_worker(test_spec()).unwrap();
        runtime.active = Some(worker);
        runtime.requested = Some(test_spec());
        runtime.phase = WorkerPhase::Live;
        runtime.handle_active_failure(FailureKind::Refused, "exit_code_73".into());
        let record = runtime
            .persisted
            .records
            .get(&test_spec().identity())
            .expect("an active-worker exit 73 must be recorded");
        assert_eq!(record.failures, 1);
        assert_eq!(record.last_failure, FailureKind::ProcessExit);
        assert_eq!(
            runtime.status().last_failure,
            Some(FailureKind::ProcessExit)
        );
        assert_eq!(runtime.phase, WorkerPhase::Restarting);
    }

    #[test]
    fn quarantined_start_reports_the_record_and_its_reason() {
        let root = temporary_directory("quarantined-status");
        let script = script_renderer(&root, "never-run.sh", "exit 0");
        let mut runtime = script_runtime(&root, script);
        let spec = test_spec();
        runtime.persisted.records.insert(
            spec.identity(),
            FailureRecord {
                wallpaper_id: spec.wallpaper_id.clone(),
                content_hash: spec.content_hash.clone(),
                failures: 3,
                quarantined: true,
                last_failure: FailureKind::ProcessExit,
                last_detail: "exit_code_73 stderr=[zygote could not fork]".into(),
                updated_unix_seconds: 1,
            },
        );
        let status = runtime.start_selected(spec.clone(), false).unwrap();
        assert_eq!(status.phase, WorkerPhase::Quarantined);
        assert!(
            status.quarantined,
            "status must say the identity is quarantined"
        );
        assert_eq!(status.failures, 3);
        assert_eq!(status.last_failure, Some(FailureKind::ProcessExit));
        assert_eq!(
            status.last_failure_detail.as_deref(),
            Some("exit_code_73 stderr=[zygote could not fork]")
        );
        assert!(
            runtime.candidate.is_none(),
            "a quarantined start spawns nothing"
        );
        // Retry clears exactly this identity and spawns.
        let status = runtime.start_selected(spec, true).unwrap();
        assert_eq!(status.phase, WorkerPhase::Starting);
        assert!(!status.quarantined);
        assert!(runtime.persisted.records.is_empty());
        runtime.stop_candidate(false);
    }

    #[test]
    fn records_from_another_build_are_dropped_at_load() {
        let root = temporary_directory("build-id");
        let dir = root.join("state");
        let (store, mut state) = StateStore::open(dir.clone()).unwrap();
        state.records.insert(
            "a:b:test".into(),
            FailureRecord {
                wallpaper_id: "a".into(),
                content_hash: "b".into(),
                failures: 3,
                quarantined: true,
                last_failure: FailureKind::ProcessExit,
                last_detail: "old build".into(),
                updated_unix_seconds: 1,
            },
        );
        state.forced_kill_count = 7;
        state.build_id = Some("build-1".into());
        store.save(&state).unwrap();

        // Same build: everything survives.
        let (_, same) = StateStore::open_for_build(dir.clone(), "build-1").unwrap();
        assert_eq!(same.records.len(), 1);
        assert_eq!(same.build_id.as_deref(), Some("build-1"));
        // New build: records go, the rest stays, the file now names the
        // new build so the next load keeps what this build earns.
        let (_, fresh) = StateStore::open_for_build(dir.clone(), "build-2").unwrap();
        assert!(fresh.records.is_empty());
        assert_eq!(fresh.forced_kill_count, 7);
        assert_eq!(fresh.build_id.as_deref(), Some("build-2"));
        let (_, again) = StateStore::open(dir.clone()).unwrap();
        assert_eq!(again.build_id.as_deref(), Some("build-2"));
        // A pre-B4 file (no build_id) is an unknown build: dropped once.
        let legacy = serde_json::json!({
            "schema_version": 1,
            "records": {"x:y:test": {
                "wallpaper_id": "x", "content_hash": "y", "failures": 3,
                "quarantined": true, "last_failure": "process_exit",
                "last_detail": "legacy", "updated_unix_seconds": 1}},
            "last_good": null,
        });
        atomic_write(
            &dir.join("supervisor-v1.json"),
            &serde_json::to_vec(&legacy).unwrap(),
        )
        .unwrap();
        let (_, migrated) = StateStore::open_for_build(dir, "build-2").unwrap();
        assert!(migrated.records.is_empty());
        assert_eq!(migrated.build_id.as_deref(), Some("build-2"));
    }

    #[test]
    fn build_identity_follows_the_renderer_binaries() {
        let root = temporary_directory("build-identity");
        let script = script_renderer(&root, "renderer.sh", "exit 0");
        let mut config = validated_config(&root);
        config.renderer_paths = BTreeMap::from([(RendererKind::Test, script.clone())]);
        let config = config.validate().unwrap();
        let first = build_identity(&config);
        assert!(first.starts_with(&format!("daemon={}:", env!("CARGO_PKG_VERSION"))));
        assert!(first.contains("test="));
        assert_eq!(build_identity(&config), first, "stable across calls");
        // A replaced renderer (different size) changes the identity.
        fs::write(&script, "#!/bin/sh\necho upgraded\nexit 0\n").unwrap();
        assert_ne!(build_identity(&config), first);
        // A missing renderer is part of the identity too.
        fs::remove_file(&script).unwrap();
        assert!(build_identity(&config).contains("test=missing"));
    }
}
