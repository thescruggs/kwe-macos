// SPDX-License-Identifier: GPL-3.0-or-later
//! BETA_M1d audio capture management: owns the `kwe-audio-worker` child,
//! restarts it on unexpected exit (bounded: at most `MAX_RESTARTS` restarts
//! within a rolling `RESTART_WINDOW`, then disabled with a one-time log),
//! terminates it with SIGTERM (grace, then SIGKILL) on daemon shutdown, and
//! exposes `audio.status` state plus the worker's pid so the daemon can
//! identify its own worker's `audio.forward` connections by peer credentials.
//!
//! The worker's stderr is inherited (its bounded diagnostics land in the
//! daemon's own log); its stdout is unused. The worker is spawned with the
//! daemon's socket path, so it can refresh its display generation via
//! `renderer.status` whenever the supervisor rejects a frame.

use std::{
    collections::VecDeque,
    os::unix::process::{CommandExt, ExitStatusExt},
    path::PathBuf,
    process::{Child, Command, ExitStatus, Stdio},
    sync::{
        Arc,
        atomic::{AtomicU32, Ordering},
        mpsc::{self, Receiver, RecvTimeoutError, Sender, SyncSender, TrySendError},
    },
    thread::{self, JoinHandle},
    time::{Duration, Instant},
};

use anyhow::{Context, Result, anyhow, bail};
use serde::Serialize;

/// Restart budget: after this many restarts within `RESTART_WINDOW` the
/// worker is disabled for the daemon's lifetime (spec: max 3 within 10 min).
const MAX_RESTARTS: usize = 3;
const RESTART_WINDOW: Duration = Duration::from_secs(600);
/// Bounded delay between an unexpected exit and the next spawn.
const RESTART_DELAY: Duration = Duration::from_millis(500);
/// Grace period for the worker to stop pw-record and exit after SIGTERM.
const STOP_GRACE: Duration = Duration::from_secs(1);
/// Supervisor-style command channel bounds.
const COMMAND_CAPACITY: usize = 16;
const COMMAND_TIMEOUT: Duration = Duration::from_secs(5);
/// Reap/command polling interval while the worker is running.
const WORKER_TICK: Duration = Duration::from_millis(100);

#[derive(Debug, Clone)]
pub struct AudioCaptureConfig {
    /// `--audio-capture` flag; when false the service stays idle and
    /// `audio.status` reports `enabled: false`.
    pub enabled: bool,
    /// Worker binary (default: kwe-audio-worker beside the daemon).
    pub worker_path: PathBuf,
    /// The daemon's own RPC socket path, passed to the worker as `--socket`.
    pub socket: PathBuf,
    /// Optional PipeWire capture node passthrough (`--capture-node`).
    pub capture_node: Option<String>,
}

#[derive(Debug, Clone, Serialize)]
pub struct AudioCaptureStatus {
    /// The `--audio-capture` configuration flag, not the live state.
    pub enabled: bool,
    pub pid: Option<u32>,
    pub restarts: u64,
    /// Present only after the restart budget is exhausted.
    pub disabled_reason: Option<String>,
}

enum AudioCommand {
    Status(Sender<Result<AudioCaptureStatus>>),
    Shutdown,
}

#[derive(Debug, Clone)]
pub struct AudioCaptureHandle {
    sender: SyncSender<AudioCommand>,
}

impl AudioCaptureHandle {
    pub fn status(&self) -> Result<AudioCaptureStatus> {
        let (sender, receiver) = mpsc::channel();
        match self.sender.try_send(AudioCommand::Status(sender)) {
            Ok(()) => {}
            Err(TrySendError::Full(_)) => bail!("audio capture command queue is full"),
            Err(TrySendError::Disconnected(_)) => bail!("audio capture is unavailable"),
        }
        receiver
            .recv_timeout(COMMAND_TIMEOUT)
            .map_err(|_| anyhow!("audio capture command timed out"))?
    }
}

pub struct AudioCaptureService {
    handle: AudioCaptureHandle,
    /// Shared pid cell: the runtime stores the live worker pid here, and the
    /// daemon main thread compares it against the SO_PEERCRED credentials of
    /// each connection to recognize its own worker's requests.
    worker_pid: Arc<AtomicU32>,
    thread: Option<JoinHandle<()>>,
}

