# M5g task contract: timed playlist selection

## Goal and user-visible outcome

Playlist authors can configure a bounded wallpaper duration and transition,
and the policy core can deterministically choose the next usable wallpaper
without selecting entries known to be unavailable or quarantined. Existing
M5f playlists load with safe defaults.

## Scope

In scope:

- backward-compatible duration and transition fields in the bounded playlist
  contract and manager settings;
- deterministic ordered and seeded-shuffle selection;
- no immediate shuffle repeat when another eligible entry exists;
- bounded exclusion of unavailable or quarantined wallpaper IDs;
- keyboard-accessible duration and transition controls in the playlist editor;
- unit tests for bounds, migration, deterministic selection, and fail-closed
  malformed settings.

Out of scope:

- starting, stopping, or assigning a renderer;
- output/group assignment and display topology;
- wall-clock, battery, fullscreen, idle, lock, or application policies;
- animated transition rendering;
- changing the live Plasma wallpaper package or session.

## Files and modules

- `crates/kwe-core/src/playlist.rs`
- `apps/kwe-manager/src/playlistcontroller.h`
- `apps/kwe-manager/src/playlistcontroller.cpp`
- `apps/kwe-manager/qml/Main.qml`
- `apps/kwe-manager/tests/CMakeLists.txt`
- `apps/kwe-manager/tests/playlistcontrollertest.cpp`
- M5 project/alpha documentation

## Acceptance and failure criteria

- Duration is restricted to 10–86,400 seconds and transition duration to
  0–10 seconds; invalid values are rejected or replaced by documented defaults.
- M5f JSON without the new fields loads as 300 seconds with no transition.
- Ordered selection scans at most one playlist cycle and respects repeat.
- Seeded shuffle is deterministic, excludes unavailable/quarantined IDs, and
  avoids the current item when at least one other eligible item exists.
- No eligible item returns no selection rather than retrying indefinitely.
- The manager rejects oversized, malformed, duplicate, and over-limit stored
  playlist data without partially loading it.
- QML controls have explicit accessible names and do not imply that display
  assignment is enabled.
- Workspace format, lint, unit, build, QML lint, and manager Qt tests pass.

## Protocol, compatibility, and recovery impact

No daemon, display, frame, or input protocol changes. This advances
`playlist.ordered-shuffle` and `playlist.timer`, but does not claim parity until
renderer assignment and timer recovery are integrated. Failure is local to the
manager: invalid persisted settings fail closed and leave the renderer and
Plasma untouched.

## Provenance

The implementation is original and uses only standard Rust, Qt, and Kirigami
APIs already present in the project. No new dependency or upstream code is
introduced.
