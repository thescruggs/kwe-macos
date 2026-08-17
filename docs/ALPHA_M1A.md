# Alpha M1a safe-display harness

M1a proves that a wallpaper renderer can fail without sharing a process or
pixel buffer with the display client. It does not install or modify a Plasma
wallpaper plugin.

## Run the harness

```sh
./scripts/dev-frame-demo.sh
```

Choose one deliberate renderer failure before launching:

```sh
KWE_FRAME_FAULT=hang ./scripts/dev-frame-demo.sh
KWE_FRAME_FAULT=corrupt ./scripts/dev-frame-demo.sh
KWE_FRAME_FAULT=exit ./scripts/dev-frame-demo.sh
```

The live pattern contains a checker/gradient grid and moving vertical marker.
After a failure, motion stops, the last validated frame remains, and the header
shows an icon plus `Renderer stalled` or `Invalid frame transport` text.

Run all four offscreen states after building:

```sh
KWE_RUN_FRAME_SMOKE=1 ./scripts/check.sh
```

## Acceptance evidence

Validated on 2026-08-16:

| Case | Injection | Observed result |
|---|---|---|
| live | external producer at 30 FPS | valid 960×540 frames and advancing sequence |
| hang | publish stops after frame 10 | frozen warning after 1.5 s; frame 10 retained |
| corruption | magic cleared after frame 300 | header rejected; frame 300 retained |
| abrupt exit | process exits with code 70 after frame 150 | frozen warning; frame 150 retained |

Protocol tests also exercise bounded layout arithmetic, refusal to replace an
existing file, stable writer/reader round trip, corruption rejection, and 100
concurrent publications without accepting a torn frame.

## Remaining M1 work

- M1b now provides daemon-owned worker launch, deadlines, kill/reap, bounded
  restart, quarantine records, and a static last-known-good image; see
  `docs/ALPHA_M1B.md`;
- add transactional two-worker canary promotion and rollback;
- add systemd/cgroup resource limits and controlled memory-pressure testing;
- add a negotiated DMA-BUF path while retaining mmap as fallback;
- define input forwarding without stealing Plasma desktop actions;
- package the minimal Plasma 6 display item and run destructive kill/hang/OOM
  testing while verifying the `plasmashell` PID never changes.

The standalone consumer is intentionally more diagnostic than the eventual
desktop surface. Its validation logic will be extracted into the thin bridge,
while detailed messages stay in the manager/activity UI.
