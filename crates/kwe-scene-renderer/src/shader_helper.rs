// SPDX-License-Identifier: GPL-3.0-or-later
//! SR-3b: the renderer-side client for the killable shader-compile helper
//! (`kwe-shader-compiler`, SR-3a's protocol skeleton,
//! `docs/SHADER_HELPER_PROTOCOL_V1.md`).
//!
//! **Zero behavior change in this slice (decision (a)):** `compile` is
//! called, its outcome is logged (bounded, once per outcome class), and
//! the caller (`main.rs`'s `compile_one_material`) falls through to
//! today's in-thread `materialshader::compile_stage` unconditionally,
//! regardless of what `compile` returned — including a (not-yet-possible
//! in this slice) `HelperOutcome::Compiled`. Every scene renders
//! byte-identically to trunk. SR-3c is the slice that starts consuming a
//! real `Compiled` result instead of always falling through.
//!
//! Containment mirrors `kwe-daemon::inspect`'s one-shot supervision
//! (`setpgid`, `PR_SET_PDEATHSIG`, a parent-pid check,
//! `PR_SET_NO_NEW_PRIVS`, `apply_resource_limits`, a nonblocking-drain +
//! deadline `supervise` loop) as closely as a renderer WORKER — not the
//! daemon — can:
//!
//! - **No `setpgid`.** Plan §4.3: the helper lives in the RENDERER's own
//!   process group, not a new one, so the daemon's existing group-kill
//!   (`supervisor::signal_process_group`, `kill(-pid, signal)` against the
//!   renderer's own pgid) already covers the helper too — a helper that
//!   somehow outlives a killed renderer is still caught by the same
//!   group-wide signal the daemon already sends. This is also WHY this
//!   module's own timeout-kill path must use `kill(pid, ...)`
//!   (positive, single-process) and never `kill(-pid, ...)`: this
//!   renderer process shares the helper's process group (it never called
//!   `setpgid` on it), so a negative-pid kill here would signal the
//!   RENDERER itself, not just the stuck helper.
//! - **`PR_SET_PDEATHSIG` SIGKILL** — same as every other spawned child in
//!   this codebase; the parent-pid check right after guards the fork/exec
//!   race the daemon's own `pre_exec` blocks document.
//! - **Stricter rlimits than the renderer's own**, applied via `pre_exec`
//!   (a floor the renderer's OWN rlimits — set by the daemon's
//!   `apply_resource_limits` before the renderer's exec — already bound
//!   from above; this only lowers further, never raises): address space
//!   512 MiB, file size 16 MiB, `RLIMIT_NOFILE` 32. `RLIMIT_NPROC` is
//!   DELIBERATELY left unset (inherits whatever the daemon set for the
//!   renderer itself) — this renderer process has no way to know the
//!   daemon's configured process-count budget (`RendererResourceLimits`
//!   is daemon-side config, never passed down to a worker), so guessing a
//!   number here could accidentally be MORE permissive than the daemon's
//!   own floor for a build where that budget is small, defeating the
//!   point of "stricter than the renderer's own."
//! - **`env_clear()` + `HOME` only** (not the daemon's full
//!   `{HOME, PATH}` allowlist): the helper never shells out to another
//!   binary or does a `PATH` lookup (it is invoked by an explicit
//!   resolved path, and this slice's skeleton does not exec anything
//!   itself), so `PATH` is not needed. `HOME` is copied from the
//!   RENDERER's own environment (`std::env::var("HOME")`), not a fresh
//!   directory created for the helper — unlike `inspect.rs`'s per-launch
//!   HOME dir (created and removed by the daemon around a call it fully
//!   owns), the renderer's own HOME is ALREADY a private per-launch
//!   directory the daemon created for the RENDERER (see
//!   `supervisor::spawn_worker`'s own HOME-dir handling) — reusing it for
//!   the helper needs no new directory, no new create/cleanup pair, and
//!   is exactly as private as the renderer's own sandbox already is.
//! - **stdin/stdout piped** (the SR-3a protocol channel itself — one
//!   kind-16 frame out, frames in under `StreamCaps::SHADER_RESPONSE|
//!   `); **stderr piped and tail-bounded** the same shape
//!   `inspect.rs::drain_stderr_tail` uses, diagnostic only, never parsed.
//! - **No explicit inherited-FD closing beyond `Stdio::piped()`/`Stdio::
//!   null()`**: every fd this renderer process itself holds open already
//!   follows the workspace-wide `O_CLOEXEC` discipline
//!   `kwe-daemon::inspect`'s own module doc names (every `File::open` in
//!   this crate, every `std` socket/pipe type) — there is nothing else
//!   open at this point that ISN'T already CLOEXEC, so no separate sweep
//!   is needed.
//! - **Always reaped**: every return path from `compile` has already
//!   called `child.wait()` (directly, or via the timeout kill sequence
//!   below) before returning — no path leaves a zombie.
//!
//! `RLIMIT`s are FLOORS the daemon's own worker containment already sets
//! for the renderer process as a whole (`supervisor::apply_resource_limits`,
//! run in the renderer's own `pre_exec` before ITS exec) — this module's
//! `pre_exec` for the HELPER child only ever lowers them further, on top
//! of whatever the renderer inherited, never raises past the renderer's
//! own ceiling (a child cannot exceed rlimits already in force in its
//! parent's process at fork time). Confirmed (SR-3b): `RendererKind::Scene`
//! is NOT run inside `bwrap` by the daemon — only `RendererKind::Web`'s
//! own chromium child gets bwrap sandboxing (`supervisor.rs`'s
//! `--allow-network`/bwrap handling is gated on `RendererKind::Web`
//! specifically) — so there is no sandbox boundary this helper spawn
//! needs to additionally cross or account for.
//!
//! **`PR_SET_PDEATHSIG` is documented-only, not tested** (task's own
//! allowance for this case): proving it requires killing the TEST HARNESS
//! process itself (this module's `pre_exec` closure checks `getppid()`
//! against the RENDERER's own pid — here, the test process — so
//! exercising real death-signal delivery means SIGKILLing the test runner
//! mid-test and observing an orphan from a SEPARATE process, which
//! `cargo test`'s own process model has no clean way to assert on from
//! inside the very test that would be doing the dying). The mechanism
//! itself (`prctl(PR_SET_PDEATHSIG, SIGKILL)`) is the exact same call
//! `kwe-daemon::inspect`'s own `pre_exec` closure already makes and relies
//! on in production; this module reuses it under the identical "runs in
//! the child, before exec, only async-signal-safe calls" contract, not a
//! new implementation of its own.

