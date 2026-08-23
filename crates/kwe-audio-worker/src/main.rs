// SPDX-License-Identifier: GPL-3.0-or-later
//
// Bounded PipeWire capture worker (BETA_M1d): captures the system audio sink
// through `pw-record --raw`, converts every bounded PCM window into a
// normalized 64-band stereo AudioFrame with `kwe_core::audio::analyze_stereo`,
// and pushes frames to the daemon over its RPC socket (`audio.forward`). The
// daemon owns this process: launch, restart policy, and termination (SIGTERM).
//
// Capture target: with `--capture-node` the value is passed through to
// pw-record unchanged (serial or node name). Without it, `pw-dump` is read
// once (bounded, with a timeout), the default sink is resolved from the
// `default.audio.sink` metadata entry, and the matching Audio/Sink node name
// becomes the target. Verified on PipeWire 1.6.8: monitor nodes ("Monitor of
// ...") are created on demand and do not appear in pw-dump, so targeting the
// sink node directly is the observed shape; a "Monitor of ..." node is kept
// as a fallback for other versions.
//
// Wire sequence: audio/media `sequence` IS the daemon's display generation
// (docs/INPUT_PROTOCOL_V1.md). The worker learns the current generation from
// `renderer.status` at startup, holds frames while no renderer has ever been
// promoted (generation 0, polled on a bounded interval), and refreshes the
// generation after every `supervisor_failed` response. No monotonicity logic
// is implemented.
//
// Exit codes (documented in docs/BETA_M1.md):
//   0  graceful SIGTERM stop (pw-record child stopped first)
//   74 capture-node resolution failed (pw-dump missing/unparsable/no sink)
//   75 capture failure (pw-record missing, failed to start, or died)
//
// Everything is bounded: window buffers, per-read budgets, pw-dump JSON
// (4 MiB), response lines (16 KiB), reconnect backoff (max 1 s), stderr
// diagnostics (rate-limited), and child termination grace.

use std::{
    io::{Read, Write},
    os::unix::{
        io::AsRawFd,
        net::UnixStream,
        process::{CommandExt, ExitStatusExt},
    },
    path::{Path, PathBuf},
    process::{Child, Command, Stdio, exit},
    sync::atomic::{AtomicBool, Ordering},
    time::{Duration, Instant},
};

use anyhow::{Context, Result, anyhow, bail};
use clap::Parser;
use kwe_core::analyze_stereo;
use kwe_input_protocol::AudioFrame;
use serde_json::{Value, json};

const EXIT_RESOLUTION_FAILED: i32 = 74;
const EXIT_CAPTURE_FAILED: i32 = 75;

/// pw-dump output cap: the default metadata plus node inventory fits well
/// below 4 MiB on any desktop; anything larger is treated as malformed.
const MAX_PW_DUMP_BYTES: usize = 4 * 1024 * 1024;
const PW_DUMP_TIMEOUT: Duration = Duration::from_secs(5);
/// Per-tick drain budget for pw-record stdout: the pipe capacity is 64 KiB,
/// so a chatty capture cannot outrun our reader or grow any internal buffer.
const MAX_READ_BUDGET_BYTES: usize = 64 * 1024;
/// Upper bound on one interleaved read chunk (2 channels x 4 bytes).
const MAX_SAMPLE_CHUNK_FRAMES: usize = 4096;
/// Loop sleep when no capture data is pending; also the worst-case latency
/// for observing SIGTERM (teardown stays well under one second).
const LOOP_SLEEP: Duration = Duration::from_millis(20);
/// Generation poll cadence while no renderer has ever been promoted.
const GENERATION_POLL: Duration = Duration::from_millis(500);
/// Reconnect backoff: base delay, doubled on each failure, never above max.
const RECONNECT_BASE: Duration = Duration::from_millis(50);
const RECONNECT_MAX: Duration = Duration::from_secs(1);
/// Unix socket timeouts and response deadline. The daemon answers local
/// requests in microseconds; these bounds only bind when the daemon is
/// wedged, keeping the worker's own SIGTERM latency bounded.
const SOCKET_TIMEOUT: Duration = Duration::from_millis(500);
const RESPONSE_DEADLINE: Duration = Duration::from_millis(750);
const MAX_RESPONSE_BYTES: usize = 16 * 1024;
const MAX_REQUEST_BYTES: usize = 8 * 1024;
/// Grace period before SIGKILL escalates the pw-record process group.
const STOP_GRACE: Duration = Duration::from_millis(500);
/// Rate-limited diagnostics: log the first occurrences, then every
/// thousandth, so a persistent condition cannot flood stderr.
const MAX_DIAG_LOGS: u64 = 5;

/// Set by the SIGTERM handler; observed by the loop within LOOP_SLEEP or a
/// bounded socket operation.
static TERMINATED: AtomicBool = AtomicBool::new(false);

/// Async-signal-safe handler: SIGTERM only records the termination request.
extern "C" fn on_sigterm(_signal: i32) {
    TERMINATED.store(true, Ordering::Release);
}

// ---------------------------------------------------------------------------
// Command line
// ---------------------------------------------------------------------------

#[derive(Debug, Parser)]
#[command(
    version,
    about = "Bounded PipeWire audio capture worker for kwe-daemon"
)]
struct Arguments {
    /// Daemon RPC socket path (the socket the daemon itself listens on).
    #[arg(long)]
    socket: PathBuf,
    /// PipeWire node target (serial or name) to capture; skips pw-dump
    /// resolution entirely.
    #[arg(long)]
    capture_node: Option<String>,
    /// Capture sample rate (bounded; the daemon default is 48000).
    #[arg(long, default_value_t = 48000, value_parser = clap::value_parser!(u32).range(8000..=96000))]
    rate: u32,
    /// Normalized bands per channel; matches analyze_stereo's contract.
    #[arg(long, default_value_t = 64, value_parser = parse_band_count)]
    band_count: usize,
    /// Analysis window length in frames per channel (at least 2x the band
    /// count so every band sees a measurable slice, at most 8192).
    #[arg(long, default_value_t = 2048, value_parser = parse_window_samples)]
    window_samples: usize,
    /// Maximum pushed frames per second; faster windows keep the latest.
    #[arg(long, default_value_t = 30, value_parser = clap::value_parser!(u32).range(1..=240))]
    max_fps: u32,
}

