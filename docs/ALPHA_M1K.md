# Alpha M1k — Workshop change history and retry backoff

M1k makes the manager safer to leave running while Steam and the daemon are
changing state:

- Catalog polling remains every five seconds while healthy.
- Transient daemon/socket errors use exponential retry backoff, capped at 30
  seconds, and return to the normal interval after a successful response.
- The client records the ten most recent meaningful Workshop transitions in a
  dismissible history panel (download completion, missing downloads, and state
  changes).
- Snapshot comparison ignores the daemon's volatile scan timestamp, preventing
  false notifications on an otherwise unchanged catalog.

The history is intentionally in-memory for this alpha. It is diagnostic UI
state, not user data, and is cleared explicitly from the manager.
