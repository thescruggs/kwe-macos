// SPDX-License-Identifier: Apache-2.0
// kwe-scene-renderer: the M3a..M3c SceneScript worker (original
// implementation, ADR 0001). The daemon spawns it as:
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
// script's Engine.clearcolor is read back, the per-frame layer draw list is
// built from the script-mutated layer states (src/layers.rs), an offscreen
// Vulkan compositor clears to that color and draws the layers in scene.json
// order (src/vulkan.rs), and the readback is published premultiplied BGRA.
// M3c added 2D image layers: the scene's image references are resolved
// (relative to the content root, or through the package entry table for
// scene.pkg), decoded with bounded limits (src/textures.rs), and uploaded
// before the first render; a missing or over-budget image skips its layer
// with a bounded one-time diagnostic, never the scene. M3e added text
// layers: fonts are resolved from the standard system directories (plus
// --font-dir / KWE_FONT_DIRS for standalone lanes), glyphs rasterized into
// one bounded 2048x2048 atlas per text layer (src/text.rs), and each text
// layer draws as a textured quad over the same compositor path. Script
// exceptions never kill the renderer; a soft-timeout frame is skipped by
// re-publishing the last pixels (the supervisor only watches sequence
// progression). M3f added particle systems (src/particles.rs): a bounded
// deterministic CPU simulation (fixed 1/60 s steps, capped accumulators,
// documented emitter-model defaults) driven by the frame loop's real dt,
// with one batched draw per system through the same compositor (per-system
// host-visible vertex buffers, the system's blend-mode pipeline variant,
// and texture slots MAX_LAYERS + system_index).

mod js;
mod layers;
mod particles;
mod scene;
mod text;
mod textures;
mod video;
mod vulkan;

use std::cell::RefCell;
use std::fs;
use std::io::Write;
use std::os::unix::fs::{DirBuilderExt, OpenOptionsExt};
use std::path::{Component, Path, PathBuf};
use std::process::exit;
use std::rc::Rc;
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
use layers::{LayerState, MAX_LAYERS, frame_draws, merged_draws};
use particles::{MAX_PARTICLE_SYSTEMS, ParticleSystemState, particle_draws};
use scene::{SceneConfig, SceneError, read_bounded};
use text::{MAX_TEXT_LAYERS, TextRenderer};
use textures::{MAX_TEXTURE_SOURCE_BYTES, decode_texture, texture_budget_allows};
use vulkan::{LayerRenderer, RenderError, is_fence_timeout};

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
    /// Extra font directories consulted before the standard system font
    /// locations (repeatable). The daemon spawns workers with a fixed
    /// environment, so this is also read from KWE_FONT_DIRS (colon-
    /// separated) for standalone lanes.
    #[arg(long)]
    font_dir: Vec<PathBuf>,
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
    media: Option<video::MediaCommand>,
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
            media: None,
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
            Some("media_state") if decode_media_state(line).is_ok() => {
                if let Ok(state) = decode_media_state(line) {
                    self.media = media_command_for(&state.playback);
                }
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

    fn take_media(&mut self) -> Option<video::MediaCommand> {
        self.media.take()
    }
}

