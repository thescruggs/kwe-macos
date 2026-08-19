// SPDX-License-Identifier: Apache-2.0
//
// Isolated KWE video renderer: decodes a video file through libmpv's software
// render API (MPV_RENDER_API_TYPE_SW) and publishes BGRA8888 frames through
// the shared frame protocol. Runs as a supervised worker process: the daemon
// owns launch, health observation, termination, and quarantine, and this
// binary never parses commands from its stderr or the frame mapping.
//
// Keepalive: if no new frame arrives within one pacing interval (paused
// video, slow decode, loop-boundary settle), the last frame is re-published
// with a new sequence so the supervisor's frame timeout never trips. An empty
// frame is never published.
//
// Exit codes (shared with the supervisor contract in docs/BETA_M1.md):
//   0  normal or graceful SIGTERM stop
//   70 --exit-after fired
//   71 --memory-pressure-after allocation denied
//   72 --memory-pressure-after allocation unexpectedly succeeded
//   73 backend rejection: decode/render unusable even with --hwdec=no, or a
//      known duration over the 24 h bound (an unreadable duration fails open)
//
// Everything that could produce unbounded output is bounded: stderr lines
// are rate-limited, input reads are nonblocking with a byte cap, and the
// render framebuffer is fixed by the frame spec.

use std::ffi::{CStr, CString, c_char, c_int, c_void};
use std::io::Write;
use std::path::PathBuf;
use std::process::exit;
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::{Duration, Instant};

use anyhow::{Context, Result, bail};
use clap::Parser;
use kwe_frame_protocol::{FrameSpec, ProducerState, SharedFrameWriter};
use kwe_input_protocol::{
    InputAck, decode_audio_frame, decode_media_state, decode_pointer_line, encode_ack_line,
};

/// Set by a signal handler; checked at the top of every loop iteration.
static TERMINATED: AtomicBool = AtomicBool::new(false);

/// Exit code for a backend that cannot decode/render even after the
/// --hwdec=no fallback. The supervisor records this as `exit_code_73`.
const EXIT_BACKEND_REJECT: i32 = 73;

/// Hardware decode probe, then the software fallback retried exactly once.
const HW_DECODE: &str = "auto-safe";
const SOFTWARE_DECODE: &str = "no";

/// Longest single mpv wait; bounds event latency and the pacing timer.
const MAX_WAIT: Duration = Duration::from_millis(50);

/// Media with a known duration above this bound is a backend rejection
/// (exit 73): the static preflight never opens media, so the 24 h cap is
/// enforced here, against the mpv `duration` property.
const MAX_VIDEO_DURATION_SECONDS: f64 = 24.0 * 3600.0;

/// Bounded polls (50 ms each) waiting for MPV_EVENT_FILE_LOADED before
/// the duration property is readable; a load that never reports loaded
/// proceeds anyway — the play loop still observes real load failures and
/// the duration check fails open on an unknown value.
const MAX_LOAD_WAIT_POLLS: usize = 100;

/// Cap on drained events per loop iteration (mpv's queue is bounded, but a
/// hard cap keeps a pathological peer from starving the pacing timer).
const MAX_EVENTS_PER_TICK: usize = 256;

/// Bound on lines read from the control stream per poll (see InputChannel).
const MAX_INPUT_MESSAGE_BYTES: usize = 4096;
const MAX_INPUT_READS_PER_POLL: usize = 16;

// ---------------------------------------------------------------------------
// libmpv render API (bound directly; see docs/BETA_M1.md)
// ---------------------------------------------------------------------------

/// Minimal direct binding to the libmpv render API as shipped with 0.41.
/// The `mpv` crate cannot host it: `MpvHandlerBuilder::build()` runs
/// `mpv_initialize` before exposing the handle, and libmpv aborts when a
/// render context is created after initialization (empirically verified
/// against 0.41 — see docs/BETA_M1.md). All render calls stay on this
/// (main) thread, and the update callback only flips an atomic.
mod mpv_ffi {
    use super::{c_char, c_int, c_void};

    pub const MPV_FORMAT_STRING: c_int = 1;
    pub const MPV_FORMAT_FLAG: c_int = 3;
    pub const MPV_FORMAT_DOUBLE: c_int = 5;

    pub const MPV_EVENT_SHUTDOWN: c_int = 1;
    pub const MPV_EVENT_END_FILE: c_int = 7;
    pub const MPV_EVENT_FILE_LOADED: c_int = 8;
    pub const MPV_END_FILE_REASON_EOF: c_int = 0;

    pub const MPV_RENDER_PARAM_INVALID: c_int = 0;
    pub const MPV_RENDER_PARAM_API_TYPE: c_int = 1;
    pub const MPV_RENDER_PARAM_BLOCK_FOR_TARGET_TIME: c_int = 12;
    pub const MPV_RENDER_PARAM_SW_SIZE: c_int = 17;
    pub const MPV_RENDER_PARAM_SW_FORMAT: c_int = 18;
    pub const MPV_RENDER_PARAM_SW_STRIDE: c_int = 19;
    pub const MPV_RENDER_PARAM_SW_POINTER: c_int = 20;

