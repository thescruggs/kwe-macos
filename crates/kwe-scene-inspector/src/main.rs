// SPDX-License-Identifier: GPL-3.0-or-later
//! SR-0b: one-shot, daemon-supervised scene inspector skeleton.
//!
//! Accepts one scene entry path, classifies it, hashes bounded bytes under a
//! byte cap and a self-watchdog wall-clock cap, and emits exactly ONE JSON
//! line on stdout conforming to the draft `scene-feature-inventory-v0`
//! record (docs/SCENE_CAPABILITIES.md). No scene parsing happens here yet
//! (SR-0c adds the loader adapter that fills `required`/`detected`/`unknown`).
//!
//! Containment (crates/kwe-daemon/src/inspect.rs) mirrors
//! `supervisor::spawn_worker`: private HOME, process-group kill, PDEATHSIG,
//! rlimits, no network. This binary's own job is only to fail closed and
//! bounded on its own — the daemon's wall-clock kill is authoritative; the
//! `--max-wall-ms` flag here is a courtesy backstop checked between chunks.

use std::{
    fs,
    io::{Read, Write},
    os::unix::fs::OpenOptionsExt,
    path::{Path, PathBuf},
    time::{Duration, Instant},
};

use clap::Parser;
use serde_json::{Value, json};
use sha2::{Digest, Sha256};

/// Draft record schema this binary emits (docs/SCENE_CAPABILITIES.md,
/// "Inventory record — draft schema `scene-feature-inventory-v0`"). Frozen
/// by SR-1, not before.
const SCHEMA: &str = "scene-feature-inventory-v0";
/// This binary's build identity. The daemon's `build_identity`
/// (crates/kwe-daemon/src/supervisor.rs) is computed daemon-side from the
/// daemon's and every renderer binary's size+mtime; there is no
/// `option_env!` git-sha mechanism anywhere in this workspace for a
/// standalone one-shot binary to mirror, so this is a literal placeholder
/// pending SR-1 (see docs/SR0.md open risks for SR-0b).
const INSPECTOR_BUILD: &str = "dev";
const INSPECTOR_ABI: u64 = 0;
/// Streamed read chunk size for hashing.
const HASH_CHUNK_BYTES: usize = 64 * 1024;
/// Serialized record safety cap (docs/SCENE_CAPABILITIES.md: "<= 64 KiB").
const MAX_REPORT_BYTES: usize = 65536;
/// `stdout` cannot be written at all; distinct from every other outcome,
/// which is always a JSON record on exit 0.
const EXIT_STDOUT_UNWRITABLE: i32 = 74;

#[derive(Debug, Parser)]
#[command(
    version,
    about = "Bounded one-shot Wallpaper Engine scene inventory inspector"
)]
struct Arguments {
    /// Scene entry to inspect: a `scene.pkg` file, or a directory containing
    /// `scene.json`.
    #[arg(long)]
    input: PathBuf,
    /// Hard cap on bytes read/hashed from the input, in MiB. Deliberately
    /// unrestricted (including 0) so tests can exercise the oversize path
    /// without a huge fixture.
    #[arg(long, default_value_t = 512)]
    max_source_mib: u64,
    /// Self-watchdog wall-clock backstop in milliseconds, checked between
    /// bounded read chunks. The daemon owns the authoritative kill.
    #[arg(long, default_value_t = 10_000)]
    max_wall_ms: u64,
}

/// What the input classified as, and the single file whose bytes are
/// hashed for it.
enum Classification {
    Pkg(PathBuf),
    JsonDir(PathBuf),
    Unrecognized,
}

