// SPDX-License-Identifier: GPL-3.0-or-later
//! BETA_M2a spike: drive an installed Chromium over `--remote-debugging-pipe`
//! and pin the screencast contract the M2b renderer will rely on.
//!
//! Spawns chromium fresh with the CDP pipe on fds 3/4 (the client owns the
//! opposite ends of two socketpairs), then getTargets -> attachToTarget
//! (flatten) -> Page.enable -> startScreencast(160x90 jpeg q80
//! everyNthFrame:1), twice, on fresh instances:
//!
//! - Phase A: no acks ever — counts how many frames arrive before the hard
//!   stall (pinned: kMaxScreencastFramesInFlight=2 -> exactly 3 frames).
//! - Phase B: ack every frame — collects >= min_frames (first-frame latency,
//!   jpeg sizes, cadence), then stops acking and counts the additional
//!   frames before silence.
//!
//! Prints one JSON summary; exits non-zero on any bound violation. All waits
//! are bounded; all buffers are bounded; chromium is always reaped.

use std::fs;
use std::io;
use std::os::fd::{AsRawFd, RawFd};
use std::os::unix::process::CommandExt;
use std::path::{Path, PathBuf};
use std::process::{Child, Command, Stdio};
use std::time::{Duration, Instant};

use anyhow::{Context, Result, bail, ensure};
use clap::Parser;
use kwe_cdp::Client;
use serde_json::{Value, json};

/// Chromium's screencast window starts from the pipe, not the terminal.
const CHROMIUM_READ_FD: RawFd = 3;
const CHROMIUM_WRITE_FD: RawFd = 4;

#[derive(Debug, Parser)]
#[command(about = "BETA_M2a CDP pipe spike: screencast contract probe against installed Chromium")]
struct Args {
    /// Chromium binary to drive (must support --headless=new).
    #[arg(long, default_value = "chromium")]
    chromium: String,

    /// Local HTML fixture (continuously animated page); a file:// URL is
    /// derived from it. Screencast frames only flow while the page animates.
    #[arg(long)]
    fixture: PathBuf,

    /// Seconds to wait for the fixture page target after spawn.
    #[arg(long, default_value_t = 10)]
    target_timeout_secs: u64,

    /// Seconds for one whole phase (spawn to summary).
    #[arg(long, default_value_t = 30)]
    phase_timeout_secs: u64,

    /// No frame for this long means the screencast stalled (cadence with
    /// acks is ~33 ms, so 1 s is 30 missed cadence slots).
    #[arg(long, default_value_t = 1000)]
    stall_window_ms: u64,

    /// After a stall, keep pumping this long to confirm hard silence.
    #[arg(long, default_value_t = 1000)]
    silence_check_ms: u64,

    /// Frames to collect with acks before the acks are deliberately stopped.
    #[arg(long, default_value_t = 5)]
    min_frames: usize,
}

/// Per-phase measurements, reported in the JSON summary.
#[derive(Debug, Default)]
struct PhaseStats {
    frames: usize,
    first_frame_after_spawn_ms: Option<u64>,
    first_frame_after_start_ms: Option<u64>,
    jpeg_bytes: Vec<usize>,
    cadence_gaps_ms: Vec<usize>,
    /// Frames counted after acks were absent (phase A) or stopped (phase B).
    stall_frames: Option<usize>,
    /// True when no frame arrived during the post-stall silence window.
    silence_confirmed: bool,
    /// Events dropped because the bounded client queue overflowed (silent
    /// frame losses must be visible in the summary).
    events_dropped: u64,
}

fn main() -> Result<()> {
    let args = Args::parse();
    ensure!(args.min_frames >= 1, "--min-frames must be >= 1");

    let phase_root = std::env::temp_dir().join(format!(
        "kwe-cdp-spike-{}-{}",
        std::process::id(),
        Instant::now().elapsed().as_nanos()
    ));
    fs::create_dir_all(phase_root.join("a"))?;
    fs::create_dir_all(phase_root.join("b"))?;

    let mut summary = json!({
        "chromium": args.chromium,
        "fixture": args.fixture.display().to_string(),
    });

    // Phase A: fresh instance, no acks ever -> hard stall after a bounded
    // number of frames.
    let phase_a = run_phase(&args, "a", &phase_root.join("a"), false)?;
    summary["phase_a"] = json!({
        "stall_frames": phase_a.stall_frames,
        "silence_confirmed": phase_a.silence_confirmed,
        "first_frame_after_spawn_ms": phase_a.first_frame_after_spawn_ms,
        "events_dropped": phase_a.events_dropped,
    });

    // Phase B: fresh instance, ack every frame -> >= min_frames, then stop
    // acking and count the tail frames before silence.
    let phase_b = run_phase(&args, "b", &phase_root.join("b"), true)?;
    summary["phase_b"] = json!({
        "frames": phase_b.frames,
        "first_frame_after_start_ms": phase_b.first_frame_after_start_ms,
        "first_frame_after_spawn_ms": phase_b.first_frame_after_spawn_ms,
        "bytes_per_frame_avg": average(&phase_b.jpeg_bytes),
        "cadence_ms_avg": average(&phase_b.cadence_gaps_ms),
        "additional_after_ack_stop": phase_b.stall_frames,
        "silence_confirmed": phase_b.silence_confirmed,
        "events_dropped": phase_b.events_dropped,
    });

    // Best-effort cleanup of the throwaway profiles.
    fs::remove_dir_all(&phase_root).ok();

    println!("{}", serde_json::to_string_pretty(&summary)?);
    Ok(())
}

