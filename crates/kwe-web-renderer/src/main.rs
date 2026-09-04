// SPDX-License-Identifier: GPL-3.0-or-later
//
// Isolated KWE web renderer (BETA_M2b): spawns a sandboxed headless
// Chromium (bubblewrap + `--remote-debugging-pipe` on fds 3/4) and
// publishes the `Page.screencast` jpegs as opaque BGRA8888 frames through
// the shared frame protocol. Runs as a supervised worker process: the
// daemon owns launch, health observation, and quarantine, and this binary
// never parses commands from its stderr or the frame mapping. The CDP wire
// contract is pinned in docs/BETA_M2.md §2 and asserted by
// scripts/smoke-cdp.sh.
//
// Bounds: the CDP pipe is 4 MiB-per-message capped by the kwe-cdp client;
// jpegs are dimension/allocation-capped at decode; input reads are
// nonblocking and capped per poll; every repeated diagnostic is
// rate-limited; the browser is reaped with a bounded deadline. A page that
// never animates produces no screencast frames, so the last published
// frame is re-published (keepalive) at each pacing deadline — the
// supervisor's frame timeout never trips, and an empty frame is never
// published.
//
// Exit codes (shared with the supervisor contract in docs/BETA_M1.md):
//   0  graceful stop after SIGTERM (state Stopping, browser torn down)
//   70 --exit-after fired
//   71 --memory-pressure-after allocation denied
//   72 --memory-pressure-after allocation unexpectedly succeeded
//   73 backend rejection: preflight failed, the browser refused the CDP
//      pipe, no decodable frame arrived within the 8 s startup deadline
//      (inside the daemon's 10 s web startup timeout), or the page failed
//      the session-scoped liveness heartbeat (a page whose renderer main
//      thread wedges after first paint stops answering CDP — acks
//      included — while the browser process survives; the keepalive
//      re-publication would otherwise mask the dead stream forever)
//
// Scale policy (documented in docs/BETA_M2.md): the screencast is
// requested at exactly the spec dimensions, so the decoded jpeg normally
// matches the frame slot 1:1; a mismatch (compositor aspect rounding, e.g.
// 160x89 for a 160x90 spec) is corrected with a bounded nearest-neighbor
// scale that stretches the delivered image to fill the fixed-size slot.

use std::ffi::c_int;
use std::fs;
use std::io::Write;
use std::os::fd::{AsRawFd, RawFd};
use std::os::unix::process::CommandExt;
use std::path::{Path, PathBuf};
use std::process::{Child, Command, Stdio, exit};
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::{Duration, Instant};

use anyhow::{Context, Result, bail};
use clap::Parser;
use kwe_cdp::{Client, Event};
use kwe_core::{preflight_web, web_renderer_command};
use kwe_frame_protocol::{FrameSpec, ProducerState, SharedFrameWriter};
use kwe_input_protocol::{
    AudioFrame, PointerButton, PointerMessage, decode_audio_frame, decode_media_state,
    decode_pointer_line, encode_ack_line,
};
use serde_json::{Value, json};

/// Set by the SIGTERM handler; checked at the top of every loop iteration
/// (the loop never blocks longer than MAX_WAIT).
static TERMINATED: AtomicBool = AtomicBool::new(false);

/// Chromium's CDP pipe descriptors (`--remote-debugging-pipe`): the browser
/// reads requests from fd 3 and writes responses/events to fd 4.
const CHROMIUM_READ_FD: RawFd = 3;
const CHROMIUM_WRITE_FD: RawFd = 4;

/// Synthetic fault exit codes, identical to the test/video renderers'
/// contract (the daemon maps exit 71 into `resource_limit`).
const EXIT_EXIT_AFTER: i32 = 70;
const EXIT_MEMORY_DENIED: i32 = 71;
const EXIT_MEMORY_UNEXPECTED: i32 = 72;
const EXIT_BACKEND_REJECT: i32 = 73;

/// Whole startup sequence (spawn -> first decodable screencast frame) must
/// fit inside this deadline, which sits inside the daemon's web startup
/// timeout (default 10 s), so a wedged browser fails the worker, not the
/// daemon (which then records `exit_code_73` and restarts).
const STARTUP_DEADLINE: Duration = Duration::from_secs(8);

/// Longest single CDP poll; also the observed-liveness bound for SIGTERM
/// and control-stream input (mirrors the video renderer's MAX_WAIT).
const MAX_WAIT: Duration = Duration::from_millis(50);

/// Bounds on the control stream reads (mirror kwe-video-renderer).
const MAX_INPUT_MESSAGE_BYTES: usize = 4096;
const MAX_INPUT_READS_PER_POLL: usize = 16;

/// Bounded base64 input: the screencast jpeg rides inside a 4 MiB-capped
/// CDP message, and its base64 form is bounded again here (belt and
/// braces; a compliant jpeg at 4096x4096 q80 is far below this).
const MAX_JPEG_BASE64_BYTES: usize = 4 * 1024 * 1024;

/// Decoded-jpeg bounds (defensive: the screencast is delivered at the
/// spec size, so anything near these caps is a hostile or broken fixture).
const MAX_DECODE_DIMENSION: u32 = 8192;
const MAX_DECODE_ALLOC_BYTES: u64 = 64 * 1024 * 1024;
const MAX_DECODED_PIXELS: u64 = 16_777_216; // 4096^2

/// Audio injection bound: at most 30 `audio_web(...)` calls per second,
/// latest-wins (a newer frame replaces an older one).
const AUDIO_MIN_INTERVAL: Duration = Duration::from_millis(34);

/// Throwaway content page for `--probe` (BETA_M2e): the probe boots the REAL
/// sandboxed browser over a temp content root and verifies three boot-class
/// round trips on the CDP pipe — Browser.getVersion, a one-frame
/// Page.startScreencast capture with its ack, and a Runtime.evaluate
/// heartbeat. The page animates because a page that paints identical pixels
/// every frame stops the compositor and no screencast frames flow
/// (docs/BETA_M2.md §5.7); the animated canvas keeps frames flowing so the
/// probe can prove the paint -> screencast -> pipe -> ack path the worker
/// runs on. The frame content itself is irrelevant.
const PROBE_PAGE: &str = "<!doctype html><html><head><title>kwe-web-probe</title></head><body><canvas id=\"c\" width=\"160\" height=\"90\"></canvas><script>const c=document.getElementById('c');const x=c.getContext('2d');let i=0;function tick(){x.fillStyle='#101214';x.fillRect(0,0,160,90);x.fillStyle=i%2?'#1a4fae':'#ae3f1a';x.fillRect(i%157,i%77,3,3);i=(i+1)%256;requestAnimationFrame(tick);}requestAnimationFrame(tick);</script></body></html>";

/// Bounded stderr ring for browser diagnostics (same size as the daemon's
/// per-worker ring).
const STDERR_RING_LIMIT: usize = 16 * 1024;

/// Longest bounded wait for the bwrap child to exit after the pipe close
/// (chromium exits within ~50 ms; pinned in docs/BETA_M2.md §2).
const CHILD_EXIT_DEADLINE: Duration = Duration::from_secs(5);
/// SIGTERM grace for the bwrap process group before SIGKILL.
const TERM_GRACE: Duration = Duration::from_secs(1);

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

