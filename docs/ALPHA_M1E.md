# Alpha M1e reusable Plasma display bridge

M1e supplies the first Plasma 6 wallpaper package and the reusable
`org.kde.kwe.display` QML module. The package remains a display/input client:
all wallpaper parsing, rendering, supervision, Steam access, audio processing,
playlist policy, and persistent state stay outside `plasmashell`.

## Installed components

The QML module exports:

- `FrameSurface`, which opens only regular non-symlink frame files, validates
  frame protocol v1 and its fixed limits, copies stable frames into private
  image storage, and retains the last good pixels after failure;
- `DisplaySession`, which polls `renderer.status` with one request in flight, a
  64 KiB response ceiling, a one-second timeout, and a fixed 500 ms cadence;
- `InputClient`, which forwards the latest passive normalized pointer event
  with a 64 KiB reply ceiling and one-second timeout.

The `org.kde.kwe.wallpaper` package is rooted in Plasma 6 `WallpaperItem`. It
shows an icon plus text for waiting, stopped, frozen, invalid, unavailable,
rolled-back, and quarantined states. `FrameSurface` accepts no mouse buttons or
touch events, takes no keyboard focus, and leaves Plasma's desktop icons,
context menu, long press, and edit gestures untouched.

`DisplaySession` never starts or retries a renderer. It only observes the
daemon's published active source. During a transactional replacement, it sends
`renderer.ack` only after `FrameSurface` safely opens and validates the exact
frame path associated with that display generation. It uses bounded positioned
reads rather than exposing a mutable mapping to `plasmashell`, so concurrent
file truncation becomes a rejected short read instead of `SIGBUS`. An unavailable or invalid
service response disables new input but does not erase the surface's private
last-good frame.

## Safe verification

Run the local-socket unit tests and isolated integration:

```sh
build/cmake/modules/org/kde/kwe/display/tests/kwe-display-session-test
scripts/smoke-plasma-display.sh
```

The smoke script:

1. installs only into a temporary staging prefix;
2. checks package metadata and generated QML plugin/type files;
3. runs `qmllint` and `kpackagetool6 --hash` without installing the package;
4. starts its own daemon and two synthetic renderers beneath a temporary
   directory;
5. proves the second renderer remains in acknowledged handoff until the
   offscreen display client validates and reads it;
6. proves generation-bound passive input reaches only the promoted worker.

Qt tests cover missing service, malformed JSON, oversized display replies,
validated-generation acknowledgement, pointer request timeout, and oversized
pointer replies. Existing frame smoke tests cover live, hang/freeze, corrupt
header, and abrupt renderer exit while retaining the last good image.

No test in this milestone installs, selects, or loads the wallpaper in the
user's live Plasma session. That destructive reliability gate remains a
separate explicitly authorized test after manager-driven install/uninstall and
safe-mode recovery exist.

## Provenance and compatibility

The bridge implementation and QML package are original. KDE's Plasma 6.7.4
package and `WallpaperItem` documentation define the public package interface;
Qt's reusable QML module documentation defines the build/deployment mechanism.
No KDE wallpaper implementation, catsout plugin, Waywallen code, Open
Wallpaper Engine code, or other upstream source was copied or adapted.

This completes the executable display boundary for `runtime.pointer-position`.
It does not yet complete `display.independent`: the manager still needs stable
output identity, assignment, installation consent, and safe rollback controls.