/// One phase: spawn a fresh chromium, attach, screencast, and drive the
/// stall/ack state machine. Everything here is bounded; the child is always
/// reaped.
fn run_phase(args: &Args, label: &str, user_data_dir: &Path, ack: bool) -> Result<PhaseStats> {
    let mut stderr = StderrRing::new(16 * 1024);
    let (mut child, read_fd, write_fd) = spawn_chromium(args, user_data_dir)?;
    let spawned_at = Instant::now();
    let mut client = Client::new(read_fd, write_fd)?.with_request_timeout(Duration::from_secs(10));

    // Stderr must drain or a chatty chromium could fill the pipe buffer;
    // set it nonblocking once and drain it inside the pump loop.
    if let Some(child_stderr) = child.stderr.as_mut() {
        set_nonblocking(child_stderr.as_raw_fd())?;
    }

    let phase = (|| -> Result<PhaseStats> {
        let target_id = find_page_target(&mut client, args, spawned_at)?;
        let (session_id, started_at) = attach_and_start(&mut client, &target_id)?;
        let mut stats = PhaseStats::default();
        let mut ctx = PhaseCtx {
            args,
            client: &mut client,
            session_id: &session_id,
            child: &mut child,
            stderr: &mut stderr,
            spawned_at,
            started_at,
        };
        collect_frames(&mut ctx, ack, &mut stats)?;
        stats.events_dropped = ctx.client.events_dropped();
        Ok(stats)
    })();

    // Teardown: dropping the client closes the pipe ends, which makes
    // chromium exit rc=0 within ~50 ms (pinned in docs/BETA_M2.md).
    drop(client);
    let exit_code = reap_child(&mut child);
    match (phase, exit_code) {
        (Ok(stats), Ok(code)) => {
            eprintln!("phase {label}: chromium exited rc={code}");
            Ok(stats)
        }
        (Err(err), _) => {
            eprintln!(
                "phase {label} failed; chromium stderr tail:\n{}",
                stderr.tail()
            );
            Err(err)
        }
        (Ok(_), Err(err)) => Err(err),
    }
}

/// getTargets until the fixture page target appears (headless=new starts on
/// a pre-navigation target that later becomes the fixture page).
fn find_page_target(client: &mut Client, args: &Args, spawned_at: Instant) -> Result<String> {
    let fixture_name = args
        .fixture
        .file_name()
        .context("fixture path has no file name")?
        .to_string_lossy();
    let deadline = spawned_at + Duration::from_secs(args.target_timeout_secs);
    loop {
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
                .is_some_and(|url| url.contains(&*fixture_name))
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
                "no fixture page target within {}s; page urls: {urls:?}",
                args.target_timeout_secs
            );
        }
        client.poll(Duration::from_millis(100))?;
    }
}

/// Attach (flattened) and start the screencast; returns the session id and
/// the moment startScreencast was answered (first-frame latency base).
fn attach_and_start(client: &mut Client, target_id: &str) -> Result<(String, Instant)> {
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
            "maxWidth": 160,
            "maxHeight": 90,
            "everyNthFrame": 1,
        }),
    )?;
    ensure_ok(&response, "Page.startScreencast")?;
    Ok((session_id, started_at))
}

/// Everything a phase pump needs, bundled so the pump signature stays small.
struct PhaseCtx<'a> {
    args: &'a Args,
    client: &'a mut Client,
    session_id: &'a str,
    child: &'a mut Child,
    stderr: &'a mut StderrRing,
    spawned_at: Instant,
    started_at: Instant,
}

