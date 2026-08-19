// SPDX-License-Identifier: Apache-2.0
// kwe-scene-renderer: the M3a SceneScript worker (original implementation,
// ADR 0001). The daemon spawns it as:
//
//   kwe-scene-renderer --output <frame> --width W --height H --fps N \
//       --content <scene.json> [fault flags]
//
// with stdin = input pipe (NDJSON), stdout = acks, stderr = the daemon's
// bounded ring. Exit codes match the video worker's contract: 0 normal,
// 70 exit-after fault, 71/72 resource faults, 73 backend rejection (scene
// unparseable, script reference rejected, engine bootstrap failure, Vulkan
// device unusable, or a sustained render failure streak).
//
// Every frame: update(dt) runs under the 8 ms/33 ms budget (src/js.rs), the
// script's Engine.clearcolor is read back, an offscreen Vulkan clear renders
// that color (src/vulkan.rs), and the readback is published premultiplied
// BGRA. Script exceptions never kill the renderer; a soft-timeout frame is
// skipped by re-publishing the last pixels (the supervisor only watches
// sequence progression).

mod js;
mod scene;
mod vulkan;

use std::fs;
use std::io::Write;
use std::os::unix::fs::{DirBuilderExt, OpenOptionsExt};
use std::path::{Path, PathBuf};
use std::process::exit;
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::{Duration, Instant};

use anyhow::{Context as AnyhowContext, Result, bail};
use clap::Parser;
use kwe_frame_protocol::{FrameSpec, ProducerState, SharedFrameWriter};
use kwe_input_protocol::{
    AudioFrame, InputAck, PointerMessage, decode_audio_frame, decode_media_state,
    decode_pointer_line, encode_ack_line,
};

use js::{EngineStartError, ScriptEngine, StepResult};
use scene::{SceneConfig, SceneError};
use vulkan::{ClearRenderer, RenderError};

/// Backend rejection: the scene cannot be rendered at all.
const EXIT_BACKEND_REJECT: i32 = 73;
/// Synthetic fault exit codes, identical to the video worker's contract.
/// Exit 71 is the resource-limit declaration: the daemon maps ANY worker
/// exit 71 to `resource_limit` (memory denied), fault flag or not.
const EXIT_EXIT_AFTER: i32 = 70;
const EXIT_MEMORY_DENIED: i32 = 71;
const EXIT_MEMORY_UNEXPECTED: i32 = 72;

/// The loop observes SIGTERM within this bound.
const MAX_WAIT: Duration = Duration::from_millis(50);
/// Bound on lines read from the control stream per poll (see InputChannel).
const MAX_INPUT_MESSAGE_BYTES: usize = 4096;
const MAX_INPUT_READS_PER_POLL: usize = 16;
/// A render failure streak longer than this escalates to backend rejection.
const MAX_CONSECUTIVE_RENDER_FAILURES: u64 = 3;

/// Set by the SIGTERM handler; observed by the loop within MAX_WAIT.
static TERMINATED: AtomicBool = AtomicBool::new(false);

/// Async-signal-safe handler: SIGTERM only records the termination request.
/// The loop observes it within MAX_WAIT and shuts down gracefully.
extern "C" fn on_sigterm(_signal: libc::c_int) {
    TERMINATED.store(true, Ordering::Release);
}

fn install_term_handler(ignore: bool) {
    if ignore {
        // SAFETY: SIG_IGN uses a process-global constant handler, installed
        // before any worker thread besides the main one exists.
        unsafe { libc::signal(libc::SIGTERM, libc::SIG_IGN) };
    } else {
        // SAFETY: `on_sigterm` only stores to a process-global atomic, which
        // is async-signal-safe.
        unsafe { libc::signal(libc::SIGTERM, on_sigterm as *const () as libc::sighandler_t) };
    }
}

// ---------------------------------------------------------------------------
// Command line
// ---------------------------------------------------------------------------

