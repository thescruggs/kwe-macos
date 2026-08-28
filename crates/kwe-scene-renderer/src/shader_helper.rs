// SPDX-License-Identifier: GPL-3.0-or-later
//! SR-3b built the renderer-side client for the killable shader-compile
//! helper (`kwe-shader-compiler`); SR-3c makes it real
//! (`docs/SHADER_HELPER_PROTOCOL_V1.md`).
//!
//! **SR-3b (zero behavior change):** `compile` was called, its outcome
//! logged, and the caller fell through to the in-thread
//! `materialshader::compile_stage` UNCONDITIONALLY — the helper's own
//! skeleton never emitted anything but `unimplemented`/`protocol-error`,
//! so there was nothing else to consume.
//!
//! **SR-3c (this slice): the helper now really compiles.** `ShaderHelper::
//! compile_stage_or_fallback` is the new entry point `main.rs`'s
//! `compile_one_material` calls instead of `compile` directly — it
//! consumes `HelperOutcome::Compiled` (use the helper's SPIR-V, skip the
//! in-thread compile entirely: decision (b)'s payoff, a killable process
//! bounds the compile instead of a detached thread inside the renderer)
//! and `HelperOutcome::CompileError` (surface it through the SAME error
//! path `compile_stage`'s own `Err` already takes — task text: "a compile
//! error is a RESULT, not a helper failure", so this does NOT retry
//! in-thread; the GLSL is the GLSL, a second compile of the identical
//! source would fail identically). Every OTHER outcome
//! (`Unimplemented`/`ProtocolError`/`Unavailable`/`Timeout` — helper not
//! configured, helper crashed, protocol violated, deadline blown) still
//! falls through to the in-thread `compile_stage`, unchanged from SR-3b.
//!
//! Byte-identity between the two compile paths (decision (a): same
//! `shaderc`, same `kwe-core::shader_compile_spec` options recipe on both
//! ends) is proven directly by this module's own differential-oracle
//! tests against the REAL helper binary, not assumed.
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
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
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