/// Classify `input` per the SR-0b contract: a regular file named
/// `scene.pkg` or with extension `.pkg`, or a directory containing
/// `scene.json` (skeleton scope: only that one file is hashed for a
/// directory input). Anything else — including a symlink at any of these
/// positions — is unrecognized; this mirrors the lstat-and-reject-symlinks
/// convention used for every other bounded read in this workspace
/// (crates/kwe-core/src/pkg.rs `PkgReader::open`).
fn classify(input: &Path) -> Classification {
    let Ok(meta) = fs::symlink_metadata(input) else {
        return Classification::Unrecognized;
    };
    if meta.file_type().is_file() {
        let named_scene_pkg = input.file_name().and_then(|name| name.to_str()) == Some("scene.pkg");
        let has_pkg_extension = input.extension().and_then(|ext| ext.to_str()) == Some("pkg");
        if named_scene_pkg || has_pkg_extension {
            return Classification::Pkg(input.to_path_buf());
        }
        return Classification::Unrecognized;
    }
    if meta.file_type().is_dir() {
        let scene_json = input.join("scene.json");
        if let Ok(inner) = fs::symlink_metadata(&scene_json)
            && inner.file_type().is_file()
        {
            return Classification::JsonDir(scene_json);
        }
    }
    Classification::Unrecognized
}

/// Outcome of the bounded, TOCTOU-safe streamed hash.
enum HashOutcome {
    Ok { hash: String, bytes: u64 },
    Oversize { bytes: u64 },
    Timeout { bytes: u64 },
    IoError,
}

/// Stream SHA-256 over `path` in `HASH_CHUNK_BYTES` chunks, stopping as soon
/// as the running byte count would exceed `max_bytes` or `deadline` passes.
/// Mirrors the lstat/O_NOFOLLOW open used by `crates/kwe-core/src/pkg.rs`
/// (reject symlinks and non-regular files at the fd, not just the path).
fn hash_bounded(path: &Path, max_bytes: u64, deadline: Instant) -> HashOutcome {
    let Ok(meta) = fs::symlink_metadata(path) else {
        return HashOutcome::IoError;
    };
    if meta.file_type().is_symlink() || !meta.file_type().is_file() {
        return HashOutcome::IoError;
    }
    let file = fs::OpenOptions::new()
        .read(true)
        .custom_flags(libc::O_CLOEXEC | libc::O_NOFOLLOW)
        .open(path);
    let mut file = match file {
        Ok(file) => file,
        Err(_) => return HashOutcome::IoError,
    };
    let mut hasher = Sha256::new();
    let mut buffer = vec![0_u8; HASH_CHUNK_BYTES];
    let mut total: u64 = 0;
    loop {
        if Instant::now() >= deadline {
            return HashOutcome::Timeout { bytes: total };
        }
        let read = match file.read(&mut buffer) {
            Ok(0) => break,
            Ok(count) => count,
            Err(_) => return HashOutcome::IoError,
        };
        total = total.saturating_add(read as u64);
        if total > max_bytes {
            return HashOutcome::Oversize { bytes: total };
        }
        hasher.update(&buffer[..read]);
    }
    HashOutcome::Ok {
        hash: format!("sha256:{}", hex::encode(hasher.finalize())),
        bytes: total,
    }
}

/// Build one draft `scene-feature-inventory-v0` record. SR-0b never parses a
/// scene, so `required`/`detected` stay empty and `unknown` stays all
/// zeros — SR-0c fills those from real inventory. `digest` is the hex
/// SHA-256 over the record serialized with `digest` itself set to `""`.
#[allow(clippy::too_many_arguments)]
fn build_record(
    outcome: &str,
    reason: &str,
    kind: &str,
    hash: &str,
    source_bytes: u64,
    wall_ms: u64,
    limits_hit: &[&str],
) -> Value {
    let mut record = json!({
        "schema": SCHEMA,
        "content": { "hash": hash, "source_bytes": source_bytes, "kind": kind },
        "inspector": { "build": INSPECTOR_BUILD, "abi": INSPECTOR_ABI },
        "outcome": outcome,
        "reason": reason,
        "required": [],
        "detected": [],
        "unknown": { "keys": 0, "types": 0, "objects": 0, "samples": [], "truncated": false },
        "bounds": { "wall_ms": wall_ms, "peak_bytes": 0, "limits_hit": limits_hit },
        "digest": "",
    });
    let serialized = serde_json::to_vec(&record).unwrap_or_default();
    let digest = hex::encode(Sha256::digest(&serialized));
    record["digest"] = json!(digest);
    record
}