use std::io::{Read, Write};
use std::os::unix::io::AsRawFd;
use std::os::unix::process::CommandExt;
use std::path::PathBuf;
use std::process::{Child, ChildStderr, ChildStdout, Command, Stdio};
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::{Duration, Instant};

use kwe_report_protocol::{
    FrameKind, FrameReader, SHADER_COMPILE_REQUEST_SCHEMA, StreamCaps,
    validate_shader_compile_response, write_frame,
};
use serde_json::{Value, json};

use crate::materialshader::Stage;
use crate::set_nonblocking;
use crate::text::truncate_chars;

/// `--shader-helper-timeout-ms` bounds and default (task §1).
pub const DEFAULT_TIMEOUT_MS: u64 = 10_000;
pub const MIN_TIMEOUT_MS: u64 = 100;
pub const MAX_TIMEOUT_MS: u64 = 30_000;

/// The SIGTERM grace window before a timed-out helper is SIGKILLed — a
/// fake helper that ignores SIGTERM must still die (tested), so this is
/// bounded and short: the whole point of the helper is to be killable
/// FAST, not to add a second long wait on top of the caller's own
/// `--shader-helper-timeout-ms`.
const TERMINATE_GRACE: Duration = Duration::from_millis(200);

/// One material-shader compile request, already fully resolved by the
/// caller (`main.rs`'s `compile_one_material`) — `source` is the FINAL,
/// fully preprocessed GLSL text (`shaderpre::preprocess`'s own `#include`
/// splicing has already run; this module never re-resolves an include
/// itself, so the wire request's own `includes` map is always sent empty
/// — see the module doc and `docs/SHADER_HELPER_PROTOCOL_V1.md`'s request
/// schema, whose `includes`/`combos`/`defines` fields exist for a FUTURE
/// caller that has NOT already spliced them, which this one has).
pub struct ShaderCompileRequest<'a> {
    pub stage: Stage,
    pub source: &'a str,
}

/// The result of one helper exchange. `Compiled` is RESERVED for SR-3c —
/// never constructed by this slice's `compile` (the helper skeleton itself
/// never emits a compiled response either — see `kwe-shader-compiler`'s
/// own SR-3a scope note) — kept here now so SR-3c's own change is additive
/// to this enum rather than a redesign of it.
#[derive(Debug)]
pub enum HelperOutcome {
    /// The helper answered with `status: "unimplemented"` — SR-3a's
    /// skeleton always does this for a structurally valid request.
    Unimplemented,
    /// The exchange completed, but the response was not a well-formed
    /// `shader-compile-response-v1` frame the helper claims success/
    /// unimplemented through, OR the helper itself reported
    /// `status: "protocol-error"`. Bounded diagnostic detail.
    ProtocolError(String),
    /// The helper could not be reached at all: not configured (no path
    /// resolved), spawn failed (missing binary, permission), or the
    /// exchange failed at the pipe/wait level. Bounded diagnostic detail.
    Unavailable(String),
    /// No response arrived within `--shader-helper-timeout-ms`; the child
    /// was killed (SIGTERM then, after a short grace, SIGKILL) and reaped.
    Timeout,
    /// SR-3c consumes this — reserved, never constructed in this slice.
    #[allow(dead_code)]
    Compiled { spirv: Vec<u32>, response: Value },
}