/// Attempt a large allocation that crosses the supervisor's address-space
/// rlimit. `malloc` returns NULL for an over-limit mmap on glibc (exit 71);
/// an allocation that unexpectedly succeeds is still a fault (exit 72).
fn try_memory_pressure(mib: Option<u64>) -> Result<(), ()> {
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

/// Bounded ring of browser stderr, kept for failure diagnostics. Drained
/// nonblocking from the pipe so a chatty browser cannot fill it (the
/// chromium GCM noise is harmless but unbounded).
struct StderrRing {
    buffer: Vec<u8>,
    limit: usize,
}

impl StderrRing {
    fn new(limit: usize) -> Self {
        StderrRing {
            buffer: Vec::new(),
            limit,
        }
    }

    fn drain_from(&mut self, stderr: Option<&mut std::process::ChildStderr>) {
        let Some(stderr) = stderr else { return };
        let mut chunk = [0u8; 4096];
        loop {
            // SAFETY: the child stderr is a valid fd; reads into a stack
            // buffer of 4096 bytes cannot overflow it.
            let n =
                unsafe { libc::read(stderr.as_raw_fd(), chunk.as_mut_ptr().cast(), chunk.len()) };
            if n > 0 {
                self.buffer.extend_from_slice(&chunk[..n as usize]);
                if self.buffer.len() > self.limit {
                    self.buffer.drain(..self.buffer.len() - self.limit);
                }
                continue;
            }
            if n == 0 {
                break;
            }
            let err = std::io::Error::last_os_error();
            if err.kind() == std::io::ErrorKind::Interrupted {
                continue;
            }
            break; // WouldBlock or a real error: nothing more to drain now.
        }
    }

    fn tail(&self) -> String {
        String::from_utf8_lossy(&self.buffer).into_owned()
    }

    /// The bootstrap-failure excerpt (B4c): the last few stderr lines that
    /// are NOT chromium's routine headless noise (no session/system bus in
    /// the sandbox, crashpad's sysfs probes), each clipped, joined on one
    /// line. The daemon keeps only 256 chars of a worker's failure detail,
    /// so the excerpt must lead with the lines that explain the failure —
    /// "Zygote could not fork" / "pthread_create: Resource temporarily
    /// unavailable" under a too-small TasksMax, for example — not with
    /// forty dbus lines that appear on every healthy start too.
    fn diagnostic_tail(&self) -> String {
        const KEEP_LINES: usize = 4;
        const LINE_CHARS: usize = 160;
        const NOISE: [&str; 4] = [
            "dbus/bus.cc",
            "dbus/object_proxy.cc",
            "crashpad/",
            "Failed to send Reap message",
        ];
        let text = self.tail();
        let lines: Vec<&str> = text
            .lines()
            .map(str::trim)
            .filter(|line| !line.is_empty() && !NOISE.iter().any(|noise| line.contains(noise)))
            .collect();
        if lines.is_empty() {
            return "(browser wrote no non-routine stderr lines)".to_string();
        }
        let start = lines.len().saturating_sub(KEEP_LINES);
        lines[start..]
            .iter()
            .map(|line| {
                // Strip chromium's `[pid:tid:date/time:LEVEL:` prefix when
                // present; the source location and message are the signal.
                let stripped = line
                    .strip_prefix('[')
                    .and_then(|rest| rest.find("] ").map(|end| &rest[end + 2..]))
                    .unwrap_or(line);
                stripped.chars().take(LINE_CHARS).collect::<String>()
            })
            .collect::<Vec<_>>()
            .join(" | ")
    }
}

fn socket_pair() -> Result<(RawFd, RawFd)> {
    let fds = kwe_platform::socketpair_stream_cloexec()
        .map_err(|error| anyhow::anyhow!("socketpair failed: {error}"))?;
    Ok((fds[0], fds[1]))
}

fn ensure_ok(response: &kwe_cdp::Response, what: &str) -> Result<()> {
    if let Some(error) = &response.error {
        bail!("{what} failed: {error}");
    }
    Ok(())
}

// ---------------------------------------------------------------------------
// Command line
// ---------------------------------------------------------------------------

#[derive(Debug, Parser)]
#[command(version, about = "Isolated KWE supervised web renderer")]
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
    /// `stretch`. The page lays itself out at the canvas size
    /// (responsive HTML), so there is nothing to letterbox or crop here;
    /// accepted for the uniform supervisor argv.
    #[arg(long, default_value = "aspect", value_parser = ["aspect", "fill", "stretch"])]
    scaling: String,
    /// Content root directory (must contain index.html; daemon-validated
    /// before spawn and preflight-checked again here). Only `--probe` may
    /// omit it.
    #[arg(long, required_unless_present = "probe")]
    content: Option<PathBuf>,
    /// Stall before spawning the browser (supervisor startup test).
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
    /// Session-scoped liveness probe interval in ms: the page's renderer
    /// main thread is probed every interval with Runtime.evaluate("1+1")
    /// through the non-blocking CDP API (see `HeartbeatTracker`). Never
    /// blocks the publish loop, so a wedged page cannot stall the pacing.
    #[arg(long, default_value_t = 5000, value_parser = clap::value_parser!(u64).range(250..=60000))]
    web_heartbeat_ms: u64,
    /// Consecutive heartbeat failures/timeouts (each one is one full
    /// interval without an answer) before the worker exits 73.
    #[arg(long, default_value_t = 3, value_parser = clap::value_parser!(u32).range(1..=10))]
    web_heartbeat_max_failures: u32,
    /// BETA_M2c production network grant channel: request the shared
    /// network namespace for the sandboxed browser (omit bwrap's
    /// --unshare-net) instead of always isolating. The daemon appends this
    /// flag only for a wallpaper whose stored grant record allows network;
    /// clients never pass it themselves.
    #[arg(long)]
    allow_network: bool,
    /// Print a JSON backend report ({backend, browser_version,
    /// protocol_version, sandbox, screencast, heartbeat}) by spawning the
    /// real sandboxed browser over a throwaway content root and answering
    /// Browser.getVersion on the CDP pipe, then exit 0; any failure is a
    /// backend rejection (exit 73). `kwe diagnose` runs this lane with a
    /// 15 s deadline.
    #[arg(long)]
    probe: bool,
}

// ---------------------------------------------------------------------------
// Input channel
// ---------------------------------------------------------------------------

