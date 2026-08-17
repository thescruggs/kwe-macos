# Alpha M5h — monotonic playlist runtime

M5h adds the bounded runtime state machine behind future playlist playback. It
accepts a caller-provided monotonic millisecond clock and emits explicit
started, waiting, advanced, paused, exhausted, or no-eligible decisions.

Deadlines honor each playlist's M5g duration. Pause/resume preserves remaining
time, clock regression is rejected, and history is capped at the maximum 1,024
playlist entries. Repeat-off shuffle does not replay history. Items that become
missing or quarantined are skipped immediately, while an unplayed item that
later becomes available can still be selected. Playlist reorder/removal is
reconciled by wallpaper identity rather than trusting a stale vector index.

This runtime does not start a renderer, persist desired state, or assign a
display. Those remain separate recovery-gated slices. Rust tests provide the
current evidence for `playlist.timer` and `playlist.ordered-shuffle`.
