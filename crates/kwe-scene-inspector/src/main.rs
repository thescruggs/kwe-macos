// SPDX-License-Identifier: GPL-3.0-or-later
//! SR-0b/SR-0c: one-shot, daemon-supervised scene inspector.
//!
//! Accepts one scene entry path, classifies it, hashes bounded bytes under a
//! byte cap and a self-watchdog wall-clock cap, and emits exactly ONE JSON
//! line on stdout conforming to the draft `scene-feature-inventory-v0`
//! record (docs/SCENE_CAPABILITIES.md). SR-0c (`inventory.rs`) fills
//! `required`/`detected`/`unknown` from a bounded raw walk of `scene.json`'s
//! object family — objects only; materials require pkg/asset resolution of
//! referenced files and are their own follow-up slice (docs/SR0.md SR-0c
//! conductor scope note).
//!
//! Containment (crates/kwe-daemon/src/inspect.rs) mirrors
//! `supervisor::spawn_worker`: private HOME, process-group kill, PDEATHSIG,
//! rlimits, no network. This binary's own job is only to fail closed and
//! bounded on its own — the daemon's wall-clock kill is authoritative; the
//! `--max-wall-ms` flag here is a courtesy backstop checked between chunks.

mod inventory;

use std::{
    fs,
    io::{Read, Write},
    os::unix::fs::OpenOptionsExt,
    path::{Path, PathBuf},
    time::{Duration, Instant},
};

use clap::Parser;
use inventory::{
    DetectedCapability, Inventory, InventoryCaps, InventoryError, inventory_scene_json,
};
use kwe_core::{MAX_SCENE_JSON_BYTES, PkgReader, scene_json_entry};
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

/// Build one draft `scene-feature-inventory-v0` record from `inventory`
/// (SR-0b's empty `Inventory::default()` for every outcome that never
/// reaches a parsed scene; SR-0c's real object-family walk otherwise).
/// `digest` is the hex SHA-256 over the record serialized with `digest`
/// itself set to `""`.
#[allow(clippy::too_many_arguments)]
fn build_record(
    outcome: &str,
    reason: &str,
    kind: &str,
    hash: &str,
    source_bytes: u64,
    wall_ms: u64,
    limits_hit: &[&str],
    inventory: &Inventory,
) -> Value {
    let detected: Vec<Value> = inventory
        .detected
        .iter()
        .map(|capability| {
            json!({
                "capability": capability.capability,
                "count": capability.count,
                "objects": capability.objects,
                "truncated": capability.truncated,
            })
        })
        .collect();
    let mut record = json!({
        "schema": SCHEMA,
        "content": { "hash": hash, "source_bytes": source_bytes, "kind": kind },
        "inspector": { "build": INSPECTOR_BUILD, "abi": INSPECTOR_ABI },
        "outcome": outcome,
        "reason": reason,
        "required": inventory.required,
        "detected": detected,
        "unknown": {
            "keys": inventory.unknown.keys,
            "types": inventory.unknown.types,
            "objects": inventory.unknown.objects,
            "samples": inventory.unknown.samples,
            "truncated": inventory.unknown.truncated,
        },
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

/// Classify + hash + (SR-0c) inventory one input into a full record.
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
            &Inventory::default(),
        ),
        Classification::Pkg(target) => pkg_record(&target, max_bytes, deadline, start),
        Classification::JsonDir(target) => json_dir_record(&target, max_bytes, deadline, start),
    }
}

