// SPDX-License-Identifier: GPL-3.0-or-later
//! SR-3a built this binary's PROTOCOL SKELETON; SR-3c makes it actually
//! compile (plan §4.3/§8 SR-3). This binary reads exactly ONE
//! `shader-compile-request-v1` frame (kind 16) off stdin, writes exactly
//! one `shader-compile-response-v1` frame (kind 17) — followed, for an
//! `"ok"` result, by that many `spirv-chunk-v1` (kind 18) raw-binary
//! frames — to stdout, and exits: decision (c) (SR-3a), still true:
//! "one serial request per helper PROCESS". A long-lived serial-loop mode
//! (reusing one process for many requests) remains an explicit OPEN
//! QUESTION, now for whenever a later slice actually MEASURES real spawn
//! cost against real compilation latency (`docs/SR3.md`'s SR-3c open
//! risks) — SR-3c itself does not build it, only makes the per-process
//! work real.
//!
//! SR-3c decision (a): compiles with the SAME `shaderc` crate/version
//! `kwe-scene-renderer` uses (workspace-pinned), invoked with the exact
//! same `CompileOptions` recipe — shared via `kwe-core::
//! shader_compile_spec`'s plain constants (no `shaderc` type lives in
//! `kwe-core` itself; see that module's own doc comment for why). Decision
//! (b)'s payoff: unlike `materialshader::compile_stage`'s own in-thread
//! path, THIS process needs no internal timeout/thread-spawn wrapper
//! around the `shaderc` call — a compile that hangs or otherwise
//! misbehaves is bounded by the CALLER's own process-level kill
//! (`kwe-scene-renderer::shader_helper`'s `--shader-helper-timeout-ms`),
//! not by anything this binary does to itself. A GLSL compile FAILURE
//! (bad shader source) is a normal, expected outcome — `status:
//! "compile-error"`, exit 0 — never treated as a helper/protocol failure
//! (task text: "a compile error is a RESULT, not a helper failure").
//!
//! Wire contract: `docs/SHADER_HELPER_PROTOCOL_V1.md`. Containment/bounds
//! model (deadline, byte caps) deliberately mirrors
//! `kwe-scene-inspector`'s own self-watchdog + bounded-read shape (SR-0b),
//! adapted to a request/response exchange instead of a one-shot file scan.

use std::io::{self, Read, Write};
use std::time::{Duration, Instant};

use kwe_report_protocol::{
    FrameError, FrameKind, FrameReader, MAX_PAYLOAD_BYTES, MAX_SHADER_COMPILE_ERROR_LOG_BYTES,
    SHADER_COMPILE_RESPONSE_SCHEMA, ShaderRequestError, StreamCaps,
    validate_shader_compile_request, write_frame,
};

/// Process exit codes. Distinct from each other (and from a plain
/// panic/segfault's own OS-assigned codes) so a future daemon-side caller
/// (SR-3b) can classify a dead helper without parsing stderr.
mod exit_code {
    /// Successful exchange: one valid request read, one response written.
    pub const OK: i32 = 0;
    /// Malformed command-line invocation (daemon-controlled in practice;
    /// this is a defensive/test-only path, not a wire-protocol outcome).
    pub const BAD_ARGUMENTS: i32 = 2;
    /// Self-watchdog deadline expired while waiting for/reading a frame.
    /// Exits SILENTLY — no response frame is attempted. The CALLER-side
    /// kill (`kwe-scene-renderer::shader_helper`, SR-3b) is the
    /// AUTHORITATIVE bound; this watchdog is a soft backstop only — see
    /// `DeadlineReader`'s doc comment for exactly what it can and cannot
    /// preempt.
    pub const WATCHDOG_EXPIRED: i32 = 64;
    /// A protocol violation: the first frame is not kind 16, a frame is
    /// malformed/oversize/exceeds the stream caps, the request JSON fails
    /// structural validation, or bytes remain on stdin after the one
    /// request this process reads (decision (c): a second request is
    /// treated as a violation, not silently ignored — the stricter of the
    /// two options the task named). A best-effort kind-17
    /// `{"status":"protocol-error",...}` response is written first when
    /// possible.
    pub const PROTOCOL_ERROR: i32 = 65;
    /// Clean EOF with zero bytes read: nothing was ever sent, so there is
    /// nothing to respond to.
    pub const NO_REQUEST: i32 = 66;
}

