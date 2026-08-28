// SPDX-License-Identifier: GPL-3.0-or-later
//! SR-3a: integration tests for the shader-compile helper skeleton, driven
//! against the REAL compiled binary (`CARGO_BIN_EXE_kwe-shader-compiler`,
//! set by cargo for tests under `tests/`) exactly the way `kwe-daemon`'s
//! own tests drive a real subprocess — the fastest available proof that
//! the binary's actual stdin/stdout/exit-code behavior, not just its
//! internal functions, matches `docs/SHADER_HELPER_PROTOCOL_V1.md`.

use std::io::{Cursor, Write};
use std::process::{Command, Stdio};

use kwe_report_protocol::{
    FrameKind, FrameReader, SHADER_COMPILE_REQUEST_SCHEMA, StreamCaps,
    validate_shader_compile_response, write_frame,
};

fn binary_path() -> &'static str {
    env!("CARGO_BIN_EXE_kwe-shader-compiler")
}

/// Spawns the helper with `extra_args`, writes `stdin_bytes`, closes
/// stdin (signaling EOF), and waits for it to exit. Returns `(exit_code,
/// stdout_bytes)`.
fn run_helper(stdin_bytes: &[u8], extra_args: &[&str]) -> (i32, Vec<u8>) {
    let mut child = Command::new(binary_path())
        .args(extra_args)
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .expect("spawn kwe-shader-compiler");
    child
        .stdin
        .take()
        .unwrap()
        .write_all(stdin_bytes)
        .expect("write stdin");
    // stdin's write half drops here (end of statement above already moved
    // it out and it is not stored), closing the pipe -- the child sees a
    // clean EOF right after `stdin_bytes`.
    let output = child.wait_with_output().expect("wait for helper");
    (
        output.status.code().expect("helper exited via a signal"),
        output.stdout,
    )
}

fn valid_request_bytes() -> Vec<u8> {
    let payload = serde_json::json!({
        "schema": SHADER_COMPILE_REQUEST_SCHEMA,
        "stage": "fragment",
        "source": "void main() {}",
        "includes": {},
        "combos": {},
        "defines": {},
    });
    let mut framed = Vec::new();
    write_frame(
        &mut framed,
        FrameKind::ShaderCompileRequestV1,
        &serde_json::to_vec(&payload).unwrap(),
    )
    .unwrap();
    framed
}

/// Decodes exactly the frames present in `stdout_bytes` under the
/// response channel's own caps, asserting there is exactly one and that
/// it validates as `shader-compile-response-v1`. Returns the parsed JSON.
fn single_response_frame(stdout_bytes: &[u8]) -> serde_json::Value {
    let mut reader = FrameReader::with_caps(Cursor::new(stdout_bytes), StreamCaps::SHADER_RESPONSE);
    let frame = reader
        .next_frame()
        .expect("a well-formed frame")
        .expect("exactly one response frame, got none");
    assert_eq!(frame.kind, FrameKind::ShaderCompileResponseV1);
    assert!(
        reader.next_frame().unwrap().is_none(),
        "exactly one response frame, found a second"
    );
    validate_shader_compile_response(&frame.payload).expect("response validates")
}

#[test]
fn a_valid_request_gets_an_unimplemented_response_and_exits_ok() {
    let (code, stdout) = run_helper(&valid_request_bytes(), &[]);
    assert_eq!(code, 0, "stdout={stdout:?}");
    let response = single_response_frame(&stdout);
    assert_eq!(response["status"], "unimplemented");
    assert_eq!(response["reason"], "skeleton");
}

#[test]
fn a_wrong_kind_first_frame_is_a_protocol_error() {
    let mut stdin = Vec::new();
    write_frame(&mut stdin, FrameKind::SceneInspectionV1, b"{}").unwrap();
    let (code, stdout) = run_helper(&stdin, &[]);
    assert_eq!(code, 65, "stdout={stdout:?}");
    let response = single_response_frame(&stdout);
    assert_eq!(response["status"], "protocol-error");
    assert_eq!(response["reason"], "wrong-kind");
}

#[test]
fn a_second_request_frame_is_an_excess_protocol_error_not_a_success() {
    // Decision (c): the process reads exactly one request; a second frame
    // present on stdin is refused, not silently ignored -- and the
    // process must NOT have already committed to the success response
    // before noticing the excess bytes.
    let mut stdin = valid_request_bytes();
    stdin.extend_from_slice(&valid_request_bytes());
    let (code, stdout) = run_helper(&stdin, &[]);
    assert_eq!(code, 65, "stdout={stdout:?}");
    let response = single_response_frame(&stdout);
    assert_eq!(response["status"], "protocol-error");
    assert_eq!(response["reason"], "excess-request");
}

