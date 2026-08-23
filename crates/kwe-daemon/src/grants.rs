// SPDX-License-Identifier: GPL-3.0-or-later
//! Daemon-owned per-wallpaper permission grants (BETA_M2c).
//!
//! The grant record for a wallpaper answers the three known permissions —
//! `network`, `audio`, and `pointer` — and lives in `permissions-v1.json`
//! beside `supervisor-v1.json` in the daemon's private state directory. The
//! record is the production gate: the supervisor appends `--allow-network`
//! to a web worker's argv exactly when its wallpaper's record grants network,
//! and drops audio frames for wallpapers without the audio grant. Pointer
//! pass-through stays enabled for every wallpaper; the pointer grant exists
//! for future stricter modes.
//!
//! Default policy when no record exists: network off, audio off, pointer on
//! (pointer is core interactivity). Persistence is atomic (`persist::atomic_write`);
//! a corrupt file is quarantined with the rename-to-invalid pattern and the
//! store starts fresh, mirroring `playlist_session::load_runtime_state`.

use std::{
    collections::BTreeMap,
    fs::OpenOptions,
    io::Read,
    os::unix::fs::OpenOptionsExt,
    path::{Path, PathBuf},
};

use anyhow::{Result, bail};
use serde::{Deserialize, Serialize};

use crate::persist::{atomic_write, ensure_private_dir, quarantine_invalid_state};
use crate::supervisor::validate_identity_part;

#[cfg(test)]
use std::fs;

const GRANTS_FILE: &str = "permissions-v1.json";
const MAX_GRANTS: usize = 256;
/// Serialized store bound: 256 identity-bounded records can never reach this,
/// so the cap is pure defense in depth (mirrors MAX_STATE_BYTES).
const MAX_GRANT_BYTES: u64 = 1024 * 1024;

/// The effective grant record for one wallpaper. Bools only; unknown fields
/// are rejected so a future format cannot silently change policy.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Grant {
    pub network: bool,
    pub audio: bool,
    pub pointer: bool,
}

impl Grant {
    /// The documented default policy for a wallpaper with no record: network
    /// off, audio off, pointer on (pointer is core interactivity).
    pub fn defaults() -> Self {
        Self {
            network: false,
            audio: false,
            pointer: true,
        }
    }
}

/// Partial `permissions.set` inputs: provided fields replace their current
/// values, omitted fields keep them.
#[derive(Debug, Clone, Copy, Default)]
pub struct GrantPatch {
    pub network: Option<bool>,
    pub audio: Option<bool>,
    pub pointer: Option<bool>,
}

impl GrantPatch {
    fn apply(self, current: Grant) -> Grant {
        Grant {
            network: self.network.unwrap_or(current.network),
            audio: self.audio.unwrap_or(current.audio),
            pointer: self.pointer.unwrap_or(current.pointer),
        }
    }
}

#[derive(Debug, Clone, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
struct PersistedGrants {
    schema_version: u32,
    #[serde(default)]
    grants: BTreeMap<String, Grant>,
}

impl Default for PersistedGrants {
    fn default() -> Self {
        Self {
            schema_version: 1,
            grants: BTreeMap::new(),
        }
    }
}

/// The grants file on disk plus its in-memory state. All mutations go through
/// `set`, which persists atomically and only commits the change once the
/// write succeeded, so a failed save never leaves a partial record.
pub struct GrantStore {
    path: PathBuf,
    state: PersistedGrants,
}

impl GrantStore {
    /// Opens (or creates) the grants file in `directory`. A corrupt file —
    /// oversize, unparsable, unsupported schema, an unknown field, an invalid
    /// wallpaper id, or more than `MAX_GRANTS` records — is quarantined to
    /// `<file>.invalid-<unix_seconds>` and the store starts fresh (logged once
    /// per event).
    pub fn open(directory: &Path) -> Result<Self> {
        ensure_private_dir(directory)?;
        let path = directory.join(GRANTS_FILE);
        let state = Self::load(&path);
        Ok(Self { path, state })
    }

    /// The effective record for a wallpaper: the stored record, or the
    /// documented defaults when none exists.
    pub fn grant(&self, wallpaper_id: &str) -> Grant {
        self.state
            .grants
            .get(wallpaper_id)
            .copied()
            .unwrap_or_else(Grant::defaults)
    }

    /// Every stored record (bounded by `MAX_GRANTS`).
    pub fn all(&self) -> &BTreeMap<String, Grant> {
        &self.state.grants
    }