fn elapsed_ms(start: Instant) -> u64 {
    u64::try_from(start.elapsed().as_millis()).unwrap_or(u64::MAX)
}

/// Classify + hash one input into a full record.
fn inspect_input(input: &Path, max_bytes: u64, deadline: Instant, start: Instant) -> Value {
    match classify(input) {
        Classification::Unrecognized => build_record(
            "incompatible",
            "unrecognized-input",
            "",
            "",
            0,
            elapsed_ms(start),
            &[],
        ),
        Classification::Pkg(target) => hashed_record(&target, "pkg", max_bytes, deadline, start),
        Classification::JsonDir(target) => {
            hashed_record(&target, "json-dir", max_bytes, deadline, start)
        }
    }
}

fn hashed_record(
    target: &Path,
    kind: &str,
    max_bytes: u64,
    deadline: Instant,
    start: Instant,
) -> Value {
    match hash_bounded(target, max_bytes, deadline) {
        HashOutcome::Ok { hash, bytes } => build_record(
            "inventoried",
            "ok",
            kind,
            &hash,
            bytes,
            elapsed_ms(start),
            &[],
        ),
        HashOutcome::Oversize { bytes } => build_record(
            "incompatible",
            "oversize",
            kind,
            "",
            bytes,
            elapsed_ms(start),
            &["oversize"],
        ),
        HashOutcome::Timeout { bytes } => build_record(
            "unknown",
            "timeout",
            kind,
            "",
            bytes,
            elapsed_ms(start),
            &["timeout"],
        ),
        HashOutcome::IoError => {
            build_record("unknown", "io-error", kind, "", 0, elapsed_ms(start), &[])
        }
    }
}

/// Serialize `record`, replacing it with a minimal `report-oversize` record
/// when the serialized form would exceed `MAX_REPORT_BYTES`. Returns the
/// bytes to write, newline-terminated.
fn bound_report(record: &Value) -> Vec<u8> {
    let mut bytes = serde_json::to_vec(record).unwrap_or_default();
    if bytes.len() > MAX_REPORT_BYTES {
        eprintln!("event=inspector.report_oversize bytes={}", bytes.len());
        let minimal = build_record("unknown", "report-oversize", "", "", 0, 0, &[]);
        bytes = serde_json::to_vec(&minimal).unwrap_or_default();
    }
    bytes.push(b'\n');
    bytes
}