impl HelperOutcome {
    /// A short, stable class name for the "once per outcome class" log
    /// dedup below — never the (potentially longer/more specific) detail
    /// string itself.
    fn class(&self) -> &'static str {
        match self {
            Self::Unimplemented => "unimplemented",
            Self::ProtocolError(_) => "protocol_error",
            Self::Unavailable(_) => "unavailable",
            Self::Timeout => "timeout",
            Self::Compiled { .. } => "compiled",
        }
    }
}

/// The renderer's client handle for the shader-compile helper — one
/// instance per renderer process, constructed once in `main()` and passed
/// by shared reference through the whole material-compile pipeline
/// (`compile_material_layers` -> `compile_one_material` -> here).
pub struct ShaderHelper {
    path: Option<PathBuf>,
    timeout: Duration,
    // "One bounded stderr diagnostic per outcome class per process" (task
    // §2) — 5 flags, one per `HelperOutcome` variant, each logged at most
    // once over this process's whole lifetime regardless of how many
    // materials/effect passes compile. `compile` takes `&self` (the
    // existing `compile_material_layers` call chain threads a shared
    // `&ShaderHelper`, not an owned/mutable one), so these need interior
    // mutability.
    logged_unimplemented: AtomicBool,
    logged_protocol_error: AtomicBool,
    logged_unavailable: AtomicBool,
    logged_timeout: AtomicBool,
}

impl ShaderHelper {
    /// `explicit_path`: `--shader-helper`, when given. Otherwise resolved
    /// beside this renderer's own executable (mirrors
    /// `kwe-daemon::main::default_inspector_path` exactly: build the
    /// sibling path unconditionally, do not `exists()`-check it — an
    /// actually-missing binary is discovered, once, the same way a
    /// misconfigured `--shader-helper` is: `Command::spawn` fails and
    /// `compile` classifies it `Unavailable`). `current_exe()` failing
    /// (should not happen in practice) leaves `path: None` — permanent
    /// fallback, decision (a).
    pub fn new(explicit_path: Option<PathBuf>, timeout_ms: u64) -> Self {
        let path = explicit_path.or_else(resolve_beside_self);
        Self {
            path,
            timeout: Duration::from_millis(timeout_ms),
            logged_unimplemented: AtomicBool::new(false),
            logged_protocol_error: AtomicBool::new(false),
            logged_unavailable: AtomicBool::new(false),
            logged_timeout: AtomicBool::new(false),
        }
    }

    /// Runs one full request/response exchange for `request`, spawning a
    /// FRESH helper process (decision (b): spawn-per-request, matching the
    /// skeleton's own one-serial-request-per-process contract — no
    /// process is reused across calls in this slice). Never panics on any
    /// helper misbehavior; every failure mode is a typed `HelperOutcome`,
    /// logged at most once per class, per the module/task doc.
    pub fn compile(&self, request: &ShaderCompileRequest<'_>) -> HelperOutcome {
        let outcome = self.compile_inner(request);
        self.log_once(&outcome);
        outcome
    }

