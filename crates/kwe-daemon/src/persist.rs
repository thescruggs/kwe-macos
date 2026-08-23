// SPDX-License-Identifier: GPL-3.0-or-later
//! Shared private-directory and atomic-write helpers for daemon state files.
//! Extracted from the supervisor so playlist state persists with the same
//! guarantees (real directory, 0700, 0600 files, no symlink following,
//! content fsync before rename, parent fsync after).

use std::{
    fs::{self, File, OpenOptions},
    io::Write,
    os::unix::fs::{MetadataExt, OpenOptionsExt, PermissionsExt},
    path::Path,
    time::{SystemTime, UNIX_EPOCH},
};

use anyhow::{Context, Result, bail};

/// Creates (or verifies) a private state directory owned by the current user.
pub fn ensure_private_dir(path: &Path) -> Result<()> {
    fs::create_dir_all(path).with_context(|| format!("create {}", path.display()))?;
    let metadata = fs::symlink_metadata(path)?;
    if metadata.file_type().is_symlink() || !metadata.is_dir() {
        bail!("private path must be a real directory: {}", path.display());
    }
    // SAFETY: geteuid has no preconditions.
    if metadata.uid() != unsafe { libc::geteuid() } {
        bail!(
            "private path is not owned by the current user: {}",
            path.display()
        );
    }
    fs::set_permissions(path, fs::Permissions::from_mode(0o700))?;
    Ok(())
}

/// Writes `contents` to `path` atomically: unique temp file in the same
/// directory, fsync of contents, rename over the target, then a parent
/// directory fsync so the rename survives a crash.
pub fn atomic_write(path: &Path, contents: &[u8]) -> Result<()> {
    let parent = path.parent().context("atomic-write path has no parent")?;
    ensure_private_dir(parent)?;
    let temporary = parent.join(format!(
        ".{}.tmp-{}-{}",
        path.file_name()
            .and_then(|name| name.to_str())
            .unwrap_or("state"),
        std::process::id(),
        unix_nanos()
    ));
    let mut file = OpenOptions::new()
        .write(true)
        .create_new(true)
        .mode(0o600)
        .custom_flags(libc::O_CLOEXEC | libc::O_NOFOLLOW)
        .open(&temporary)?;
    file.write_all(contents)?;
    file.sync_all()?;
    fs::rename(&temporary, path)?;
    File::open(parent)?.sync_all()?;
    Ok(())
}

pub(crate) fn unix_nanos() -> u128 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_nanos()
}

pub(crate) fn unix_seconds() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_secs()
}

/// The newest `.invalid-*` siblings of a state file that are kept; older ones
/// are pruned so repeated corruption cannot accumulate quarantine files
/// without bound across daemon restarts.
const MAX_INVALID_SIBLINGS: usize = 8;

/// Moves an invalid state file to a `.invalid-<unix>-<nanos>` sibling so the
/// data is preserved for diagnosis while the service restarts fresh. The
/// nanosecond suffix keeps repeated quarantines within the same second from
/// colliding (a best-effort rename to an existing name would silently drop
/// the second file). Afterwards the sibling set of the same base name is
/// pruned to the newest `MAX_INVALID_SIBLINGS` (the names carry the
/// timestamps, so lexicographic order is chronological). Best-effort: a
/// failing rename or prune is logged, never fatal.
pub(crate) fn quarantine_invalid_state(path: &Path) {
    let name = path
        .file_name()
        .and_then(|name| name.to_str())
        .unwrap_or("state");
    let quarantined = path.with_file_name(format!(
        "{name}.invalid-{}-{}",
        unix_seconds(),
        unix_nanos()
    ));
    if let Err(error) = fs::rename(path, &quarantined) {
        eprintln!(
            "event=state.quarantine_error path={} detail={error}",
            path.display()
        );
        return;
    }
    eprintln!("event=state.quarantined path={}", quarantined.display());
    let Some(parent) = path.parent() else { return };
    let Ok(entries) = fs::read_dir(parent) else {
        return;
    };
    let prefix = format!("{name}.invalid-");
    let mut siblings: Vec<_> = entries
        .filter_map(|entry| entry.ok())
        .map(|entry| entry.path())
        .filter(|path| {
            path.file_name()
                .and_then(|name| name.to_str())
                .is_some_and(|name| name.starts_with(&prefix))
        })
        .collect();
    siblings.sort();
    while siblings.len() > MAX_INVALID_SIBLINGS {
        let oldest = siblings.remove(0);
        if let Err(error) = fs::remove_file(&oldest) {
            eprintln!(
                "event=state.quarantine_prune_error path={} detail={error}",
                oldest.display()
            );
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::path::PathBuf;

    fn temporary_directory(label: &str) -> PathBuf {
        let dir = std::env::temp_dir().join(format!(
            "kwe-persist-{label}-{}-{}",
            std::process::id(),
            unix_nanos()
        ));
        let _ = fs::remove_dir_all(&dir);
        fs::create_dir_all(&dir).unwrap();
        dir
    }

    #[test]
    fn quarantine_prunes_old_invalid_siblings_to_the_bound() {
        let root = temporary_directory("prune");
        for _ in 0..12 {
            let path = root.join("state.json");
            fs::write(&path, b"junk").unwrap();
            quarantine_invalid_state(&path);
            // The quarantine consumed the live file every time.
            assert!(!path.exists(), "the live file must be consumed");
            let count = fs::read_dir(&root).unwrap().count();
            assert!(count <= MAX_INVALID_SIBLINGS, "grew to {count} siblings");
        }
        let names: Vec<_> = fs::read_dir(&root)
            .unwrap()
            .map(|entry| entry.unwrap().file_name())
            .collect();
        assert_eq!(names.len(), MAX_INVALID_SIBLINGS);
        // Every survivor is a quarantine of the same base name (the 4 oldest
        // were pruned; the names carry the timestamps, so the survivors are
        // the newest quarantines).
        for name in &names {
            let name = name.to_string_lossy();
            assert!(
                name.starts_with("state.json.invalid-"),
                "unexpected sibling {name}"
            );
        }
        let _ = fs::remove_dir_all(&root);
    }
}