#[derive(Debug, Parser)]
#[command(version, about = "Isolated KWE supervised SceneScript renderer")]
struct Arguments {
    /// Frame mapping file (validated and pre-opened by the daemon). Only
    /// `--probe` may omit it.
    #[arg(long, required_unless_present = "probe")]
    output: Option<PathBuf>,
    /// Frame width in pixels.
    #[arg(long, default_value_t = 960, value_parser = clap::value_parser!(u32).range(1..=8192))]
    width: u32,
    /// Frame height in pixels.
    #[arg(long, default_value_t = 540, value_parser = clap::value_parser!(u32).range(1..=8192))]
    height: u32,
    /// Publish pacing in frames per second.
    #[arg(long, default_value_t = 30, value_parser = clap::value_parser!(u32).range(1..=240))]
    fps: u32,
    /// Scene descriptor (scene.json) to render (daemon-validated before
    /// spawn). Only `--probe` may omit it.
    #[arg(long, required_unless_present = "probe")]
    content: Option<PathBuf>,
    /// Restrict the Vulkan physical device pick to names containing this
    /// substring (e.g. "llvmpipe" for the software test lane, which the
    /// daemon's env_clear would otherwise strip VK_ICD_FILENAMES of).
    #[arg(long)]
    device: Option<String>,
    /// Stall before creating the frame mapping (supervisor startup test).
    #[arg(long)]
    startup_hang: bool,
    /// Publish one frame then hang (supervisor frame-timeout test).
    #[arg(long)]
    hang_after: Option<u64>,
    /// Publish N frames then corrupt the mapping magic and hang.
    #[arg(long)]
    corrupt_after: Option<u64>,
    /// Exit with code 70 after publishing N frames.
    #[arg(long)]
    exit_after: Option<u64>,
    /// Ignore SIGTERM and hang forever (supervisor force-kill test).
    #[arg(long)]
    ignore_term: bool,
    /// Attempt a large allocation after publishing N frames (exit code 71).
    #[arg(long)]
    memory_pressure_after: Option<u64>,
    /// Allocation size in MiB for the memory-pressure fault.
    #[arg(long)]
    memory_pressure_mib: Option<u64>,
    /// Print a JSON backend report ({device, device_kind, scene_format,
    /// quickjs, ...}) without creating a device or loading a scene, then
    /// exit 0. `kwe diagnose` runs this lane.
    #[arg(long)]
    probe: bool,
}

fn try_memory_pressure(mib: Option<u64>) -> Result<(), ()> {
    // Simulate an allocation that crosses the supervisor's address-space
    // rlimit. `malloc` returns NULL for an over-limit mmap on glibc (exit 71);
    // an allocation that unexpectedly succeeds is still a fault (exit 72).
    let bytes = mib.unwrap_or(1024) * 1024 * 1024;
    let mut pointer: *mut libc::c_void = std::ptr::null_mut();
    // SAFETY: posix_memalign initializes `pointer` on success; we only free
    // on success and never touch the allocation otherwise.
    let code = unsafe { libc::posix_memalign(&mut pointer, 4096, bytes as usize) };
    if code == 0 && !pointer.is_null() {
        // SAFETY: allocated exactly once above.
        unsafe { libc::free(pointer) };
        Ok(())
    } else {
        Err(())
    }
}

/// Block forever with bounded stderr diagnostics. Used by the fault path so
/// the supervisor's kill/restart machinery is what reclaims the worker.
fn park_forever() -> ! {
    loop {
        std::thread::sleep(Duration::from_secs(86400));
    }
}

// ---------------------------------------------------------------------------
// Input channel
// ---------------------------------------------------------------------------