    pub const MPV_RENDER_UPDATE_FRAME: u64 = 1;

    /// `struct mpv_event` (client.h): event_id, error, reply_userdata, data.
    #[repr(C)]
    pub struct mpv_event {
        pub event_id: c_int,
        pub error: c_int,
        pub reply_userdata: u64,
        pub data: *mut c_void,
    }

    /// `struct mpv_event_end_file` (client.h): reason, error, then per-format
    /// fields we never read (any trailing layout is fine).
    #[repr(C)]
    pub struct mpv_event_end_file {
        pub reason: c_int,
        pub error: c_int,
    }

    /// `struct mpv_render_param` (render.h): type, then data.
    #[repr(C)]
    pub struct mpv_render_param {
        pub type_: c_int,
        pub data: *mut c_void,
    }

    pub type MpvHandle = c_void;
    pub type MpvRenderContext = c_void;

    unsafe extern "C" {
        pub fn mpv_create() -> *mut MpvHandle;
        pub fn mpv_initialize(handle: *mut MpvHandle) -> c_int;
        pub fn mpv_terminate_destroy(handle: *mut MpvHandle);
        pub fn mpv_set_option(
            handle: *mut MpvHandle,
            name: *const c_char,
            format: c_int,
            data: *const c_void,
        ) -> c_int;
        pub fn mpv_set_property(
            handle: *mut MpvHandle,
            name: *const c_char,
            format: c_int,
            data: *const c_void,
        ) -> c_int;
        pub fn mpv_get_property(
            handle: *mut MpvHandle,
            name: *const c_char,
            format: c_int,
            data: *mut c_void,
        ) -> c_int;
        pub fn mpv_command(handle: *mut MpvHandle, args: *const *const c_char) -> c_int;
        pub fn mpv_wait_event(handle: *mut MpvHandle, timeout: f64) -> *mut mpv_event;
        pub fn mpv_error_string(code: c_int) -> *const c_char;
        pub fn mpv_render_context_create(
            res: *mut *mut MpvRenderContext,
            handle: *mut MpvHandle,
            params: *mut mpv_render_param,
        ) -> c_int;
        pub fn mpv_render_context_set_update_callback(
            context: *mut MpvRenderContext,
            callback: Option<extern "C" fn(*mut c_void)>,
            callback_ctx: *mut c_void,
        );
        pub fn mpv_render_context_update(context: *mut MpvRenderContext) -> u64;
        pub fn mpv_render_context_render(
            context: *mut MpvRenderContext,
            params: *mut mpv_render_param,
        ) -> c_int;
        pub fn mpv_render_context_free(context: *mut MpvRenderContext);
    }
}