const DEFAULT_MAX_WALL_MS: u64 = 10_000;
/// Today's real shader-source cap (`kwe-scene-renderer`'s own material
/// shader containment) — the same number, not a new one.
const DEFAULT_MAX_SOURCE_BYTES: usize = 256 * 1024;

#[derive(Debug)]
struct Arguments {
    max_wall_ms: u64,
    max_source_bytes: usize,
}

impl Default for Arguments {
    fn default() -> Self {
        Self {
            max_wall_ms: DEFAULT_MAX_WALL_MS,
            max_source_bytes: DEFAULT_MAX_SOURCE_BYTES,
        }
    }
}

/// Hand-rolled flag parsing (no `clap`, per the task's dependency-light
/// decision) — two flags only, `--max-wall-ms <n>` and
/// `--max-source-bytes <n>`, both `u64`/`usize` values.
fn parse_arguments(mut args: impl Iterator<Item = String>) -> Result<Arguments, String> {
    let mut arguments = Arguments::default();
    while let Some(flag) = args.next() {
        match flag.as_str() {
            "--max-wall-ms" => {
                let value = args.next().ok_or("--max-wall-ms requires a value")?;
                arguments.max_wall_ms = value
                    .parse()
                    .map_err(|_| format!("--max-wall-ms: invalid number {value:?}"))?;
            }
            "--max-source-bytes" => {
                let value = args.next().ok_or("--max-source-bytes requires a value")?;
                arguments.max_source_bytes = value
                    .parse()
                    .map_err(|_| format!("--max-source-bytes: invalid number {value:?}"))?;
            }
            other => return Err(format!("unrecognized argument {other:?}")),
        }
    }
    Ok(arguments)
}

/// Wraps a `Read` so every call first checks a wall-clock deadline BEFORE
/// delegating to the inner reader — the "checked between reads" watchdog
/// decision (c) names.
///
/// This can only ever preempt a read that has not yet STARTED blocking. A
/// `read()` call already blocked inside the OS (an empty pipe with no
/// writer, or a writer that stalls mid-frame) keeps blocking past the
/// deadline regardless — this wrapper has no way to interrupt a syscall
/// already in flight without a second thread/signal this skeleton
/// deliberately does not add (the daemon's own process-level kill, SR-3b,
/// is what makes that bound airtight; this is documented as a soft
/// backstop, not a guarantee, matching the inspector's own watchdog
/// caveat).
struct DeadlineReader<R: Read> {
    inner: R,
    deadline: Instant,
}

impl<R: Read> DeadlineReader<R> {
    fn new(inner: R, deadline: Instant) -> Self {
        Self { inner, deadline }
    }
}

impl<R: Read> Read for DeadlineReader<R> {
    fn read(&mut self, buf: &mut [u8]) -> io::Result<usize> {
        if Instant::now() >= self.deadline {
            return Err(io::Error::new(
                io::ErrorKind::TimedOut,
                "shader helper watchdog deadline expired",
            ));
        }
        self.inner.read(buf)
    }
}

fn is_watchdog_timeout(error: &FrameError) -> bool {
    matches!(error, FrameError::Io(io_error) if io_error.kind() == io::ErrorKind::TimedOut)
}