/// Nonblocking stdin reader for the newline-delimited JSON control protocol.
/// Never blocks, never grows: reads are capped per poll, junk is ignored
/// silently. M1a review decision: sequence numbers are NOT checked for
/// monotonicity on audio/media (the daemon is the authority on ordering);
/// every valid message is acked.
struct InputChannel {
    pending: Vec<u8>,
    stdout: std::io::Stdout,
    /// Latest pointer position (exposed to the script in M3i; stored now).
    pointer: Option<PointerMessage>,
    /// Latest audio spectrum (consumed by M3k effects; stored + counted now).
    audio: Option<AudioFrame>,
    audio_frames: u64,
}

impl InputChannel {
    fn new() -> Result<Self> {
        // Both descriptors were inherited from the daemon.
        set_nonblocking(libc::STDIN_FILENO)?;
        set_nonblocking(libc::STDOUT_FILENO)?;
        Ok(Self {
            pending: Vec::with_capacity(MAX_INPUT_MESSAGE_BYTES),
            stdout: std::io::stdout(),
            pointer: None,
            audio: None,
            audio_frames: 0,
        })
    }

    fn poll(&mut self) {
        for _ in 0..MAX_INPUT_READS_PER_POLL {
            let mut chunk = [0_u8; 256];
            // SAFETY: stdin was set nonblocking; reads into a stack buffer
            // of 256 bytes with a bounded count cannot overflow it.
            let read =
                unsafe { libc::read(libc::STDIN_FILENO, chunk.as_mut_ptr().cast(), chunk.len()) };
            if read <= 0 {
                break; // nothing ready (EAGAIN) or closed: stop polling
            }
            self.pending.extend_from_slice(&chunk[..read as usize]);
            while let Some(position) = self.pending.iter().position(|&b| b == b'\n') {
                let line = self.pending.drain(..=position).collect::<Vec<u8>>();
                self.dispatch(&line[..line.len() - 1]);
            }
            // Bound the pending buffer: a peer that never sends newlines
            // grows it by up to 4 KiB per poll, so once the cap is crossed
            // the partial line is discarded wholesale (it is junk anyway;
            // mirror of the video worker's guard).
            if self.pending.len() > MAX_INPUT_MESSAGE_BYTES {
                self.pending.clear();
            }
        }
    }

    fn dispatch(&mut self, line: &[u8]) {
        let Some(text) = std::str::from_utf8(line).ok() else {
            return; // junk is ignored silently
        };
        let Ok(value) = serde_json::from_str::<serde_json::Value>(text) else {
            return;
        };
        let message_type = value.get("type").and_then(serde_json::Value::as_str);
        let sequence = value.get("sequence").and_then(serde_json::Value::as_u64);
        // SAFETY: an ack line is small and self-contained; a non-zero
        // sequence is required by the protocol (zero means "no ack").
        let ack = sequence
            .filter(|&sequence| sequence != 0)
            .and_then(|sequence| InputAck::new(sequence).ok())
            .and_then(|ack| encode_ack_line(&ack).ok());
        match message_type {
            Some("pointer_position") => {
                if let Ok(message) = decode_pointer_line(line) {
                    self.pointer = Some(message);
                    self.ack(ack.as_deref());
                }
            }
            Some("audio_bands") => {
                if let Ok(frame) = decode_audio_frame(line) {
                    self.audio = Some(frame);
                    self.audio_frames = self.audio_frames.saturating_add(1);
                    if self.audio_frames.is_multiple_of(1000) {
                        eprintln!(
                            "event=renderer.scene.audio_frames count={}",
                            self.audio_frames
                        );
                    }
                    self.ack(ack.as_deref());
                }
            }
            // Acked and otherwise a no-op in M3a: scenes have no media
            // transport of their own.
            Some("media_state") if decode_media_state(line).is_ok() => {
                self.ack(ack.as_deref());
            }
            _ => {} // unknown types and malformed messages are ignored
        }
    }

    fn ack(&mut self, ack: Option<&[u8]>) {
        let Some(ack) = ack else { return };
        // SAFETY: stdout is nonblocking; a full pipe simply drops the ack,
        // which is diagnostic only.
        let _ = self
            .stdout
            .write_all(ack)
            .and_then(|()| self.stdout.flush());
    }
}