#[test]
fn oversize_source_is_a_protocol_error() {
    let payload = serde_json::json!({
        "schema": SHADER_COMPILE_REQUEST_SCHEMA,
        "stage": "vertex",
        "source": "x".repeat(2048),
        "includes": {},
        "combos": {},
        "defines": {},
    });
    let mut stdin = Vec::new();
    write_frame(
        &mut stdin,
        FrameKind::ShaderCompileRequestV1,
        &serde_json::to_vec(&payload).unwrap(),
    )
    .unwrap();
    let (code, stdout) = run_helper(&stdin, &["--max-source-bytes", "1024"]);
    assert_eq!(code, 65, "stdout={stdout:?}");
    let response = single_response_frame(&stdout);
    assert_eq!(response["status"], "protocol-error");
    assert_eq!(response["reason"], "source-oversize");
}

#[test]
fn malformed_json_is_a_protocol_error() {
    let mut stdin = Vec::new();
    write_frame(&mut stdin, FrameKind::ShaderCompileRequestV1, b"{not json").unwrap();
    let (code, stdout) = run_helper(&stdin, &[]);
    assert_eq!(code, 65, "stdout={stdout:?}");
    let response = single_response_frame(&stdout);
    assert_eq!(response["status"], "protocol-error");
    assert_eq!(response["reason"], "malformed-json");
}

#[test]
fn thirty_three_includes_is_a_protocol_error() {
    let includes: serde_json::Map<String, serde_json::Value> = (0..33)
        .map(|index| (format!("f{index}.glsl"), serde_json::json!("x")))
        .collect();
    let payload = serde_json::json!({
        "schema": SHADER_COMPILE_REQUEST_SCHEMA,
        "stage": "vertex",
        "source": "void main() {}",
        "includes": includes,
        "combos": {},
        "defines": {},
    });
    let mut stdin = Vec::new();
    write_frame(
        &mut stdin,
        FrameKind::ShaderCompileRequestV1,
        &serde_json::to_vec(&payload).unwrap(),
    )
    .unwrap();
    let (code, stdout) = run_helper(&stdin, &[]);
    assert_eq!(code, 65, "stdout={stdout:?}");
    let response = single_response_frame(&stdout);
    assert_eq!(response["status"], "protocol-error");
    assert_eq!(response["reason"], "too-many-includes");
}

#[test]
fn garbage_stdin_is_a_protocol_error_not_a_panic() {
    let garbage = vec![0x42_u8; 256];
    let (code, stdout) = run_helper(&garbage, &[]);
    assert_eq!(code, 65, "stdout={stdout:?}");
    let response = single_response_frame(&stdout);
    assert_eq!(response["status"], "protocol-error");
    assert_eq!(response["reason"], "bad-magic");
}

#[test]
fn empty_stdin_is_a_clean_no_request_exit_with_no_response_frame() {
    let (code, stdout) = run_helper(&[], &[]);
    assert_eq!(code, 66, "stdout={stdout:?}");
    assert!(
        stdout.is_empty(),
        "no response frame is expected when nothing was ever sent: {stdout:?}"
    );
}

#[test]
fn the_watchdog_kills_an_already_expired_deadline_without_a_response() {
    // `--max-wall-ms 0` makes the deadline equal to `Instant::now()` at
    // the moment `run()` computes it; by the time the FIRST read's
    // deadline check runs (a handful of instructions later), real time
    // has strictly advanced past it -- so this fires deterministically,
    // no timing race, no sleep-and-poll needed. A genuinely mid-exchange
    // expiry (the deadline passing only after some, but not all, of a
    // request has arrived) is the same code path -- `DeadlineReader`
    // checks the SAME way on every call -- just harder to make
    // deterministic from a test, so this is the reliable proof: the
    // watchdog outcome (exit 64, no response frame) is exactly what
    // fires once `Instant::now() >= deadline` is true at a read.
    let (code, stdout) = run_helper(&valid_request_bytes(), &["--max-wall-ms", "0"]);
    assert_eq!(code, 64, "stdout={stdout:?}");
    assert!(
        stdout.is_empty(),
        "watchdog expiry must exit silently, no response frame: {stdout:?}"
    );
}