/// Nonblocking stdin reader for the newline-delimited JSON control protocol.
/// Never blocks, never grows: reads are capped per poll, junk is ignored
/// silently, every valid message is acked, and the newest pointer/audio
/// message replaces any pending one (the daemon is the authority on
/// ordering — the wire sequence is never validated for monotonicity here).
struct InputChannel {
    pending: Vec<u8>,
    stdout: std::io::Stdout,
    pointer: Option<PointerMessage>,
    audio: Option<AudioFrame>,
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
            .and_then(|sequence| kwe_input_protocol::InputAck::new(sequence).ok())
            .and_then(|ack| encode_ack_line(&ack).ok());
        match message_type {
            Some("pointer_position") => {
                if let Ok(message) = decode_pointer_line(line) {
                    self.ack(ack.as_deref());
                    self.pointer = Some(message); // latest-wins
                }
            }
            Some("media_state") => {
                // Acked and discarded: a web page has no media transport to
                // command (the page owns its own audio; no-op, documented in
                // docs/BETA_M2.md).
                if decode_media_state(line).is_ok() {
                    self.ack(ack.as_deref());
                }
            }
            Some("audio_bands") => {
                if let Ok(frame) = decode_audio_frame(line) {
                    self.ack(ack.as_deref());
                    self.audio = Some(frame); // latest-wins
                }
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

    fn take_pointer(&mut self) -> Option<PointerMessage> {
        self.pointer.take()
    }

    fn take_audio(&mut self) -> Option<AudioFrame> {
        self.audio.take()
    }
}

// ---------------------------------------------------------------------------
// Screencast decode
// ---------------------------------------------------------------------------

/// Bounded base64 decoder (RFC 4648 standard alphabet, padded). The
/// screencast jpeg arrives base64 inside the 4 MiB CDP message; this
/// explicit bound and hand-rolled decode keep the jpeg path dependency-free
/// and capped (style precedent: the mpv crate was replaced by local FFI in
/// M1e). Returns None on invalid characters, wrong padding, or an
/// over-bound input.
fn decode_base64(input: &str) -> Option<Vec<u8>> {
    let bytes = input.as_bytes();
    if bytes.is_empty() || !bytes.len().is_multiple_of(4) || bytes.len() > MAX_JPEG_BASE64_BYTES {
        return None;
    }
    let table = |c: u8| -> Option<u8> {
        match c {
            b'A'..=b'Z' => Some(c - b'A'),
            b'a'..=b'z' => Some(c - b'a' + 26),
            b'0'..=b'9' => Some(c - b'0' + 52),
            b'+' => Some(62),
            b'/' => Some(63),
            _ => None,
        }
    };
    let mut output = Vec::with_capacity(bytes.len() / 4 * 3);
    for chunk in bytes.chunks_exact(4) {
        // Padding is valid only in the trailing positions ("", "=", "==").
        if chunk[0] == b'=' || chunk[1] == b'=' || (chunk[2] == b'=' && chunk[3] != b'=') {
            return None;
        }
        let padding = if chunk[2] == b'=' {
            2
        } else if chunk[3] == b'=' {
            1
        } else {
            0
        };
        let values = [
            table(chunk[0])?,
            table(chunk[1])?,
            if padding >= 2 { 0 } else { table(chunk[2])? },
            if padding >= 1 { 0 } else { table(chunk[3])? },
        ];
        let n = (u32::from(values[0]) << 18)
            | (u32::from(values[1]) << 12)
            | (u32::from(values[2]) << 6)
            | u32::from(values[3]);
        output.push((n >> 16) as u8);
        if padding < 2 {
            output.push((n >> 8) as u8);
        }
        if padding < 1 {
            output.push(n as u8);
        }
    }
    Some(output)
}

/// Decode one bounded screencast jpeg into opaque BGRA8888 premultiplied
/// pixels (alpha 255) at exactly the spec size. All caps are defense in
/// depth: the screencast is requested at the spec dimensions, so a jpeg at
/// the caps (or one that fails to decode) is a hostile or broken fixture —
/// counted and skipped, never published.
fn decode_screencast(base64: &str, spec: FrameSpec) -> Option<Vec<u8>> {
    let jpeg = decode_base64(base64)?;
    let mut reader = image::ImageReader::new(std::io::Cursor::new(jpeg))
        .with_guessed_format()
        .ok()?;
    // `Limits` is non_exhaustive: start from the crate default (512 MiB
    // max_alloc) and tighten every bound we care about.
    let mut limits = image::Limits::default();
    limits.max_image_width = Some(MAX_DECODE_DIMENSION);
    limits.max_image_height = Some(MAX_DECODE_DIMENSION);
    limits.max_alloc = Some(MAX_DECODE_ALLOC_BYTES);
    reader.limits(limits);
    let decoded = reader.decode().ok()?;
    let rgb = decoded.into_rgb8();
    let (width, height) = rgb.dimensions();
    if u64::from(width) * u64::from(height) > MAX_DECODED_PIXELS {
        return None;
    }
    Some(scale_and_convert(&rgb, spec))
}

/// Convert one decoded RGB8 image into opaque BGRA8888 premultiplied
/// pixels at exactly the spec size. The screencast is delivered at
/// maxWidth/maxHeight = the target spec, so dimensions match in the normal
/// path; a mismatch (compositor aspect rounding, e.g. 160x89 for a 160x90
/// target) is corrected with a bounded nearest-neighbor scale that
/// stretches the delivered image to fill the fixed-size slot (the chosen
/// policy — documented in docs/BETA_M2.md; the frame slot is fixed-size, so
/// letterboxing is unavailable).
fn scale_and_convert(rgb: &image::RgbImage, spec: FrameSpec) -> Vec<u8> {
    let (source_width, source_height) = rgb.dimensions();
    // Defensive zero-dimension guard: the nearest-neighbor mapping below
    // would divide by zero and index an empty image (a decode panic would
    // abort the worker with SIGABRT — a hostile or broken fixture must be
    // skipped and counted, never allowed to take the process down). An
    // empty result fails the caller's exact-size check and is counted as
    // an invalid frame; the spec is always >= 1x1 by construction.
    if source_width == 0 || source_height == 0 {
        return Vec::new();
    }
    let mut output = Vec::with_capacity(spec.pixel_bytes());
    for y in 0..spec.height {
        let source_y = if source_height == spec.height {
            y
        } else {
            (y * source_height) / spec.height
        };
        for x in 0..spec.width {
            let source_x = if source_width == spec.width {
                x
            } else {
                (x * source_width) / spec.width
            };
            let pixel = rgb.get_pixel(source_x, source_y);
            output.extend_from_slice(&[pixel[2], pixel[1], pixel[0], 255]);
        }
    }
    output
}

// ---------------------------------------------------------------------------
// Browser session
// ---------------------------------------------------------------------------

/// One sandboxed chromium with its CDP pipe, stderr ring, and the state the
/// capture loop needs (attached-session id, viewport for the pointer
/// mapping, held-button bitfield, bounded diagnostics counters).
struct BrowserSession {
    client: Client,
    child: Child,
    stderr: StderrRing,
    session_id: String,
    /// Layout (viewport) dimensions from the latest screencast metadata
    /// (--window-size is ignored by headless=new; measured in M2a). The
    /// pointer mapping target; unknown until the first frame.
    viewport: (u32, u32),
    /// CDP mouse-button bitfield of currently held buttons (1 left, 2
    /// right, 4 middle), carried on every dispatched event.
    held_buttons: u8,
    /// Latest decoded frame, consumed by the pacing path.
    pending: Option<Vec<u8>>,
    /// Bounded diagnostics counters.
    frames_seen: u64,
    decode_failures: u64,
    stall_ticks: u64,
    audio_skipped: u64,
    audio_evaluate_errors: u64,
    /// Gate for the 30/s audio evaluate bound.
    last_audio_at: Option<Instant>,
}

impl BrowserSession {
    fn has_viewport(&self) -> bool {
        self.viewport != (0, 0)
    }

    fn take_pending(&mut self) -> Option<Vec<u8>> {
        self.pending.take()
    }

    /// Spawn the sandboxed browser (defense in depth: preflight has already
    /// passed at the daemon, but a re-pointed or swapped content root must
    /// fail closed here too) and run the pinned bootstrap sequence
    /// (docs/BETA_M2.md §2) within the startup deadline: getTargets -> page
    /// target -> attachToTarget{flatten:true} -> Page.enable ->
    /// Page.startScreencast{jpeg, q80, maxWidth/maxHeight = spec,
    /// everyNthFrame:1}. Startup completes only when the first screencast
    /// frame has been acked and decoded, so the daemon's canary sees
    /// progress immediately.
    fn start(content: &Path, spec: FrameSpec, allow_network: bool) -> Result<Self> {
        let deadline = Instant::now() + STARTUP_DEADLINE;
        let (mut child, read_fd, write_fd, page_marker) =
            spawn_browser(content, spec, allow_network)?;
        let stderr = StderrRing::new(STDERR_RING_LIMIT);
        let mut client = Client::new(read_fd, write_fd)?;
        // Stderr must drain or a chatty browser could fill the pipe buffer;
        // set it nonblocking once and drain it inside the pump loop.
        if let Some(child_stderr) = child.stderr.as_mut() {
            set_nonblocking(child_stderr.as_raw_fd())?;
        }

        // B4c: drain the browser's stderr WHILE bootstrapping. The ring used
        // to be first drained after the session existed, so every bootstrap
        // failure reported "chromium stderr tail:" empty — the one line
        // that names a browser that died at exec (B4: the unit's TasksMax
        // cut it off) never reached the daemon.
        let mut stderr = stderr;
        let target_id =
            find_page_target(&mut client, deadline, &mut stderr, &mut child, &page_marker)
                .with_context(|| {
                stderr.drain_from(child.stderr.as_mut());
                format!(
                    "browser bootstrap failed; chromium stderr tail: {}",
                    stderr.diagnostic_tail()
                )
            })?;
        let (session_id, _started_at) = attach_and_start(&mut client, &target_id, spec)
            .with_context(|| {
                stderr.drain_from(child.stderr.as_mut());
                format!(
                    "browser attach/screencast failed; chromium stderr tail: {}",
                    stderr.diagnostic_tail()
                )
            })?;

        let mut session = Self {
            client,
            child,
            stderr,
            session_id,
            viewport: (0, 0),
            held_buttons: 0,
            pending: None,
            frames_seen: 0,
            decode_failures: 0,
            stall_ticks: 0,
            audio_skipped: 0,
            audio_evaluate_errors: 0,
            last_audio_at: None,
        };
        loop {
            let now = Instant::now();
            if now >= deadline {
                bail!(
                    "no decodable screencast frame within the startup deadline (stderr tail: {})",
                    session.stderr.tail()
                );
            }
            session.client.poll((deadline - now).min(MAX_WAIT))?;
            session.stderr.drain_from(session.child.stderr.as_mut());
            while let Some(event) = session.client.next_event() {
                if event.method == "Page.screencastFrame"
                    && event.session_id.as_deref() == Some(&session.session_id)
                    && session.handle_frame(&event, spec)
                {
                    // First frame in hand: the backend is alive and decodable.
                    return Ok(session);
                }
            }
        }
    }

    /// Handle one screencast frame: ack it FIRST (the producer hard-stalls
    /// after exactly 3 unacked frames — docs/BETA_M2.md §1.6), record the
    /// viewport from the metadata, then decode; returns whether a new
    /// frame is now pending. A failed ack or decode is counted with a
    /// bounded diagnostic and never aborts the loop (the pipe EOF or the
    /// supervisor's frame timeout reclaims a genuinely dead browser).
    fn handle_frame(&mut self, event: &Event, spec: FrameSpec) -> bool {
        self.frames_seen = self.frames_seen.saturating_add(1);
        if let Some(metadata) = event.params.get("metadata") {
            let width = metadata.get("deviceWidth").and_then(Value::as_u64);
            let height = metadata.get("deviceHeight").and_then(Value::as_u64);
            if let (Some(width), Some(height)) = (width, height)
                && (width, height) != (0, 0)
            {
                self.viewport = (width as u32, height as u32);
            }
        }
        let Some(frame_session) = event.params.get("sessionId").cloned() else {
            self.decode_failures = self.decode_failures.saturating_add(1);
            diag_decode_failure(self.decode_failures);
            return false;
        };
        match self.client.request_session(
            &self.session_id,
            "Page.screencastFrameAck",
            &json!({ "sessionId": frame_session }),
        ) {
            Ok(response) if response.error.is_none() => {}
            Ok(_) | Err(_) => {
                // An unacked frame stream stalls after 3 more frames; the
                // bounded diagnostics surface the cause.
                self.decode_failures = self.decode_failures.saturating_add(1);
                diag_decode_failure(self.decode_failures);
                return false;
            }
        }
        let Some(data) = event.params.get("data").and_then(Value::as_str) else {
            self.decode_failures = self.decode_failures.saturating_add(1);
            diag_decode_failure(self.decode_failures);
            return false;
        };
        match decode_screencast(data, spec) {
            Some(pixels) => {
                self.pending = Some(pixels);
                true
            }
            None => {
                self.decode_failures = self.decode_failures.saturating_add(1);
                diag_decode_failure(self.decode_failures);
                false
            }
        }
    }

    /// Dispatch one latest-wins pointer message to the page. The daemon
    /// coalesces to one message, so each dispatch replaces any earlier
    /// position; Chromium receives the newest state only. Coordinates are
    /// normalized u16/65535 scaled to the screencast viewport (layout CSS
    /// px; the whole page is uniformly scaled into the spec size, so the
    /// mapping is viewport-independent).
    fn dispatch_pointer(&mut self, message: &PointerMessage) -> Result<()> {
        let (width, height) = self.viewport;
        let x = message.normalized_x() * f64::from(width);
        let y = message.normalized_y() * f64::from(height);
        let (event_type, button, click_count) = match message.phase {
            kwe_input_protocol::PointerPhase::Enter | kwe_input_protocol::PointerPhase::Move => {
                ("mouseMoved", None, 0)
            }
            kwe_input_protocol::PointerPhase::Leave => {
                // Move outside the viewport to clear hover state, and drop
                // the held-button mask: the pointer left the surface, so
                // any button held across the leave is implicitly released —
                // a stale mask would otherwise bleed into every later
                // mouseMoved and leave Chromium thinking a button is still
                // down (drag state that never ends).
                let response = self.client.request_session(
                    &self.session_id,
                    "Input.dispatchMouseEvent",
                    &json!({
                        "type": "mouseMoved",
                        "x": -10.0,
                        "y": -10.0,
                        "buttons": self.held_buttons,
                    }),
                )?;
                ensure_ok(&response, "Input.dispatchMouseEvent (leave)")?;
                self.held_buttons = 0;
                return Ok(());
            }
            kwe_input_protocol::PointerPhase::Down | kwe_input_protocol::PointerPhase::Up => {
                let Some(button) = message.button else {
                    return Ok(()); // protocol-validated; defensive
                };
                let bit = cdp_button_bit(button);
                if message.phase == kwe_input_protocol::PointerPhase::Down {
                    self.held_buttons |= bit;
                } else {
                    self.held_buttons &= !bit;
                }
                (
                    if message.phase == kwe_input_protocol::PointerPhase::Down {
                        "mousePressed"
                    } else {
                        "mouseReleased"
                    },
                    Some(cdp_button_name(button)),
                    1,
                )
            }
        };
        let mut params = json!({
            "type": event_type,
            "x": x,
            "y": y,
            "buttons": self.held_buttons,
            "clickCount": click_count,
        });
        if let Some(button) = button {
            params["button"] = Value::String(button.into());
        }
        let response =
            self.client
                .request_session(&self.session_id, "Input.dispatchMouseEvent", &params)?;
        ensure_ok(&response, "Input.dispatchMouseEvent")
    }

    /// Bounded page-side audio injection: `typeof audio_web === "function"
    /// && audio_web([L...,R...])` with the 64+64 floats JSON-embedded (one
    /// array, left then right — the pinned expression in docs/BETA_M2.md).
    /// At most 30 calls per second; a newer frame replaces an older one
    /// (latest-wins). The page is untrusted, so an evaluate error or
    /// exception is a bounded diagnostic, never a worker failure.
    fn evaluate_audio(&mut self, frame: &AudioFrame) -> Result<()> {
        let now = Instant::now();
        if self
            .last_audio_at
            .is_some_and(|last| now.duration_since(last) < AUDIO_MIN_INTERVAL)
        {
            self.audio_skipped = self.audio_skipped.saturating_add(1);
            diag_audio_skipped(self.audio_skipped);
            return Ok(());
        }
        self.last_audio_at = Some(now);
        let mut bands = Vec::with_capacity(frame.left.len() + frame.right.len());
        bands.extend_from_slice(&frame.left);
        bands.extend_from_slice(&frame.right);
        let expression = format!(
            "typeof audio_web === \"function\" && audio_web({})",
            serde_json::to_string(&bands)?
        );
        let response = self.client.request_session(
            &self.session_id,
            "Runtime.evaluate",
            &json!({ "expression": expression }),
        )?;
        if let Some(error) = response.error {
            bail!("Runtime.evaluate failed: {error}");
        }
        if response
            .result
            .as_ref()
            .and_then(|result| result.get("exceptionDetails"))
            .is_some()
        {
            self.audio_evaluate_errors = self.audio_evaluate_errors.saturating_add(1);
            diag_audio_evaluate_error(self.audio_evaluate_errors);
        }
        Ok(())
    }

    /// Bounded teardown: close the CDP pipe ends (chromium exits rc=0
    /// within ~50 ms), reap with a deadline, then SIGTERM and SIGKILL the
    /// bwrap process group (the sandbox child dies with its parent, and
    /// --die-with-parent covers the wedge case). Consumes the session so
    /// the pipe close and the reap use the same owned state.
    fn stop(self) {
        drop(self.client); // closes fds 3/4 opposite ends; the teardown signal
        let mut child = self.child;
        let deadline = Instant::now() + CHILD_EXIT_DEADLINE;
        loop {
            if let Some(status) = child.try_wait().ok().flatten() {
                eprintln!(
                    "event=renderer.web.browser_exit rc={}",
                    status.code().unwrap_or(-1)
                );
                return;
            }
            if Instant::now() >= deadline {
                break;
            }
            std::thread::sleep(Duration::from_millis(20));
        }
        eprintln!("event=renderer.web.browser_wedged forcing termination");
        let pid = child.id() as libc::pid_t;
        // SAFETY: the sandboxed child was placed in its own process group
        // (setpgid in pre_exec); a negative pid restricts delivery to it.
        unsafe {
            libc::kill(-pid, libc::SIGTERM);
        }
        let term_deadline = Instant::now() + TERM_GRACE;
        loop {
            if child.try_wait().ok().flatten().is_some() {
                return;
            }
            if Instant::now() >= term_deadline {
                break;
            }
            std::thread::sleep(Duration::from_millis(20));
        }
        // SAFETY: same process group as above; SIGKILL cannot be blocked.
        unsafe {
            libc::kill(-pid, libc::SIGKILL);
        }
        let _ = child.wait();
    }
}

/// Spawn `bwrap` with the sandboxed chromium command; the CDP pipe ends
/// land on fds 3/4 in the sandboxed child (they survive bwrap's exec of
/// chromium because they are real, non-CLOEXEC descriptors — verified and
/// pinned in docs/BETA_M2.md). The worker keeps only its own pipe ends.
fn spawn_browser(
    content: &Path,
    spec: FrameSpec,
    allow_network: bool,
) -> Result<(Child, RawFd, RawFd, String)> {
    let web_command = web_renderer_command(content, allow_network, spec.width, spec.height);
    // What the page's CDP target URL must contain: `/wallpaper/index.html`
    // inside the Linux namespace, the real content path on macOS.
    let page_marker = kwe_core::page_url_marker(&web_command);
    match web_command.sandbox {
        "bwrap" | "seatbelt" => {}
        weakened => eprintln!(
            "event=renderer.web.sandbox_weakened mode={weakened} detail=KWE_WEB_SANDBOX is set; the OS sandbox around the browser is reduced or off"
        ),
    }
    let (client_read, browser_write) = socket_pair()?;
    let (browser_read, client_write) = socket_pair()?;
    let mut process = Command::new(&web_command.program);
    process
        .args(&web_command.arguments)
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::piped());
    if let Some(directory) = &web_command.working_dir {
        process.current_dir(directory);
    }
    unsafe {
        process.pre_exec(move || {
            // The sandboxed child must be its own process group so the
            // worker can signal the whole bwrap tree without signaling
            // itself (the daemon already made this worker a group leader).
            if libc::setpgid(0, 0) != 0 {
                return Err(std::io::Error::last_os_error());
            }
            // Order-independent fd setup (reviewed spike pattern): the
            // socketpair ends may themselves sit at fd 3 or fd 4, and a
            // direct dup2(old, 3) could clobber the other end before its
            // turn. Duplicate both ends to temp fds (>= 5) first, then
            // move the temps onto 3/4; temps that high make every dup2 a
            // real dup, which also clears FD_CLOEXEC (chromium checks
            // fcntl(3/4, F_GETFL) at startup and bails on closed
            // descriptors). Finally close temps and the originals.
            let temp_read = libc::fcntl(browser_read, libc::F_DUPFD_CLOEXEC, 5);
            if temp_read < 0 {
                return Err(std::io::Error::last_os_error());
            }
            let temp_write = libc::fcntl(browser_write, libc::F_DUPFD_CLOEXEC, 5);
            if temp_write < 0 {
                libc::close(temp_read);
                return Err(std::io::Error::last_os_error());
            }
            if libc::dup2(temp_read, CHROMIUM_READ_FD) < 0 {
                libc::close(temp_read);
                libc::close(temp_write);
                return Err(std::io::Error::last_os_error());
            }
            if libc::dup2(temp_write, CHROMIUM_WRITE_FD) < 0 {
                libc::close(temp_read);
                libc::close(temp_write);
                return Err(std::io::Error::last_os_error());
            }
            libc::close(temp_read);
            libc::close(temp_write);
            // A browser end that already sits at fd 3 or fd 4 IS the
            // descriptor chromium now runs on: closing it would sever the
            // pipe (chromium checks fcntl(3/4, F_GETFL) at startup and
            // bails on a missing descriptor). Close only the non-aliased
            // originals; an aliased one was replaced by its own dup2.
            if browser_read != CHROMIUM_READ_FD && browser_read != CHROMIUM_WRITE_FD {
                libc::close(browser_read);
            }
            if browser_write != CHROMIUM_READ_FD && browser_write != CHROMIUM_WRITE_FD {
                libc::close(browser_write);
            }
            Ok(())
        });
    }
    let child = process
        .spawn()
        .with_context(|| format!("spawning {}", web_command.program))?;
    // The parent must not keep the browser's ends: the dup2'd copies in the
    // child keep the pipes alive, and a stray parent reference to the write
    // end would mask EOF on the client's read side (the teardown signal).
    // Guarded against aliasing: a browser end that shares a number with a
    // client end must survive, or the client transport would lose the pipe.
    unsafe {
        if browser_read != client_read && browser_read != client_write {
            libc::close(browser_read);
        }
        if browser_write != client_read && browser_write != client_write {
            libc::close(browser_write);
        }
    }
    Ok((child, client_read, client_write, page_marker))
}

/// Whether a CDP target URL names the wallpaper page. Chromium reports
/// `file://` URLs percent-encoded (`Application Support` -> `Application%20Support`),
/// so the raw marker is also compared against the percent-decoded URL.
fn url_matches_marker(url: &str, page_marker: &str) -> bool {
    url.contains(page_marker) || percent_decode(url).contains(page_marker)
}

/// Minimal `%XX` decoder (bytes, lossy UTF-8); never fails.
fn percent_decode(text: &str) -> String {
    let bytes = text.as_bytes();
    let mut out = Vec::with_capacity(bytes.len());
    let mut index = 0;
    let hex_value = |byte: u8| (byte as char).to_digit(16).map(|digit| digit as u8);
    while index < bytes.len() {
        if bytes[index] == b'%'
            && index + 2 < bytes.len()
            && let (Some(high), Some(low)) = (hex_value(bytes[index + 1]), hex_value(bytes[index + 2]))
        {
            out.push(high << 4 | low);
            index += 3;
            continue;
        }
        out.push(bytes[index]);
        index += 1;
    }
    String::from_utf8_lossy(&out).into_owned()
}

/// getTargets until the fixture page target appears (headless=new starts on
/// a pre-navigation target that later becomes the fixture page; measured in
/// M2a). Bounded by the startup deadline.
fn find_page_target(
    client: &mut Client,
    deadline: Instant,
    stderr: &mut StderrRing,
    child: &mut Child,
    page_marker: &str,
) -> Result<String> {
    loop {
        // Keep the diagnostics ring current on every round: a browser that
        // dies here leaves its last words on stderr, and the caller folds
        // the tail into the bootstrap error (B4c).
        stderr.drain_from(child.stderr.as_mut());
        let response = client.request_browser("Target.getTargets", &json!({}))?;
        ensure_ok(&response, "Target.getTargets")?;
        let pages: Vec<&Value> = response
            .result
            .as_ref()
            .and_then(|result| result["targetInfos"].as_array())
            .map(|infos| infos.iter().filter(|info| info["type"] == "page").collect())
            .unwrap_or_default();
        if let Some(target) = pages.iter().find(|info| {
            info["url"]
                .as_str()
                .is_some_and(|url| url_matches_marker(url, page_marker))
        }) {
            return Ok(target["targetId"]
                .as_str()
                .context("fixture target has no targetId")?
                .to_owned());
        }
        // A lone page target with a non-empty URL is the fixture page once
        // navigation completed.
        if pages.len() == 1
            && let Some(url) = pages[0]["url"].as_str()
            && !url.is_empty()
        {
            return Ok(pages[0]["targetId"]
                .as_str()
                .context("page target has no targetId")?
                .to_owned());
        }
        if Instant::now() >= deadline {
            let urls: Vec<&str> = pages
                .iter()
                .filter_map(|info| info["url"].as_str())
                .collect();
            bail!(
                "no page target within the startup deadline; page urls: {urls:?} (the caller folds in the chromium stderr tail)"
            );
        }
        client.poll(Duration::from_millis(100))?;
    }
}

/// Attach (flattened) and start the screencast at the spec dimensions;
/// returns the attached session id.
fn attach_and_start(
    client: &mut Client,
    target_id: &str,
    spec: FrameSpec,
) -> Result<(String, Instant)> {
    let response = client.request_browser(
        "Target.attachToTarget",
        &json!({ "targetId": target_id, "flatten": true }),
    )?;
    ensure_ok(&response, "Target.attachToTarget")?;
    let session_id = response
        .result
        .as_ref()
        .and_then(|result| result.get("sessionId"))
        .and_then(Value::as_str)
        .context("attachToTarget response lacks sessionId")?
        .to_owned();

    let response = client.request_session(&session_id, "Page.enable", &json!({}))?;
    ensure_ok(&response, "Page.enable")?;

    let started_at = Instant::now();
    let response = client.request_session(
        &session_id,
        "Page.startScreencast",
        &json!({
            "format": "jpeg",
            "quality": 80,
            "maxWidth": spec.width,
            "maxHeight": spec.height,
            "everyNthFrame": 1,
        }),
    )?;
    ensure_ok(&response, "Page.startScreencast")?;
    Ok((session_id, started_at))
}

fn cdp_button_name(button: PointerButton) -> &'static str {
    match button {
        PointerButton::Primary => "left",
        PointerButton::Secondary => "right",
        PointerButton::Middle => "middle",
    }
}