/// Toggle O_NONBLOCK on one standard descriptor inherited from the daemon.
fn set_nonblocking(fd: libc::c_int) -> Result<()> {
    // SAFETY: `fd` is a standard descriptor of this process; fcntl on it is
    // always safe for our own descriptors.
    let flags = unsafe { libc::fcntl(fd, libc::F_GETFL) };
    if flags < 0 {
        bail!("fcntl F_GETFL failed: {}", std::io::Error::last_os_error());
    }
    // SAFETY: same descriptor, read-modify-write of the O_NONBLOCK bit only.
    let updated = unsafe { libc::fcntl(fd, libc::F_SETFL, flags | libc::O_NONBLOCK) };
    if updated < 0 {
        bail!("fcntl F_SETFL failed: {}", std::io::Error::last_os_error());
    }
    Ok(())
}

// ---------------------------------------------------------------------------
// Publish decisions
// ---------------------------------------------------------------------------

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum PublishDecision {
    NewFrame,
    Keepalive,
}

/// What a script step outcome means for this frame. Pure, so the escalation
/// policy is unit-testable without an engine. A skipped frame (timeout,
/// exception, disabled script) is re-published as a keepalive so the
/// supervisor's frame timeout never trips; Allocation is a keepalive
/// frame-wise, but the loop exits 71.
fn publish_decision(step: StepResult) -> PublishDecision {
    match step {
        StepResult::NewFrame(_) => PublishDecision::NewFrame,
        StepResult::SoftTimeout | StepResult::HardTimeout | StepResult::ScriptError => {
            PublishDecision::Keepalive
        }
        StepResult::Allocation => PublishDecision::Keepalive,
    }
}

/// A render failure streak of this length escalates to backend rejection
/// (exit 73). Pure so the policy is unit-testable.
fn render_failure_fatal(consecutive: u64) -> bool {
    consecutive >= MAX_CONSECUTIVE_RENDER_FAILURES
}

// ---------------------------------------------------------------------------
// Worker loop
// ---------------------------------------------------------------------------

struct SceneWorker {
    arguments: Arguments,
    spec: FrameSpec,
    writer: SharedFrameWriter,
    input: InputChannel,
    engine: ScriptEngine,
    renderer: ClearRenderer,
    published: u64,
    consecutive_render_failures: u64,
}

impl SceneWorker {
    /// Synthetic faults keyed on the published-frame count, mirroring the
    /// video worker's fault block exactly (order: exit, corrupt, hang,
    /// memory).
    fn check_faults(&mut self) -> Result<()> {
        if let Some(after) = self.arguments.exit_after
            && self.published >= after
        {
            eprintln!("event=renderer.fault kind=exit_after");
            exit(EXIT_EXIT_AFTER);
        }
        if let Some(after) = self.arguments.corrupt_after
            && self.published >= after
        {
            eprintln!("event=renderer.fault kind=corrupt_after");
            self.writer.corrupt_magic_for_test();
            park_forever();
        }
        if let Some(after) = self.arguments.hang_after
            && self.published >= after
        {
            eprintln!("event=renderer.fault kind=hang_after");
            park_forever();
        }
        if let Some(after) = self.arguments.memory_pressure_after
            && self.published >= after
        {
            eprintln!("event=renderer.fault kind=memory_pressure_after");
            match try_memory_pressure(self.arguments.memory_pressure_mib) {
                // An allocation that unexpectedly succeeded is itself the
                // anomaly: exit 72 (mirrors the video worker exactly).
                Ok(()) => exit(EXIT_MEMORY_UNEXPECTED),
                Err(()) => exit(EXIT_MEMORY_DENIED),
            }
        }
        Ok(())
    }