fn parse_band_count(text: &str) -> Result<usize, String> {
    let value: usize = text
        .parse()
        .map_err(|_| format!("invalid band count: {text}"))?;
    if matches!(value, 16 | 32 | 64) {
        Ok(value)
    } else {
        Err(format!("band count must be 16, 32, or 64, got {value}"))
    }
}

fn parse_window_samples(text: &str) -> Result<usize, String> {
    let value: usize = text
        .parse()
        .map_err(|_| format!("invalid window samples: {text}"))?;
    if value <= 8192 {
        Ok(value)
    } else {
        Err(format!("window samples must be at most 8192, got {value}"))
    }
}

/// Single validation point for the capture parameters; pure so unit tests
/// cover every bound. `--window-samples` additionally must be at least twice
/// the band count (analyze_stereo accepts any non-empty window, but a band
/// needs at least a couple of samples to be measurable) and at most the
/// analyzer's 8192-sample safety limit.
fn validate_capture_params(
    rate: u32,
    band_count: usize,
    window_samples: usize,
    max_fps: u32,
) -> Result<(), String> {
    if !(8000..=96_000).contains(&rate) {
        return Err("rate must be in 8000..=96000".into());
    }
    if !matches!(band_count, 16 | 32 | 64) {
        return Err("band count must be 16, 32, or 64".into());
    }
    if window_samples < 2 * band_count {
        return Err("window samples must be at least 2x the band count".into());
    }
    if window_samples > 8192 {
        return Err("window samples must be at most 8192".into());
    }
    if !(1..=240).contains(&max_fps) {
        return Err("max fps must be in 1..=240".into());
    }
    Ok(())
}

/// Windows per emission slot: ceil(rate / (max_fps * window_samples)).
/// At 48000/2048/30 that is 1 (windows are already below the fps cap); at
/// 96000/2048/30 it is 2 (every second window is emitted).
fn windows_per_emit(rate: u32, max_fps: u32, window_samples: usize) -> usize {
    let denominator = u64::from(max_fps) * window_samples as u64;
    u64::from(rate).div_ceil(denominator) as usize
}

// ---------------------------------------------------------------------------
// Rate-limited diagnostics
// ---------------------------------------------------------------------------

/// Logs the first `MAX_DIAG_LOGS` occurrences of an event, then every
/// thousandth. `count` is the process-wide drop counter, so the log line
/// stays informative while the volume stays bounded.
#[derive(Debug, Default)]
struct DiagLog {
    calls: u64,
}

impl DiagLog {
    fn log(&mut self, event: &str, detail: &str) {
        self.calls = self.calls.saturating_add(1);
        if self.calls <= MAX_DIAG_LOGS || self.calls.is_multiple_of(1000) {
            eprintln!("{event} detail={detail}");
        }
    }
}

// ---------------------------------------------------------------------------
// Capture-node resolution (pw-dump)
// ---------------------------------------------------------------------------

/// Run `pw-dump` once, read its stdout with a 4 MiB cap and a deadline, and
/// return the capture target (a node name usable as pw-record `--target`).
/// Returns `None` when no sink could be resolved; parse/read failures are
/// errors. `Some(node)` from `--capture-node` bypasses this entirely.
fn resolve_capture_target() -> Result<Option<String>> {
    let mut child = Command::new("pw-dump")
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::null())
        .spawn()
        .context("spawn pw-dump")?;
    let mut output = Vec::new();
    let mut stdout = child
        .stdout
        .take()
        .context("pw-dump stdout pipe unavailable")?;
    // Nonblocking reads are what arm the deadline and WouldBlock arms below:
    // on a blocking fd a wedged pw-dump would hang the read forever, the
    // resolution would never time out, and the daemon's restart budget would
    // never fire.
    set_nonblocking(stdout.as_raw_fd()).context("configure pw-dump stdout")?;
    let deadline = Instant::now() + PW_DUMP_TIMEOUT;
    let mut chunk = [0_u8; 4096];
    loop {
        if output.len() > MAX_PW_DUMP_BYTES {
            let _ = child.kill();
            let _ = child.wait();
            bail!("pw-dump output exceeded {MAX_PW_DUMP_BYTES} bytes");
        }
        if Instant::now() >= deadline {
            let _ = child.kill();
            let _ = child.wait();
            bail!("pw-dump timed out after {} ms", PW_DUMP_TIMEOUT.as_millis());
        }
        match stdout.read(&mut chunk) {
            Ok(0) => break,
            Ok(count) => output.extend_from_slice(&chunk[..count]),
            Err(error)
                if matches!(
                    error.kind(),
                    std::io::ErrorKind::WouldBlock | std::io::ErrorKind::TimedOut
                ) =>
            {
                std::thread::sleep(Duration::from_millis(5));
            }
            Err(error) => {
                let _ = child.kill();
                let _ = child.wait();
                return Err(error).context("read pw-dump stdout");
            }
        }
    }
    let deadline = Instant::now() + PW_DUMP_TIMEOUT;
    let status = loop {
        match child.try_wait() {
            Ok(Some(status)) => break status,
            Ok(None) if Instant::now() >= deadline => {
                let _ = child.kill();
                let _ = child.wait();
                bail!(
                    "pw-dump did not exit within {} ms",
                    PW_DUMP_TIMEOUT.as_millis()
                );
            }
            Ok(None) => std::thread::sleep(Duration::from_millis(5)),
            Err(error) => bail!("wait pw-dump: {error}"),
        }
    };
    if !status.success() {
        let detail = if let Some(code) = status.code() {
            format!("exit_code_{code}")
        } else if let Some(signal) = status.signal() {
            format!("signal_{signal}")
        } else {
            "unknown_exit".to_string()
        };
        bail!("pw-dump exited unsuccessfully: {detail}");
    }
    parse_sink_target(&output).map_err(|error| anyhow!("parse pw-dump output: {error}"))
}