fn cdp_button_bit(button: PointerButton) -> u8 {
    match button {
        PointerButton::Primary => 1,
        PointerButton::Secondary => 2,
        PointerButton::Middle => 4,
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

/// Pure keepalive decision (mirror of the video renderer): a new frame is
/// published only at the pacing deadline; past it, a stale frame is
/// re-published (keepalive) so the supervisor's frame timeout never trips
/// (a static page emits no screencast frames at all); before the deadline,
/// or with nothing published yet, wait. An empty frame is never published.
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
// Heartbeat
// ---------------------------------------------------------------------------

/// Result of one in-flight (or attempted) probe, computed by the caller
/// from the transport; the tracker only decides what it means.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum HeartbeatProbeResult {
    /// A response arrived; the bool is whether it carried no CDP error
    /// envelope (a protocol error counts as a failure).
    Answered(bool),
    /// No response within the probe deadline (one full interval).
    TimedOut,
    /// The send itself failed (transport dead, in-flight cap reached).
    SendFailed,
}

/// Outcome of one heartbeat evaluation tick.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum HeartbeatOutcome {
    /// Within bounds; keep running.
    Continue,
    /// `max_failures` consecutive failures/timeouts crossed; the worker
    /// must shut down with exit 73 (bounded stderr diagnostic emitted by
    /// the caller).
    Exceeded { consecutive: u32 },
}