    fn run(&mut self) -> Result<()> {
        let interval = Duration::from_secs_f64(1.0 / f64::from(self.arguments.fps));
        let mut deadline = Instant::now();
        let mut last_step = Instant::now();
        eprintln!(
            "event=renderer.scene.start fps={} script={}",
            self.arguments.fps,
            self.engine.script_ok()
        );
        // The script may be broken (contained at load), so publish the
        // scene.json clear color once before the loop: the supervisor's
        // canary must pass regardless of script health. An initial render
        // failure means the compositor cannot produce frames at all: that
        // is a backend rejection (exit 73), not an internal error.
        let initial = match self.renderer.render(self.engine.clear_color()) {
            Ok(pixels) => pixels,
            Err(error) => reject_render(&error, "initial render failure"),
        };
        self.published = self.writer.publish(&initial)?;
        let mut last_pixels: Option<Vec<u8>> = Some(initial);
        loop {
            self.input.poll();
            self.check_faults()?;
            if TERMINATED.load(Ordering::Acquire) {
                self.writer.set_state(ProducerState::Stopping);
                let stats = self.engine.stats();
                eprintln!(
                    "event=renderer.complete frames={} script_errors={} soft_timeouts={} hard_timeouts={}",
                    self.published, stats.script_errors, stats.soft_timeouts, stats.hard_timeouts
                );
                return Ok(());
            }
            let now = Instant::now();
            if now < deadline {
                // Wait the shorter of the pacing remainder and the bound,
                // so TERMINATED and input are observed at least every 50 ms.
                std::thread::sleep(deadline.duration_since(now).min(MAX_WAIT));
                continue;
            }
            let dt = now.duration_since(last_step).as_secs_f64();
            last_step = now;
            let step = self.engine.step(dt);
            match publish_decision(step) {
                PublishDecision::NewFrame => {
                    let StepResult::NewFrame(color) = step else {
                        unreachable!("publish_decision matched NewFrame");
                    };
                    match self.renderer.render(color) {
                        Ok(pixels) if pixels.len() == self.spec.pixel_bytes() => {
                            // Exact-size check: the conversion is exact by
                            // construction; a mismatch means a malformed
                            // frame, which is skipped and counted, never
                            // published.
                            self.consecutive_render_failures = 0;
                            self.published = self.writer.publish(&pixels)?;
                            last_pixels = Some(pixels);
                        }
                        Ok(_) => {
                            self.consecutive_render_failures =
                                self.consecutive_render_failures.saturating_add(1);
                            eprintln!(
                                "event=renderer.scene.render_error consecutive={} detail=size-mismatch",
                                self.consecutive_render_failures
                            );
                            self.render_keepalive(&last_pixels)?;
                        }
                        Err(error) => {
                            self.consecutive_render_failures =
                                self.consecutive_render_failures.saturating_add(1);
                            eprintln!(
                                "event=renderer.scene.render_error consecutive={} detail={error}",
                                self.consecutive_render_failures
                            );
                            // A fence timeout means the GPU is not making
                            // progress: the submit still owns the fence and
                            // the command buffer, so any retry would reset a
                            // pending fence and re-record a pending command
                            // buffer (Vulkan VUID violations). It is
                            // immediately fatal. Other render failures
                            // escalate after a bounded streak.
                            if matches!(error, RenderError::FenceTimeout) {
                                reject_render(&error, "fence timeout (device not making progress)");
                            }
                            if render_failure_fatal(self.consecutive_render_failures) {
                                reject_render(&error, "render failure streak");
                            }
                            self.render_keepalive(&last_pixels)?;
                        }
                    }
                }
                PublishDecision::Keepalive => {
                    if step == StepResult::Allocation {
                        // The engine already emitted the bounded memory-limit
                        // diagnostic; the resource-limit exit matches the
                        // video worker's contract (daemon: resource_limit).
                        exit(EXIT_MEMORY_DENIED);
                    }
                    self.render_keepalive(&last_pixels)?;
                }
            }
            deadline = now + interval;
        }
    }