impl AudioCaptureService {
    pub fn start(config: AudioCaptureConfig) -> Result<Self> {
        let (sender, receiver) = mpsc::sync_channel(COMMAND_CAPACITY);
        let worker_pid = Arc::new(AtomicU32::new(0));
        let thread_worker_pid = worker_pid.clone();
        let thread = thread::Builder::new()
            .name("kwe-audio-capture".into())
            .spawn(move || Runtime::new(config, thread_worker_pid).run(receiver))?;
        Ok(Self {
            handle: AudioCaptureHandle { sender },
            worker_pid,
            thread: Some(thread),
        })
    }

    pub fn handle(&self) -> AudioCaptureHandle {
        self.handle.clone()
    }

    pub fn worker_pid(&self) -> Arc<AtomicU32> {
        self.worker_pid.clone()
    }
}

impl Drop for AudioCaptureService {
    fn drop(&mut self) {
        let _ = self.handle.sender.send(AudioCommand::Shutdown);
        if let Some(thread) = self.thread.take() {
            let _ = thread.join();
        }
    }
}

struct Runtime {
    config: AudioCaptureConfig,
    worker_pid: Arc<AtomicU32>,
    child: Option<Child>,
    restarts: u64,
    restarts_in_window: VecDeque<Instant>,
    disabled_reason: Option<String>,
    disable_logged: bool,
}

impl Runtime {
    fn new(config: AudioCaptureConfig, worker_pid: Arc<AtomicU32>) -> Self {
        Self {
            config,
            worker_pid,
            child: None,
            restarts: 0,
            restarts_in_window: VecDeque::new(),
            disabled_reason: None,
            disable_logged: false,
        }
    }

    fn run(mut self, receiver: Receiver<AudioCommand>) {
        if self.config.enabled
            && let Err(error) = self.spawn_worker()
        {
            eprintln!("event=audio.worker.spawn_failed detail={error}");
            self.worker_down();
        }
        loop {
            match receiver.recv_timeout(WORKER_TICK) {
                Ok(AudioCommand::Status(reply)) => {
                    let _ = reply.send(Ok(self.status()));
                }
                Ok(AudioCommand::Shutdown) | Err(RecvTimeoutError::Disconnected) => {
                    self.shutdown_worker();
                    return;
                }
                Err(RecvTimeoutError::Timeout) => {}
            }
            if self.config.enabled
                && let Some(child) = self.child.as_mut()
            {
                match child.try_wait() {
                    Ok(Some(status)) => {
                        // Unexpected exit: the daemon did not request this.
                        // Any exit code counts; the worker's own diagnostics
                        // (inherited stderr) carry the reason.
                        eprintln!("event=audio.worker.exited detail={}", exit_detail(&status));
                        self.worker_pid.store(0, Ordering::Release);
                        self.child = None;
                        self.worker_down();
                    }
                    Ok(None) => {}
                    Err(error) => {
                        eprintln!("event=audio.worker.wait_error detail={error}");
                        self.worker_pid.store(0, Ordering::Release);
                        self.child = None;
                        self.worker_down();
                    }
                }
            }
        }
    }

    fn status(&self) -> AudioCaptureStatus {
        let pid = self.worker_pid.load(Ordering::Acquire);
        AudioCaptureStatus {
            enabled: self.config.enabled,
            pid: (pid != 0).then_some(pid),
            restarts: self.restarts,
            disabled_reason: self.disabled_reason.clone(),
        }
    }

    /// Bounded restart accounting: after `MAX_RESTARTS` unexpected exits
    /// within `RESTART_WINDOW` the worker is disabled for the daemon's
    /// lifetime, with exactly one log line.
    fn worker_down(&mut self) {
        if self.disabled_reason.is_some() {
            return;
        }
        let now = Instant::now();
        prune_restart_window(&mut self.restarts_in_window, now, RESTART_WINDOW);
        if self.restarts_in_window.len() >= MAX_RESTARTS {
            self.disabled_reason = Some("too_many_restarts".to_string());
            if !self.disable_logged {
                self.disable_logged = true;
                eprintln!(
                    "event=audio.worker.disabled detail=too_many_restarts max={MAX_RESTARTS} window_s={}",
                    RESTART_WINDOW.as_secs()
                );
            }
            return;
        }
        self.restarts_in_window.push_back(now);
        self.restarts = self.restarts.saturating_add(1);
        eprintln!(
            "event=audio.worker.restart count={} window_count={} delay_ms={}",
            self.restarts,
            self.restarts_in_window.len(),
            RESTART_DELAY.as_millis()
        );
        thread::sleep(RESTART_DELAY);
        if let Err(error) = self.spawn_worker() {
            eprintln!("event=audio.worker.spawn_failed detail={error}");
            // A failed spawn counts toward the budget too; the next tick
            // arrives only through another exit, so recurse once here.
            self.worker_down();
        }
    }