    /// Patches the stored record and persists it atomically. Returns the new
    /// effective record. The wallpaper id is validated like every other
    /// identity part, and the store never grows past `MAX_GRANTS` records.
    pub fn set(&mut self, wallpaper_id: &str, patch: GrantPatch) -> Result<Grant> {
        validate_identity_part("wallpaper_id", wallpaper_id)?;
        let current = self.grant(wallpaper_id);
        let next = patch.apply(current);
        if !self.state.grants.contains_key(wallpaper_id) && self.state.grants.len() >= MAX_GRANTS {
            bail!("permission grant count exceeds the {MAX_GRANTS} safety limit");
        }
        let mut next_state = self.state.clone();
        next_state.grants.insert(wallpaper_id.to_string(), next);
        self.save(&next_state)?;
        self.state = next_state;
        Ok(next)
    }

    fn save(&self, state: &PersistedGrants) -> Result<()> {
        let bytes = serde_json::to_vec_pretty(state)?;
        if bytes.len() as u64 > MAX_GRANT_BYTES {
            bail!("permissions state exceeds {MAX_GRANT_BYTES} bytes");
        }
        atomic_write(&self.path, &bytes)
    }

    fn load(path: &Path) -> PersistedGrants {
        let mut file = match OpenOptions::new()
            .read(true)
            .custom_flags(libc::O_CLOEXEC | libc::O_NOFOLLOW)
            .open(path)
        {
            Ok(file) => file,
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
                return PersistedGrants::default();
            }
            Err(error) => {
                Self::quarantine(path, &format!("open failed: {error}"));
                return PersistedGrants::default();
            }
        };
        let metadata = match file.metadata() {
            Ok(metadata) => metadata,
            Err(error) => {
                Self::quarantine(path, &format!("metadata failed: {error}"));
                return PersistedGrants::default();
            }
        };
        if !metadata.is_file() || metadata.len() > MAX_GRANT_BYTES {
            Self::quarantine(path, "not a bounded regular file");
            return PersistedGrants::default();
        }
        let mut bytes = Vec::with_capacity(metadata.len() as usize);
        if let Err(error) = file.read_to_end(&mut bytes) {
            Self::quarantine(path, &format!("read failed: {error}"));
            return PersistedGrants::default();
        }
        match serde_json::from_slice::<PersistedGrants>(&bytes) {
            Ok(state)
                if state.schema_version == 1
                    && state.grants.len() <= MAX_GRANTS
                    && state
                        .grants
                        .keys()
                        .all(|id| validate_identity_part("wallpaper_id", id).is_ok()) =>
            {
                state
            }
            Ok(_) => {
                Self::quarantine(
                    path,
                    "unsupported schema, unknown field, or invalid wallpaper id",
                );
                PersistedGrants::default()
            }
            Err(error) => {
                Self::quarantine(path, &format!("parse failed: {error}"));
                PersistedGrants::default()
            }
        }
    }

    fn quarantine(path: &Path, reason: &str) {
        eprintln!(
            "event=permissions.state_invalid detail=permissions-v1.json is corrupt ({reason}); quarantining and starting fresh"
        );
        quarantine_invalid_state(path);
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::persist::unix_nanos;

    fn temporary_directory(label: &str) -> PathBuf {
        std::env::temp_dir().join(format!(
            "kwe-grants-{label}-{}-{}",
            std::process::id(),
            unix_nanos()
        ))
    }

    fn invalid_siblings(directory: &Path) -> Vec<String> {
        fs::read_dir(directory)
            .unwrap()
            .filter_map(|entry| entry.ok())
            .map(|entry| entry.file_name().to_string_lossy().into_owned())
            .filter(|name| name.starts_with(&format!("{GRANTS_FILE}.invalid-")))
            .collect()
    }

    #[test]
    fn defaults_are_network_off_audio_off_pointer_on() {
        assert_eq!(
            Grant::defaults(),
            Grant {
                network: false,
                audio: false,
                pointer: true
            }
        );
        let root = temporary_directory("defaults");
        let store = GrantStore::open(&root).unwrap();
        assert_eq!(store.grant("431960-123"), Grant::defaults());
        assert!(store.all().is_empty());
        fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn set_and_patch_semantics_round_trip_through_the_file() {
        let root = temporary_directory("set-patch");
        let mut store = GrantStore::open(&root).unwrap();
        // A partial set keeps the defaults for the omitted fields.
        let record = store
            .set(
                "431960-123",
                GrantPatch {
                    network: Some(true),
                    ..GrantPatch::default()
                },
            )
            .unwrap();
        assert_eq!(
            record,
            Grant {
                network: true,
                audio: false,
                pointer: true
            }
        );
        // A later patch keeps the stored network value.
        let patched = store
            .set(
                "431960-123",
                GrantPatch {
                    audio: Some(true),
                    ..GrantPatch::default()
                },
            )
            .unwrap();
        assert_eq!(
            patched,
            Grant {
                network: true,
                audio: true,
                pointer: true
            }
        );
        assert_eq!(store.grant("431960-123"), patched);
        // A reopen sees the persisted record (atomic write + same schema).
        let reopened = GrantStore::open(&root).unwrap();
        assert_eq!(reopened.grant("431960-123"), patched);
        assert_eq!(reopened.all().len(), 1);
        fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn set_rejects_invalid_wallpaper_ids() {
        let root = temporary_directory("bad-ids");
        let mut store = GrantStore::open(&root).unwrap();
        for id in ["", "../escape", &"x".repeat(129), "bad space", "tab\tid"] {
            let error = format!("{}", store.set(id, GrantPatch::default()).unwrap_err());
            assert!(
                error.contains("wallpaper_id must be 1..=128"),
                "unexpected error for {id:?}: {error}"
            );
        }
        assert!(store.all().is_empty());
        fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn the_257th_record_is_rejected_and_patching_at_the_bound_still_works() {
        let root = temporary_directory("bounds");
        let mut store = GrantStore::open(&root).unwrap();
        for index in 0..MAX_GRANTS {
            store
                .set(&format!("wallpaper-{index:03}"), GrantPatch::default())
                .unwrap();
        }
        assert_eq!(store.all().len(), MAX_GRANTS);
        let error = format!(
            "{}",
            store
                .set("wallpaper-256", GrantPatch::default())
                .unwrap_err()
        );
        assert!(error.contains("safety limit"), "unexpected error: {error}");
        // Patching an existing record at the bound is still allowed.
        let patched = store
            .set(
                "wallpaper-000",
                GrantPatch {
                    network: Some(true),
                    ..GrantPatch::default()
                },
            )
            .unwrap();
        assert!(patched.network);
        assert_eq!(store.all().len(), MAX_GRANTS);
        fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn a_corrupt_file_is_quarantined_and_the_store_starts_fresh() {
        let root = temporary_directory("corrupt");
        fs::create_dir_all(&root).unwrap();
        // Unparsable JSON.
        fs::write(root.join(GRANTS_FILE), b"{not json").unwrap();
        let store = GrantStore::open(&root).unwrap();
        assert!(store.all().is_empty());
        assert_eq!(invalid_siblings(&root).len(), 1);
        // Unknown fields and invalid ids are corrupt too.
        fs::write(
            root.join(GRANTS_FILE),
            r#"{"schema_version":1,"grants":{"a":{"network":true,"audio":false,"pointer":true,"bogus":false}}}"#,
        )
        .unwrap();
        let store = GrantStore::open(&root).unwrap();
        assert!(store.all().is_empty());
        assert_eq!(invalid_siblings(&root).len(), 2);
        fs::write(
            root.join(GRANTS_FILE),
            r#"{"schema_version":1,"grants":{"../escape":{"network":true,"audio":false,"pointer":true}}}"#,
        )
        .unwrap();
        let store = GrantStore::open(&root).unwrap();
        assert!(store.all().is_empty());
        assert_eq!(invalid_siblings(&root).len(), 3);
        // Oversized input is quarantined without reading it all.
        fs::write(
            root.join(GRANTS_FILE),
            vec![b'x'; (MAX_GRANT_BYTES + 1) as usize],
        )
        .unwrap();
        let store = GrantStore::open(&root).unwrap();
        assert!(store.all().is_empty());
        assert_eq!(invalid_siblings(&root).len(), 4);
        // A wrong schema version is corrupt.
        fs::write(
            root.join(GRANTS_FILE),
            r#"{"schema_version":2,"grants":{}}"#,
        )
        .unwrap();
        let store = GrantStore::open(&root).unwrap();
        assert!(store.all().is_empty());
        assert_eq!(invalid_siblings(&root).len(), 5);
        fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn a_failed_save_leaves_the_previous_file_intact() {
        let root = temporary_directory("atomic");
        let mut store = GrantStore::open(&root).unwrap();
        store
            .set(
                "431960-123",
                GrantPatch {
                    network: Some(true),
                    ..GrantPatch::default()
                },
            )
            .unwrap();
        let before = fs::read(root.join(GRANTS_FILE)).unwrap();
        // A state whose serialized form exceeds the byte cap cannot be saved.
        // atomic_write only ever targets a temp file plus rename, so the
        // on-disk file must remain exactly as it was (no partial write).
        let oversized = PersistedGrants {
            schema_version: 1,
            grants: BTreeMap::from([(
                format!("x-{}", "y".repeat(MAX_GRANT_BYTES as usize)),
                Grant::defaults(),
            )]),
        };
        let error = format!("{}", store.save(&oversized).unwrap_err());
        assert!(error.contains("exceeds"), "unexpected error: {error}");
        assert_eq!(fs::read(root.join(GRANTS_FILE)).unwrap(), before);
        // The in-memory state was untouched too: the record still reads back.
        assert!(store.grant("431960-123").network);
        fs::remove_dir_all(root).unwrap();
    }
}