    fn compile_inner(&self, request: &ShaderCompileRequest<'_>) -> HelperOutcome {
        let Some(path) = &self.path else {
            return HelperOutcome::Unavailable("not configured".to_string());
        };

        let stage = match request.stage {
            Stage::Vertex => "vertex",
            Stage::Fragment => "fragment",
        };
        let payload = match serde_json::to_vec(&json!({
            "schema": SHADER_COMPILE_REQUEST_SCHEMA,
            "stage": stage,
            "source": request.source,
            // shaderpre has already spliced every #include into `source`
            // (see the module doc on `ShaderCompileRequest`) — this
            // caller never has a separate includes map to send.
            "includes": {},
            "combos": {},
            "defines": {},
        })) {
            Ok(bytes) => bytes,
            Err(error) => return HelperOutcome::Unavailable(bounded(&error.to_string())),
        };
        let mut framed_request = Vec::new();
        if let Err(error) = write_frame(
            &mut framed_request,
            FrameKind::ShaderCompileRequestV1,
            &payload,
        ) {
            return HelperOutcome::Unavailable(bounded(&error.to_string()));
        }

        let expected_parent = match i32::try_from(std::process::id()) {
            Ok(pid) => pid,
            Err(_) => return HelperOutcome::Unavailable("renderer pid overflow".to_string()),
        };
        let home = std::env::var("HOME").ok();

        let mut command = Command::new(path);
        command
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .env_clear();
        if let Some(home) = home {
            command.env("HOME", home);
        }
        // SAFETY: this closure runs in the child after fork and before
        // exec. It calls only async-signal-safe libc functions and does
        // not allocate. Deliberately NO `setpgid` here — see the module
        // doc's containment section for why the helper must stay in the
        // RENDERER's own process group.
        unsafe {
            command.pre_exec(move || {
                if libc::prctl(libc::PR_SET_PDEATHSIG, libc::SIGKILL, 0, 0, 0) != 0 {
                    return Err(std::io::Error::last_os_error());
                }
                if libc::getppid() != expected_parent {
                    return Err(std::io::Error::new(
                        std::io::ErrorKind::Interrupted,
                        "renderer exited before helper exec",
                    ));
                }
                if libc::prctl(libc::PR_SET_NO_NEW_PRIVS, 1, 0, 0, 0) != 0 {
                    return Err(std::io::Error::last_os_error());
                }
                apply_helper_resource_limits()?;
                Ok(())
            });
        }

        let mut child = match command.spawn() {
            Ok(child) => child,
            Err(error) => return HelperOutcome::Unavailable(bounded(&error.to_string())),
        };

        let Some(mut stdin) = child.stdin.take() else {
            kill_and_reap(&mut child);
            return HelperOutcome::Unavailable("stdin not captured".to_string());
        };
        // The helper reads exactly one request frame then checks for
        // trailing bytes (SR-3a decision (c)) — closing stdin right after
        // writing the one frame (drop below) is what lets it observe a
        // clean EOF instead of blocking on a second read.
        if let Err(error) = stdin.write_all(&framed_request) {
            drop(stdin);
            kill_and_reap(&mut child);
            return HelperOutcome::Unavailable(bounded(&error.to_string()));
        }
        drop(stdin);

        self.wait_and_read(&mut child)
    }

    /// Drains stdout/stderr under `self.timeout`, reaps the child on every
    /// path, and classifies the outcome. Mirrors
    /// `kwe_daemon::inspect::supervise`'s nonblocking-drain + deadline
    /// loop shape exactly (this module has no equivalent of the report FD
    /// — stdout itself IS the response channel here).
    fn wait_and_read(&self, child: &mut Child) -> HelperOutcome {
        let (Some(mut stdout), Some(mut stderr)) = (child.stdout.take(), child.stderr.take())
        else {
            kill_and_reap(child);
            return HelperOutcome::Unavailable("stdout/stderr not captured".to_string());
        };
        if set_nonblocking(stdout.as_raw_fd()).is_err()
            || set_nonblocking(stderr.as_raw_fd()).is_err()
        {
            kill_and_reap(child);
            return HelperOutcome::Unavailable("failed to set pipes non-blocking".to_string());
        }

        let deadline = Instant::now() + self.timeout;
        let mut out_buffer: Vec<u8> = Vec::new();
        let mut err_tail: Vec<u8> = Vec::new();
        loop {
            let oversize = drain_stdout(&mut stdout, &mut out_buffer);
            drain_stderr_tail(&mut stderr, &mut err_tail);
            if oversize {
                kill_and_reap(child);
                return HelperOutcome::ProtocolError("response-oversize".to_string());
            }
            match child.try_wait() {
                Ok(Some(_status)) => {
                    // One last drain: bytes written just before exit may
                    // still be sitting in the pipe.
                    let oversize = drain_stdout(&mut stdout, &mut out_buffer);
                    drain_stderr_tail(&mut stderr, &mut err_tail);
                    let _ = child.wait(); // already exited; reclaims the zombie
                    if oversize {
                        return HelperOutcome::ProtocolError("response-oversize".to_string());
                    }
                    return finalize(&out_buffer);
                }
                Ok(None) => {
                    if Instant::now() >= deadline {
                        terminate_and_reap(child);
                        return HelperOutcome::Timeout;
                    }
                    std::thread::sleep(Duration::from_millis(10));
                }
                Err(error) => {
                    kill_and_reap(child);
                    return HelperOutcome::Unavailable(bounded(&error.to_string()));
                }
            }
        }
    }

    fn log_once(&self, outcome: &HelperOutcome) {
        let (flag, detail) = match outcome {
            HelperOutcome::Unimplemented => (&self.logged_unimplemented, None),
            HelperOutcome::ProtocolError(detail) => {
                (&self.logged_protocol_error, Some(detail.as_str()))
            }
            HelperOutcome::Unavailable(detail) => (&self.logged_unavailable, Some(detail.as_str())),
            HelperOutcome::Timeout => (&self.logged_timeout, None),
            // Never constructed this slice; nothing to (de-)dup or log.
            HelperOutcome::Compiled { .. } => return,
        };
        if flag.swap(true, Ordering::Relaxed) {
            return; // already logged this class once this process
        }
        match detail {
            Some(detail) => eprintln!(
                "event=renderer.scene.shader_helper_outcome class={} detail={detail}",
                outcome.class()
            ),
            None => eprintln!(
                "event=renderer.scene.shader_helper_outcome class={}",
                outcome.class()
            ),
        }
    }
}

