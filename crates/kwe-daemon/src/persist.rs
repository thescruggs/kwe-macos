// SPDX-License-Identifier: Apache-2.0
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

/// Moves an invalid state file to a `.invalid-<unix>` sibling so the data is
/// preserved for diagnosis while the service restarts fresh. Best-effort.
pub(crate) fn quarantine_invalid_state(path: &Path) {
    let quarantined = path.with_file_name(format!(
        "{}.invalid-{}",
        path.file_name()
            .and_then(|name| name.to_str())
            .unwrap_or("state"),
        unix_seconds()
    ));
    if let Err(error) = fs::rename(path, &quarantined) {
        eprintln!(
            "event=state.quarantine_error path={} detail={error}",
            path.display()
        );
    } else {
        eprintln!("event=state.quarantined path={}", quarantined.display());
    }
}