    /// Re-publish the last pixels with a new sequence (the supervisor only
    /// watches sequence progression), or nothing if none exist yet.
    fn render_keepalive(&mut self, last_pixels: &Option<Vec<u8>>) -> Result<()> {
        if let Some(pixels) = last_pixels {
            self.published = self.writer.publish(pixels)?;
        }
        Ok(())
    }
}

// ---------------------------------------------------------------------------
// Entry point
// ---------------------------------------------------------------------------

fn print_probe_report(arguments: &Arguments) {
    let report = match ClearRenderer::probe(arguments.device.as_deref()) {
        Ok(report) => report,
        Err(error) => {
            eprintln!("event=renderer.probe.failed detail={error}");
            exit(EXIT_BACKEND_REJECT);
        }
    };
    let probe = serde_json::json!({
        "backend": "vulkan+quickjs",
        "device": report.device_name,
        "device_kind": report.device_kind,
        "scene_format": report.format,
        "quickjs": "0.12.2",
        "script_memory_limit_mib": js::MEMORY_LIMIT_BYTES / (1024 * 1024),
        "script_budget_ms": { "soft": 8, "hard": 33 },
    });
    println!("{probe}");
}

fn main() -> Result<()> {
    let arguments = Arguments::parse();
    if arguments.probe {
        print_probe_report(&arguments);
        return Ok(());
    }
    if arguments.memory_pressure_after.is_some() != arguments.memory_pressure_mib.is_some() {
        bail!("--memory-pressure-after and --memory-pressure-mib must be supplied together");
    }
    install_term_handler(arguments.ignore_term);
    let input = InputChannel::new()?;
    if arguments.startup_hang {
        eprintln!("event=renderer.fault kind=startup_hang");
        park_forever();
    }
    let output = arguments
        .output
        .clone()
        .context("--output is required (--probe excepted)")?;
    let content = arguments
        .content
        .clone()
        .context("--content is required (--probe excepted)")?;

    // 1. Scene: parse and reject (exit 73) anything the engine cannot render.
    let config = load_scene(&content);
    if let Some((w, h)) = config.resolution
        && (w, h) != (arguments.width, arguments.height)
    {
        eprintln!(
            "event=renderer.scene.resolution scene={w}x{h} requested={}x{}",
            arguments.width, arguments.height
        );
    }
    if let Some(scene_fps) = config.fps
        && (scene_fps - arguments.fps as f32).abs() > 0.5
    {
        eprintln!(
            "event=renderer.scene.fps scene={scene_fps} requested={}",
            arguments.fps
        );
    }

    // 2. Frame mapping.
    let spec = FrameSpec::new(arguments.width, arguments.height)?;
    let writer = SharedFrameWriter::create(&output, spec)
        .with_context(|| format!("create frame mapping {}", output.to_string_lossy()))?;

    // 3. Script engine: allocation failures exit 71 (resource limit), any
    //    other bootstrap failure is a backend rejection.
    let engine = match ScriptEngine::new(&config, spec.width, spec.height, arguments.fps) {
        Ok(engine) => engine,
        Err(EngineStartError::Allocation) => {
            eprintln!("event=renderer.scene.memory_limit phase=bootstrap fatal=1");
            eprintln!("event=renderer.scene.memory_limit exit_code={EXIT_MEMORY_DENIED}");
            exit(EXIT_MEMORY_DENIED);
        }
        Err(EngineStartError::Bootstrap(message)) => {
            eprintln!("event=renderer.scene.backend_reject detail={message}");
            eprintln!("event=renderer.scene.backend_reject exit_code={EXIT_BACKEND_REJECT}");
            exit(EXIT_BACKEND_REJECT);
        }
    };

    // 4. Vulkan compositor: an unusable backend is a rejection.
    let renderer = match ClearRenderer::new(arguments.device.as_deref(), spec.width, spec.height) {
        Ok(renderer) => renderer,
        Err(RenderError::Vulkan(message)) => {
            eprintln!("event=renderer.scene.backend_reject detail={message}");
            eprintln!("event=renderer.scene.backend_reject exit_code={EXIT_BACKEND_REJECT}");
            exit(EXIT_BACKEND_REJECT);
        }
        // Defensive: `new` performs no fence waits today, but a setup path
        // that ever does must reject the backend instead of inventing a
        // recovery (fence waits happen at the initial render below too).
        Err(RenderError::FenceTimeout) => {
            eprintln!(
                "event=renderer.scene.backend_reject detail=fence timeout during device setup"
            );
            eprintln!("event=renderer.scene.backend_reject exit_code={EXIT_BACKEND_REJECT}");
            exit(EXIT_BACKEND_REJECT);
        }
    };

    let mut worker = SceneWorker {
        arguments,
        spec,
        writer,
        input,
        engine,
        renderer,
        published: 0,
        consecutive_render_failures: 0,
    };
    worker.run()
}

