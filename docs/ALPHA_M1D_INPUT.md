# Alpha M1d-B normalized pointer position

M1d-B completes the safe pointer-position transport from the standalone Qt
display harness through the daemon supervisor to the generated renderer. The
renderer acknowledges each observed event and draws a small crosshair at its
latest position, providing a synthetic visual fixture.

The transport is intentionally passive. The display surface accepts no mouse
buttons or touch events, so it cannot take Plasma's right-click menu, long
press, desktop-icon selection, or edit-mode gestures. Button interaction
remains the separate P1 `runtime.pointer-buttons` capability and will require
an explicit per-wallpaper interaction mode.

Run the isolated verification suites:

```sh
scripts/smoke-supervisor.sh
scripts/smoke-frame-transport.sh
scripts/smoke-input-preview.sh
```

The supervisor suite proves renderer acknowledgement, deterministic coordinate
quantization, no-active rejection, stale-generation rejection, malformed
coordinate rejection, and routing to the active worker while a candidate is
being tested. The frame suite proves the passive hover additions do not regress
live, frozen, corrupt, or exited renderer display behavior.

Neither script installs a Plasma package or changes the live wallpaper. M1e
now extracts these types into `org.kde.kwe.display`, adds bounded display
status/ack, and stages the minimal Plasma shell; see `docs/ALPHA_M1E.md`.
