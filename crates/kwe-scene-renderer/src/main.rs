// SPDX-License-Identifier: GPL-3.0-or-later
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
mod materialshader;
mod particlefile;
mod particles;
mod scene;
mod scene_ir_adapter;
mod shaderpre;
mod text;
mod textures;
mod texv;
mod video;
mod vulkan;

use std::cell::RefCell;
use std::ffi::CString;
use std::fs;
use std::io::{Read, Write};
use std::os::fd::{FromRawFd, RawFd};
use std::os::unix::ffi::OsStrExt;
use std::os::unix::fs::{DirBuilderExt, MetadataExt, OpenOptionsExt};
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
use textures::{MAX_TEXTURE_SOURCE_BYTES, texture_budget_allows};
use vulkan::{
    EffectTargetRequest, LayerRenderer, MaterialTextureBind, RenderError, is_fence_timeout,
};

/// Backend rejection: the scene cannot be rendered at all.
const EXIT_BACKEND_REJECT: i32 = 73;
/// B2: the scene declares objects and NONE of them can put a pixel on the
/// screen in this build — every layer is a model whose material texture
/// could not be resolved (S1: no Wallpaper Engine assets configured, or
/// the corpus reference simply does not resolve), a texture that would
/// not decode, or a particle system with no material. Compositing
/// it would publish the bare clear colour as a healthy frame and the desktop
/// would go flat with nothing anywhere saying why, so the worker refuses
/// before the first publish and the apply transaction rolls back. Preflight
/// refuses the same scenes statically; this is the backstop for the ones
/// whose content only fails once it is decoded.
const EXIT_NO_DRAWABLE_CONTENT: i32 = 74;
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
    /// F1 (docs/backlog/WALLPAPER_SCALING_MODES.md): how the picture maps
    /// onto the frame canvas — `aspect` (letterbox), `fill` (crop),
    /// `stretch`. Scene: scene units are the declared scene
    /// resolution; the compositor maps that rectangle onto the canvas by
    /// this mode (before F1 scene units were canvas pixels 1:1, so a
    /// 1920x1080 scene in a 960x540 canvas showed its centre quarter).
    #[arg(long, default_value = "aspect", value_parser = ["aspect", "fill", "stretch"])]
    scaling: String,
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
    /// Wallpaper Engine assets root (S1), consulted after the scene's own
    /// package/directory when resolving a model layer's material texture
    /// (kwe_core::scenemodel::resolve_model). Optional: without it, model
    /// layers only resolve against assets the scene itself carries.
    #[arg(long = "assets-dir")]
    assets_dir: Option<PathBuf>,
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
    /// S2: per layer, whether its material shader preprocessed, compiled,
    /// and bound a pipeline (vulkan.rs `bind_material_layer`) — draws
    /// through the material pipeline instead of the S1 base-texture quad.
    /// Index-aligned with `layers`/`texture_ok`; always `false` for a
    /// layer with no model reference, and for any material that fell
    /// back (see `compile_material_layers`).
    material_ok: Vec<bool>,
    /// S5: layer indices whose final bound material samples
    /// `_rt_FullFrameBuffer` (`compile_material_layers`, capped at
    /// `vulkan::MAX_FULL_FRAME_BUFFER_SNAPSHOTS_PER_FRAME`) — fixed for
    /// the worker's lifetime (which layers reference this name by name is
    /// a load-time scene property), handed to `renderer.render` every
    /// frame so it can snapshot the scene composited so far immediately
    /// before each one draws.
    ffb_snapshot_layers: Vec<usize>,
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
            frame_draws(&self.layers, &self.texture_ok, &self.material_ok),
            particle_draws(&self.particles, &self.particle_texture_ok),
        );
        // S3: replay every effect action recorded at scene load (fresh
        // content into every targeted FBO, then any `command`) BEFORE the
        // main composite pass, so a layer's own material — whose bound
        // texture slots may sample those FBOs — draws with this frame's
        // effect output. A single bounds check for the overwhelming
        // majority of scenes, which have no effects at all.
        if let Err(error) = self.renderer.render_effect_chains() {
            reject_render(&error, "fence timeout while replaying effect chains");
        }
        let initial =
            match self
                .renderer
                .render(self.engine.clear_color(), &draws, &self.ffb_snapshot_layers)
            {
                Ok(pixels) => pixels,
                Err(error) => reject_render(&error, "initial render failure"),
            };
        // S3: refresh `_rt_FullFrameBuffer` with THIS frame's finished
        // composite, ready for the NEXT frame's `render_effect_chains`
        // call — a one-frame lag, documented on
        // `vulkan::LayerRenderer::snapshot_full_frame_buffer`. A no-op
        // when no layer resolved an effect chain.
        if let Err(error) = self.renderer.snapshot_full_frame_buffer() {
            reject_render(
                &error,
                "fence timeout while snapshotting _rt_FullFrameBuffer",
            );
        }
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
            // SR-2c2: this `dt` is REAL elapsed wall-clock time, not a
            // deterministic virtual clock -- particle simulation (a young,
            // still-ramping-up system especially) is therefore sensitive
            // to incidental per-process timing (OS scheduling, cache
            // warmth, which two binaries happen to be compared), NOT just
            // to scene data/renderer logic. Two builds with byte-identical
            // parsed scene data and byte-identical simulation code can
            // still land on a different tick count by a fixed wall-clock
            // deadline -- investigated in depth for a real corpus scene in
            // docs/SR2.md's SR-2c2 entry (a genuine SR-2c false alarm this
            // property produced) and documented as a known false-positive
            // source in scripts/scene-corpus-byte-identity-sweep.sh's own
            // header. `ParticleSystemState::step`/`simulate` themselves
            // ARE deterministic given a FIXED dt sequence (particles.rs's
            // `deterministic_across_independent_runs` proves this at the
            // unit level) -- the nondeterminism lives entirely in what
            // real-world `dt` sequence a given process run happens to
            // produce, not in anything downstream of it.
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
                        frame_draws(&self.layers, &self.texture_ok, &self.material_ok),
                        particle_draws(&self.particles, &self.particle_texture_ok),
                    );
                    // S3: same ordering as the initial frame above — a
                    // fence timeout here is exactly as fatal as any other
                    // per-frame Vulkan call.
                    if let Err(error) = self.renderer.render_effect_chains() {
                        reject_render(&error, "fence timeout while replaying effect chains");
                    }
                    match self
                        .renderer
                        .render(color, &draws, &self.ffb_snapshot_layers)
                    {
                        Ok(pixels) if pixels.len() == self.spec.pixel_bytes() => {
                            // Exact-size check: the conversion is exact by
                            // construction; a mismatch means a malformed
                            // frame, which is skipped and counted, never
                            // published.
                            self.consecutive_render_failures = 0;
                            if let Err(error) = self.renderer.snapshot_full_frame_buffer() {
                                reject_render(
                                    &error,
                                    "fence timeout while snapshotting _rt_FullFrameBuffer",
                                );
                            }
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
    // Establish cleanup ownership before load_scene can stage either file or
    // package videos. Rust returns/unwinds therefore remove staged media even
    // when bootstrap fails; process::exit paths are additionally covered by
    // the supervisor removing the private per-launch HOME.
    let video_cleanup = VideoCleanupGuard::new();

    // 1. Scene: parse and reject (exit 73) anything the engine cannot render.
    let mut config = load_scene(&content, arguments.assets_dir.as_deref());
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

    // S1: `config.model_layer_skips` now counts every model object the
    // parse saw (registered or not; the field predates texture
    // resolution). Whether each one actually draws depends on
    // `load_model_textures` (run inside `load_scene` above), whose own
    // one-time diagnostic (`event=renderer.scene.model_texture_skip`)
    // already named how many failed to resolve or decode — no separate
    // diagnostic here, to avoid two overlapping lines for the same fact.

    // 1c. B2 no-drawable-content guard: the same rule preflight runs
    // (`kwe_core::summarize_scene_objects_resolved`), applied to the scene
    // this worker actually parsed. A scene that declares objects and can
    // draw NONE of them composites to bare clear colour forever — no
    // script can change that, because a script only moves and recolours
    // what the scene declared — so the worker refuses before its first
    // publish and the apply transaction rolls back instead of promoting a
    // flat frame.
    //
    // The rule is STATIC for every layer kind except models (S1): a layer
    // whose content fails to decode or whose video source will not open
    // is a degraded layer, and degrading a layer never rejects a scene
    // (the M3c/M3g skip-never-reject contract) — `drawable_objects` still
    // counts it. A model layer is the one exception: it counts toward
    // `drawable_objects` only when `load_model_textures` actually resolved
    // its texture (deliverable 4's honesty gate — see that function's
    // doc), because unlike a direct image reference a model has no
    // pipeline at all without a resolvable texture.
    //
    // A scene that declares NO objects at all is exempt: an empty scene is
    // the author's choice (and its script may animate the clear colour),
    // not a feature this build is missing.
    let declared_objects = config.layers.len() + config.particles.len();
    if declared_objects > 0 && config.drawable_objects == 0 {
        eprintln!(
            "event=renderer.scene.no_drawable_content objects={declared_objects} model_layers={} particle_systems={} layers={} detail=scene renders nothing in this build",
            config.model_layer_skips,
            config.particles.len(),
            config.layers.len()
        );
        eprintln!("event=renderer.scene.unsupported exit_code={EXIT_NO_DRAWABLE_CONTENT}");
        exit(EXIT_NO_DRAWABLE_CONTENT);
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

    // 4b. F1: the scene rectangle (declared resolution, scene units) maps
    //     onto the canvas by the scaling mode; the compositor's NDC divisor
    //     becomes the visible world extent instead of the canvas size.
    let (world_w, world_h) = world_extent(
        config.resolution,
        (spec.width, spec.height),
        &arguments.scaling,
    );
    // S6: every layer's `t` (layers.rs `layer_model`) is its scene.json
    // `origin` verbatim — an absolute scene-pixel position, top-left
    // origin, +y down (`docs/SCENE_FORMAT_V1.md`) — not an offset from
    // the visible rectangle's centre. `world_extent`'s letterbox/crop
    // rectangle is, by construction, centered on the SCENE's own centre
    // (declared resolution / 2), which is not always `world_w/2,
    // world_h/2` once letterboxing or cropping makes the extent differ
    // from the declared resolution on one axis (`fill` crops one axis
    // short of the scene; `aspect` pads one axis past it). Falls back to
    // the canvas centre when there is no declared resolution, matching
    // `world_extent`'s own "scene units are canvas pixels" fallback.
    let scene_center = config.resolution.map_or(
        (spec.width as f32 / 2.0, spec.height as f32 / 2.0),
        |(w, h)| (w as f32 / 2.0, h as f32 / 2.0),
    );
    renderer.set_world_extent(world_w, world_h, [scene_center.0, scene_center.1]);
    if (world_w, world_h) != (spec.width as f32, spec.height as f32) {
        eprintln!(
            "event=renderer.scene.world_extent scaling={} extent={world_w}x{world_h} canvas={}x{}",
            arguments.scaling, spec.width, spec.height
        );
    }

    // 5. Layer textures: upload every decoded layer texture before the first
    //    render (all uploads share the startup fence, which is waited to
    //    completion per upload). A failed upload skips the layer with a
    //    bounded one-time diagnostic — the renderer stays healthy.
    let layers = engine.layers();
    let mut texture_ok = upload_layer_textures(&mut renderer, &config.layers, &layers);
    upload_video_first_frames(&mut renderer, &config.layers, &mut videos, &mut texture_ok);

    // 5a. S2: for every model layer with resolved material data, attempt
    //     to preprocess + compile + bind its material shader; on any
    //     failure the layer keeps drawing through the S1 base-texture
    //     quad above (texture_ok already covers it) — this step only ever
    //     ADDS the material pipeline on top, never removes drawability.
    let (material_ok, ffb_snapshot_layers) = compile_material_layers(
        &mut config.layers,
        &mut renderer,
        world_w,
        world_h,
        spec.width,
        spec.height,
        &content,
        arguments.assets_dir.as_deref(),
        config.resolution,
    );

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
            "event=renderer.scene.particle_file_ref count={} (S4b: external particle definition files are resolved and parsed; a reference that fails to resolve keeps the M3f flat-model defaults, see particle_file_skip)",
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
        material_ok,
        ffb_snapshot_layers,
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
    // M3g: the extracted video directory is the worker's too. Drop the guard
    // only after all decoders have gone away, preserving teardown ordering.
    drop(video_cleanup);
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
/// The visible world rectangle, in scene units, that the canvas shows
/// (F1). `aspect`: the whole scene fits with letterbox (extent ≥ scene on
/// one axis); `fill`: the scene covers the canvas and is cropped (extent ≤
/// scene on one axis); `stretch`: the extent IS the scene (aspect ignored).
/// No declared resolution → scene units are canvas pixels, as before F1.
/// Pure; unit-tested.
fn world_extent(scene: Option<(u32, u32)>, canvas: (u32, u32), scaling: &str) -> (f32, f32) {
    let (cw, ch) = (canvas.0.max(1) as f32, canvas.1.max(1) as f32);
    let Some((sw, sh)) = scene else {
        return (cw, ch);
    };
    if sw == 0 || sh == 0 {
        return (cw, ch);
    }
    let (sw, sh) = (sw as f32, sh as f32);
    match scaling {
        "stretch" => (sw, sh),
        "fill" => {
            let scale = (cw / sw).max(ch / sh);
            (cw / scale, ch / scale)
        }
        _ => {
            let scale = (cw / sw).min(ch / sh);
            (cw / scale, ch / scale)
        }
    }
}

fn load_scene(content: &Path, assets_dir: Option<&Path>) -> SceneConfig {
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
        // S1: model layers resolve model -> material -> texture through
        // kwe_core::scenemodel, looked up against the scene directory
        // first and the Wallpaper Engine assets root second (file lane
        // has no package entries to try first).
        config.drawable_objects += load_model_textures(
            &mut config.layers,
            &mut used_bytes,
            config.resolution,
            |reference| {
                resolve_layer_image(&root, reference).ok().or_else(|| {
                    assets_dir.and_then(|assets| resolve_layer_image(assets, reference).ok())
                })
            },
        );
        // S4b: external particle files resolve model -> material -> texture
        // the same lookup order as model layers just above (scene
        // directory first, Wallpaper Engine assets root second).
        config.drawable_objects +=
            load_particle_file_definitions(&mut config.particles, &mut used_bytes, |reference| {
                resolve_layer_image(&root, reference).ok().or_else(|| {
                    assets_dir.and_then(|assets| resolve_layer_image(assets, reference).ok())
                })
            });
        // M3g: stage every file-scene video into the worker-owned private
        // directory before libmpv sees it. The source is opened with
        // O_NOFOLLOW and copied through that already-open fd, so a later
        // symlink/path replacement cannot redirect libmpv outside root.
        let mut video_slot = 0usize;
        load_layer_videos(&mut config.layers, |reference| {
            let slot = video_slot;
            video_slot += 1;
            stage_file_video(&root, reference, slot)
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
    // S1: model layers resolve model -> material -> texture through
    // kwe_core::scenemodel, looked up against the package entry table
    // first, the scene's own directory (the pkg's parent — the rare
    // corpus layout with loose files beside the archive) second, and the
    // Wallpaper Engine assets root last.
    let pkg_dir = content
        .parent()
        .and_then(|parent| parent.canonicalize().ok());
    config.drawable_objects += load_model_textures(
        &mut config.layers,
        &mut used_bytes,
        config.resolution,
        |reference| {
            if let Ok(index) = kwe_core::image_entry(reference, reader.entries())
                && let Ok(bytes) = reader.read_entry_bounded(index, MAX_TEXTURE_SOURCE_BYTES)
            {
                return Some(bytes);
            }
            if let Some(dir) = &pkg_dir
                && let Ok(bytes) = resolve_layer_image(dir, reference)
            {
                return Some(bytes);
            }
            assets_dir.and_then(|assets| resolve_layer_image(assets, reference).ok())
        },
    );
    // S4b: external particle files resolve the same lookup order as model
    // layers just above (pkg entries first, the pkg's own directory
    // second, the Wallpaper Engine assets root last).
    config.drawable_objects +=
        load_particle_file_definitions(&mut config.particles, &mut used_bytes, |reference| {
            if let Ok(index) = kwe_core::image_entry(reference, reader.entries())
                && let Ok(bytes) = reader.read_entry_bounded(index, MAX_TEXTURE_SOURCE_BYTES)
            {
                return Some(bytes);
            }
            if let Some(dir) = &pkg_dir
                && let Ok(bytes) = resolve_layer_image(dir, reference)
            {
                return Some(bytes);
            }
            assets_dir.and_then(|assets| resolve_layer_image(assets, reference).ok())
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
        // S7: route through `texv::decode_model_texture` (falls back to
        // plain `decode_texture` for non-TEXV0005 bytes) so a `.tex`-backed
        // animated image layer decodes AND carries its spritesheet grid,
        // instead of unconditionally failing decode as a plain image.
        let Some(texture) = texv::decode_model_texture(&bytes) else {
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
        // S7: same `decode_model_texture` swap as `load_layer_textures` —
        // a flat-model (M3f) particle system whose `material` names a
        // `.tex` spritesheet now decodes (and animates) instead of
        // silently drawing nothing.
        let Some(texture) = texv::decode_model_texture(&bytes) else {
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

/// S4b: fill `particles[*].component`/`texture`/`max_count` for every
/// system whose `particle` value was an external file reference
/// (`file_ref`, set by `scene::parse_particle_system`): resolve the file
/// and its `material` chain (`kwe_core::particlefile::
/// resolve_particle_file`, shared with preflight's honesty gate), decode
/// the resolved texture the same way a model layer's material resolves
/// (`texv::decode_model_texture` — a particle file's `material` field
/// names a `materials/*.json` pass exactly like a model's does, not a
/// direct texture file), parse the component model
/// (`particlefile::parse_component_model`) for the worker's own
/// simulation, and set `max_count` from the file's own `maxcount`.
///
/// A particle system whose file does not resolve, is not valid JSON, or
/// whose material texture does not resolve/decode/fit the shared budget
/// keeps its M3f flat-model defaults — the existing honest fallback (the
/// system stays registered and simulates the flat model rather than
/// vanishing); never a scene rejection. Returns the count of systems whose
/// component texture actually resolved and decoded — the honest addend to
/// `drawable_objects`, mirroring `load_model_textures`'s return value.
fn load_particle_file_definitions(
    systems: &mut [scene::ParticleSpec],
    used_bytes: &mut u64,
    mut resolve_asset: impl FnMut(&str) -> Option<Vec<u8>>,
) -> usize {
    let mut resolved = 0usize;
    let mut skipped = 0usize;
    let mut unsupported = particlefile::ComponentParseStats::default();
    for particle in systems.iter_mut() {
        let Some(file_ref) = particle.file_ref.as_deref() else {
            continue;
        };
        let resolved_file = match kwe_core::resolve_particle_file(file_ref, &mut resolve_asset) {
            Ok(resolved) => resolved,
            Err(_detail) => {
                skipped += 1;
                continue;
            }
        };
        let is_render_target_base =
            kwe_core::is_runtime_target_name(&resolved_file.material.texture_name);
        let texture = if is_render_target_base {
            None // a particle sprite sampling a live render target is not
        // implemented this slice — degrades to the M3f defaults like
        // any other unresolved texture, never a crash.
        } else {
            texv::decode_model_texture(&resolved_file.material.texture_bytes)
        };
        let Some(texture) = texture else {
            skipped += 1;
            continue;
        };
        let pixels = u64::from(texture.width) * u64::from(texture.height);
        if !texture_budget_allows(*used_bytes, texture.width, texture.height) {
            skipped += 1;
            continue;
        }
        *used_bytes = used_bytes.saturating_add(pixels.saturating_mul(4));
        let (component, stats) = particlefile::parse_component_model(&resolved_file.particle_json);
        unsupported.unsupported_emitters += stats.unsupported_emitters;
        unsupported.unsupported_initializers += stats.unsupported_initializers;
        unsupported.unsupported_operators += stats.unsupported_operators;
        // S7 (P9): merge this file's unrecognized names into the scene-wide
        // sets, bounded by the SAME per-category cap `parse_component_model`
        // already enforces per file — `record_unsupported_name`'s own
        // "first N distinct" rule composes correctly across multiple merges
        // (a name already present is a no-op re-insert, not a second slot).
        for name in &stats.unsupported_emitter_names {
            particlefile::record_unsupported_name(&mut unsupported.unsupported_emitter_names, name);
        }
        for name in &stats.unsupported_initializer_names {
            particlefile::record_unsupported_name(
                &mut unsupported.unsupported_initializer_names,
                name,
            );
        }
        for name in &stats.unsupported_operator_names {
            particlefile::record_unsupported_name(
                &mut unsupported.unsupported_operator_names,
                name,
            );
        }
        // S7 (P6): the scene's `instanceoverride.count` scales the file's
        // own `maxcount` BEFORE the cap clamps it — WE's day/night star
        // systems carry `instanceoverride.count = {..., value: 0.0}` to
        // show no stars by day (Avatar's night-sky systems; we drew 750
        // regardless before this). `clamp_max_count` floors its input to 1
        // (the flat-model default-count contract, unrelated to this path),
        // so a scaled result of exactly 0 is special-cased directly to 0
        // rather than going through it — "nothing ever spawns" needs an
        // actual zero, not the floor.
        //
        // Borrowed-From: Almamu/linux-wallpaperengine (GPL-3.0-or-later)
        // src/WallpaperEngine/Render/Objects/CParticle.cpp:59-61 @
        // b016d7d1 — adapted.
        let scaled_maxcount = (f64::from(component.maxcount) * f64::from(particle.instance_count))
            .round()
            .max(0.0) as u64;
        particle.max_count = if scaled_maxcount == 0 {
            0
        } else {
            particles::clamp_max_count(scaled_maxcount)
        };
        particle.texture = Some(texture);
        particle.component = Some(component);
        // S7 (P4): honour the material's own blend mode instead of the
        // object's colorBlendMode (0 = normal) — every file-based particle
        // system previously drew with plain alpha blending, so an
        // additive star/halo/fog/bokeh sprite's black background drew as
        // an opaque black box (Avatar report: "stars ... blocked out
        // around"). Borrowed-From: Almamu/linux-wallpaperengine
        // (GPL-3.0-or-later) src/WallpaperEngine/Render/Objects/
        // CPass.cpp:129-140 @ b016d7d1 — adapted (this crate's
        // `material_blend_mode` already implements the same additive/
        // translucent mapping other material paths use).
        if let Some(blending) = resolved_file.material.blending.as_deref() {
            particle.blend_mode = material_blend_mode(Some(blending)).as_u32();
        }
        // S7 (P4): the material constant `ui_editor_properties_overbright`
        // multiplies the drawn color (upstream `CParticle.cpp:122-126`,
        // `genericparticle.frag: color.rgb *= g_Overbright`) — folded here
        // into the existing `brightness` multiplier instead of adding a
        // new uniform, since this renderer's particle draw already
        // multiplies color by `particle.brightness` (see `particles.rs`).
        // A non-finite or unparsable value leaves `particle.brightness`
        // untouched (no multiply), matching this file's other "absent or
        // garbage -> keep the default" constant-parsing contract.
        // Borrowed-From: Almamu/linux-wallpaperengine (GPL-3.0-or-later)
        // src/WallpaperEngine/Render/Objects/CParticle.cpp:122-126 @
        // b016d7d1 — adapted.
        if let Some(overbright) = resolved_file
            .material
            .constant_shader_values
            .get("ui_editor_properties_overbright")
            .and_then(parse_constant_components)
            .map(|components| components[0])
            .filter(|value| value.is_finite())
        {
            particle.brightness = layers::clamp_layer_brightness(
                f64::from(particle.brightness) * f64::from(overbright),
            );
        }
        resolved += 1;
    }
    if skipped > 0 {
        eprintln!("event=renderer.scene.particle_file_skip count={skipped}");
    }
    if unsupported.unsupported_emitters > 0
        || unsupported.unsupported_initializers > 0
        || unsupported.unsupported_operators > 0
    {
        // S7 (P9): name the actual unrecognized kinds (bounded — up to
        // MAX_UNSUPPORTED_NAMES distinct names per category, merged across
        // every resolved file this scene load touched) instead of just a
        // count, so the diagnostic says what's actually missing.
        let names: Vec<&str> = unsupported
            .unsupported_emitter_names
            .iter()
            .chain(unsupported.unsupported_initializer_names.iter())
            .chain(unsupported.unsupported_operator_names.iter())
            .map(String::as_str)
            .collect();
        eprintln!(
            "event=renderer.scene.particle_file_unsupported_items emitters={} initializers={} operators={} names={} note=unrecognized-kind-skipped",
            unsupported.unsupported_emitters,
            unsupported.unsupported_initializers,
            unsupported.unsupported_operators,
            names.join(",")
        );
    }
    resolved
}

/// Fill `layers[*].texture` for every model layer (S1): resolve
/// `model_ref` all the way to a texture through
/// `kwe_core::scenemodel::resolve_model` (model.json -> material path ->
/// material.json -> passes[0] -> first texture slot), decode the result
/// (`texv::decode_model_texture`: TEXV0005 containers, defensively also a
/// plain image container), and account the decoded bytes against the same
/// texture budget layer/particle textures share.
///
/// A model whose texture never resolves, or whose resolved bytes fail to
/// decode or exceed the shared budget, is a degraded layer — it stays
/// registered (a script can still reach it by name) but never uploads a
/// texture; this is the skip-never-reject contract every other texture
/// path in this file already follows. Both failure classes fold into one
/// bounded, one-time diagnostic (`event=renderer.scene.model_texture_skip
/// count=N`) rather than one line per layer, matching the model of the
/// pre-S1 `model_layer_skip` diagnostic this replaces for texture
/// failures specifically.
///
/// Returns the count of layers whose texture actually resolved and
/// decoded — the honest addend to `drawable_objects` the B2 gate reads
/// (deliverable 4: unlike a direct image reference, which counts as
/// drawable statically even before its bytes are known to decode, a model
/// layer has no pipeline at all without a resolved texture).
fn material_texture_slots(
    resolved_model: &kwe_core::ResolvedModel,
) -> Vec<Option<scene::MaterialTextureSource>> {
    resolved_model
        .texture_slots
        .iter()
        .map(|slot| {
            slot.as_ref().map(|slot| {
                if slot.is_render_target {
                    scene::MaterialTextureSource::RenderTarget(slot.name.clone())
                } else {
                    scene::MaterialTextureSource::Bytes(slot.bytes.clone())
                }
            })
        })
        .collect()
}

/// S3: sizes and effect chains added on top of the S1/S2 model-texture
/// resolution walk below. `scene_resolution` is the scene's own declared
/// `general.orthogonalprojection` extent (scene units) — used ONLY to
/// size a `fullscreen: true` model layer (the corpus's
/// `models/util/fullscreenlayer.json` post-process base, S3) that has no
/// static texture to size from and no explicit `size` in scene.json.
fn load_model_textures(
    layers: &mut [scene::LayerSpec],
    used_bytes: &mut u64,
    scene_resolution: Option<(u32, u32)>,
    mut resolve_asset: impl FnMut(&str) -> Option<Vec<u8>>,
) -> usize {
    let mut resolved = 0usize;
    let mut skipped = 0usize;
    // S3 review RECOMMENDED #4: aggregate byte budget across every
    // effect-triggered asset read (effect.json/material.json/texture
    // files, via `resolve_object_effects`'s `AssetLookup`) for this
    // WHOLE scene load -- mirrors `used_bytes`/`texture_budget_allows`'s
    // existing cap on the base-texture path. Each individual read is
    // already bounded per-file (`MAX_EFFECT_JSON_BYTES`,
    // `MAX_TEXTURE_SOURCE_BYTES`), but nothing capped the SUM: S3 raises
    // the worst-case lookup count from ~10/layer (S1/S2) to up to
    // ~4096/object (`MAX_PASSES_PER_EFFECT` x `MAX_EFFECTS_PER_OBJECT` x
    // `MAX_MATERIAL_TEXTURES`), so a scene with many objects each
    // re-reading large files across many `bind`/`usertextures`
    // overrides could otherwise read gigabytes.
    let mut used_effect_bytes: u64 = 0;
    let mut effect_budget_exceeded = false;
    for layer in layers.iter_mut() {
        let Some(model_ref) = layer.model_ref.as_deref() else {
            continue; // not a model layer
        };
        let resolved_model = match kwe_core::resolve_model(model_ref, &mut resolve_asset) {
            Ok(resolved_model) => resolved_model,
            Err(_detail) => {
                skipped += 1;
                continue;
            }
        };
        // S3: a base texture that is a `_rt_`/`_alias_` runtime render
        // target (the B2 honesty fix in `kwe_core::resolve_model`) has no
        // bytes to decode — the object's real pixel content comes
        // entirely from its effect chain (see `run_effect_chains`,
        // vulkan.rs), and the base draw itself degrades to the shared
        // dummy texture in that slot (never a refusal). Skip the texv
        // decode step for this case; every other model layer keeps the
        // exact S1/S2 decode-and-budget contract unchanged.
        let is_render_target_base = kwe_core::is_runtime_target_name(&resolved_model.texture_name);
        if !is_render_target_base {
            let Some(texture) = texv::decode_model_texture(&resolved_model.texture_bytes) else {
                skipped += 1;
                continue;
            };
            let pixels = u64::from(texture.width) * u64::from(texture.height);
            if !texture_budget_allows(*used_bytes, texture.width, texture.height) {
                skipped += 1;
                continue;
            }
            *used_bytes = used_bytes.saturating_add(pixels.saturating_mul(4));
            if layer.size == [0.0, 0.0] {
                layer.size = [texture.width as f32, texture.height as f32];
            }
            layer.texture = Some(texture);
        } else if layer.size == [0.0, 0.0]
            && resolved_model.fullscreen
            && let Some((width, height)) = scene_resolution
        {
            // Borrowed-From: Almamu/linux-wallpaperengine (GPL-3.0-or-later)
            // src/WallpaperEngine/Render/Objects/CImage.cpp (the
            // `fullscreen` model flag sizes the layer to the scene/output
            // extent rather than a decoded texture) @ b016d7d1 — adapted
            // (upstream sizes to the live output; this renderer uses the
            // scene's own declared resolution, matching how every other
            // "no explicit size" layer already falls back to a size
            // derived from its content rather than the output).
            layer.size = [width as f32, height as f32];
        }
        layer.fullscreen = resolved_model.fullscreen;
        // S2: keep the material data `resolve_model` already walked so
        // `compile_material_layers` (run after every layer's base texture
        // is known) can attempt a material-pipeline draw without
        // re-parsing model.json/material.json. `constant_shader_values`
        // keeps serde_json's insertion order (not sorted) so
        // `shaderpre`'s slot assignment is deterministic per-material
        // without this module needing to know the ordering rule.
        layer.material = Some(scene::MaterialSpec {
            shader: resolved_model.shader.clone(),
            blending: resolved_model.blending.clone(),
            combos: resolved_model
                .combos
                .iter()
                .filter_map(|(name, value)| Some((name.clone(), value.as_i64()?)))
                .collect(),
            constant_shader_values: resolved_model
                .constant_shader_values
                .iter()
                .map(|(name, value)| (name.clone(), value.clone()))
                .collect(),
            texture_slots: material_texture_slots(&resolved_model),
            passthrough: resolved_model.passthrough,
            fullscreen: resolved_model.fullscreen,
        });
        // S3: resolve this object's effects[] through the same lookup
        // chain (pkg entries -> scene dir -> assets root) that just
        // resolved model_ref/material_ref/texture_ref — effects live on
        // the raw scene-object JSON, not model.json, so this could not
        // happen inside `resolve_model` itself. Never fails the layer
        // (`resolve_object_effects`'s own honesty rule).
        if !layer.effects_raw.is_empty() {
            let mut effect_lookup = |reference: &str| -> Option<Vec<u8>> {
                if !effect_asset_budget_allows(used_effect_bytes) {
                    effect_budget_exceeded = true;
                    return None;
                }
                let bytes = resolve_asset(reference)?;
                used_effect_bytes = used_effect_bytes.saturating_add(bytes.len() as u64);
                Some(bytes)
            };
            layer.effects =
                kwe_core::resolve_object_effects(&layer.effects_raw, &mut effect_lookup);
        }
        resolved += 1;
    }
    if skipped > 0 {
        eprintln!("event=renderer.scene.model_texture_skip count={skipped}");
    }
    if effect_budget_exceeded {
        eprintln!(
            "event=renderer.scene.effect_asset_budget_exceeded bytes={used_effect_bytes} \
             cap={MAX_EFFECT_ASSET_READ_BYTES}"
        );
    }
    resolved
}

/// S3 review RECOMMENDED #4: cumulative byte budget across every
/// effect-triggered asset read for one scene load (see
/// `load_model_textures`'s doc comment on `used_effect_bytes`).
const MAX_EFFECT_ASSET_READ_BYTES: u64 = 256 * 1024 * 1024;

/// Pure, unit-tested budget check — mirrors
/// `textures::texture_budget_allows`'s existing pattern for the
/// base-texture path.
fn effect_asset_budget_allows(used_bytes: u64) -> bool {
    used_bytes < MAX_EFFECT_ASSET_READ_BYTES
}

/// Cap on one shader source file (`.vert`/`.frag`/`.h` include), read
/// through `kwe_core::confined_read`. Generous over any real
/// corpus shader (the largest is under 32 KiB) while bounding a crafted
/// assets tree; matches `materialshader::MAX_SHADER_TEXT_BYTES`, the
/// separate cap on the fully PREPROCESSED text `shaderc` receives.
const MAX_SHADER_SOURCE_BYTES: u64 = 256 * 1024;

/// The `AssetLocator::shader` resolution rule (`AssetLocator.cpp:9-35`):
/// a `workshop/<id>/<file>` reference tries the compat redirect
/// `zcompat/scene/shaders/<id>/<file>` first, falling back to the plain
/// `shaders/<reference>` path either way `reference` did not start with
/// `workshop/`, or the redirect did not resolve. `reference` already
/// carries its extension (`.vert`/`.frag` for a shader, `.h` as `#include`
/// names already do). `lookup` is the SAME pkg-entries -> scene-dir ->
/// assets-root chain `load_model_textures`'s own asset resolution already
/// uses (S3: a scene that bundles its OWN custom effect shaders inside
/// its `scene.pkg` — the real corpus's godrays/tint effects both do this —
/// could never resolve them when this only ever read the filesystem
/// assets root; `shaders/<name>` is just one more relative path the SAME
/// lookup chain already knows how to try).
///
/// Borrowed-From: Almamu/linux-wallpaperengine (GPL-3.0-or-later)
/// src/WallpaperEngine/Assets/AssetLocator.cpp:9-35 (`AssetLocator::shader`)
/// @ b016d7d1 — adapted (the caller's lookup chain replaces the upstream
/// virtual filesystem's `readString`).
fn resolve_shader_reference(
    lookup: &mut dyn FnMut(&str) -> Option<Vec<u8>>,
    reference: &str,
) -> Option<Vec<u8>> {
    let path = Path::new(reference);
    if let Ok(stripped) = path.strip_prefix("workshop") {
        let mut components = stripped.components();
        if let Some(id) = components.next() {
            let rest: PathBuf = components.collect();
            if let (Some(id), Some(rest)) = (id.as_os_str().to_str(), rest.to_str()) {
                let redirect = format!("zcompat/scene/shaders/{id}/{rest}");
                if let Some(bytes) = lookup(&redirect) {
                    return Some(bytes);
                }
            }
        }
    }
    lookup(&format!("shaders/{reference}"))
}

/// Read one shader stage's top-level source (`shaders/<shader_name>.vert`
/// or `.frag`, through the workshop redirect above) as UTF-8. `None` on
/// any read/decode failure — the caller treats it as one more material
/// fallback reason.
fn read_shader_stage(
    lookup: &mut dyn FnMut(&str) -> Option<Vec<u8>>,
    shader_name: &str,
    extension: &str,
) -> Option<String> {
    let mut path = PathBuf::from(shader_name);
    path.set_extension(extension);
    let reference = path.to_str()?;
    let bytes = resolve_shader_reference(lookup, reference)?;
    String::from_utf8(bytes).ok()
}

/// S4: the material pipeline draws through a flat quad
/// (`vulkan::MATERIAL_UNIT_QUAD`) carrying constant per-vertex data for
/// every attribute name an image-object vertex shader is known to
/// declare: `a_Position`/`a_PositionVec4` (the quad's own xy, z/w
/// implicit), `a_TexCoord`/`a_TexCoordVec4` (the quad's own uv, z/w
/// implicit), `a_Normal` (always `+Z` — the quad is flat, matching
/// upstream's `CImage`/`CPass` convention for a 2D object with no real
/// surface normal), and `a_Color` (always opaque white — the same
/// "no per-vertex tint" default upstream's flat quad geometry carries).
/// Puppet/mesh geometry (bone indices/weights, tangents, the
/// multi-UV-channel `C1`/`C2`/... variants used only by rope/mesh
/// particle shaders) stays out of this slice — zero local-corpus usage
/// on an image object, see `docs/SCENE_FORMAT_V1.md`.
///
/// `attributes` must already be the LIVE set for this material's actual
/// combo values (`shaderpre::preprocess`'s S4 `#if`/`#ifdef` tracking —
/// see `shaderpre::fold_declarations` — filters out attributes behind a
/// combo branch that is not actually taken, so a shader whose
/// `SKINNING`/`MORPHING`/... guard defaults off no longer shows dead
/// attributes here the way the pre-S4 textual scraper did). Every
/// remaining attribute name must be one of the six known names above,
/// with the matching declared type, no duplicate names (a scraper/shader
/// disagreement that arithmetic can't safely fill in for), and at least
/// one position-like and one texcoord-like attribute. Anything else
/// falls back to the S1 base-texture quad.
fn material_vertex_format_supported(attributes: &[shaderpre::AttributeDecl]) -> bool {
    if attributes.is_empty() || attributes.len() > vulkan::KNOWN_MATERIAL_ATTRIBUTES.len() {
        return false;
    }
    let mut seen = std::collections::BTreeSet::new();
    let mut has_position = false;
    let mut has_texcoord = false;
    for attribute in attributes {
        if !seen.insert(attribute.name.as_str()) {
            return false;
        }
        match (attribute.name.as_str(), attribute.glsl_type.as_str()) {
            ("a_Position", "vec2" | "vec3") => has_position = true,
            ("a_PositionVec4", "vec4") => has_position = true,
            ("a_TexCoord", "vec2") => has_texcoord = true,
            ("a_TexCoordVec4", "vec4") => has_texcoord = true,
            ("a_Normal", "vec3") => {}
            ("a_Color", "vec4") => {}
            _ => return false,
        }
    }
    has_position && has_texcoord
}

/// `blending` (`normal`/`translucent`/`additive` in the corpus) onto the
/// existing implemented `BlendMode` set: `additive` maps to `Add`;
/// everything else (including `normal`, which some WE materials use to
/// mean "no extra blending beyond the standard alpha composite" rather
/// than literally opaque) maps to `Normal` — the same alpha-blended
/// src-over every other layer already renders with, since the material
/// pipeline draws through the identical blend-attachment table
/// (`blend_attachment_for`) the S1 quad pipeline uses.
fn material_blend_mode(blending: Option<&str>) -> layers::BlendMode {
    match blending {
        Some("additive") => layers::BlendMode::Add,
        _ => layers::BlendMode::Normal,
    }
}

/// Lenient parse of a `constantshadervalues` entry into up to 4 float
/// components — a WE vector field can be a JSON number, a space-separated
/// string (`"1 0.5 0"`), or a JSON array; components beyond 4 are dropped,
/// missing components stay 0.0. `None` only when the value's shape is
/// unrecognized (an object, a bool, unparseable tokens) — the caller
/// leaves that constant at its shader-side zero default rather than
/// failing the whole material.
fn parse_constant_components(value: &serde_json::Value) -> Option<[f32; 4]> {
    let mut out = [0.0f32; 4];
    if let Some(text) = value.as_str() {
        for (slot, token) in out.iter_mut().zip(text.split_whitespace()) {
            *slot = token.parse::<f32>().ok()?;
        }
        Some(out)
    } else if let Some(number) = value.as_f64() {
        out[0] = number as f32;
        Some(out)
    } else if let Some(array) = value.as_array() {
        for (slot, entry) in out.iter_mut().zip(array.iter()) {
            *slot = entry.as_f64()? as f32;
        }
        Some(out)
    } else {
        None
    }
}

/// S2: for every model layer with resolved material data (`load_model_
/// textures` already populated `layer.material`), attempt to preprocess,
/// compile, and bind its material shader pass. This step only ever ADDS
/// the material-pipeline draw on top of the S1 base-texture quad — a
/// layer whose material falls back for any reason keeps drawing through
/// `texture_ok`/the base texture exactly as S1 left it; nothing here can
/// make a previously-drawable layer stop drawing.
///
/// Bounded: `materialshader::MAX_PIPELINES_PER_SCENE` distinct pipelines
/// per scene (two model layers with the same shader/combos/blend share
/// one pipeline and do not count twice — the cap is checked against
/// compiled pipelines, not attempts); every shader-text/include read goes
/// through `resolve_shader_reference`'s `confined_read` cap; every
/// preprocess call is bounded by `shaderpre`'s own include-depth/size
/// caps; every compile is bounded by `materialshader::MAX_SHADER_TEXT_
/// BYTES`.
///
/// Emits one bounded diagnostic per distinct fallback reason
/// (`event=renderer.scene.shader_fallback reason=... count=N`) and one
/// summary line after every layer has been attempted
/// (`event=renderer.scene.shaders compiled=N fallback=M`) — never one
/// line per layer, matching every other load-time diagnostic in this
/// file.
///
/// S2 review #2: whether a `bind_material_layer` error should terminate
/// the process instead of being treated as an ordinary material
/// fallback -- a `FenceTimeout` means the queue submit backing whatever
/// `bind_material_layer` had already uploaded for this call may still be
/// executing on the GPU, exactly the hazard every other fence-touching
/// call site in this file (`upload_layer_textures`, particle vertex/
/// texture upload, video refresh/texture, text upload, `render()`
/// itself) guards against by calling `reject_render` instead of letting
/// a later `reset_fences`/`queue_submit` reuse the same fence/command
/// buffer out from under a not-yet-complete submission. Pure and
/// unit-tested (`material_bind_fence_timeout_is_fatal_other_errors_are_not`)
/// so a future refactor of the match arm below cannot silently drop this
/// check the way it was originally missing.
fn material_bind_error_is_fatal(error: &RenderError) -> bool {
    is_fence_timeout(error)
}

/// S3: a shader+combos+constants+texture-slots tuple in a shape both the
/// base-material path and an effect pass can produce, so one shared
/// preprocess/compile/bind flow serves both.
struct PlannedMaterial {
    shader: Option<String>,
    blending: Option<String>,
    combos: std::collections::BTreeMap<String, i64>,
    constant_shader_values: Vec<(String, serde_json::Value)>,
    texture_slots: Vec<Option<scene::MaterialTextureSource>>,
}

/// True when `slots` has NOTHING but `_rt_`/render-target texture slots
/// — no real `.tex` bytes anywhere (an empty slice is vacuously true:
/// "no real bytes" holds trivially when there are no slots at all).
/// Shared, pure, unit-tested predicate behind TWO S3 safety decisions in
/// `compile_material_layers` (see each call site's own doc comment):
/// (1) a layer with no resolved effect chain whose material is this
/// bare-passthrough shape never draws at all (the `models/util/
/// fullscreenlayer.json` used WITHOUT `effects[]` case); (2) an effect
/// chain's own final untargeted pass replaces the layer's base material
/// ONLY when the base material is ALSO this bare-passthrough shape (the
/// `copybackground` case) — never when it has a real photo/texture of
/// its own to lose.
fn texture_slots_are_bare_render_target_only(
    slots: &[Option<scene::MaterialTextureSource>],
) -> bool {
    !slots
        .iter()
        .any(|slot| matches!(slot, Some(scene::MaterialTextureSource::Bytes(_))))
}

/// True when at least one slot in `slots` names `_rt_FullFrameBuffer`
/// specifically. S5: marks a layer whose final bound material needs the
/// same-frame snapshot `vulkan::LayerRenderer::render` inserts right
/// before it draws (`ffb_consumer_layers`), and (paired with
/// `texture_slots_reference_only_full_frame_buffer`) whether a bare
/// render-target-only passthrough is safe to draw at all.
fn texture_slots_reference_full_frame_buffer(
    slots: &[Option<scene::MaterialTextureSource>],
) -> bool {
    slots.iter().any(|slot| {
        matches!(slot, Some(scene::MaterialTextureSource::RenderTarget(name)) if name == kwe_core::FULL_FRAME_BUFFER)
    })
}

/// True when `slots` has NO `RenderTarget` slot naming anything OTHER
/// than `_rt_FullFrameBuffer` (vacuously true when it has no
/// render-target slots at all — callers combine this with
/// `texture_slots_reference_full_frame_buffer`, which is false in that
/// case, so the combination is never true for an all-`None`/all-`Bytes`
/// material).
fn texture_slots_reference_only_full_frame_buffer(
    slots: &[Option<scene::MaterialTextureSource>],
) -> bool {
    !slots.iter().any(|slot| {
        matches!(slot, Some(scene::MaterialTextureSource::RenderTarget(name)) if name != kwe_core::FULL_FRAME_BUFFER)
    })
}

/// S3 review MUST-FIX #3, pure and unit-tested: true when `texture_slots`
/// names `target_name` as one of its OWN `RenderTarget` slots — a
/// targeted effect pass sampling the same FBO it renders into is an
/// unguarded Vulkan feedback loop (see the call site's doc comment).
fn effect_pass_samples_its_own_target(
    texture_slots: &[Option<scene::MaterialTextureSource>],
    target_name: &str,
) -> bool {
    texture_slots.iter().any(|slot| {
        matches!(slot, Some(scene::MaterialTextureSource::RenderTarget(name)) if name == target_name)
    })
}

/// S3/S5: one layer's resolved effect chain, walked in scene-declared
/// order (every visible `ObjectEffect`, each effect's `passes[]` in file
/// order, flattened into one list — see `plan_effect_chain`):
/// `final_material` — the LAST material pass with no `target` across the
/// whole chain, which becomes this layer's OWN material (upstream's "no
/// target = draws directly onto the compositor" case is exactly what a
/// layer's own draw already does — folding it in reuses 100% of the
/// existing per-layer pipeline/bind machinery instead of a second draw
/// call). `intermediate` — every material pass WITH a `target`, PLUS (S5)
/// every untargeted pass that is NOT the chain's last one, each
/// compiled+bound to its own FBO each scene load and re-rendered every
/// frame; a targeted pass's FBO is the scene-declared `fbos[]` name
/// (`scoped_target_name`), an intermediate untargeted pass's is this
/// object's own reused <= 2-target ping-pong pair
/// (`pingpong_target_name`) — see `plan_effect_chain`'s doc comment for
/// the full upstream-matching mechanics. `commands` — every
/// `command: copy`/`swap` pass, `(source, target)` with the `"previous"`
/// sentinel already substituted for the concrete FBO name it meant at
/// that point in the chain, plus the original `EffectCommand` (S3
/// review NIT #7: distinguishing `copy` from `swap` at the diagnostic
/// level, since this renderer executes both identically -- see
/// `vulkan.rs`'s `EffectFrameAction::Copy` doc comment for why).
struct EffectChainPlan {
    final_material: Option<PlannedMaterial>,
    intermediate: Vec<(PlannedMaterial, String)>,
    commands: Vec<(String, String, kwe_core::EffectCommand)>,
}

fn combos_as_i64(
    combos: &serde_json::Map<String, serde_json::Value>,
) -> std::collections::BTreeMap<String, i64> {
    combos
        .iter()
        .filter_map(|(name, value)| Some((name.clone(), value.as_i64()?)))
        .collect()
}

/// S3 review RECOMMENDED #5: `effect_targets` (vulkan.rs) is a single
/// scene-wide `HashMap`, so an FBO name written by one object's chain is
/// visible to every other object's chain that happens to reference the
/// same literal name. Scoping every effect-declared FBO name to the
/// LAYER that declared it (except the one deliberately scene-wide name,
/// `_rt_FullFrameBuffer`) makes that aliasing structurally impossible
/// instead of merely "not observed in the local corpus" (the prior
/// wording on `effect_targets`'s own doc comment) — this directly
/// addresses the interaction flagged in `docs/SCENE_FORMAT_V1.md`'s
/// "Scope boundary" note: an object whose chain runs can no longer
/// silently feed a different object's same-named target, because after
/// scoping the two objects' declared names never collide in the first
/// place.
fn scoped_target_name(layer_index: usize, name: &str) -> String {
    if name == kwe_core::FULL_FRAME_BUFFER {
        name.to_string()
    } else {
        format!("{name}#obj{layer_index}")
    }
}

/// Convert one effect pass's resolved texture slots into
/// `scene::MaterialTextureSource`, resolving the `"previous"` sentinel
/// against `previous_source` (module doc comment on `plan_effect_chain`)
/// and scoping any OTHER `RenderTarget` name to `layer_index`
/// (`scoped_target_name`).
fn effect_pass_texture_sources(
    layer_index: usize,
    slots: &[Option<kwe_core::EffectTextureSlot>],
    previous_source: &scene::MaterialTextureSource,
) -> Vec<Option<scene::MaterialTextureSource>> {
    slots
        .iter()
        .map(|slot| {
            slot.as_ref().map(|slot| match slot {
                kwe_core::EffectTextureSlot::Texture { bytes, .. } => {
                    scene::MaterialTextureSource::Bytes(bytes.clone())
                }
                kwe_core::EffectTextureSlot::RenderTarget(name) => {
                    scene::MaterialTextureSource::RenderTarget(scoped_target_name(
                        layer_index,
                        name,
                    ))
                }
                kwe_core::EffectTextureSlot::Previous => previous_source.clone(),
            })
        })
        .collect()
}

/// `"previous"` is a chain-local, PER-SLOT concept (upstream
/// `m_previousInput`/`m_input`, `CPass.cpp:209-244`): for the first pass
/// in an object's whole effect chain, it means "whatever this pass's
/// texture slot would otherwise have resolved to from the object's OWN
/// base material" — NOT unconditionally the scene-wide
/// `_rt_FullFrameBuffer`. Getting this wrong is a real, corpus-observed
/// regression: an ordinary photo layer with an attached colour-grade
/// effect (its base material samples its OWN real photo at slot 0, no
/// `_rt_` anywhere) would have its first pass's `"previous"` wrongly
/// resolve to the one-frame-stale, initially-transparent scene
/// composite instead of that photo, making the whole effect (and thus
/// the layer's final visible content, since the chain's last untargeted
/// pass replaces the layer's own material) render black. Seeding
/// `last_output` from the base material's OWN slot-0 content (falling
/// back to `_rt_FullFrameBuffer` only when that slot is itself empty or
/// already a render target — the `models/util/fullscreenlayer.json`
/// `copybackground` case, e.g. Workshop scene 1652229298) gets both real
/// corpus patterns right with one rule. `layer_index` scopes every
/// effect-declared FBO name this chain touches to this object
/// (`scoped_target_name`, S3 review RECOMMENDED #5) — the base
/// material's OWN slot-0 seed is left UNSCOPED, since it is already
/// object-specific by construction (it came from THIS layer's own
/// `resolve_model` walk, not from a shared `effects[]` namespace).
///
/// Borrowed-From: Almamu/linux-wallpaperengine (GPL-3.0-or-later)
/// src/WallpaperEngine/Render/Objects/CImage.cpp:729-880
/// (`setupPasses`/`pinpongFramebuffer`/`configurePassTarget`: the
/// per-object two-target ping-pong pair an untargeted pass writes into
/// versus a targeted pass's own named FBO, and the "previous"/"input"
/// threading between them) @ b016d7d1 — adapted: this renderer has no
/// "draw to an unnamed target" GPU primitive, so an intermediate
/// untargeted pass (S5, see the `None` match arm below) reuses the SAME
/// named-target machinery a targeted pass uses
/// (`vulkan::LayerRenderer::compile_effect_pass`), against a
/// synthesized per-object name (`pingpong_target_name`) instead of a
/// scene-declared one; upstream's separate base color-blend `CPass` is
/// not materialized as its own pass here — this function's `last_output`
/// seed above plays that role directly.
fn plan_effect_chain(layer_index: usize, layer: &scene::LayerSpec) -> EffectChainPlan {
    let mut last_output = layer
        .material
        .as_ref()
        .and_then(|material| material.texture_slots.first())
        .and_then(|slot| slot.clone())
        .unwrap_or_else(|| {
            scene::MaterialTextureSource::RenderTarget(kwe_core::FULL_FRAME_BUFFER.to_string())
        });
    let mut intermediate = Vec::new();
    let mut commands = Vec::new();
    let mut final_material = None;
    // S5: this object's OWN two-target ping-pong pair (upstream
    // `_rt_imageLayerComposite_<id>_a`/`_b`, `CImage.cpp`'s
    // `m_currentMainFBO`/`m_currentSubFBO`) — `write_slot` is which of
    // the two an UNTARGETED, non-final pass writes into next; it
    // alternates after every such write, exactly like upstream's
    // `pinpongFramebuffer`. A TARGETED pass (one naming an `fbos[]`
    // entry) never touches this pair, matching upstream's `writesToTarget`
    // branch (`configurePassTarget`), which restores the pre-sequence
    // ping-pong state (`prevDrawTo`) once the targeted run ends.
    let mut write_slot: u8 = 0;
    // Flatten every visible effect's passes into ONE ordered list, the
    // same way upstream concatenates every effect's `CPass`es into one
    // per-object `m_passes` (after its own base color-blend pass, which
    // this renderer instead seeds via `last_output` above rather than
    // materializing as its own pass — see this function's own doc
    // comment on why that seed already carries the object's real texture
    // through pass 1 either way). Needed so the LAST untargeted pass —
    // wherever in the whole per-object chain it falls, not merely the
    // last pass of its OWN effect — can be told apart from an
    // INTERMEDIATE untargeted pass that must ping-pong instead of
    // becoming the layer's own material.
    let passes: Vec<&kwe_core::EffectPass> = layer
        .effects
        .iter()
        .filter(|object_effect| object_effect.visible)
        .flat_map(|object_effect| object_effect.effect.passes.iter())
        .collect();
    let last_pass_index = passes.len().checked_sub(1);
    for (pass_index, pass) in passes.into_iter().enumerate() {
        let is_last_pass = Some(pass_index) == last_pass_index;
        match pass {
            kwe_core::EffectPass::Command(command) => {
                // A command's source/target are always meant to be
                // named FBOs (an image copy, not a byte source) — if
                // `last_output` is not itself a named render target
                // at this point (the chain's very first pass and the
                // base material samples a real photo, not a render
                // target), "previous" has no FBO name to substitute;
                // falling through to the literal string is safe
                // (`copy_effect_target` no-ops on an unresolvable
                // name, this module's universal degrade contract). A
                // literal (non-"previous") name is scoped the same
                // way a material pass's texture slots are.
                let resolve = |name: &str| {
                    if name != kwe_core::PREVIOUS_INPUT {
                        return scoped_target_name(layer_index, name);
                    }
                    match &last_output {
                        scene::MaterialTextureSource::RenderTarget(target) => target.clone(),
                        scene::MaterialTextureSource::Bytes(_) => {
                            kwe_core::PREVIOUS_INPUT.to_string()
                        }
                    }
                };
                commands.push((
                    resolve(&command.source),
                    resolve(&command.target),
                    command.command,
                ));
            }
            kwe_core::EffectPass::Material(material_pass) => {
                let texture_slots = effect_pass_texture_sources(
                    layer_index,
                    &material_pass.texture_slots,
                    &last_output,
                );
                let planned = PlannedMaterial {
                    shader: material_pass.shader.clone(),
                    blending: material_pass.blending.clone(),
                    combos: combos_as_i64(&material_pass.combos),
                    constant_shader_values: material_pass
                        .constant_shader_values
                        .iter()
                        .map(|(name, value)| (name.clone(), value.clone()))
                        .collect(),
                    texture_slots,
                };
                match &material_pass.target {
                    Some(target_name) => {
                        let scoped = scoped_target_name(layer_index, target_name);
                        last_output = scene::MaterialTextureSource::RenderTarget(scoped.clone());
                        intermediate.push((planned, scoped));
                    }
                    None if is_last_pass => {
                        // Upstream draws this one straight to the
                        // screen FBO (`setupPasses`'s
                        // `shouldRenderFinalPass` branch); this
                        // renderer's equivalent is "becomes the
                        // layer's own bound material" (main.rs's
                        // caller draws every layer through the normal
                        // per-layer quad either way).
                        final_material = Some(planned);
                    }
                    None => {
                        // S5: an INTERMEDIATE untargeted pass — not
                        // the chain's last pass overall (a later
                        // effect, or a later pass of THIS effect,
                        // still follows). Upstream writes this into
                        // the object's own ping-pong FBO and swaps
                        // (`pinpongFramebuffer`); this renderer has no
                        // "draw to an unnamed target" primitive, so it
                        // reuses the exact same named-target machinery
                        // a targeted pass uses (`compile_effect_pass`
                        // via `intermediate`), just against a
                        // SYNTHESIZED, per-object, REUSED (<= 2 total)
                        // name instead of a scene-declared `fbos[]`
                        // one — see `pingpong_target_name`.
                        let target = pingpong_target_name(layer_index, layer.id, write_slot);
                        last_output = scene::MaterialTextureSource::RenderTarget(target.clone());
                        intermediate.push((planned, target));
                        write_slot = 1 - write_slot;
                    }
                }
            }
        }
    }
    EffectChainPlan {
        final_material,
        intermediate,
        commands,
    }
}

/// This object's own ping-pong render-target name — matches upstream's
/// EXACT naming, `_rt_imageLayerComposite_<id>_a`/`_b` (`slot` 0 = `_a`,
/// the FIRST buffer an object's chain writes into, matching
/// `CImage.cpp`'s own `m_currentMainFBO = m_mainFBO` initial state; `slot`
/// 1 = `_b`). `object_id` is the object's own WE `"id"` field
/// (`scene::LayerSpec::id`) when present, falling back to `layer_index`
/// when absent (defensive; every real corpus object carries one).
///
/// S5 review finding: a REAL corpus scene (Workshop `1131061888`
/// "trigun") explicitly REFERENCES this exact name — its `godrays`
/// effect's own "combine" pass overrides one texture slot to the literal
/// string `_rt_imageLayerComposite_46_a` (`46` being that scene's
/// `trigun` object's own declared `id`) — to read back an earlier state
/// of the SAME per-object ping-pong buffer the rest of its chain is
/// already implicitly threading through `"previous"`. Naming this
/// module's own buffers anything else (an earlier revision used a
/// private, unguessable `__pingpong<N>#obj<index>` name specifically to
/// avoid any possible collision) meant that reference always resolved to
/// the shared, empty `dummy_texture` instead — never a crash, but never
/// the real content either. Matching upstream's name exactly closes
/// that gap: `scoped_target_name` (applied to the base name below, same
/// as any other effect-declared FBO reference) makes a LITERAL
/// `_rt_imageLayerComposite_<id>_a` texture-slot override resolve to the
/// SAME key this function produces for that object, so it samples this
/// renderer's actual ping-pong content instead of nothing.
///
/// Known bounded gap this does NOT fully close: upstream's swap timing
/// for a pass that reads the "off" ping-pong buffer while a chain of
/// TARGETED passes runs in between (as `trigun`'s godrays chain does) is
/// only approximated here — this renderer threads one linear
/// `"previous"`/`last_output` value rather than upstream's separate
/// `input`/`previousInput` pair, so a pass whose OWN declared write
/// target happens to coincide with a literal `_rt_imageLayerComposite_
/// <id>_a`/`_b` reference it also reads (as `trigun`'s combine pass
/// does, in this specific real scene) is caught by the existing
/// feedback-loop guard (`effect_pass_samples_its_own_target`, S3
/// MUST-FIX #3) and degrades to `effect_self_reference` rather than
/// running with a real same-image read+write bind — a genuine Vulkan
/// constraint (undefined without `VK_EXT_attachment_feedback_loop_
/// layout`), not a bug to route around. See `docs/SCENE_FORMAT_V1.md`'s
/// S5 section for the full account.
fn pingpong_target_name(layer_index: usize, object_id: Option<i64>, slot: u8) -> String {
    let id = object_id.unwrap_or(layer_index as i64);
    let letter = if slot == 0 { 'a' } else { 'b' };
    scoped_target_name(
        layer_index,
        &format!("_rt_imageLayerComposite_{id}_{letter}"),
    )
}

/// Build the `MAX_MATERIAL_TEXTURES` texture-bind list AND populate
/// `uniforms.texture_resolution` from a positional slot list — shared by
/// the base/final-material path and every targeted effect pass (S1/S2's
/// original per-slot loop, generalized over `scene::MaterialTextureSource`
/// instead of raw bytes so a `RenderTarget` slot passes straight through
/// instead of needing bytes to decode).
fn build_material_textures(
    slots: &[Option<scene::MaterialTextureSource>],
    uniforms: &mut materialshader::MaterialUniforms,
) -> Vec<Option<MaterialTextureBind>> {
    let mut textures: Vec<Option<MaterialTextureBind>> =
        Vec::with_capacity(shaderpre::MAX_MATERIAL_TEXTURES);
    for slot in slots.iter().take(shaderpre::MAX_MATERIAL_TEXTURES) {
        match slot {
            Some(scene::MaterialTextureSource::Bytes(bytes)) => {
                match texv::decode_model_texture(bytes) {
                    Some(texture) => {
                        let index = textures.len();
                        uniforms.texture_resolution[index] = [
                            texture.width as f32,
                            texture.height as f32,
                            1.0 / (texture.width as f32).max(1.0),
                            1.0 / (texture.height as f32).max(1.0),
                        ];
                        textures.push(Some(MaterialTextureBind::Bytes(
                            texture.rgba,
                            texture.width,
                            texture.height,
                        )));
                    }
                    None => textures.push(None),
                }
            }
            Some(scene::MaterialTextureSource::RenderTarget(name)) => {
                textures.push(Some(MaterialTextureBind::RenderTarget(name.clone())));
            }
            None => textures.push(None),
        }
    }
    while textures.len() < shaderpre::MAX_MATERIAL_TEXTURES {
        textures.push(None);
    }
    textures
}

/// `(vertex SPIR-V, fragment SPIR-V, blend mode, pipeline key, live-branch
/// scraped vertex attributes)` — `compile_one_material`'s success shape,
/// named to satisfy `clippy::type_complexity`. The attribute list feeds
/// `vulkan::LayerRenderer::register_material_pipeline`/`compile_effect_pass`
/// (S4).
type CompiledMaterial = (
    Vec<u32>,
    Vec<u32>,
    layers::BlendMode,
    materialshader::MaterialKey,
    Vec<shaderpre::AttributeDecl>,
);

/// Preprocess+compile one material's vertex/fragment pair (shared by the
/// base/final-material path and every targeted effect pass). `label`
/// (e.g. `"layer[3]"` or `"layer[3] effect[0] pass[2]"`) only reaches
/// diagnostics via `fallback_reasons`' aggregate counts — this function
/// itself is silent per-attempt (matching this file's one-line-per-reason
/// convention). Returns `None` on any failure, having already
/// incremented the matching `fallback_reasons` entry.
/// S7 (P9): how many distinct `"{shader_name}: {error}"` entries
/// `preprocess_failed_detail` keeps — bounded independent of how many
/// materials in a scene fail to preprocess (a hostile/degenerate scene
/// with hundreds of distinct shader names must not grow this set
/// unboundedly), matching this file's other "first N distinct" diags.
const MAX_PREPROCESS_FAILED_DETAILS: usize = 8;

/// Record one `"{shader_name}: {error}"` detail line into a bounded set
/// (S7, P9) — first `MAX_PREPROCESS_FAILED_DETAILS` distinct entries only,
/// each truncated to 160 chars so one pathological error message cannot
/// blow up the eventual one-line diagnostic. Called from both
/// `compile_one_material`'s preprocess failure sites (vertex, fragment).
fn record_preprocess_failed_detail(
    details: &mut std::collections::BTreeSet<String>,
    shader_name: &str,
    error: &shaderpre::PreprocessError,
) {
    if details.len() >= MAX_PREPROCESS_FAILED_DETAILS {
        return;
    }
    let detail = text::truncate_chars(&format!("{shader_name}: {error}"), 160);
    details.insert(detail);
}

#[allow(clippy::too_many_arguments)]
fn compile_one_material(
    lookup: &mut dyn FnMut(&str) -> Option<Vec<u8>>,
    material: &PlannedMaterial,
    fallback_reasons: &mut std::collections::BTreeMap<&'static str, usize>,
    unsupported_uniform_names: &mut std::collections::BTreeSet<String>,
    preprocess_failed_detail: &mut std::collections::BTreeSet<String>,
) -> Option<CompiledMaterial> {
    let shader_name = material.shader.as_deref().or_else(|| {
        *fallback_reasons.entry("no_shader_name").or_insert(0) += 1;
        None
    })?;

    let vertex_source = read_shader_stage(lookup, shader_name, "vert").or_else(|| {
        *fallback_reasons.entry("shader_source_missing").or_insert(0) += 1;
        None
    })?;
    let fragment_source = read_shader_stage(lookup, shader_name, "frag").or_else(|| {
        *fallback_reasons.entry("shader_source_missing").or_insert(0) += 1;
        None
    })?;

    let constant_names: Vec<String> = material
        .constant_shader_values
        .iter()
        .map(|(name, _)| name.clone())
        .take(shaderpre::MAX_MATERIAL_CONSTANTS)
        .collect();
    let mut varying_locations = std::collections::BTreeMap::new();
    let mut include: Box<shaderpre::IncludeLookup<'_>> =
        Box::new(|name: &str| resolve_shader_reference(&mut *lookup, name));

    let vertex_label = format!("{shader_name}.vert");
    let vertex_pre = match shaderpre::preprocess(
        shaderpre::Stage::Vertex,
        &vertex_label,
        &vertex_source,
        &material.combos,
        &constant_names,
        &mut varying_locations,
        &mut include,
    ) {
        Ok(output) => output,
        Err(error) => {
            *fallback_reasons.entry("preprocess_failed").or_insert(0) += 1;
            record_preprocess_failed_detail(preprocess_failed_detail, shader_name, &error);
            return None;
        }
    };
    if !material_vertex_format_supported(&vertex_pre.attributes) {
        *fallback_reasons
            .entry("unsupported_vertex_format")
            .or_insert(0) += 1;
        return None;
    }

    let fragment_label = format!("{shader_name}.frag");
    let fragment_pre = match shaderpre::preprocess(
        shaderpre::Stage::Fragment,
        &fragment_label,
        &fragment_source,
        &material.combos,
        &constant_names,
        &mut varying_locations,
        &mut include,
    ) {
        Ok(output) => output,
        Err(error) => {
            *fallback_reasons.entry("preprocess_failed").or_insert(0) += 1;
            record_preprocess_failed_detail(preprocess_failed_detail, shader_name, &error);
            return None;
        }
    };

    unsupported_uniform_names.extend(vertex_pre.unsupported_uniforms.iter().cloned());
    unsupported_uniform_names.extend(fragment_pre.unsupported_uniforms.iter().cloned());

    // S3: the S2 `render_target_reference` gate (reject any material
    // mentioning `_rt_` at all — S2 had no FBO infrastructure to satisfy
    // one) is gone: a `_rt_`/`Previous` texture slot now resolves to a
    // live FBO view (or the shared dummy texture, never a failure) via
    // `build_material_textures`/`resolve_texture_slots`, so there is
    // nothing left for this gate to protect against.
    let blend_mode = material_blend_mode(material.blending.as_deref());
    let key = materialshader::MaterialKey::compute(
        shader_name,
        &fragment_pre.combos,
        blend_mode.variant_index(),
    );

    let vertex_spirv = match materialshader::compile_stage(
        &vertex_pre.source,
        materialshader::Stage::Vertex,
        &vertex_label,
    ) {
        Ok(spirv) => spirv,
        Err(error) => {
            *fallback_reasons.entry("compile_failed").or_insert(0) += 1;
            // S4 deliverable 5: one bounded, truncated diagnostic line
            // per failed compile (this function runs once per material
            // at scene load, never per frame, so this is a one-time cost
            // matching every other `fallback_reasons` diagnostic in this
            // file) -- lets a maintainer collect the actual `shaderc`
            // error text across the corpus without re-instrumenting.
            eprintln!(
                "event=renderer.scene.shader_compile_error stage=vertex shader={shader_name} detail={}",
                text::truncate_chars(&error.to_string(), 300)
            );
            return None;
        }
    };
    let fragment_spirv = match materialshader::compile_stage(
        &fragment_pre.source,
        materialshader::Stage::Fragment,
        &fragment_label,
    ) {
        Ok(spirv) => spirv,
        Err(error) => {
            *fallback_reasons.entry("compile_failed").or_insert(0) += 1;
            eprintln!(
                "event=renderer.scene.shader_compile_error stage=fragment shader={shader_name} detail={}",
                text::truncate_chars(&error.to_string(), 300)
            );
            return None;
        }
    };
    Some((
        vertex_spirv,
        fragment_spirv,
        blend_mode,
        key,
        vertex_pre.attributes,
    ))
}

/// S3: every `fbos[]` request across every layer's resolved, visible
/// effects, sized to that LAYER's own pixel size / `scale` (upstream
/// `CImage.cpp:652-654` — an effect FBO is sized relative to the OBJECT,
/// not the scene) — `canvas_width`/`canvas_height` (pixels) and
/// `world_width`/`world_height` (scene units, the F1 visible-extent
/// divisor) convert `layer.size` (scene units) to pixels.
fn effect_target_requests(
    layers: &[scene::LayerSpec],
    world_width: f32,
    world_height: f32,
    canvas_width: u32,
    canvas_height: u32,
) -> Vec<EffectTargetRequest> {
    let mut requests = Vec::new();
    let scale_x = if world_width > 0.0 {
        canvas_width as f32 / world_width
    } else {
        1.0
    };
    let scale_y = if world_height > 0.0 {
        canvas_height as f32 / world_height
    } else {
        1.0
    };
    for (layer_index, layer) in layers.iter().enumerate() {
        // S5: this object's own <= 2 ping-pong targets (see
        // `pingpong_target_name`/`plan_effect_chain`), sized to the
        // OBJECT's own pixel size (matching upstream's `_a`/`_b`, and
        // every other request in this loop) — requested unconditionally
        // whenever the object has ANY resolved effect, mirroring
        // upstream creating both unconditionally in `CImage`'s own
        // constructor (whether or not the object's chain ever actually
        // has an intermediate untargeted pass that needs them).
        if !layer.effects.is_empty() {
            let width = (layer.size[0] * scale_x).round().max(1.0) as u32;
            let height = (layer.size[1] * scale_y).round().max(1.0) as u32;
            for slot in 0..2u8 {
                requests.push(EffectTargetRequest {
                    name: pingpong_target_name(layer_index, layer.id, slot),
                    width,
                    height,
                });
            }
        }
        for object_effect in &layer.effects {
            if !object_effect.visible {
                continue;
            }
            for fbo in &object_effect.effect.fbos {
                if fbo.scale <= 0.0 || !fbo.scale.is_finite() {
                    continue;
                }
                let width = ((layer.size[0] * scale_x) / fbo.scale).round().max(1.0) as u32;
                let height = ((layer.size[1] * scale_y) / fbo.scale).round().max(1.0) as u32;
                requests.push(EffectTargetRequest {
                    // S3 review RECOMMENDED #5: scoped the same way
                    // `plan_effect_chain` scopes every reference to this
                    // name, so the target this creates and the target
                    // the chain later renders into/samples are the same
                    // key.
                    name: scoped_target_name(layer_index, &fbo.name),
                    width,
                    height,
                });
            }
        }
    }
    requests
}

/// S2/S3: for every model layer with resolved material data, attempt to
/// preprocess + compile + bind its material shader (S2), and (S3) run
/// every resolved effect chain: targeted passes get their own FBO and
/// are re-rendered every frame (`renderer.render_effect_chains`, called
/// from the main loop); the chain's own final untargeted pass — if any —
/// REPLACES the layer's base material for this compile/bind step (see
/// `EffectChainPlan`'s doc comment). On any failure the layer keeps
/// drawing through the S1 base-texture quad above (`texture_ok` already
/// covers it) — this step only ever ADDS a material-pipeline draw on top,
/// never removes drawability, and an effect chain's own failure degrades
/// to the layer's ordinary material/quad, never a refusal (deliverable 1's
/// honesty rule, upheld at the render layer too).
///
/// Bounded: `materialshader::MAX_PIPELINES_PER_SCENE` distinct base
/// pipelines; `vulkan::MAX_EFFECT_PASS_BINDINGS` distinct effect-pass
/// pipelines; `vulkan::MAX_EFFECT_TARGETS_PER_SCENE`/`MAX_EFFECT_TARGET_
/// BYTES` FBOs — every cap already enforced inside `vulkan.rs`'s own
/// methods, this function just calls them and counts fallbacks.
///
/// Emits one bounded diagnostic per distinct fallback reason
/// (`event=renderer.scene.shader_fallback reason=... count=N`), the S2
/// summary line, and (S3, only when at least one layer has a resolved
/// effect) `event=renderer.scene.effects objects=N passes=M fallback=K`.
/// C2: whether `compile_material_layers` should proceed to bind/draw a
/// layer's own material this frame, given its BASE material's
/// `passthrough`/`fullscreen` flags, whether a resolved effect chain
/// produced a final material for it, the layer's declared size, and the
/// scene's declared resolution (`scene.rs::parse_resolution`, P1). A
/// non-passthrough base material (the overwhelming majority of layers)
/// always returns `Ok(())` — this only changes behaviour for the
/// composelayer/fullscreenlayer/projectlayer model family.
///
/// Upstream never draws a passthrough model bare (its `gl_Position` comes
/// from `a_TexCoord` and it samples `_rt_FullFrameBuffer` at
/// `MVP·a_Position`, so drawing it as an ordinary frame-pass quad paints a
/// fullscreen re-projection of the compositor instead of the object) —
/// `Err("passthrough_without_effects")` when there is no chain to draw
/// instead.
///
/// Borrowed-From: Almamu/linux-wallpaperengine (GPL-3.0-or-later)
/// src/WallpaperEngine/Render/Objects/CImage.cpp:605-606 @ b016d7d1
/// ("passthrough images without effects are bad, do not draw them").
///
/// With a chain, upstream extracts the screen region UNDER the object and
/// runs the chain on that copy (`CImage.cpp` texcoordCopy/
/// copySpacePosition) — this renderer's chain seed is a full-frame
/// snapshot (`_rt_FullFrameBuffer`) instead, which only paints the right
/// content when the object covers the whole scene (a `fullscreen`/
/// `projectlayer` model, or a layer whose declared size is at least the
/// scene's declared resolution on both axes — when the scene has no
/// declared resolution at all, scene units are canvas pixels, so the layer
/// size is compared against the canvas, matching `world_extent`'s own
/// "scene units are canvas pixels" rule). Running the full-frame seed for
/// a smaller SUB-REGION composition layer (e.g. Avatar's 768x768
/// "Adjustable Composition Layer") paints wrong-region garbage — a
/// documented deviation from upstream; the honest degrade is
/// `Err("passthrough_region_unsupported")`, skipping the chain and the
/// draw entirely rather than showing it. Real per-object screen-region
/// extraction is a future slice.
fn passthrough_draw_decision(
    material: Option<&scene::MaterialSpec>,
    has_chain_material: bool,
    layer_size: [f32; 2],
    scene_resolution: Option<(u32, u32)>,
    canvas: (u32, u32),
) -> Result<(), &'static str> {
    let Some(material) = material else {
        return Ok(()); // not a model layer, or its base texture never resolved
    };
    if !material.passthrough {
        return Ok(());
    }
    if !has_chain_material {
        return Err("passthrough_without_effects");
    }
    let covering = material.fullscreen
        || if let Some((scene_width, scene_height)) = scene_resolution {
            layer_size[0] >= scene_width as f32 && layer_size[1] >= scene_height as f32
        } else {
            // No declared scene resolution: scene units are canvas pixels.
            // Compare layer size against the canvas.
            layer_size[0] >= canvas.0 as f32 && layer_size[1] >= canvas.1 as f32
        };
    if covering {
        Ok(())
    } else {
        Err("passthrough_region_unsupported")
    }
}

#[allow(clippy::too_many_arguments)]
fn compile_material_layers(
    layers: &mut [scene::LayerSpec],
    renderer: &mut LayerRenderer,
    world_width: f32,
    world_height: f32,
    canvas_width: u32,
    canvas_height: u32,
    content: &Path,
    assets_dir: Option<&Path>,
    // C2: the scene's declared resolution (P1's `config.resolution`) —
    // used to decide whether a passthrough layer with a resolved effect
    // chain is scene-covering (see the `base_passthrough` block below).
    scene_resolution: Option<(u32, u32)>,
) -> (Vec<bool>, Vec<usize>) {
    let mut material_ok = vec![false; layers.len()];
    // S5: layer indices whose final bound material samples
    // `_rt_FullFrameBuffer` — capped and diagnosed below, then handed to
    // `vulkan::LayerRenderer::render` so it can snapshot the scene
    // composited so far immediately before each one draws.
    let mut ffb_consumer_layers: Vec<usize> = Vec::new();
    let assets_root = assets_dir.and_then(|dir| dir.canonicalize().ok());
    // S3: a scene can bundle its OWN custom shaders inside its `scene.pkg`
    // (the real corpus's godrays/tint effects both do — none of their
    // shaders live in the WE assets tree) — resolve shader source through
    // the SAME pkg-entries -> scene-dir -> assets-root chain
    // `load_model_textures` already uses for models/materials/textures,
    // not the assets root alone. Still bail entirely only when NEITHER
    // source could possibly resolve anything.
    let pkg_reader = if content
        .extension()
        .is_some_and(|extension| extension.eq_ignore_ascii_case("pkg"))
    {
        kwe_core::PkgReader::open(content).ok()
    } else {
        None
    };
    if assets_root.is_none() && pkg_reader.is_none() {
        return (material_ok, ffb_consumer_layers);
    }
    let pkg_dir = content
        .parent()
        .and_then(|parent| parent.canonicalize().ok());
    let mut shader_lookup = move |reference: &str| -> Option<Vec<u8>> {
        if let Some(reader) = &pkg_reader
            && let Ok(index) = kwe_core::image_entry(reference, reader.entries())
            && let Ok(bytes) = reader.read_entry_bounded(index, MAX_SHADER_SOURCE_BYTES)
        {
            return Some(bytes);
        }
        if let Some(dir) = &pkg_dir
            && let Some(bytes) = kwe_core::confined_read(dir, reference, MAX_SHADER_SOURCE_BYTES)
        {
            return Some(bytes);
        }
        assets_root
            .as_deref()
            .and_then(|root| kwe_core::confined_read(root, reference, MAX_SHADER_SOURCE_BYTES))
    };

    let mut compiled = 0usize;
    let mut compiled_pipelines: std::collections::HashSet<u64> = std::collections::HashSet::new();
    let mut fallback_reasons: std::collections::BTreeMap<&'static str, usize> =
        std::collections::BTreeMap::new();
    // Every uniform name `fold_declarations` could not map to a WE
    // standard slot or a material constant (zero-defaulted instead) —
    // deduplicated across every layer attempted, reported once at the
    // end rather than per layer.
    let mut unsupported_uniform_names: std::collections::BTreeSet<String> =
        std::collections::BTreeSet::new();
    // S7 (P9): the actual preprocess error text behind every
    // "preprocess_failed" fallback count — bounded, deduplicated, shared
    // across both the base-material and effect-pass compile call sites
    // (mirrors `unsupported_uniform_names` just above), printed once after
    // the whole loop instead of per-material.
    let mut preprocess_failed_detail: std::collections::BTreeSet<String> =
        std::collections::BTreeSet::new();

    // S3: build every plan up front (pure, no I/O) so the FBO targets it
    // needs can all be created in ONE `prepare_effect_targets` call before
    // any pipeline compiles — a targeted effect pass's own pipeline
    // creation (`compile_effect_pass`) requires its target to already
    // exist.
    let plans: Vec<Option<EffectChainPlan>> = layers
        .iter()
        .enumerate()
        .map(|(layer_index, layer)| {
            if layer.effects.is_empty() {
                None
            } else {
                Some(plan_effect_chain(layer_index, layer))
            }
        })
        .collect();
    let effect_objects = plans.iter().filter(|plan| plan.is_some()).count();
    let requests = effect_target_requests(
        layers,
        world_width,
        world_height,
        canvas_width,
        canvas_height,
    );
    let targets_created = match renderer.prepare_effect_targets(&requests) {
        Ok(created) => created,
        Err(error) => {
            if is_fence_timeout(&error) {
                reject_render(
                    &error,
                    "fence timeout while preparing effect render targets",
                );
            }
            0
        }
    };

    let mut effect_passes_attempted = 0usize;
    let mut effect_fallback_reasons: std::collections::BTreeMap<&'static str, usize> =
        std::collections::BTreeMap::new();
    // S3 review NIT #7: how often a scene actually uses `command: swap`
    // (executed identically to `copy` in this renderer — see
    // `EffectChainPlan`'s doc comment) vs `copy`, where the
    // simplification is a no-op difference.
    let mut swap_used = 0usize;

    for (index, layer) in layers.iter_mut().enumerate() {
        let plan = plans[index].as_ref();
        let has_chain_material = plan.and_then(|p| p.final_material.as_ref()).is_some();
        if let Err(reason) = passthrough_draw_decision(
            layer.material.as_ref(),
            has_chain_material,
            layer.size,
            scene_resolution,
            (canvas_width, canvas_height),
        ) {
            *fallback_reasons.entry(reason).or_insert(0) += 1;
            continue;
        }
        // S5: the effect chain's own final untargeted pass (if any) now
        // ALWAYS becomes the layer's own bound material, regardless of
        // whether the base material had a real texture of its own to
        // "lose" — the S3-era `base_is_passthrough` gate that used to
        // restrict this to bare passthrough objects only (the
        // `copybackground` pattern) is gone. It existed because S3's
        // chain model carried only an opaque `MaterialTextureSource`
        // seed from the base material into pass 1, with no real GPU
        // target for any INTERMEDIATE untargeted pass to render into —
        // an object like Workshop 1131061888's "trigun" (a real photo
        // with four chained effects: waterripple/waterflow/godrays/
        // waterwaves) needs its own untargeted intermediate passes to
        // actually composite, and S3 had nowhere to put them, so letting
        // the chain's last pass win discarded the real photo outright.
        // `plan_effect_chain` (S5) now gives every object with effects a
        // real <= 2-target ping-pong pair (`pingpong_target_name`) and
        // compiles+queues EVERY untargeted pass, not just the final one
        // — pass 1 still samples the base material's own real texture
        // (the seed is unchanged), so the base image survives the whole
        // chain instead of being replaced by it.
        let planned = if let Some(final_material) = plan.and_then(|p| p.final_material.as_ref()) {
            Some(PlannedMaterial {
                shader: final_material.shader.clone(),
                blending: final_material.blending.clone(),
                combos: final_material.combos.clone(),
                constant_shader_values: final_material.constant_shader_values.clone(),
                texture_slots: final_material.texture_slots.clone(),
            })
        } else {
            layer.material.as_ref().map(|material| PlannedMaterial {
                shader: material.shader.clone(),
                blending: material.blending.clone(),
                combos: material.combos.clone(),
                constant_shader_values: material.constant_shader_values.clone(),
                texture_slots: material.texture_slots.clone(),
            })
        };
        let Some(material) = planned else {
            continue; // not a model layer, or its base texture never resolved (load_model_textures)
        };

        // S3 safety guard, narrowed in S5: a layer with NO resolved
        // effect chain whose material has NOTHING but `_rt_`/render-
        // target texture slots (no real `.tex` bytes anywhere) must NOT
        // draw UNLESS its only render-target reference is
        // `_rt_FullFrameBuffer` specifically. This is exactly the real
        // corpus's `models/util/fullscreenlayer.json` used bare (no
        // `effects[]` at all — a `copybackground` recomposite utility
        // layer several scenes place elsewhere in their object stack):
        // pre-S3, its unresolvable `_rt_FullFrameBuffer` slot meant
        // `resolve_model` failed outright and the layer silently never
        // drew (a true no-op, matching upstream's intent when nothing
        // else in the frame needs it "copied back"). S3 kept refusing it
        // even once the slot resolved, because `_rt_FullFrameBuffer` was
        // only ever a ONE-FRAME-STALE, transparent-black-at-startup
        // snapshot — drawing it at any point in the object stack could
        // paint stale/black over real same-frame content. S5 makes
        // `_rt_FullFrameBuffer` a genuine SAME-FRAME snapshot for any
        // layer registered in `ffb_consumer_layers` below
        // (`vulkan::LayerRenderer::render` snapshots the scene-so-far
        // immediately before that layer draws — see its own doc
        // comment), so this exact bare-`copybackground` pattern is now
        // safe: it sees this frame's already-drawn layers below it, not
        // stale data. Any OTHER bare render-target reference (a name
        // this layer's own effects never wrote, which should not occur
        // in a well-formed scene) still has nothing real to show and
        // stays refused. An object WITH a resolved effect chain is fine
        // regardless: its `final_material` (this same `material` value)
        // is the chain's OWN deliberate output, not a blind passthrough.
        let ffb_only_passthrough =
            texture_slots_reference_full_frame_buffer(&material.texture_slots)
                && texture_slots_reference_only_full_frame_buffer(&material.texture_slots);
        if plan.is_none()
            && texture_slots_are_bare_render_target_only(&material.texture_slots)
            && !ffb_only_passthrough
        {
            *fallback_reasons
                .entry("render_target_only_without_effects")
                .or_insert(0) += 1;
            continue;
        }
        // S5: this layer's final bound material samples
        // `_rt_FullFrameBuffer` — register it so `renderer.render` can
        // snapshot the scene composited so far immediately before this
        // layer's own draw (bounded, capped, and diagnosed below).
        if texture_slots_reference_full_frame_buffer(&material.texture_slots) {
            ffb_consumer_layers.push(index);
        }

        let Some((vertex_spirv, fragment_spirv, blend_mode, key, vertex_attributes)) =
            compile_one_material(
                &mut shader_lookup,
                &material,
                &mut fallback_reasons,
                &mut unsupported_uniform_names,
                &mut preprocess_failed_detail,
            )
        else {
            continue;
        };
        if !compiled_pipelines.contains(&key.0)
            && compiled_pipelines.len() >= materialshader::MAX_PIPELINES_PER_SCENE
        {
            *fallback_reasons.entry("pipeline_cap").or_insert(0) += 1;
            continue;
        }
        if renderer
            .register_material_pipeline(
                key.clone(),
                &vertex_spirv,
                &fragment_spirv,
                blend_mode,
                &vertex_attributes,
            )
            .is_err()
        {
            *fallback_reasons
                .entry("pipeline_creation_failed")
                .or_insert(0) += 1;
            continue;
        }
        compiled_pipelines.insert(key.0);

        let mut uniforms = materialshader::MaterialUniforms::default();
        let textures = build_material_textures(&material.texture_slots, &mut uniforms);
        for (slot, (_, value)) in material
            .constant_shader_values
            .iter()
            .enumerate()
            .take(shaderpre::MAX_MATERIAL_CONSTANTS)
        {
            if let Some(components) = parse_constant_components(value) {
                uniforms.material_constants[slot] = components;
            }
        }
        uniforms.mvp = materialshader::build_orthographic_mvp(
            [[1.0, 0.0], [0.0, 1.0]],
            [0.0, 0.0],
            world_width,
            world_height,
        );

        match renderer.bind_material_layer(index, key, &textures, uniforms) {
            Ok(()) => {
                material_ok[index] = true;
                compiled += 1;
            }
            Err(error) => {
                // S2 review #2 (MUST-FIX): every other fence-touching
                // call site in this file (upload_layer_textures,
                // particle vertex/texture upload, video refresh/
                // texture, text upload, render() itself) checks
                // is_fence_timeout and calls reject_render instead of
                // treating the error as an ordinary skip -- a
                // FenceTimeout means the queue submit may still be
                // reading the staging buffer/destination image
                // bind_material_layer already uploaded for this call
                // (vulkan.rs's own doc comment on the identical pattern
                // explains why), so the process must exit immediately
                // rather than let the next render() reset/reuse the
                // same fence and command buffer out from under a
                // possibly still-executing submission.
                if material_bind_error_is_fatal(&error) {
                    reject_render(&error, "fence timeout during material texture upload");
                }
                *fallback_reasons.entry("bind_failed").or_insert(0) += 1;
            }
        }

        // S3: the chain's TARGETED passes — every one of them, in chain
        // order — regardless of whether the final-untargeted pass above
        // compiled successfully (a targeted pass's job is to feed a LATER
        // pass, not necessarily the one this layer draws through).
        let Some(plan) = plan else { continue };
        for (pass_material, target_name) in &plan.intermediate {
            effect_passes_attempted += 1;
            // S3 review MUST-FIX #3: a pass that samples the SAME `_rt_*`
            // target it renders into is an unguarded Vulkan feedback loop
            // (read via descriptor set + write via colour attachment, same
            // image, same render-pass instance) -- undefined per spec
            // absent VK_EXT_attachment_feedback_loop_layout. Reject it the
            // same way an unresolvable reference already degrades
            // (fallback, never a crash), mirroring `copy_effect_target`'s
            // analogous `source == target` guard for the command-pass
            // case.
            if effect_pass_samples_its_own_target(&pass_material.texture_slots, target_name) {
                *effect_fallback_reasons
                    .entry("effect_self_reference")
                    .or_insert(0) += 1;
                continue;
            }
            let Some((vertex_spirv, fragment_spirv, blend_mode, _key, vertex_attributes)) =
                compile_one_material(
                    &mut shader_lookup,
                    pass_material,
                    &mut effect_fallback_reasons,
                    &mut unsupported_uniform_names,
                    &mut preprocess_failed_detail,
                )
            else {
                continue;
            };
            let mut uniforms = materialshader::MaterialUniforms::default();
            let textures = build_material_textures(&pass_material.texture_slots, &mut uniforms);
            for (slot, (_, value)) in pass_material
                .constant_shader_values
                .iter()
                .enumerate()
                .take(shaderpre::MAX_MATERIAL_CONSTANTS)
            {
                if let Some(components) = parse_constant_components(value) {
                    uniforms.material_constants[slot] = components;
                }
            }
            match renderer.compile_effect_pass(
                &vertex_spirv,
                &fragment_spirv,
                blend_mode,
                target_name,
                &textures,
                uniforms,
                &vertex_attributes,
            ) {
                Ok(binding_index) => {
                    // S3 review MUST-FIX #2: `queue_effect_render` is
                    // bounded (`MAX_EFFECT_FRAME_ACTIONS`); a `false`
                    // means the pass compiled+bound fine but the scene's
                    // total per-frame action budget is exhausted -- count
                    // it as a bounded fallback, never silently drop it
                    // without a trace.
                    if !renderer.queue_effect_render(binding_index) {
                        *effect_fallback_reasons
                            .entry("effect_frame_action_cap")
                            .or_insert(0) += 1;
                    }
                }
                Err(error) => {
                    if is_fence_timeout(&error) {
                        reject_render(&error, "fence timeout while compiling an effect pass");
                    }
                    *effect_fallback_reasons.entry("compile_failed").or_insert(0) += 1;
                }
            }
        }
        for (source, target, command) in &plan.commands {
            if *command == kwe_core::EffectCommand::Swap {
                swap_used += 1;
            }
            // S3 review MUST-FIX #2: see the `queue_effect_render` note
            // above -- the same shared per-scene action budget applies to
            // `command` passes, which need neither a shader nor a texture
            // asset to parse (the cheapest possible way to exhaust it).
            if !renderer.queue_effect_copy(source.clone(), target.clone()) {
                *effect_fallback_reasons
                    .entry("effect_frame_action_cap")
                    .or_insert(0) += 1;
            }
        }
    }

    for (reason, count) in &fallback_reasons {
        eprintln!("event=renderer.scene.shader_fallback reason={reason} count={count}");
    }
    if !unsupported_uniform_names.is_empty() {
        eprintln!(
            "event=renderer.scene.shader_unsupported_uniform count={} names={}",
            unsupported_uniform_names.len(),
            unsupported_uniform_names
                .iter()
                .cloned()
                .collect::<Vec<_>>()
                .join(",")
        );
    }
    // S7 (P9): one bounded line naming the actual preprocess failures
    // behind the "preprocess_failed" fallback counts above/below (base
    // material and effect passes share this one set) — never per-material,
    // matching every other load-time-only diagnostic in this file.
    if !preprocess_failed_detail.is_empty() {
        eprintln!(
            "event=renderer.scene.shader_preprocess_failed_detail count={} first={}",
            preprocess_failed_detail.len(),
            preprocess_failed_detail
                .iter()
                .cloned()
                .collect::<Vec<_>>()
                .join(";")
        );
    }
    let fallback_total: usize = fallback_reasons.values().sum();
    eprintln!("event=renderer.scene.shaders compiled={compiled} fallback={fallback_total}");
    if effect_objects > 0 {
        let effect_fallback_total: usize = effect_fallback_reasons.values().sum();
        for (reason, count) in &effect_fallback_reasons {
            eprintln!("event=renderer.scene.effect_fallback reason={reason} count={count}");
        }
        eprintln!(
            "event=renderer.scene.effects objects={effect_objects} passes={effect_passes_attempted} \
             fallback={effect_fallback_total} targets={targets_created} swap_used={swap_used}"
        );
    }
    // S5: bound the number of same-frame `_rt_FullFrameBuffer` snapshots
    // `renderer.render` will insert this frame (each one splits the main
    // render pass and does a full-attachment copy — see that function's
    // own doc comment for why this is bounded rather than "one per
    // consumer"). A scene with more consumers than the cap keeps drawing
    // every layer; only the snapshot TIMING degrades for the excess ones
    // (they see whatever the most recent snapshot held, one documented,
    // bounded fallback among many in this module).
    if ffb_consumer_layers.len() > vulkan::MAX_FULL_FRAME_BUFFER_SNAPSHOTS_PER_FRAME {
        eprintln!(
            "event=renderer.scene.full_frame_buffer_snapshot_cap consumers={} cap={}",
            ffb_consumer_layers.len(),
            vulkan::MAX_FULL_FRAME_BUFFER_SNAPSHOTS_PER_FRAME
        );
        ffb_consumer_layers.truncate(vulkan::MAX_FULL_FRAME_BUFFER_SNAPSHOTS_PER_FRAME);
    }
    (material_ok, ffb_consumer_layers)
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

/// Validate the lexical part of a file-scene video reference and return the
/// candidate path. Callers that open the source must still use
/// `open_video_source`, which performs the no-follow fd and post-open
/// identity checks before copying any bytes.
fn video_candidate(root: &Path, reference: &str) -> Result<PathBuf, String> {
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
    Ok(root.join(joined))
}

/// Resolve one layer's video reference for diagnostics/tests. Production
/// file scenes use `stage_file_video` instead: canonicalize-then-open would
/// leave a TOCTOU window between validation and libmpv's later path open.
#[cfg(test)]
fn resolve_layer_video(root: &Path, reference: &str) -> Result<PathBuf, String> {
    let candidate = video_candidate(root, reference)?;
    let canonical = candidate
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

/// Open a file-scene video once, with no symlink following on the final
/// component, and verify that the opened inode is still the canonical path
/// inside the content root. The fd is then copied, so libmpv only receives a
/// worker-owned immutable snapshot and never reopens attacker-controlled
/// content paths.
fn open_video_source(root: &Path, reference: &str) -> Result<fs::File, String> {
    let candidate = video_candidate(root, reference)?;
    let source = fs::OpenOptions::new()
        .read(true)
        .custom_flags(libc::O_NOFOLLOW)
        .open(&candidate)
        .map_err(|error| format!("video \"{reference}\" is missing or unreadable: {error}"))?;
    let opened = source
        .metadata()
        .map_err(|error| format!("video \"{reference}\" is unreadable: {error}"))?;
    if !opened.is_file() {
        return Err(format!("video \"{reference}\" is not a regular file"));
    }
    if opened.len() > video::MAX_VIDEO_SOURCE_BYTES {
        return Err(format!(
            "video \"{reference}\" is {} bytes, over the {} byte cap",
            opened.len(),
            video::MAX_VIDEO_SOURCE_BYTES
        ));
    }
    let canonical = candidate
        .canonicalize()
        .map_err(|error| format!("video \"{reference}\" changed during validation: {error}"))?;
    if !canonical.starts_with(root) {
        return Err(format!(
            "video \"{reference}\" resolves outside the scene directory"
        ));
    }
    let canonical_metadata = fs::metadata(&canonical)
        .map_err(|error| format!("video \"{reference}\" changed during validation: {error}"))?;
    if !canonical_metadata.is_file()
        || canonical_metadata.dev() != opened.dev()
        || canonical_metadata.ino() != opened.ino()
    {
        return Err(format!("video \"{reference}\" changed during validation"));
    }
    Ok(source)
}

/// Copy one validated source into the worker-owned video directory. The
/// extra byte read is intentional: a concurrently growing source cannot
/// bypass the source cap merely because its initial metadata was smaller.
fn stage_file_video(root: &Path, reference: &str, slot: usize) -> Result<PathBuf, String> {
    let mut source = open_video_source(root, reference)?;
    let home = std::env::var_os("HOME")
        .map(PathBuf::from)
        .unwrap_or_else(std::env::temp_dir);
    copy_video_file_into(&home, slot, &mut source).map_err(|error| {
        format!("cannot stage video \"{reference}\" into the worker directory: {error}")
    })
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
    if bytes.len() as u64 > video::MAX_VIDEO_SOURCE_BYTES {
        return Err(std::io::Error::new(
            std::io::ErrorKind::InvalidInput,
            "video source exceeds the bounded extraction limit",
        ));
    }
    let dir = ensure_video_dir(home)?;
    let path = dir.slot_path(slot);
    let mut file = dir.create_slot(slot)?;
    file.write_all(bytes)?;
    Ok(path)
}

/// Stream a validated file descriptor into the same private slot layout as
/// package extraction. No whole-source allocation is needed, and a source
/// that grows while being copied is rejected and removed.
fn copy_video_file_into(
    home: &Path,
    slot: usize,
    source: &mut fs::File,
) -> std::io::Result<PathBuf> {
    if source.metadata()?.len() > video::MAX_VIDEO_SOURCE_BYTES {
        return Err(std::io::Error::new(
            std::io::ErrorKind::InvalidInput,
            "video source exceeds the bounded extraction limit",
        ));
    }
    let dir = ensure_video_dir(home)?;
    let path = dir.slot_path(slot);
    let mut output = dir.create_slot(slot)?;
    // Write at most the kernel file-size limit. Probe one additional source
    // byte only after the bounded copy; it must never be written to the
    // destination, otherwise RLIMIT_FSIZE can kill the worker at exactly the
    // contract boundary.
    let copied = std::io::copy(&mut source.take(video::MAX_VIDEO_SOURCE_BYTES), &mut output);
    match copied {
        Ok(bytes) if bytes == video::MAX_VIDEO_SOURCE_BYTES => {
            let mut extra = [0u8; 1];
            match source.read(&mut extra) {
                Ok(0) => Ok(path),
                Ok(_) => {
                    dir.unlink_slot(slot);
                    Err(std::io::Error::new(
                        std::io::ErrorKind::InvalidData,
                        "video source grew beyond the bounded extraction limit",
                    ))
                }
                Err(error) => {
                    dir.unlink_slot(slot);
                    Err(error)
                }
            }
        }
        Ok(_) => {
            // A short source is complete after the bounded copy. No extra
            // byte was written, and the subsequent probe distinguishes an
            // exact-cap source from one that grew during the read.
            let mut extra = [0u8; 1];
            match source.read(&mut extra) {
                Ok(0) => Ok(path),
                Ok(_) => {
                    dir.unlink_slot(slot);
                    Err(std::io::Error::new(
                        std::io::ErrorKind::InvalidData,
                        "video source grew beyond the bounded extraction limit",
                    ))
                }
                Err(error) => {
                    dir.unlink_slot(slot);
                    Err(error)
                }
            }
        }
        Err(error) => {
            dir.unlink_slot(slot);
            Err(error)
        }
    }
}

/// Open the worker-private extraction directory and perform all slot
/// operations relative to its stable fd. This closes the intermediate
/// directory-symlink swap window left by pathname remove/open calls.
struct VideoDir {
    path: PathBuf,
    fd: RawFd,
}

impl Drop for VideoDir {
    fn drop(&mut self) {
        // SAFETY: fd was returned by libc::open and is owned by this value.
        unsafe { libc::close(self.fd) };
    }
}

impl VideoDir {
    fn slot_path(&self, slot: usize) -> PathBuf {
        self.path.join(format!("video-{slot}.bin"))
    }

    fn slot_name(slot: usize) -> CString {
        CString::new(format!("video-{slot}.bin")).expect("slot name has no NUL")
    }

    fn create_slot(&self, slot: usize) -> std::io::Result<fs::File> {
        let name = Self::slot_name(slot);
        // SAFETY: self.fd is an open directory fd and name is NUL-terminated.
        let unlinked = unsafe { libc::unlinkat(self.fd, name.as_ptr(), 0) };
        if unlinked != 0 {
            let error = std::io::Error::last_os_error();
            if error.raw_os_error() != Some(libc::ENOENT) {
                return Err(error);
            }
        }
        // SAFETY: openat creates the slot beneath the already-open directory;
        // O_NOFOLLOW and O_EXCL prevent symlink redirection or replacement.
        let fd = unsafe {
            libc::openat(
                self.fd,
                name.as_ptr(),
                libc::O_WRONLY | libc::O_CREAT | libc::O_EXCL | libc::O_NOFOLLOW | libc::O_CLOEXEC,
                0o600,
            )
        };
        if fd < 0 {
            return Err(std::io::Error::last_os_error());
        }
        // SAFETY: fd is a fresh, exclusively-created file descriptor now
        // owned by the returned File.
        Ok(unsafe { fs::File::from_raw_fd(fd) })
    }

    fn unlink_slot(&self, slot: usize) {
        let name = Self::slot_name(slot);
        // SAFETY: unlinkat acts only below this stable directory fd.
        let _ = unsafe { libc::unlinkat(self.fd, name.as_ptr(), 0) };
    }
}

fn ensure_video_dir(home: &Path) -> std::io::Result<VideoDir> {
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
    let meta = fs::symlink_metadata(&dir)?;
    if !meta.is_dir() || meta.file_type().is_symlink() {
        return Err(std::io::Error::new(
            std::io::ErrorKind::InvalidData,
            "video directory changed to a symlink or non-directory",
        ));
    }
    let c_path = CString::new(dir.as_os_str().as_bytes()).map_err(|_| {
        std::io::Error::new(
            std::io::ErrorKind::InvalidInput,
            "video directory contains NUL",
        )
    })?;
    // SAFETY: c_path is a valid path; flags open exactly the plain directory
    // just validated and never follow a final symlink.
    let fd = unsafe {
        libc::open(
            c_path.as_ptr(),
            libc::O_RDONLY | libc::O_DIRECTORY | libc::O_NOFOLLOW | libc::O_CLOEXEC,
        )
    };
    if fd < 0 {
        return Err(std::io::Error::last_os_error());
    }
    let mut stat = std::mem::MaybeUninit::<libc::stat>::uninit();
    // SAFETY: stat points to writable storage and fd is valid.
    let stat_result = unsafe { libc::fstat(fd, stat.as_mut_ptr()) };
    if stat_result != 0 {
        let error = std::io::Error::last_os_error();
        // SAFETY: fd is owned here after open.
        unsafe { libc::close(fd) };
        return Err(error);
    }
    // SAFETY: fstat succeeded and initialized stat.
    let stat = unsafe { stat.assume_init() };
    if (stat.st_mode & libc::S_IFMT) != libc::S_IFDIR
        || stat.st_dev != meta.dev()
        || stat.st_ino != meta.ino()
    {
        // SAFETY: fd is owned here after open.
        unsafe { libc::close(fd) };
        return Err(std::io::Error::new(
            std::io::ErrorKind::InvalidData,
            "video directory changed during staging",
        ));
    }
    Ok(VideoDir { path: dir, fd })
}

struct VideoCleanupGuard {
    home: PathBuf,
}

impl VideoCleanupGuard {
    fn new() -> Self {
        Self {
            home: std::env::var_os("HOME")
                .map(PathBuf::from)
                .unwrap_or_else(std::env::temp_dir),
        }
    }
}

impl Drop for VideoCleanupGuard {
    fn drop(&mut self) {
        cleanup_video_dir_in(&self.home);
    }
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

    /// S2 review #2 (MUST-FIX): the guard `compile_material_layers` runs
    /// before deciding whether a `bind_material_layer` failure is an
    /// ordinary fallback or a process-terminating fence timeout. A real
    /// `FenceTimeout` cannot be produced without a live Vulkan device
    /// mid-submission (a mock is impractical here, per the review's own
    /// note), so this pins the pure decision function directly: it must
    /// stay `true` for `FenceTimeout` and `false` for every other
    /// `RenderError`, matching every other fence-touching call site in
    /// this file.
    #[test]
    fn material_bind_fence_timeout_is_fatal_other_errors_are_not() {
        assert!(material_bind_error_is_fatal(
            &vulkan::RenderError::FenceTimeout
        ));
        assert!(!material_bind_error_is_fatal(&vulkan::RenderError::Vulkan(
            "out of memory".to_string()
        )));
    }

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

    // ---- S7 (P9): bounded shader_preprocess_failed_detail diagnostic ----

    #[test]
    fn record_preprocess_failed_detail_is_bounded_and_truncates() {
        let mut details = std::collections::BTreeSet::new();
        for i in 0..(MAX_PREPROCESS_FAILED_DETAILS + 5) {
            record_preprocess_failed_detail(
                &mut details,
                &format!("shader{i}"),
                &shaderpre::PreprocessError::IncludeDepthExceeded,
            );
        }
        assert_eq!(details.len(), MAX_PREPROCESS_FAILED_DETAILS);

        let mut details = std::collections::BTreeSet::new();
        let long_name = "x".repeat(500);
        record_preprocess_failed_detail(
            &mut details,
            &long_name,
            &shaderpre::PreprocessError::IncludeDepthExceeded,
        );
        let entry = details.iter().next().unwrap();
        assert!(
            entry.chars().count() <= 160,
            "detail entry must truncate to 160 chars, got {}",
            entry.chars().count()
        );
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

    // ---- S7 (P4): particle-file material blending/overbright ----

    /// A minimal `ParticleSpec` naming an external particle file reference
    /// (S4b) — every other field takes the M3f flat-model default, mirroring
    /// `scene::particle_spec_defaults` (private to scene.rs, so this test
    /// helper duplicates the defaults directly rather than reaching in).
    fn particle_spec_with_file_ref(file_ref: &str) -> scene::ParticleSpec {
        scene::ParticleSpec {
            name: "test".to_string(),
            scene_order: 0,
            origin: [0.0, 0.0],
            spawn_rate: particles::DEFAULT_PARTICLE_SPAWN_RATE,
            life: particles::DEFAULT_PARTICLE_LIFE,
            speed_min: particles::DEFAULT_PARTICLE_SPEED,
            speed_max: particles::DEFAULT_PARTICLE_SPEED,
            direction: particles::DEFAULT_PARTICLE_DIRECTION,
            spread: particles::DEFAULT_PARTICLE_SPREAD,
            gravity: particles::DEFAULT_PARTICLE_GRAVITY,
            size_start: particles::DEFAULT_PARTICLE_SIZE,
            size_end: particles::DEFAULT_PARTICLE_SIZE,
            color_start: [1.0, 1.0, 1.0, 1.0],
            color_end: [1.0, 1.0, 1.0, 1.0],
            alpha_start: particles::DEFAULT_PARTICLE_ALPHA_START,
            alpha_end: particles::DEFAULT_PARTICLE_ALPHA_END,
            material: None,
            max_count: particles::DEFAULT_PARTICLE_MAX_COUNT,
            blend_mode: layers::BlendMode::Normal.as_u32(),
            alpha: 1.0,
            visible: true,
            brightness: 1.0,
            texture: None,
            file_ref: Some(file_ref.to_string()),
            component: None,
            instance_count: 1.0,
            instance_rate: 1.0,
            instance_size: 1.0,
            instance_lifetime: 1.0,
            instance_speed: 1.0,
            instance_alpha: 1.0,
            instance_colorn: 1.0,
            scale: [1.0, 1.0],
        }
    }

    /// A particle-file/material/texture fixture resolver: `particles/p.json`
    /// names `materials/p.json`, which declares `blending` and
    /// `constantshadervalues.ui_editor_properties_overbright` from the two
    /// arguments, and points its slot-0 texture at `materials/p.tex` (a
    /// real decodable PNG — `tiny_png()` — so the texture-decode step that
    /// gates `load_particle_file_definitions` succeeds).
    fn particle_material_fixture(
        blending: &str,
        overbright: &str,
    ) -> impl FnMut(&str) -> Option<Vec<u8>> {
        let png = tiny_png();
        move |reference: &str| {
            match reference {
            "particles/p.json" => {
                Some(br#"{"material": "materials/p.json", "maxcount": 100}"#.to_vec())
            }
            "materials/p.json" => Some(
                format!(
                    r#"{{"passes": [{{"textures": ["p"], "blending": "{blending}",
                         "constantshadervalues": {{"ui_editor_properties_overbright": {overbright}}}}}]}}"#
                )
                .into_bytes(),
            ),
            "materials/p.tex" => Some(png.clone()),
            _ => None,
        }
        }
    }

    /// S7 (P4): the material's own `blending` — not the object's
    /// `colorBlendMode` — decides a file-based particle system's blend
    /// mode. `additive` -> `BlendMode::Add` (WE 6); before this fix every
    /// file-based system drew with plain alpha (0) regardless of the
    /// material, so an additive sprite's black background painted an
    /// opaque black box.
    #[test]
    fn particle_file_material_additive_blending_maps_to_add() {
        let mut systems = vec![particle_spec_with_file_ref("particles/p.json")];
        let mut used_bytes = 0u64;
        let resolved = load_particle_file_definitions(
            &mut systems,
            &mut used_bytes,
            particle_material_fixture("additive", "1.0"),
        );
        assert_eq!(resolved, 1);
        assert_eq!(systems[0].blend_mode, layers::BlendMode::Add.as_u32());
    }

    #[test]
    fn particle_file_material_translucent_blending_maps_to_normal() {
        let mut systems = vec![particle_spec_with_file_ref("particles/p.json")];
        let mut used_bytes = 0u64;
        let resolved = load_particle_file_definitions(
            &mut systems,
            &mut used_bytes,
            particle_material_fixture("translucent", "1.0"),
        );
        assert_eq!(resolved, 1);
        assert_eq!(systems[0].blend_mode, layers::BlendMode::Normal.as_u32());
    }

    /// S7 (P4): `ui_editor_properties_overbright` multiplies the drawn
    /// brightness (folded into the existing `brightness` field, which
    /// defaults to 1.0 for a flat-model particle system, so 0.25 -> 0.25).
    #[test]
    fn particle_file_material_overbright_multiplies_brightness() {
        let mut systems = vec![particle_spec_with_file_ref("particles/p.json")];
        let mut used_bytes = 0u64;
        let resolved = load_particle_file_definitions(
            &mut systems,
            &mut used_bytes,
            particle_material_fixture("translucent", "0.25"),
        );
        assert_eq!(resolved, 1);
        assert!(
            (systems[0].brightness - 0.25).abs() < 1e-6,
            "brightness={}",
            systems[0].brightness
        );
    }

    /// C2: a passthrough base material with NO resolved effect chain must
    /// never draw — upstream never draws a passthrough model bare
    /// (CImage.cpp:605-606).
    #[test]
    fn passthrough_draw_decision_without_a_chain_is_refused() {
        let material = scene::MaterialSpec {
            passthrough: true,
            ..Default::default()
        };
        assert_eq!(
            passthrough_draw_decision(
                Some(&material),
                false,
                [768.0, 768.0],
                Some((3840, 2160)),
                (960, 540)
            ),
            Err("passthrough_without_effects")
        );
    }

    /// C2: a passthrough base material declared `fullscreen` (or
    /// `projectlayer`, which `resolve_model` folds into `fullscreen`) with
    /// a resolved chain still resolves — the object covers the whole
    /// scene, so the full-frame-buffer seed paints the right content.
    #[test]
    fn passthrough_draw_decision_fullscreen_with_a_chain_resolves() {
        let material = scene::MaterialSpec {
            passthrough: true,
            fullscreen: true,
            ..Default::default()
        };
        assert_eq!(
            passthrough_draw_decision(
                Some(&material),
                true,
                [64.0, 64.0],
                Some((3840, 2160)),
                (960, 540)
            ),
            Ok(())
        );
    }

    /// C2: a passthrough base material whose declared size covers the
    /// scene on both axes (even without the `fullscreen` flag) also
    /// resolves.
    #[test]
    fn passthrough_draw_decision_size_covering_the_scene_resolves() {
        let material = scene::MaterialSpec {
            passthrough: true,
            ..Default::default()
        };
        assert_eq!(
            passthrough_draw_decision(
                Some(&material),
                true,
                [3840.0, 2160.0],
                Some((3840, 2160)),
                (960, 540)
            ),
            Ok(())
        );
    }

    /// C2 root cause case: a SUB-REGION passthrough composition layer
    /// (e.g. Avatar's 768x768 "Adjustable Composition Layer" against a
    /// 3840x2160 scene) with a resolved chain must skip both the chain and
    /// the draw — this renderer's full-frame-buffer chain seed would paint
    /// wrong-region garbage for anything smaller than the whole scene.
    #[test]
    fn passthrough_draw_decision_sub_region_with_a_chain_is_refused() {
        let material = scene::MaterialSpec {
            passthrough: true,
            ..Default::default()
        };
        assert_eq!(
            passthrough_draw_decision(
                Some(&material),
                true,
                [768.0, 768.0],
                Some((3840, 2160)),
                (960, 540)
            ),
            Err("passthrough_region_unsupported")
        );
    }

    /// C2: no declared scene resolution (`general.resolution`/
    /// `orthogonalprojection` absent) compares the layer size against the
    /// canvas (scene units are canvas pixels), matching `world_extent`'s own
    /// "scene units are canvas pixels" rule. A layer whose size covers the
    /// canvas on both axes draws.
    #[test]
    fn passthrough_draw_decision_no_scene_resolution_layer_size_covers_canvas_resolves() {
        let material = scene::MaterialSpec {
            passthrough: true,
            ..Default::default()
        };
        assert_eq!(
            passthrough_draw_decision(Some(&material), true, [960.0, 540.0], None, (960, 540)),
            Ok(())
        );
    }

    /// C2: no declared scene resolution with a layer whose size does not
    /// cover the canvas is refused, avoiding wrong-region garbage.
    #[test]
    fn passthrough_draw_decision_no_scene_resolution_layer_size_smaller_than_canvas_is_refused() {
        let material = scene::MaterialSpec {
            passthrough: true,
            ..Default::default()
        };
        assert_eq!(
            passthrough_draw_decision(Some(&material), true, [1.0, 1.0], None, (960, 540)),
            Err("passthrough_region_unsupported")
        );
    }

    /// C2: a non-passthrough material is never affected by any of the
    /// above, chain or no chain.
    #[test]
    fn passthrough_draw_decision_non_passthrough_always_resolves() {
        let material = scene::MaterialSpec::default();
        assert_eq!(
            passthrough_draw_decision(
                Some(&material),
                false,
                [1.0, 1.0],
                Some((3840, 2160)),
                (960, 540)
            ),
            Ok(())
        );
    }

    /// One effect chain with a single untargeted material pass whose only
    /// texture slot is the `"previous"` bind sentinel — the minimal shape
    /// that exercises `plan_effect_chain`'s seeding rule.
    fn single_previous_pass_effect() -> kwe_core::ObjectEffect {
        kwe_core::ObjectEffect {
            id: 1,
            name: "test".into(),
            visible: true,
            effect: kwe_core::EffectSpec {
                name: "test".into(),
                fbos: Vec::new(),
                passes: vec![kwe_core::EffectPass::Material(
                    kwe_core::EffectMaterialPass {
                        material_ref: "materials/effects/test.json".into(),
                        shader: Some("test".into()),
                        blending: None,
                        combos: serde_json::Map::new(),
                        constant_shader_values: serde_json::Map::new(),
                        texture_slots: vec![Some(kwe_core::EffectTextureSlot::Previous)],
                        target: None,
                    },
                )],
            },
        }
    }

    #[test]
    fn plan_effect_chain_seeds_previous_from_the_objects_own_base_texture() {
        let mut base = layer("photo", None);
        base.material = Some(scene::MaterialSpec {
            texture_slots: vec![Some(scene::MaterialTextureSource::Bytes(vec![1, 2, 3, 4]))],
            ..Default::default()
        });
        base.effects = vec![single_previous_pass_effect()];
        let plan = plan_effect_chain(0, &base);
        let final_material = plan.final_material.expect("one untargeted pass");
        assert!(matches!(
            &final_material.texture_slots[0],
            Some(scene::MaterialTextureSource::Bytes(bytes)) if bytes == &[1, 2, 3, 4]
        ));
    }

    #[test]
    fn plan_effect_chain_seeds_previous_from_full_frame_buffer_when_base_has_no_real_texture() {
        let mut base = layer("passthrough", None);
        base.material = Some(scene::MaterialSpec {
            texture_slots: vec![Some(scene::MaterialTextureSource::RenderTarget(
                "_rt_FullFrameBuffer".into(),
            ))],
            ..Default::default()
        });
        base.effects = vec![single_previous_pass_effect()];
        let plan = plan_effect_chain(0, &base);
        let final_material = plan.final_material.expect("one untargeted pass");
        assert!(matches!(
            &final_material.texture_slots[0],
            Some(scene::MaterialTextureSource::RenderTarget(name)) if name == "_rt_FullFrameBuffer"
        ));

        // Same result when the base material has NO texture slots at all.
        let mut no_slots = layer("no-slots", None);
        no_slots.material = Some(scene::MaterialSpec::default());
        no_slots.effects = vec![single_previous_pass_effect()];
        let plan2 = plan_effect_chain(0, &no_slots);
        let final_material2 = plan2.final_material.expect("one untargeted pass");
        assert!(matches!(
            &final_material2.texture_slots[0],
            Some(scene::MaterialTextureSource::RenderTarget(name)) if name == "_rt_FullFrameBuffer"
        ));
    }

    #[test]
    fn bare_render_target_passthrough_is_detected_and_a_real_texture_is_not() {
        let render_target_only = vec![
            Some(scene::MaterialTextureSource::RenderTarget(
                "_rt_FullFrameBuffer".into(),
            )),
            None,
        ];
        assert!(texture_slots_are_bare_render_target_only(
            &render_target_only
        ));

        let with_real_texture = vec![
            Some(scene::MaterialTextureSource::RenderTarget(
                "_rt_Something".into(),
            )),
            Some(scene::MaterialTextureSource::Bytes(vec![9, 9, 9, 9])),
        ];
        assert!(!texture_slots_are_bare_render_target_only(
            &with_real_texture
        ));

        // No slots at all is also "nothing but render targets" (vacuously
        // true) — the caller's `plan.is_none()` guard is what actually
        // matters for whether this material draws.
        assert!(texture_slots_are_bare_render_target_only(&[]));
    }

    #[test]
    fn effect_final_material_only_overrides_a_bare_passthrough_base() {
        // S5: `plan_effect_chain`'s `final_material` is populated
        // regardless of whether the base material has a real texture of
        // its own — the S3-era `base_is_passthrough` gate that used to
        // ignore it for real-texture objects lived in
        // `compile_material_layers`'s CALLER-side selection, not in
        // `plan_effect_chain` itself, and has been removed (see
        // `plan_effect_chain_ping_pongs_multiple_untargeted_passes_
        // reusing_two_targets` below for the real fix: pass 1 samples
        // the base photo's own real bytes either way, so nothing is
        // discarded — matching Workshop 1131061888's "trigun" photo with
        // its four attached effects).
        let mut photo = layer("photo", None);
        photo.material = Some(scene::MaterialSpec {
            shader: Some("photo_shader".into()),
            texture_slots: vec![Some(scene::MaterialTextureSource::Bytes(vec![1, 2, 3, 4]))],
            ..Default::default()
        });
        photo.effects = vec![single_previous_pass_effect()];
        let plan = plan_effect_chain(0, &photo);
        assert!(
            plan.final_material.is_some(),
            "a real-texture base must still resolve a final_material from its effect chain"
        );

        // A bare `copybackground` passthrough (no real texture) also
        // resolves one, the same way.
        let mut passthrough = layer("passthrough", None);
        passthrough.material = Some(scene::MaterialSpec {
            shader: Some("passthrough_shader".into()),
            texture_slots: vec![Some(scene::MaterialTextureSource::RenderTarget(
                "_rt_FullFrameBuffer".into(),
            ))],
            ..Default::default()
        });
        passthrough.effects = vec![single_previous_pass_effect()];
        let plan = plan_effect_chain(0, &passthrough);
        assert!(plan.final_material.is_some());
    }

    /// Build an `ObjectEffect` from an ordered list of untargeted-vs-
    /// targeted markers: `None` for an untargeted pass (`target: None`),
    /// `Some(name)` for a pass targeting the named FBO — shared by the
    /// S5 ping-pong tests below, which only care about each pass's
    /// target shape, not its shader/combos/constants.
    fn effect_with_pass_targets(targets: &[Option<&str>]) -> kwe_core::ObjectEffect {
        kwe_core::ObjectEffect {
            id: 1,
            name: "test".into(),
            visible: true,
            effect: kwe_core::EffectSpec {
                name: "test".into(),
                fbos: Vec::new(),
                passes: targets
                    .iter()
                    .map(|target| {
                        kwe_core::EffectPass::Material(kwe_core::EffectMaterialPass {
                            material_ref: "materials/effects/test.json".into(),
                            shader: Some("test".into()),
                            blending: None,
                            combos: serde_json::Map::new(),
                            constant_shader_values: serde_json::Map::new(),
                            texture_slots: vec![Some(kwe_core::EffectTextureSlot::Previous)],
                            target: target.map(str::to_string),
                        })
                    })
                    .collect(),
            },
        }
    }

    /// S5: TWO untargeted passes on a real-texture base — the first is
    /// NOT the chain's last pass, so it must ping-pong into this
    /// object's own reused target (`pingpong_target_name`) instead of
    /// becoming `final_material` outright; the second (the true last
    /// pass) becomes `final_material` and samples the first pass's
    /// ping-pong output as its `"previous"` input. This is the exact
    /// mechanism that lets a multi-effect real-texture object (Workshop
    /// 1131061888's "trigun") composite correctly instead of losing its
    /// base photo.
    #[test]
    fn plan_effect_chain_ping_pongs_multiple_untargeted_passes_reusing_two_targets() {
        let mut base = layer("photo", None);
        base.material = Some(scene::MaterialSpec {
            texture_slots: vec![Some(scene::MaterialTextureSource::Bytes(vec![1, 2, 3, 4]))],
            ..Default::default()
        });
        base.effects = vec![effect_with_pass_targets(&[None, None])];
        let plan = plan_effect_chain(0, &base);

        assert_eq!(
            plan.intermediate.len(),
            1,
            "only the FIRST (non-last) untargeted pass is intermediate"
        );
        let (first_pass, first_target) = &plan.intermediate[0];
        assert_eq!(first_target, &pingpong_target_name(0, None, 0));
        // Pass 1 samples the base photo's OWN real bytes, not a stale/
        // empty render target — the fix that keeps the photo alive.
        assert!(matches!(
            &first_pass.texture_slots[0],
            Some(scene::MaterialTextureSource::Bytes(bytes)) if bytes == &[1, 2, 3, 4]
        ));

        let final_material = plan
            .final_material
            .expect("second pass is the chain's last");
        assert!(matches!(
            &final_material.texture_slots[0],
            Some(scene::MaterialTextureSource::RenderTarget(name)) if name == &pingpong_target_name(0, None, 0)
        ));
    }

    /// S5 bound: however many intermediate untargeted passes a chain has,
    /// they cycle through exactly TWO reused target names (upstream's
    /// `_a`/`_b`) — never allocate a third.
    #[test]
    fn plan_effect_chain_ping_pong_targets_stay_bounded_to_two() {
        let mut base = layer("photo", None);
        base.material = Some(scene::MaterialSpec {
            texture_slots: vec![Some(scene::MaterialTextureSource::Bytes(vec![1, 2, 3, 4]))],
            ..Default::default()
        });
        // Five untargeted passes: the first four are intermediate (not
        // the chain's last), the fifth becomes final_material.
        base.effects = vec![effect_with_pass_targets(&[None, None, None, None, None])];
        let plan = plan_effect_chain(0, &base);

        assert_eq!(plan.intermediate.len(), 4);
        let names: std::collections::BTreeSet<&String> =
            plan.intermediate.iter().map(|(_, name)| name).collect();
        assert_eq!(
            names,
            std::collections::BTreeSet::from([
                &pingpong_target_name(0, None, 0),
                &pingpong_target_name(0, None, 1),
            ]),
            "must reuse exactly two target names, never allocate more: {names:?}"
        );
        // Alternation order: 0, 1, 0, 1.
        let order: Vec<&String> = plan.intermediate.iter().map(|(_, name)| name).collect();
        assert_eq!(
            order,
            vec![
                &pingpong_target_name(0, None, 0),
                &pingpong_target_name(0, None, 1),
                &pingpong_target_name(0, None, 0),
                &pingpong_target_name(0, None, 1),
            ]
        );
    }

    /// A TARGETED pass (naming a scene-declared `fbos[]` entry) never
    /// touches the object's own ping-pong pair — matches upstream's
    /// `configurePassTarget`/`writesToTarget` branch, which restores the
    /// pre-sequence ping-pong state instead of swapping it.
    #[test]
    fn plan_effect_chain_targeted_passes_do_not_consume_ping_pong_slots() {
        let mut base = layer("photo", None);
        base.material = Some(scene::MaterialSpec {
            texture_slots: vec![Some(scene::MaterialTextureSource::Bytes(vec![1, 2, 3, 4]))],
            ..Default::default()
        });
        // pass 0: targeted ("Blur"); pass 1: untargeted intermediate;
        // pass 2: targeted ("Blur2"); pass 3: untargeted final.
        base.effects = vec![effect_with_pass_targets(&[
            Some("Blur"),
            None,
            Some("Blur2"),
            None,
        ])];
        let plan = plan_effect_chain(0, &base);
        assert_eq!(plan.intermediate.len(), 3);
        // The untargeted intermediate pass (index 1 in source order,
        // which is intermediate[1]) still uses ping-pong slot 0 — the
        // targeted passes around it never advanced `write_slot`.
        assert_eq!(plan.intermediate[1].1, pingpong_target_name(0, None, 0));
    }

    #[test]
    fn scoped_target_name_leaves_full_frame_buffer_global_and_scopes_everything_else() {
        assert_eq!(
            scoped_target_name(0, kwe_core::FULL_FRAME_BUFFER),
            kwe_core::FULL_FRAME_BUFFER
        );
        assert_eq!(
            scoped_target_name(7, kwe_core::FULL_FRAME_BUFFER),
            kwe_core::FULL_FRAME_BUFFER
        );
        assert_eq!(scoped_target_name(0, "_rt_Foo"), "_rt_Foo#obj0");
        assert_eq!(scoped_target_name(3, "_rt_Foo"), "_rt_Foo#obj3");
        // Different objects declaring the SAME raw name never collide.
        assert_ne!(
            scoped_target_name(0, "_rt_Foo"),
            scoped_target_name(1, "_rt_Foo")
        );
    }

    #[test]
    fn effect_pass_samples_its_own_target_detects_the_feedback_loop() {
        let self_referencing = vec![
            Some(scene::MaterialTextureSource::RenderTarget(
                "_rt_Foo#obj0".into(),
            )),
            None,
        ];
        assert!(effect_pass_samples_its_own_target(
            &self_referencing,
            "_rt_Foo#obj0"
        ));

        let different_target = vec![Some(scene::MaterialTextureSource::RenderTarget(
            "_rt_Bar#obj0".into(),
        ))];
        assert!(!effect_pass_samples_its_own_target(
            &different_target,
            "_rt_Foo#obj0"
        ));

        let real_texture = vec![Some(scene::MaterialTextureSource::Bytes(vec![1, 2, 3, 4]))];
        assert!(!effect_pass_samples_its_own_target(
            &real_texture,
            "_rt_Foo#obj0"
        ));
    }

    /// S3 review RECOMMENDED #5: two DIFFERENT objects (layer indices 0
    /// and 1) each declare an effect with the SAME raw `fbos[]` name
    /// (`_rt_Shared`) and a pass that binds it via `"previous"`. Their
    /// resolved plans must end up with DIFFERENT scoped target/slot
    /// names — the whole point of scoping being that these two objects'
    /// FBOs can never alias.
    #[test]
    fn plan_effect_chain_scopes_fbo_names_so_different_objects_never_alias() {
        fn shared_name_effect() -> kwe_core::ObjectEffect {
            kwe_core::ObjectEffect {
                id: 1,
                name: "test".into(),
                visible: true,
                effect: kwe_core::EffectSpec {
                    name: "test".into(),
                    fbos: vec![kwe_core::FboSpec {
                        name: "_rt_Shared".into(),
                        format: "rgba8888".into(),
                        scale: 1.0,
                        unique: false,
                    }],
                    passes: vec![kwe_core::EffectPass::Material(
                        kwe_core::EffectMaterialPass {
                            material_ref: "materials/effects/test.json".into(),
                            shader: Some("test".into()),
                            blending: None,
                            combos: serde_json::Map::new(),
                            constant_shader_values: serde_json::Map::new(),
                            texture_slots: vec![Some(kwe_core::EffectTextureSlot::Previous)],
                            target: Some("_rt_Shared".into()),
                        },
                    )],
                },
            }
        }

        let mut layer_a = layer("a", None);
        layer_a.effects = vec![shared_name_effect()];
        let mut layer_b = layer("b", None);
        layer_b.effects = vec![shared_name_effect()];

        let plan_a = plan_effect_chain(0, &layer_a);
        let plan_b = plan_effect_chain(1, &layer_b);
        assert_eq!(plan_a.intermediate.len(), 1);
        assert_eq!(plan_b.intermediate.len(), 1);
        let target_a = &plan_a.intermediate[0].1;
        let target_b = &plan_b.intermediate[0].1;
        assert_ne!(
            target_a, target_b,
            "same raw fbo name, different objects, must not alias"
        );
        assert_eq!(target_a, "_rt_Shared#obj0");
        assert_eq!(target_b, "_rt_Shared#obj1");
    }

    /// A pass that `bind`s its own declared `target` name (the feedback
    /// loop MUST-FIX #3 guards against) survives `plan_effect_chain`'s
    /// resolution — the scoped target name and the scoped `RenderTarget`
    /// slot name it produces are IDENTICAL, exactly what
    /// `effect_pass_samples_its_own_target` is meant to catch at the
    /// `compile_material_layers` call site.
    #[test]
    fn plan_effect_chain_resolves_a_self_referencing_pass_to_a_detectable_shape() {
        let object_effect = kwe_core::ObjectEffect {
            id: 1,
            name: "test".into(),
            visible: true,
            effect: kwe_core::EffectSpec {
                name: "test".into(),
                fbos: vec![kwe_core::FboSpec {
                    name: "_rt_Foo".into(),
                    format: "rgba8888".into(),
                    scale: 1.0,
                    unique: false,
                }],
                passes: vec![kwe_core::EffectPass::Material(
                    kwe_core::EffectMaterialPass {
                        material_ref: "materials/effects/test.json".into(),
                        shader: Some("test".into()),
                        blending: None,
                        combos: serde_json::Map::new(),
                        constant_shader_values: serde_json::Map::new(),
                        texture_slots: vec![Some(kwe_core::EffectTextureSlot::RenderTarget(
                            "_rt_Foo".into(),
                        ))],
                        target: Some("_rt_Foo".into()),
                    },
                )],
            },
        };
        let mut self_ref = layer("self-ref", None);
        self_ref.effects = vec![object_effect];
        let plan = plan_effect_chain(0, &self_ref);
        assert_eq!(plan.intermediate.len(), 1);
        let (pass_material, target_name) = &plan.intermediate[0];
        assert!(effect_pass_samples_its_own_target(
            &pass_material.texture_slots,
            target_name
        ));
    }

    /// An image layer with every field at its default, `image` optional.
    fn layer(name: &str, image: Option<&str>) -> scene::LayerSpec {
        scene::LayerSpec {
            name: name.into(),
            id: None,
            scene_order: 0,
            image: image.map(Into::into),
            model_ref: None,
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
            material: None,
            fullscreen: false,
            effects_raw: Vec::new(),
            effects: Vec::new(),
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

    /// A minimal TEXV0005/TEXI0001/TEXB0003 ARGB8888 container: same shape
    /// texv.rs's own tests build, duplicated here (private to that
    /// module's `#[cfg(test)]`) so this integration test does not need a
    /// visibility change just to reuse a fixture builder.
    fn solid_texv_argb8888(width: u32, height: u32, rgba: [u8; 4]) -> Vec<u8> {
        let mut out = Vec::new();
        out.extend_from_slice(b"TEXV0005\0");
        out.extend_from_slice(b"TEXI0001\0");
        out.extend_from_slice(&0u32.to_le_bytes()); // format ARGB8888
        out.extend_from_slice(&0u32.to_le_bytes()); // flags
        out.extend_from_slice(&width.to_le_bytes());
        out.extend_from_slice(&height.to_le_bytes());
        out.extend_from_slice(&width.to_le_bytes());
        out.extend_from_slice(&height.to_le_bytes());
        out.extend_from_slice(&0u32.to_le_bytes());
        out.extend_from_slice(b"TEXB0003\0");
        out.extend_from_slice(&1u32.to_le_bytes()); // image count
        out.extend_from_slice(&(-1i32).to_le_bytes()); // FIF_UNKNOWN
        out.extend_from_slice(&1u32.to_le_bytes()); // mipmap count
        out.extend_from_slice(&width.to_le_bytes());
        out.extend_from_slice(&height.to_le_bytes());
        out.extend_from_slice(&0u32.to_le_bytes()); // compression = 0
        let mut pixels = Vec::with_capacity((width * height * 4) as usize);
        for _ in 0..(width * height) {
            pixels.extend_from_slice(&rgba);
        }
        out.extend_from_slice(&(pixels.len() as i32).to_le_bytes());
        out.extend_from_slice(&(pixels.len() as i32).to_le_bytes());
        out.extend_from_slice(&pixels);
        out
    }

    /// S3 review RECOMMENDED #4: pins the aggregate effect-asset budget
    /// boundary without allocating anywhere near 256 MiB in a test.
    #[test]
    fn effect_asset_budget_allows_pins_the_boundary() {
        assert!(effect_asset_budget_allows(0));
        assert!(effect_asset_budget_allows(MAX_EFFECT_ASSET_READ_BYTES - 1));
        assert!(!effect_asset_budget_allows(MAX_EFFECT_ASSET_READ_BYTES));
        assert!(!effect_asset_budget_allows(MAX_EFFECT_ASSET_READ_BYTES + 1));
    }

    /// S1 end-to-end: a model layer whose model -> material -> texture
    /// chain resolves through the lookup closure decodes its TEXV texture,
    /// fills the layer's size from the decoded dimensions, and counts
    /// toward the returned resolved count (the honest addend to
    /// `drawable_objects` main() adds after `load_scene`).
    #[test]
    fn load_model_textures_resolves_decodes_and_counts_drawable() {
        let mut assets: std::collections::HashMap<String, Vec<u8>> =
            std::collections::HashMap::new();
        assets.insert(
            "models/deco.json".into(),
            br#"{"material": "materials/deco.json"}"#.to_vec(),
        );
        assets.insert(
            "materials/deco.json".into(),
            br#"{"passes": [{"shader": "genericimage2", "textures": ["deco"]}]}"#.to_vec(),
        );
        assets.insert(
            "materials/deco.tex".into(),
            solid_texv_argb8888(4, 4, [10, 20, 30, 255]),
        );

        let mut layers = vec![layer("m", None)];
        layers[0].model_ref = Some("models/deco.json".into());
        let mut used_bytes = 0u64;
        let resolved = load_model_textures(&mut layers, &mut used_bytes, None, |reference| {
            assets.get(reference).cloned()
        });

        assert_eq!(resolved, 1);
        let texture = layers[0].texture.as_ref().expect("model texture decodes");
        assert_eq!((texture.width, texture.height), (4, 4));
        assert_eq!(&texture.rgba[0..4], &[10, 20, 30, 255]);
        assert_eq!(
            layers[0].size,
            [4.0, 4.0],
            "absent size takes the decoded texture's dimensions, like an image layer"
        );
        assert!(used_bytes > 0);
    }

    /// The honesty half of deliverable 4: an unresolvable model layer
    /// (no assets to satisfy the lookup) counts 0 toward the resolved
    /// return value and stays textureless — never a scene rejection.
    #[test]
    fn load_model_textures_unresolvable_model_is_skipped_not_rejected() {
        let mut layers = vec![layer("m", None)];
        layers[0].model_ref = Some("models/missing.json".into());
        let mut used_bytes = 0u64;
        let resolved = load_model_textures(&mut layers, &mut used_bytes, None, |_reference| None);
        assert_eq!(resolved, 0);
        assert!(layers[0].texture.is_none());
        assert_eq!(used_bytes, 0);
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
    fn file_video_is_staged_as_an_immutable_private_snapshot() {
        // The decoder must never receive the content-root path. It receives
        // a worker-owned copy made from the already-open source fd, so a
        // later replacement cannot change what libmpv reads.
        let root =
            std::env::temp_dir().join(format!("kwe-video-stage-root-{}", std::process::id()));
        let home =
            std::env::temp_dir().join(format!("kwe-video-stage-home-{}", std::process::id()));
        let _ = fs::remove_dir_all(&root);
        let _ = fs::remove_dir_all(&home);
        fs::create_dir_all(&root).unwrap();
        fs::create_dir_all(&home).unwrap();
        let source = root.join("clip.mp4");
        fs::write(&source, b"original synthetic video").unwrap();
        let mut opened = open_video_source(&root, "clip.mp4").unwrap();
        let staged = copy_video_file_into(&home, 0, &mut opened).unwrap();
        assert!(staged.starts_with(&home));
        assert_eq!(fs::read(&staged).unwrap(), b"original synthetic video");

        // Replacing the source after staging cannot affect the private
        // decoder input. A symlink reference is refused at open time too.
        fs::write(&source, b"replacement outside the snapshot").unwrap();
        assert_eq!(fs::read(&staged).unwrap(), b"original synthetic video");
        let link = root.join("link.mp4");
        std::os::unix::fs::symlink(&source, &link).unwrap();
        assert!(open_video_source(&root, "link.mp4").is_err());

        let _ = fs::remove_dir_all(&root);
        let _ = fs::remove_dir_all(&home);
    }

    #[test]
    fn file_video_staging_enforces_the_source_cap_while_streaming() {
        let home = std::env::temp_dir().join(format!("kwe-video-stage-cap-{}", std::process::id()));
        let _ = fs::remove_dir_all(&home);
        fs::create_dir_all(&home).unwrap();
        let oversized = home.join("oversized.mp4");
        fs::File::create(&oversized)
            .unwrap()
            .set_len(video::MAX_VIDEO_SOURCE_BYTES + 1)
            .unwrap();
        let mut source = fs::File::open(&oversized).unwrap();
        assert!(copy_video_file_into(&home, 0, &mut source).is_err());
        assert!(
            !home
                .join(format!("kwe-scene-video-{}", std::process::id()))
                .exists()
        );
        let _ = fs::remove_dir_all(&home);
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
    fn video_slot_fd_survives_directory_path_swap_without_redirecting() {
        let home = std::env::temp_dir().join(format!("kwe-video-dirfd-{}", std::process::id()));
        let outside =
            std::env::temp_dir().join(format!("kwe-video-dirfd-out-{}", std::process::id()));
        let _ = fs::remove_dir_all(&home);
        let _ = fs::remove_dir_all(&outside);
        fs::create_dir_all(&home).unwrap();
        fs::create_dir_all(&outside).unwrap();
        let dir = ensure_video_dir(&home).unwrap();
        let original = dir.path.clone();
        let moved = home.join("moved-video-dir");
        fs::rename(&original, &moved).unwrap();
        std::os::unix::fs::symlink(&outside, &original).unwrap();
        let mut slot = dir.create_slot(0).unwrap();
        slot.write_all(b"fd-owned").unwrap();
        drop(slot);
        assert_eq!(fs::read(moved.join("video-0.bin")).unwrap(), b"fd-owned");
        assert!(!outside.join("video-0.bin").exists());
        drop(dir);
        let _ = fs::remove_dir_all(&home);
        let _ = fs::remove_dir_all(&outside);
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
    fn video_cleanup_guard_removes_staged_media_on_drop() {
        let home = std::env::temp_dir().join(format!("kwe-video-guard-{}", std::process::id()));
        let _ = fs::remove_dir_all(&home);
        fs::create_dir_all(&home).unwrap();
        let staged = extract_video_into(&home, 0, b"payload").unwrap();
        let dir = staged.parent().unwrap().to_path_buf();
        {
            let guard = VideoCleanupGuard { home: home.clone() };
            assert!(dir.exists());
            drop(guard);
        }
        assert!(!dir.exists(), "guard drop must remove staged media");
        let _ = fs::remove_dir_all(&home);
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

    #[test]
    fn world_extent_follows_the_scaling_mode() {
        // No declared resolution: scene units are canvas pixels.
        assert_eq!(world_extent(None, (960, 540), "aspect"), (960.0, 540.0));
        // 16:9 scene on a wider canvas: aspect letterboxes (wider extent),
        // fill crops (shorter extent), stretch is the scene itself.
        let scene = Some((1920, 1080));
        let (w, h) = world_extent(scene, (2926, 823), "aspect");
        assert!((h - 1080.0).abs() < 0.01 && w > 1920.0, "{w}x{h}");
        let (w, h) = world_extent(scene, (2926, 823), "fill");
        assert!((w - 1920.0).abs() < 0.01 && h < 1080.0, "{w}x{h}");
        assert_eq!(
            world_extent(scene, (2926, 823), "stretch"),
            (1920.0, 1080.0)
        );
        // Matching aspect: every mode shows exactly the scene.
        assert_eq!(world_extent(scene, (960, 540), "aspect"), (1920.0, 1080.0));
        assert_eq!(world_extent(scene, (960, 540), "fill"), (1920.0, 1080.0));
        // Degenerate declared size falls back to the canvas.
        assert_eq!(world_extent(Some((0, 0)), (100, 50), "fill"), (100.0, 50.0));
    }
}