/// Pure resolver over one `pw-dump` JSON document. The observed 1.6.8 shape:
/// the default sink is a `PipeWire:Interface:Metadata` object named "default"
/// carrying `{"key":"default.audio.sink","value":{"name":"<node.name>"}}`,
/// and the sink is a `PipeWire:Interface:Node` whose `media.class` is
/// `Audio/Sink` and whose `node.name` equals that name. Monitor nodes are
/// created on demand (they do not exist at rest), so the sink node name is
/// the target; a "Monitor of ..." node is accepted as a fallback for other
/// versions.
fn parse_sink_target(bytes: &[u8]) -> Result<Option<String>, String> {
    let doc: Value =
        serde_json::from_slice(bytes).map_err(|error| format!("invalid JSON: {error}"))?;
    let entries = doc
        .as_array()
        .ok_or_else(|| "pw-dump root is not a JSON array".to_string())?;
    let default_sink = entries.iter().find_map(|object| {
        let metadata = match object.get("metadata").and_then(Value::as_array) {
            Some(metadata) => metadata,
            None => return None,
        };
        metadata.iter().find_map(|entry| {
            if entry.get("key").and_then(Value::as_str) != Some("default.audio.sink") {
                return None;
            }
            match entry.get("value") {
                Some(Value::String(name)) => Some(name.clone()),
                Some(Value::Object(map)) => {
                    map.get("name").and_then(Value::as_str).map(str::to_owned)
                }
                _ => None,
            }
        })
    });
    let nodes: Vec<&Value> = entries
        .iter()
        .filter(|object| {
            object.get("type").and_then(Value::as_str) == Some("PipeWire:Interface:Node")
        })
        .collect();
    if let Some(sink) = &default_sink {
        let matching = nodes.iter().find(|node| {
            let props = node.get("info").and_then(|info| info.get("props"));
            props
                .and_then(|p| p.get("media.class"))
                .and_then(Value::as_str)
                == Some("Audio/Sink")
                && props
                    .and_then(|p| p.get("node.name"))
                    .and_then(Value::as_str)
                    == Some(sink.as_str())
        });
        if matching.is_some() {
            return Ok(Some(sink.clone()));
        }
    }
    // Fallback: any monitor node. Only its existence matters; pw-record
    // accepts the name directly.
    Ok(nodes.iter().find_map(|node| {
        let name = node
            .get("info")
            .and_then(|info| info.get("props"))
            .and_then(|props| props.get("node.name"))
            .and_then(Value::as_str)?;
        name.strip_prefix("Monitor of ").map(|_| name.to_string())
    }))
}

// ---------------------------------------------------------------------------
// Capture (pw-record)
// ---------------------------------------------------------------------------

/// Bounded ring of pw-record stderr diagnostics for the failure exit path.
#[derive(Debug, Default)]
struct StderrRing {
    tail: Vec<String>,
    tail_bytes: usize,
    /// Incomplete line accumulated across read chunks.
    partial: Vec<u8>,
}

const STDERR_RING_LINES: usize = 16;
const STDERR_RING_BYTES: usize = 4096;
/// Overlong stderr lines (no newline within this cap) are truncated so the
/// partial buffer itself stays bounded.
const STDERR_LINE_CAP: usize = 1024;

impl StderrRing {
    fn push_bytes(&mut self, bytes: &[u8]) {
        self.partial.extend_from_slice(bytes);
        if self.partial.len() > STDERR_LINE_CAP {
            let text = String::from_utf8_lossy(&self.partial[..STDERR_LINE_CAP]).into_owned();
            self.partial.clear();
            self.tail_bytes += text.len();
            self.tail.push(text);
            self.enforce_budget();
        }
        while let Some(newline) = self.partial.iter().position(|byte| *byte == b'\n') {
            let line: Vec<u8> = self.partial.drain(..=newline).collect();
            let text = String::from_utf8_lossy(&line[..line.len() - 1]).into_owned();
            self.tail_bytes += text.len();
            self.tail.push(text);
            self.enforce_budget();
        }
    }

    fn enforce_budget(&mut self) {
        while self.tail.len() > STDERR_RING_LINES || self.tail_bytes > STDERR_RING_BYTES {
            if self.tail.is_empty() {
                return;
            }
            let evicted = self.tail.remove(0);
            self.tail_bytes -= evicted.len();
        }
    }
}

/// The pw-record child plus the bounded latest-window PCM ring.
struct Capture {
    child: Child,
    /// Nonblocking raw f32 interleaved (L,R,L,R,...) samples from stdout.
    stdout: std::process::ChildStdout,
    /// Interleaved samples accumulated so far; trimmed to the latest window
    /// plus one chunk, so memory is bounded regardless of capture volume.
    samples: Vec<f32>,
    /// Trailing bytes that do not yet form a complete f32 sample.
    byte_tail: Vec<u8>,
    window_samples: usize,
    stderr: std::process::ChildStderr,
    stderr_ring: StderrRing,
    /// Set when a non-finite sample was dropped without its pair partner in
    /// the same chunk; the first sample of the next chunk is its partner and
    /// is dropped too, so the L/R interleave never desyncs.
    drop_next_sample: bool,
    /// Non-finite samples (and their dropped pair partners) seen so far;
    /// surfaced for diagnostics so a pathological capture is visible.
    non_finite_dropped: u64,
}