/// JsonDir: the hashed file (`target`) already IS `scene.json`.
fn json_dir_record(target: &Path, max_bytes: u64, deadline: Instant, start: Instant) -> Value {
    match hash_bounded(target, max_bytes, deadline) {
        HashOutcome::Ok { hash, bytes } => {
            // §2's JsonDir rule: re-open and bounded-read the same
            // scene.json file again (simplest correct — `target` already
            // hashed successfully within `max_bytes`, so the same cap
            // applies here) rather than threading the hashed bytes
            // through; a TOCTOU re-read failure (the file changed between
            // the two opens) is reported the same way any other bounded
            // read failure is: unknown/io-error.
            match read_bytes_bounded(target, max_bytes) {
                Ok(scene_bytes) => inventoried_record(
                    "json-dir",
                    &hash,
                    bytes,
                    &scene_bytes,
                    deadline,
                    start,
                    false,
                ),
                Err(()) => build_record(
                    "unknown",
                    "io-error",
                    "json-dir",
                    &hash,
                    bytes,
                    elapsed_ms(start),
                    &[],
                    &Inventory::default(),
                ),
            }
        }
        HashOutcome::Oversize { bytes } => build_record(
            "incompatible",
            "oversize",
            "json-dir",
            "",
            bytes,
            elapsed_ms(start),
            &["oversize"],
            &Inventory::default(),
        ),
        HashOutcome::Timeout { bytes } => build_record(
            "unknown",
            "timeout",
            "json-dir",
            "",
            bytes,
            elapsed_ms(start),
            &["timeout"],
            &Inventory::default(),
        ),
        HashOutcome::IoError => build_record(
            "unknown",
            "io-error",
            "json-dir",
            "",
            0,
            elapsed_ms(start),
            &[],
            &Inventory::default(),
        ),
    }
}

/// Pkg: `target` is the whole `scene.pkg` archive, already hashed as raw
/// bytes; the scene.json to inventory is a separate entry inside it,
/// located and bounded-read through kwe-core's real `PkgReader` (never a
/// second pkg parser).
fn pkg_record(target: &Path, max_bytes: u64, deadline: Instant, start: Instant) -> Value {
    match hash_bounded(target, max_bytes, deadline) {
        HashOutcome::Ok { hash, bytes } => match read_pkg_scene_json(target) {
            PkgSceneJson::Bytes(scene_bytes) => {
                inventoried_record("pkg", &hash, bytes, &scene_bytes, deadline, start, true)
            }
            PkgSceneJson::Missing => build_record(
                "incompatible",
                "parse-error",
                "pkg",
                &hash,
                bytes,
                elapsed_ms(start),
                &["pkg-no-scene-json"],
                &Inventory::default(),
            ),
            PkgSceneJson::Oversize => build_record(
                "incompatible",
                "oversize",
                "pkg",
                &hash,
                bytes,
                elapsed_ms(start),
                &["pkg-scene-json-oversize"],
                &Inventory::default(),
            ),
        },
        HashOutcome::Oversize { bytes } => build_record(
            "incompatible",
            "oversize",
            "pkg",
            "",
            bytes,
            elapsed_ms(start),
            &["oversize"],
            &Inventory::default(),
        ),
        HashOutcome::Timeout { bytes } => build_record(
            "unknown",
            "timeout",
            "pkg",
            "",
            bytes,
            elapsed_ms(start),
            &["timeout"],
            &Inventory::default(),
        ),
        HashOutcome::IoError => build_record(
            "unknown",
            "io-error",
            "pkg",
            "",
            0,
            elapsed_ms(start),
            &[],
            &Inventory::default(),
        ),
    }
}

/// Outcome of locating and bounded-reading a pkg's `scene.json` entry.
enum PkgSceneJson {
    Bytes(Vec<u8>),
    /// The archive could not be opened, has no `scene.json` entry, or the
    /// entry could not be read/decompressed.
    Missing,
    /// The entry exists but is larger than `MAX_SCENE_JSON_BYTES`.
    Oversize,
}

/// Locate and bounded-read a pkg's `scene.json` entry, mirroring exactly
/// the sequence `kwe-scene-renderer`'s `load_scene` and
/// `kwe-core::pkg::preflight_pkg` both use: `PkgReader::open`,
/// `scene_json_entry` to find the descriptor, a static size check against
/// `MAX_SCENE_JSON_BYTES`, then `read_entry_bounded` with that same cap.
fn read_pkg_scene_json(path: &Path) -> PkgSceneJson {
    let Ok(reader) = PkgReader::open(path) else {
        return PkgSceneJson::Missing;
    };
    let Ok(scene_idx) = scene_json_entry(reader.entries()) else {
        return PkgSceneJson::Missing;
    };
    if reader.entries()[scene_idx].size > MAX_SCENE_JSON_BYTES {
        return PkgSceneJson::Oversize;
    }
    match reader.read_entry_bounded(scene_idx, MAX_SCENE_JSON_BYTES) {
        Ok(bytes) => PkgSceneJson::Bytes(bytes),
        Err(_) => PkgSceneJson::Missing,
    }
}

