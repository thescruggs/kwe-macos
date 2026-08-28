// SPDX-License-Identifier: GPL-3.0-or-later
//! SR-3a: the killable shader-compile helper's PROTOCOL SKELETON (plan
//! §4.3/§8 SR-3). This binary reads exactly ONE `shader-compile-request-v1`
//! frame (kind 16) off stdin, writes exactly one `shader-compile-response-v1`
//! frame (kind 17) to stdout, and exits — decision (c): "one serial request
//! per helper PROCESS" for this skeleton. A long-lived serial-loop mode
//! (reusing one process for many requests) is an explicit OPEN QUESTION for
//! a later slice once SR-3c measures real spawn cost against real
//! compilation latency (`docs/SR3.md`); this skeleton does not build it.
//!
//! No `shaderc` dependency here yet — SR-3c decides how/whether shaderc
//! reaches this crate (this skeleton stays dependency-light:
//! `kwe-report-protocol` + `serde_json` only, per the task). Every
//! STRUCTURALLY VALID request gets `{"status":"unimplemented",
//! "reason":"skeleton"}` and exit 0; nothing is ever compiled.
//!
//! Wire contract: `docs/SHADER_HELPER_PROTOCOL_V1.md`. Containment/bounds
//! model (deadline, byte caps) deliberately mirrors
//! `kwe-scene-inspector`'s own self-watchdog + bounded-read shape (SR-0b),
//! adapted to a request/response exchange instead of a one-shot file scan.

use std::io::{self, Read, Write};
use std::time::{Duration, Instant};

use kwe_report_protocol::{
    FrameError, FrameKind, FrameReader, SHADER_COMPILE_RESPONSE_SCHEMA, ShaderRequestError,
    StreamCaps, validate_shader_compile_request, write_frame,
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
    /// Exits SILENTLY — no response frame is attempted. The daemon-side
    /// kill is the AUTHORITATIVE bound (SR-3b, not built yet); this
    /// watchdog is a soft backstop only — see `DeadlineReader`'s doc
    /// comment for exactly what it can and cannot preempt.
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

fn respond_unimplemented() {
    write_response_best_effort(&serde_json::json!({
        "schema": SHADER_COMPILE_RESPONSE_SCHEMA,
        "status": "unimplemented",
        "reason": "skeleton",
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

    if let Err(error) = validate_shader_compile_request(&first.payload, arguments.max_source_bytes)
    {
        return respond_protocol_error(&reason_for_request_error(&error));
    }

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

    respond_unimplemented();
    exit_code::OK
}

fn main() {
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