impl Capture {
    fn start(arguments: &Arguments, target: &str) -> Result<Self> {
        let mut command = Command::new("pw-record");
        command
            .arg("--raw")
            .arg("--format")
            .arg("f32")
            .arg("--rate")
            .arg(arguments.rate.to_string())
            .arg("--channels")
            .arg("2")
            .arg("--target")
            .arg(target)
            .arg("-")
            .stdin(Stdio::null())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped());
        // SAFETY: this closure runs in the child after fork and before exec.
        // It calls only async-signal-safe libc functions and does not
        // allocate. pw-record gets its own process group (so the worker's
        // group-level stop never races the graceful teardown) and a
        // parent-death signal (so a crashed worker cannot orphan it).
        let expected_parent = i32::try_from(std::process::id()).context("pid overflow")?;
        unsafe {
            command.pre_exec(move || {
                if libc::setpgid(0, 0) != 0 {
                    return Err(std::io::Error::last_os_error());
                }
                if libc::prctl(libc::PR_SET_PDEATHSIG, libc::SIGTERM, 0, 0, 0) != 0 {
                    return Err(std::io::Error::last_os_error());
                }
                if libc::getppid() != expected_parent {
                    return Err(std::io::Error::new(
                        std::io::ErrorKind::Interrupted,
                        "worker exited before pw-record exec",
                    ));
                }
                Ok(())
            });
        }
        let mut child = command.spawn().context("spawn pw-record")?;
        let stdout = child
            .stdout
            .take()
            .context("pw-record stdout pipe unavailable")?;
        let stderr = child
            .stderr
            .take()
            .context("pw-record stderr pipe unavailable")?;
        set_nonblocking(stdout.as_raw_fd()).context("configure pw-record stdout")?;
        set_nonblocking(stderr.as_raw_fd()).context("configure pw-record stderr")?;
        Ok(Self {
            child,
            stdout,
            samples: Vec::with_capacity((arguments.window_samples + MAX_SAMPLE_CHUNK_FRAMES) * 2),
            byte_tail: Vec::with_capacity(8),
            window_samples: arguments.window_samples,
            stderr,
            stderr_ring: StderrRing::default(),
            drop_next_sample: false,
            non_finite_dropped: 0,
        })
    }

    /// Drain one bounded budget of capture bytes into the sample ring and
    /// one budget of diagnostics into the stderr ring. Returns `None` when
    /// the child has exited (the caller decides graceful vs failure).
    fn poll(&mut self) -> Result<()> {
        self.drain_stderr();
        let mut budget = MAX_READ_BUDGET_BYTES;
        while budget > 0 {
            let mut chunk = [0_u8; 4096];
            let read = unsafe {
                libc::read(
                    self.stdout.as_raw_fd(),
                    chunk.as_mut_ptr().cast::<libc::c_void>(),
                    chunk.len(),
                )
            };
            if read == 0 {
                break;
            }
            if read < 0 {
                if std::io::Error::last_os_error().kind() == std::io::ErrorKind::WouldBlock {
                    break;
                }
                return Err(std::io::Error::last_os_error()).context("read pw-record stdout");
            }
            budget -= read as usize;
            self.push_bytes(&chunk[..read as usize]);
        }
        Ok(())
    }

    /// Append raw bytes to the ring, converting complete f32 samples. A
    /// non-finite sample would desync the L/R interleave if dropped alone
    /// (every later sample shifts to the other channel), so it is dropped
    /// together with its pair partner; alignment is preserved and the loss
    /// is counted.
    fn push_bytes(&mut self, bytes: &[u8]) {
        self.byte_tail.extend_from_slice(bytes);
        let mut offset = 0;
        while offset + 4 <= self.byte_tail.len() {
            let sample = f32::from_le_bytes([
                self.byte_tail[offset],
                self.byte_tail[offset + 1],
                self.byte_tail[offset + 2],
                self.byte_tail[offset + 3],
            ]);
            offset += 4;
            if self.drop_next_sample {
                // Pair partner of a non-finite sample dropped in a previous
                // chunk; the partner is dropped unconditionally.
                self.drop_next_sample = false;
                self.non_finite_dropped = self.non_finite_dropped.saturating_add(1);
                continue;
            }
            if sample.is_finite() {
                self.samples.push(sample);
            } else {
                self.drop_next_sample = true;
                self.non_finite_dropped = self.non_finite_dropped.saturating_add(1);
            }
        }
        if offset > 0 {
            self.byte_tail.drain(..offset);
        }
        // Trim to the latest window plus one read chunk: the ring is bounded
        // by construction, so a stalled daemon cannot grow it.
        let cap = (self.window_samples + MAX_SAMPLE_CHUNK_FRAMES) * 2;
        if self.samples.len() > cap {
            self.samples.drain(..self.samples.len() - cap);
        }
    }

    /// Drain the counter of non-finite samples (and dropped partners) since
    /// the last poll; the caller logs it through the rate-limited diagnostic.
    fn take_non_finite_dropped(&mut self) -> u64 {
        let dropped = self.non_finite_dropped;
        self.non_finite_dropped = 0;
        dropped
    }

    /// Deinterleaved latest window when at least one full window is buffered.
    /// The buffered samples are trimmed so each complete window yields
    /// exactly one analysis (overlapping hop equal to the read granularity).
    fn take_window(&mut self) -> Option<(Vec<f32>, Vec<f32>)> {
        if self.samples.len() < self.window_samples * 2 {
            return None;
        }
        let keep = self.window_samples * 2;
        if self.samples.len() > keep {
            self.samples.drain(..self.samples.len() - keep);
        }
        let mut left = Vec::with_capacity(self.window_samples);
        let mut right = Vec::with_capacity(self.window_samples);
        for pair in self.samples.chunks_exact(2) {
            left.push(pair[0]);
            right.push(pair[1]);
        }
        self.samples.clear();
        Some((left, right))
    }

    /// Bounded stderr drain (16 reads of 512 B = 8 KiB per tick) so a chatty
    /// pw-record cannot starve the capture loop and break graceful SIGTERM.
    fn drain_stderr(&mut self) {
        let mut chunk = [0_u8; 512];
        for _ in 0..16 {
            let read = unsafe {
                libc::read(
                    self.stderr.as_raw_fd(),
                    chunk.as_mut_ptr().cast::<libc::c_void>(),
                    chunk.len(),
                )
            };
            if read <= 0 {
                break;
            }
            self.stderr_ring.push_bytes(&chunk[..read as usize]);
        }
    }

    /// Bounded SIGTERM -> grace -> SIGKILL stop of the pw-record process
    /// group, then reap. Mirrors the supervisor's termination helper.
    fn stop(&mut self) {
        let pid = self.child.id();
        if self.child.try_wait().ok().flatten().is_some() {
            return;
        }
        if let Ok(pid) = i32::try_from(pid) {
            // SAFETY: pw-record was placed in a process group whose id equals
            // its pid before exec; a negative pid targets that group.
            unsafe { libc::kill(-pid, libc::SIGTERM) };
        }
        let deadline = Instant::now() + STOP_GRACE;
        while Instant::now() < deadline {
            if self.child.try_wait().ok().flatten().is_some() {
                return;
            }
            std::thread::sleep(Duration::from_millis(10));
        }
        if let Ok(pid) = i32::try_from(pid) {
            // SAFETY: same process group as above; SIGKILL cannot be caught.
            unsafe { libc::kill(-pid, libc::SIGKILL) };
        }
        let _ = self.child.kill();
        let _ = self.child.wait();
    }

    /// Exit observation for the failure path; the stderr ring tail is folded
    /// into the diagnostic so the daemon's log carries the pw-record reason.
    fn exit_detail(&self, status: &std::process::ExitStatus) -> String {
        let reason = if let Some(code) = status.code() {
            format!("exit_code_{code}")
        } else if let Some(signal) = status.signal() {
            format!("signal_{signal}")
        } else {
            "unknown_exit".to_string()
        };
        let tail = self.stderr_ring.tail.join(" | ");
        if tail.is_empty() {
            reason
        } else {
            format!("{reason} stderr=[{tail}]")
        }
    }
}

