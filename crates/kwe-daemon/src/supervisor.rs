// SPDX-License-Identifier: Apache-2.0
//! Original bounded renderer-process supervisor.
//!
//! Upstream projects in `THIRD_PARTY.yml` informed the process-isolation goal,
//! but this state machine, persistence format, and implementation are original.

use std::{
    collections::{BTreeMap, BTreeSet},
    fs::{self, OpenOptions},
    io::{self, Read},
    os::unix::{
        fs::OpenOptionsExt,
        io::AsRawFd,
        process::{CommandExt, ExitStatusExt},
    },
    path::PathBuf,
    process::{Child, ChildStdin, ChildStdout, Command as ProcessCommand, Stdio},
    sync::mpsc::{self, Receiver, SyncSender, TrySendError},
    thread::{self, JoinHandle},
    time::{Duration, Instant},
};

#[cfg(test)]
use crate::persist::unix_nanos;
use crate::persist::{atomic_write, ensure_private_dir, unix_seconds};
use anyhow::{Context, Result, anyhow, bail};
use kwe_core::preflight_scene;
use kwe_frame_protocol::{FrameSnapshot, FrameSpec, ProtocolError, SharedFrameReader};
use kwe_input_protocol::{
    MAX_MESSAGE_BYTES as MAX_INPUT_MESSAGE_BYTES, PointerMessage, PointerPhase, decode_ack_line,
    encode_pointer_line,
};
use serde::{Deserialize, Serialize};

const COMMAND_CAPACITY: usize = 16;
const COMMAND_TIMEOUT: Duration = Duration::from_secs(5);
const POLL_INTERVAL: Duration = Duration::from_millis(40);
const MAX_RECORDS: usize = 256;
const MAX_STATE_BYTES: u64 = 1024 * 1024;
const MAX_SUPERVISED_MAPPING_BYTES: u64 = 128 * 1024 * 1024;
const SNAPSHOT_INTERVAL: Duration = Duration::from_secs(30);
const MAX_ACK_BUFFER_BYTES: usize = 1024;
const MAX_ACK_READ_BYTES_PER_TICK: usize = 4096;

#[derive(Debug, Clone)]
pub struct SupervisorConfig {
    pub renderer_path: PathBuf,
    pub runtime_dir: PathBuf,
    pub state_dir: PathBuf,
    pub startup_timeout: Duration,
    pub frame_timeout: Duration,
    pub stop_grace: Duration,
    pub restart_delay: Duration,
    pub canary_duration: Duration,
    pub handoff_timeout: Duration,
    pub max_failures: u32,
    pub resource_limits: RendererResourceLimits,
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
        if !(256..=65_536).contains(&self.address_space_mib)
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
        self.renderer_path = fs::canonicalize(&self.renderer_path).with_context(|| {
            format!(
                "resolve renderer executable {}",
                self.renderer_path.display()
            )
        })?;
        if !self.renderer_path.is_file() {
            bail!(
                "renderer executable is not a regular file: {}",
                self.renderer_path.display()
            );
        }
        if self.startup_timeout.is_zero()
            || self.frame_timeout.is_zero()
            || self.stop_grace.is_zero()
            || self.canary_duration.is_zero()
            || self.handoff_timeout.is_zero()
            || self.max_failures == 0
            || self.max_failures > 10
        {
            bail!("supervisor deadlines and failure budget must be bounded and non-zero");
        }
        self.resource_limits = self.resource_limits.validate()?;
        ensure_private_dir(&self.runtime_dir)?;
        ensure_private_dir(&self.state_dir)?;
        Ok(self)
    }
}

#[derive(Debug, Clone)]
pub struct StartSpec {
    pub wallpaper_id: String,
    pub content_hash: String,
    pub width: u32,
    pub height: u32,
    pub fps: u32,
    pub test_fault: Option<TestFault>,
    pub scene_path: Option<PathBuf>,
}

impl StartSpec {
    pub fn validate(&self) -> Result<()> {
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
        if let Some(path) = &self.scene_path {
            let report = preflight_scene(path);
            if !report.safe {
                bail!(
                    "scene preflight rejected {}: {}",
                    path.display(),
                    report.reasons.join("; ")
                );
            }
        }
        Ok(())
    }

