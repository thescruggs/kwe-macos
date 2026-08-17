# M1e task contract: reusable Plasma display bridge

## Goal and user-visible outcome

Provide the first installable Plasma 6 wallpaper package for KDE Wallpaper
Engine. It displays only frames already validated and rendered by the external
worker, forwards passive pointer position through the daemon, and presents a
quiet native fallback when the service or frame transport is unavailable.

The package is built and validated in an isolated staging prefix during this
task. It is not installed or loaded into the user's live Plasma session.

## Scope

In scope:

- extract `FrameSurface` and the local-socket client into the reusable
  `org.kde.kwe.display` Qt QML module;
- add a bounded display-session client for `renderer.status` and
  generation-matched `renderer.ack` on supervisor API v1;
- reopen only when the published frame path or generation changes;
- retain the last copied frame when the daemon, worker, or frame file fails;
- add an original Plasma 6 `Plasma/Wallpaper` package rooted in
  `WallpaperItem`;
- preserve Plasma mouse buttons, touch, right-click, long-press, containment,
  and desktop icons by observing hover position only;
- build-time QML linting plus an offline staged-package smoke test;
- fault tests for unavailable service, malformed/oversized responses, stale
  handoff state, renderer hang, renderer exit, and corrupt frame metadata;
- architecture, compatibility, recovery, packaging, and provenance updates.

Out of scope:

- installing, selecting, restarting, or otherwise changing the live Plasma
  desktop;
- DMA-BUF, renderer optimization, or changes to frame protocol v1;
- mouse buttons, touch, scrolling, keyboard capture, or interaction mode;
- manager-driven per-output assignment and persistent display identity;
- video/scene/web wallpaper parsing, Steam access, audio capture, or playlist
  scheduling in the Plasma process;
- claiming Wallpaper Engine parity beyond the existing synthetic frame and
  normalized-position contracts.

## Acceptance and explicit failure criteria

- `org.kde.kwe.display` is a dynamically loadable, installable QML module with
  generated type metadata; the standalone preview consumes the same module.
- The display session has one bounded local request in flight, a 64 KiB reply
  limit, finite timeouts, bounded polling, exact API/id validation, and no
  automatic renderer start or retry behavior.
- Only a non-empty regular frame file paired with a non-zero display
  generation is published. A generation is acknowledged only after
  `FrameSurface` safely opens and validates that exact file. The Plasma-facing
  fallback uses bounded positioned reads, not a mutable mapping that can
  deliver `SIGBUS` after worker truncation.
- Service loss or invalid replies enter a text-and-icon degraded state without
  clearing the last good pixels or spinning an unbounded retry loop.
- The wallpaper root is Plasma 6 `WallpaperItem`; loading/degraded state is
  visible without relying on color, animation, hover, or sound.
- The QML surface accepts no mouse buttons or touch. Pointer events remain
  passive, normalized, rate-limited, generation-bound, and latest-event-wins.
- Package metadata, QML imports, installed file layout, and
  generated module files pass the isolated smoke test.
- Existing Rust tests, Clippy, C++ build, QML lint, frame transport, supervisor,
  and pointer smoke tests remain green.
- Any test that would install the package into a real user data directory or
  load it in `plasmashell` is an explicit failure of this task's safety scope.

## Files and protocols

Expected areas:

- `modules/org/kde/kwe/display/` for original C++ QML types;
- `apps/kwe-frame-preview/` as the standalone integration harness;
- `plasma/wallpapers/org.kde.kwe.wallpaper/` for the package;
- top-level CMake/install rules, smoke scripts, and M1 documentation.

Supervisor API v1, frame protocol v1, and input protocol v1 remain compatible
and unchanged. This task adds a client implementation, not a new daemon API.

## Accessibility, recovery, compatibility, and provenance

The display surface is not an interactive control and is hidden from the
keyboard focus chain. The degraded overlay has an accessible name and includes
plain-language status text. Normal desktop actions remain owned by Plasma.

Capabilities affected are `runtime.pointer-position` (same synthetic evidence)
and the enabling display boundary for `display.independent`; per-output
identity/assignment remains unimplemented, so `display.independent` is not yet
claimed compatible.

The implementation is original. KDE's official Plasma 6 wallpaper packages
and documentation are interface references for package metadata,
`WallpaperItem`, and install layout. Qt's official reusable-QML-module guidance
is an API reference. No KDE, catsout, Waywallen, Open Wallpaper Engine, or other
upstream code is copied or adapted.