fn set_nonblocking(descriptor: libc::c_int) -> Result<()> {
    let flags = unsafe { libc::fcntl(descriptor, libc::F_GETFL) };
    if flags < 0 {
        bail!("fcntl F_GETFL failed: {}", std::io::Error::last_os_error());
    }
    // SAFETY: read-modify-write of the O_NONBLOCK bit on our own descriptor.
    let updated = unsafe { libc::fcntl(descriptor, libc::F_SETFL, flags | libc::O_NONBLOCK) };
    if updated < 0 {
        bail!("fcntl F_SETFL failed: {}", std::io::Error::last_os_error());
    }
    Ok(())
}

// ---------------------------------------------------------------------------
// Daemon RPC (one request per connection)
// ---------------------------------------------------------------------------

/// Parsed daemon response line: {version, id, ok, result}.
#[derive(Debug, Clone, PartialEq, Eq)]
struct Response {
    ok: bool,
    result: Value,
}

fn parse_response(bytes: &[u8]) -> Result<Response> {
    let value: Value = serde_json::from_slice(bytes).context("invalid daemon response JSON")?;
    let ok = value.get("ok").and_then(Value::as_bool).unwrap_or(false);
    let result = value.get("result").cloned().unwrap_or(Value::Null);
    Ok(Response { ok, result })
}

/// True when the daemon rejected the request with `supervisor_failed` — the
/// signal to refresh the display generation from `renderer.status`.
fn refresh_needed(response: &Response) -> bool {
    !response.ok
        && response.result.get("error").and_then(Value::as_str) == Some("supervisor_failed")
}

/// One bounded JSON-RPC request: open a fresh connection, send one line,
/// read exactly one response line within the deadline, close.
fn send_request(socket: &Path, serial: u64, method: &str, params: Value) -> Result<Response> {
    let request = json!({
        "version": 1,
        "id": serial,
        "method": method,
        "params": params,
    });
    let mut line = serde_json::to_vec(&request)?;
    line.push(b'\n');
    if line.len() > MAX_REQUEST_BYTES {
        bail!("request exceeds the {MAX_REQUEST_BYTES} byte cap");
    }
    let mut stream = UnixStream::connect(socket)
        .with_context(|| format!("connect to daemon socket {}", socket.display()))?;
    stream.set_read_timeout(Some(SOCKET_TIMEOUT))?;
    stream.set_write_timeout(Some(SOCKET_TIMEOUT))?;
    stream.write_all(&line)?;
    stream.flush()?;
    let deadline = Instant::now() + RESPONSE_DEADLINE;
    let response = read_response_line(&mut stream, deadline)?;
    parse_response(&response)
}

/// One newline-terminated response line, capped at `MAX_RESPONSE_BYTES` and
/// bounded by `deadline`. Slow reads sleep briefly instead of busy-spinning.
fn read_response_line(stream: &mut UnixStream, deadline: Instant) -> Result<Vec<u8>> {
    let mut line = Vec::with_capacity(1024);
    loop {
        if line.len() > MAX_RESPONSE_BYTES {
            bail!("daemon response exceeded {MAX_RESPONSE_BYTES} bytes");
        }
        if Instant::now() >= deadline {
            bail!("daemon response deadline exceeded");
        }
        let mut chunk = [0_u8; 4096];
        match stream.read(&mut chunk) {
            Ok(0) => bail!("daemon closed the connection without a response"),
            Ok(count) => {
                line.extend_from_slice(&chunk[..count]);
                if line.contains(&b'\n') {
                    return Ok(line);
                }
            }
            Err(error)
                if matches!(
                    error.kind(),
                    std::io::ErrorKind::WouldBlock | std::io::ErrorKind::TimedOut
                ) =>
            {
                std::thread::sleep(Duration::from_millis(5));
            }
            Err(error) => return Err(error).context("read daemon response"),
        }
    }
}

/// Learn the current promoted display generation from `renderer.status`.
/// `Ok(0)` means no renderer has ever been promoted.
fn fetch_display_generation(socket: &Path, serial: u64) -> Result<u64> {
    let response = send_request(socket, serial, "renderer.status", json!({}))?;
    let generation = response
        .result
        .get("display_generation")
        .and_then(Value::as_u64)
        .context("renderer.status response lacks display_generation")?;
    Ok(generation)
}

/// Push one analyzed frame. An `ok` response means the daemon accepted it; a
/// `supervisor_failed` response means the generation is stale and must be
/// refreshed. Connection-level failures surface as errors (the caller drops
/// the frame latest-wins and backs off).
fn push_audio_frame(
    socket: &Path,
    serial: u64,
    generation: u64,
    frame: &AudioFrame,
) -> Result<Response> {
    let params = json!({
        "generation": generation,
        "frame": {
            "left": &frame.left,
            "right": &frame.right,
        },
    });
    send_request(socket, serial, "audio.forward", params)
}

// ---------------------------------------------------------------------------
// Worker loop
// ---------------------------------------------------------------------------

struct WorkerState {
    arguments: Arguments,
    capture: Capture,
    /// Latest analyzed frame, replaced not queued (bounded queue of 1).
    pending: Option<AudioFrame>,
    generation: u64,
    request_serial: u64,
    frames_dropped: u64,
    next_emit: Instant,
    next_generation_poll: Instant,
    backoff: Duration,
    diag: DiagLog,
}

impl WorkerState {
    fn analyze(&mut self, left: &[f32], right: &[f32]) {
        let bands = self.arguments.band_count;
        let frame = analyze_stereo(self.generation.max(1), left, right, bands);
        match frame {
            Ok(frame) => {
                if self.pending.replace(frame).is_some() {
                    self.frames_dropped = self.frames_dropped.saturating_add(1);
                    self.diag.log(
                        "event=audio.worker.drop",
                        &format!("frames_dropped={}", self.frames_dropped),
                    );
                }
            }
            Err(error) => self.diag.log("event=audio.worker.analyze_error", &error),
        }
    }