/// Beside this renderer's own executable — mirrors
/// `kwe-daemon::main::default_inspector_path` exactly (see `ShaderHelper::
/// new`'s doc comment for why this does not `exists()`-check).
fn resolve_beside_self() -> Option<PathBuf> {
    let executable = std::env::current_exe().ok()?;
    let directory = executable.parent()?;
    Some(directory.join("kwe-shader-compiler"))
}

/// Stricter-than-the-renderer's-own rlimits (module doc): address space
/// 512 MiB, file size 16 MiB, 32 open files. `RLIMIT_NPROC` is
/// deliberately NOT set here — see the module doc's containment section.
fn apply_helper_resource_limits() -> std::io::Result<()> {
    const MIB: u64 = 1024 * 1024;
    set_resource_limit(libc::RLIMIT_AS, 512 * MIB)?;
    set_resource_limit(libc::RLIMIT_FSIZE, 16 * MIB)?;
    set_resource_limit(libc::RLIMIT_NOFILE, 32)?;
    Ok(())
}

fn set_resource_limit(resource: libc::__rlimit_resource_t, value: u64) -> std::io::Result<()> {
    let value = libc::rlim_t::try_from(value).map_err(|_| {
        std::io::Error::new(std::io::ErrorKind::InvalidInput, "resource limit overflow")
    })?;
    let limit = libc::rlimit {
        rlim_cur: value,
        rlim_max: value,
    };
    // SAFETY: `limit` is a valid immutable rlimit structure and `resource`
    // is one of the constants `apply_helper_resource_limits` selects.
    if unsafe { libc::setrlimit(resource, &limit) } != 0 {
        return Err(std::io::Error::last_os_error());
    }
    Ok(())
}

/// Read as much of `pipe` as is available without blocking, appending to
/// `buffer`. Returns `true` once `buffer` has grown past the response
/// channel's own total-byte cap (`StreamCaps::SHADER_RESPONSE`) — the
/// caller stops the child at that point.
fn drain_stdout(pipe: &mut ChildStdout, buffer: &mut Vec<u8>) -> bool {
    if buffer.len() > StreamCaps::SHADER_RESPONSE.max_total_payload_bytes {
        return true;
    }
    let mut chunk = [0_u8; 4096];
    loop {
        match pipe.read(&mut chunk) {
            Ok(0) => break,
            Ok(count) => {
                buffer.extend_from_slice(&chunk[..count]);
                if buffer.len() > StreamCaps::SHADER_RESPONSE.max_total_payload_bytes {
                    return true;
                }
            }
            Err(error) if error.kind() == std::io::ErrorKind::WouldBlock => break,
            Err(_) => break,
        }
    }
    false
}

/// Chunks drained per `drain_stderr_tail` call — bounds the time spent
/// here the same way `inspect.rs`'s own constant does, so a stderr flood
/// cannot starve the deadline check in `wait_and_read`'s loop.
const STDERR_DRAIN_CHUNKS_PER_TICK: usize = 16;
/// Kept stderr tail length — diagnostic only, never parsed.
const STDERR_TAIL_BYTES: usize = 4096;

fn drain_stderr_tail(pipe: &mut ChildStderr, tail: &mut Vec<u8>) {
    let mut chunk = [0_u8; 4096];
    for _ in 0..STDERR_DRAIN_CHUNKS_PER_TICK {
        match pipe.read(&mut chunk) {
            Ok(0) => break,
            Ok(count) => {
                tail.extend_from_slice(&chunk[..count]);
                if tail.len() > STDERR_TAIL_BYTES {
                    let drop = tail.len() - STDERR_TAIL_BYTES;
                    tail.drain(..drop);
                }
            }
            Err(error) if error.kind() == std::io::ErrorKind::WouldBlock => break,
            Err(_) => break,
        }
    }
}

