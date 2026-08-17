# Alpha M5g — timed and recoverable playlist selection

M5g completes the bounded selection contract needed by a future playlist
runner:

- Playlist duration is limited to 10 seconds–24 hours.
- Optional crossfades are limited to 10 seconds.
- Existing M5f playlists migrate to a five-minute duration and no transition.
- Ordered selection scans no more than one playlist cycle.
- Seeded shuffle is deterministic and avoids an immediate repeat when another
  eligible entry exists.
- Callers can exclude unavailable or quarantined IDs; no eligible entry returns
  no selection instead of retrying indefinitely.

The manager exposes keyboard-accessible duration and transition controls and
fails closed when its stored JSON is malformed, oversized, duplicated, or over
the playlist limits. Display assignment and timer-driven renderer activation
remain disabled, so this slice does not change the live Plasma session.

Automated evidence is provided by the Rust playlist tests and
`kwe-playlist-controller-test`. This advances `playlist.ordered-shuffle` and
`playlist.timer` without yet claiming complete parity.