/// Page-independent liveness probe (adversarial review MUST-FIX 2): a page
/// whose renderer main thread wedges after first paint stops answering CDP
/// — screencast acks included — while the browser process survives and the
/// keepalive re-publication keeps the supervisor's frame timeout from ever
/// tripping. Without a page-independent probe the dead stream would be
/// masked forever. Every `interval`, the capture loop sends a
/// session-scoped `Runtime.evaluate("1+1")` through the non-blocking
/// [`Client::send_session`] (the response is checked on later ticks; a
/// blocking probe would stall the publish pipeline past the supervisor's
/// frame timeout, so the daemon would reap the worker before this exit-73
/// path could fire). A static page answers probes fine — the heartbeat
/// only trips when the page is genuinely unresponsive. Pure decision
/// logic, unit-tested below.
struct HeartbeatTracker {
    interval: Duration,
    max_failures: u32,
    consecutive_failures: u32,
    /// Earliest time the next probe may be sent.
    next_probe_at: Instant,
    /// In-flight probe: request id and the moment it was sent (its
    /// deadline is one interval later). At most one probe is ever in
    /// flight.
    pending: Option<(u32, Instant)>,
}

impl HeartbeatTracker {
    fn new(interval: Duration, max_failures: u32, now: Instant) -> Self {
        Self {
            interval,
            max_failures,
            consecutive_failures: 0,
            next_probe_at: now + interval,
            pending: None,
        }
    }

    /// Whether a fresh probe should be sent now: nothing in flight and the
    /// interval has elapsed since the last resolution.
    fn should_probe(&self, now: Instant) -> bool {
        self.pending.is_none() && now >= self.next_probe_at
    }

    /// The in-flight probe, if any.
    fn pending_probe(&self) -> Option<(u32, Instant)> {
        self.pending
    }

    /// Record that a probe was sent.
    fn note_sent(&mut self, id: u32, now: Instant) {
        self.pending = Some((id, now));
    }

    /// Resolve the current probe with the transport's answer. Success
    /// resets the streak; a protocol-error envelope, a timeout, or a
    /// failed send all count as one consecutive failure. The next probe is
    /// scheduled one full interval from `now` (a failed probe is retried,
    /// not hammered). Crossing `max_failures` consecutive failures returns
    /// `Exceeded`.
    fn resolve(&mut self, now: Instant, result: HeartbeatProbeResult) -> HeartbeatOutcome {
        self.pending = None;
        match result {
            HeartbeatProbeResult::Answered(true) => {
                self.consecutive_failures = 0;
            }
            HeartbeatProbeResult::Answered(false)
            | HeartbeatProbeResult::TimedOut
            | HeartbeatProbeResult::SendFailed => {
                self.consecutive_failures = self.consecutive_failures.saturating_add(1);
            }
        }
        self.next_probe_at = now + self.interval;
        if self.consecutive_failures >= self.max_failures {
            HeartbeatOutcome::Exceeded {
                consecutive: self.consecutive_failures,
            }
        } else {
            HeartbeatOutcome::Continue
        }
    }
}

// ---------------------------------------------------------------------------
// Worker loop
// ---------------------------------------------------------------------------

struct WebWorker {
    arguments: Arguments,
    content: PathBuf,
    spec: FrameSpec,
    writer: SharedFrameWriter,
    input: InputChannel,
    published: u64,
    browser: Option<BrowserSession>,
    input_errors: u64,
    invalid_frames: u64,
}

impl WebWorker {
    /// Synthetic faults keyed on the published-frame count, mirroring the
    /// test/video renderers' fault block exactly (order: exit, corrupt,
    /// hang, memory; exit codes 70/71/72). Hang/corrupt park forever with
    /// the browser torn down, so the supervisor's kill/restart machinery is
    /// what reclaims the worker.
    fn check_faults(&mut self) -> Option<i32> {
        if let Some(after) = self.arguments.exit_after
            && self.published >= after
        {
            eprintln!("event=renderer.fault kind=exit_after");
            return Some(EXIT_EXIT_AFTER);
        }
        if let Some(after) = self.arguments.corrupt_after
            && self.published >= after
        {
            eprintln!("event=renderer.fault kind=corrupt_after");
            self.writer.corrupt_magic_for_test();
            self.browser.take(); // bounded browser teardown, then park
            park_forever();
        }
        if let Some(after) = self.arguments.hang_after
            && self.published >= after
        {
            eprintln!("event=renderer.fault kind=hang_after");
            self.browser.take();
            park_forever();
        }
        if let Some(after) = self.arguments.memory_pressure_after
            && self.published >= after
        {
            eprintln!("event=renderer.fault kind=memory_pressure_after");
            let result = try_memory_pressure(self.arguments.memory_pressure_mib);
            self.browser.take();
            return match result {
                // An allocation that unexpectedly succeeded is itself the
                // anomaly: exit 72 (mirrors the test renderer exactly).
                Ok(()) => Some(EXIT_MEMORY_UNEXPECTED),
                Err(()) => Some(EXIT_MEMORY_DENIED),
            };
        }
        None
    }

    fn run(&mut self) -> Result<()> {
        // Defense in depth: the daemon validated the content root already;
        // re-run the bounded preflight here so a root swapped between the
        // daemon's check and this spawn fails closed. The browser is
        // isolated unless the --allow-network test hook was given (the
        // wallpaper network-grant lane lands in M2c; until then the
        // production path always requests isolation).
        let report = preflight_web(&self.content, &[]);
        if !report.safe {
            bail!(
                "web preflight rejected {}: {}",
                self.content.display(),
                report.reasons.join("; ")
            );
        }
        let browser =
            match BrowserSession::start(&self.content, self.spec, self.arguments.allow_network) {
                Ok(session) => session,
                // One bounded retry, and only for a browser that never
                // reached its page (died or produced no target): a
                // browser's first-ever launch on a machine can fail its own
                // first-run setup (measured on macOS: backup-exclusion XPC
                // on a fresh account) and the daemon never retries a
                // refusal. Attach/screencast failures on a live browser
                // (a busy page) are the content's fault and are not retried.
                Err(first) if format!("{first:#}").contains("browser bootstrap failed") => {
                    eprintln!("event=renderer.web.bootstrap_retry detail={first:#}");
                    BrowserSession::start(&self.content, self.spec, self.arguments.allow_network)?
                }
                Err(error) => return Err(error),
            };
        self.browser = Some(browser);
        self.capture_loop()
    }

