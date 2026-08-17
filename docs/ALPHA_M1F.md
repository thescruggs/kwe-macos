# Alpha M1f — manager-owned package safety

M1f adds the first manager-owned lifecycle for the user-local Plasma display
package. The manager validates and stages a package transactionally, reports a
recoverable failure, and can disable or restore the package through safe mode.

The implementation lives in `apps/kwe-manager/src/packageinstaller.*`. It is
deliberately separate from the daemon and Plasma display bridge. A package
failure therefore cannot require the renderer or `plasmashell` to be restarted.

## Safety behavior

1. Validate package metadata and the required `contents/ui/main.qml` entry.
2. Reject symlinks and non-regular files.
3. Copy to `.new`, retain the prior package as `.previous`, then promote.
4. On safe mode, rename the active package to `.disabled`.
5. Restore it only through an explicit manager action.

## Validation

`kwe-package-installer-test` covers successful installation, safe-mode
disable/restore, and wrong-package rejection. The manager UI smoke test still
runs against a temporary daemon and does not install into the live Plasma
session.

## Remaining gate

M1f does not apply the wallpaper or restart Plasma. The next safety gate is a
manager-driven staged install followed by an explicitly authorized live
`plasmashell` PID-survival test and rollback exercise.
