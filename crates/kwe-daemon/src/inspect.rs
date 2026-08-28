// SPDX-License-Identifier: GPL-3.0-or-later
//! `scene.inspect` (SR-0b): one-shot bounded supervision of
//! `kwe-scene-inspector`.
//!
//! Containment mirrors `supervisor::spawn_worker` exactly (same private
//! per-launch HOME, `env_clear()` + the shared allowlist, stdin null /
//! stdout+stderr piped, the same `pre_exec` block: `setpgid(0, 0)`,
//! `PR_SET_PDEATHSIG` SIGKILL, a parent-pid check, `PR_SET_NO_NEW_PRIVS`,
//! `apply_resource_limits`) — but this is a single blocking call, not a
//! supervised long-lived worker: it spawns, drains stdout/stderr under a
//! wall-clock deadline, reaps the child, removes the HOME dir, and returns
//! one JSON value. No renderer worker state is touched.

use std::{
    fs,
    io::Read,
    os::unix::{fs::PermissionsExt, io::AsRawFd, process::CommandExt},
    path::{Path, PathBuf},
    process::{Child, ChildStderr, ChildStdout, Command as ProcessCommand, Stdio},
    sync::atomic::{AtomicU64, Ordering},
    time::{Duration, Instant},
};

use serde_json::{Value, json};

use crate::supervisor::{
    RendererKind, RendererResourceLimits, apply_resource_limits, cleanup_renderer_home,
    env_allowlist, set_nonblocking, signal_process_group,
};

/// The draft schema `run_inspection` requires on a successful inspector
/// report before passing it through (docs/SCENE_CAPABILITIES.md). SR-1
/// freezes the exact name; this stays a plain string compare until then.
const EXPECTED_SCHEMA: &str = "scene-feature-inventory-v0";
/// Same cap the inspector itself enforces on its own report
/// (kwe-scene-inspector's `MAX_REPORT_BYTES`); read one byte past it so the
/// "exactly at the cap" and "over the cap" cases are distinguishable.
const MAX_REPORT_BYTES: usize = 65536;
/// Bounded stderr diagnostic carried in a failed result, lossy-UTF8, tail
/// only (mirrors the supervisor's `StderrRing` bound, scaled down for a
/// one-shot report instead of a long-lived worker).
const STDERR_TAIL_BYTES: usize = 512;

static LAUNCH_SERIAL: AtomicU64 = AtomicU64::new(0);

/// `scene.inspect` containment configuration, constructed alongside the
/// supervisor's own `SupervisorConfig` in `main.rs`.
#[derive(Debug, Clone)]
pub(crate) struct InspectConfig {
    /// The `kwe-scene-inspector` binary. `None` when never configured —
    /// fails closed with `inspector-unavailable` rather than guessing at a
    /// binary that might not be the right one.
    pub inspector_path: Option<PathBuf>,
    /// Directory under which private per-launch HOME dirs are created,
    /// same as the supervisor's `runtime_dir`.
    pub runtime_dir: PathBuf,
    /// Wall-clock deadline for one inspection: on expiry the inspector's
    /// whole process group is SIGKILLed and reaped.
    pub wall_timeout: Duration,
    /// Pre-exec resource ceilings. Reused from the scene renderer kind
    /// (`resource_limits_for(RendererKind::Scene)`) so the inspector is
    /// never less contained than the renderer it stands in for.
    pub resource_limits: RendererResourceLimits,
}

/// Run one bounded inspection of `input` and return the draft
/// `scene-feature-inventory-v0` record (or a typed `{"outcome":"unknown",
/// "reason":"..."}` result on any containment failure) verbatim as the
/// `scene.inspect` RPC result.
pub(crate) fn run_inspection(config: &InspectConfig, input: &Path) -> Value {
    run_inspection_traced(config, input).0
}