/// A short, bounded diagnostic code for a wire-level `FrameError` — never
/// echoes attacker-controlled bytes (every `FrameError` variant's own
/// payload is either absent or a small bounded number).
fn reason_for_frame_error(error: &FrameError) -> String {
    match error {
        FrameError::BadMagic => "bad-magic".to_string(),
        FrameError::BadFlags(_) => "bad-flags".to_string(),
        FrameError::BadReserved(_) => "bad-reserved".to_string(),
        FrameError::PayloadOversize { .. } => "payload-oversize".to_string(),
        FrameError::TruncatedHeader => "truncated-header".to_string(),
        FrameError::TruncatedPayload => "truncated-payload".to_string(),
        FrameError::FrameCountExceeded { .. } => "frame-count-exceeded".to_string(),
        FrameError::TotalBytesExceeded { .. } => "total-bytes-exceeded".to_string(),
        FrameError::Io(_) => "io-error".to_string(),
    }
}

/// Truncates `text` to at most `max_bytes`, on a UTF-8 char boundary (a
/// plain byte-index slice can panic mid-character) — used so an
/// attacker-influenced string (a JSON object key) can never make a
/// diagnostic line unbounded OR panic the process while building one.
fn bounded(text: &str, max_bytes: usize) -> &str {
    if text.len() <= max_bytes {
        return text;
    }
    let mut end = max_bytes;
    while end > 0 && !text.is_char_boundary(end) {
        end -= 1;
    }
    &text[..end]
}

/// A short, bounded diagnostic code for a `ShaderRequestError`.
/// `InvalidInclude` is the one variant that carries a caller-supplied
/// string (a JSON object key); it is truncated defensively even though
/// the request's own 64 KiB single-frame cap already bounds it well under
/// any reasonable diagnostic line length.
fn reason_for_request_error(error: &ShaderRequestError) -> String {
    match error {
        ShaderRequestError::Parse(_) => "malformed-json".to_string(),
        ShaderRequestError::NotAnObject => "not-an-object".to_string(),
        ShaderRequestError::WrongSchema => "wrong-schema".to_string(),
        ShaderRequestError::MissingField(field) => format!("missing-field:{field}"),
        ShaderRequestError::WrongType(field) => format!("wrong-type:{field}"),
        ShaderRequestError::InvalidStage => "invalid-stage".to_string(),
        ShaderRequestError::SourceOversize { .. } => "source-oversize".to_string(),
        ShaderRequestError::TooManyIncludes => "too-many-includes".to_string(),
        ShaderRequestError::InvalidInclude(name) => {
            format!("invalid-include:{}", bounded(name, 128))
        }
        ShaderRequestError::TooManyCombos => "too-many-combos".to_string(),
        ShaderRequestError::TooManyDefines => "too-many-defines".to_string(),
        ShaderRequestError::OptionOversize(field) => format!("option-oversize:{field}"),
    }
}

/// Writes one kind-17 response frame to stdout, best-effort — a write
/// failure (e.g. the caller already closed its read end) is swallowed:
/// this process is exiting either way, and there is nothing else useful
/// to do about a broken pipe on the way out.
fn write_response_best_effort(payload: &serde_json::Value) {
    let Ok(bytes) = serde_json::to_vec(payload) else {
        return;
    };
    let stdout = io::stdout();
    let mut handle = stdout.lock();
    let _ = write_frame(&mut handle, FrameKind::ShaderCompileResponseV1, &bytes);
    let _ = handle.flush();
}

fn respond_protocol_error(reason: &str) -> i32 {
    write_response_best_effort(&serde_json::json!({
        "schema": SHADER_COMPILE_RESPONSE_SCHEMA,
        "status": "protocol-error",
        "reason": reason,
    }));
    eprintln!("event=shader_compiler.protocol_error reason={reason}");
    exit_code::PROTOCOL_ERROR
}

/// SR-3c2: the wire `"options"` sub-fields resolved to REAL `shaderc`
/// values -- `resolve_wire_options` builds this from the (already
/// structurally-validated) request, falling back per-field to `kwe-core::
/// shader_compile_spec`'s own constant for whatever is absent.
struct ResolvedOptions {
    target_env: shaderc::TargetEnv,
    target_env_version: shaderc::EnvVersion,
    optimization_level: shaderc::OptimizationLevel,
}