/// Pump the screencast stream until the phase's stall contract is met.
///
/// With `ack`, every frame up to `min_frames` is acked, then the acks stop
/// and the tail frames are counted. Without `ack`, every frame is counted
/// (the stream must stall on its own).
fn collect_frames(ctx: &mut PhaseCtx<'_>, ack: bool, stats: &mut PhaseStats) -> Result<()> {
    let stall_window = Duration::from_millis(ctx.args.stall_window_ms);
    let silence_check = Duration::from_millis(ctx.args.silence_check_ms);
    let phase_deadline = ctx.started_at + Duration::from_secs(ctx.args.phase_timeout_secs);
    let mut last_frame_at: Option<Instant> = None;
    let mut silence_until: Option<Instant> = None;

    loop {
        if Instant::now() >= phase_deadline {
            if ack && stats.frames < ctx.args.min_frames {
                bail!(
                    "only {} frames with acks before the phase deadline (need >= {})",
                    stats.frames,
                    ctx.args.min_frames
                );
            }
            if stats.stall_frames.is_none() {
                bail!("no stall observed within the phase window");
            }
            break;
        }
        ctx.client.poll(Duration::from_millis(50))?;
        ctx.stderr.drain_from(ctx.child.stderr.as_mut());

        while let Some(event) = ctx.client.next_event() {
            if event.method != "Page.screencastFrame"
                || event.session_id.as_deref() != Some(ctx.session_id)
            {
                continue;
            }
            let frame_at = Instant::now();
            stats.frames += 1;
            stats
                .first_frame_after_spawn_ms
                .get_or_insert_with(|| frame_at.duration_since(ctx.spawned_at).as_millis() as u64);
            stats
                .first_frame_after_start_ms
                .get_or_insert_with(|| frame_at.duration_since(ctx.started_at).as_millis() as u64);
            if let Some(last) = last_frame_at
                && stats.cadence_gaps_ms.len() < 64
            {
                stats
                    .cadence_gaps_ms
                    .push(frame_at.duration_since(last).as_millis() as usize);
            }
            last_frame_at = Some(frame_at);
            let data = event
                .params
                .get("data")
                .and_then(Value::as_str)
                .context("screencastFrame without base64 data")?;
            stats.jpeg_bytes.push(decoded_len(data)?);

            if silence_until.is_some() {
                bail!("a frame arrived during the silence window: the stall was not hard");
            }
            if ack && stats.frames <= ctx.args.min_frames {
                let frame_session = event
                    .params
                    .get("sessionId")
                    .cloned()
                    .context("screencastFrame without its sessionId")?;
                let response = ctx.client.request_session(
                    ctx.session_id,
                    "Page.screencastFrameAck",
                    &json!({ "sessionId": frame_session }),
                )?;
                ensure_ok(&response, "Page.screencastFrameAck")?;
            } else if ack {
                // Acks deliberately stopped: count the tail frames.
                *stats.stall_frames.get_or_insert(0) += 1;
            } else {
                // No acks ever: every frame is an unacked frame.
                *stats.stall_frames.get_or_insert(0) += 1;
            }
        }

        // Stall detection: the last frame is older than the window -> begin
        // the silence check. A frame during the check fails the phase.
        if let (Some(last), None) = (last_frame_at, silence_until)
            && last.elapsed() >= stall_window
        {
            silence_until = Some(Instant::now() + silence_check);
        }
        if let Some(until) = silence_until
            && Instant::now() >= until
        {
            stats.silence_confirmed = true;
            break;
        }
    }
    Ok(())
}