fn main() {
    let arguments = Arguments::parse();
    let start = Instant::now();
    let deadline = start + Duration::from_millis(arguments.max_wall_ms);
    let max_bytes = arguments.max_source_mib.saturating_mul(1024 * 1024);

    let record = inspect_input(&arguments.input, max_bytes, deadline, start);
    let bytes = bound_report(&record);

    let mut stdout = std::io::stdout().lock();
    let write_result = stdout.write_all(&bytes).and_then(|()| stdout.flush());
    if let Err(error) = write_result {
        eprintln!("event=inspector.stdout_error detail={error}");
        std::process::exit(EXIT_STDOUT_UNWRITABLE);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn far_deadline() -> Instant {
        Instant::now() + Duration::from_secs(30)
    }

    fn temp_dir(label: &str) -> PathBuf {
        let dir = std::env::temp_dir().join(format!(
            "kwe-scene-inspector-{label}-{}-{}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap_or_default()
                .as_nanos()
        ));
        fs::create_dir_all(&dir).unwrap();
        dir
    }

    /// (a) A small `scene.json` directory hashes identically across two runs.
    #[test]
    fn json_dir_scene_hashes_deterministically() {
        let dir = temp_dir("json-dir");
        fs::write(dir.join("scene.json"), br#"{"general":{}}"#).unwrap();

        let record = inspect_input(&dir, 512 * 1024 * 1024, far_deadline(), Instant::now());
        assert_eq!(record["outcome"], "inventoried");
        assert_eq!(record["reason"], "ok");
        assert_eq!(record["content"]["kind"], "json-dir");
        let hash = record["content"]["hash"].as_str().unwrap().to_string();
        assert!(!hash.is_empty());
        assert!(hash.starts_with("sha256:"));

        let record_again = inspect_input(&dir, 512 * 1024 * 1024, far_deadline(), Instant::now());
        assert_eq!(record_again["content"]["hash"], hash);

        fs::remove_dir_all(&dir).unwrap();
    }

    /// (b) A byte cap smaller than the input refuses typed, without needing
    /// a huge fixture (the flag accepts 0 MiB).
    #[test]
    fn oversize_input_is_refused_typed() {
        let dir = temp_dir("oversize");
        fs::write(dir.join("scene.json"), vec![b'x'; 4096]).unwrap();

        let record = inspect_input(&dir, 0, far_deadline(), Instant::now());
        assert_eq!(record["outcome"], "incompatible");
        assert_eq!(record["reason"], "oversize");
        assert_eq!(record["content"]["kind"], "json-dir");
        assert_eq!(record["content"]["hash"], "");
        assert_eq!(record["bounds"]["limits_hit"][0], "oversize");

        fs::remove_dir_all(&dir).unwrap();
    }

    /// (c) A directory with no `scene.json`, and a file that is neither
    /// `scene.pkg` nor `*.pkg`, both classify as unrecognized.
    #[test]
    fn unrecognized_input_is_refused_typed() {
        let dir = temp_dir("unrecognized");
        let record = inspect_input(&dir, 512 * 1024 * 1024, far_deadline(), Instant::now());
        assert_eq!(record["outcome"], "incompatible");
        assert_eq!(record["reason"], "unrecognized-input");
        assert_eq!(record["content"]["hash"], "");
        assert_eq!(record["content"]["source_bytes"], 0);

        let stray_file = dir.join("readme.txt");
        fs::write(&stray_file, b"not a scene").unwrap();
        let record = inspect_input(
            &stray_file,
            512 * 1024 * 1024,
            far_deadline(),
            Instant::now(),
        );
        assert_eq!(record["outcome"], "incompatible");
        assert_eq!(record["reason"], "unrecognized-input");

        fs::remove_dir_all(&dir).unwrap();
    }

    /// (d) The normal serialized record stays comfortably within the cap,
    /// and an artificially oversized one is replaced by the minimal
    /// `report-oversize` fallback, which itself stays within the cap.
    #[test]
    fn serialized_record_stays_within_the_64_kib_cap() {
        let record = inspect_input(
            &PathBuf::from("/nonexistent"),
            512,
            far_deadline(),
            Instant::now(),
        );
        let bytes = bound_report(&record);
        assert!(bytes.len() <= MAX_REPORT_BYTES + 1);

        let huge_hash = format!("sha256:{}", "a".repeat(MAX_REPORT_BYTES));
        let oversized = build_record("inventoried", "ok", "json-dir", &huge_hash, 1, 1, &[]);
        let bytes = bound_report(&oversized);
        assert!(bytes.len() <= MAX_REPORT_BYTES + 1);
        let parsed: Value = serde_json::from_slice(&bytes[..bytes.len() - 1]).unwrap();
        assert_eq!(parsed["outcome"], "unknown");
        assert_eq!(parsed["reason"], "report-oversize");
    }

    /// A wall-clock deadline that has already passed is honored even for a
    /// tiny input: the read loop checks the deadline before every chunk.
    #[test]
    fn expired_deadline_yields_a_timeout_record() {
        let dir = temp_dir("timeout");
        fs::write(dir.join("scene.json"), br#"{"general":{}}"#).unwrap();
        let expired = Instant::now() - Duration::from_secs(1);

        let record = inspect_input(&dir, 512 * 1024 * 1024, expired, Instant::now());
        assert_eq!(record["outcome"], "unknown");
        assert_eq!(record["reason"], "timeout");
        assert_eq!(record["bounds"]["limits_hit"][0], "timeout");

        fs::remove_dir_all(&dir).unwrap();
    }
}
