# KDE Wallpaper Engine

KDE Wallpaper Engine is an experimental KDE Plasma 6 application for browsing,
installing, configuring, and safely running wallpapers owned through Wallpaper
Engine's Steam Workshop.

The project is aimed first at Arch Linux and CachyOS on Plasma Wayland. It is
not affiliated with Valve or Wallpaper Engine/Wallpaper Engine's publisher.
Users must own Wallpaper Engine and obtain Workshop content through Steam; this
project will not redistribute Wallpaper Engine assets or Workshop items.

## Installing

Arch Linux and CachyOS. An AUR recipe lives in `packaging/PKGBUILD`; the
`kde-wallpaper-engine` package is installed once it is published, or built
locally from this repository:

```sh
cd packaging
makepkg -si
```

When run inside this repository the PKGBUILD sources from the local git
checkout (its committed state — commit first if you have uncommitted changes),
so no GitHub access or credentials are needed. Only outside the checkout (AUR
builders) does it fetch the published GitHub repository.

The package installs `kwe-daemon` (supervised renderer daemon), `kwe-manager`
(the Kirigami app), the `kwe` CLI, the `kwe-test-renderer`, `kwe-vulkan`,
`kwe-video-renderer`, `kwe-web-renderer`, and `kwe-audio-worker` renderers and
workers, the staged `org.kde.kwe.wallpaper` Plasma wallpaper package, and a
user systemd unit. After installing, enable the daemon user service:

```sh
systemctl --user enable --now kwe-daemon.service
```

The unit is part of the graphical session, so it starts once the desktop is
up and stops with it. **Upgrading from a release before this change:** the old
enablement symlink points at `default.target` and keeps starting the daemon
before the session exists, which leaves the output picker empty until the
service is restarted. Re-enable it once:

```sh
systemctl --user disable kwe-daemon.service
systemctl --user enable --now kwe-daemon.service
```

The manager also starts the service on demand when it is not running, and
surfaces a manual `systemctl --user start kwe-daemon` hint if activation
fails.

On CachyOS the AUR helper `yay` is already in the `cachyos` repository
(`sudo pacman -S yay`); AUR packages are then installed with `yay -S <name>`.
There is no repository to "add" — `yay` queries the AUR API directly.

Alpha 0.1 is now runnable. It safely indexes installed Wallpaper Engine
Workshop content and presents it in a native Kirigami gallery. It also includes
an isolated Vulkan hardware preflight. Applying is now manager-controlled for
video, web, and scene content: the wallpaper service runs a validated
transaction (renderer start, bounded promotion wait, assignment persist,
Plasma switch) with per-output assignment, and every switch is reversible from
the wallpaper details page ("Reset to image wallpaper" reverts the saved
previous wallpaper config, or resets to the stock image wallpaper when no
saved assignment exists).

Alpha M1a also includes the first safe-display harness: an external generated
frame producer and a standalone native preview using the bounded shared-memory
fallback. Try it without changing Plasma:

```sh
./scripts/dev-frame-demo.sh
KWE_FRAME_FAULT=hang ./scripts/dev-frame-demo.sh
```

Alpha M1b adds daemon-owned process supervision, frame deadlines, bounded
restart/quarantine, forced kill and reap, a persistent static fallback, and
parent-death cleanup. Run its isolated fault matrix with:

```sh
scripts/smoke-supervisor.sh
```

Alpha M1c makes replacement transactional: a candidate cannot displace the
active renderer until it passes a bounded canary, and the previous mapping is
retained until display-generation acknowledgement or timeout.

Alpha M1d-A adds per-renderer Linux resource ceilings, aggregate systemd
budgets, resource-limit diagnostics, and an active-preserving memory-pressure
recovery test. It does not install or load the Plasma bridge.

Alpha M1d-B adds generation-bound normalized pointer position, nonblocking
active-worker routing, bounded renderer acknowledgements, and passive Qt hover
observation that accepts no mouse buttons or touch events.

```sh
scripts/smoke-input-preview.sh
```

Alpha M1e extracts the validated frame and input code into the installable
`org.kde.kwe.display` QML module and adds the original Plasma 6
`org.kde.kwe.wallpaper` package. The package polls and acknowledges the daemon
through bounded IPC, preserves the last copied frame on failure, and presents
a native text-and-icon fallback. Its smoke test stages everything in a
temporary prefix and does not load or change the live desktop:

```sh
scripts/smoke-plasma-display.sh
```