/// Resolves the wire request's optional `"options"` object (already
/// shape-checked by `validate_shader_compile_request` -- each PRESENT
/// sub-field is a bounded string, but its VALUE is unchecked coming in)
/// against the vocabulary this crate actually supports. `wire_options` is
/// `None` when the whole `"options"` key was absent from the request;
/// `Some` may itself have any subset of the three sub-fields present
/// (SR-3c2: no longer all-or-nothing). Each ABSENT field -- the whole
/// object, or one field within a present object -- falls back to
/// `kwe-core::shader_compile_spec`'s own constant, byte-compatible with
/// every caller/test that predates this slice and never populated
/// `"options"` at all (`compile_source_compiles_valid_glsl_to_spirv`
/// below, and every SR-3c integration test). A PRESENT value outside the
/// known vocabulary (this crate only ever targets Vulkan; the version
/// must be one of `shaderc::EnvVersion`'s own `Vulkan1_0..=Vulkan1_4`
/// variants; the optimization level must be one of `shaderc::
/// OptimizationLevel`'s three) is `Err(())` -- `run` turns this into a
/// `"bad-options"` protocol error BEFORE any `shaderc` call is attempted,
/// never a silent fallback to the default (the caller asked for
/// something specific; silently answering with something else would be
/// worse than an explicit refusal).
fn resolve_wire_options(wire_options: Option<&serde_json::Value>) -> Result<ResolvedOptions, ()> {
    let target_env_str = wire_options
        .and_then(|options| options.get("target_env"))
        .and_then(serde_json::Value::as_str)
        .unwrap_or(kwe_core::TARGET_ENV);
    let target_env_version_str = wire_options
        .and_then(|options| options.get("target_env_version"))
        .and_then(serde_json::Value::as_str)
        .unwrap_or(kwe_core::TARGET_ENV_VERSION);
    let optimization_level_str = wire_options
        .and_then(|options| options.get("optimization_level"))
        .and_then(serde_json::Value::as_str)
        .unwrap_or(kwe_core::OPTIMIZATION_LEVEL);

    let target_env = match target_env_str {
        "vulkan" => shaderc::TargetEnv::Vulkan,
        _ => return Err(()),
    };
    let target_env_version = match target_env_version_str {
        "1.0" => shaderc::EnvVersion::Vulkan1_0,
        "1.1" => shaderc::EnvVersion::Vulkan1_1,
        "1.2" => shaderc::EnvVersion::Vulkan1_2,
        "1.3" => shaderc::EnvVersion::Vulkan1_3,
        "1.4" => shaderc::EnvVersion::Vulkan1_4,
        _ => return Err(()),
    };
    let optimization_level = match optimization_level_str {
        "zero" => shaderc::OptimizationLevel::Zero,
        "size" => shaderc::OptimizationLevel::Size,
        "performance" => shaderc::OptimizationLevel::Performance,
        _ => return Err(()),
    };
    Ok(ResolvedOptions {
        target_env,
        target_env_version,
        optimization_level,
    })
}

