# M5k task contract: daemon-owned playlist session with recovery

## Goal and user-visible outcome

Playlist state lives in the daemon and survives daemon restarts, monitor
hotplug (catalog churn), suspend/resume, and missing removable Steam
libraries, satisfying the M5 exit gate. The manager edits the same
playlists it did before; its pre-M5k local settings are migrated once and
left untouched afterwards.

## Scope

In scope:

- `kwe-core`: validated, fail-closed `PlaylistRuntimeSnapshot` with
  `snapshot()`/`restore()` (remaining durations only, re-anchored at
  restore), durable `PlaylistStore::save` (fsync + stale-temp sweep),
  unknown-field rejection on playlist JSON.
- `kwe-daemon`: bounded playlist session owning `playlists-v1.json`
  (definitions) and `playlist-runtime-v1.json` (per-playlist snapshots,
  schema-versioned, bounded with non-active eviction); monotonic tick
  cadence that survives continuous command polling; unavailable sets
  derived from the catalog plus supervisor quarantine records; legacy
  import with bounded title-derived ids; test-only suspend simulation
  behind `--allow-test-faults`.
- Protocol (additive, v1): `playlist.list`, `playlist.put`,
  `playlist.remove`, `playlist.activate`, `playlist.status`,
  `playlist.import` (4 MiB + 1 KiB cap), `playlist.debug-clock-skip`.
- Manager: `PlaylistClient` with bounded offline queue (64 ops,
  5 s→30 s backoff) and a migrated `PlaylistController`; QML surface
  unchanged; new names capped at 128 characters.
- Smoke suite `scripts/smoke-playlist-restart.sh` covering all four exit
  gate scenarios plus corrupt-store containment.

Out of scope:

- renderer start/stop/apply, display assignment, or transition rendering
  (unchanged from M5d–M5j);
- policy signal adapters (KWin, logind, UPower, MPRIS) and the local-time
  policy adapter;
- live Plasma modification;
- a SIGTERM handler for final-state capture (last transition is persisted
  before SIGTERM, so at most one in-flight decision is re-derived).

## Files and modules

- `crates/kwe-core/src/playlist.rs`, `playlist_runtime.rs`, `lib.rs`
- `crates/kwe-daemon/src/playlist_session.rs` (new), `persist.rs` (new),
  `supervisor.rs`, `main.rs`
- `apps/kwe-manager/src/playlistclient.{h,cpp}` (new),
  `playlistcontroller.{h,cpp}`, `main.cpp`, `CMakeLists.txt`,
  `tests/playlistcontrollertest.cpp`
- `scripts/smoke-playlist-restart.sh` (new), `scripts/check.sh`
- M5 project, compatibility, protocol, and alpha documentation

## Acceptance and failure criteria

- After SIGTERM and restart with the same state directory, the active
  playlist, current wallpaper, and remaining time (re-anchored, downtime
  not charged) are reported identically within one tick.
- A 60-second suspend simulation preserves the remaining time within one
  tick; clock regression remains a hard error.
- A playlist entry whose library disappears is skipped deterministically
  and becomes eligible again after a rescan finds it.
- Quarantined wallpaper ids are skipped; clearing the quarantine makes
  them eligible again.
- A corrupt runtime-state file is renamed `.invalid-*`, the daemon stays
  up, and the session restarts fresh. A corrupt definitions store
  disables playlist methods and reports `store_health: "corrupt"` while
  `health`, `catalog`, and `renderer.*` keep working.
- Import merges only into an empty store, derives bounded ids from
  titles with deterministic collision suffixes, and reports rejected
  entries.

## Protocol, compatibility, and recovery impact

Additive v1 methods; `API_VERSION` unchanged. `playlist.import` is the
only request capped above 64 KiB. The manager's QSettings blob becomes
read-only migration input flagged by `playlists/migrated`. Playlist
definitions and runtime snapshots are daemon-owned; the Plasma package
still owns nothing.

## Provenance

Original implementation with no new dependencies or upstream source use.