    fn identity(&self) -> String {
        format!("{}:{}", self.wallpaper_id, self.content_hash)
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
        reply: mpsc::Sender<Result<WorkerStatus>>,
    },
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
    ) -> Result<WorkerStatus> {
        self.request(|reply| ControlCommand::PointerInput {
            generation,
            phase,
            x,
            y,
            reply,
        })
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
        let (store, state) = StateStore::open(config.state_dir.clone())?;
        let (sender, receiver) = mpsc::sync_channel(COMMAND_CAPACITY);
        let thread = thread::Builder::new()
            .name("kwe-renderer-supervisor".into())
            .spawn(move || SupervisorRuntime::new(config, store, state).run(receiver))?;
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
    fn new(config: SupervisorConfig, store: StateStore, persisted: PersistedState) -> Self {
        Self {
            config,
            store,
            persisted,
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
                    reply,
                }) => {
                    let result = self.forward_pointer_input(generation, phase, x, y);
                    let _ = reply.send(result);
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
        spec.validate()?;
        if self.retired.is_some() {
            bail!("display handoff is still awaiting acknowledgement");
        }
        self.stop_candidate(false);
        self.pending = None;
        self.restart_count = 0;
        self.last_failure = None;
        let identity = spec.identity();
        if clear_failure {
            self.persisted.records.remove(&identity);
            self.store.save(&self.persisted)?;
        } else if self
            .persisted
            .records
            .get(&identity)
            .is_some_and(|record| record.quarantined)
        {
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
        let frame_path = self.config.runtime_dir.join(format!(
            "frame-{}-{}.bin",
            std::process::id(),
            self.launch_serial
        ));
        let mut command = ProcessCommand::new(&self.config.renderer_path);
        command
            .arg("--output")
            .arg(&frame_path)
            .arg("--width")
            .arg(spec.width.to_string())
            .arg("--height")
            .arg(spec.height.to_string())
            .arg("--fps")
            .arg(spec.fps.to_string())
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::null())
            .env_clear();
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
        let resource_limits = self.config.resource_limits;
        // SAFETY: this closure runs in the child after fork and before exec. It
        // calls only async-signal-safe libc functions and does not allocate.
        unsafe {
            command.pre_exec(move || {
                if libc::setpgid(0, 0) != 0 {
                    return Err(io::Error::last_os_error());
                }
                if libc::prctl(libc::PR_SET_PDEATHSIG, libc::SIGKILL, 0, 0, 0) != 0 {
                    return Err(io::Error::last_os_error());
                }
                if libc::getppid() != expected_parent {
                    return Err(io::Error::new(
                        io::ErrorKind::Interrupted,
                        "daemon exited before renderer exec",
                    ));
                }
                if libc::prctl(libc::PR_SET_NO_NEW_PRIVS, 1, 0, 0, 0) != 0 {
                    return Err(io::Error::last_os_error());
                }
                apply_resource_limits(resource_limits)?;
                Ok(())
            });
        }
        let mut child = command
            .spawn()
            .with_context(|| format!("launch renderer {}", self.config.renderer_path.display()))?;
        let channels = (|| -> Result<(ChildStdin, ChildStdout)> {
            let input = child
                .stdin
                .take()
                .context("renderer input pipe unavailable")?;
            let input_ack = child
                .stdout
                .take()
                .context("renderer input acknowledgement pipe unavailable")?;
            set_nonblocking(input.as_raw_fd()).context("configure renderer input pipe")?;
            set_nonblocking(input_ack.as_raw_fd())
                .context("configure renderer acknowledgement pipe")?;
            Ok((input, input_ack))
        })();
        let (input, input_ack) = match channels {
            Ok(channels) => channels,
            Err(error) => {
                terminate_and_reap(&mut child, self.config.stop_grace);
                return Err(error);
            }
        };
        let now = Instant::now();
        Ok(ActiveWorker {
            spec,
            child,
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
        })
    }

    fn forward_pointer_input(
        &mut self,
        generation: u64,
        phase: PointerPhase,
        x: f64,
        y: f64,
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
        let message = PointerMessage::from_normalized(sequence, phase, x, y)?;
        let bytes = encode_pointer_line(&message)?;

        if worker.pending_input.take().is_some() {
            worker.input_coalesced = worker.input_coalesced.saturating_add(1);
        }
        match try_write_input(&worker.input, &bytes)? {
            PipeWrite::Written => {}
            PipeWrite::WouldBlock => worker.pending_input = Some(bytes),
        }
        worker.input_sequence = sequence;
        worker.pointer_inside = phase != PointerPhase::Leave;
        worker.pointer_x = message.x;
        worker.pointer_y = message.y;
        Ok(self.status())
    }

    fn service_active_input(&mut self) {
        let Some(worker) = self.active.as_mut() else {
            return;
        };
        drain_input_acks(worker);
        let Some(bytes) = worker.pending_input.take() else {
            return;
        };
        match try_write_input(&worker.input, &bytes) {
            Ok(PipeWrite::Written) => {}
            Ok(PipeWrite::WouldBlock) => worker.pending_input = Some(bytes),
            Err(error) => {
                worker.input_protocol_errors = worker.input_protocol_errors.saturating_add(1);
                eprintln!("event=renderer.input_write_error detail={error}");
            }
        }
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
        let record = requested.and_then(|spec| self.persisted.records.get(&spec.identity()));
        WorkerStatus {
            phase: self.phase,
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
            resource_limits: self.config.resource_limits,
            input_sequence: active.map_or(0, |worker| worker.input_sequence),
            input_ack_sequence: active.map_or(0, |worker| worker.input_ack_sequence),
            input_pending: active.is_some_and(|worker| worker.pending_input.is_some()),
            input_coalesced: active.map_or(0, |worker| worker.input_coalesced),
            input_protocol_errors: active.map_or(0, |worker| worker.input_protocol_errors),
            pointer_inside: active.is_some_and(|worker| worker.pointer_inside),
            pointer_x: active.map_or(0, |worker| worker.pointer_x),
            pointer_y: active.map_or(0, |worker| worker.pointer_y),
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
            let controlled_memory_pressure = matches!(
                worker.spec.test_fault.as_ref(),
                Some(TestFault::MemoryPressure { .. })
            );
            if status.code() == Some(71) && controlled_memory_pressure {
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
            return Some(WorkerObservation::Failure(FailureKind::ProcessExit, detail));
        }
        Ok(None) => {}
        Err(error) => {
            return Some(WorkerObservation::Failure(
                FailureKind::ProcessExit,
                format!("wait_error:{error}"),
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
                if now.duration_since(worker.started) >= config.startup_timeout {
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
    if worker.sequence == 0 && now.duration_since(worker.started) >= config.startup_timeout {
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
                Ok(ack)
                    if ack.sequence > worker.input_ack_sequence
                        && ack.sequence <= worker.input_sequence =>
                {
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

fn set_nonblocking(descriptor: libc::c_int) -> io::Result<()> {
    let flags = unsafe { libc::fcntl(descriptor, libc::F_GETFL) };
    if flags < 0 {
        return Err(io::Error::last_os_error());
    }
    if unsafe { libc::fcntl(descriptor, libc::F_SETFL, flags | libc::O_NONBLOCK) } < 0 {
        return Err(io::Error::last_os_error());
    }
    Ok(())
}

fn apply_resource_limits(limits: RendererResourceLimits) -> io::Result<()> {
    let mib = 1024_u64 * 1024;
    set_resource_limit(libc::RLIMIT_AS, limits.address_space_mib * mib)?;
    set_resource_limit(libc::RLIMIT_FSIZE, limits.file_size_mib * mib)?;
    set_resource_limit(libc::RLIMIT_NOFILE, limits.open_files)?;
    set_resource_limit(libc::RLIMIT_NPROC, limits.processes)?;
    set_resource_limit(libc::RLIMIT_CORE, limits.core_dump_bytes)?;
    Ok(())
}

fn set_resource_limit(resource: libc::__rlimit_resource_t, value: u64) -> io::Result<()> {
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

fn signal_process_group(pid: u32, signal: libc::c_int) {
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

fn validate_identity_part(name: &str, value: &str) -> Result<()> {
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
            test_fault: None,
            scene_path: None,
        };
        assert!(valid.validate().is_ok());
        let mut invalid = valid.clone();
        invalid.wallpaper_id = "../escape".into();
        assert!(invalid.validate().is_err());
        invalid = valid.clone();
        invalid.fps = 0;
        assert!(invalid.validate().is_err());
        invalid = valid;
        invalid.width = 8192;
        invalid.height = 8192;
        assert!(invalid.validate().is_err());
        let mut invalid_scene = StartSpec {
            wallpaper_id: "431960-123".into(),
            content_hash: "abc123".into(),
            width: 1920,
            height: 1080,
            fps: 60,
            test_fault: None,
            scene_path: Some(std::env::temp_dir().join("kwe-missing-scene.json")),
        };
        assert!(invalid_scene.validate().is_err());
        invalid_scene.scene_path = None;
        assert!(invalid_scene.validate().is_ok());
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
}
