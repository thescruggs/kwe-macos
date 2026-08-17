# Alpha M3d — preflight diagnostics in the manager

M3d surfaces scanner diagnostics in the gallery and detail pane. Invalid or
unsafe projects remain visible, but their bounded diagnostic messages are now
readable before a user attempts a future Apply action. This is deliberately
separate from supervisor quarantine: persisted runtime failures remain owned by
the daemon and never get silently cleared by a catalog refresh.