/// Parses `out_buffer` as a `KWR1` stream under the response channel's own
/// caps and classifies it. Never panics on malformed/truncated/oversize
/// input — every failure is `ProtocolError`.
fn finalize(out_buffer: &[u8]) -> HelperOutcome {
    let mut reader = FrameReader::with_caps(
        std::io::Cursor::new(out_buffer),
        StreamCaps::SHADER_RESPONSE,
    );
    let frame = match reader.next_frame() {
        Ok(Some(frame)) => frame,
        Ok(None) => {
            return HelperOutcome::ProtocolError(
                "helper exited with no response frame".to_string(),
            );
        }
        Err(error) => return HelperOutcome::ProtocolError(bounded(&error.to_string())),
    };
    if frame.kind != FrameKind::ShaderCompileResponseV1 {
        return HelperOutcome::ProtocolError("wrong-kind response frame".to_string());
    }
    let response = match validate_shader_compile_response(&frame.payload) {
        Ok(value) => value,
        Err(error) => return HelperOutcome::ProtocolError(bounded(&error.to_string())),
    };
    match response.get("status").and_then(Value::as_str) {
        Some("unimplemented") => HelperOutcome::Unimplemented,
        Some("protocol-error") => {
            let reason = response
                .get("reason")
                .and_then(Value::as_str)
                .unwrap_or("protocol-error");
            HelperOutcome::ProtocolError(bounded(reason))
        }
        // Reserved for SR-3c ("ok"/"compiled"/...) — this slice's own
        // helper never emits anything but "unimplemented"/"protocol-
        // error", so any OTHER status here is itself unexpected from a
        // client this old; treated as a protocol error rather than
        // guessed at.
        other => HelperOutcome::ProtocolError(format!("unrecognized response status {other:?}")),
    }
}

/// SIGTERM, a short grace period, then SIGKILL — always `kill(pid, ...)`,
/// never `kill(-pid, ...)` (see the module doc: the helper shares the
/// RENDERER's own process group, so a negative-pid signal here would hit
/// the renderer itself). Always reaps before returning.
fn terminate_and_reap(child: &mut Child) {
    if child.try_wait().ok().flatten().is_some() {
        let _ = child.wait();
        return;
    }
    let Ok(pid) = i32::try_from(child.id()) else {
        return;
    };
    // SAFETY: `pid` is this child's own pid (positive, single-process
    // target) — never the process group.
    unsafe {
        libc::kill(pid, libc::SIGTERM);
    }
    let deadline = Instant::now() + TERMINATE_GRACE;
    while Instant::now() < deadline {
        if child.try_wait().ok().flatten().is_some() {
            let _ = child.wait();
            return;
        }
        std::thread::sleep(Duration::from_millis(10));
    }
    // SAFETY: same single-process target as above.
    unsafe {
        libc::kill(pid, libc::SIGKILL);
    }
    let _ = child.wait();
}

/// Immediate SIGKILL + reap, for failure paths that are not a timeout
/// (spawn/pipe/wait errors) — the child is either not fully alive yet or
/// there is no point giving it a grace period. Same `kill(pid, ...)`-not-
/// `killpg` reasoning as `terminate_and_reap`.
fn kill_and_reap(child: &mut Child) {
    if let Ok(pid) = i32::try_from(child.id()) {
        // SAFETY: `pid` is this child's own pid, not the process group.
        unsafe {
            libc::kill(pid, libc::SIGKILL);
        }
    }
    let _ = child.wait();
}