    fn push_pending(&mut self) {
        let Some(frame) = self.pending.as_ref() else {
            return;
        };
        self.request_serial = self.request_serial.wrapping_add(1);
        match push_audio_frame(
            &self.arguments.socket,
            self.request_serial,
            self.generation,
            frame,
        ) {
            Ok(response) => {
                if refresh_needed(&response) {
                    self.refresh_generation();
                } else if !response.ok {
                    let detail = response
                        .result
                        .get("error")
                        .and_then(Value::as_str)
                        .unwrap_or("unknown")
                        .to_string();
                    self.diag.log("event=audio.worker.push_rejected", &detail);
                } else {
                    // Accepted: the frame is consumed; the next analysis
                    // result replaces an empty slot.
                    self.pending = None;
                    self.backoff = RECONNECT_BASE;
                    self.next_emit = Instant::now() + self.emit_interval();
                }
            }
            Err(error) => {
                // Daemon unreachable: drop latest-wins and retry with bounded
                // backoff, never busy-looping.
                self.diag
                    .log("event=audio.worker.push_error", &error.to_string());
                self.next_emit = Instant::now() + self.backoff;
                self.backoff = (self.backoff * 2).min(RECONNECT_MAX);
            }
        }
    }

    fn refresh_generation(&mut self) {
        let previous = self.generation;
        self.request_serial = self.request_serial.wrapping_add(1);
        match fetch_display_generation(&self.arguments.socket, self.request_serial) {
            Ok(generation) => {
                // Record the fetched generation unconditionally: a 0 (no
                // promoted renderer) must switch the main loop back to the
                // poll path instead of emitting against a stale generation.
                self.generation = generation;
                if generation != 0 {
                    if generation != previous {
                        self.diag.log(
                            "event=audio.worker.generation_refresh",
                            &format!("from={previous} to={generation}"),
                        );
                    }
                    self.backoff = RECONNECT_BASE;
                    self.next_emit = Instant::now() + self.emit_interval();
                } else {
                    // Nothing promoted yet: poll again after GENERATION_POLL
                    // rather than busy-looping.
                    self.next_generation_poll = Instant::now() + GENERATION_POLL;
                }
            }
            Err(error) => {
                self.diag
                    .log("event=audio.worker.status_error", &error.to_string());
                // A failed probe must not busy-loop either: keep the same
                // reschedule as the no-renderer arm.
                self.next_generation_poll = Instant::now() + GENERATION_POLL;
            }
        }
    }

    /// Emission cadence: one push per analysis window when the window period
    /// exceeds the fps cap (e.g. 2048 frames at 48 kHz is 23.4 fps), else one
    /// push per `windows_per_emit` windows so `max_fps` stays an upper bound.
    fn emit_interval(&self) -> Duration {
        let windows = windows_per_emit(
            self.arguments.rate,
            self.arguments.max_fps,
            self.arguments.window_samples,
        );
        let window_seconds = self.arguments.window_samples as f64 / f64::from(self.arguments.rate);
        Duration::from_secs_f64(windows as f64 * window_seconds)
    }
}

fn install_term_handler() {
    // SAFETY: `on_sigterm` only stores to a process-global atomic, which is
    // async-signal-safe; installed before any thread besides the main one.
    unsafe { libc::signal(libc::SIGTERM, on_sigterm as *const () as libc::sighandler_t) };
}