fn media_command_for(playback: &str) -> Option<video::MediaCommand> {
    match playback {
        "playing" => Some(video::MediaCommand::Play),
        "paused" => Some(video::MediaCommand::Pause),
        "stopped" => Some(video::MediaCommand::Stop),
        _ => None,
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
    renderer: LayerRenderer,
    /// M3e: font resolution, glyph atlas, and per-layer text quad geometry.
    text: TextRenderer,
    /// The script-visible layer states, index-aligned with the scene's
    /// `objects` array and with `texture_ok`.
    layers: Vec<Rc<RefCell<LayerState>>>,
    /// Per layer: whether its texture uploaded (or it has no image at all —
    /// a model/particle object). frame_draws skips false entries.
    texture_ok: Vec<bool>,
    /// M3f: the script-visible particle-system states, index-aligned with
    /// the scene's `objects` array (particle entries) and with
    /// `particle_texture_ok`. Simulated every frame (particles.rs) with
    /// the same dt the script's update() ran under.
    particles: Vec<Rc<RefCell<ParticleSystemState>>>,
    /// M3f: per system, whether its texture uploaded (a system without a
    /// texture draws nothing — the descriptor set only exists after a
    /// successful upload, see vulkan.rs). particle_draws skips false
    /// entries.
    particle_texture_ok: Vec<bool>,
    /// M3g: the open video decoders, each paired with the layer index it
    /// feeds. At most video::MAX_VIDEO_LAYERS entries — a scene without
    /// video carries none, and the frame loop's sync_videos is then a
    /// no-op over an empty slice.
    videos: Vec<(usize, video::VideoDecoder)>,
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
        // M3e: text layers are synced before every frame's draw list, so
        // the first published frame carries any static scene text too.
        // M3f: particle systems are seeded with one pacing interval of
        // simulation, so the first published frame already shows particles
        // (the burst a script queued in init() spawns on this first step).
        if let Err(error) = self.sync_text() {
            reject_render(&error, "fence timeout during text upload");
        }
        self.sync_particles(interval.as_secs_f64());
        self.sync_videos();
        // M3f draw order: the layer and particle lists are merged by the
        // scene.json objects-array order, so a particle system and an
        // image interleave exactly as the file says (an image listed after
        // a particle system draws on top of it).
        let draws = merged_draws(
            frame_draws(&self.layers, &self.texture_ok),
            particle_draws(&self.particles, &self.particle_texture_ok),
        );
        let initial = match self.renderer.render(self.engine.clear_color(), &draws) {
            Ok(pixels) => pixels,
            Err(error) => reject_render(&error, "initial render failure"),
        };
        self.published = self.writer.publish(&initial)?;
        let mut last_pixels: Option<Vec<u8>> = Some(initial);
        loop {
            self.input.poll();
            if let Some(command) = self.input.take_media() {
                for (index, decoder) in &mut self.videos {
                    if let Err(error) = decoder.apply_media(command) {
                        decoder.disable();
                        eprintln!(
                            "event=renderer.scene.layer_skip layer={} detail=video-media-failed: {error}",
                            self.layers[*index].borrow().name
                        );
                    }
                }
            }
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
                    // The draw list is rebuilt per frame: the script may
                    // have mutated layer alpha, position, size, visibility,
                    // or rotation since the last step. M3e: dirty text
                    // layers (text/pointsize/alignment/color changed by the
                    // script) are re-rasterized here — geometry is rebuilt
                    // on change only, never per frame. M3f: particle
                    // systems are simulated with the same dt the script's
                    // update() ran under, and their per-frame vertex
                    // buffers are rebuilt whenever a fixed step ran.
                    if let Err(error) = self.sync_text() {
                        reject_render(&error, "fence timeout during text upload");
                    }
                    self.sync_particles(dt);
                    self.sync_videos();
                    let draws = merged_draws(
                        frame_draws(&self.layers, &self.texture_ok),
                        particle_draws(&self.particles, &self.particle_texture_ok),
                    );
                    match self.renderer.render(color, &draws) {
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

    /// M3e: re-sync every dirty text layer — resolve its font, relayout,
    /// rasterize missing glyphs into the layer's bounded atlas, and upload
    /// atlas + quad geometry. A failed upload marks the layer as not
    /// drawable this frame, exactly like an image layer; a hostile text can
    /// only degrade rendering, never the scene. All diagnostics are bounded
    /// one-time eprintlns (text.rs).
    fn sync_text(&mut self) -> std::result::Result<(), RenderError> {
        let failed = self
            .text
            .sync_and_upload(&mut self.renderer, &self.layers)?;
        for index in failed {
            self.texture_ok[index] = false;
            eprintln!(
                "event=renderer.scene.layer_skip layer={} detail=text-upload-failed",
                self.layers[index].borrow().name
            );
        }
        Ok(())
    }

    /// M3f: step every particle system with the frame's dt (the same dt
    /// the script's update() ran under — a script-visible write and its
    /// simulation land in the same frame). When a fixed step ran, the
    /// system's vertex bytes are rebuilt and uploaded (bounded: at most
    /// MAX_PARTICLES × 6 × 40 B per system, create-or-grow buffers). A
    /// failed vertex upload marks the system as not drawable this frame,
    /// exactly like a failed texture; a hostile system can only degrade
    /// rendering, never the scene.
    fn sync_particles(&mut self, dt: f64) {
        // One scratch Vec reused across systems AND frames: build_vertex_bytes
        // clears and refills it, and the upload copies it into the system's
        // own host-visible buffer — so a frame with steps allocates only
        // when a system grows past its previous high-water mark.
        let mut scratch = Vec::new();
        for (index, system) in self.particles.iter().enumerate() {
            let mut system = system.borrow_mut();
            if !system.simulate(dt) {
                continue;
            }
            let vertex_count = system.build_vertex_bytes(&mut scratch);
            system.vertex_count = vertex_count;
            if vertex_count == 0 {
                continue; // all aged out: nothing to upload, no draw
            }
            if let Err(error) = self.renderer.upload_particle_vertices(index, &scratch) {
                if is_fence_timeout(&error) {
                    reject_render(&error, "fence timeout during particle vertex upload");
                }
                self.particle_texture_ok[index] = false;
                eprintln!(
                    "event=renderer.scene.particle_skip system={} detail=vertex-upload-failed: {error}",
                    system.name
                );
            }
        }
    }

    /// M3g: pull one frame from each open decoder and refresh its layer
    /// texture in place. `poll_frame` returns None when libmpv has nothing
    /// new this tick (the common case at a pacing rate above the video's
    /// frame rate) or when the decoder has failed — either way the layer
    /// keeps its current texture, so a stalled or dead video freezes on its
    /// last frame instead of disappearing.
    ///
    /// `refresh_layer` reuses the image and descriptor set as long as the
    /// dimensions match, which they always do here (the decoder's size is
    /// fixed at open). A failed refresh disables only future decoder polls;
    /// the already-uploaded last good texture remains drawable.
    fn sync_videos(&mut self) {
        // Disjoint field borrows: poll_frame borrows the decoder for the
        // life of the returned slice, so the renderer and the texture table
        // have to be reborrowed outside the loop.
        let renderer = &mut self.renderer;
        let layers = &self.layers;
        for (index, decoder) in self.videos.iter_mut() {
            let index = *index;
            let (width, height) = (decoder.width(), decoder.height());
            let Some(frame) = decoder.poll_frame() else {
                continue;
            };
            if let Err(error) = renderer.refresh_layer(index, frame, width, height) {
                if is_fence_timeout(&error) {
                    reject_render(&error, "fence timeout during video refresh");
                }
                decoder.disable();
                eprintln!(
                    "event=renderer.scene.layer_skip layer={} detail=video-refresh-failed: {error}",
                    layers[index].borrow().name
                );
            }
        }
    }
}

// ---------------------------------------------------------------------------
// Entry point
// ---------------------------------------------------------------------------

fn print_probe_report(arguments: &Arguments) {
    let report = match LayerRenderer::probe(arguments.device.as_deref()) {
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
    let mut config = load_scene(&content);
    // 1b. M3g: open the video decoders before the script engine, so a layer
    //     that declared size [0, 0] carries the video's own dimensions by
    //     the time init() reads it (the same rule image layers follow).
    let mut videos = open_video_layers(&mut config.layers);
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
    let mut renderer =
        match LayerRenderer::new(arguments.device.as_deref(), spec.width, spec.height) {
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

    // 5. Layer textures: upload every decoded layer texture before the first
    //    render (all uploads share the startup fence, which is waited to
    //    completion per upload). A failed upload skips the layer with a
    //    bounded one-time diagnostic — the renderer stays healthy.
    let layers = engine.layers();
    let mut texture_ok = upload_layer_textures(&mut renderer, &config.layers, &layers);
    upload_video_first_frames(&mut renderer, &config.layers, &mut videos, &mut texture_ok);

    // 5b. M3f: particle-system textures (slot MAX_LAYERS + system_index).
    let particles = engine.particles();
    let particle_texture_ok =
        upload_particle_textures(&mut renderer, &config.particles, particles.len());

    // 6. M3e text subsystem: font directories come from --font-dir (the
    //    daemon spawns workers with a fixed environment, so standalone
    //    lanes use the flag) plus KWE_FONT_DIRS (colon-separated, for
    //    integration tests). Load-time diagnostics are bounded one-time
    //    eprintlns; a hostile scene only degrades text rendering.
    let mut font_dirs = arguments.font_dir.clone();
    if let Some(dirs) = std::env::var_os("KWE_FONT_DIRS") {
        font_dirs.extend(std::env::split_paths(&dirs));
    }
    let text = TextRenderer::new(&font_dirs);
    eprintln!(
        "event=renderer.scene.text fonts={} layers={}",
        text.font_file_count(),
        layers
            .iter()
            .filter(|layer| layer.borrow().text.is_some())
            .count()
    );
    if config.text_layer_skips > 0 {
        eprintln!(
            "event=renderer.scene.text_layer_skip count={} (cap is {MAX_TEXT_LAYERS})",
            config.text_layer_skips
        );
    }
    if config.text_on_image_objects > 0 {
        eprintln!(
            "event=renderer.scene.text_on_image_objects count={} (text on image objects is ignored)",
            config.text_on_image_objects
        );
    }
    if config.text_size_ignored > 0 {
        eprintln!(
            "event=renderer.scene.text_size_ignored count={} (text layers pin size=1x1)",
            config.text_size_ignored
        );
    }
    if config.particle_system_skips > 0 {
        eprintln!(
            "event=renderer.scene.particle_system_skip count={} (cap is {MAX_PARTICLE_SYSTEMS})",
            config.particle_system_skips
        );
    }
    if config.video_layer_skips > 0 {
        eprintln!(
            "event=renderer.scene.video_layer_skip count={} (cap is {})",
            config.video_layer_skips,
            video::MAX_VIDEO_LAYERS
        );
    }
    if config.particle_file_refs > 0 {
        eprintln!(
            "event=renderer.scene.particle_file_ref count={} (external particle files are planned; defaults used)",
            config.particle_file_refs
        );
    }

    let mut worker = SceneWorker {
        arguments,
        spec,
        writer,
        input,
        engine,
        renderer,
        text,
        layers,
        texture_ok,
        particles,
        particle_texture_ok,
        videos,
        published: 0,
        consecutive_render_failures: 0,
    };
    let run_result = worker.run();
    // Release libmpv render contexts before removing package-backed files;
    // the decoder's teardown is ordered render-context -> mpv handle.
    drop(worker);
    // M3b review follow-up: the worker removes its own extracted script
    // directory on the graceful exit path (it owns its HOME).
    cleanup_script_dir(config.script_path.as_deref());
    // M3g: the extracted video directory is the worker's too (pkg lane only;
    // a file scene never creates one, and the cleanup is a no-op then).
    cleanup_video_dir();
    run_result
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
/// archive (M3b), then resolve and decode its layer images (M3c).
///
/// Packaged scenes are opened and validated by kwe-core's PkgReader, the
/// unique `scene.json` entry (exact basename, case-insensitive, at most
/// one leading directory component — see kwe-core `scene_json_entry`) is
/// parsed in memory (≤ 16 MiB), and — when `general.script` names a
/// package entry — that entry (≤ 2 MiB) is extracted into a private
/// `kwe-scene-script-<pid>` directory under the worker's HOME (mode 0700;
/// the daemon gives every worker its own private 0700 HOME). The worker
/// removes the directory on its graceful exit path (cleanup_script_dir);
/// a stale directory left by a hard kill is replaced by extract_script's
/// pid-recycle retry, so a restarted worker with a recycled pid never
/// bounces on AlreadyExists.
///
/// M3c: after parsing, every image layer's reference is resolved against
/// the same root the script uses — the canonicalized content directory
/// (file scenes) or the package entry table (pkg scenes) — then decoded
/// with bounded limits (textures.rs). A missing, escaping, unreadable, or
/// over-budget image skips its layer with a bounded one-time diagnostic;
/// only the descriptor and script problems above reject the scene.
fn load_scene(content: &Path) -> SceneConfig {
    let is_pkg = content
        .extension()
        .and_then(|value| value.to_str())
        .is_some_and(|value| value.eq_ignore_ascii_case("pkg"));
    if !is_pkg {
        let mut config = match SceneConfig::parse(content) {
            Ok(config) => config,
            Err(error) => reject_scene(&error),
        };
        let root = match scene::canonical_root(content) {
            Ok(root) => root,
            Err(error) => reject_scene(&error),
        };
        let mut used_bytes = load_layer_textures(&mut config.layers, |reference| {
            resolve_layer_image(&root, reference)
        });
        // M3f: particle-system textures share the same budget, so a
        // texture-heavy scene degrades the same way for both kinds.
        load_particle_textures(&mut config.particles, &mut used_bytes, |reference| {
            resolve_layer_image(&root, reference)
        });
        // M3g: video layers resolve to a path inside the same root; the
        // file is never read here (libmpv opens it).
        load_layer_videos(&mut config.layers, |reference| {
            resolve_layer_video(&root, reference)
        });
        return config;
    }

    let reader = match kwe_core::PkgReader::open(content) {
        Ok(reader) => reader,
        Err(error) => reject_pkg(error),
    };
    // The descriptor-location rule lives in kwe-core (shared with pkg
    // preflight so both agree on what the descriptor entry is).
    let scene_idx = match kwe_core::scene_json_entry(reader.entries()) {
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
    // Image references resolve against the package entry table, never the
    // host file system; entries were already validated at package open.
    let mut used_bytes = load_layer_textures(&mut config.layers, |reference| {
        let index = kwe_core::image_entry(reference, reader.entries())?;
        reader
            .read_entry_bounded(index, MAX_TEXTURE_SOURCE_BYTES)
            .map_err(|error| format!("cannot read entry: {error}"))
    });
    // M3f: particle-system textures resolve the same way (the shared
    // budget carries over from the layer textures above).
    load_particle_textures(&mut config.particles, &mut used_bytes, |reference| {
        let index = kwe_core::image_entry(reference, reader.entries())?;
        reader
            .read_entry_bounded(index, MAX_TEXTURE_SOURCE_BYTES)
            .map_err(|error| format!("cannot read entry: {error}"))
    });
    // M3g: a packaged video is extracted into the worker's private HOME,
    // because libmpv opens a path rather than a byte slice. Bounded by
    // MAX_VIDEO_SOURCE_BYTES and by the concurrency cap the parse enforced.
    let mut video_slot = 0usize;
    load_layer_videos(&mut config.layers, |reference| {
        if !kwe_core::video_extension_allowed(reference) {
            return Err(format!(
                "video \"{reference}\" has an unsupported container extension"
            ));
        }
        let index = kwe_core::video_entry(reference, reader.entries())?;
        let bytes = reader
            .read_entry_bounded(index, video::MAX_VIDEO_SOURCE_BYTES)
            .map_err(|error| format!("cannot read entry: {error}"))?;
        let slot = video_slot;
        video_slot += 1;
        extract_video(slot, &bytes).map_err(|error| format!("cannot extract video entry: {error}"))
    });
    eprintln!(
        "event=renderer.scene.pkg entries={} script_entry={}",
        reader.entries().len(),
        config.script_entry.is_some()
    );
    config
}

/// Resolve one layer's image reference against the content root (file
/// scenes): relative, no `..`/absolute components, canonicalized inside the
/// root (so symlinks cannot smuggle the image out), a regular file, at most
/// MAX_TEXTURE_SOURCE_BYTES. Mirrors resolve_script's checks, but a failure
/// is a `Err(detail)` the caller logs once and skips the layer over — an
/// image problem never rejects the scene.
fn resolve_layer_image(root: &Path, reference: &str) -> Result<Vec<u8>, String> {
    if reference.is_empty() {
        return Err("image reference must not be empty".into());
    }
    let joined = Path::new(reference);
    if joined.is_absolute() {
        return Err(format!(
            "image \"{reference}\" must be relative to the scene directory"
        ));
    }
    for component in joined.components() {
        if matches!(
            component,
            Component::ParentDir | Component::RootDir | Component::Prefix(_)
        ) {
            return Err(format!(
                "image \"{reference}\" must stay inside the scene directory"
            ));
        }
    }
    let candidate = root.join(joined);
    let canonical = candidate
        .canonicalize()
        .map_err(|error| format!("image \"{reference}\" is missing or unreadable: {error}"))?;
    if !canonical.starts_with(root) {
        return Err(format!(
            "image \"{reference}\" resolves outside the scene directory"
        ));
    }
    if !canonical.is_file() {
        return Err(format!("image \"{reference}\" is not a regular file"));
    }
    read_bounded(&canonical, MAX_TEXTURE_SOURCE_BYTES)
        .map_err(|error| format!("read image \"{reference}\": {}", error.message))
}

/// Fill `layers[*].texture` for every image layer: resolve the
/// reference (the caller's closure is lane-specific — file system or
/// package), decode within the bounded limits, and account the decoded
/// bytes against the total texture budget. A layer whose image is absent
/// (non-string `image` field), unresolved, undecodable, or over budget is
/// skipped with a bounded one-time diagnostic and stays registered but
/// textureless; a layer with `size` [0, 0] takes the texture's decoded
/// dimensions (WE semantics: absent size = the image's own size), so the
/// script's init() always sees the real size. A `colorBlendMode` outside
/// the implemented fixed-function set (the researched corpus values
/// 11/12/24/30 — see docs/SCENE_FORMAT_V1.md) is clamped to normal at the
/// layer boundary and noted ONCE per scene; unknown values are tolerated
/// silently (the M3c behavior, still src-over).
fn load_layer_textures(
    layers: &mut [scene::LayerSpec],
    mut resolve: impl FnMut(&str) -> Result<Vec<u8>, String>,
) -> u64 {
    let mut used_bytes: u64 = 0;
    let mut blend_diag_emitted = false;
    for layer in layers {
        let Some(reference) = layer.image.as_deref() else {
            continue; // no image reference (model/particle objects or a
            // non-string image field): nothing to load
        };
        if !blend_diag_emitted
            && crate::layers::BLEND_MODE_UNIMPLEMENTED.contains(&layer.blend_mode)
        {
            blend_diag_emitted = true;
            eprintln!(
                "event=renderer.scene.blend_mode_clamped layer={} mode={} note=not-fixed-function-clamped-to-normal",
                layer.name, layer.blend_mode
            );
        }
        let bytes = match resolve(reference) {
            Ok(bytes) => bytes,
            Err(detail) => {
                eprintln!(
                    "event=renderer.scene.layer_skip layer={} detail={detail}",
                    layer.name
                );
                continue;
            }
        };
        let Some(texture) = decode_texture(&bytes) else {
            eprintln!(
                "event=renderer.scene.layer_skip layer={} detail=undecodable-or-over-budget",
                layer.name
            );
            continue;
        };
        let pixels = u64::from(texture.width) * u64::from(texture.height);
        if !texture_budget_allows(used_bytes, texture.width, texture.height) {
            eprintln!(
                "event=renderer.scene.layer_skip layer={} detail=total-texture-budget",
                layer.name
            );
            continue;
        }
        used_bytes = used_bytes.saturating_add(pixels.saturating_mul(4));
        if layer.size == [0.0, 0.0] {
            layer.size = [texture.width as f32, texture.height as f32];
        }
        layer.texture = Some(texture);
    }
    used_bytes
}

/// M3f: fill `particles[*].texture` for every system with a texture
/// reference (the raw reference lives in the `material` slot; `texture`
/// won over WE's `material` at parse). The reference resolves exactly like
/// a layer image — the caller's closure is lane-specific (file system or
/// package), decode is bounded, and the decoded bytes count against the
/// shared texture budget carried in `used_bytes`. A system whose texture
/// is absent, unresolved, undecodable, or over budget keeps its defaults
/// with a bounded one-time diagnostic and renders nothing (the compositor
/// only allocates a descriptor set on a successful upload, see
/// upload_particle_textures); a texture problem never rejects the scene.
fn load_particle_textures(
    particles: &mut [scene::ParticleSpec],
    used_bytes: &mut u64,
    mut resolve: impl FnMut(&str) -> Result<Vec<u8>, String>,
) {
    for particle in particles {
        let Some(reference) = particle.material.as_deref() else {
            continue; // no texture reference: the system draws nothing
        };
        let bytes = match resolve(reference) {
            Ok(bytes) => bytes,
            Err(detail) => {
                eprintln!(
                    "event=renderer.scene.particle_skip system={} detail={detail}",
                    particle.name
                );
                continue;
            }
        };
        let Some(texture) = decode_texture(&bytes) else {
            eprintln!(
                "event=renderer.scene.particle_skip system={} detail=undecodable-or-over-budget",
                particle.name
            );
            continue;
        };
        let pixels = u64::from(texture.width) * u64::from(texture.height);
        if !texture_budget_allows(*used_bytes, texture.width, texture.height) {
            eprintln!(
                "event=renderer.scene.particle_skip system={} detail=total-texture-budget",
                particle.name
            );
            continue;
        }
        *used_bytes = used_bytes.saturating_add(pixels.saturating_mul(4));
        particle.texture = Some(texture);
    }
}

/// Upload the decoded layer textures into the compositor. Index-aligned
/// with `config.layers` and `engine.layers()` (the scene's `objects`
/// order). Returns the per-layer `texture_ok` table frame_draws consults:
/// true when the layer uploaded or has no image at all, false when a decode
/// or upload failed (the layer then draws nothing; the renderer stays
/// healthy). Upload failures are bounded one-time diagnostics.
fn upload_layer_textures(
    renderer: &mut LayerRenderer,
    config_layers: &[scene::LayerSpec],
    layers: &[Rc<RefCell<LayerState>>],
) -> Vec<bool> {
    let mut texture_ok = vec![true; layers.len()];
    for (index, layer) in config_layers.iter().enumerate() {
        let Some(texture) = &layer.texture else {
            continue; // skipped at load or no image reference
        };
        match renderer.upload_layer(index, &texture.rgba, texture.width, texture.height) {
            Ok(()) => {}
            Err(error) => {
                if is_fence_timeout(&error) {
                    reject_render(&error, "fence timeout during layer texture upload");
                }
                texture_ok[index] = false;
                eprintln!(
                    "event=renderer.scene.layer_skip layer={} detail=upload-failed: {error}",
                    layer.name
                );
            }
        }
    }
    texture_ok
}

/// M3f: upload the decoded particle-system textures into the compositor at
/// slot MAX_LAYERS + system_index (vulkan.rs TEXTURE_SLOT_COUNT), so a
/// system's texture can never collide with a layer's. Index-aligned with
/// `config.particles` and `engine.particles()`. Returns the per-system
/// `texture_ok` table particle_draws consults: true only when the system's
/// texture uploaded — a system without a texture (or with a failed
/// upload) draws nothing, exactly like a layer without an image. Upload
/// failures are bounded one-time diagnostics.
fn upload_particle_textures(
    renderer: &mut LayerRenderer,
    config_particles: &[scene::ParticleSpec],
    particle_count: usize,
) -> Vec<bool> {
    let mut texture_ok = vec![false; particle_count];
    for (index, particle) in config_particles.iter().enumerate() {
        let Some(texture) = &particle.texture else {
            continue; // no texture loaded: the system draws nothing
        };
        match renderer.upload_layer(
            MAX_LAYERS + index,
            &texture.rgba,
            texture.width,
            texture.height,
        ) {
            Ok(()) => texture_ok[index] = true,
            Err(error) => {
                if is_fence_timeout(&error) {
                    reject_render(&error, "fence timeout during particle texture upload");
                }
                eprintln!(
                    "event=renderer.scene.particle_skip system={} detail=upload-failed: {error}",
                    particle.name
                );
            }
        }
    }
    texture_ok
}

/// M3g: fill `layers[*].video.path` for every video layer — resolve the
/// reference the same way an image resolves (the caller's closure is
/// lane-specific: a path inside the content root for file scenes, an
/// extracted copy of the package entry for pkg scenes). libmpv opens a
/// path, not a byte slice, which is the one asymmetry with images: a
/// package-embedded video is written into the worker's private HOME first.
///
/// A layer whose source is absent (non-string `video`, or over the
/// concurrency cap — parse already cleared those), unresolved, or too
/// large is skipped with a bounded one-time diagnostic and stays
/// registered but textureless; a video problem never rejects the scene.
fn load_layer_videos(
    layers: &mut [scene::LayerSpec],
    mut resolve: impl FnMut(&str) -> Result<PathBuf, String>,
) {
    for layer in layers {
        let name = layer.name.clone();
        let Some(spec) = layer.video.as_mut() else {
            continue; // not a video layer
        };
        let Some(reference) = spec.source.clone() else {
            eprintln!(
                "event=renderer.scene.layer_skip layer={name} detail=video-source-unavailable"
            );
            continue;
        };
        match resolve(&reference) {
            Ok(path) => spec.path = Some(path),
            Err(detail) => {
                eprintln!("event=renderer.scene.layer_skip layer={name} detail={detail}");
            }
        }
    }
}

/// Resolve one layer's video reference against the content root (file
/// scenes). The same containment rules as `resolve_layer_image` —
/// relative, no `..`/absolute components, canonicalized inside the root so
/// a symlink cannot smuggle the source out, a regular file — but the file
/// is never read: libmpv opens the path itself, so the size cap is checked
/// from the metadata instead of from a bounded read.
fn resolve_layer_video(root: &Path, reference: &str) -> Result<PathBuf, String> {
    if reference.is_empty() {
        return Err("video reference must not be empty".into());
    }
    if !kwe_core::video_extension_allowed(reference) {
        return Err(format!(
            "video \"{reference}\" has an unsupported container extension"
        ));
    }
    let joined = Path::new(reference);
    if joined.is_absolute() {
        return Err(format!(
            "video \"{reference}\" must be relative to the scene directory"
        ));
    }
    for component in joined.components() {
        if matches!(
            component,
            Component::ParentDir | Component::RootDir | Component::Prefix(_)
        ) {
            return Err(format!(
                "video \"{reference}\" must stay inside the scene directory"
            ));
        }
    }
    let canonical = root
        .join(joined)
        .canonicalize()
        .map_err(|error| format!("video \"{reference}\" is missing or unreadable: {error}"))?;
    if !canonical.starts_with(root) {
        return Err(format!(
            "video \"{reference}\" resolves outside the scene directory"
        ));
    }
    let metadata = canonical
        .metadata()
        .map_err(|error| format!("video \"{reference}\" is unreadable: {error}"))?;
    if !metadata.is_file() {
        return Err(format!("video \"{reference}\" is not a regular file"));
    }
    if metadata.len() > video::MAX_VIDEO_SOURCE_BYTES {
        return Err(format!(
            "video \"{reference}\" is {} bytes, over the {} byte cap",
            metadata.len(),
            video::MAX_VIDEO_SOURCE_BYTES
        ));
    }
    Ok(canonical)
}

/// Write one package video entry into a private 0700 directory under the
/// worker's HOME, mirroring `extract_script`. Videos are extracted rather
/// than decoded in memory because libmpv opens a path; the directory is
/// removed on the graceful exit path (`cleanup_video_dir`), and a stale
/// one left by a hard kill is reused after its files are replaced.
fn extract_video(slot: usize, bytes: &[u8]) -> std::io::Result<PathBuf> {
    let home = std::env::var_os("HOME")
        .map(PathBuf::from)
        .unwrap_or_else(std::env::temp_dir);
    extract_video_into(&home, slot, bytes)
}

/// The extraction core, testable against a caller-chosen HOME. Unlike
/// `extract_script_into` the directory is reused across calls (a scene may
/// carry MAX_VIDEO_LAYERS videos), so staleness is handled per file: the
/// old entry is unlinked before the exclusive create. `remove_file` unlinks
/// a symlink itself rather than following it, so a planted link cannot
/// redirect the write.
fn extract_video_into(home: &Path, slot: usize, bytes: &[u8]) -> std::io::Result<PathBuf> {
    let dir = home.join(format!("kwe-scene-video-{}", std::process::id()));
    match fs::symlink_metadata(&dir) {
        Ok(meta) if meta.is_dir() && !meta.file_type().is_symlink() => {}
        Ok(_) => {
            return Err(std::io::Error::new(
                std::io::ErrorKind::InvalidData,
                "stale video directory is not a plain directory",
            ));
        }
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
            fs::DirBuilder::new().mode(0o700).create(&dir)?;
        }
        Err(error) => return Err(error),
    }
    let path = dir.join(format!("video-{slot}.bin"));
    let _ = fs::remove_file(&path);
    let mut file = fs::OpenOptions::new()
        .write(true)
        .create_new(true)
        .mode(0o600)
        .open(&path)?;
    file.write_all(bytes)?;
    Ok(path)
}

/// Remove the extracted video directory on the worker's graceful exit path
/// (the worker owns its HOME). Unconditional: the directory only exists
/// when a packaged scene carried a video, and a missing one is not an
/// error. A kill -9 leaves it behind; the next start with a recycled pid
/// replaces the files inside it.
fn cleanup_video_dir() {
    let home = std::env::var_os("HOME")
        .map(PathBuf::from)
        .unwrap_or_else(std::env::temp_dir);
    cleanup_video_dir_in(&home);
}

/// The cleanup core, testable against a caller-chosen HOME (the same split
/// `extract_video_into` uses). Name-guarded and symlink-guarded: only a
/// plain directory named `kwe-scene-video-<our pid>` is removed, so a
/// planted symlink of that name is left where it is rather than followed.
fn cleanup_video_dir_in(home: &Path) {
    let dir = home.join(format!("kwe-scene-video-{}", std::process::id()));
    match fs::symlink_metadata(&dir) {
        Ok(meta) if meta.is_dir() && !meta.file_type().is_symlink() => {}
        _ => return, // nothing we created
    }
    match fs::remove_dir_all(&dir) {
        Ok(()) => eprintln!(
            "event=renderer.scene.video_dir_cleanup path={}",
            dir.display()
        ),
        Err(error) => eprintln!("event=renderer.scene.video_dir_cleanup error={error}"),
    }
}

/// M3g: open a decoder for every video layer whose source resolved, and
/// give the layer the video's own dimensions when the scene declared
/// `size` [0, 0] (the same WE semantics image layers get from their
/// decoded texture). This runs before the script engine is built, so
/// `init()` always sees the real size.
///
/// At most `video::MAX_VIDEO_LAYERS` decoders open: the parse already
/// cleared the source of every layer past the cap, and the defensive break
/// here means a future parse change cannot silently uncap concurrency. A
/// decoder that fails to open skips its layer with a bounded diagnostic —
/// a broken video degrades one layer, never the scene.
fn open_video_layers(layers: &mut [scene::LayerSpec]) -> Vec<(usize, video::VideoDecoder)> {
    let mut decoders = Vec::new();
    for (index, layer) in layers.iter_mut().enumerate() {
        let Some(spec) = layer.video.as_ref() else {
            continue;
        };
        let Some(path) = spec.path.clone() else {
            continue; // no source, or it did not resolve (already diagnosed)
        };
        if decoders.len() >= video::MAX_VIDEO_LAYERS {
            eprintln!(
                "event=renderer.scene.layer_skip layer={} detail=video-concurrency-cap",
                layer.name
            );
            continue;
        }
        let (loop_playback, rate) = (spec.loop_playback, spec.rate);
        match video::VideoDecoder::open(&path, loop_playback, rate) {
            Ok(decoder) => {
                if layer.size == [0.0, 0.0] {
                    layer.size = [decoder.width() as f32, decoder.height() as f32];
                }
                eprintln!(
                    "event=renderer.scene.video_open layer={} size={}x{} loop={loop_playback} rate={rate}",
                    layer.name,
                    decoder.width(),
                    decoder.height()
                );
                decoders.push((index, decoder));
            }
            Err(detail) => {
                eprintln!(
                    "event=renderer.scene.layer_skip layer={} detail=video-open-failed: {detail}",
                    layer.name
                );
            }
        }
    }
    decoders
}

/// M3g: upload each decoder's current frame once before the first render,
/// so the layer owns an image and a descriptor set that `refresh_layer`
/// can then update in place at frame rate. Video layers with no decoder
/// are marked not drawable: `frame_draws` would otherwise emit a draw the
/// compositor silently discards for want of a descriptor set.
fn upload_video_first_frames(
    renderer: &mut LayerRenderer,
    config_layers: &[scene::LayerSpec],
    videos: &mut [(usize, video::VideoDecoder)],
    texture_ok: &mut [bool],
) {
    for (index, layer) in config_layers.iter().enumerate() {
        if layer.video.is_some() && !videos.iter().any(|(slot, _)| *slot == index) {
            texture_ok[index] = false;
        }
    }
    for (index, decoder) in videos.iter_mut() {
        // Best effort: a decoder that has not produced a frame yet uploads
        // its zero-filled buffer, which is transparent black under the
        // layer's blend — the next sync_videos replaces it in place.
        let _ = decoder.poll_frame();
        // A decoder that already failed will never produce a frame, so it
        // gets no image and no descriptor set: allocating a texture slot
        // for it would waste the bounded pool on a layer that draws
        // nothing. The decoder emitted its own one-time video_error.
        if decoder.failed() {
            texture_ok[*index] = false;
            eprintln!(
                "event=renderer.scene.layer_skip layer={} detail=video-failed-before-first-frame",
                config_layers[*index].name
            );
            continue;
        }
        let (width, height) = (decoder.width(), decoder.height());
        match renderer.upload_layer(*index, decoder.frame(), width, height) {
            Ok(()) => texture_ok[*index] = true,
            Err(error) => {
                if is_fence_timeout(&error) {
                    reject_render(&error, "fence timeout during video texture upload");
                }
                texture_ok[*index] = false;
                decoder.disable();
                eprintln!(
                    "event=renderer.scene.layer_skip layer={} detail=video-upload-failed: {error}",
                    config_layers[*index].name
                );
            }
        }
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
    extract_script_into(&home, script)
}

/// The extraction core, testable against a caller-chosen HOME. A stale
/// `kwe-scene-script-<pid>` directory is replaced rather than refused
/// (M3b review follow-up: a daemon restart can recycle a pid, and an
/// AlreadyExists on the pid directory would bounce a valid scene at exit
/// 73). The directory is the worker's own — inside its private 0700 HOME
/// — so removing a stale plain directory is safe; a stale symlink is
/// refused instead of followed.
fn extract_script_into(home: &Path, script: &[u8]) -> std::io::Result<PathBuf> {
    let dir = home.join(format!("kwe-scene-script-{}", std::process::id()));
    match fs::symlink_metadata(&dir) {
        Ok(meta) if meta.is_dir() && !meta.file_type().is_symlink() => {
            fs::remove_dir_all(&dir)?;
        }
        Ok(_) => {
            return Err(std::io::Error::new(
                std::io::ErrorKind::InvalidData,
                "stale script directory is not a plain directory",
            ));
        }
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => {}
        Err(error) => return Err(error),
    }
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

/// Remove the extracted script directory on the worker's graceful exit
/// path (the worker owns its HOME). A kill -9 leaves the directory behind;
/// extract_script's pid-recycle retry replaces it on the next start, so a
/// leftover is never a brick.
fn cleanup_script_dir(script_path: Option<&Path>) {
    let Some(script_path) = script_path else {
        return;
    };
    let Some(dir) = script_path.parent() else {
        return;
    };
    // Defensive: only ever remove what extract_script created.
    let name = dir.file_name().and_then(|name| name.to_str());
    if !name.is_some_and(|name| name.starts_with("kwe-scene-script-")) {
        return;
    }
    match fs::remove_dir_all(dir) {
        Ok(()) => eprintln!(
            "event=renderer.scene.script_dir_cleanup path={}",
            dir.display()
        ),
        Err(error) => eprintln!("event=renderer.scene.script_dir_cleanup error={error}"),
    }
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
    use std::os::unix::fs::PermissionsExt;

    #[test]
    fn media_state_maps_to_latest_wins_commands() {
        assert_eq!(
            media_command_for("playing"),
            Some(video::MediaCommand::Play)
        );
        assert_eq!(
            media_command_for("paused"),
            Some(video::MediaCommand::Pause)
        );
        assert_eq!(
            media_command_for("stopped"),
            Some(video::MediaCommand::Stop)
        );
        assert_eq!(media_command_for("metadata"), None);
    }

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

    #[test]
    fn extract_script_replaces_stale_pid_dir() {
        // M3b review follow-up (pid-recycle brick): a daemon restart can
        // recycle a pid, leaving a stale kwe-scene-script-<pid> dir in the
        // worker's own HOME. Extraction must replace it instead of failing
        // with AlreadyExists (which would bounce a valid scene at exit 73).
        let home = std::env::temp_dir().join(format!("kwe-extract-home-{}", std::process::id()));
        let _ = fs::remove_dir_all(&home);
        let dir = home.join(format!("kwe-scene-script-{}", std::process::id()));
        fs::create_dir_all(&dir).unwrap();
        fs::write(dir.join("script.js"), b"stale").unwrap();
        fs::write(dir.join("leftover.bin"), b"old").unwrap();
        let first = extract_script_into(&home, b"function init() {}").unwrap();
        assert_eq!(fs::read(&first).unwrap(), b"function init() {}");
        // Second extraction on the same pid: the fresh dir now exists
        // again, exercising the replace-stale path once more.
        let second = extract_script_into(&home, b"function update() {}").unwrap();
        assert_eq!(fs::read(&second).unwrap(), b"function update() {}");
        assert!(
            !dir.join("leftover.bin").exists(),
            "stale content must be gone"
        );
        let _ = fs::remove_dir_all(&home);
    }

    #[test]
    fn extract_script_refuses_stale_symlink_dir() {
        // Defense in depth: a stale directory that is a symlink is refused,
        // never followed and removed.
        let home = std::env::temp_dir().join(format!("kwe-extract-link-{}", std::process::id()));
        let _ = fs::remove_dir_all(&home);
        let target = home.join("target");
        fs::create_dir_all(&target).unwrap();
        let dir = home.join(format!("kwe-scene-script-{}", std::process::id()));
        std::os::unix::fs::symlink(&target, &dir).unwrap();
        assert!(extract_script_into(&home, b"x").is_err());
        assert!(target.exists(), "symlink target must be untouched");
        let _ = fs::remove_dir_all(&home);
    }

    #[test]
    fn cleanup_script_dir_removes_only_worker_dirs() {
        // The worker removes its extracted script dir on graceful exit; the
        // cleanup refuses anything it did not create (name-guarded).
        let home = std::env::temp_dir().join(format!("kwe-cleanup-home-{}", std::process::id()));
        let _ = fs::remove_dir_all(&home);
        let dir = home.join(format!("kwe-scene-script-{}", std::process::id()));
        fs::create_dir_all(&dir).unwrap();
        fs::write(dir.join("script.js"), b"x").unwrap();
        cleanup_script_dir(Some(&dir.join("script.js")));
        assert!(!dir.exists(), "script dir must be removed on graceful exit");
        // A path outside the kwe-scene-script-* naming is left alone.
        let foreign = home.join("foreign-dir");
        fs::create_dir_all(&foreign).unwrap();
        fs::write(foreign.join("f.js"), b"x").unwrap();
        cleanup_script_dir(Some(&foreign.join("f.js")));
        assert!(foreign.exists(), "foreign dirs must not be touched");
        cleanup_script_dir(None);
        let _ = fs::remove_dir_all(&home);
    }

    // ---- M3c: layer image resolution and loading ----

    /// A tiny decodable RGBA png (2×2, red at the first pixel), via the
    /// image crate encoder — the fixtures and the decoder agree by
    /// construction.
    fn tiny_png() -> Vec<u8> {
        let rgba = image::RgbaImage::from_raw(
            2,
            2,
            vec![255, 0, 0, 255, 0, 0, 0, 255, 0, 0, 0, 255, 0, 0, 0, 255],
        )
        .unwrap();
        let mut bytes = Vec::new();
        image::DynamicImage::ImageRgba8(rgba)
            .write_to(
                &mut std::io::Cursor::new(&mut bytes),
                image::ImageFormat::Png,
            )
            .unwrap();
        bytes
    }

    /// An image layer with every field at its default, `image` optional.
    fn layer(name: &str, image: Option<&str>) -> scene::LayerSpec {
        scene::LayerSpec {
            name: name.into(),
            scene_order: 0,
            image: image.map(Into::into),
            origin: [0.0, 0.0],
            angles: [0.0, 0.0, 0.0],
            scale: [1.0, 1.0],
            size: [0.0, 0.0],
            alpha: 1.0,
            visible: true,
            blend_mode: 0,
            brightness: 1.0,
            tint: [1.0, 1.0, 1.0, 1.0],
            texture: None,
            text: None,
            video: None,
        }
    }

    #[test]
    fn resolve_layer_image_confines_to_the_content_root() {
        // The file lane's resolution must match the script resolver's
        // confinement: relative paths only, no `..`, nothing a symlink can
        // smuggle out of the root, regular files only.
        let root = std::env::temp_dir().join(format!("kwe-image-root-{}", std::process::id()));
        let outside =
            std::env::temp_dir().join(format!("kwe-image-outside-{}", std::process::id()));
        let _ = fs::remove_dir_all(&root);
        let _ = fs::remove_dir_all(&outside);
        fs::create_dir_all(root.join("textures")).unwrap();
        fs::create_dir_all(&outside).unwrap();
        fs::write(root.join("textures").join("red.png"), tiny_png()).unwrap();
        fs::write(outside.join("secret.png"), tiny_png()).unwrap();
        std::os::unix::fs::symlink(
            outside.join("secret.png"),
            root.join("textures").join("link.png"),
        )
        .unwrap();

        // Relative and inside the root: resolves and reads the bytes.
        let bytes = resolve_layer_image(&root, "textures/red.png").expect("inside root");
        assert_eq!(bytes, tiny_png());
        // Absolute, parent-directory, symlink-escape, missing, directory,
        // and empty references are all refused.
        assert!(resolve_layer_image(&root, "/etc/passwd").is_err());
        assert!(resolve_layer_image(&root, "../outside/secret.png").is_err());
        assert!(resolve_layer_image(&root, "textures/link.png").is_err());
        assert!(resolve_layer_image(&root, "textures/missing.png").is_err());
        assert!(resolve_layer_image(&root, "textures").is_err());
        assert!(resolve_layer_image(&root, "").is_err());
        let _ = fs::remove_dir_all(&root);
        let _ = fs::remove_dir_all(&outside);
    }

    #[test]
    fn load_layer_textures_resolves_decodes_and_fills_size() {
        // The file lane, through a real closure: a decodable texture fills
        // `texture` and (when `size` is absent) the decoded dimensions; a
        // missing image and a layer without an image reference stay
        // textureless — the layer is skipped, never the scene.
        let root = std::env::temp_dir().join(format!("kwe-layer-root-{}", std::process::id()));
        let _ = fs::remove_dir_all(&root);
        fs::create_dir_all(&root).unwrap();
        fs::write(root.join("red.png"), tiny_png()).unwrap();
        let mut layers = vec![
            layer("a", Some("red.png")),
            layer("b", Some("missing.png")),
            layer("c", None),
        ];
        layers[0].size = [0.0, 0.0]; // absent size
        load_layer_textures(&mut layers, |reference| {
            resolve_layer_image(&root, reference)
        });
        let a = layers[0].texture.as_ref().expect("a decodes");
        assert_eq!((a.width, a.height), (2, 2));
        assert_eq!(&a.rgba[0..4], &[255, 0, 0, 255]);
        assert_eq!(
            layers[0].size,
            [2.0, 2.0],
            "absent size takes the texture's decoded dimensions"
        );
        assert!(layers[1].texture.is_none(), "missing image skips the layer");
        assert!(
            layers[2].texture.is_none(),
            "no image reference stays textureless"
        );
        let _ = fs::remove_dir_all(&root);
    }

    #[test]
    fn load_layer_textures_keeps_explicit_size() {
        let mut layers = vec![layer("a", Some("red.png"))];
        layers[0].size = [100.0, 50.0];
        load_layer_textures(&mut layers, |_| Ok(tiny_png()));
        assert_eq!(
            layers[0].size,
            [100.0, 50.0],
            "an explicit size is never overwritten by the texture"
        );
        assert!(layers[0].texture.is_some());
    }

    #[test]
    fn load_layer_textures_pkg_lane_contract_and_undecodable_skip() {
        // The closure is lane-agnostic: this one simulates the pkg entry
        // table (a `kwe_core::image_entry`-shaped resolver: literal or tail
        // match, everything else an Err), which is what the pkg lane feeds
        // in. Undecodable bytes skip the layer.
        let mut layers = vec![
            layer("pkg", Some("textures/red.png")),
            layer("junk", Some("textures/junk.png")),
        ];
        let mut calls = 0;
        load_layer_textures(&mut layers, |reference| {
            calls += 1;
            match reference {
                "textures/red.png" => Ok(tiny_png()),
                "textures/junk.png" => Ok(b"not an image".to_vec()),
                other => Err(format!("{other} is not an entry of the package")),
            }
        });
        assert_eq!(calls, 2, "every image reference is resolved exactly once");
        assert!(
            layers[0].texture.is_some(),
            "the pkg lane decodes through the same path"
        );
        assert!(
            layers[1].texture.is_none(),
            "undecodable bytes skip the layer"
        );
    }

    // ---- M3g: video layer resolution, extraction, and lifecycle ----

    /// A video layer with every shared prop at its default. `source` is
    /// the raw reference exactly as a scene would write it; `path` is
    /// filled by the loaders, never by the parse.
    fn video_layer(name: &str, source: Option<&str>) -> scene::LayerSpec {
        let mut spec = layer(name, None);
        spec.video = Some(scene::VideoSpec {
            source: source.map(Into::into),
            loop_playback: true,
            rate: 1.0,
            path: None,
        });
        spec
    }

    #[test]
    fn resolve_layer_video_confines_to_the_content_root() {
        // Same containment contract as resolve_layer_image, with one
        // deliberate difference: the file is never read (libmpv opens the
        // path), so the size cap is checked from the metadata.
        let root = std::env::temp_dir().join(format!("kwe-video-root-{}", std::process::id()));
        let outside =
            std::env::temp_dir().join(format!("kwe-video-outside-{}", std::process::id()));
        let _ = fs::remove_dir_all(&root);
        let _ = fs::remove_dir_all(&outside);
        fs::create_dir_all(root.join("videos")).unwrap();
        fs::create_dir_all(&outside).unwrap();
        fs::write(root.join("videos").join("clip.mp4"), b"not really a video").unwrap();
        fs::write(outside.join("secret.mp4"), b"secret").unwrap();
        std::os::unix::fs::symlink(
            outside.join("secret.mp4"),
            root.join("videos").join("link.mp4"),
        )
        .unwrap();

        // Relative and inside the root: resolves to the canonical path and
        // does NOT read the file (the bytes above are not a video).
        let path = resolve_layer_video(&root, "videos/clip.mp4").expect("inside root");
        assert!(path.starts_with(root.canonicalize().unwrap()));
        assert_eq!(fs::read(&path).unwrap(), b"not really a video");
        // Absolute, parent-directory, symlink-escape, missing, directory,
        // and empty references are all refused.
        assert!(resolve_layer_video(&root, "/etc/passwd").is_err());
        assert!(resolve_layer_video(&root, "../outside/secret.mp4").is_err());
        assert!(resolve_layer_video(&root, "videos/link.mp4").is_err());
        assert!(resolve_layer_video(&root, "videos/missing.mp4").is_err());
        assert!(resolve_layer_video(&root, "videos").is_err());
        assert!(resolve_layer_video(&root, "").is_err());

        // Over the source cap: a sparse file costs no space but reports a
        // length past MAX_VIDEO_SOURCE_BYTES, and metadata alone refuses it.
        let huge = root.join("videos").join("huge.mp4");
        fs::File::create(&huge)
            .unwrap()
            .set_len(video::MAX_VIDEO_SOURCE_BYTES + 1)
            .unwrap();
        assert!(resolve_layer_video(&root, "videos/huge.mp4").is_err());

        let _ = fs::remove_dir_all(&root);
        let _ = fs::remove_dir_all(&outside);
    }

    #[test]
    fn load_layer_videos_fills_paths_and_skips_unresolved() {
        // Through a real closure, the file lane's shape: a resolvable
        // source fills `path`; an unresolvable one and a cleared source
        // (the over-cap case) leave it None — the layer stays registered
        // either way, and a non-video layer is untouched.
        let root = std::env::temp_dir().join(format!("kwe-video-load-{}", std::process::id()));
        let _ = fs::remove_dir_all(&root);
        fs::create_dir_all(&root).unwrap();
        fs::write(root.join("clip.mp4"), b"bytes").unwrap();
        let mut layers = vec![
            video_layer("a", Some("clip.mp4")),
            video_layer("b", Some("missing.mp4")),
            video_layer("c", None),
            layer("d", Some("red.png")),
        ];
        load_layer_videos(&mut layers, |reference| {
            resolve_layer_video(&root, reference)
        });
        assert!(
            layers[0].video.as_ref().unwrap().path.is_some(),
            "resolvable source fills the path"
        );
        assert!(
            layers[1].video.as_ref().unwrap().path.is_none(),
            "missing source skips the layer, never the scene"
        );
        assert!(
            layers[2].video.as_ref().unwrap().path.is_none(),
            "a cleared source opens no decoder"
        );
        assert!(layers[3].video.is_none(), "non-video layers are untouched");
        let _ = fs::remove_dir_all(&root);
    }

    #[test]
    fn extract_video_into_reuses_one_dir_across_slots() {
        // Unlike the script dir (replaced wholesale), the video dir is
        // reused: a scene may carry MAX_VIDEO_LAYERS videos, so staleness
        // is handled per file. Both slots must survive the second call.
        let home = std::env::temp_dir().join(format!("kwe-video-home-{}", std::process::id()));
        let _ = fs::remove_dir_all(&home);
        fs::create_dir_all(&home).unwrap();
        let first = extract_video_into(&home, 0, b"one").unwrap();
        let second = extract_video_into(&home, 1, b"two").unwrap();
        assert_ne!(first, second, "each slot gets its own file");
        assert_eq!(fs::read(&first).unwrap(), b"one");
        assert_eq!(fs::read(&second).unwrap(), b"two");
        assert_eq!(
            first.parent(),
            second.parent(),
            "one directory holds every slot"
        );
        let dir = first.parent().unwrap().to_path_buf();
        assert_eq!(
            fs::metadata(&dir).unwrap().permissions().mode() & 0o777,
            0o700
        );
        assert_eq!(
            fs::metadata(&first).unwrap().permissions().mode() & 0o777,
            0o600
        );
        // A stale file from a recycled pid is replaced, not appended to
        // and not a create_new failure.
        let again = extract_video_into(&home, 0, b"fresh").unwrap();
        assert_eq!(again, first);
        assert_eq!(fs::read(&first).unwrap(), b"fresh");
        assert_eq!(
            fs::read(&second).unwrap(),
            b"two",
            "the other slot survives"
        );
        let _ = fs::remove_dir_all(&home);
    }

    #[test]
    fn extract_video_into_unlinks_a_planted_symlink_file() {
        // remove_file unlinks the link itself rather than following it, so
        // a link planted at the slot path cannot redirect the write into
        // the target.
        let home = std::env::temp_dir().join(format!("kwe-video-link-{}", std::process::id()));
        let _ = fs::remove_dir_all(&home);
        let dir = home.join(format!("kwe-scene-video-{}", std::process::id()));
        fs::create_dir_all(&dir).unwrap();
        let target = home.join("target.bin");
        fs::write(&target, b"original").unwrap();
        std::os::unix::fs::symlink(&target, dir.join("video-0.bin")).unwrap();
        let path = extract_video_into(&home, 0, b"payload").unwrap();
        assert_eq!(fs::read(&path).unwrap(), b"payload");
        assert_eq!(
            fs::read(&target).unwrap(),
            b"original",
            "the symlink target must be untouched"
        );
        let _ = fs::remove_dir_all(&home);
    }

    #[test]
    fn extract_video_into_refuses_a_stale_non_directory() {
        // A stale entry of that name which is not a plain directory (a
        // file, or a symlink to one) is an error, never something we
        // remove and replace.
        let home = std::env::temp_dir().join(format!("kwe-video-stale-{}", std::process::id()));
        let _ = fs::remove_dir_all(&home);
        fs::create_dir_all(&home).unwrap();
        let dir = home.join(format!("kwe-scene-video-{}", std::process::id()));
        fs::write(&dir, b"not a directory").unwrap();
        assert!(extract_video_into(&home, 0, b"x").is_err());
        assert_eq!(fs::read(&dir).unwrap(), b"not a directory");

        let linked = std::env::temp_dir().join(format!("kwe-video-stale-t-{}", std::process::id()));
        let _ = fs::remove_dir_all(&linked);
        fs::create_dir_all(&linked).unwrap();
        fs::remove_file(&dir).unwrap();
        std::os::unix::fs::symlink(&linked, &dir).unwrap();
        assert!(extract_video_into(&home, 0, b"x").is_err());
        assert!(
            fs::read_dir(&linked).unwrap().next().is_none(),
            "the symlink target must stay empty"
        );
        let _ = fs::remove_dir_all(&home);
        let _ = fs::remove_dir_all(&linked);
    }

    #[test]
    fn cleanup_video_dir_removes_only_its_own_dir() {
        // The graceful exit path removes what extract_video_into created,
        // and nothing else: a symlink of the same name is left alone.
        let home = std::env::temp_dir().join(format!("kwe-video-clean-{}", std::process::id()));
        let _ = fs::remove_dir_all(&home);
        fs::create_dir_all(&home).unwrap();
        let path = extract_video_into(&home, 0, b"payload").unwrap();
        let dir = path.parent().unwrap().to_path_buf();
        cleanup_video_dir_in(&home);
        assert!(!dir.exists(), "the extracted dir must be gone");
        // A missing dir is not an error (the common case: no video layer).
        cleanup_video_dir_in(&home);

        let linked = std::env::temp_dir().join(format!("kwe-video-clean-t-{}", std::process::id()));
        let _ = fs::remove_dir_all(&linked);
        fs::create_dir_all(&linked).unwrap();
        fs::write(linked.join("keep.bin"), b"keep").unwrap();
        std::os::unix::fs::symlink(&linked, &dir).unwrap();
        cleanup_video_dir_in(&home);
        assert!(
            linked.join("keep.bin").exists(),
            "a planted symlink must not be followed"
        );
        let _ = fs::remove_dir_all(&home);
        let _ = fs::remove_dir_all(&linked);
    }

    #[test]
    fn open_video_layers_skips_layers_without_a_resolved_path() {
        // No path means no decoder — the parse's over-cap clearing and the
        // loader's skip both land here, and neither costs the scene.
        let mut layers = vec![
            video_layer("a", Some("clip.mp4")),
            video_layer("b", None),
            layer("c", None),
        ];
        assert!(
            open_video_layers(&mut layers).is_empty(),
            "an unresolved source opens nothing"
        );
    }
}