/// A render failure the compositor cannot recover from: the device is not
/// producing frames, so the worker declares the backend unusable (exit 73).
fn reject_render(error: &RenderError, detail: &str) -> ! {
    eprintln!("event=renderer.scene.render_error consecutive=1 detail={error}");
    eprintln!("event=renderer.scene.backend_reject detail={detail}");
    eprintln!("event=renderer.scene.backend_reject exit_code={EXIT_BACKEND_REJECT}");
    exit(EXIT_BACKEND_REJECT);
}

/// Load the scene descriptor: a plain scene.json (M3a) or a scene.pkg
/// archive (M3b).
///
/// Packaged scenes are opened and validated by kwe-core's PkgReader, the
/// unique `scene.json` entry is parsed in memory (≤ 16 MiB), and — when
/// `general.script` names a package entry — that entry (≤ 2 MiB) is
/// extracted into a private `kwe-scene-script-<pid>` directory under the
/// worker's HOME (mode 0700; the daemon gives every worker its own 0700
/// HOME inside its runtime tree, so the extraction is removed with the
/// runtime). Textures and other assets are M3c+ and are deliberately not
/// extracted.
fn load_scene(content: &Path) -> SceneConfig {
    let is_pkg = content
        .extension()
        .and_then(|value| value.to_str())
        .is_some_and(|value| value.eq_ignore_ascii_case("pkg"));
    if !is_pkg {
        return match SceneConfig::parse(content) {
            Ok(config) => config,
            Err(error) => reject_scene(&error),
        };
    }

    let reader = match kwe_core::PkgReader::open(content) {
        Ok(reader) => reader,
        Err(error) => reject_pkg(error),
    };
    let scene_idx = match find_scene_entry(reader.entries()) {
        Ok(idx) => idx,
        Err(detail) => reject_pkg(detail),
    };
    let bytes = match reader.read_entry_bounded(scene_idx, scene::MAX_SCENE_JSON_BYTES) {
        Ok(bytes) => bytes,
        Err(error) => reject_pkg(error),
    };
    let mut config = match SceneConfig::parse_pkg(&bytes, reader.entries()) {
        Ok(config) => config,
        Err(error) => reject_scene(&error),
    };
    if let Some(script_idx) = config.script_entry {
        let script = match reader.read_entry_bounded(script_idx, scene::MAX_SCRIPT_BYTES) {
            Ok(script) => script,
            Err(error) => reject_pkg(error),
        };
        let extracted = match extract_script(&script) {
            Ok(path) => path,
            Err(error) => reject_pkg(format!("cannot extract script entry: {error}")),
        };
        config.script_path = Some(extracted);
    }
    eprintln!(
        "event=renderer.scene.pkg entries={} script_entry={}",
        reader.entries().len(),
        config.script_entry.is_some()
    );
    config
}

