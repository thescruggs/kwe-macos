// SPDX-License-Identifier: GPL-3.0-or-later
//! `scene.inspect` (SR-0b, report FD wiring SR-1b): one-shot bounded
//! supervision of `kwe-scene-inspector`.
//!
//! Containment mirrors `supervisor::spawn_worker` exactly (same private
//! per-launch HOME, `env_clear()` + the shared allowlist, stdin null /
//! stdout+stderr piped, the same `pre_exec` block: `setpgid(0, 0)`,
//! `PR_SET_PDEATHSIG` SIGKILL, a parent-pid check, `PR_SET_NO_NEW_PRIVS`,
//! `apply_resource_limits`) — but this is a single blocking call, not a
//! supervised long-lived worker: it spawns, drains stdout/stderr/the report
//! FD under a wall-clock deadline, reaps the child, removes the HOME dir,
//! and returns one JSON value. No renderer worker state is touched.
//!
//! SR-1b adds the report FD itself (docs/REPORT_PROTOCOL_V1.md): this
//! daemon creates a pipe with `libc::pipe2(..., O_CLOEXEC)`, and the
//! child's `pre_exec` closure `dup2`s the write end onto fd 3 (`dup2`
//! clears `O_CLOEXEC` on the TARGET fd, so fd 3 survives exec; the
//! `pipe2(O_CLOEXEC)` source fd — whatever number it actually landed at —
//! still closes automatically at exec regardless, so no explicit close of
//! it is needed in the child). fd 3 is free at this point in every launch:
//! `std::process::Command`'s own stdio setup (0/1/2) already ran before
//! `pre_exec`, and every OTHER fd this daemon process holds open is
//! `O_CLOEXEC` (every `OpenOptions::open` in this crate sets it; `std`'s
//! own socket/pipe types default to it too), so nothing else could be
//! sitting at fd 3 to collide with. The daemon (parent) closes its own
//! copy of the write end immediately after spawn — the pipe only reports
//! EOF once EVERY process's copy of the write end has closed, so if the
//! parent kept a copy open, the child closing its own copy would never be
//! visible as EOF on the read end the daemon retains and owns for the rest
//! of this call.
//!
//! The old stdout-JSON parsing path is REMOVED: `--report-fd 3` is always
//! passed, so a new inspector never writes its record to stdout at all.
//! `stdout` is still piped and drained (bounded, same as before) purely to
//! prevent a misbehaving/old-format child from deadlocking on a full pipe;
//! its content no longer feeds the RPC result.

use std::{
    fs,
    io::{Cursor, Read},
    os::{
        fd::OwnedFd,
        unix::{fs::PermissionsExt, io::AsRawFd, io::FromRawFd, process::CommandExt},
    },
    path::{Path, PathBuf},
    process::{Child, ChildStderr, ChildStdout, Command as ProcessCommand, Stdio},
    sync::atomic::{AtomicU64, Ordering},
    time::{Duration, Instant},
};

use kwe_report_protocol::{FrameKind, FrameReader, validate_inspection};
use serde_json::{Value, json};

use crate::supervisor::{
    RendererKind, RendererResourceLimits, apply_resource_limits, cleanup_renderer_home,
    env_allowlist, set_nonblocking, signal_process_group,
};

/// The report FD number the child is told about via `--report-fd`
/// (docs/REPORT_PROTOCOL_V1.md). Fixed: see the module doc for why fd 3
/// is always free at this point in a launch.
const REPORT_FD: i32 = 3;
/// Same cap the inspector itself enforces on its own stdout report before
/// SR-1b (kwe-scene-inspector's `MAX_REPORT_BYTES`); stdout is still
/// drained under this bound purely to prevent pipe backpressure from a
/// misbehaving/old-format child — its content is never parsed as the
/// result anymore. Read one byte past it so "exactly at the cap" and "over
/// the cap" stay distinguishable.
const MAX_REPORT_BYTES: usize = 65536;
/// One byte past the theoretical maximum well-formed report-FD stream size
/// (`kwe_report_protocol::MAX_TOTAL_PAYLOAD_BYTES` plus
/// `MAX_FRAMES_PER_STREAM` headers), so "exactly at the cap" (a legitimate,
/// maximally sized stream) and "over the cap" stay distinguishable —
/// mirrors the `MAX_REPORT_BYTES`-vs-`report-oversize` contract above, now
/// for the report FD specifically.
const REPORT_STREAM_MAX_BYTES: usize = kwe_report_protocol::MAX_TOTAL_PAYLOAD_BYTES
    + kwe_report_protocol::MAX_FRAMES_PER_STREAM * kwe_report_protocol::HEADER_BYTES
    + 1;