Alpha M2 implements the video and web renderers behind the supervisor:
`kwe-video-renderer` (the M1e libmpv worker) and the sandboxed
`kwe-web-renderer` (bwrap + headless Chromium over the CDP pipe, grant-gated
network and audio, heartbeat-bounded liveness) publish BGRA8888 frames through
the shared frame protocol; the catalog marks web wallpapers renderer-dependent
("sandboxed Chromium worker; network and audio off until granted"). Both
feature-compatibility rows are honestly `partial` — `content.video` (M1e) and
`content.web` / `runtime.audio-web-64` (M2e) — and `kwe diagnose` prints
versioned backend probes for each lane. See [docs/BETA_M2.md](docs/BETA_M2.md)
for the pinned CDP wire contract, the sandbox compromise suite, and the
close-out; rendering never runs inside plasmashell.

```sh
scripts/smoke-web.sh
scripts/smoke-web-compromise.sh
./target/debug/kwe diagnose
```

Playlist work through Alpha M5j provides bounded persistent membership,
shuffle/repeat, duration and transition settings, a monotonic pause-aware
runtime, and deterministic playback/time policy decisions. Alpha M5k moves
playlist state into the daemon: definitions and per-playlist runtime
snapshots survive daemon restarts, monitor hotplug, suspend/resume, and
missing removable Steam libraries, and the manager edits them through the
daemon after a one-time migration. It still does not assign or start
wallpapers; see [Alpha M5g](docs/ALPHA_M5G.md),
[M5h](docs/ALPHA_M5H.md), [M5i](docs/ALPHA_M5I.md),
[M5j](docs/ALPHA_M5J.md), and [M5k](docs/ALPHA_M5K.md).

Alpha M6a adds the Steam-SDK-free Workshop half: a Workshop destination in
the manager sharing the Installed card and details components (subscribed
items only, with download states and an Installed badge), and a bounded
daemon-side offline metadata cache so subscriptions keep their titles,
tags, and kind when Steam libraries are unmounted or the daemon restarts.
Subscription management stays in Steam; see [Alpha M6a](docs/ALPHA_M6A.md).

```sh
./scripts/dev-run.sh
scripts/smoke-playlist-restart.sh
scripts/smoke-workshop-cache.sh
```

See [Alpha 0.1](docs/ALPHA_0_1.md) for requirements, manual commands, known
limits, and cleanup, [Alpha M1a](docs/ALPHA_M1A.md) for the safe-display fault
harness, [Alpha M1b](docs/ALPHA_M1B.md) for supervised recovery, and
[Alpha M1c](docs/ALPHA_M1C.md) for transactional replacement.
[Alpha M1d-A](docs/ALPHA_M1D.md) documents resource containment and its safe
fault-injection procedure, and [Alpha M1d-B](docs/ALPHA_M1D_INPUT.md) documents
the pointer-position slice. [Alpha M1e](docs/ALPHA_M1E.md) documents the
reusable QML module, staged Plasma package, and acknowledged offscreen handoff.
Design references:

- [Project plan](docs/PROJECT_PLAN.md)
- [Architecture](docs/ARCHITECTURE.md)
- [User experience design](docs/UX_DESIGN.md)
- [Wallpaper Engine feature compatibility](docs/FEATURE_COMPATIBILITY.md)
- [Provenance policy](docs/PROVENANCE.md)
- [AI contributor workflow](AGENTS.md)
- [Alpha protocol](docs/PROTOCOL_V1.md)
- [Original Vulkan renderer decision](docs/adr/0001-original-vulkan-renderer.md)
- [Shared frame protocol](docs/FRAME_PROTOCOL_V1.md)
- [Shared frame fallback decision](docs/adr/0002-shared-frame-fallback.md)
- [Renderer supervisor API](docs/SUPERVISOR_API_V1.md)
- [Normalized input protocol](docs/INPUT_PROTOCOL_V1.md)
- [Deferred renderer optimization backlog](docs/backlog/POST_RELEASE_RENDERER_OPTIMIZATION.md)
- [Layered resource containment decision](docs/adr/0003-renderer-resource-containment.md)
- [Thin Plasma display bridge decision](docs/adr/0004-thin-plasma-display-bridge.md)

The defining reliability rule is simple: untrusted wallpaper parsing,
rendering, web content, audio processing, and Steam integration must never run
inside `plasmashell`.

The product standard is equally important: the application should feel like a
first-class KDE app while making Wallpaper Engine compatibility visible and
understandable instead of hiding unsupported behavior.

This repository contains no Workshop payloads or Wallpaper Engine runtime
assets. Users must own Wallpaper Engine and obtain content through Steam.