    fn capture_loop(&mut self) -> Result<()> {
        let interval = Duration::from_secs_f64(1.0 / f64::from(self.arguments.fps));
        let mut deadline = Instant::now();
        let mut last_pixels: Option<Vec<u8>> = None;
        let mut heartbeat = HeartbeatTracker::new(
            Duration::from_millis(self.arguments.web_heartbeat_ms),
            self.arguments.web_heartbeat_max_failures,
            Instant::now(),
        );
        loop {
            self.input.poll();
            self.dispatch_input();
            if let Some(code) = self.check_faults() {
                self.browser.take();
                exit(code);
            }
            if TERMINATED.load(Ordering::Acquire) {
                self.writer.set_state(ProducerState::Stopping);
                eprintln!("event=renderer.complete frames={}", self.published);
                // Bounded browser teardown: close the pipe ends, reap, and
                // escalate to SIGTERM/SIGKILL on the bwrap group (the
                // daemon's stop grace is 500 ms by default).
                if let Some(browser) = self.browser.take() {
                    browser.stop();
                }
                return Ok(());
            }
            // Heartbeat (page-independent liveness): strictly non-blocking —
            // at most one probe in flight, the answer is checked on later
            // ticks, and sending a probe never stalls the publish pacing (a
            // blocking probe would trip the supervisor's frame timeout and
            // the daemon would reap the worker before this exit-73 path).
            // Responses delivered by the previous poll are consumed here,
            // so resolution lags a probe by at most one MAX_WAIT.
            let heartbeat_outcome = {
                let browser = self.browser.as_mut().expect("browser present");
                let now = Instant::now();
                let mut outcome = HeartbeatOutcome::Continue;
                if let Some((probe_id, sent_at)) = heartbeat.pending_probe() {
                    let result = match browser.client.take_response(probe_id) {
                        Some(response) => {
                            Some(HeartbeatProbeResult::Answered(response.error.is_none()))
                        }
                        None if now.duration_since(sent_at) >= heartbeat.interval => {
                            Some(HeartbeatProbeResult::TimedOut)
                        }
                        None => None, // still in flight: wait for the next tick
                    };
                    if let Some(result) = result {
                        outcome = heartbeat.resolve(now, result);
                    }
                }
                if heartbeat.should_probe(now) {
                    match browser.client.send_session(
                        &browser.session_id,
                        "Runtime.evaluate",
                        &json!({ "expression": "1+1" }),
                    ) {
                        Ok(id) => heartbeat.note_sent(id, now),
                        Err(error) => {
                            eprintln!("event=renderer.web.heartbeat_send_failed detail={error}");
                            outcome = heartbeat.resolve(now, HeartbeatProbeResult::SendFailed);
                        }
                    }
                }
                outcome
            };
            if let HeartbeatOutcome::Exceeded { consecutive } = heartbeat_outcome {
                eprintln!("event=renderer.web.heartbeat_failed consecutive={consecutive}");
                // Bounded browser teardown, then exit 73 (the supervisor
                // folds it into `exit_code_73` and restarts the worker).
                if let Some(browser) = self.browser.take() {
                    browser.stop();
                }
                exit(EXIT_BACKEND_REJECT);
            }
            let Some(browser) = self.browser.as_mut() else {
                unreachable!("the browser is always present in the capture loop");
            };
            let now = Instant::now();
            let wait = if now < deadline {
                deadline.duration_since(now).min(MAX_WAIT)
            } else {
                Duration::ZERO
            };
            // The poll is the wait: a deadline hit with events pending
            // returns immediately, and TERMINATED/input are observed at
            // least every 50 ms. Any error here (EOF when the browser
            // died, oversize message, parse) is a backend rejection.
            browser.client.poll(wait).context("CDP pipe")?;
            browser.stderr.drain_from(browser.child.stderr.as_mut());
            while let Some(event) = browser.client.next_event() {
                if event.method == "Page.screencastFrame"
                    && event.session_id.as_deref() == Some(&browser.session_id)
                {
                    browser.handle_frame(&event, self.spec);
                }
            }
            let now = Instant::now();
            if now < deadline {
                continue;
            }
            match next_publish(
                now,
                deadline,
                browser.pending.is_some(),
                last_pixels.is_some(),
            ) {
                PublishDecision::NewFrame => {
                    if let Some(pixels) = browser.take_pending()
                        && pixels.len() == self.spec.pixel_bytes()
                    {
                        // Exact-size check: the conversion is exact by
                        // construction; a mismatch means a malformed frame,
                        // which is skipped and counted, never published.
                        self.published = self.writer.publish(&pixels)?;
                        last_pixels = Some(pixels);
                    } else {
                        self.invalid_frames = self.invalid_frames.saturating_add(1);
                        diag_invalid_frame(self.invalid_frames);
                    }
                }
                PublishDecision::Keepalive => {
                    // No new frame within one pacing interval (static page,
                    // paused animation): re-publish the last frame with a
                    // new sequence. The pixels are identical — the
                    // supervisor only watches sequence progression.
                    if let Some(pixels) = &last_pixels {
                        self.published = self.writer.publish(pixels)?;
                        browser.stall_ticks = browser.stall_ticks.saturating_add(1);
                        diag_stall(browser.stall_ticks);
                    }
                }
                PublishDecision::Wait => {}
            }
            deadline = now + interval;
        }
    }

    /// Bounded CDP dispatch of the latest-wins input actions: at most one
    /// pointer event per tick (the pointer waits for the first frame's
    /// viewport — the daemon only sends pointer traffic to a live
    /// renderer), and one audio evaluate per tick behind the 30/s gate.
    fn dispatch_input(&mut self) {
        let Some(browser) = self.browser.as_mut() else {
            return;
        };
        if browser.has_viewport()
            && let Some(message) = self.input.take_pointer()
            && let Err(error) = browser.dispatch_pointer(&message)
        {
            self.input_errors = self.input_errors.saturating_add(1);
            diag_input_error("pointer", &error, self.input_errors);
        }
        if let Some(frame) = self.input.take_audio()
            && let Err(error) = browser.evaluate_audio(&frame)
        {
            self.input_errors = self.input_errors.saturating_add(1);
            diag_input_error("audio", &error, self.input_errors);
        }
    }
}

/// Bounded diagnostic for malformed frames: first occurrences, then every
/// thousandth, so a misbehaving page cannot flood the daemon's stderr ring.
fn diag_invalid_frame(count: u64) {
    if count <= 10 || count.is_multiple_of(1000) {
        eprintln!("event=renderer.web.invalid_frame count={count}");
    }
}

fn diag_decode_failure(count: u64) {
    if count <= 10 || count.is_multiple_of(1000) {
        eprintln!("event=renderer.web.decode_failure count={count}");
    }
}

fn diag_stall(count: u64) {
    if count <= 5 || count.is_multiple_of(1000) {
        eprintln!("event=renderer.web.keepalive count={count}");
    }
}

fn diag_audio_skipped(count: u64) {
    if count <= 10 || count.is_multiple_of(1000) {
        eprintln!("event=renderer.web.audio_rate_limited count={count}");
    }
}

fn diag_audio_evaluate_error(count: u64) {
    if count <= 10 || count.is_multiple_of(1000) {
        eprintln!("event=renderer.web.audio_evaluate_error count={count}");
    }
}

fn diag_input_error(kind: &str, error: &anyhow::Error, count: u64) {
    if count <= 10 || count.is_multiple_of(1000) {
        eprintln!("event=renderer.web.input_error kind={kind} detail={error} count={count}");
    }
}

// ---------------------------------------------------------------------------
// --probe (BETA_M2e)
// ---------------------------------------------------------------------------

/// Bounded reap for probe mode: after the CDP pipe ends close (the drop
/// signal), the sandboxed browser exits rc=0 within ~50 ms (pinned in
/// docs/BETA_M2.md §1.7); wait with a deadline, then SIGKILL the bwrap
/// process group (its own group: setpgid in spawn_browser's pre_exec).
/// Mirror of [`BrowserSession::stop`]'s escalation without the stderr ring.
fn reap_browser(child: &mut Child) {
    let deadline = Instant::now() + CHILD_EXIT_DEADLINE;
    loop {
        if child.try_wait().ok().flatten().is_some() {
            return;
        }
        if Instant::now() >= deadline {
            break;
        }
        std::thread::sleep(Duration::from_millis(20));
    }
    eprintln!("event=renderer.web.probe_browser_wedged forcing termination");
    let pid = child.id() as libc::pid_t;
    // SAFETY: the sandboxed child was placed in its own process group; a
    // negative pid restricts delivery to it.
    unsafe {
        libc::kill(-pid, libc::SIGKILL);
    }
    let _ = child.wait();
}

/// Deadline for the probe's one-frame screencast round-trip. First frames
/// arrive 20–53 ms after Page.startScreencast on this stack
/// (docs/BETA_M2.md §1.7); 5 s leaves an order of magnitude for a cold
/// compositor. The overall probe stays far inside `kwe diagnose`'s 15 s
/// budget (measured ≈0.7 s).
const PROBE_FRAME_DEADLINE: Duration = Duration::from_secs(5);

/// Receive and ack screencast frames until at least one has arrived.
/// Every frame is acked with `Page.screencastFrameAck` (the producer
/// hard-stalls after exactly 3 unacked frames — docs/BETA_M2.md §1.6);
/// acking is part of the round-trip under test, so an ack failure is a
/// backend rejection, not a tolerated diagnostic.
fn wait_for_probe_frame(client: &mut Client, session_id: &str) -> Result<u32> {
    let deadline = Instant::now() + PROBE_FRAME_DEADLINE;
    let mut frames = 0u32;
    while Instant::now() < deadline {
        client.poll(
            deadline
                .saturating_duration_since(Instant::now())
                .min(MAX_WAIT),
        )?;
        while let Some(event) = client.next_event() {
            if event.method != "Page.screencastFrame"
                || event.session_id.as_deref() != Some(session_id)
            {
                continue;
            }
            let Some(frame_session) = event.params.get("sessionId").cloned() else {
                continue;
            };
            let response = client.request_session(
                session_id,
                "Page.screencastFrameAck",
                &json!({ "sessionId": frame_session }),
            )?;
            ensure_ok(&response, "Page.screencastFrameAck")?;
            frames = frames.saturating_add(1);
        }
        if frames >= 1 {
            return Ok(frames);
        }
    }
    bail!(
        "no Page.screencastFrame within {PROBE_FRAME_DEADLINE:?} of Page.startScreencast (capture round-trip failed)"
    );
}