/// Compiles `source` for `stage` (already validated to be `"vertex"` or
/// `"fragment"` by `validate_shader_compile_request`) using `resolved`
/// (SR-3c2: the wire request's own options, defaulted/validated by
/// `resolve_wire_options` -- decision (a)'s "the helper must compile WITH
/// the wire options it receives"). Returns the raw SPIR-V bytes
/// (`shaderc::CompilationArtifact::as_binary_u8`'s own native-endian
/// layout — both processes always run on the same host, so this matches
/// `materialshader::compile_stage`'s `Vec<u32>` byte-for-byte once the
/// renderer reassembles it, no endian conversion needed) on success, or a
/// short diagnostic string (shaderc's own error text, UNBOUNDED at this
/// point — the caller bounds it to `MAX_SHADER_COMPILE_ERROR_LOG_BYTES`
/// before it reaches the wire) on a GLSL compile failure or a `shaderc`
/// setup failure (compiler/options construction). Never panics on a
/// compile FAILURE; `shaderc-rs`'s own `CString::new` on the source text
/// can panic on an embedded NUL byte — the same latent risk
/// `materialshader::compile_stage` already carries today, not a new one
/// introduced here (this process's own isolation, not a `catch_unwind`,
/// is what contains it: a panic here just exits this one-shot process,
/// which the caller (`wait_and_read`) already classifies as
/// `ProtocolError` — no response frame — and falls back in-thread).
fn compile_source(
    source: &str,
    stage: &str,
    resolved: &ResolvedOptions,
) -> Result<Vec<u8>, String> {
    let compiler = shaderc::Compiler::new().map_err(|error| error.to_string())?;
    let mut options = shaderc::CompileOptions::new().map_err(|error| error.to_string())?;
    options.set_target_env(resolved.target_env, resolved.target_env_version as u32);
    options.set_optimization_level(resolved.optimization_level);
    let shader_kind = match stage {
        "vertex" => shaderc::ShaderKind::Vertex,
        "fragment" => shaderc::ShaderKind::Fragment,
        // validate_shader_compile_request already restricts "stage" to
        // exactly these two values before this function is ever called.
        _ => return Err(format!("unreachable stage {stage:?}")),
    };
    let artifact = compiler
        .compile_into_spirv(
            source,
            shader_kind,
            "shader-helper-input",
            kwe_core::ENTRY_POINT,
            Some(&options),
        )
        .map_err(|error| error.to_string())?;
    Ok(artifact.as_binary_u8().to_vec())
}

/// Writes the `"ok"` kind-17 header (schema/status/`spirv_chunks`/
/// `spirv_total_bytes`) then that many kind-18 raw-binary chunks, each at
/// most [`MAX_PAYLOAD_BYTES`] (64 KiB) — the per-frame cap every frame on
/// this wire already obeys, so `.chunks(MAX_PAYLOAD_BYTES)` alone is
/// sufficient, no extra bound needed. Best-effort throughout: a write
/// failure partway through (the caller's read side already gone) stops
/// silently — this process is exiting either way.
fn respond_ok(spirv: &[u8]) {
    let chunks: Vec<&[u8]> = if spirv.is_empty() {
        Vec::new()
    } else {
        spirv.chunks(MAX_PAYLOAD_BYTES).collect()
    };
    write_response_best_effort(&serde_json::json!({
        "schema": SHADER_COMPILE_RESPONSE_SCHEMA,
        "status": "ok",
        "spirv_chunks": chunks.len() as u64,
        "spirv_total_bytes": spirv.len() as u64,
    }));
    let stdout = io::stdout();
    let mut handle = stdout.lock();
    for chunk in chunks {
        if write_frame(&mut handle, FrameKind::SpirvChunkV1, chunk).is_err() {
            return;
        }
    }
    let _ = handle.flush();
}

/// Writes a `"compile-error"` kind-17 response — a compile error is a
/// RESULT, not a helper failure (task text), so this is a normal, non-
/// protocol-error response shape with its own `"log"` field, bounded to
/// [`MAX_SHADER_COMPILE_ERROR_LOG_BYTES`] on a UTF-8 char boundary (never
/// panics on a multi-byte character at the cut point, same discipline
/// `bounded` already uses for protocol-error reason codes).
fn respond_compile_error(log: &str) {
    write_response_best_effort(&serde_json::json!({
        "schema": SHADER_COMPILE_RESPONSE_SCHEMA,
        "status": "compile-error",
        "log": bounded(log, MAX_SHADER_COMPILE_ERROR_LOG_BYTES),
    }));
}