/// Bounded re-read of `path` for the JsonDir kind's scene.json, mirroring
/// `hash_bounded`'s TOCTOU-safe open (lstat reject-symlink, O_NOFOLLOW).
fn read_bytes_bounded(path: &Path, max_bytes: u64) -> Result<Vec<u8>, ()> {
    let meta = fs::symlink_metadata(path).map_err(|_| ())?;
    if meta.file_type().is_symlink() || !meta.file_type().is_file() {
        return Err(());
    }
    let mut file = fs::OpenOptions::new()
        .read(true)
        .custom_flags(libc::O_CLOEXEC | libc::O_NOFOLLOW)
        .open(path)
        .map_err(|_| ())?;
    let mut buffer = Vec::new();
    (&mut file)
        .take(max_bytes.saturating_add(1))
        .read_to_end(&mut buffer)
        .map_err(|_| ())?;
    if buffer.len() as u64 > max_bytes {
        return Err(());
    }
    Ok(buffer)
}

/// Run the SR-0c object-family walk over `scene_bytes` and finish the
/// record. `is_pkg` adds `scene.package` to `detected` (§2: "on successful
/// entry read" — even when the bytes then fail to parse as JSON, the pkg
/// entry itself was still successfully located and read).
#[allow(clippy::too_many_arguments)]
fn inventoried_record(
    kind: &str,
    hash: &str,
    source_bytes: u64,
    scene_bytes: &[u8],
    deadline: Instant,
    start: Instant,
    is_pkg: bool,
) -> Value {
    match inventory_scene_json(scene_bytes, &InventoryCaps::default(), deadline) {
        Err(InventoryError::Parse) => build_record(
            "incompatible",
            "parse-error",
            kind,
            hash,
            source_bytes,
            elapsed_ms(start),
            &[],
            &package_only_inventory(is_pkg),
        ),
        Ok(mut inventory) => {
            if is_pkg {
                add_scene_package(&mut inventory);
            }
            let (outcome, reason) = if inventory.limits_hit.contains(&"timeout") {
                ("unknown", "timeout")
            } else {
                ("inventoried", "ok")
            };
            let limits_hit = inventory.limits_hit.clone();
            build_record(
                outcome,
                reason,
                kind,
                hash,
                source_bytes,
                elapsed_ms(start),
                &limits_hit,
                &inventory,
            )
        }
    }
}

/// `Inventory::default()`, optionally carrying only the `scene.package`
/// entry (the pkg-parse-error case: the entry read, its content did not).
fn package_only_inventory(is_pkg: bool) -> Inventory {
    let mut inventory = Inventory::default();
    if is_pkg {
        add_scene_package(&mut inventory);
    }
    inventory
}

/// Adds `scene.package` (count 1, no object ids) to `detected`, keeping it
/// sorted by capability id like the walk's own output. R2 review: also adds
/// it to `required` — the pkg container format is unconditionally required
/// to render a pkg scene at all, independent of any object's visibility
/// (docs/SCENE_CAPABILITIES.md's `scene.package` taxonomy row) — so this
/// runs on the parse-error path too (`package_only_inventory`): the pkg
/// itself was still read even when its `scene.json` content then failed to
/// parse.
fn add_scene_package(inventory: &mut Inventory) {
    inventory.detected.push(DetectedCapability {
        capability: "scene.package",
        count: 1,
        objects: Vec::new(),
        truncated: false,
    });
    inventory
        .detected
        .sort_by_key(|capability| capability.capability);
    if !inventory
        .required
        .iter()
        .any(|capability| capability == "scene.package")
    {
        let position = inventory
            .required
            .partition_point(|existing| existing.as_str() < "scene.package");
        inventory
            .required
            .insert(position, "scene.package".to_string());
    }
}