/// `run_inspection`, additionally returning the spawned child's pid (when
/// spawn succeeded) and the HOME dir path, so tests can assert the child is
/// actually reaped and the HOME dir actually removed.
fn run_inspection_traced(config: &InspectConfig, input: &Path) -> (Value, Option<u32>, PathBuf) {
    let Some(inspector_path) = &config.inspector_path else {
        return (
            json!({"outcome": "unknown", "reason": "inspector-unavailable"}),
            None,
            PathBuf::new(),
        );
    };
    let serial = LAUNCH_SERIAL.fetch_add(1, Ordering::Relaxed);
    let home_dir = config
        .runtime_dir
        .join(format!("inspect-home-{}-{serial}", std::process::id()));
    if let Err(error) = fs::create_dir_all(&home_dir) {
        eprintln!(
            "event=inspect.home_error path={} detail={error}",
            home_dir.display()
        );
        return (
            json!({"outcome": "unknown", "reason": "inspector-unavailable"}),
            None,
            home_dir,
        );
    }
    if let Err(error) = fs::set_permissions(&home_dir, fs::Permissions::from_mode(0o700)) {
        eprintln!(
            "event=inspect.home_error path={} detail={error}",
            home_dir.display()
        );
        cleanup_renderer_home(&home_dir);
        return (
            json!({"outcome": "unknown", "reason": "inspector-unavailable"}),
            None,
            home_dir,
        );
    }

    // The u32 -> i32 pid conversion mirrors spawn_worker's own overflow
    // guard (crates/kwe-daemon/src/supervisor.rs); a pid this large never
    // happens in practice, but the check keeps the pre_exec parent-pid
    // comparison well-defined instead of silently wrapping.
    let expected_parent = match i32::try_from(std::process::id()) {
        Ok(pid) => pid,
        Err(_) => {
            eprintln!("event=inspect.pid_overflow");
            cleanup_renderer_home(&home_dir);
            return (
                json!({"outcome": "unknown", "reason": "inspector-unavailable"}),
                None,
                home_dir,
            );
        }
    };

    let mut command = ProcessCommand::new(inspector_path);
    command
        .arg("--input")
        .arg(input)
        .arg("--max-wall-ms")
        .arg(
            config
                .wall_timeout
                .as_millis()
                .min(u128::from(u64::MAX))
                .to_string(),
        )
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .env_clear()
        // The inspector never needs XDG_RUNTIME_DIR (that is web-renderer
        // only in env_allowlist); RendererKind::Scene picks the same
        // {HOME, PATH}-only allowlist every non-web kind gets.
        .envs(env_allowlist(RendererKind::Scene, &home_dir));

    let resource_limits = config.resource_limits;
    // SAFETY: this closure runs in the child after fork and before exec,
    // mirroring supervisor::spawn_worker's pre_exec exactly. It calls only
    // async-signal-safe libc functions and does not allocate.
    unsafe {
        command.pre_exec(move || {
            if libc::setpgid(0, 0) != 0 {
                return Err(std::io::Error::last_os_error());
            }
            if libc::prctl(libc::PR_SET_PDEATHSIG, libc::SIGKILL, 0, 0, 0) != 0 {
                return Err(std::io::Error::last_os_error());
            }
            if libc::getppid() != expected_parent {
                return Err(std::io::Error::new(
                    std::io::ErrorKind::Interrupted,
                    "daemon exited before inspector exec",
                ));
            }
            if libc::prctl(libc::PR_SET_NO_NEW_PRIVS, 1, 0, 0, 0) != 0 {
                return Err(std::io::Error::last_os_error());
            }
            apply_resource_limits(resource_limits)?;
            Ok(())
        });
    }

    let mut child = match command.spawn() {
        Ok(child) => child,
        Err(error) => {
            eprintln!("event=inspect.spawn_error detail={error}");
            cleanup_renderer_home(&home_dir);
            return (
                json!({"outcome": "unknown", "reason": "inspector-unavailable"}),
                None,
                home_dir,
            );
        }
    };
    let pid = child.id();
    let result = supervise(&mut child, config.wall_timeout);
    cleanup_renderer_home(&home_dir);
    (result, Some(pid), home_dir)
}

/// Drain the child's stdout/stderr under `wall_timeout`, reap it, and
/// classify the outcome. Every return path has already reaped `child`.
fn supervise(child: &mut Child, wall_timeout: Duration) -> Value {
    let (Some(mut stdout), Some(mut stderr)) = (child.stdout.take(), child.stderr.take()) else {
        eprintln!("event=inspect.pipe_error detail=stdout or stderr not captured");
        kill_and_reap(child);
        return json!({"outcome": "unknown", "reason": "inspector-unavailable"});
    };
    if set_nonblocking(stdout.as_raw_fd()).is_err() || set_nonblocking(stderr.as_raw_fd()).is_err()
    {
        eprintln!("event=inspect.pipe_error detail=failed to set pipes non-blocking");
        kill_and_reap(child);
        return json!({"outcome": "unknown", "reason": "inspector-unavailable"});
    }

    let deadline = Instant::now() + wall_timeout;
    let mut out_buffer: Vec<u8> = Vec::new();
    let mut err_tail: Vec<u8> = Vec::new();
    loop {
        let oversize = drain_stdout(&mut stdout, &mut out_buffer);
        drain_stderr_tail(&mut stderr, &mut err_tail);
        if oversize {
            kill_and_reap(child);
            return json!({"outcome": "unknown", "reason": "report-oversize"});
        }
        match child.try_wait() {
            Ok(Some(status)) => {
                // One last drain: bytes written just before exit may still
                // be sitting in the pipe.
                let oversize = drain_stdout(&mut stdout, &mut out_buffer);
                drain_stderr_tail(&mut stderr, &mut err_tail);
                if oversize {
                    return json!({"outcome": "unknown", "reason": "report-oversize"});
                }
                return finalize(status, &out_buffer, &err_tail);
            }
            Ok(None) => {
                if Instant::now() >= deadline {
                    kill_and_reap(child);
                    return json!({"outcome": "unknown", "reason": "timeout"});
                }
                std::thread::sleep(Duration::from_millis(10));
            }
            Err(error) => {
                eprintln!("event=inspect.wait_error detail={error}");
                kill_and_reap(child);
                return json!({"outcome": "unknown", "reason": "inspector-unavailable"});
            }
        }
    }
}

