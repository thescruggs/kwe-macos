// SPDX-License-Identifier: GPL-3.0-or-later
//! SR-1b integration tests: spawn the REAL compiled `kwe-scene-inspector`
//! binary (not the in-crate unit-test functions) and exercise both delivery
//! modes end to end — `--report-fd` present and absent.
//!
//! `env!("CARGO_BIN_EXE_kwe-scene-inspector")` is Cargo's own generated
//! path to the compiled binary for this package's `[[bin]]` target
//! (verified empirically: the env var name keeps the package's hyphens,
//! `CARGO_BIN_EXE_kwe-scene-inspector`, not underscores).

use std::{
    fs,
    io::Read,
    os::fd::{FromRawFd, OwnedFd},
    path::PathBuf,
    process::{Command, Stdio},
};

fn temp_dir(label: &str) -> PathBuf {
    let dir = std::env::temp_dir().join(format!(
        "kwe-scene-inspector-report-fd-{label}-{}-{}",
        std::process::id(),
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap_or_default()
            .as_nanos()
    ));
    fs::create_dir_all(&dir).unwrap();
    dir
}

/// `--report-fd`: stdout is empty, exactly one `scene-inspection-v1` frame
/// arrives on the pipe, and `kwe_report_protocol::validate_inspection`
/// accepts it.
#[test]
fn report_fd_present_emits_one_validated_frame_and_empty_stdout() {
    let dir = temp_dir("present");
    fs::write(
        dir.join("scene.json"),
        br#"{"objects":[{"id":1,"image":"a.png"}]}"#,
    )
    .unwrap();

    // A real anonymous pipe, CLOEXEC on both ends initially (mirrors the
    // daemon's own pipe2(O_CLOEXEC) — see crates/kwe-daemon/src/inspect.rs);
    // the write end's CLOEXEC is then cleared so the spawned child inherits
    // it (at the SAME fd number, since fork()+exec() via `Command` does not
    // renumber a non-stdio inherited fd), the way `--report-fd` expects.
    let [read_fd, write_fd] = kwe_platform::pipe_cloexec().expect("pipe failed");
    // SAFETY: write_fd is the valid, just-created pipe write end.
    let flags = unsafe { libc::fcntl(write_fd, libc::F_GETFD) };
    assert!(flags >= 0);
    assert_eq!(
        unsafe { libc::fcntl(write_fd, libc::F_SETFD, flags & !libc::FD_CLOEXEC) },
        0,
        "clearing CLOEXEC on the write end failed: {}",
        std::io::Error::last_os_error()
    );

    let mut child = Command::new(env!("CARGO_BIN_EXE_kwe-scene-inspector"))
        .arg("--input")
        .arg(&dir)
        .arg("--report-fd")
        .arg(write_fd.to_string())
        .stdout(Stdio::piped())
        .spawn()
        .unwrap();

    // This test process's own copy of the write end must close too, or the
    // read below blocks forever: EOF on the read end requires every
    // process's copy of the write end to be closed, not just the child's.
    // SAFETY: write_fd is a valid fd owned solely by this test, and it is
    // not used again after this point.
    unsafe { libc::close(write_fd) };

    // SAFETY: read_fd is a valid, open, test-exclusive fd from the pipe2
    // call above; ownership transfers to this File, which closes it on drop.
    let mut report = std::fs::File::from(unsafe { OwnedFd::from_raw_fd(read_fd) });
    let mut report_bytes = Vec::new();
    report.read_to_end(&mut report_bytes).unwrap();

    let mut stdout_bytes = Vec::new();
    child
        .stdout
        .take()
        .unwrap()
        .read_to_end(&mut stdout_bytes)
        .unwrap();
    let status = child.wait().unwrap();
    assert!(status.success(), "{status:?}");
    assert!(
        stdout_bytes.is_empty(),
        "stdout must be empty with --report-fd, got {stdout_bytes:?}"
    );

    let mut reader = kwe_report_protocol::FrameReader::new(std::io::Cursor::new(report_bytes));
    let frame = reader.next_frame().unwrap().expect("one frame");
    assert_eq!(
        frame.kind,
        kwe_report_protocol::FrameKind::SceneInspectionV1
    );
    assert!(
        reader.next_frame().unwrap().is_none(),
        "exactly one frame must arrive"
    );

    let record = kwe_report_protocol::validate_inspection(&frame.payload).unwrap();
    assert_eq!(record["outcome"], "inventoried");
    assert_eq!(record["schema"], "scene-inspection-v1");

    fs::remove_dir_all(&dir).unwrap();
}

/// No `--report-fd`: behavior is byte-identical to before this slice — the
/// v0 record line, non-empty, on stdout. The record's own content is
/// already covered by the in-crate unit tests; this only proves the flag's
/// absence keeps the stdout channel exactly as it was.
#[test]
fn no_report_fd_flag_emits_the_v0_line_on_stdout() {
    let dir = temp_dir("absent");
    fs::write(dir.join("scene.json"), br#"{"objects":[]}"#).unwrap();

    let output = Command::new(env!("CARGO_BIN_EXE_kwe-scene-inspector"))
        .arg("--input")
        .arg(&dir)
        .output()
        .unwrap();
    assert!(output.status.success(), "{output:?}");
    assert!(!output.stdout.is_empty(), "stdout must carry the v0 line");

    let line = output.stdout.strip_suffix(b"\n").unwrap_or(&output.stdout);
    let record: serde_json::Value = serde_json::from_slice(line).unwrap();
    assert_eq!(record["schema"], "scene-feature-inventory-v0");
    assert!(record.get("capabilities_schema").is_none());
    assert!(record.get("backend").is_none());

    fs::remove_dir_all(&dir).unwrap();
}