/// Async-signal-safe handler: SIGTERM only records the termination request.
/// The loop observes it within MAX_WAIT and shuts down gracefully.
extern "C" fn on_sigterm(_signal: c_int) {
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
#[command(version, about = "Isolated KWE supervised video renderer")]
struct Arguments {
    /// Frame mapping file (validated and pre-opened by the daemon).
    #[arg(long)]
    output: PathBuf,
    /// Frame width in pixels.
    #[arg(long, default_value_t = 960, value_parser = clap::value_parser!(u32).range(1..=8192))]
    width: u32,
    /// Frame height in pixels.
    #[arg(long, default_value_t = 540, value_parser = clap::value_parser!(u32).range(1..=8192))]
    height: u32,
    /// Publish pacing in frames per second.
    #[arg(long, default_value_t = 30, value_parser = clap::value_parser!(u32).range(1..=240))]
    fps: u32,
    /// Video file to decode (daemon-validated before spawn).
    #[arg(long)]
    content: PathBuf,
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
}

/// Synthetic fault exit codes, identical to the test renderer's contract
/// (the daemon maps exit 71 into `resource_limit`).
const EXIT_EXIT_AFTER: i32 = 70;
const EXIT_MEMORY_DENIED: i32 = 71;
const EXIT_MEMORY_UNEXPECTED: i32 = 72;

fn try_memory_pressure(mib: Option<u64>) -> Result<(), ()> {
    // Simulate an allocation that crosses the supervisor's address-space
    // rlimit. `malloc` returns NULL for an over-limit mmap on glibc (exit 71);
    // an allocation that unexpectedly succeeds is still a fault (exit 72).
    let bytes = mib.unwrap_or(1024) * 1024 * 1024;
    let mut pointer: *mut c_void = std::ptr::null_mut();
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

/// Latest-wins media command decoded from the control stream.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum MediaCommand {
    Play,
    Pause,
    Stop,
}

/// Nonblocking stdin reader for the newline-delimited JSON control protocol.
/// Never blocks, never grows: reads are capped per poll, junk is ignored
/// silently, and the newest media command replaces any pending one (the
/// daemon is the authority on ordering).
struct InputChannel {
    pending: Vec<u8>,
    stdout: std::io::Stdout,
    media: Option<MediaCommand>,
}

impl InputChannel {
    fn new() -> Result<Self> {
        // Both descriptors were inherited from the daemon.
        set_nonblocking(libc::STDIN_FILENO)?;
        set_nonblocking(libc::STDOUT_FILENO)?;
        Ok(Self {
            pending: Vec::with_capacity(MAX_INPUT_MESSAGE_BYTES),
            stdout: std::io::stdout(),
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
            // mirror of the test renderer's guard).
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
                if decode_pointer_line(line).is_ok() {
                    self.ack(ack.as_deref());
                }
            }
            Some("media_state") => {
                if let Ok(state) = decode_media_state(line) {
                    if let Some(command) = media_command_for(&state.playback) {
                        self.media = Some(command);
                    }
                    self.ack(ack.as_deref());
                }
            }
            Some("audio_bands") if decode_audio_frame(line).is_ok() => {
                self.ack(ack.as_deref()); // count and discard; video has no audio path
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

    fn take_media(&mut self) -> Option<MediaCommand> {
        self.media.take()
    }
}

/// Map the wire playback enum onto the player command. The daemon already
/// validates the value, but this worker treats unknown values as absent.
fn media_command_for(playback: &str) -> Option<MediaCommand> {
    match playback {
        "playing" => Some(MediaCommand::Play),
        "paused" => Some(MediaCommand::Pause),
        "stopped" => Some(MediaCommand::Stop),
        _ => None,
    }
}

/// Toggle O_NONBLOCK on one standard descriptor inherited from the daemon.
fn set_nonblocking(fd: c_int) -> Result<()> {
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
// Pixel conversion
// ---------------------------------------------------------------------------

/// Convert one libmpv SW framebuffer into opaque BGRA8888 premultiplied
/// pixels (alpha 255 — video is opaque). `input` holds `stride` bytes per
/// row; the row content is `width * bpp` bytes of the given format. The SW
/// API is normally "rgb24" but also accepts "bgr0", "0bgr", "0rgb", "rgb0"
/// (libmpv render.h); everything else is unsupported and rejected by the
/// caller (backend rejection, exit 73). Returns None for a format or size
/// mismatch instead of ever publishing garbage.
fn convert_to_bgra(
    input: &[u8],
    width: u32,
    height: u32,
    stride: usize,
    format: &str,
) -> Option<Vec<u8>> {
    let (bpp, red, green, blue) = match format {
        "rgb24" => (3usize, 0usize, 1usize, 2usize),
        "bgr0" => (4usize, 2usize, 1usize, 0usize),
        "0bgr" => (4usize, 3usize, 2usize, 1usize),
        "rgb0" => (4usize, 0usize, 1usize, 2usize),
        "0rgb" => (4usize, 1usize, 2usize, 3usize),
        _ => return None,
    };
    let width = width as usize;
    let height = height as usize;
    let row_bytes = width.checked_mul(bpp)?;
    if stride < row_bytes {
        return None;
    }
    let needed = stride.checked_mul(height)?;
    if input.len() < needed {
        return None;
    }
    let mut output = Vec::with_capacity(width.checked_mul(height)?.checked_mul(4)?);
    for y in 0..height {
        let row = &input[y * stride..y * stride + row_bytes];
        for pixel in row.chunks_exact(bpp) {
            output.extend_from_slice(&[pixel[blue], pixel[green], pixel[red], 255]);
        }
    }
    Some(output)
}

// ---------------------------------------------------------------------------
// mpv session
// ---------------------------------------------------------------------------

struct MpvSession {
    handle: *mut mpv_ffi::MpvHandle,
    render_ctx: Option<*mut mpv_ffi::MpvRenderContext>,
    update_flag: Box<AtomicBool>,
    width: u32,
    height: u32,
}

/// Notification that a new frame is available. Must not call any mpv API
/// (render.h forbids it); it only flips an atomic observed by the loop.
extern "C" fn on_render_update(context: *mut c_void) {
    let flag = context as *const AtomicBool;
    // SAFETY: the flag outlives the context (session teardown frees the
    // context before the flag is dropped); stores to an atomic are safe.
    unsafe { (*flag).store(true, Ordering::Release) };
}

impl MpvSession {
    fn create(hwdec: &str, spec: FrameSpec) -> Result<Self> {
        // SAFETY: mpv_create returns a valid handle or NULL (no preconditions).
        let handle = unsafe { mpv_ffi::mpv_create() };
        if handle.is_null() {
            bail!("mpv_create returned a null handle");
        }
        let mut session = Self {
            handle,
            render_ctx: None,
            update_flag: Box::new(AtomicBool::new(false)),
            width: spec.width,
            height: spec.height,
        };
        // Options must be set before mpv_initialize. All are set as strings
        // and parsed exactly like --option=value on the command line.
        for (name, value) in [
            ("loop-file", "inf"),
            ("hwdec", hwdec),
            ("keep-open", "no"),
            ("idle", "no"),
            ("vo", "libmpv"),
            ("cache", "yes"),
        ] {
            session.set_option(name, value)?;
        }
        // The software render context must exist before mpv_initialize;
        // libmpv 0.41 aborts if it is created afterwards (verified, see
        // docs/BETA_M1.md). The SW API requires the caller to pick the
        // format; we ask for bgr0 and convert defensively.
        let api_type = CString::new("sw").context("invalid sw API string")?;
        let mut params = [
            mpv_ffi::mpv_render_param {
                type_: mpv_ffi::MPV_RENDER_PARAM_API_TYPE,
                data: api_type.as_ptr().cast_mut().cast(),
            },
            mpv_ffi::mpv_render_param {
                type_: mpv_ffi::MPV_RENDER_PARAM_INVALID,
                data: std::ptr::null_mut(),
            },
        ];
        let mut context: *mut mpv_ffi::MpvRenderContext = std::ptr::null_mut();
        // SAFETY: the handle is valid; the params array is valid during the
        // call and stays valid for the SW render calls below.
        let code = unsafe {
            mpv_ffi::mpv_render_context_create(&mut context, handle, params.as_mut_ptr())
        };
        check_mpv(code, "mpv_render_context_create")?;
        if context.is_null() {
            bail!("mpv_render_context_create returned a null context");
        }
        session.render_ctx = Some(context);
        // SAFETY: the flag outlives the context (terminate frees the context
        // before the box is dropped); the callback only stores to the atomic.
        unsafe {
            mpv_ffi::mpv_render_context_set_update_callback(
                context,
                Some(on_render_update),
                (&*session.update_flag as *const AtomicBool)
                    .cast_mut()
                    .cast(),
            )
        };
        Ok(session)
    }

    fn set_option(&mut self, name: &str, value: &str) -> Result<()> {
        let name = CString::new(name).context("option name contains a NUL")?;
        let value = CString::new(value).context("option value contains a NUL")?;
        // MPV_FORMAT_STRING requires the *address of* the char* variable
        // (client.h: "you pass the address to the variable"), not the string
        // itself; mpv parses it exactly like a command line option.
        let value_pointer: *const c_char = value.as_ptr();
        // SAFETY: both CStrings live for the duration of the call and the
        // value_pointer variable outlives it.
        let code = unsafe {
            mpv_ffi::mpv_set_option(
                self.handle,
                name.as_ptr(),
                mpv_ffi::MPV_FORMAT_STRING,
                (&value_pointer as *const *const c_char).cast(),
            )
        };
        check_mpv(code, &name.to_string_lossy())
    }

    fn set_property_flag(&mut self, name: &str, value: bool) -> Result<()> {
        let name = CString::new(name).context("property name contains a NUL")?;
        let flag = c_int::from(value);
        // SAFETY: MPV_FORMAT_FLAG reads one int; mpv copies it immediately.
        let code = unsafe {
            mpv_ffi::mpv_set_property(
                self.handle,
                name.as_ptr(),
                mpv_ffi::MPV_FORMAT_FLAG,
                (&flag as *const c_int).cast(),
            )
        };
        check_mpv(code, &name.to_string_lossy())
    }

    fn command(&mut self, args: &[&str]) -> Result<()> {
        let owned: Vec<CString> = args
            .iter()
            .map(|arg| CString::new(*arg).context("command argument contains a NUL"))
            .collect::<Result<_>>()?;
        let mut pointers: Vec<*const c_char> =
            owned.iter().map(|argument| argument.as_ptr()).collect();
        pointers.push(std::ptr::null());
        // SAFETY: the NULL-terminated argument array is valid for the call;
        // libmpv copies what it needs.
        let code = unsafe { mpv_ffi::mpv_command(self.handle, pointers.as_ptr()) };
        check_mpv(code, args[0])
    }

    fn initialize(&mut self) -> Result<()> {
        // SAFETY: the handle was created but not yet initialized; the render
        // context already exists, which is the requirement for SW output.
        let code = unsafe { mpv_ffi::mpv_initialize(self.handle) };
        check_mpv(code, "mpv_initialize")
    }

    fn load_file(&mut self, path: &std::path::Path) -> Result<()> {
        let path = path.to_str().context("video content path is not UTF-8")?;
        self.command(&["loadfile", path])
    }

    /// Current value of the `duration` property in seconds, or None when it
    /// is unknown or not yet readable. Metadata-less containers and races
    /// with metadata loading report unavailable, which fails the duration
    /// bound open by contract.
    fn duration_seconds(&mut self) -> Option<f64> {
        let name = CString::new("duration").ok()?;
        let mut seconds = 0.0_f64;
        // SAFETY: MPV_FORMAT_DOUBLE writes exactly one f64 into `seconds`;
        // the handle is valid for the session lifetime.
        let code = unsafe {
            mpv_ffi::mpv_get_property(
                self.handle,
                name.as_ptr(),
                mpv_ffi::MPV_FORMAT_DOUBLE,
                (&mut seconds as *mut f64).cast(),
            )
        };
        (code >= 0).then_some(seconds)
    }

    /// One nonblocking event, copied into a local enum (the pointer is only
    /// valid until the next mpv_wait_event call). Ok(None) on timeout.
    fn wait_event(&mut self, timeout: f64) -> Result<Option<Event>> {
        // SAFETY: the handle is valid; the returned pointer is only used to
        // copy scalars before any further libmpv call.
        let event = unsafe { mpv_ffi::mpv_wait_event(self.handle, timeout) };
        if event.is_null() {
            return Ok(None);
        }
        let event = unsafe { &*event };
        let kind = match event.event_id {
            mpv_ffi::MPV_EVENT_END_FILE => {
                // SAFETY: data points at mpv_event_end_file for this id.
                let end = unsafe { &*(event.data.cast::<mpv_ffi::mpv_event_end_file>()) };
                Event::EndFile { reason: end.reason }
            }
            mpv_ffi::MPV_EVENT_SHUTDOWN => Event::Shutdown,
            mpv_ffi::MPV_EVENT_FILE_LOADED => Event::FileLoaded,
            _ => Event::Ignored,
        };
        Ok(Some(kind))
    }

    fn drain_events(&mut self) -> Result<()> {
        let mut drained = 0;
        while drained < MAX_EVENTS_PER_TICK {
            let Some(event) = self.wait_event(0.0)? else {
                break;
            };
            drained += 1;
            match event {
                Event::EndFile { reason } if reason != mpv_ffi::MPV_END_FILE_REASON_EOF => {
                    // EOF loops back via --loop-file=inf; any other reason is
                    // a backend failure (decode error, format reject, ...)
                    // and goes through the --hwdec=no retry.
                    bail!("libmpv end_file reason {reason}");
                }
                Event::EndFile { .. } => {
                    // EOF: the loop restarts the file; keepalive covers the
                    // gap until the first frame of the next pass.
                }
                Event::Shutdown => bail!("libmpv core shutdown"),
                Event::FileLoaded | Event::Ignored => {}
            }
        }
        Ok(())
    }

    /// Render into the SW framebuffer when a new frame is available.
    /// Returns Ok(true) when a fresh frame was rendered.
    fn render_new_frame(
        &mut self,
        scratch: &mut [u8],
        stride: usize,
        format: &str,
    ) -> Result<bool> {
        // SAFETY: the context exists for the session lifetime.
        let flags = unsafe { mpv_ffi::mpv_render_context_update(self.render_ctx.unwrap()) };
        if flags & mpv_ffi::MPV_RENDER_UPDATE_FRAME == 0 {
            return Ok(false);
        }
        self.render(scratch, stride, format)?;
        Ok(true)
    }

    fn render(&mut self, scratch: &mut [u8], stride: usize, format: &str) -> Result<()> {
        let mut size = [self.width as c_int, self.height as c_int];
        let format = CString::new(format).context("invalid SW format string")?;
        // render.h declares SW_STRIDE as int*; the stride is bounded by the
        // spec (width <= 8192, 4 bytes per pixel), so c_int cannot overflow.
        let mut stride_value = stride as c_int;
        // 0: never block on target-time pacing inside the render call; the
        // outer loop owns pacing.
        let block_for_target_time = 0_i32;
        let mut params = [
            mpv_ffi::mpv_render_param {
                type_: mpv_ffi::MPV_RENDER_PARAM_SW_SIZE,
                data: (&mut size as *mut [c_int; 2]).cast(),
            },
            mpv_ffi::mpv_render_param {
                type_: mpv_ffi::MPV_RENDER_PARAM_SW_FORMAT,
                data: format.as_ptr().cast_mut().cast(),
            },
            mpv_ffi::mpv_render_param {
                type_: mpv_ffi::MPV_RENDER_PARAM_SW_STRIDE,
                data: (&mut stride_value as *mut c_int).cast(),
            },
            mpv_ffi::mpv_render_param {
                type_: mpv_ffi::MPV_RENDER_PARAM_SW_POINTER,
                data: scratch.as_mut_ptr().cast(),
            },
            mpv_ffi::mpv_render_param {
                type_: mpv_ffi::MPV_RENDER_PARAM_BLOCK_FOR_TARGET_TIME,
                data: (&block_for_target_time as *const i32).cast_mut().cast(),
            },
            mpv_ffi::mpv_render_param {
                type_: mpv_ffi::MPV_RENDER_PARAM_INVALID,
                data: std::ptr::null_mut(),
            },
        ];
        // SAFETY: the context is valid; every SW param is set before the
        // INVALID terminator and stays valid during the call.
        let code = unsafe {
            mpv_ffi::mpv_render_context_render(self.render_ctx.unwrap(), params.as_mut_ptr())
        };
        check_mpv(code, "mpv_render_context_render")
    }

    fn terminate(&mut self) {
        if let Some(context) = self.render_ctx.take() {
            // SAFETY: frees the render context; after this the update
            // callback cannot fire, so the atomic flag is safe to drop.
            unsafe { mpv_ffi::mpv_render_context_free(context) };
        }
        if !self.handle.is_null() {
            // SAFETY: the render context was freed first (libmpv requires
            // this ordering) and the handle is destroyed exactly once.
            unsafe { mpv_ffi::mpv_terminate_destroy(self.handle) };
            self.handle = std::ptr::null_mut();
        }
    }
}

impl Drop for MpvSession {
    fn drop(&mut self) {
        self.terminate();
    }
}

/// A single mpv event, copied out of libmpv's event struct.
enum Event {
    EndFile { reason: c_int },
    Shutdown,
    FileLoaded,
    Ignored,
}

/// Bounded stderr diagnostic for a libmpv error code.
fn check_mpv(code: c_int, action: &str) -> Result<()> {
    if code >= 0 {
        return Ok(());
    }
    // SAFETY: libmpv returns a static string for the lifetime of the call.
    let message = unsafe { CStr::from_ptr(mpv_ffi::mpv_error_string(code)) }
        .to_string_lossy()
        .into_owned();
    Err(anyhow::anyhow!("{action} failed: {message}"))
}

/// Bounded wait for MPV_EVENT_FILE_LOADED so the `duration` property is
/// readable before the bound check. A load failure surfaces here exactly
/// as in the play loop (non-EOF end_file or shutdown). If the file never
/// reports loaded within the poll bound, proceed: the play loop still
/// observes real load failures, and the duration check fails open.
fn wait_for_load(session: &mut MpvSession) -> Result<()> {
    for _ in 0..MAX_LOAD_WAIT_POLLS {
        match session.wait_event(MAX_WAIT.as_secs_f64())? {
            Some(Event::FileLoaded) => return Ok(()),
            Some(Event::EndFile { reason }) if reason != mpv_ffi::MPV_END_FILE_REASON_EOF => {
                bail!("libmpv end_file reason {reason}");
            }
            Some(Event::Shutdown) => bail!("libmpv core shutdown"),
            Some(Event::EndFile { .. }) | Some(Event::Ignored) | None => {}
        }
    }
    Ok(())
}

/// Duration-bound decision: a known duration above 24 h is rejected
/// (bubbles up to a backend rejection, exit 73); an unknown duration
/// (metadata-less container) fails open. Pure so the bound is
/// unit-testable without a media file.
fn duration_decision(seconds: Option<f64>) -> Result<()> {
    match seconds {
        None => Ok(()),
        Some(seconds) if seconds > MAX_VIDEO_DURATION_SECONDS => {
            bail!("media duration {seconds:.1}s exceeds the 24 h bound")
        }
        Some(_) => Ok(()),
    }
}

// ---------------------------------------------------------------------------
// Pacing
// ---------------------------------------------------------------------------

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum PublishDecision {
    NewFrame,
    Keepalive,
    Wait,
}

/// Pure keepalive decision: a new frame is published only at the pacing
/// deadline; past it, a stale frame is re-published (keepalive) so the
/// supervisor's frame timeout never trips; before the deadline, or with
/// nothing published yet, wait. An empty frame is never published.
fn next_publish(
    now: Instant,
    deadline: Instant,
    have_new_frame: bool,
    have_last: bool,
) -> PublishDecision {
    if now < deadline {
        PublishDecision::Wait
    } else if have_new_frame {
        PublishDecision::NewFrame
    } else if have_last {
        PublishDecision::Keepalive
    } else {
        PublishDecision::Wait
    }
}

// ---------------------------------------------------------------------------
// Worker loop
// ---------------------------------------------------------------------------

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Playback {
    Graceful,
}

struct VideoWorker {
    arguments: Arguments,
    spec: FrameSpec,
    writer: SharedFrameWriter,
    input: InputChannel,
    published: u64,
}

impl VideoWorker {
    /// Synthetic faults keyed on the published-frame count, mirroring the
    /// test renderer's fault block exactly (order: exit, corrupt, hang,
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
                // anomaly: exit 72 (mirrors the test renderer exactly).
                Ok(()) => exit(EXIT_MEMORY_UNEXPECTED),
                Err(()) => exit(EXIT_MEMORY_DENIED),
            }
        }
        Ok(())
    }

    fn run(&mut self) -> Result<()> {
        match self.run_playback(HW_DECODE) {
            Ok(Playback::Graceful) => Ok(()),
            Err(_first) => self.fallback_and_retry(HW_DECODE),
        }
    }

    /// One full decode/render attempt. On success the worker exits 0 through
    /// the caller; on failure the caller retries once with --hwdec=no.
    fn run_playback(&mut self, hwdec: &str) -> Result<Playback> {
        // Session creation happens here so the hwdec=no retry gets a fresh
        // handle (mpv can be initialized only once per handle).
        let mut session = MpvSession::create(hwdec, self.spec)?;
        session.initialize()?;
        session.load_file(&self.arguments.content)?;
        // The duration bound is per-file, so both decode attempts reject an
        // overlong media identically; the retry costs one bounded re-load.
        wait_for_load(&mut session)?;
        duration_decision(session.duration_seconds())?;
        self.play_loop(&mut session, hwdec)
    }

    /// Retry once with software decoding when the hardware-accelerated
    /// session fails (libmpv reports hwdec errors at init/play time). A
    /// second failure is a backend rejection: bounded stderr diagnostic and
    /// exit 73 (the daemon folds it into `exit_code_73`).
    fn fallback_and_retry(&mut self, hwdec: &str) -> Result<()> {
        if TERMINATED.load(Ordering::Acquire) {
            return Ok(()); // a graceful SIGTERM raced the failure; exit 0
        }
        eprintln!("event=renderer.video.hwdec_fallback from={hwdec} (retry once with --hwdec=no)");
        match self.run_playback(SOFTWARE_DECODE) {
            Ok(Playback::Graceful) => Ok(()),
            Err(error) => {
                eprintln!("event=renderer.video.backend_reject detail={error}");
                eprintln!("event=renderer.video.backend_reject exit_code={EXIT_BACKEND_REJECT}");
                exit(EXIT_BACKEND_REJECT);
            }
        }
    }

    fn play_loop(&mut self, session: &mut MpvSession, hwdec: &str) -> Result<Playback> {
        let interval = Duration::from_secs_f64(1.0 / f64::from(self.arguments.fps));
        let mut deadline = Instant::now();
        let mut last_pixels: Option<Vec<u8>> = None;
        let mut new_frame_queued = false;
        let mut invalid_frames = 0_u64;
        // SW rendering always targets the exact spec size; the stride is
        // chosen to keep libmpv happy with arbitrary SIMD copies.
        let stride = ((self.spec.width as usize * 4) + 63) & !63;
        let scratch_len = stride
            .checked_mul(self.spec.height as usize)
            .context("frame dimensions overflow the scratch buffer")?;
        let mut scratch = vec![0_u8; scratch_len];
        let format = "bgr0";
        eprintln!("event=renderer.video.session hwdec={hwdec} format={format}");
        loop {
            self.input.poll();
            if let Some(command) = self.input.take_media() {
                self.apply_media(session, command);
            }
            self.check_faults()?;
            if TERMINATED.load(Ordering::Acquire) {
                self.writer.set_state(ProducerState::Stopping);
                eprintln!("event=renderer.complete frames={}", self.published);
                return Ok(Playback::Graceful);
            }
            session.drain_events()?;
            if session.render_new_frame(&mut scratch, stride, format)? {
                new_frame_queued = true;
            }
            let now = Instant::now();
            if now < deadline {
                // Wait the shorter of the pacing remainder and the bound,
                // so TERMINATED and input are observed at least every 50 ms.
                let wait = deadline.duration_since(now).min(MAX_WAIT).as_secs_f64();
                session.wait_event(wait)?;
                continue;
            }
            match next_publish(now, deadline, new_frame_queued, last_pixels.is_some()) {
                PublishDecision::NewFrame => {
                    new_frame_queued = false;
                    match convert_to_bgra(
                        &scratch,
                        self.spec.width,
                        self.spec.height,
                        stride,
                        format,
                    ) {
                        Some(pixels) if pixels.len() == self.spec.pixel_bytes() => {
                            // Exact-size check: the conversion is exact by
                            // construction; a mismatch means a malformed
                            // frame, which is skipped and counted, never
                            // published.
                            self.published = self.writer.publish(&pixels)?;
                            last_pixels = Some(pixels);
                        }
                        Some(_) | None => {
                            invalid_frames = invalid_frames.saturating_add(1);
                            diag_invalid_frame(invalid_frames);
                        }
                    }
                }
                PublishDecision::Keepalive => {
                    // No new frame within one pacing interval (paused video,
                    // slow decode, loop settle): re-publish the last frame
                    // with a new sequence. The pixels are identical — the
                    // supervisor only watches sequence progression.
                    if let Some(pixels) = &last_pixels {
                        self.published = self.writer.publish(pixels)?;
                    }
                }
                PublishDecision::Wait => {}
            }
            deadline = now + interval;
        }
    }

    fn apply_media(&mut self, session: &mut MpvSession, command: MediaCommand) {
        let outcome = match command {
            MediaCommand::Play => session.set_property_flag("pause", false),
            MediaCommand::Pause => session.set_property_flag("pause", true),
            MediaCommand::Stop => session
                .set_property_flag("pause", true)
                .and_then(|()| session.command(&["seek", "0", "absolute"])),
        };
        match outcome {
            Ok(()) => eprintln!("event=renderer.media_state applied={command:?}"),
            Err(error) => {
                // Bounded diagnostics only; a failed control command never
                // kills the supervised worker or blocks the loop.
                eprintln!("event=renderer.media_error detail={error}");
            }
        }
    }
}

/// Bounded diagnostic for malformed frames: first occurrences, then every
/// thousandth, so a misbehaving decoder cannot flood the daemon's stderr
/// ring (64 lines / 16 KiB) or this process's output.
fn diag_invalid_frame(count: u64) {
    if count <= 10 || count.is_multiple_of(1000) {
        eprintln!("event=renderer.video.invalid_frame count={count}");
    }
}

// ---------------------------------------------------------------------------
// Entry point
// ---------------------------------------------------------------------------

fn main() -> Result<()> {
    let arguments = Arguments::parse();
    if arguments.memory_pressure_after.is_some() != arguments.memory_pressure_mib.is_some() {
        bail!("--memory-pressure-after and --memory-pressure-mib must be supplied together");
    }
    install_term_handler(arguments.ignore_term);
    let input = InputChannel::new()?;
    if arguments.startup_hang {
        eprintln!("event=renderer.fault kind=startup_hang");
        park_forever();
    }
    let spec = FrameSpec::new(arguments.width, arguments.height)?;
    let writer = SharedFrameWriter::create(&arguments.output, spec).with_context(|| {
        format!(
            "create frame mapping {}",
            arguments.output.to_string_lossy()
        )
    })?;
    let mut worker = VideoWorker {
        arguments,
        spec,
        writer,
        input,
        published: 0,
    };
    // libmpv client API version diagnostic (reported through the pinned mpv
    // crate; the render API itself is bound directly — docs/BETA_M1.md).
    let (major, minor) = mpv::client_api_version();
    eprintln!("event=renderer.video.start mpv_api={major}.{minor}");
    worker.run()
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn rgb24_converts_with_swapped_channels_and_opaque_alpha() {
        let input = [255, 0, 0, 0, 255, 0, 0, 0, 255, 42, 42, 42];
        let output = convert_to_bgra(&input, 4, 1, 12, "rgb24").unwrap();
        assert_eq!(
            output,
            [
                0, 0, 255, 255, 0, 255, 0, 255, 255, 0, 0, 255, 42, 42, 42, 255
            ]
        );
    }

    #[test]
    fn bgr0_converts_and_ignores_stride_padding() {
        // 2 pixels per row, stride 12: 8 used, 4 padding bytes that must be
        // skipped, and the alpha byte must be forced to 255 (not 0).
        let input = [10, 20, 30, 0, 40, 50, 60, 0, 99, 99, 99, 99];
        let output = convert_to_bgra(&input, 2, 1, 12, "bgr0").unwrap();
        assert_eq!(output, [10, 20, 30, 255, 40, 50, 60, 255]);
    }

    #[test]
    fn other_sw_formats_are_handled_defensively() {
        let rgb0 = convert_to_bgra(&[9, 8, 7, 3], 1, 1, 4, "rgb0").unwrap();
        assert_eq!(rgb0, [7, 8, 9, 255]);
        let zero_bgr = convert_to_bgra(&[3, 9, 8, 7], 1, 1, 4, "0bgr").unwrap();
        assert_eq!(zero_bgr, [9, 8, 7, 255]);
        let zero_rgb = convert_to_bgra(&[3, 9, 8, 7], 1, 1, 4, "0rgb").unwrap();
        assert_eq!(zero_rgb, [7, 8, 9, 255]);
    }

    #[test]
    fn rejects_unsupported_formats_and_undersized_buffers() {
        assert!(convert_to_bgra(&[0; 4], 1, 1, 4, "yuv420p").is_none());
        assert!(convert_to_bgra(&[0; 3], 1, 1, 4, "bgr0").is_none());
        // stride * height exceeds the buffer length.
        assert!(convert_to_bgra(&[0; 12], 2, 2, 8, "bgr0").is_none());
        // stride narrower than the row content.
        assert!(convert_to_bgra(&[0; 16], 2, 2, 6, "bgr0").is_none());
    }

    #[test]
    fn keepalive_decides_when_to_publish_what() {
        let now = Instant::now();
        assert_eq!(
            next_publish(now, now + Duration::from_secs(1), true, true),
            PublishDecision::Wait
        );
        assert_eq!(
            next_publish(now, now, true, true),
            PublishDecision::NewFrame
        );
        assert_eq!(
            next_publish(now, now, false, true),
            PublishDecision::Keepalive
        );
        assert_eq!(next_publish(now, now, false, false), PublishDecision::Wait);
    }

    #[test]
    fn media_state_maps_to_player_commands() {
        assert_eq!(media_command_for("playing"), Some(MediaCommand::Play));
        assert_eq!(media_command_for("paused"), Some(MediaCommand::Pause));
        assert_eq!(media_command_for("stopped"), Some(MediaCommand::Stop));
        assert_eq!(media_command_for("junk"), None);
    }

    #[test]
    fn fault_flags_map_to_the_documented_exit_codes() {
        assert_eq!(EXIT_EXIT_AFTER, 70);
        assert_eq!(EXIT_MEMORY_DENIED, 71);
        assert_eq!(EXIT_MEMORY_UNEXPECTED, 72);
        assert_eq!(EXIT_BACKEND_REJECT, 73);
    }

    #[test]
    fn duration_bound_rejects_over_24h_and_fails_open_on_unknown() {
        // At or below the 24 h cap the media is accepted.
        assert!(duration_decision(Some(23.0 * 3600.0)).is_ok());
        assert!(duration_decision(Some(24.0 * 3600.0)).is_ok());
        // Above the cap the rejection names the bound.
        let error = format!(
            "{}",
            duration_decision(Some(24.0 * 3600.0 + 1.0)).unwrap_err()
        );
        assert!(error.contains("24 h"), "unexpected error: {error}");
        // An unknown duration (metadata-less container) fails open.
        assert!(duration_decision(None).is_ok());
    }
}