/// Read as much of `pipe` as is available without blocking, appending to
/// `buffer`. Returns `true` once `buffer` has grown past
/// `MAX_REPORT_BYTES` (the caller stops the child at that point; the exact
/// byte where it happened does not matter, only that it did).
fn drain_stdout(pipe: &mut ChildStdout, buffer: &mut Vec<u8>) -> bool {
    if buffer.len() > MAX_REPORT_BYTES {
        return true;
    }
    let mut chunk = [0_u8; 4096];
    loop {
        match pipe.read(&mut chunk) {
            Ok(0) => break,
            Ok(count) => {
                buffer.extend_from_slice(&chunk[..count]);
                if buffer.len() > MAX_REPORT_BYTES {
                    return true;
                }
            }
            Err(error) if error.kind() == std::io::ErrorKind::WouldBlock => break,
            Err(_) => break,
        }
    }
    false
}

/// Read as much of `pipe` as is available without blocking, keeping only
/// the most recent `STDERR_TAIL_BYTES` bytes.
fn drain_stderr_tail(pipe: &mut ChildStderr, buffer: &mut Vec<u8>) {
    let mut chunk = [0_u8; 4096];
    loop {
        match pipe.read(&mut chunk) {
            Ok(0) => break,
            Ok(count) => {
                buffer.extend_from_slice(&chunk[..count]);
                if buffer.len() > STDERR_TAIL_BYTES {
                    let excess = buffer.len() - STDERR_TAIL_BYTES;
                    buffer.drain(..excess);
                }
            }
            Err(error) if error.kind() == std::io::ErrorKind::WouldBlock => break,
            Err(_) => break,
        }
    }
}

fn finalize(status: std::process::ExitStatus, stdout: &[u8], stderr_tail: &[u8]) -> Value {
    if !status.success() {
        return json!({
            "outcome": "unknown",
            "reason": "inspector-failed",
            "stderr_tail": String::from_utf8_lossy(stderr_tail),
        });
    }
    match serde_json::from_slice::<Value>(stdout) {
        Ok(record) if record.get("schema").and_then(Value::as_str) == Some(EXPECTED_SCHEMA) => {
            record
        }
        _ => json!({
            "outcome": "unknown",
            "reason": "inspector-failed",
            "stderr_tail": String::from_utf8_lossy(stderr_tail),
        }),
    }
}

/// SIGKILL the child's whole process group and reap it. Safe to call on an
/// already-exited child: `signal_process_group` on a gone pgid is a no-op
/// kill(2) failure, and `wait()` on an already-reaped `Child` handle
/// returns immediately with the exit status std cached at reap time.
fn kill_and_reap(child: &mut Child) {
    signal_process_group(child.id(), libc::SIGKILL);
    let _ = child.wait();
}

#[cfg(test)]
mod tests {
    use super::*;

    fn temp_dir(label: &str) -> PathBuf {
        let dir = std::env::temp_dir().join(format!(
            "kwe-inspect-{label}-{}-{}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap_or_default()
                .as_nanos()
        ));
        fs::create_dir_all(&dir).unwrap();
        dir
    }

    fn sample_limits() -> RendererResourceLimits {
        RendererResourceLimits {
            address_space_mib: 4096,
            file_size_mib: 160,
            open_files: 256,
            processes: 1024,
            core_dump_bytes: 0,
        }
    }

    fn write_script(root: &Path, name: &str, body: &str) -> PathBuf {
        let path = root.join(name);
        fs::write(&path, body).unwrap();
        fs::set_permissions(&path, fs::Permissions::from_mode(0o755)).unwrap();
        path
    }

    fn config_with(
        root: &Path,
        inspector_path: Option<PathBuf>,
        wall_timeout: Duration,
    ) -> InspectConfig {
        InspectConfig {
            inspector_path,
            runtime_dir: root.join("runtime"),
            wall_timeout,
            resource_limits: sample_limits(),
        }
    }