/// Three bounded boot-class round trips through the real sandboxed browser
/// (bwrap prefix + headless chromium, network-isolated, throwaway tmpfs
/// profile — the supervised command from [`web_renderer_command`] with the
/// screencast viewport): Browser.getVersion (boot + CDP pipe),
/// Page.startScreencast with one received-and-acked frame (paint -> capture
/// -> pipe -> ack), and Runtime.evaluate("1+1") answering 2 (the worker's
/// heartbeat probe). The browser is torn down on every path (pipe close ->
/// rc=0, then the bounded reap), so a hung browser cannot leak. The probe
/// covers boot-class failures only: the daemon's per-kind rlimit envelope
/// (docs/BETA_M2.md §5.4) is applied by the supervisor at spawn, not by the
/// probe, so an rlimit-induced failure can pass the probe yet fail a
/// supervised launch — that gap is bounded by the envelope's own validation
/// and the supervisor's failure budget. `kwe diagnose` wraps this lane in a
/// 15 s overall deadline.
fn probe_browser_version(content: &Path) -> Result<Value> {
    let spec = FrameSpec::new(160, 90)?;
    let (mut child, read_fd, write_fd, page_marker) = spawn_browser(content, spec, false)?;
    let mut client = Client::new(read_fd, write_fd)?;
    // Same bootstrap diagnostics as the supervised lane (B4c): the probe's
    // failure message carries the browser's last stderr lines.
    let mut stderr = StderrRing::new(STDERR_RING_LIMIT);
    if let Some(child_stderr) = child.stderr.as_mut() {
        set_nonblocking(child_stderr.as_raw_fd())?;
    }
    let report: Result<Value> = (|| {
        // 1. Boot + CDP pipe: the browser must answer Browser.getVersion.
        let response = client
            .request_browser("Browser.getVersion", &json!({}))
            .with_context(|| {
                stderr.drain_from(child.stderr.as_mut());
                format!(
                    "browser did not answer Browser.getVersion within the CDP request timeout; chromium stderr tail: {}",
                    stderr.diagnostic_tail()
                )
            })?;
        ensure_ok(&response, "Browser.getVersion")?;
        let result = response.result.unwrap_or_default();
        // Chromium 151 answers with `product` ("Chrome/151.0.7922.137");
        // older DevTools versions used `browser` — read either.
        let browser_version = result
            .get("product")
            .or_else(|| result.get("browser"))
            .and_then(Value::as_str)
            .unwrap_or("unknown");
        let protocol_version = result
            .get("protocolVersion")
            .and_then(Value::as_str)
            .unwrap_or("unknown");

        // 2. Capture round-trip: attach and start the screencast, then
        // receive and ack at least one frame (the probe page animates, so
        // the compositor keeps producing frames — see PROBE_PAGE).
        let target_id = find_page_target(
            &mut client,
            Instant::now() + STARTUP_DEADLINE,
            &mut stderr,
            &mut child,
            &page_marker,
        )
        .with_context(|| {
            stderr.drain_from(child.stderr.as_mut());
            format!(
                "browser bootstrap failed; chromium stderr tail: {}",
                stderr.tail()
            )
        })?;
        let (session_id, _started_at) = attach_and_start(&mut client, &target_id, spec)
            .with_context(|| {
                stderr.drain_from(child.stderr.as_mut());
                format!(
                    "browser attach/screencast failed; chromium stderr tail: {}",
                    stderr.diagnostic_tail()
                )
            })?;
        let frames = wait_for_probe_frame(&mut client, &session_id).with_context(|| {
            stderr.drain_from(child.stderr.as_mut());
            format!(
                "capture round-trip failed; chromium stderr tail: {}",
                stderr.diagnostic_tail()
            )
        })?;

        // 3. Heartbeat round-trip: Runtime.evaluate("1+1") must answer 2
        // (the worker's wedged-page probe, docs/BETA_M2.md §5.3).
        let response = client.request_session(
            &session_id,
            "Runtime.evaluate",
            &json!({ "expression": "1+1" }),
        )?;
        ensure_ok(&response, "Runtime.evaluate")?;
        let evaluated = response.result.unwrap_or_default();
        if evaluated.get("exceptionDetails").is_some() {
            bail!("Runtime.evaluate heartbeat raised an exception");
        }
        let heartbeat_value = evaluated
            .get("result")
            .and_then(|result| result.get("value"))
            .and_then(Value::as_u64)
            .map(|value| value.to_string())
            .unwrap_or_else(|| "unknown".to_string());
        if heartbeat_value != "2" {
            bail!("Runtime.evaluate heartbeat did not answer 1+1=2 (got {heartbeat_value:?})");
        }

        Ok(json!({
            "backend": "chromium",
            "browser_version": browser_version,
            "protocol_version": protocol_version,
            "sandbox": "bwrap",
            "screencast": "jpeg-q80",
            "screencast_frames": frames,
            "heartbeat": true,
            "heartbeat_value": heartbeat_value,
        }))
    })();
    drop(client); // closes the CDP pipe ends: the teardown signal
    reap_browser(&mut child);
    report
}

/// `--probe` entry: create a throwaway content root, drive the real
/// sandboxed browser against it, print the one-line JSON backend report and
/// exit 0. A failure (missing bwrap/chromium, browser that never answers,
/// sandbox that cannot boot) is a backend rejection — the caller exits 73.
fn probe_report() -> Result<()> {
    let root = std::env::temp_dir().join(format!("kwe-web-probe-{}", std::process::id()));
    fs::create_dir_all(&root).context("create probe content root")?;
    fs::write(root.join("index.html"), PROBE_PAGE).context("write probe index.html")?;
    let report = match probe_browser_version(&root) {
        Ok(report) => report,
        Err(error) => {
            let _ = fs::remove_dir_all(&root);
            return Err(error);
        }
    };
    let _ = fs::remove_dir_all(&root);
    println!("{report}");
    Ok(())
}

// ---------------------------------------------------------------------------
// Entry point
// ---------------------------------------------------------------------------