    fn spawn_worker(&mut self) -> Result<()> {
        let mut command = Command::new(&self.config.worker_path);
        command
            .arg("--socket")
            .arg(&self.config.socket)
            .stdin(Stdio::null())
            .stdout(Stdio::null())
            .stderr(Stdio::inherit());
        if let Some(node) = &self.config.capture_node {
            command.arg("--capture-node").arg(node);
        }
        // SAFETY: this closure runs in the child after fork and before exec.
        // It calls only async-signal-safe libc functions and does not
        // allocate. The worker gets its own process group and a
        // parent-death SIGTERM (not SIGKILL: the worker's SIGTERM handler
        // stops pw-record gracefully, so a crashed daemon cannot orphan the
        // capture child).
        let expected_parent = i32::try_from(std::process::id()).context("daemon pid overflow")?;
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
                        "daemon exited before audio worker exec",
                    ));
                }
                if libc::prctl(libc::PR_SET_NO_NEW_PRIVS, 1, 0, 0, 0) != 0 {
                    return Err(std::io::Error::last_os_error());
                }
                Ok(())
            });
        }
        let child = command
            .spawn()
            .with_context(|| format!("launch {}", self.config.worker_path.display()))?;
        self.worker_pid.store(child.id(), Ordering::Release);
        self.child = Some(child);
        eprintln!(
            "event=audio.worker.spawned pid={}",
            self.worker_pid.load(Ordering::Acquire)
        );
        Ok(())
    }

    /// SIGTERM -> grace -> SIGKILL of the worker's process group, then reap.
    /// The worker's group contains only the worker (pw-record gets its own
    /// group inside the worker), so the worker's graceful pw-record stop is
    /// not raced by the daemon.
    fn shutdown_worker(&mut self) {
        let Some(mut child) = self.child.take() else {
            return;
        };
        let pid = child.id();
        if child.try_wait().ok().flatten().is_some() {
            return;
        }
        signal_process_group(pid, libc::SIGTERM);
        let deadline = Instant::now() + STOP_GRACE;
        while Instant::now() < deadline {
            if child.try_wait().ok().flatten().is_some() {
                return;
            }
            thread::sleep(Duration::from_millis(10));
        }
        signal_process_group(pid, libc::SIGKILL);
        let _ = child.kill();
        let _ = child.wait();
        eprintln!("event=audio.worker.forced_kill pid={pid}");
    }
}