    /// (a) A well-behaved fake inspector's report passes through verbatim.
    #[test]
    fn valid_report_passes_through_verbatim() {
        let root = temp_dir("valid");
        let script = write_script(
            &root,
            "fake-inspector.py",
            r#"#!/usr/bin/env python3
import argparse
parser = argparse.ArgumentParser()
parser.add_argument("--input", required=True)
parser.add_argument("--max-wall-ms", type=int, default=10000)
args = parser.parse_args()
print('{"schema":"scene-feature-inventory-v0","outcome":"inventoried","reason":"ok",'
      '"content":{"hash":"sha256:deadbeef","source_bytes":1,"kind":"json-dir"},'
      '"inspector":{"build":"dev","abi":0},"required":[],"detected":[],'
      '"unknown":{"keys":0,"types":0,"objects":0,"samples":[],"truncated":false},'
      '"bounds":{"wall_ms":1,"peak_bytes":0,"limits_hit":[]},"digest":"abc"}')
"#,
        );
        let config = config_with(&root, Some(script), Duration::from_secs(5));
        let (result, pid, home_dir) = run_inspection_traced(&config, Path::new("/tmp/scene"));
        assert_eq!(result["schema"], "scene-feature-inventory-v0");
        assert_eq!(result["outcome"], "inventoried");
        assert_eq!(result["content"]["hash"], "sha256:deadbeef");
        assert!(pid.is_some());
        assert!(!home_dir.as_os_str().is_empty());
        assert!(!home_dir.exists(), "HOME dir must be removed on exit");
        fs::remove_dir_all(&root).unwrap();
    }

    /// (b) A hung fake inspector is killed at the wall-clock deadline, is
    /// actually reaped (kill(pid, 0) fails afterward), and its HOME dir is
    /// removed.
    #[test]
    fn hung_inspector_times_out_and_is_reaped() {
        let root = temp_dir("hang");
        let script = write_script(
            &root,
            "fake-inspector.py",
            "#!/usr/bin/env python3\nimport time\ntime.sleep(600)\n",
        );
        let config = config_with(&root, Some(script), Duration::from_millis(300));
        let (result, pid, home_dir) = run_inspection_traced(&config, Path::new("/tmp/scene"));
        assert_eq!(result["outcome"], "unknown");
        assert_eq!(result["reason"], "timeout");
        let pid = pid.expect("child was spawned") as libc::pid_t;
        // SAFETY: signal 0 only probes existence; no signal is delivered.
        let alive = unsafe { libc::kill(pid, 0) } == 0;
        assert!(
            !alive,
            "the hung inspector must be reaped after the timeout"
        );
        assert!(!home_dir.exists(), "HOME dir must be removed on timeout");
        fs::remove_dir_all(&root).unwrap();
    }

    /// (c) An inspector that floods stdout past the 64 KiB cap is refused
    /// as report-oversize instead of being buffered without bound.
    #[test]
    fn flooded_stdout_is_refused_as_report_oversize() {
        let root = temp_dir("flood");
        let script = write_script(
            &root,
            "fake-inspector.py",
            "#!/usr/bin/env python3\nimport sys\nsys.stdout.write('x' * 200000)\nsys.stdout.flush()\n",
        );
        let config = config_with(&root, Some(script), Duration::from_secs(5));
        let (result, _pid, home_dir) = run_inspection_traced(&config, Path::new("/tmp/scene"));
        assert_eq!(result["outcome"], "unknown");
        assert_eq!(result["reason"], "report-oversize");
        assert!(!home_dir.exists());
        fs::remove_dir_all(&root).unwrap();
    }

    /// (d) A nonzero exit surfaces as inspector-failed with the stderr text.
    #[test]
    fn nonzero_exit_surfaces_stderr_tail() {
        let root = temp_dir("fail");
        let script = write_script(
            &root,
            "fake-inspector.py",
            "#!/usr/bin/env python3\nimport sys\nsys.stderr.write('boom detail\\n')\nsys.exit(1)\n",
        );
        let config = config_with(&root, Some(script), Duration::from_secs(5));
        let (result, _pid, home_dir) = run_inspection_traced(&config, Path::new("/tmp/scene"));
        assert_eq!(result["outcome"], "unknown");
        assert_eq!(result["reason"], "inspector-failed");
        assert!(
            result["stderr_tail"]
                .as_str()
                .unwrap()
                .contains("boom detail"),
            "{result}"
        );
        assert!(!home_dir.exists());
        fs::remove_dir_all(&root).unwrap();
    }

    /// (e) An unconfigured binary fails closed without spawning anything.
    #[test]
    fn unconfigured_binary_is_unavailable() {
        let root = temp_dir("unconfigured");
        let config = config_with(&root, None, Duration::from_secs(5));
        let (result, pid, _home_dir) = run_inspection_traced(&config, Path::new("/tmp/scene"));
        assert_eq!(result["outcome"], "unknown");
        assert_eq!(result["reason"], "inspector-unavailable");
        assert!(pid.is_none());
        fs::remove_dir_all(&root).unwrap();
    }
}