/// Locate the `scene.json` descriptor entry inside a package. Exactly one
/// is required (case-insensitive match on the entry name ending). No match
/// with a `scene.pkg` entry present means a nested archive, which M3b does
/// not support.
fn find_scene_entry(entries: &[kwe_core::PkgEntry]) -> Result<usize, String> {
    let matches: Vec<usize> = entries
        .iter()
        .enumerate()
        .filter(|(_, entry)| entry.path.to_ascii_lowercase().ends_with("scene.json"))
        .map(|(idx, _)| idx)
        .collect();
    match matches.as_slice() {
        [] => {
            let nested = entries
                .iter()
                .any(|entry| entry.path.to_ascii_lowercase().ends_with("scene.pkg"));
            if nested {
                Err("nested scene.pkg inside the package is not supported (M3b)".into())
            } else {
                Err("package has no scene.json entry".into())
            }
        }
        [idx] => Ok(*idx),
        _ => Err(format!(
            "package has {} scene.json entries; exactly one is required",
            matches.len()
        )),
    }
}

/// Write a script entry into a private 0700 directory under the worker's
/// HOME (falling back to the system temp dir when the daemon does not set
/// one), returning the extracted path. The pid-qualified directory keeps
/// concurrent workers from colliding.
fn extract_script(script: &[u8]) -> std::io::Result<PathBuf> {
    let home = std::env::var_os("HOME")
        .map(PathBuf::from)
        .unwrap_or_else(std::env::temp_dir);
    let dir = home.join(format!("kwe-scene-script-{}", std::process::id()));
    fs::DirBuilder::new().mode(0o700).create(&dir)?;
    let path = dir.join("script.js");
    let mut file = fs::OpenOptions::new()
        .write(true)
        .create_new(true)
        .mode(0o700)
        .open(&path)?;
    file.write_all(script)?;
    Ok(path)
}

/// Backend rejection for a packaged-scene problem (M3b): corrupt archive,
/// missing/ambiguous scene.json entry, nested pkg, unreadable or oversized
/// entries. The detail is bounded to a single line.
fn reject_pkg(detail: impl std::fmt::Display) -> ! {
    let detail = detail.to_string().replace(['\n', '\r'], " ");
    eprintln!("event=renderer.scene.backend_reject kind=Pkg detail={detail}");
    eprintln!("event=renderer.scene.backend_reject exit_code={EXIT_BACKEND_REJECT}");
    exit(EXIT_BACKEND_REJECT);
}

/// Backend rejection for anything the scene itself cannot be rendered from.
fn reject_scene(error: &SceneError) -> ! {
    eprintln!(
        "event=renderer.scene.backend_reject kind={:?} detail={}",
        error.kind, error.message
    );
    eprintln!("event=renderer.scene.backend_reject exit_code={EXIT_BACKEND_REJECT}");
    exit(EXIT_BACKEND_REJECT);
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn publish_decision_pure() {
        assert_eq!(
            publish_decision(StepResult::NewFrame([1.0, 0.0, 0.0, 1.0])),
            PublishDecision::NewFrame
        );
        assert_eq!(
            publish_decision(StepResult::SoftTimeout),
            PublishDecision::Keepalive
        );
        assert_eq!(
            publish_decision(StepResult::HardTimeout),
            PublishDecision::Keepalive
        );
        assert_eq!(
            publish_decision(StepResult::ScriptError),
            PublishDecision::Keepalive
        );
        // Allocation is a keepalive frame-wise, but the loop exits 71.
        assert_eq!(
            publish_decision(StepResult::Allocation),
            PublishDecision::Keepalive
        );
    }

    #[test]
    fn render_failure_streak_escalates() {
        assert!(!render_failure_fatal(0));
        assert!(!render_failure_fatal(MAX_CONSECUTIVE_RENDER_FAILURES - 1));
        assert!(render_failure_fatal(MAX_CONSECUTIVE_RENDER_FAILURES));
        assert!(render_failure_fatal(MAX_CONSECUTIVE_RENDER_FAILURES + 10));
    }
}