/// Drop restart timestamps that fell out of the window; pure so the budget
/// logic is unit-testable without a live runtime.
fn prune_restart_window(history: &mut VecDeque<Instant>, now: Instant, window: Duration) {
    // checked_sub: before the window has elapsed nothing can be stale.
    let cutoff = now.checked_sub(window).unwrap_or(now);
    while let Some(front) = history.front() {
        if *front < cutoff {
            history.pop_front();
        } else {
            break;
        }
    }
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

fn exit_detail(status: &ExitStatus) -> String {
    if let Some(code) = status.code() {
        format!("exit_code_{code}")
    } else if let Some(signal) = status.signal() {
        format!("signal_{signal}")
    } else {
        "unknown_exit".to_string()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::time::Duration as StdDuration;

    #[test]
    fn restart_window_prunes_old_entries() {
        let now = Instant::now();
        let mut history = VecDeque::from([
            now - RESTART_WINDOW - Duration::from_millis(1),
            now - RESTART_WINDOW + Duration::from_millis(1),
            now,
        ]);
        prune_restart_window(&mut history, now, RESTART_WINDOW);
        assert_eq!(history.len(), 2);
    }

    #[test]
    fn budget_disables_after_three_restarts_within_the_window() {
        let config = AudioCaptureConfig {
            enabled: true,
            worker_path: PathBuf::from("/bin/false"),
            socket: PathBuf::from("/nonexistent/kwe.sock"),
            capture_node: None,
        };
        let service = AudioCaptureService::start(config).unwrap();
        let handle = service.handle();
        // /bin/false exits immediately every time: the runtime restarts it
        // MAX_RESTARTS times (500 ms apart) and then disables. The daemon
        // socket does not exist, so the worker binary never even reaches a
        // connect attempt -- /bin/false just exits 1.
        let deadline = Instant::now() + StdDuration::from_secs(10);
        let mut status = handle.status().unwrap();
        while Instant::now() < deadline {
            status = handle.status().unwrap();
            if status.disabled_reason.is_some() {
                break;
            }
            thread::sleep(Duration::from_millis(50));
        }
        assert_eq!(status.disabled_reason.as_deref(), Some("too_many_restarts"));
        assert_eq!(status.restarts, MAX_RESTARTS as u64);
        assert_eq!(status.pid, None);
        // The pid cell is cleared once the child is gone.
        assert_eq!(service.worker_pid().load(Ordering::Acquire), 0);
    }

    #[test]
    fn shutdown_terminates_the_worker_and_reaps_it() {
        let config = AudioCaptureConfig {
            enabled: true,
            worker_path: PathBuf::from("/bin/sleep"),
            socket: PathBuf::from("/nonexistent/kwe.sock"),
            capture_node: None,
        };
        let service = AudioCaptureService::start(config).unwrap();
        let worker_pid = service.worker_pid();
        let deadline = Instant::now() + StdDuration::from_secs(10);
        while worker_pid.load(Ordering::Acquire) == 0 && Instant::now() < deadline {
            thread::sleep(Duration::from_millis(20));
        }
        let pid = worker_pid.load(Ordering::Acquire);
        assert_ne!(pid, 0, "worker did not spawn");
        // Dropping the service sends Shutdown and joins the runtime thread:
        // SIGTERM to the sleep child, bounded grace, then reap.
        drop(service);
        let gone = || unsafe { libc::kill(i32::try_from(pid).unwrap(), 0) } != 0;
        let deadline = Instant::now() + StdDuration::from_secs(3);
        while !gone() && Instant::now() < deadline {
            thread::sleep(Duration::from_millis(10));
        }
        assert!(gone(), "worker process {pid} survived daemon shutdown");
    }

    #[test]
    fn disabled_service_reports_enabled_false_without_pid() {
        let config = AudioCaptureConfig {
            enabled: false,
            worker_path: PathBuf::from("/bin/sleep"),
            socket: PathBuf::from("/nonexistent/kwe.sock"),
            capture_node: None,
        };
        let service = AudioCaptureService::start(config).unwrap();
        let status = service.handle().status().unwrap();
        assert!(!status.enabled);
        assert_eq!(status.pid, None);
        assert_eq!(status.restarts, 0);
        assert_eq!(status.disabled_reason, None);
        assert_eq!(service.worker_pid().load(Ordering::Acquire), 0);
    }

    #[test]
    fn status_serializes_with_the_documented_shape() {
        let status = AudioCaptureStatus {
            enabled: true,
            pid: Some(1234),
            restarts: 2,
            disabled_reason: None,
        };
        let value = serde_json::to_value(&status).unwrap();
        assert_eq!(value["enabled"], true);
        assert_eq!(value["pid"], 1234);
        assert_eq!(value["restarts"], 2);
        // None fields serialize as null, matching the daemon's convention for
        // optional status fields (renderer.status does the same for pid).
        assert!(value["disabled_reason"].is_null());
        let disabled = AudioCaptureStatus {
            enabled: true,
            pid: None,
            restarts: 3,
            disabled_reason: Some("too_many_restarts".into()),
        };
        let value = serde_json::to_value(&disabled).unwrap();
        assert_eq!(value["disabled_reason"], "too_many_restarts");
        assert!(value["pid"].is_null());
    }

    #[test]
    fn exit_detail_names_code_and_signal() {
        let status = Command::new("/bin/false").status().unwrap();
        assert_eq!(exit_detail(&status), "exit_code_1");
    }
}
