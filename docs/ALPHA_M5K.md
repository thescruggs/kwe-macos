# Alpha M5k — daemon-owned playlist session

M5k closes the M5 exit gate: playlist state now lives in `kwe-daemon`, and
daemon restarts, monitor hotplug, suspend/resume, and missing removable
Steam libraries recover without losing playlist state.

The daemon owns `playlists-v1.json` (definitions) and
`playlist-runtime-v1.json` (per-playlist runtime snapshots) in its private
state directory. Snapshots store only remaining durations — never absolute
deadlines — so a restart re-anchors the deadline and downtime is not
charged against the current wallpaper. Transitions persist immediately;
waiting states refresh at most every 30 seconds. A corrupt runtime-state
file is quarantined to an `.invalid-*` sibling and the session restarts
fresh; a corrupt definitions store disables playlist methods with an
actionable error while the daemon keeps serving everything else.

The session ticks on a monotonic clock at a fixed cadence (default
500 ms) that survives continuous command polling, and derives the
unavailable set from the catalog plus supervisor quarantine records:
entries whose library is unmounted are skipped deterministically and
become eligible again when a rescan finds them; quarantined wallpapers
are skipped until a successful retry clears the record. Linux monotonic
time excludes suspend, so playback freezes across sleep and resumes with
the remaining time — deterministic, and verified by the fault-gated
`playlist.debug-clock-skip` test hook.

The manager now persists through the daemon. Its pre-M5k QSettings blob
is imported once into an empty store (bounded title-derived ids with
collision suffixes), then left untouched as a backup; offline edits queue
with bounded backoff and surface an actionable notice. No renderer is
started and no display is assigned; those remain separately safety-gated
work.

Run the full recovery matrix with:

```sh
scripts/smoke-playlist-restart.sh
```
