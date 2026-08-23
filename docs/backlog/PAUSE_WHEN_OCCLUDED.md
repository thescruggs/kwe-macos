# Feature: pause rendering when a window is maximized / fullscreen (F3)

- **Requested:** 2026-08-22 (user: "an option to pause when windows are
  maximized / full screen")
- **Status:** DESIGN (not started). Needs a protocol addition and a KWin-side
  detector; sized as its own slice.

## Why it is not a one-liner

Nothing in the current stack knows about windows: the Plasma wallpaper item
only sees the desktop, the daemon talks to KWin only to switch wallpaper
plugins, and the renderers have no pause control (`media.state` is the
user's media-player state forwarded TO wallpapers, not a renderer control).

## Design

1. **Detector (KWin side).** A small KWin script (`kwin-scripting`, packaged
   with the plugin) watches `workspace.windowList()` / `activeWindow` and
   the `fullScreen` / `maximizeMode` / minimized / screen properties, and
   reports per-output "covered" state over the session bus on a
   kwe-owned D-Bus name — or the daemon polls it through KWin's existing
   scripting D-Bus. Must be bounded (debounce 250 ms; one message per
   transition), and must treat "no detector" as "never covered".
2. **Daemon.** A new supervisor command `renderer.pause {generation,
   paused}` (mirrors `media.state`'s generation rule) sends a new input
   line `render_pause <0|1>` to the worker (`kwe-input-protocol` additive
   message, acked like pointer/audio). While paused the supervisor relaxes
   the frame timeout for the active worker (no restart because frames
   stopped on purpose) and the keepalive republish is suppressed. Playlist
   timers keep running (the schedule is wall-clock).
3. **Renderers.** video: `pause` property; web: `Page.stopScreencast` +
   `Emulation.setAutoDarkModeOverride`-free freeze via
   `Page.setWebLifecycleState frozen`, resume restarts the screencast; scene:
   skip render/update ticks; test renderer: accept + idle.
4. **Policy / UI.** Per-wallpaper or global toggle in the manager
   ("Pause when a window covers the desktop"), persisted in the assignment
   (`pause_when_covered: bool`, additive). Default off for the first
   release.
5. **Acceptance.** smoke lane with a fake detector (send pause/resume over
   the API, assert the sequence stops advancing and no failure is recorded,
   resumes on resume); plasmashell PID unchanged; a live check with a
   maximized window.

## Cost note

Until F3 lands, F2's frame-rate limit is the lever for CPU use while
covered.