/// Reads exactly one request and answers it. Returns the process exit
/// code; never panics on any input (malformed frames/JSON are ordinary
/// `Err` values, handled explicitly).
fn run(arguments: &Arguments) -> i32 {
    let deadline = Instant::now() + Duration::from_millis(arguments.max_wall_ms);
    let stdin = io::stdin();
    let mut reader = FrameReader::with_caps(
        DeadlineReader::new(stdin.lock(), deadline),
        StreamCaps::SHADER_REQUEST,
    );

    let first = match reader.next_frame() {
        Ok(Some(frame)) => frame,
        Ok(None) => {
            eprintln!("event=shader_compiler.no_request");
            return exit_code::NO_REQUEST;
        }
        Err(error) if is_watchdog_timeout(&error) => {
            eprintln!("event=shader_compiler.watchdog_expired stage=first-frame");
            return exit_code::WATCHDOG_EXPIRED;
        }
        Err(error) => return respond_protocol_error(&reason_for_frame_error(&error)),
    };

    if first.kind != FrameKind::ShaderCompileRequestV1 {
        return respond_protocol_error("wrong-kind");
    }

    let request = match validate_shader_compile_request(&first.payload, arguments.max_source_bytes)
    {
        Ok(request) => request,
        Err(error) => return respond_protocol_error(&reason_for_request_error(&error)),
    };

    // Decision (c): exactly one request per process. ANY trailing bytes on
    // stdin after the one request — whether they form another valid
    // frame or not — are excess and refused, the stricter of the two
    // options the task named (vs. silently ignoring them).
    match reader.next_frame() {
        Ok(None) => {}
        Ok(Some(_)) => return respond_protocol_error("excess-request"),
        Err(error) if is_watchdog_timeout(&error) => {
            // A slow/hanging SECOND write is not this process's protocol
            // to police differently from the first read's own watchdog —
            // same silent exit.
            eprintln!("event=shader_compiler.watchdog_expired stage=excess-check");
            return exit_code::WATCHDOG_EXPIRED;
        }
        Err(_) => return respond_protocol_error("excess-request"),
    }

    // Both already validated to be present strings of the expected shape
    // by `validate_shader_compile_request` above.
    let stage = request["stage"].as_str().unwrap_or_default();
    let source = request["source"].as_str().unwrap_or_default();
    // SR-3c2: resolve+validate the wire "options" vocabulary BEFORE
    // attempting any shaderc call -- an out-of-vocabulary value is a
    // caller-side request problem (protocol-error), never folded into a
    // "compile-error" (which is reserved for a real shaderc/GLSL
    // failure) and never silently defaulted.
    let resolved = match resolve_wire_options(request.get("options")) {
        Ok(resolved) => resolved,
        Err(()) => return respond_protocol_error("bad-options"),
    };
    match compile_source(source, stage, &resolved) {
        Ok(spirv) => respond_ok(&spirv),
        Err(log) => respond_compile_error(&log),
    }
    // SR-3c task text: "a compile error is a RESULT, not a helper
    // failure" — exit 0 either way, same as the "ok"/"unimplemented"
    // shapes before it.
    exit_code::OK
}

fn main() {
    kwe_platform::guard_parent_exit(libc::SIGKILL);
    let arguments = match parse_arguments(std::env::args().skip(1)) {
        Ok(arguments) => arguments,
        Err(message) => {
            eprintln!("event=shader_compiler.bad_arguments detail={message}");
            std::process::exit(exit_code::BAD_ARGUMENTS);
        }
    };
    std::process::exit(run(&arguments));
}

#[cfg(test)]
mod tests {
    use super::*;

    const VALID_FRAGMENT_SOURCE: &str = "#version 450\nlayout(location = 0) out vec4 outColor;\nvoid main() { outColor = vec4(1.0); }\n";

    /// Skips (rather than fails) when the system `libshaderc` is not
    /// present in this build environment -- same "checked at runtime"
    /// convention `materialshader.rs`'s own shaderc-dependent tests use.
    fn skip_if_shaderc_unavailable() -> bool {
        if shaderc::Compiler::new().is_err() {
            eprintln!("skipping: libshaderc unavailable");
            return true;
        }
        false
    }

    fn default_resolved_options() -> ResolvedOptions {
        resolve_wire_options(None).expect("kwe-core's own defaults must always resolve")
    }