/// Bound on the `detail` string carried in a `report-malformed` result —
/// the codec error's bounded `Display`, never raw report bytes.
const MAX_DETAIL_BYTES: usize = 256;
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

    // The report pipe (docs/REPORT_PROTOCOL_V1.md): both ends O_CLOEXEC so
    // neither leaks into any OTHER child this daemon spawns; the write
    // end's CLOEXEC is cleared (via dup2 onto REPORT_FD, not here) only
    // inside THIS child's own pre_exec, right before its own exec.
    let mut report_fds = [0_i32; 2];
    // SAFETY: report_fds is a valid 2-element buffer for pipe2 to fill.
    if unsafe { libc::pipe2(report_fds.as_mut_ptr(), libc::O_CLOEXEC) } != 0 {
        eprintln!(
            "event=inspect.pipe_error detail={}",
            std::io::Error::last_os_error()
        );
        cleanup_renderer_home(&home_dir);
        return (
            json!({"outcome": "unknown", "reason": "inspector-unavailable"}),
            None,
            home_dir,
        );
    }
    let [report_read_fd, report_write_fd] = report_fds;

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
        .arg("--report-fd")
        .arg(REPORT_FD.to_string())
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
    // mirroring supervisor::spawn_worker's pre_exec exactly (plus the
    // report-fd dup2, SR-1b). It calls only async-signal-safe libc
    // functions and does not allocate.
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
            // dup2 clears O_CLOEXEC on the TARGET fd (REPORT_FD), so it
            // survives exec; report_write_fd's own O_CLOEXEC (from the
            // pipe2 call above) still closes it automatically at exec
            // regardless of its actual number, so no separate close is
            // needed here. See the module doc for why REPORT_FD is free.
            if libc::dup2(report_write_fd, REPORT_FD) < 0 {
                return Err(std::io::Error::last_os_error());
            }
            apply_resource_limits(resource_limits)?;
            Ok(())
        });
    }

    let spawn_result = command.spawn();
    // The daemon's own copy of the write end must close now regardless of
    // spawn outcome: on success, only the child's copy may stay open (or
    // the pipe would never report EOF once the child itself closes); on
    // failure, this is simply releasing the fd that was already created.
    // SAFETY: report_write_fd is a valid fd owned solely by this function,
    // not used again after this point.
    unsafe {
        libc::close(report_write_fd);
    }

    let mut child = match spawn_result {
        Ok(child) => child,
        Err(error) => {
            eprintln!("event=inspect.spawn_error detail={error}");
            // SAFETY: report_read_fd is a valid fd owned solely by this
            // function; nothing else has touched it yet.
            unsafe {
                libc::close(report_read_fd);
            }
            cleanup_renderer_home(&home_dir);
            return (
                json!({"outcome": "unknown", "reason": "inspector-unavailable"}),
                None,
                home_dir,
            );
        }
    };
    let pid = child.id();
    // SAFETY: report_read_fd is a valid, open fd from the pipe2 call
    // above, exclusively owned by this function up to this point;
    // ownership transfers to this File, which closes it on drop.
    let report = fs::File::from(unsafe { OwnedFd::from_raw_fd(report_read_fd) });
    let result = supervise(&mut child, config.wall_timeout, report);
    cleanup_renderer_home(&home_dir);
    (result, Some(pid), home_dir)
}

