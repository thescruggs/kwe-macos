# Alpha M1b supervised renderer

M1b moves generated-renderer lifecycle and recovery into `kwe-daemon` without
changing the M1a renderer or loading code into Plasma.

## Run the fault suite

```sh
scripts/smoke-supervisor.sh
```

The script builds the workspace, uses a private temporary socket/runtime/state
tree, and removes it on exit. It does not install a wallpaper package or touch
the running Plasma session.

To include it in the full check:

```sh
KWE_RUN_SUPERVISOR_SMOKE=1 ./scripts/check.sh
```

## Acceptance evidence

Validated on 2026-08-16:

| Case | Expected containment | Result |
|---|---|---|
| healthy | live frames plus static fallback | reached `live`; valid P6 last-good still persisted |
| explicit stop | bounded terminate and reap | stopped with no remaining worker |
| ignores `SIGTERM` | forced-kill fallback | process group killed and reaped after 80 ms test grace |
| frame stall | progress deadline | three bounded attempts, then `frame_timeout` quarantine |
| corrupt header | protocol rejection | three bounded attempts, then `invalid_frame` quarantine |
| abrupt exit | exit observation | three bounded attempts, then `process_exit` quarantine |
| pre-frame stall | startup deadline | three bounded attempts, then `startup_timeout` quarantine |
| explicit retry | user-authorized recovery | quarantine cleared; unchanged identity reached `live` |
| daemon killed | no orphan renderer | Linux parent-death signal removed the live worker |
| daemon restart | persistent safety record | unchanged identity remained quarantined with no child PID |

The workspace currently has 23 Rust unit tests in addition to this process
fault suite and the M1a display-transport suite.

## Remaining safe-display work

- M1c now provides the two-worker canary/promote/rollback transaction and
  acknowledged display-generation handoff; see `docs/ALPHA_M1C.md`;
- systemd user-unit and cgroup resource enforcement, including a controlled
  memory-pressure/OOM lane;
- a display-control handshake that lets the thin client retain the current mmap
  and static fallback across daemon reconnection;
- normalized pointer forwarding and desktop-gesture preservation;
- a minimal Plasma 6 wallpaper bridge followed by destructive tests that
  assert the `plasmashell` PID never changes.

Renderer performance optimization is intentionally deferred until after the
initial release and tracked in
`docs/backlog/POST_RELEASE_RENDERER_OPTIMIZATION.md`.