/// The result of one helper exchange.
#[derive(Debug)]
pub enum HelperOutcome {
    /// The helper answered with `status: "unimplemented"` — SR-3a's
    /// skeleton always does this for a structurally valid request; kept
    /// as a real possible outcome (not removed in SR-3c) since an OLDER
    /// helper binary lagging a renderer upgrade would still answer this
    /// way, and `--shader-helper` can point at any binary on disk.
    Unimplemented,
    /// The exchange completed, but the response was not a well-formed
    /// `shader-compile-response-v1` frame, OR the helper itself reported
    /// `status: "protocol-error"`, OR (SR-3c) an `"ok"` response's
    /// declared kind-18 chunk count/total bytes did not match what was
    /// actually read back. Bounded diagnostic detail.
    ProtocolError(String),
    /// The helper could not be reached at all: not configured (no path
    /// resolved), spawn failed (missing binary, permission), or the
    /// exchange failed at the pipe/wait level. Bounded diagnostic detail.
    Unavailable(String),
    /// No response arrived within `--shader-helper-timeout-ms`; the child
    /// was killed (SIGTERM then, after a short grace, SIGKILL) and reaped.
    Timeout,
    /// SR-3c: `status: "ok"` — the helper compiled the requested stage.
    /// `spirv` is the reassembled SPIR-V, byte-identical (differential-
    /// oracle tests below) to what `materialshader::compile_stage` would
    /// have produced for the same source. `response` is the validated
    /// kind-17 header JSON (`spirv_chunks`/`spirv_total_bytes`), kept for
    /// a future caller that wants it; `compile_stage_or_fallback` itself
    /// only needs `spirv`.
    Compiled {
        spirv: Vec<u32>,
        #[allow(dead_code)]
        response: Value,
    },
    /// SR-3c: `status: "compile-error"` — the helper's `shaderc` call
    /// itself failed (bad GLSL). A compile error is a RESULT, not a
    /// helper failure (task text): `compile_stage_or_fallback` surfaces
    /// this through the exact same error path an in-thread
    /// `CompileError::Failed` already takes, WITHOUT retrying in-thread —
    /// the same source recompiled a second time fails identically.
    /// Bounded diagnostic detail (the helper's own `"log"`, already
    /// truncated to `MAX_SHADER_COMPILE_ERROR_LOG_BYTES` on the wire).
    CompileError(String),
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
            Self::CompileError(_) => "compile_error",
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
    // SR-3c task item 3: a grep-able `event=shader_helper.compiled
    // count=<n>` line emitted ONCE at process teardown (`Drop`, below),
    // only when count > 0 — evidence smoke-scene.sh actually exercised
    // the real compile path, not just the fallback. A per-process running
    // total, not per-material, so it costs one line regardless of scene
    // size (kept permanently past this slice: a standing signal of
    // helper effectiveness is worth more than the log-storm risk a
    // per-compile line would have, and there is exactly one of these per
    // renderer process lifetime).
    compiled_count: AtomicU64,
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
            compiled_count: AtomicU64::new(0),
        }
    }

    /// SR-3c: the entry point `main.rs`'s `compile_one_material` calls
    /// instead of `compile_stage` directly. Tries the helper first
    /// (`compile`); `Compiled`/`CompileError` are CONSUMED here (skip or
    /// short-circuit the in-thread path — see the module doc); every
    /// other outcome falls through to the in-thread `materialshader::
    /// compile_stage`, unchanged from SR-3b. Returns the exact same
    /// `Result<Vec<u32>, materialshader::CompileError>` shape
    /// `compile_stage` itself returns, so the call site's own error
    /// handling (fallback-reason bump, bounded diagnostic, `return None`)
    /// needs no change beyond calling this instead.
    pub fn compile_stage_or_fallback(
        &self,
        request: &ShaderCompileRequest<'_>,
        label: &str,
    ) -> Result<Vec<u32>, crate::materialshader::CompileError> {
        match self.compile(request) {
            HelperOutcome::Compiled { spirv, .. } => Ok(spirv),
            HelperOutcome::CompileError(log) => {
                Err(crate::materialshader::CompileError::Failed(log))
            }
            HelperOutcome::Unimplemented
            | HelperOutcome::ProtocolError(_)
            | HelperOutcome::Unavailable(_)
            | HelperOutcome::Timeout => {
                crate::materialshader::compile_stage(request.source, request.stage, label)
            }
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
            // SR-3c task item 2: populate `options` from the SAME values
            // `compile_stage` uses (decision (a)) — `kwe-core::
            // shader_compile_spec`'s constants, the single shared recipe.
            // The helper does not actually parse these back into shaderc
            // types (it always compiles with its OWN copy of the same
            // constants — see `kwe-shader-compiler`'s own doc comment);
            // this field exists so the wire request is self-describing/
            // auditable, per the task's explicit ask, not because the
            // helper's compile behavior depends on it today.
            "options": {
                "target_env": kwe_core::TARGET_ENV,
                "target_env_version": kwe_core::TARGET_ENV_VERSION,
                "optimization_level": kwe_core::OPTIMIZATION_LEVEL,
            },
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
            // A success needs no diagnostic of its own (the teardown
            // `compiled_count` summary below covers it, once per
            // process). A compile error already gets its OWN per-
            // material diagnostic at the call site
            // (`event=renderer.scene.shader_compile_error`, main.rs) once
            // `compile_stage_or_fallback` maps it to `CompileError::
            // Failed` — logging it AGAIN here would be a duplicate line
            // per material, not a dedup.
            HelperOutcome::Compiled { .. } | HelperOutcome::CompileError(_) => {
                if matches!(outcome, HelperOutcome::Compiled { .. }) {
                    self.compiled_count.fetch_add(1, Ordering::Relaxed);
                }
                return;
            }
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

/// SR-3c task item 3's teardown evidence line: `event=shader_helper.
/// compiled count=<n>`, emitted once when this `ShaderHelper` (one per
/// renderer process, constructed once in `main()`) is dropped, and only
/// when `count > 0` — a scene with no material shaders, or one where the
/// helper never actually succeeded, prints nothing. Bounded (one line,
/// one integer) regardless of scene size.
impl Drop for ShaderHelper {
    fn drop(&mut self) {
        let count = self.compiled_count.load(Ordering::Relaxed);
        if count > 0 {
            eprintln!("event=shader_helper.compiled count={count}");
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
        // `validate_shader_compile_response` already rejects an "ok"
        // response's over-claimed `spirv_chunks`/`spirv_total_bytes`
        // (`MAX_SPIRV_CHUNKS`/`MAX_SPIRV_TOTAL_BYTES`) right here, before
        // this function ever tries to read that many kind-18 frames — a
        // dishonest/buggy helper claiming e.g. 200 chunks is refused at
        // this point, not discovered partway through reassembly below.
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
        Some("compile-error") => {
            let log = response
                .get("log")
                .and_then(Value::as_str)
                .unwrap_or("compile-error");
            HelperOutcome::CompileError(bounded(log))
        }
        Some("ok") => reassemble_ok(&mut reader, response),
        other => HelperOutcome::ProtocolError(format!("unrecognized response status {other:?}")),
    }
}

/// SR-3c: reads the `spirv_chunks` kind-18 frames the "ok" header
/// declared, reassembles the raw SPIR-V bytes, and validates BOTH the
/// count and total length actually read back against what the header
/// claimed (task item 2) — a mismatch either way (fewer chunks than
/// declared because the helper died mid-stream, a wrong total, or extra
/// trailing frames) is `ProtocolError`, never a panic and never a
/// silently-wrong SPIR-V blob.
fn reassemble_ok(
    reader: &mut FrameReader<std::io::Cursor<&[u8]>>,
    response: Value,
) -> HelperOutcome {
    let declared_chunks = response.get("spirv_chunks").and_then(Value::as_u64);
    let declared_total = response.get("spirv_total_bytes").and_then(Value::as_u64);
    let (Some(declared_chunks), Some(declared_total)) = (declared_chunks, declared_total) else {
        // Already validated present/typed by `validate_shader_compile_response`
        // -- unreachable in practice, kept as a typed fallback rather than
        // an `unwrap`/panic.
        return HelperOutcome::ProtocolError("ok response missing spirv fields".to_string());
    };

    let mut spirv_bytes: Vec<u8> = Vec::new();
    for index in 0..declared_chunks {
        match reader.next_frame() {
            Ok(Some(frame)) if frame.kind == FrameKind::SpirvChunkV1 => {
                spirv_bytes.extend_from_slice(&frame.payload);
            }
            Ok(Some(frame)) => {
                return HelperOutcome::ProtocolError(format!(
                    "expected a spirv chunk frame, got kind {:?} at index {index}",
                    frame.kind
                ));
            }
            Ok(None) => {
                return HelperOutcome::ProtocolError(format!(
                    "helper declared {declared_chunks} spirv chunks but stopped after {index}"
                ));
            }
            Err(error) => return HelperOutcome::ProtocolError(bounded(&error.to_string())),
        }
    }
    // No trailing frames beyond exactly the declared chunk count.
    match reader.next_frame() {
        Ok(None) => {}
        Ok(Some(_)) => {
            return HelperOutcome::ProtocolError(
                "trailing frame(s) after the declared spirv chunk count".to_string(),
            );
        }
        Err(error) => return HelperOutcome::ProtocolError(bounded(&error.to_string())),
    }

    if spirv_bytes.len() as u64 != declared_total {
        return HelperOutcome::ProtocolError(format!(
            "spirv_total_bytes header said {declared_total}, reassembled {} bytes",
            spirv_bytes.len()
        ));
    }
    if !spirv_bytes.len().is_multiple_of(4) {
        return HelperOutcome::ProtocolError(
            "reassembled spirv byte length is not a multiple of 4".to_string(),
        );
    }

    // Native-endian: both processes always run on the same host/
    // architecture (the helper is a child of THIS renderer process), so
    // this matches `shaderc::CompilationArtifact::as_binary_u8`'s own
    // native layout byte-for-byte, the same as
    // `materialshader::compile_stage`'s own `Vec<u32>` — no conversion
    // needed, see the module doc.
    let spirv: Vec<u32> = spirv_bytes
        .chunks_exact(4)
        .map(|word| u32::from_ne_bytes([word[0], word[1], word[2], word[3]]))
        .collect();

    HelperOutcome::Compiled { spirv, response }
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
            compiled_count: AtomicU64::new(0),
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
            compiled_count: AtomicU64::new(0),
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

    /// The real, built `kwe-shader-compiler` binary path, or `None` with a
    /// printed skip note if it has not been built yet (e.g. `cargo test -p
    /// kwe-scene-renderer` run in isolation without a prior workspace
    /// build) — mirroring this repo's other opt-in/skip-if-prerequisite-
    /// missing tests (`ir_parity_corpus`, `scripts/smoke-scene-corpus.sh`).
    /// `caller`: the test's own name, for the skip message.
    fn resolve_real_helper_binary(caller: &str) -> Option<PathBuf> {
        let candidates: Vec<PathBuf> = ["debug", "release"]
            .iter()
            .map(|profile| {
                workspace_target_dir()
                    .join(profile)
                    .join("kwe-shader-compiler")
            })
            .collect();
        let found = candidates.iter().find(|path| path.is_file()).cloned();
        if found.is_none() {
            let checked: Vec<String> = candidates
                .iter()
                .map(|path| path.display().to_string())
                .collect();
            eprintln!(
                "skipping {caller}: kwe-shader-compiler not built (checked {})",
                checked.join(", ")
            );
        }
        found
    }

    /// SR-3c task item 3: for each representative shader below, compile
    /// the SAME preprocessed source through BOTH paths — the REAL helper
    /// binary (proving the wire round trip, not just `compile_source`'s
    /// own Rust function in isolation) and the in-thread
    /// `materialshader::compile_stage` — and assert byte-identical
    /// SPIR-V. This is decision (a)'s central claim, proven directly:
    /// same `shaderc`, same `kwe-core::shader_compile_spec` options
    /// recipe on both ends, not assumed from reading the code.
    ///
    /// Each source is run through the REAL `shaderpre::preprocess`
    /// pipeline first (like `materialshader::tests::
    /// compile_round_trip_produces_spirv` and the S2-era shaderpre tests
    /// this reuses the shape of) — a raw, un-preprocessed source (e.g.
    /// bare `void main() {}`) lacks the `#version` directive `shaderpre`
    /// injects via `SHADER_HEADER`, which a Vulkan-targeted `shaderc`
    /// compile requires; preprocessed sources are what `compile_stage`
    /// (and now the helper) actually ever see in production.
    #[test]
    fn real_helper_binary_produces_byte_identical_spirv_to_in_thread_compile() {
        let Some(binary) = resolve_real_helper_binary(
            "real_helper_binary_produces_byte_identical_spirv_to_in_thread_compile",
        ) else {
            return;
        };
        if crate::materialshader::compile_stage(
            "void main() { gl_FragColor = vec4(1.0); }",
            Stage::Fragment,
            "probe.frag",
        )
        .is_err()
        {
            eprintln!(
                "skipping real_helper_binary_produces_byte_identical_spirv_to_in_thread_compile: libshaderc unavailable"
            );
            return;
        }
        let helper = ShaderHelper::new(Some(binary), 5000);

        for (label, stage, source, combos, include_lookup) in representative_shaders() {
            let mut include = include_lookup;
            let mut locs = std::collections::BTreeMap::new();
            let shaderpre_stage = match stage {
                Stage::Vertex => crate::shaderpre::Stage::Vertex,
                Stage::Fragment => crate::shaderpre::Stage::Fragment,
            };
            let preprocessed = crate::shaderpre::preprocess(
                shaderpre_stage,
                label,
                source,
                &combos,
                &[],
                &mut locs,
                &mut include,
            )
            .unwrap_or_else(|error| panic!("{label}: preprocess failed: {error:?}"));

            let request = ShaderCompileRequest {
                stage,
                source: &preprocessed.source,
            };
            let outcome = helper.compile(&request);
            let HelperOutcome::Compiled {
                spirv: helper_spirv,
                ..
            } = outcome
            else {
                panic!("{label}: expected Compiled from the real helper, got {outcome:?}");
            };
            let in_thread_spirv =
                crate::materialshader::compile_stage(&preprocessed.source, stage, label)
                    .unwrap_or_else(|error| panic!("{label}: in-thread compile failed: {error:?}"));
            assert_eq!(
                helper_spirv, in_thread_spirv,
                "{label}: helper and in-thread SPIR-V diverged"
            );
        }
    }

    /// The four representative shaders task item 3 names, each as
    /// `(label, stage, raw_source, material_combos, include_lookup)`
    /// ready for `shaderpre::preprocess`: (1) a plain quad fragment
    /// shader (`materialshader::tests::compile_round_trip_produces_spirv`'s
    /// own source), (2) one with a combo/define that changes which branch
    /// compiles (`shaderpre::tests::
    /// material_combo_override_wins_over_shader_default`'s pattern), (3)
    /// one with an `#include` spliced in (`shaderpre::tests::
    /// include_resolves_and_inlines`'s pattern, made load-bearing: the
    /// included function is actually called), (4) a vertex-stage shader
    /// (`shaderpre::tests::vertex_shader_is_not_wrapped_for_premultiplication`'s
    /// source).
    #[allow(clippy::type_complexity)]
    fn representative_shaders() -> Vec<(
        &'static str,
        Stage,
        &'static str,
        std::collections::BTreeMap<String, i64>,
        Box<crate::shaderpre::IncludeLookup<'static>>,
    )> {
        vec![
            (
                "plain_quad.frag",
                Stage::Fragment,
                "void main() { gl_FragColor = vec4(1.0, 0.0, 0.0, 1.0); }\n",
                std::collections::BTreeMap::new(),
                Box::new(|_: &str| None),
            ),
            (
                "combo.frag",
                Stage::Fragment,
                "// [COMBO] {\"combo\":\"LIGHTING\",\"default\":0}\n#if LIGHTING\nvoid main() { gl_FragColor = vec4(1.0, 1.0, 0.0, 1.0); }\n#else\nvoid main() { gl_FragColor = vec4(0.0, 0.0, 1.0, 1.0); }\n#endif\n",
                {
                    let mut combos = std::collections::BTreeMap::new();
                    combos.insert("LIGHTING".to_string(), 1);
                    combos
                },
                Box::new(|_: &str| None),
            ),
            (
                "include.frag",
                Stage::Fragment,
                "#include \"common.h\"\nvoid main() { gl_FragColor = helper(); }\n",
                std::collections::BTreeMap::new(),
                Box::new(|name: &str| {
                    if name == "common.h" {
                        Some(b"vec4 helper() { return vec4(0.5, 0.5, 0.5, 1.0); }\n".to_vec())
                    } else {
                        None
                    }
                }),
            ),
            (
                "plain_quad.vert",
                Stage::Vertex,
                "void main() { gl_Position = vec4(0.0); }\n",
                std::collections::BTreeMap::new(),
                Box::new(|_: &str| None),
            ),
        ]
    }

    /// SR-3c task item 4: bad GLSL through the REAL helper produces
    /// `HelperOutcome::CompileError`, and `compile_stage_or_fallback`
    /// surfaces it through the exact same `Err` path an in-thread
    /// `CompileError::Failed` already takes — same fallback_reasons bump/
    /// bounded-diagnostic/`return None` at the `main.rs` call site,
    /// proven here at the level this module owns: the `Result` shape
    /// itself, both variants classified the same way by the caller.
    #[test]
    fn helper_compile_error_surfaces_as_the_same_compile_stage_error_shape() {
        let Some(binary) = resolve_real_helper_binary(
            "helper_compile_error_surfaces_as_the_same_compile_stage_error_shape",
        ) else {
            return;
        };
        let helper = ShaderHelper::new(Some(binary), 5000);
        let request = ShaderCompileRequest {
            stage: Stage::Fragment,
            source: "#version 450\nvoid main() { this is not valid glsl !!! }\n",
        };
        let result = helper.compile_stage_or_fallback(&request, "bad.frag");
        assert!(
            matches!(result, Err(crate::materialshader::CompileError::Failed(_))),
            "{result:?}"
        );
    }

    /// SR-3c task item 4: a helper that answers with a well-formed "ok"
    /// header but is KILLED before writing its declared kind-18 chunks
    /// (simulated here: a fake helper that writes the header then exits
    /// without ever writing the chunk) is a `ProtocolError`, and
    /// `compile_stage_or_fallback` falls back in-thread — the layer still
    /// draws (a real compile succeeds), it just did not come from the
    /// helper this time.
    #[test]
    fn helper_that_dies_mid_chunks_is_a_protocol_error_and_falls_back_in_thread() {
        if crate::materialshader::compile_stage(
            "void main() { gl_FragColor = vec4(1.0); }",
            Stage::Fragment,
            "probe.frag",
        )
        .is_err()
        {
            eprintln!(
                "skipping helper_that_dies_mid_chunks_is_a_protocol_error_and_falls_back_in_thread: libshaderc unavailable"
            );
            return;
        }
        let root = temp_dir("dies-mid-chunks");
        let script = write_script(
            &root,
            "fake-helper.py",
            &format!(
                r#"#!/usr/bin/env python3
{PYTHON_FRAME_HELPERS}
read_frame()
# Declares 2 spirv chunks but writes none -- a helper that died partway
# through streaming its own response.
payload = b'{{"schema":"shader-compile-response-v1","status":"ok","spirv_chunks":2,"spirv_total_bytes":16}}'
write_frame(17, payload)
"#
            ),
        );
        let helper = ShaderHelper::new(Some(script), 5000);
        let source = "void main() { gl_FragColor = vec4(1.0, 0.0, 0.0, 1.0); }\n";
        let outcome = helper.compile(&sample_request(source));
        assert!(
            matches!(outcome, HelperOutcome::ProtocolError(_)),
            "{outcome:?}"
        );
        // The full pipeline still produces a real compile via fallback.
        let result = helper.compile_stage_or_fallback(&sample_request(source), "fallback.frag");
        assert!(result.is_ok(), "{result:?}");
        fs::remove_dir_all(&root).unwrap();
    }

    /// SR-3c task item 4: an "ok" header that claims an oversized SPIR-V
    /// (e.g. 200 chunks, past `MAX_SPIRV_CHUNKS`) is refused by
    /// `validate_shader_compile_response`'s own cap BEFORE this module
    /// tries to read that many kind-18 frames — `ProtocolError`, and the
    /// full pipeline still falls back in-thread.
    #[test]
    fn oversized_spirv_chunk_claim_is_refused_by_the_response_cap() {
        if crate::materialshader::compile_stage(
            "void main() { gl_FragColor = vec4(1.0); }",
            Stage::Fragment,
            "probe.frag",
        )
        .is_err()
        {
            eprintln!(
                "skipping oversized_spirv_chunk_claim_is_refused_by_the_response_cap: libshaderc unavailable"
            );
            return;
        }
        let root = temp_dir("oversized-chunks");
        let script = write_script(
            &root,
            "fake-helper.py",
            &format!(
                r#"#!/usr/bin/env python3
{PYTHON_FRAME_HELPERS}
read_frame()
payload = b'{{"schema":"shader-compile-response-v1","status":"ok","spirv_chunks":200,"spirv_total_bytes":1000000}}'
write_frame(17, payload)
"#
            ),
        );
        let helper = ShaderHelper::new(Some(script), 5000);
        let source = "void main() { gl_FragColor = vec4(1.0, 0.0, 0.0, 1.0); }\n";
        let outcome = helper.compile(&sample_request(source));
        assert!(
            matches!(outcome, HelperOutcome::ProtocolError(_)),
            "{outcome:?}"
        );
        let result = helper.compile_stage_or_fallback(&sample_request(source), "fallback.frag");
        assert!(result.is_ok(), "{result:?}");
        fs::remove_dir_all(&root).unwrap();
    }
}