fn main() -> Result<()> {
    kwe_platform::guard_parent_exit(libc::SIGKILL);
    let arguments = Arguments::parse();
    if arguments.probe {
        match probe_report() {
            Ok(()) => return Ok(()),
            Err(error) => {
                // Backend rejection (mirror of the worker's main error
                // mapping): bounded stderr diagnostic and exit 73, which
                // `kwe diagnose`'s web lane reports as a failed probe.
                eprintln!("event=renderer.web.backend_reject detail={error}");
                eprintln!("event=renderer.web.backend_reject exit_code={EXIT_BACKEND_REJECT}");
                exit(EXIT_BACKEND_REJECT);
            }
        }
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
    let output = arguments.output.clone().context("--output is required")?;
    let content = arguments.content.clone().context("--content is required")?;
    let spec = FrameSpec::new(arguments.width, arguments.height)?;
    let writer = SharedFrameWriter::create(&output, spec)
        .with_context(|| format!("create frame mapping {}", output.to_string_lossy()))?;
    let mut worker = WebWorker {
        arguments,
        content,
        spec,
        writer,
        input,
        published: 0,
        browser: None,
        input_errors: 0,
        invalid_frames: 0,
    };
    match worker.run() {
        Ok(()) => Ok(()),
        Err(error) => {
            // Backend rejection: bounded stderr diagnostic and exit 73, the
            // supervisor folds it into `exit_code_73` (docs/BETA_M1.md).
            eprintln!("event=renderer.web.backend_reject detail={error}");
            eprintln!("event=renderer.web.backend_reject exit_code={EXIT_BACKEND_REJECT}");
            exit(EXIT_BACKEND_REJECT);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn spec_160x90() -> FrameSpec {
        FrameSpec::new(160, 90).unwrap()
    }

    #[test]
    fn base64_decodes_standard_vectors_and_bounds_input() {
        assert_eq!(decode_base64("TWFu"), Some(b"Man".to_vec()));
        assert_eq!(decode_base64("TWE="), Some(b"Ma".to_vec()));
        assert_eq!(decode_base64("TQ=="), Some(b"M".to_vec()));
        assert_eq!(decode_base64("AA=="), Some(vec![0]));
        // Rejects: empty, wrong length, invalid characters, misplaced
        // padding, and over-bound inputs.
        assert!(decode_base64("").is_none());
        assert!(decode_base64("TWFuT").is_none());
        assert!(decode_base64("TW!u").is_none());
        assert!(decode_base64("TW=F").is_none()); // padding then data
        assert!(decode_base64("=WFF").is_none()); // leading padding
        assert!(decode_base64("=".repeat(4).as_str()).is_none());
        assert!(decode_base64(&"A".repeat(MAX_JPEG_BASE64_BYTES + 4)).is_none());
    }

    #[test]
    fn base64_round_trips_jpeg_sized_payloads() {
        // The screencast jpegs are a few hundred bytes; round-trip a
        // 3-byte-multiple payload to prove the streaming case.
        let payload: Vec<u8> = (0..=255u8).cycle().take(3 * 257).collect();
        let encoded = base64_encode_test(&payload);
        assert_eq!(decode_base64(&encoded), Some(payload));
    }

    /// Minimal test-only encoder (inverse of `decode_base64`); keeps the
    /// decode unit tests dependency-free.
    fn base64_encode_test(bytes: &[u8]) -> String {
        const ALPHABET: &[u8] = b"ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789+/";
        let mut output = String::new();
        for chunk in bytes.chunks(3) {
            let n = (u32::from(chunk[0]) << 16)
                | (u32::from(chunk.get(1).copied().unwrap_or(0)) << 8)
                | u32::from(chunk.get(2).copied().unwrap_or(0));
            for i in 0..4 {
                if i * 6 < chunk.len() * 8 {
                    output.push(ALPHABET[((n >> (18 - i * 6)) & 0x3f) as usize] as char);
                } else {
                    output.push('=');
                }
            }
        }
        output
    }

    #[test]
    fn scale_and_convert_is_exact_at_spec_size() {
        let spec = spec_160x90();
        let mut rgb = image::RgbImage::new(160, 90);
        for (x, y, pixel) in rgb.enumerate_pixels_mut() {
            *pixel = image::Rgb([x as u8, y as u8, (x + y) as u8]);
        }
        let pixels = scale_and_convert(&rgb, spec);
        assert_eq!(pixels.len(), spec.pixel_bytes());
        // Pixel (3, 4) maps to itself and converts RGB(3,4,7) -> BGRA, alpha
        // 255 (the blue channel of the source is the first output byte).
        let offset = (4 * spec.width as usize + 3) * 4;
        assert_eq!(pixels[offset..offset + 4], [7, 4, 3, 255]);
        // The last pixel is exactly at the buffer end.
        assert_eq!(pixels.len(), 160 * 90 * 4);
    }

    #[test]
    fn scale_and_convert_stretches_mismatched_dimensions() {
        // The compositor can deliver 160x89 for a 160x90 spec (aspect
        // rounding); the fixed-size slot is filled by nearest-neighbor.
        let spec = spec_160x90();
        let mut rgb = image::RgbImage::new(160, 89);
        for (x, y, pixel) in rgb.enumerate_pixels_mut() {
            *pixel = image::Rgb([x as u8, y as u8, (x + y) as u8]);
        }
        let pixels = scale_and_convert(&rgb, spec);
        assert_eq!(pixels.len(), spec.pixel_bytes());
        // Bottom row samples source row floor(89 * 89 / 90) = 88; source
        // pixel (0, 88) = Rgb(0, 88, 88) -> BGRA(88, 88, 0, 255).
        let offset = (89 * spec.width as usize) * 4;
        assert_eq!(pixels[offset..offset + 4], [88, 88, 0, 255]);
        // Upscaling a 80x45 source into 160x90 keeps the first row exact.
        let mut small = image::RgbImage::new(80, 45);
        for (x, _y, pixel) in small.enumerate_pixels_mut() {
            *pixel = image::Rgb([x as u8, 0, 0]);
        }
        let stretched = scale_and_convert(&small, spec);
        assert_eq!(stretched.len(), spec.pixel_bytes());
        assert_eq!(stretched[0], 0); // (0,0) source -> (0,0) target
        assert_eq!(stretched[2], 0);
        assert_eq!(stretched[3], 255);
    }

    #[test]
    fn next_publish_follows_the_pacing_contract() {
        let now = Instant::now();
        let soon = now + Duration::from_millis(10);
        assert_eq!(next_publish(now, soon, true, true), PublishDecision::Wait);
        assert_eq!(next_publish(now, soon, false, false), PublishDecision::Wait);
        assert_eq!(
            next_publish(now, now, true, true),
            PublishDecision::NewFrame
        );
        // At the deadline with no new frame, the last frame is re-published
        // (keepalive) — the supervisor's frame timeout never trips.
        assert_eq!(
            next_publish(now, now, false, true),
            PublishDecision::Keepalive
        );
        // An empty frame is never published: nothing to keep alive yet.
        assert_eq!(next_publish(now, now, false, false), PublishDecision::Wait);
    }

    #[test]
    fn decode_screencast_rejects_caps_and_garbage() {
        let spec = spec_160x90();
        // Over-bound base64 is rejected before decoding.
        assert!(decode_screencast(&"A".repeat(MAX_JPEG_BASE64_BYTES + 4), spec).is_none());
        // Garbage that decodes base64 but is not a jpeg is rejected by the
        // decoder (with the image crate's limits applied).
        let base64 = base64_encode_test(&[0x00; 64]);
        assert!(decode_screencast(&base64, spec).is_none());
        assert!(decode_screencast("!!!!", spec).is_none());
    }

    #[test]
    fn pointer_mapping_and_buttons_follow_the_cdp_contract() {
        // The normalized u16/65535 coordinates scale onto the viewport.
        let message =
            PointerMessage::from_normalized(1, kwe_input_protocol::PointerPhase::Move, 0.5, 0.25)
                .unwrap();
        assert_eq!(message.x, 32_768);
        assert_eq!(message.y, 16_384);
        // Down requires a button; the CDP names map 1:1.
        let down = PointerMessage::button_event(
            2,
            kwe_input_protocol::PointerPhase::Down,
            PointerButton::Secondary,
            0.5,
            0.5,
        )
        .unwrap();
        assert_eq!(down.button, Some(PointerButton::Secondary));
        assert_eq!(cdp_button_name(PointerButton::Primary), "left");
        assert_eq!(cdp_button_name(PointerButton::Secondary), "right");
        assert_eq!(cdp_button_name(PointerButton::Middle), "middle");
        assert_eq!(cdp_button_bit(PointerButton::Primary), 1);
        assert_eq!(cdp_button_bit(PointerButton::Secondary), 2);
        assert_eq!(cdp_button_bit(PointerButton::Middle), 4);
    }

    #[test]
    fn audio_expression_embeds_128_guarded_floats() {
        // The pinned expression embeds one JSON array of 64+64 floats and
        // guards on the page's audio_web before calling it.
        let frame = AudioFrame::new(3, vec![0.25; 64], vec![0.75; 64]).unwrap();
        let mut bands = Vec::with_capacity(128);
        bands.extend_from_slice(&frame.left);
        bands.extend_from_slice(&frame.right);
        let expression = format!(
            "typeof audio_web === \"function\" && audio_web({})",
            serde_json::to_string(&bands).unwrap()
        );
        assert!(expression.starts_with("typeof audio_web === \"function\" && audio_web(["));
        let embedded = expression
            .trim_start_matches("typeof audio_web === \"function\" && audio_web(")
            .strip_suffix(')')
            .unwrap();
        let parsed: Vec<f64> = serde_json::from_str(embedded).unwrap();
        assert_eq!(parsed.len(), 128);
        assert!(parsed[..64].iter().all(|&v| v == 0.25));
        assert!(parsed[64..].iter().all(|&v| v == 0.75));
    }

    #[test]
    fn heartbeat_success_resets_the_streak_and_reschedules() {
        let interval = Duration::from_millis(1000);
        let now = Instant::now();
        let mut tracker = HeartbeatTracker::new(interval, 3, now);
        // The first probe waits one full interval.
        assert!(!tracker.should_probe(now));
        assert!(tracker.should_probe(now + interval));
        tracker.note_sent(7, now + interval);
        assert!(!tracker.should_probe(now + interval)); // in flight
        assert_eq!(
            tracker.resolve(now + interval * 2, HeartbeatProbeResult::Answered(true)),
            HeartbeatOutcome::Continue
        );
        assert_eq!(tracker.consecutive_failures, 0);
        // The next probe is rescheduled a full interval after the resolve.
        assert!(tracker.should_probe(now + interval * 3));
        // Two failures followed by success reset the streak entirely.
        tracker.note_sent(8, now + interval * 3);
        assert_eq!(
            tracker.resolve(now + interval * 4, HeartbeatProbeResult::TimedOut),
            HeartbeatOutcome::Continue
        );
        tracker.note_sent(9, now + interval * 5);
        assert_eq!(
            tracker.resolve(now + interval * 6, HeartbeatProbeResult::TimedOut),
            HeartbeatOutcome::Continue
        );
        tracker.note_sent(10, now + interval * 7);
        assert_eq!(
            tracker.resolve(now + interval * 8, HeartbeatProbeResult::Answered(true)),
            HeartbeatOutcome::Continue
        );
        assert_eq!(tracker.consecutive_failures, 0);
    }

    #[test]
    fn heartbeat_timeouts_count_up_to_the_threshold() {
        let interval = Duration::from_millis(1000);
        let mut tracker = HeartbeatTracker::new(interval, 2, Instant::now());
        let now = Instant::now() + interval; // first probe eligible
        // A response whose deadline has NOT passed yet is still in flight.
        tracker.note_sent(1, now);
        assert_eq!(tracker.pending_probe(), Some((1, now)));
        assert_eq!(
            tracker.resolve(now + interval / 2, HeartbeatProbeResult::Answered(false)),
            HeartbeatOutcome::Continue
        );
        // A protocol-error envelope counts as a failure.
        assert_eq!(tracker.consecutive_failures, 1);
        // Second consecutive timeout crosses the threshold of 2.
        tracker.note_sent(2, now + interval * 2);
        assert_eq!(
            tracker.resolve(now + interval * 3, HeartbeatProbeResult::TimedOut),
            HeartbeatOutcome::Exceeded { consecutive: 2 }
        );
    }

    #[test]
    fn heartbeat_send_failure_is_a_failure() {
        let interval = Duration::from_millis(1000);
        let mut tracker = HeartbeatTracker::new(interval, 2, Instant::now());
        let now = Instant::now() + interval;
        tracker.note_sent(1, now);
        assert_eq!(
            tracker.resolve(now + interval, HeartbeatProbeResult::SendFailed),
            HeartbeatOutcome::Continue
        );
        tracker.note_sent(2, now + interval * 2);
        assert_eq!(
            tracker.resolve(now + interval * 3, HeartbeatProbeResult::SendFailed),
            HeartbeatOutcome::Exceeded { consecutive: 2 }
        );
    }

    #[test]
    fn heartbeat_max_failures_of_one_fails_fast() {
        let interval = Duration::from_millis(1000);
        let mut tracker = HeartbeatTracker::new(interval, 1, Instant::now());
        let now = Instant::now() + interval;
        tracker.note_sent(1, now);
        assert_eq!(
            tracker.resolve(now + interval, HeartbeatProbeResult::TimedOut),
            HeartbeatOutcome::Exceeded { consecutive: 1 }
        );
    }

    #[test]
    fn scale_and_convert_guards_against_zero_dimension_sources() {
        // A hostile zero-dimension source must be skipped, never panicked
        // on (a decode panic would abort the worker via SIGABRT).
        let spec = spec_160x90();
        let empty = image::RgbImage::new(0, 0);
        assert!(scale_and_convert(&empty, spec).is_empty());
        // The exact-size check in the publish path rejects the empty slice.
        assert_ne!(scale_and_convert(&empty, spec).len(), spec.pixel_bytes());
    }
}

#[cfg(test)]
mod page_marker_tests {
    use super::*;

    #[test]
    fn percent_encoded_space_in_the_reported_url_still_matches() {
        let marker = "/Users/me/Library/Application Support/Steam/steamapps/workshop/content/431960/1/index.html";
        let reported = "file:///Users/me/Library/Application%20Support/Steam/steamapps/workshop/content/431960/1/index.html";
        assert!(url_matches_marker(reported, marker));
        assert!(url_matches_marker("file:///wallpaper/index.html", "/wallpaper/index.html"));
        assert!(!url_matches_marker("about:blank", marker));
        assert_eq!(percent_decode("a%2Fb%zz%"), "a/b%zz%");
        // A '%' followed by multibyte text must not panic or corrupt.
        assert_eq!(percent_decode("x%éy%2"), "x%éy%2");
        assert_eq!(percent_decode("%C3%A9"), "é");
    }
}