    #[test]
    fn compile_source_compiles_valid_glsl_to_spirv() {
        if skip_if_shaderc_unavailable() {
            return;
        }
        let spirv = compile_source(
            VALID_FRAGMENT_SOURCE,
            "fragment",
            &default_resolved_options(),
        )
        .unwrap();
        assert!(!spirv.is_empty());
        assert_eq!(spirv.len() % 4, 0);
        assert_eq!(&spirv[0..4], &0x0723_0203_u32.to_le_bytes());
    }

    #[test]
    fn compile_source_reports_a_glsl_error_as_err_not_a_panic() {
        if skip_if_shaderc_unavailable() {
            return;
        }
        let error = compile_source(
            "#version 450\nvoid main() { !!! }",
            "fragment",
            &default_resolved_options(),
        )
        .unwrap_err();
        assert!(!error.is_empty());
    }

    // -----------------------------------------------------------------
    // SR-3c2: resolve_wire_options
    // -----------------------------------------------------------------

    #[test]
    fn resolve_wire_options_with_no_options_at_all_matches_kwe_core_defaults() {
        let resolved = resolve_wire_options(None).unwrap();
        assert_eq!(resolved.target_env, shaderc::TargetEnv::Vulkan);
        assert_eq!(resolved.target_env_version, shaderc::EnvVersion::Vulkan1_2);
        assert_eq!(
            resolved.optimization_level,
            shaderc::OptimizationLevel::Zero
        );
    }

    #[test]
    fn resolve_wire_options_defaults_each_field_independently_when_absent() {
        // Only optimization_level present -- target_env/target_env_version
        // still default to kwe-core's own constants (SR-3c2: per-field,
        // not all-or-nothing).
        let wire = serde_json::json!({"optimization_level": "performance"});
        let resolved = resolve_wire_options(Some(&wire)).unwrap();
        assert_eq!(resolved.target_env, shaderc::TargetEnv::Vulkan);
        assert_eq!(resolved.target_env_version, shaderc::EnvVersion::Vulkan1_2);
        assert_eq!(
            resolved.optimization_level,
            shaderc::OptimizationLevel::Performance
        );
    }

    #[test]
    fn resolve_wire_options_accepts_every_documented_vocabulary_value() {
        for version in ["1.0", "1.1", "1.2", "1.3", "1.4"] {
            let wire = serde_json::json!({"target_env": "vulkan", "target_env_version": version});
            assert!(
                resolve_wire_options(Some(&wire)).is_ok(),
                "version={version}"
            );
        }
        for level in ["zero", "size", "performance"] {
            let wire = serde_json::json!({"optimization_level": level});
            assert!(resolve_wire_options(Some(&wire)).is_ok(), "level={level}");
        }
    }

    #[test]
    fn resolve_wire_options_rejects_an_unknown_value_for_any_field() {
        for wire in [
            serde_json::json!({"target_env": "opengl"}),
            serde_json::json!({"target_env_version": "9.9"}),
            serde_json::json!({"optimization_level": "maximum"}),
        ] {
            assert!(resolve_wire_options(Some(&wire)).is_err(), "wire={wire:?}");
        }
    }

    /// SR-3c2 task item 3's own "do not fake it" instruction: proves the
    /// optimization level is genuinely CONSUMED, not just accepted and
    /// discarded, by compiling a shader with an obvious optimization
    /// opportunity (an unused variable, and a loop whose result is
    /// multiplied by a compile-time-constant 0.0, both dead at
    /// Size/Performance) at "zero" vs "performance" and asserting the
    /// SPIR-V differs -- empirically confirmed (not assumed) before this
    /// test was written: zero=1132 bytes, performance=size=304 bytes on
    /// this shaderc build.
    #[test]
    fn optimization_level_actually_changes_the_compiled_spirv() {
        if skip_if_shaderc_unavailable() {
            return;
        }
        let source = "#version 450\nlayout(location=0) out vec4 outColor;\nvoid main() {\n    float a = 1.0 + 2.0 - 3.0;\n    float unused = a * 42.0;\n    float b = 0.0;\n    for (int i = 0; i < 4; i++) { b += float(i); }\n    outColor = vec4(1.0, 0.0, 0.0, 1.0) + vec4(0.0) * unused * b;\n}\n";
        let zero =
            resolve_wire_options(Some(&serde_json::json!({"optimization_level": "zero"}))).unwrap();
        let performance = resolve_wire_options(Some(
            &serde_json::json!({"optimization_level": "performance"}),
        ))
        .unwrap();
        let zero_spirv = compile_source(source, "fragment", &zero).unwrap();
        let performance_spirv = compile_source(source, "fragment", &performance).unwrap();
        assert_ne!(
            zero_spirv, performance_spirv,
            "optimization_level must actually affect the compiled output"
        );
    }