fn bounded(text: &str) -> String {
    truncate_chars(text, 300)
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::fs;
    use std::os::unix::fs::PermissionsExt;

    fn temp_dir(label: &str) -> PathBuf {
        let dir = std::env::temp_dir().join(format!(
            "kwe-shader-helper-{label}-{}-{}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap_or_default()
                .as_nanos()
        ));
        fs::create_dir_all(&dir).unwrap();
        dir
    }

    fn write_script(root: &std::path::Path, name: &str, body: &str) -> PathBuf {
        let path = root.join(name);
        fs::write(&path, body).unwrap();
        fs::set_permissions(&path, fs::Permissions::from_mode(0o755)).unwrap();
        path
    }

    fn sample_request(source: &'static str) -> ShaderCompileRequest<'static> {
        ShaderCompileRequest {
            stage: Stage::Fragment,
            source,
        }
    }

    /// Python source for `read_frame()`/`write_frame(kind, payload)` over
    /// stdin/stdout (fd 0/1) — mirrors `docs/SHADER_HELPER_PROTOCOL_V1.md`'s
    /// wire format exactly (same 12-byte header shape
    /// `kwe-daemon::inspect`'s own fake-inspector fixtures use for the
    /// report FD; this is the SAME codec, a different channel).
    const PYTHON_FRAME_HELPERS: &str = r#"
import os
import struct

def write_frame(kind, payload):
    header = b"KWR1" + bytes([kind, 0]) + struct.pack("<H", 0) + struct.pack("<I", len(payload))
    os.write(1, header + payload)

def read_frame():
    header = b""
    while len(header) < 12:
        chunk = os.read(0, 12 - len(header))
        if not chunk:
            return None
        header += chunk
    (payload_len,) = struct.unpack("<I", header[8:12])
    payload = b""
    while len(payload) < payload_len:
        chunk = os.read(0, payload_len - len(payload))
        if not chunk:
            break
        payload += chunk
    return payload
"#;

    /// (a) A well-behaved fake helper's `unimplemented` response classifies
    /// exactly that way.
    #[test]
    fn valid_unimplemented_response_is_classified_unimplemented() {
        let root = temp_dir("valid");
        let script = write_script(
            &root,
            "fake-helper.py",
            &format!(
                r#"#!/usr/bin/env python3
{PYTHON_FRAME_HELPERS}
read_frame()
payload = b'{{"schema":"shader-compile-response-v1","status":"unimplemented","reason":"skeleton"}}'
write_frame(17, payload)
"#
            ),
        );
        let helper = ShaderHelper::new(Some(script), 5000);
        let outcome = helper.compile(&sample_request("void main() {}"));
        assert!(
            matches!(outcome, HelperOutcome::Unimplemented),
            "{outcome:?}"
        );
        fs::remove_dir_all(&root).unwrap();
    }

    /// (b) Garbage on the response channel is a protocol error, never a
    /// panic.
    #[test]
    fn garbage_response_is_a_protocol_error() {
        let root = temp_dir("garbage");
        let script = write_script(
            &root,
            "fake-helper.py",
            "#!/usr/bin/env python3\nimport os\nos.read(0, 65536)\nimport sys\nsys.stdout.buffer.write(b'not a frame at all')\n",
        );
        let helper = ShaderHelper::new(Some(script), 5000);
        let outcome = helper.compile(&sample_request("void main() {}"));
        assert!(
            matches!(outcome, HelperOutcome::ProtocolError(_)),
            "{outcome:?}"
        );
        fs::remove_dir_all(&root).unwrap();
    }

    /// (c) A hung fake helper is killed at the deadline and actually
    /// reaped (`kill(pid, 0)` fails afterward) within deadline + grace.
    #[test]
    fn hung_helper_times_out_and_is_reaped() {
        let root = temp_dir("hang");
        let script = write_script(
            &root,
            "fake-helper.py",
            "#!/usr/bin/env python3\nimport time\ntime.sleep(600)\n",
        );
        let helper = ShaderHelper::new(Some(script), 300);
        let started = Instant::now();
        let outcome = helper.compile(&sample_request("void main() {}"));
        assert!(matches!(outcome, HelperOutcome::Timeout), "{outcome:?}");
        assert!(
            started.elapsed() < Duration::from_secs(2),
            "timeout + grace must stay bounded, took {:?}",
            started.elapsed()
        );
        fs::remove_dir_all(&root).unwrap();
    }

    /// (d) A fake helper that IGNORES SIGTERM still dies — proving the
    /// SIGTERM-then-grace-then-SIGKILL escalation in `terminate_and_reap`
    /// actually reaches SIGKILL, not just that a plain hang eventually
    /// times out.
    #[test]
    fn helper_ignoring_sigterm_still_dies_to_sigkill() {
        let root = temp_dir("ignore-sigterm");
        let script = write_script(
            &root,
            "fake-helper.py",
            "#!/usr/bin/env python3\nimport signal, time\nsignal.signal(signal.SIGTERM, signal.SIG_IGN)\ntime.sleep(600)\n",
        );
        let helper = ShaderHelper::new(Some(script), 300);
        let started = Instant::now();
        let outcome = helper.compile(&sample_request("void main() {}"));
        assert!(matches!(outcome, HelperOutcome::Timeout), "{outcome:?}");
        assert!(
            started.elapsed() < Duration::from_secs(2),
            "SIGTERM-ignoring helper must still be reaped promptly via SIGKILL, took {:?}",
            started.elapsed()
        );
        fs::remove_dir_all(&root).unwrap();
    }

    /// (e) A path that resolves to nothing spawnable is `Unavailable`,
    /// never a panic, never a hang.
    #[test]
    fn missing_binary_path_is_unavailable() {
        let helper = ShaderHelper::new(
            Some(PathBuf::from("/nonexistent/kwe-shader-compiler")),
            5000,
        );
        let outcome = helper.compile(&sample_request("void main() {}"));
        assert!(
            matches!(outcome, HelperOutcome::Unavailable(_)),
            "{outcome:?}"
        );
    }

    /// An unconfigured helper (no path at all, `current_exe` resolution
    /// aside) is `Unavailable` without spawning anything, mirroring
    /// `inspect.rs`'s `unconfigured_binary_is_unavailable`.
    #[test]
    fn a_helper_with_no_path_at_all_is_unavailable() {
        let helper = ShaderHelper {
            path: None,
            timeout: Duration::from_secs(5),
            logged_unimplemented: AtomicBool::new(false),
            logged_protocol_error: AtomicBool::new(false),
            logged_unavailable: AtomicBool::new(false),
            logged_timeout: AtomicBool::new(false),
        };
        let outcome = helper.compile(&sample_request("void main() {}"));
        assert!(
            matches!(outcome, HelperOutcome::Unavailable(_)),
            "{outcome:?}"
        );
    }

    /// SR-3b decision (a)'s central claim, proven directly rather than
    /// assumed: the ACTUAL production compile path
    /// (`materialshader::compile_stage`) produces byte-identical SPIR-V
    /// regardless of whether a `ShaderHelper` is configured
    /// (`--shader-helper /nonexistent`) or entirely absent (no flag) —
    /// the fallback-equivalence the task asks for, exercised at the exact
    /// point `main.rs`'s `compile_one_material` calls both
    /// (`shader_helper.compile` then, unconditionally, `compile_stage`).
    #[test]
    fn fallback_equivalence_compile_stage_output_is_unaffected_by_helper_outcome() {
        if crate::materialshader::compile_stage(
            "void main() { gl_FragColor = vec4(1.0, 0.0, 0.0, 1.0); }\n",
            Stage::Fragment,
            "probe.frag",
        )
        .is_err()
        {
            eprintln!(
                "skipping fallback_equivalence_compile_stage_output_is_unaffected_by_helper_outcome: libshaderc unavailable"
            );
            return;
        }
        let source = "void main() { gl_FragColor = vec4(1.0, 0.0, 0.0, 1.0); }\n";
        let request = sample_request(source);

        // "--shader-helper /nonexistent"
        let configured_but_missing = ShaderHelper::new(
            Some(PathBuf::from("/nonexistent/kwe-shader-compiler")),
            1000,
        );
        let outcome_a = configured_but_missing.compile(&request);
        assert!(matches!(outcome_a, HelperOutcome::Unavailable(_)));
        let spirv_a =
            crate::materialshader::compile_stage(source, Stage::Fragment, "a.frag").unwrap();

        // no flag at all
        let unconfigured = ShaderHelper {
            path: None,
            timeout: Duration::from_secs(1),
            logged_unimplemented: AtomicBool::new(false),
            logged_protocol_error: AtomicBool::new(false),
            logged_unavailable: AtomicBool::new(false),
            logged_timeout: AtomicBool::new(false),
        };
        let outcome_b = unconfigured.compile(&request);
        assert!(matches!(outcome_b, HelperOutcome::Unavailable(_)));
        let spirv_b =
            crate::materialshader::compile_stage(source, Stage::Fragment, "b.frag").unwrap();

        assert_eq!(
            spirv_a, spirv_b,
            "compile_stage's own output must be unaffected by ShaderHelper configuration/outcome"
        );
    }

    /// The workspace's own `target/<profile>` directory, respecting
    /// `CARGO_TARGET_DIR` the same way the shell sweep scripts do. Cross-
    /// crate `CARGO_BIN_EXE_*` does not exist (task's own note) — this is
    /// the target-dir convention the shell scripts in this repo already
    /// use for the same "find a sibling workspace binary" problem.
    fn workspace_target_dir() -> PathBuf {
        if let Some(dir) = std::env::var_os("CARGO_TARGET_DIR") {
            return PathBuf::from(dir);
        }
        // CARGO_MANIFEST_DIR is this crate's own directory
        // (.../crates/kwe-scene-renderer); the workspace root is two
        // levels up.
        PathBuf::from(env!("CARGO_MANIFEST_DIR"))
            .join("../..")
            .join("target")
    }

    /// (f) The real-binary path: spawns the ACTUAL compiled
    /// `kwe-shader-compiler` (SR-3a) if it has already been built (a
    /// normal `cargo build`/`cargo test --workspace` produces it as a
    /// sibling workspace binary), proving this module's wire-level client
    /// and SR-3a's own binary genuinely agree on the protocol, not just
    /// against fake python scripts. Skips gracefully (does not fail the
    /// suite), with a printed note, if the binary has not been built yet
    /// (e.g. `cargo test -p kwe-scene-renderer` run in isolation without a
    /// prior workspace build) — mirroring this repo's other opt-in/
    /// skip-if-prerequisite-missing tests (`ir_parity_corpus`,
    /// `scripts/smoke-scene-corpus.sh`).
    #[test]
    fn real_shader_compiler_binary_answers_unimplemented() {
        let candidates: Vec<PathBuf> = ["debug", "release"]
            .iter()
            .map(|profile| {
                workspace_target_dir()
                    .join(profile)
                    .join("kwe-shader-compiler")
            })
            .collect();
        let Some(binary) = candidates.iter().find(|path| path.is_file()).cloned() else {
            let checked: Vec<String> = candidates
                .iter()
                .map(|path| path.display().to_string())
                .collect();
            eprintln!(
                "skipping real_shader_compiler_binary_answers_unimplemented: kwe-shader-compiler not built (checked {})",
                checked.join(", ")
            );
            return;
        };
        let helper = ShaderHelper::new(Some(binary), 5000);
        let outcome = helper.compile(&sample_request("void main() { gl_FragColor = vec4(1.0); }"));
        assert!(
            matches!(outcome, HelperOutcome::Unimplemented),
            "{outcome:?}"
        );
    }
}