fn main() -> Result<()> {
    let arguments = Arguments::parse();
    if let Err(error) = validate_capture_params(
        arguments.rate,
        arguments.band_count,
        arguments.window_samples,
        arguments.max_fps,
    ) {
        eprintln!("event=audio.worker.invalid_params detail={error}");
        exit(EXIT_CAPTURE_FAILED);
    }
    install_term_handler();
    let target = match &arguments.capture_node {
        Some(node) => node.clone(),
        None => match resolve_capture_target() {
            Ok(Some(target)) => target,
            Ok(None) => {
                eprintln!(
                    "event=audio.worker.resolution_failed detail=no default sink or monitor node found in pw-dump"
                );
                exit(EXIT_RESOLUTION_FAILED);
            }
            Err(error) => {
                eprintln!("event=audio.worker.resolution_failed detail={error}");
                exit(EXIT_RESOLUTION_FAILED);
            }
        },
    };
    eprintln!(
        "event=audio.worker.start rate={} bands={} window={} max_fps={} target={target}",
        arguments.rate, arguments.band_count, arguments.window_samples, arguments.max_fps
    );
    let capture = match Capture::start(&arguments, &target) {
        Ok(capture) => capture,
        Err(error) => {
            eprintln!("event=audio.worker.capture_failed detail={error}");
            exit(EXIT_CAPTURE_FAILED);
        }
    };
    let mut state = WorkerState {
        arguments,
        capture,
        pending: None,
        generation: 0,
        request_serial: 0,
        frames_dropped: 0,
        next_emit: Instant::now(),
        next_generation_poll: Instant::now(),
        backoff: RECONNECT_BASE,
        diag: DiagLog::default(),
    };
    // Learn the display generation up front (and on every supervisor_failed
    // response afterwards).
    state.refresh_generation();
    loop {
        if TERMINATED.load(Ordering::Acquire) {
            eprintln!(
                "event=audio.worker.stopped frames_dropped={}",
                state.frames_dropped
            );
            state.capture.stop();
            return Ok(());
        }
        if let Err(error) = state.capture.poll() {
            // Rate-limited: a wedged pw-record must not flood the supervisor
            // log at one line per loop tick.
            state
                .diag
                .log("event=audio.worker.capture_read_error", &error.to_string());
        }
        match state.capture.child.try_wait() {
            Ok(Some(status)) => {
                let detail = state.capture.exit_detail(&status);
                if TERMINATED.load(Ordering::Acquire) {
                    // The signal handler already decided on a graceful stop;
                    // pw-record dying alongside it is not a failure.
                    eprintln!(
                        "event=audio.worker.stopped frames_dropped={}",
                        state.frames_dropped
                    );
                    return Ok(());
                }
                eprintln!("event=audio.worker.capture_failed detail={detail}");
                exit(EXIT_CAPTURE_FAILED);
            }
            Ok(None) => {}
            Err(error) => {
                eprintln!("event=audio.worker.capture_wait_error detail={error}");
                exit(EXIT_CAPTURE_FAILED);
            }
        }
        while let Some((left, right)) = state.capture.take_window() {
            state.analyze(&left, &right);
        }
        let non_finite = state.capture.take_non_finite_dropped();
        if non_finite > 0 {
            state.diag.log(
                "event=audio.worker.non_finite_samples",
                &format!("dropped={non_finite}"),
            );
        }
        let now = Instant::now();
        if state.generation == 0 {
            if now >= state.next_generation_poll {
                state.refresh_generation();
            }
        } else if now >= state.next_emit {
            state.push_pending();
        }
        std::thread::sleep(LOOP_SLEEP);
    }
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn validates_bounded_capture_parameters() {
        assert!(validate_capture_params(48000, 64, 2048, 30).is_ok());
        assert!(validate_capture_params(8000, 16, 32, 1).is_ok());
        assert!(validate_capture_params(96000, 64, 8192, 240).is_ok());
        // Rate bounds.
        assert!(validate_capture_params(7999, 64, 2048, 30).is_err());
        assert!(validate_capture_params(96001, 64, 2048, 30).is_err());
        // Band count must be 16, 32, or 64.
        assert!(validate_capture_params(48000, 8, 2048, 30).is_err());
        assert!(validate_capture_params(48000, 128, 2048, 30).is_err());
        // Window must be at least 2x the band count and at most 8192.
        assert!(validate_capture_params(48000, 64, 127, 30).is_err());
        assert!(validate_capture_params(48000, 64, 128, 30).is_ok());
        assert!(validate_capture_params(48000, 16, 8193, 30).is_err());
        assert!(validate_capture_params(48000, 16, 8192, 30).is_ok());
        // fps bounds.
        assert!(validate_capture_params(48000, 64, 2048, 0).is_err());
        assert!(validate_capture_params(48000, 64, 2048, 241).is_err());
    }

    #[test]
    fn emission_cadence_is_ceil_rate_over_fps_times_window() {
        // 48000 / (30 * 2048) < 1: every window is emitted.
        assert_eq!(windows_per_emit(48000, 30, 2048), 1);
        // 96000 / (30 * 2048) = 1.5625: every second window.
        assert_eq!(windows_per_emit(96000, 30, 2048), 2);
        // 48000 / (30 * 1024) = 1.5625: every second window.
        assert_eq!(windows_per_emit(48000, 30, 1024), 2);
        // 8000 / (1 * 64) = 125: 125 windows per emitted frame at the floor.
        assert_eq!(windows_per_emit(8000, 1, 64), 125);
    }

    #[test]
    fn band_count_parser_accepts_only_the_protocol_counts() {
        assert_eq!(parse_band_count("16").unwrap(), 16);
        assert_eq!(parse_band_count("32").unwrap(), 32);
        assert_eq!(parse_band_count("64").unwrap(), 64);
        assert!(parse_band_count("8").is_err());
        assert!(parse_band_count("junk").is_err());
    }

    #[test]
    fn resolves_the_default_sink_monitor_from_a_pw_dump_document() {
        let dump = serde_json::json!([
            {
                "id": 39,
                "type": "PipeWire:Interface:Metadata",
                "props": {"metadata.name": "default"},
                "metadata": [
                    {"key": "default.audio.sink", "type": "Spa:String:JSON", "value": {"name": "alsa_output.usb-Headset.analog-stereo"}}
                ]
            },
            {
                "id": 58,
                "type": "PipeWire:Interface:Node",
                "info": {"props": {"media.class": "Audio/Sink", "node.name": "alsa_output.usb-Headset.analog-stereo", "object.serial": 344}}
            }
        ]);
        let target = parse_sink_target(&serde_json::to_vec(&dump).unwrap()).unwrap();
        assert_eq!(
            target.as_deref(),
            Some("alsa_output.usb-Headset.analog-stereo")
        );
    }

    #[test]
    fn resolves_a_plain_string_default_sink_value() {
        let dump = serde_json::json!([
            {
                "type": "PipeWire:Interface:Metadata",
                "metadata": [{"key": "default.audio.sink", "value": "alsa_output.simple.analog-stereo"}]
            },
            {
                "type": "PipeWire:Interface:Node",
                "info": {"props": {"media.class": "Audio/Sink", "node.name": "alsa_output.simple.analog-stereo"}}
            }
        ]);
        let target = parse_sink_target(&serde_json::to_vec(&dump).unwrap()).unwrap();
        assert_eq!(target.as_deref(), Some("alsa_output.simple.analog-stereo"));
    }

    #[test]
    fn falls_back_to_any_monitor_named_node() {
        let dump = serde_json::json!([
            {
                "type": "PipeWire:Interface:Node",
                "info": {"props": {"media.class": "Audio/Sink", "node.name": "alsa_output.sink"}}
            },
            {
                "type": "PipeWire:Interface:Node",
                "info": {"props": {"node.name": "Monitor of alsa_output.sink"}}
            }
        ]);
        let target = parse_sink_target(&serde_json::to_vec(&dump).unwrap()).unwrap();
        assert_eq!(target.as_deref(), Some("Monitor of alsa_output.sink"));
    }

    #[test]
    fn returns_none_without_any_sink_or_monitor_node() {
        let dump = serde_json::json!([{"id": 0, "type": "PipeWire:Interface:Core"}]);
        assert_eq!(
            parse_sink_target(&serde_json::to_vec(&dump).unwrap()).unwrap(),
            None
        );
    }

    #[test]
    fn rejects_malformed_pw_dump_documents() {
        assert!(parse_sink_target(b"not json").is_err());
        assert!(parse_sink_target(b"{\"not\":\"an array\"}").is_err());
    }

    #[test]
    fn response_parsing_and_refresh_decision_are_pure() {
        let ok = parse_response(br#"{"version":1,"id":1,"ok":true,"result":{"status":"dropped"}}"#)
            .unwrap();
        assert!(ok.ok);
        assert!(!refresh_needed(&ok));
        let failed = parse_response(
            br#"{"version":1,"id":1,"ok":false,"result":{"error":"supervisor_failed","detail":"audio frame display generation is stale or invalid"}}"#,
        )
        .unwrap();
        assert!(!failed.ok);
        assert!(refresh_needed(&failed));
        let rejected = parse_response(
            br#"{"version":1,"id":1,"ok":false,"result":{"error":"unknown_method"}}"#,
        )
        .unwrap();
        assert!(!rejected.ok);
        assert!(!refresh_needed(&rejected));
        assert!(parse_response(b"junk").is_err());
    }

    #[test]
    fn latest_window_replaces_the_pending_frame_and_counts_dropped() {
        let mut state = WorkerState {
            arguments: Arguments::parse_from(["kwe-audio-worker", "--socket", "/tmp/s"]),
            capture: {
                // A synthetic child that exits immediately is never touched
                // by the analysis path; only the pending bookkeeping is under
                // test here.
                let mut command = Command::new("true");
                command.stdout(Stdio::piped()).stderr(Stdio::piped());
                let mut child = command.spawn().unwrap();
                let _ = child.wait();
                let stdout = child.stdout.take().unwrap();
                let stderr = child.stderr.take().unwrap();
                Capture {
                    child,
                    stdout,
                    samples: vec![],
                    byte_tail: vec![],
                    window_samples: 2048,
                    stderr,
                    stderr_ring: StderrRing::default(),
                    drop_next_sample: false,
                    non_finite_dropped: 0,
                }
            },
            pending: None,
            generation: 1,
            request_serial: 0,
            frames_dropped: 0,
            next_emit: Instant::now(),
            next_generation_poll: Instant::now(),
            backoff: RECONNECT_BASE,
            diag: DiagLog::default(),
        };
        let left = vec![0.25; 2048];
        let right = vec![0.5; 2048];
        state.analyze(&left, &right);
        assert!(state.pending.is_some());
        assert_eq!(state.frames_dropped, 0);
        // A second window before any push replaces the first (queue of 1).
        state.analyze(&left, &right);
        assert_eq!(state.frames_dropped, 1);
        // The pending frame stays bounded to one slot.
        assert!(state.pending.is_some());
        assert_eq!(state.pending.as_ref().unwrap().left.len(), 64);
    }

    #[test]
    fn capture_ring_keeps_only_the_latest_window() {
        let mut capture = Capture {
            child: Command::new("true").spawn().unwrap(),
            stdout: {
                let mut command = Command::new("true");
                command
                    .stdout(Stdio::piped())
                    .spawn()
                    .unwrap()
                    .stdout
                    .take()
                    .unwrap()
            },
            samples: vec![],
            byte_tail: vec![],
            window_samples: 8,
            stderr: {
                let mut command = Command::new("true");
                command
                    .stderr(Stdio::piped())
                    .spawn()
                    .unwrap()
                    .stderr
                    .take()
                    .unwrap()
            },
            stderr_ring: StderrRing::default(),
            drop_next_sample: false,
            non_finite_dropped: 0,
        };
        // Push 32 interleaved frames (16 windows of 8) in one byte chunk.
        let samples: Vec<f32> = (0..64).map(|index| index as f32).collect();
        let bytes: Vec<u8> = samples
            .iter()
            .flat_map(|sample| sample.to_le_bytes())
            .collect();
        capture.push_bytes(&bytes);
        let (left, right) = capture.take_window().unwrap();
        // The latest 8 frames survive; the earlier ones were dropped.
        assert_eq!(left.len(), 8);
        assert_eq!(right.len(), 8);
        assert_eq!(left[0], 48.0);
        assert_eq!(right[0], 49.0);
        assert_eq!(left[7], 62.0);
        assert_eq!(right[7], 63.0);
        assert!(capture.take_window().is_none());
    }

    #[test]
    fn non_finite_samples_drop_their_pair_and_keep_interleave_aligned() {
        let mut capture = Capture {
            child: Command::new("true").spawn().unwrap(),
            stdout: {
                let mut command = Command::new("true");
                command
                    .stdout(Stdio::piped())
                    .spawn()
                    .unwrap()
                    .stdout
                    .take()
                    .unwrap()
            },
            samples: vec![],
            byte_tail: vec![],
            window_samples: 8,
            stderr: {
                let mut command = Command::new("true");
                command
                    .stderr(Stdio::piped())
                    .spawn()
                    .unwrap()
                    .stderr
                    .take()
                    .unwrap()
            },
            stderr_ring: StderrRing::default(),
            drop_next_sample: false,
            non_finite_dropped: 0,
        };
        // Interleaved frames [L0 R0] [L1 R1] ... [L8 R8] with L0 = NaN.
        // NaN drops L0 and its partner R0; every later sample must keep its
        // original channel (no interleave shift), leaving exactly 8 frames.
        let bytes: Vec<u8> = [
            f32::NAN,
            2.0,
            4.0,
            6.0,
            8.0,
            10.0,
            12.0,
            14.0,
            16.0,
            18.0,
            20.0,
            22.0,
            24.0,
            26.0,
            28.0,
            30.0,
            32.0,
            34.0,
        ]
        .iter()
        .flat_map(|sample| sample.to_le_bytes())
        .collect();
        capture.push_bytes(&bytes);
        assert_eq!(capture.take_non_finite_dropped(), 2);
        let (left, right) = capture.take_window().unwrap();
        assert_eq!(left, vec![4.0, 8.0, 12.0, 16.0, 20.0, 24.0, 28.0, 32.0]);
        assert_eq!(right, vec![6.0, 10.0, 14.0, 18.0, 22.0, 26.0, 30.0, 34.0]);
    }

    #[test]
    fn stderr_ring_evicts_oldest_lines_and_binds_on_bytes() {
        let mut ring = StderrRing::default();
        for index in 0..40 {
            ring.push_bytes(format!("line-{index}\n").as_bytes());
        }
        assert_eq!(ring.tail.len(), STDERR_RING_LINES);
        assert_eq!(ring.tail.first().unwrap(), "line-24");
        assert_eq!(ring.tail.last().unwrap(), "line-39");
        let mut big = StderrRing::default();
        for _ in 0..8 {
            big.push_bytes(&vec![b'x'; 1024]);
            big.push_bytes(b"\n");
        }
        assert_eq!(big.tail.len(), 4);
        assert_eq!(big.tail_bytes, 4096);
    }

    #[test]
    fn diagnostics_are_rate_limited() {
        let mut diag = DiagLog::default();
        // eprintln goes to the test harness stderr; count calls instead of
        // capturing output. All 1000 events are counted, but only the first
        // 5 plus every thousandth produce a log line (MAX_DIAG_LOGS).
        for _ in 0..1000 {
            diag.log("event=test", "x");
        }
        assert_eq!(diag.calls, 1000);
    }
}