    #[test]
    fn default_arguments_match_the_documented_defaults() {
        let arguments = parse_arguments(std::iter::empty()).unwrap();
        assert_eq!(arguments.max_wall_ms, DEFAULT_MAX_WALL_MS);
        assert_eq!(arguments.max_source_bytes, DEFAULT_MAX_SOURCE_BYTES);
    }

    #[test]
    fn flags_override_the_defaults() {
        let arguments = parse_arguments(
            ["--max-wall-ms", "500", "--max-source-bytes", "1024"]
                .into_iter()
                .map(str::to_string),
        )
        .unwrap();
        assert_eq!(arguments.max_wall_ms, 500);
        assert_eq!(arguments.max_source_bytes, 1024);
    }

    #[test]
    fn a_flag_missing_its_value_is_an_error() {
        let error = parse_arguments(["--max-wall-ms"].into_iter().map(str::to_string)).unwrap_err();
        assert!(error.contains("--max-wall-ms"), "{error}");
    }

    #[test]
    fn a_non_numeric_value_is_an_error() {
        let error = parse_arguments(
            ["--max-wall-ms", "not-a-number"]
                .into_iter()
                .map(str::to_string),
        )
        .unwrap_err();
        assert!(error.contains("invalid number"), "{error}");
    }

    #[test]
    fn an_unrecognized_flag_is_an_error() {
        let error = parse_arguments(["--bogus"].into_iter().map(str::to_string)).unwrap_err();
        assert!(error.contains("unrecognized argument"), "{error}");
    }

    #[test]
    fn deadline_reader_rejects_once_the_deadline_has_passed() {
        let already_expired = Instant::now() - Duration::from_secs(1);
        let mut reader = DeadlineReader::new(io::Cursor::new(b"hello".to_vec()), already_expired);
        let mut buffer = [0_u8; 5];
        let error = reader.read(&mut buffer).unwrap_err();
        assert_eq!(error.kind(), io::ErrorKind::TimedOut);
    }

    #[test]
    fn deadline_reader_passes_through_before_the_deadline() {
        let far_future = Instant::now() + Duration::from_secs(30);
        let mut reader = DeadlineReader::new(io::Cursor::new(b"hello".to_vec()), far_future);
        let mut buffer = [0_u8; 5];
        assert_eq!(reader.read(&mut buffer).unwrap(), 5);
        assert_eq!(&buffer, b"hello");
    }

    #[test]
    fn reason_strings_never_exceed_a_bounded_length_even_for_a_long_include_name() {
        let long_name = "x".repeat(10_000);
        let reason = reason_for_request_error(&ShaderRequestError::InvalidInclude(long_name));
        assert!(reason.len() < 256, "reason was {} bytes", reason.len());
    }

    #[test]
    fn bounded_truncates_on_a_char_boundary_never_panicking() {
        // A multibyte character sitting right at the truncation point:
        // truncating mid-character (a plain byte slice) would panic.
        let text = "x".repeat(127) + "€€€€"; // '€' is 3 bytes in UTF-8
        let truncated = bounded(&text, 128);
        assert!(truncated.len() <= 128);
        assert!(text.starts_with(truncated));

        // Shorter than the bound: unchanged.
        assert_eq!(bounded("short", 128), "short");
        // Exactly at the bound.
        let exact = "y".repeat(128);
        assert_eq!(bounded(&exact, 128), exact);
    }
}