/// Serialize `record`, replacing it with a minimal `report-oversize` record
/// when the serialized form would exceed `MAX_REPORT_BYTES`. Returns the
/// bytes to write, newline-terminated.
fn bound_report(record: &Value) -> Vec<u8> {
    let mut bytes = serde_json::to_vec(record).unwrap_or_default();
    if bytes.len() > MAX_REPORT_BYTES {
        eprintln!("event=inspector.report_oversize bytes={}", bytes.len());
        let minimal = build_record(
            "unknown",
            "report-oversize",
            "",
            "",
            0,
            0,
            &[],
            &Inventory::default(),
        );
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
        let oversized = build_record(
            "inventoried",
            "ok",
            "json-dir",
            &huge_hash,
            1,
            1,
            &[],
            &Inventory::default(),
        );
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

    // -------------------------------------------------------------------
    // SR-0c: object-family inventory wired into the record
    // -------------------------------------------------------------------

    /// A JsonDir scene populates `required`/`detected`/`unknown` in the
    /// actual emitted record, not just in the `Inventory` struct.
    #[test]
    fn json_dir_scene_populates_the_record_inventory() {
        let dir = temp_dir("json-dir-inventory");
        fs::write(
            dir.join("scene.json"),
            br#"{"objects":[{"id":1,"image":"a.png"},{"text":"hi","visible":false}],"stray":1}"#,
        )
        .unwrap();

        let record = inspect_input(&dir, 512 * 1024 * 1024, far_deadline(), Instant::now());
        assert_eq!(record["outcome"], "inventoried");
        assert_eq!(record["reason"], "ok");
        assert_eq!(record["required"], json!(["scene.layer.image"]));
        assert_eq!(record["unknown"]["keys"], 1);
        assert_eq!(record["unknown"]["samples"], json!(["stray"]));
        let detected: Vec<&str> = record["detected"]
            .as_array()
            .unwrap()
            .iter()
            .map(|entry| entry["capability"].as_str().unwrap())
            .collect();
        assert!(detected.contains(&"scene.layer.image"));
        assert!(detected.contains(&"scene.layer.text"));

        fs::remove_dir_all(&dir).unwrap();
    }

    /// Invalid JSON in scene.json is `incompatible`/`parse-error`, and the
    /// content hash (computed before the inventory parse ever ran) is
    /// still populated.
    #[test]
    fn malformed_scene_json_is_incompatible_parse_error() {
        let dir = temp_dir("parse-error");
        fs::write(dir.join("scene.json"), b"{not json").unwrap();

        let record = inspect_input(&dir, 512 * 1024 * 1024, far_deadline(), Instant::now());
        assert_eq!(record["outcome"], "incompatible");
        assert_eq!(record["reason"], "parse-error");
        assert!(
            record["content"]["hash"]
                .as_str()
                .unwrap()
                .starts_with("sha256:")
        );

        fs::remove_dir_all(&dir).unwrap();
    }

    /// `build_record` (the digest computation in particular) is pure and
    /// deterministic given identical inputs, isolated from wall-clock
    /// variance the way an end-to-end two-run comparison could not be
    /// (`bounds.wall_ms` legitimately differs run to run and feeds the
    /// digest).
    #[test]
    fn record_building_is_deterministic_including_digest() {
        let scene_bytes = br#"{"objects":[{"id":1,"image":"a.png"},{"text":"hi"}]}"#;
        let inventory_a =
            inventory_scene_json(scene_bytes, &InventoryCaps::default(), far_deadline()).unwrap();
        let inventory_b =
            inventory_scene_json(scene_bytes, &InventoryCaps::default(), far_deadline()).unwrap();
        assert_eq!(inventory_a, inventory_b);

        let record_a = build_record(
            "inventoried",
            "ok",
            "json-dir",
            "sha256:aaaa",
            10,
            5,
            &[],
            &inventory_a,
        );
        let record_b = build_record(
            "inventoried",
            "ok",
            "json-dir",
            "sha256:aaaa",
            10,
            5,
            &[],
            &inventory_b,
        );
        assert_eq!(record_a, record_b);
        assert_eq!(record_a["digest"], record_b["digest"]);
        assert_ne!(record_a["digest"], "");
    }

    /// Mirrors `kwe_core::pkg::testutil::PkgWriter`'s byte layout (that
    /// helper is `#[cfg(test)] pub(crate)` inside kwe-core, so it is not
    /// visible from this crate's tests — see docs/SR0.md SR-0c for the
    /// reasoning). Every fixture built here is read exclusively through
    /// the real `kwe_core::PkgReader`; this only ever writes bytes.
    fn build_pkg(entries: &[(&str, &[u8])]) -> Vec<u8> {
        let mut out = Vec::new();
        out.extend_from_slice(&8_u32.to_le_bytes());
        out.extend_from_slice(b"PKGV0001");
        out.extend_from_slice(&(entries.len() as u32).to_le_bytes());
        let mut offset: u32 = 0;
        for (path, payload) in entries {
            out.extend_from_slice(&(path.len() as u32).to_le_bytes());
            out.extend_from_slice(path.as_bytes());
            out.extend_from_slice(&offset.to_le_bytes());
            out.extend_from_slice(&(payload.len() as u32).to_le_bytes());
            offset += payload.len() as u32;
        }
        for (_, payload) in entries {
            out.extend_from_slice(payload);
        }
        out
    }

    /// A pkg carrying a `scene.json` with one image object detects both
    /// `scene.package` (the container itself) and `scene.layer.image`.
    #[test]
    fn pkg_with_scene_json_detects_package_and_image() {
        let dir = temp_dir("pkg-image");
        let scene_json: &[u8] = br#"{"objects":[{"id":1,"image":"textures/a.png"}]}"#;
        let pkg_path = dir.join("scene.pkg");
        fs::write(&pkg_path, build_pkg(&[("scene.json", scene_json)])).unwrap();

        let record = inspect_input(&pkg_path, 512 * 1024 * 1024, far_deadline(), Instant::now());
        assert_eq!(record["outcome"], "inventoried", "{record}");
        assert_eq!(record["reason"], "ok");
        assert_eq!(record["content"]["kind"], "pkg");
        let capabilities: Vec<&str> = record["detected"]
            .as_array()
            .unwrap()
            .iter()
            .map(|entry| entry["capability"].as_str().unwrap())
            .collect();
        assert!(capabilities.contains(&"scene.package"), "{capabilities:?}");
        assert!(
            capabilities.contains(&"scene.layer.image"),
            "{capabilities:?}"
        );
        // R2 review: the pkg container format is unconditionally required
        // to render a pkg scene, independent of any object's visibility.
        assert_eq!(
            record["required"],
            json!(["scene.layer.image", "scene.package"])
        );

        fs::remove_dir_all(&dir).unwrap();
    }

    /// R2 review: a pkg whose `scene.json` entry reads successfully but is
    /// not valid JSON still requires `scene.package` — the entry was read,
    /// only its content failed to parse.
    #[test]
    fn pkg_with_unparseable_scene_json_still_requires_scene_package() {
        let dir = temp_dir("pkg-unparseable");
        let pkg_path = dir.join("scene.pkg");
        fs::write(&pkg_path, build_pkg(&[("scene.json", b"{not json")])).unwrap();

        let record = inspect_input(&pkg_path, 512 * 1024 * 1024, far_deadline(), Instant::now());
        assert_eq!(record["outcome"], "incompatible", "{record}");
        assert_eq!(record["reason"], "parse-error");
        assert_eq!(record["required"], json!(["scene.package"]));
        let capabilities: Vec<&str> = record["detected"]
            .as_array()
            .unwrap()
            .iter()
            .map(|entry| entry["capability"].as_str().unwrap())
            .collect();
        assert_eq!(capabilities, vec!["scene.package"]);

        fs::remove_dir_all(&dir).unwrap();
    }

    /// A pkg with no `scene.json` entry is refused typed with a
    /// distinguishing limits_hit code.
    #[test]
    fn pkg_without_scene_json_is_parse_error() {
        let dir = temp_dir("pkg-missing");
        let pkg_path = dir.join("scene.pkg");
        fs::write(&pkg_path, build_pkg(&[("readme.txt", b"hello")])).unwrap();

        let record = inspect_input(&pkg_path, 512 * 1024 * 1024, far_deadline(), Instant::now());
        assert_eq!(record["outcome"], "incompatible", "{record}");
        assert_eq!(record["reason"], "parse-error");
        assert_eq!(record["bounds"]["limits_hit"], json!(["pkg-no-scene-json"]));
        // The pkg file itself still hashed fine; only the descriptor lookup
        // failed.
        assert!(
            record["content"]["hash"]
                .as_str()
                .unwrap()
                .starts_with("sha256:")
        );
        // R2 review boundary: no entry was ever read, so — unlike
        // `pkg_with_unparseable_scene_json_still_requires_scene_package` —
        // `scene.package` never appears here at all.
        assert_eq!(record["required"], json!([]));
        assert_eq!(record["detected"], json!([]));

        fs::remove_dir_all(&dir).unwrap();
    }
}