/// Drain the child's stdout/stderr/report-FD under `wall_timeout`, reap it,
/// and classify the outcome. Every return path has already reaped `child`.
fn supervise(child: &mut Child, wall_timeout: Duration, mut report: fs::File) -> Value {
    let (Some(mut stdout), Some(mut stderr)) = (child.stdout.take(), child.stderr.take()) else {
        eprintln!("event=inspect.pipe_error detail=stdout or stderr not captured");
        kill_and_reap(child);
        return json!({"outcome": "unknown", "reason": "inspector-unavailable"});
    };
    if set_nonblocking(stdout.as_raw_fd()).is_err()
        || set_nonblocking(stderr.as_raw_fd()).is_err()
        || set_nonblocking(report.as_raw_fd()).is_err()
    {
        eprintln!("event=inspect.pipe_error detail=failed to set pipes non-blocking");
        kill_and_reap(child);
        return json!({"outcome": "unknown", "reason": "inspector-unavailable"});
    }

    let deadline = Instant::now() + wall_timeout;
    let mut out_buffer: Vec<u8> = Vec::new();
    let mut err_tail: Vec<u8> = Vec::new();
    let mut report_buffer: Vec<u8> = Vec::new();
    loop {
        // stdout is drained (and bounded the same way) purely to prevent
        // pipe backpressure from a misbehaving/old-format child; only the
        // report buffer feeds `finalize`.
        let stdout_oversize = drain_stdout(&mut stdout, &mut out_buffer);
        drain_stderr_tail(&mut stderr, &mut err_tail);
        let report_oversize = drain_report(&mut report, &mut report_buffer);
        if stdout_oversize || report_oversize {
            kill_and_reap(child);
            return json!({"outcome": "unknown", "reason": "report-oversize"});
        }
        match child.try_wait() {
            Ok(Some(status)) => {
                // One last drain: bytes written just before exit may still
                // be sitting in the pipes.
                let stdout_oversize = drain_stdout(&mut stdout, &mut out_buffer);
                drain_stderr_tail(&mut stderr, &mut err_tail);
                let report_oversize = drain_report(&mut report, &mut report_buffer);
                if stdout_oversize || report_oversize {
                    return json!({"outcome": "unknown", "reason": "report-oversize"});
                }
                return finalize(status, &report_buffer, &err_tail);
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
/// byte where it happened does not matter, only that it did). stdout is no
/// longer parsed as the result (SR-1b: the report FD is), but is still
/// drained and bounded so a misbehaving/old-format child cannot deadlock on
/// a full pipe.
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

/// Same shape as `drain_stdout`, for the report FD (SR-1b): bounded to
/// `REPORT_STREAM_MAX_BYTES` rather than `MAX_REPORT_BYTES`, since a
/// well-formed report stream can (legitimately, at the cap) be larger than
/// one frame's payload — up to `MAX_FRAMES_PER_STREAM` frames, each with a
/// 12-byte header, totalling `MAX_TOTAL_PAYLOAD_BYTES` of payload.
fn drain_report(pipe: &mut fs::File, buffer: &mut Vec<u8>) -> bool {
    if buffer.len() > REPORT_STREAM_MAX_BYTES {
        return true;
    }
    let mut chunk = [0_u8; 4096];
    loop {
        match pipe.read(&mut chunk) {
            Ok(0) => break,
            Ok(count) => {
                buffer.extend_from_slice(&chunk[..count]);
                if buffer.len() > REPORT_STREAM_MAX_BYTES {
                    return true;
                }
            }
            Err(error) if error.kind() == std::io::ErrorKind::WouldBlock => break,
            Err(_) => break,
        }
    }
    false
}

/// Chunks drained per `drain_stderr_tail` call. At 4096 bytes each this
/// reads at most 64 KiB of stderr per `supervise` tick (review R2): without
/// a cap, a child that floods stderr continuously would keep this loop's
/// `Ok(count)` arm satisfied forever and starve the caller's `try_wait`/
/// deadline check — the tail-trim bounds memory, not the time spent here.
const STDERR_DRAIN_CHUNKS_PER_TICK: usize = 16;

/// Read up to `STDERR_DRAIN_CHUNKS_PER_TICK` chunks of `pipe` without
/// blocking, keeping only the most recent `STDERR_TAIL_BYTES` bytes.
/// Bounded per call (R2) so a stderr flood cannot starve the caller's
/// `try_wait`/deadline check: this always returns, and any remaining bytes
/// are simply picked up on the next `supervise` tick.
fn drain_stderr_tail(pipe: &mut ChildStderr, buffer: &mut Vec<u8>) {
    let mut chunk = [0_u8; 4096];
    for _ in 0..STDERR_DRAIN_CHUNKS_PER_TICK {
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

/// Classify a completed inspection from its exit status and accumulated
/// report-FD bytes (SR-1b). Nonzero exit keeps the pre-SR-1b contract:
/// `inspector-failed` + the stderr tail, report bytes ignored entirely —
/// this is also how an old inspector binary that rejects the unknown
/// `--report-fd` flag with a clap usage error (exit 2, message on stderr)
/// resolves: as `inspector-failed` carrying that usage error in
/// `stderr_tail` (docs/REPORT_PROTOCOL_V1.md's skew note; SR-1d builds the
/// fuller old/new matrix).
fn finalize(status: std::process::ExitStatus, report_bytes: &[u8], stderr_tail: &[u8]) -> Value {
    if !status.success() {
        return json!({
            "outcome": "unknown",
            "reason": "inspector-failed",
            "stderr_tail": String::from_utf8_lossy(stderr_tail),
        });
    }

    let mut reader = FrameReader::new(Cursor::new(report_bytes));
    let mut inspection_payloads: Vec<Vec<u8>> = Vec::new();
    loop {
        match reader.next_frame() {
            Ok(Some(frame)) => {
                // Unknown (and, until it has its own producer/validator,
                // SceneRenderReportV1) frames are skipped and counted —
                // FrameReader's own stream caps already counted them
                // against the limits above; nothing more to do with them
                // here. If ONLY such frames arrive, inspection_payloads
                // stays empty and falls through to report-missing below,
                // exactly like a stream with no frames at all.
                if frame.kind == FrameKind::SceneInspectionV1 {
                    inspection_payloads.push(frame.payload);
                }
            }
            Ok(None) => break,
            Err(error) => {
                return json!({
                    "outcome": "unknown",
                    "reason": "report-malformed",
                    "detail": bounded_detail(&error.to_string()),
                    "stderr_tail": String::from_utf8_lossy(stderr_tail),
                });
            }
        }
    }

    match inspection_payloads.len() {
        0 => json!({
            "outcome": "unknown",
            "reason": "report-missing",
            "stderr_tail": String::from_utf8_lossy(stderr_tail),
        }),
        1 => match validate_inspection(&inspection_payloads[0]) {
            Ok(record) => record,
            Err(error) => json!({
                "outcome": "unknown",
                "reason": "report-malformed",
                "detail": bounded_detail(&error.to_string()),
                "stderr_tail": String::from_utf8_lossy(stderr_tail),
            }),
        },
        _ => json!({"outcome": "unknown", "reason": "report-duplicate"}),
    }
}

/// Truncate `text` to at most `MAX_DETAIL_BYTES` bytes without splitting a
/// UTF-8 character (mirrors `kwe-scene-inspector::inventory`'s
/// `truncate_bytes`, independently — this crate has no dependency on that
/// one).
fn bounded_detail(text: &str) -> String {
    if text.len() <= MAX_DETAIL_BYTES {
        return text.to_string();
    }
    let mut end = MAX_DETAIL_BYTES;
    while end > 0 && !text.is_char_boundary(end) {
        end -= 1;
    }
    text[..end].to_string()
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

    /// Python source for a `write_frame(kind, payload)` helper, embedded in
    /// every fake inspector script below that writes to fd 3 — mirrors
    /// docs/REPORT_PROTOCOL_V1.md's wire format exactly (12-byte header:
    /// magic `KWR1`, kind, flags=0, reserved=0 as a u16 LE, payload_len as
    /// a u32 LE). Every fake declares `--report-fd` in its own argparse (so
    /// it does not choke on the flag the daemon always passes) except the
    /// one that deliberately mimics an old, pre-SR-1b inspector binary.
    const PYTHON_WRITE_FRAME_HELPER: &str = r#"
import os
import struct

def write_frame(kind, payload):
    header = b"KWR1" + bytes([kind, 0]) + struct.pack("<H", 0) + struct.pack("<I", len(payload))
    os.write(3, header + payload)
"#;

    /// (a) A well-behaved fake inspector's `scene-inspection-v1` report
    /// passes through verbatim — digest-verified, proving the daemon's
    /// `validate_inspection` call and this python fixture's digest
    /// computation (sorted keys, compact `(",", ":")` separators) agree
    /// byte-for-byte with `serde_json::to_vec`'s canonical form.
    #[test]
    fn valid_v1_report_passes_through_verbatim_digest_verified() {
        let root = temp_dir("valid");
        let script = write_script(
            &root,
            "fake-inspector.py",
            &format!(
                r#"#!/usr/bin/env python3
import argparse
import hashlib
import json
{PYTHON_WRITE_FRAME_HELPER}
parser = argparse.ArgumentParser()
parser.add_argument("--input", required=True)
parser.add_argument("--max-wall-ms", type=int, default=10000)
parser.add_argument("--report-fd", type=int, required=True)
args = parser.parse_args()

record = {{
    "schema": "scene-inspection-v1",
    "capabilities_schema": "scene-capabilities-v1",
    "content": {{"hash": "sha256:deadbeef", "source_bytes": 1, "kind": "json-dir"}},
    "inspector": {{"build": "dev", "abi": 0}},
    "outcome": "inventoried",
    "reason": "ok",
    "required": [],
    "detected": [],
    "unknown": {{"keys": 0, "types": 0, "objects": 0, "samples": [], "truncated": False}},
    "bounds": {{"wall_ms": 1, "peak_bytes": 0, "limits_hit": []}},
    "backend": None,
    "digest": "",
}}
# Byte-for-byte the same canonicalization serde_json::to_vec produces for a
# BTreeMap-backed Value: sorted keys, no whitespace.
serialized = json.dumps(record, sort_keys=True, separators=(",", ":")).encode()
record["digest"] = hashlib.sha256(serialized).hexdigest()
payload = json.dumps(record, sort_keys=True, separators=(",", ":")).encode()
write_frame(1, payload)
"#
            ),
        );
        let config = config_with(&root, Some(script), Duration::from_secs(5));
        let (result, pid, home_dir) = run_inspection_traced(&config, Path::new("/tmp/scene"));
        assert_eq!(result["schema"], "scene-inspection-v1", "{result}");
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

    /// (c) An inspector that floods the report FD past the stream cap is
    /// refused as report-oversize instead of being buffered without bound.
    #[test]
    fn flooded_report_fd_is_refused_as_report_oversize() {
        let root = temp_dir("flood-report");
        let script = write_script(
            &root,
            "fake-inspector.py",
            &format!(
                r#"#!/usr/bin/env python3
import argparse
{PYTHON_WRITE_FRAME_HELPER}
parser = argparse.ArgumentParser()
parser.add_argument("--input", required=True)
parser.add_argument("--max-wall-ms", type=int, default=10000)
parser.add_argument("--report-fd", type=int, required=True)
args = parser.parse_args()
# Well past REPORT_STREAM_MAX_BYTES; a single wildly-oversize payload_len
# claim would already be a report-malformed case, so this instead sends
# many small, individually well-formed frames.
for _ in range(20000):
    write_frame(1, b"x" * 100)
"#
            ),
        );
        let config = config_with(&root, Some(script), Duration::from_secs(5));
        let (result, _pid, home_dir) = run_inspection_traced(&config, Path::new("/tmp/scene"));
        assert_eq!(result["outcome"], "unknown", "{result}");
        assert_eq!(result["reason"], "report-oversize");
        assert!(!home_dir.exists());
        fs::remove_dir_all(&root).unwrap();
    }

    /// stdout is still drained and bounded defensively (pipe-backpressure
    /// safety, not because its content is used) even though a well-behaved
    /// new inspector never writes there — flooding it alone still resolves
    /// the same way flooding the report FD does.
    #[test]
    fn flooded_stdout_is_also_refused_as_report_oversize() {
        let root = temp_dir("flood-stdout");
        let script = write_script(
            &root,
            "fake-inspector.py",
            r#"#!/usr/bin/env python3
import argparse
import sys
parser = argparse.ArgumentParser()
parser.add_argument("--input", required=True)
parser.add_argument("--max-wall-ms", type=int, default=10000)
parser.add_argument("--report-fd", type=int, required=True)
args = parser.parse_args()
sys.stdout.write("x" * 200000)
sys.stdout.flush()
"#,
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

    /// A fake that writes its (otherwise well-formed v0-looking) report to
    /// stdout instead of the report FD — the pre-SR-1b behavior — sends
    /// zero frames on fd 3, so this resolves report-missing exactly like a
    /// child that reports nothing at all.
    #[test]
    fn report_written_to_stdout_instead_of_fd_is_report_missing() {
        let root = temp_dir("stdout-instead");
        let script = write_script(
            &root,
            "fake-inspector.py",
            r#"#!/usr/bin/env python3
import argparse
parser = argparse.ArgumentParser()
parser.add_argument("--input", required=True)
parser.add_argument("--max-wall-ms", type=int, default=10000)
parser.add_argument("--report-fd", type=int, required=True)
args = parser.parse_args()
print('{"schema":"scene-feature-inventory-v0","outcome":"inventoried","reason":"ok"}')
"#,
        );
        let config = config_with(&root, Some(script), Duration::from_secs(5));
        let (result, _pid, home_dir) = run_inspection_traced(&config, Path::new("/tmp/scene"));
        assert_eq!(result["outcome"], "unknown", "{result}");
        assert_eq!(result["reason"], "report-missing");
        assert!(!home_dir.exists());
        fs::remove_dir_all(&root).unwrap();
    }

    /// An unrecognized-kind frame, and nothing else, resolves report-missing
    /// too: `FrameReader` yields it as `FrameKind::Unknown` (additive
    /// evolution — docs/REPORT_PROTOCOL_V1.md), but zero `scene-inspection-v1`
    /// frames ever arrived.
    #[test]
    fn unknown_kind_frame_then_nothing_is_report_missing() {
        let root = temp_dir("unknown-kind");
        let script = write_script(
            &root,
            "fake-inspector.py",
            &format!(
                r#"#!/usr/bin/env python3
import argparse
{PYTHON_WRITE_FRAME_HELPER}
parser = argparse.ArgumentParser()
parser.add_argument("--input", required=True)
parser.add_argument("--max-wall-ms", type=int, default=10000)
parser.add_argument("--report-fd", type=int, required=True)
args = parser.parse_args()
write_frame(200, b"future schema this daemon does not know yet")
"#
            ),
        );
        let config = config_with(&root, Some(script), Duration::from_secs(5));
        let (result, _pid, home_dir) = run_inspection_traced(&config, Path::new("/tmp/scene"));
        assert_eq!(result["outcome"], "unknown", "{result}");
        assert_eq!(result["reason"], "report-missing");
        assert!(!home_dir.exists());
        fs::remove_dir_all(&root).unwrap();
    }

    /// Two kind-1 frames in one stream is a protocol violation the codec
    /// itself does not adjudicate (docs/REPORT_PROTOCOL_V1.md: "duplicate-
    /// kind policy ... is daemon policy, not codec") — this is that policy.
    #[test]
    fn duplicate_kind_one_frames_is_report_duplicate() {
        let root = temp_dir("duplicate");
        let script = write_script(
            &root,
            "fake-inspector.py",
            &format!(
                r#"#!/usr/bin/env python3
import argparse
{PYTHON_WRITE_FRAME_HELPER}
parser = argparse.ArgumentParser()
parser.add_argument("--input", required=True)
parser.add_argument("--max-wall-ms", type=int, default=10000)
parser.add_argument("--report-fd", type=int, required=True)
args = parser.parse_args()
write_frame(1, b'{{"schema":"scene-inspection-v1"}}')
write_frame(1, b'{{"schema":"scene-inspection-v1"}}')
"#
            ),
        );
        let config = config_with(&root, Some(script), Duration::from_secs(5));
        let (result, _pid, home_dir) = run_inspection_traced(&config, Path::new("/tmp/scene"));
        assert_eq!(result["outcome"], "unknown", "{result}");
        assert_eq!(result["reason"], "report-duplicate");
        assert!(!home_dir.exists());
        fs::remove_dir_all(&root).unwrap();
    }

    /// Garbage bytes on the report FD (not a well-formed frame stream at
    /// all — bad magic here) surfaces as report-malformed, carrying the
    /// codec's own bounded error detail plus the stderr tail.
    #[test]
    fn garbage_bytes_on_report_fd_is_report_malformed() {
        let root = temp_dir("garbage");
        let script = write_script(
            &root,
            "fake-inspector.py",
            r#"#!/usr/bin/env python3
import argparse
import os
parser = argparse.ArgumentParser()
parser.add_argument("--input", required=True)
parser.add_argument("--max-wall-ms", type=int, default=10000)
parser.add_argument("--report-fd", type=int, required=True)
args = parser.parse_args()
os.write(3, b"not a frame at all, just garbage bytes")
"#,
        );
        let config = config_with(&root, Some(script), Duration::from_secs(5));
        let (result, _pid, home_dir) = run_inspection_traced(&config, Path::new("/tmp/scene"));
        assert_eq!(result["outcome"], "unknown", "{result}");
        assert_eq!(result["reason"], "report-malformed");
        assert!(result["detail"].as_str().unwrap().len() <= MAX_DETAIL_BYTES);
        assert!(!home_dir.exists());
        fs::remove_dir_all(&root).unwrap();
    }

    /// A valid `scene-inspection-v1` frame whose JSON payload fails
    /// `validate_inspection` (missing every required field here) is also
    /// report-malformed — the frame itself was well-formed, its content
    /// was not.
    #[test]
    fn invalid_inspection_payload_is_report_malformed() {
        let root = temp_dir("invalid-payload");
        let script = write_script(
            &root,
            "fake-inspector.py",
            &format!(
                r#"#!/usr/bin/env python3
import argparse
{PYTHON_WRITE_FRAME_HELPER}
parser = argparse.ArgumentParser()
parser.add_argument("--input", required=True)
parser.add_argument("--max-wall-ms", type=int, default=10000)
parser.add_argument("--report-fd", type=int, required=True)
args = parser.parse_args()
write_frame(1, b'{{"not_a_real_field": true}}')
"#
            ),
        );
        let config = config_with(&root, Some(script), Duration::from_secs(5));
        let (result, _pid, home_dir) = run_inspection_traced(&config, Path::new("/tmp/scene"));
        assert_eq!(result["outcome"], "unknown", "{result}");
        assert_eq!(result["reason"], "report-malformed");
        assert!(result["detail"].as_str().unwrap().contains("schema"));
        assert!(!home_dir.exists());
        fs::remove_dir_all(&root).unwrap();
    }

    /// Old-binary skew (docs/REPORT_PROTOCOL_V1.md): an inspector that
    /// predates `--report-fd` rejects the unknown flag the way clap itself
    /// does (a usage error printed to stderr, exit 2). The daemon resolves
    /// this as inspector-failed with that usage error visible in
    /// stderr_tail, never a crash and never a guess reconstructed from
    /// stdout. This fake deliberately omits `--report-fd` from its own
    /// argparse to reproduce that exact rejection.
    #[test]
    fn old_inspector_without_report_fd_support_is_inspector_failed() {
        let root = temp_dir("old-binary");
        let script = write_script(
            &root,
            "fake-inspector.py",
            r#"#!/usr/bin/env python3
import argparse
parser = argparse.ArgumentParser(prog="fake-inspector.py")
parser.add_argument("--input", required=True)
parser.add_argument("--max-wall-ms", type=int, default=10000)
# No --report-fd: argparse itself rejects the daemon's --report-fd 3 as an
# unrecognized argument, prints a usage error to stderr, and exits 2 -- the
# same shape a real clap-generated usage error takes.
args = parser.parse_args()
"#,
        );
        let config = config_with(&root, Some(script), Duration::from_secs(5));
        let (result, _pid, home_dir) = run_inspection_traced(&config, Path::new("/tmp/scene"));
        assert_eq!(result["outcome"], "unknown", "{result}");
        assert_eq!(result["reason"], "inspector-failed");
        assert!(
            result["stderr_tail"]
                .as_str()
                .unwrap()
                .to_lowercase()
                .contains("unrecognized"),
            "{result}"
        );
        assert!(!home_dir.exists());
        fs::remove_dir_all(&root).unwrap();
    }

    /// SR-1d version-skew matrix: "inspector binary replaced on disk
    /// mid-uptime" (a package upgrade landing while the daemon keeps
    /// running, no restart). `run_inspection` spawns whatever is at
    /// `inspector_path` at call time -- there is no binary caching or
    /// stale handle anywhere in this module (no `File`/fd to the binary is
    /// held between calls, only the `PathBuf`) -- so overwriting the same
    /// path between two calls must be picked up by the second call with no
    /// daemon-side action at all.
    #[test]
    fn replaced_binary_on_disk_is_picked_up_without_caching() {
        let root = temp_dir("binary-replaced");
        let fake_with_marker = |marker: &str| -> String {
            format!(
                r#"#!/usr/bin/env python3
import argparse
import hashlib
import json
{PYTHON_WRITE_FRAME_HELPER}
parser = argparse.ArgumentParser()
parser.add_argument("--input", required=True)
parser.add_argument("--max-wall-ms", type=int, default=10000)
parser.add_argument("--report-fd", type=int, required=True)
args = parser.parse_args()

record = {{
    "schema": "scene-inspection-v1",
    "capabilities_schema": "scene-capabilities-v1",
    "content": {{"hash": "sha256:deadbeef", "source_bytes": 1, "kind": "json-dir"}},
    "inspector": {{"build": "{marker}", "abi": 0}},
    "outcome": "inventoried",
    "reason": "ok",
    "required": [],
    "detected": [],
    "unknown": {{"keys": 0, "types": 0, "objects": 0, "samples": [], "truncated": False}},
    "bounds": {{"wall_ms": 1, "peak_bytes": 0, "limits_hit": []}},
    "backend": None,
    "digest": "",
}}
serialized = json.dumps(record, sort_keys=True, separators=(",", ":")).encode()
record["digest"] = hashlib.sha256(serialized).hexdigest()
payload = json.dumps(record, sort_keys=True, separators=(",", ":")).encode()
write_frame(1, payload)
"#
            )
        };
        let script = write_script(&root, "fake-inspector.py", &fake_with_marker("build-a"));
        let config = config_with(&root, Some(script.clone()), Duration::from_secs(5));

        let (first, _pid, _home) = run_inspection_traced(&config, Path::new("/tmp/scene"));
        assert_eq!(first["outcome"], "inventoried", "{first}");
        assert_eq!(first["inspector"]["build"], "build-a");

        // The package upgrade: SAME path, new content, daemon not
        // restarted. write_script rewrites the file at the same path and
        // re-sets it executable.
        write_script(&root, "fake-inspector.py", &fake_with_marker("build-b"));

        let (second, _pid, _home) = run_inspection_traced(&config, Path::new("/tmp/scene"));
        assert_eq!(second["outcome"], "inventoried", "{second}");
        assert_eq!(
            second["inspector"]["build"], "build-b",
            "the daemon must spawn whatever is at the configured path NOW, \
             not a binary it cached from an earlier call: {second}"
        );
        fs::remove_dir_all(&root).unwrap();
    }
}
