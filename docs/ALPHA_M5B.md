# Alpha M5b — persistent playlist store

M5b adds an atomic user-local JSON playlist store with bounded file size,
playlist count, and entry limits. Missing stores load as empty; malformed or
oversized stores fail closed instead of being silently overwritten.
