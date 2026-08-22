//! Stale-binary detection (BETA B4, `docs/bugs/APPLY_REJECTED_QUARANTINED.md`).
//!
//! A package upgrade replaces `/usr/bin/kwe-daemon` and the renderer
//! binaries on disk, but the running daemon keeps executing the OLD image
//! until something restarts the unit — and the `post_upgrade` hook can
//! only print, it cannot restart a per-user service. The old daemon then
//! supervises NEW renderers whose contracts it does not know: the measured
//! case was a `-4` daemon without the B2 preflight refusal spawning a `-5`
//! scene worker that refused the scene itself (exit 74) three times, which
//! the stale daemon filed as three crashes and quarantined the wallpaper.
//!
//! The daemon captures its own executable identity (device + inode of the
//! resolved path) at startup and re-checks the path on demand. A replaced
//! file has a different inode (package managers install a new file and
//! rename it over the old one — the mapped image keeps the old inode, so
//! `/proc/self/exe` reports `… (deleted)`). While stale, the user-facing
//! `wallpaper.apply` lane refuses with `service_stale` and the exact
//! restart command instead of letting the version skew turn into
//! quarantine records; `health` reports the flag so the manager can show
//! it. The check is a single `stat`, bounded and read-only; nothing here
//! restarts anything by itself (a restart drops the live renderer and the
//! assignment is not re-applied on start today — B5).

use std::{
    fs,
    os::unix::fs::MetadataExt,
    path::{Path, PathBuf},
    sync::OnceLock,
};

/// The daemon's own identity, captured once by `init` in `main`. Unit
/// tests never call `init`, so the check is inert there (no gating on a
/// test binary that cargo may rebuild mid-run).
static INSTALLED: OnceLock<Option<InstalledBinary>> = OnceLock::new();

/// Capture the running daemon's executable identity (idempotent).
pub fn init() -> Option<&'static InstalledBinary> {
    INSTALLED.get_or_init(InstalledBinary::capture).as_ref()
}

/// True when `init` ran and the executable on disk is no longer this one.
pub fn is_stale() -> bool {
    INSTALLED
        .get()
        .and_then(|binary| binary.as_ref())
        .is_some_and(InstalledBinary::is_stale)
}

/// The `service_stale` error payload when the daemon is stale, else None.
pub fn stale_error() -> Option<serde_json::Value> {
    let binary = INSTALLED.get()?.as_ref()?;
    binary.is_stale().then(|| {
        serde_json::json!({
            "error": "service_stale",
            "detail": stale_detail(binary),
        })
    })
}

/// The executable the daemon started from, as observed at startup.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct InstalledBinary {
    path: PathBuf,
    device: u64,
    inode: u64,
}

impl InstalledBinary {
    /// Capture the running executable's identity. `None` when the path or
    /// its metadata cannot be read (unusual; callers then skip the check
    /// rather than refusing applies on a guess).
    pub fn capture() -> Option<Self> {
        let exe = std::env::current_exe().ok()?;
        Self::for_path(&exe)
    }

    /// Identity of `path` as it exists now. The ` (deleted)` suffix the
    /// kernel appends to an unlinked `/proc/self/exe` target is stripped so
    /// the re-check stats the install path, not a name that cannot exist.
    pub fn for_path(path: &Path) -> Option<Self> {
        let text = path.to_string_lossy();
        let trimmed = text.strip_suffix(" (deleted)").unwrap_or(&text);
        let path = PathBuf::from(trimmed);
        let metadata = fs::metadata(&path).ok()?;
        Some(Self {
            device: metadata.dev(),
            inode: metadata.ino(),
            path,
        })
    }

    /// True when the file at the captured path is no longer the one this
    /// process is running (replaced or removed). A transient stat error is
    /// reported as stale too: the install path vanishing is exactly the
    /// mid-upgrade window this guards.
    pub fn is_stale(&self) -> bool {
        match fs::metadata(&self.path) {
            Ok(metadata) => metadata.dev() != self.device || metadata.ino() != self.inode,
            Err(_) => true,
        }
    }

    pub fn path(&self) -> &Path {
        &self.path
    }
}

/// The `service_stale` detail: one sentence with the exact command.
pub fn stale_detail(binary: &InstalledBinary) -> String {
    format!(
        "the wallpaper service binary {} was replaced after the service started (package upgrade); restart it with `systemctl --user daemon-reload && systemctl --user restart kwe-daemon`, then apply again",
        binary.path().display()
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    fn temp_file(name: &str) -> PathBuf {
        let dir =
            std::env::temp_dir().join(format!("kwe-selfcheck-{}-{}", std::process::id(), name));
        let _ = fs::remove_dir_all(&dir);
        fs::create_dir_all(&dir).unwrap();
        let path = dir.join("kwe-daemon");
        fs::write(&path, b"one").unwrap();
        path
    }

    #[test]
    fn unchanged_file_is_not_stale() {
        let path = temp_file("same");
        let binary = InstalledBinary::for_path(&path).unwrap();
        assert!(!binary.is_stale());
        // Rewriting IN PLACE keeps the inode: still the same file.
        fs::write(&path, b"two").unwrap();
        assert!(!binary.is_stale());
    }

    #[test]
    fn replaced_or_removed_file_is_stale() {
        let path = temp_file("replaced");
        let binary = InstalledBinary::for_path(&path).unwrap();
        // Package managers write a sibling and rename it over the target:
        // a new inode at the same path.
        let sibling = path.with_extension("new");
        fs::write(&sibling, b"upgraded").unwrap();
        fs::rename(&sibling, &path).unwrap();
        assert!(binary.is_stale());
        fs::remove_file(&path).unwrap();
        assert!(binary.is_stale());
    }

    #[test]
    fn deleted_suffix_is_stripped_before_stat() {
        let path = temp_file("deleted");
        let decorated = PathBuf::from(format!("{} (deleted)", path.display()));
        let binary = InstalledBinary::for_path(&decorated).unwrap();
        assert_eq!(binary.path(), path.as_path());
        assert!(!binary.is_stale());
    }

    #[test]
    fn capture_sees_the_running_test_binary() {
        let binary = InstalledBinary::capture().expect("test binary exists");
        assert!(!binary.is_stale());
        assert!(stale_detail(&binary).contains("systemctl --user daemon-reload"));
    }
}