/// Spawn chromium with `--remote-debugging-pipe`: the browser reads CDP
/// requests from fd 3 and writes responses/events to fd 4. Two socketpairs
/// carry the directions; the client keeps the opposite ends.
fn spawn_chromium(args: &Args, user_data_dir: &Path) -> Result<(Child, RawFd, RawFd)> {
    let (client_read, browser_write) = socket_pair()?;
    let (browser_read, client_write) = socket_pair()?;
    let mut command = Command::new(&args.chromium);
    command
        .args([
            "--no-sandbox",
            "--disable-gpu",
            "--disable-dev-shm-usage",
            "--disable-extensions",
            "--no-first-run",
            "--no-default-browser-check",
            "--disable-background-networking",
            "--headless=new",
            "--remote-debugging-pipe",
        ])
        .arg(format!("--user-data-dir={}", user_data_dir.display()))
        .arg(format!("file://{}", args.fixture.display()))
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::piped());
    unsafe {
        command.pre_exec(move || {
            // Order-independent fd setup: the socketpair ends may themselves
            // sit at fd 3 or fd 4, and a direct dup2(old, 3) could clobber
            // the other end before its turn. Duplicate both ends to temp
            // fds (>= 5) first, then move the temps onto 3/4; temps that
            // high make every dup2 a real dup, which also clears
            // FD_CLOEXEC (chromium checks fcntl(3/4, F_GETFL) at startup and
            // bails on closed descriptors). Finally close temps and the
            // originals.
            let temp_read = libc::fcntl(browser_read, libc::F_DUPFD_CLOEXEC, 5);
            if temp_read < 0 {
                return Err(io::Error::last_os_error());
            }
            let temp_write = libc::fcntl(browser_write, libc::F_DUPFD_CLOEXEC, 5);
            if temp_write < 0 {
                libc::close(temp_read);
                return Err(io::Error::last_os_error());
            }
            if libc::dup2(temp_read, CHROMIUM_READ_FD) < 0 {
                libc::close(temp_read);
                libc::close(temp_write);
                return Err(io::Error::last_os_error());
            }
            if libc::dup2(temp_write, CHROMIUM_WRITE_FD) < 0 {
                libc::close(temp_read);
                libc::close(temp_write);
                return Err(io::Error::last_os_error());
            }
            libc::close(temp_read);
            libc::close(temp_write);
            // A browser end that already sits at fd 3 or fd 4 IS the
            // descriptor chromium now runs on: closing it would sever the
            // pipe (chromium checks fcntl(3/4, F_GETFL) at startup and bails
            // on a missing descriptor). Close only the non-aliased
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
    let child = command
        .spawn()
        .with_context(|| format!("spawning {}", args.chromium))?;
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
    Ok((child, client_read, client_write))
}

/// Wait for the child to exit after the pipe close (chromium does so within
/// ~50 ms; a 5 s bound guards against a wedged browser). Kills on timeout.
fn reap_child(child: &mut Child) -> Result<i32> {
    let deadline = Instant::now() + Duration::from_secs(5);
    loop {
        if let Some(status) = child.try_wait()? {
            return Ok(status.code().unwrap_or(-1));
        }
        if Instant::now() >= deadline {
            let _ = child.kill();
            let _ = child.wait();
            bail!("chromium did not exit within 5s of the pipe close");
        }
        std::thread::sleep(Duration::from_millis(20));
    }
}

/// Base64 of a jpeg: 4 characters per 3 bytes, `=` padding at the tail.
fn decoded_len(base64: &str) -> Result<usize> {
    let padding = base64.bytes().rev().take(2).filter(|&b| b == b'=').count();
    ensure!(
        base64.len().is_multiple_of(4),
        "jpeg base64 length {} is not a multiple of 4",
        base64.len()
    );
    Ok(base64.len() / 4 * 3 - padding)
}

fn average(values: &[usize]) -> Option<f64> {
    if values.is_empty() {
        return None;
    }
    Some(values.iter().sum::<usize>() as f64 / values.len() as f64)
}

fn ensure_ok(response: &kwe_cdp::Response, what: &str) -> Result<()> {
    if let Some(error) = &response.error {
        bail!("{what} failed: {error}");
    }
    Ok(())
}

/// Bounded ring of chromium stderr, kept for failure diagnostics. Drained
/// nonblocking from the pipe so a chatty browser cannot fill it.
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
            let err = io::Error::last_os_error();
            if err.kind() == io::ErrorKind::Interrupted {
                continue;
            }
            break; // WouldBlock or a real error: nothing more to drain now.
        }
    }

    fn tail(&self) -> String {
        String::from_utf8_lossy(&self.buffer).into_owned()
    }
}

fn socket_pair() -> Result<(RawFd, RawFd)> {
    let mut fds = [0 as RawFd; 2];
    let rc = unsafe {
        libc::socketpair(
            libc::AF_UNIX,
            libc::SOCK_STREAM | libc::SOCK_CLOEXEC,
            0,
            fds.as_mut_ptr(),
        )
    };
    ensure!(rc == 0, "socketpair failed: {}", io::Error::last_os_error());
    Ok((fds[0], fds[1]))
}

fn set_nonblocking(fd: RawFd) -> Result<()> {
    let flags = unsafe { libc::fcntl(fd, libc::F_GETFL) };
    ensure!(
        flags >= 0,
        "fcntl F_GETFL failed: {}",
        io::Error::last_os_error()
    );
    let rc = unsafe { libc::fcntl(fd, libc::F_SETFL, flags | libc::O_NONBLOCK) };
    ensure!(
        rc == 0,
        "fcntl F_SETFL failed: {}",
        io::Error::last_os_error()
    );
    Ok(())
}
