# Wallpaper Engine feature compatibility

## Purpose

This is a living parity contract, not a marketing checklist. Each capability
gets a stable ID used by renderer manifests, the compatibility database, UI,
fixtures, and automated tests.

Status values:

- `Target P0` — required for the first useful release.
- `Target P1` — required for a polished 1.0 release.
- `P2` — desirable after 1.0.
- `Backend-dependent` — expose honest per-renderer support.
- `Deferred` — recognized but not scheduled for 1.0.
- `Unsupported` — intentionally excluded for security, licensing, or platform
  reasons.

Official references confirm configurable user properties, audio buffers,
cursor events, media integration, playlists/profiles, runtime controls, and
performance policies:

- <https://docs.wallpaperengine.io/en/web/customization/properties.html>
- <https://docs.wallpaperengine.io/en/scene/scenescript/reference/class/IEngine.html>
- <https://docs.wallpaperengine.io/scene/scenescript/tutorial/cursor>
- <https://docs.wallpaperengine.io/en/web/audio/media.html>
- <https://help.wallpaperengine.io/en/functionality/cli.html>
- <https://help.wallpaperengine.io/en/functionality/wallpaperperapp.html>

## Content and distribution

| ID | Native concept | Target | Linux/KDE behavior |
|---|---|---|---|
| `content.video` | Video wallpapers | P0 | Supervised mpv/libmpv worker with software fallback. |
| `content.scene2d` | Packed 2D scenes | P0, backend-dependent | External scene renderer with feature manifest. |
| `content.scene3d` | 3D models, cameras, lights, shaders | P1, backend-dependent | Report model/effect/shader support individually. |
| `content.web` | HTML/JS/WebGL wallpapers | P1 | Sandboxed external browser process; permissions are explicit. |
| `content.application` | Downloaded executable wallpapers | Unsupported | Never execute arbitrary Workshop programs as wallpapers. |
| `workshop.browse` | Search/filter Workshop | P0 | `partial` (M6a): a Workshop destination lists subscribed items (installed/downloading/awaiting download) with the same card/detail model as Installed; remote paginated browsing remains SDK work. Steam-client fallback per M1g. |
| `workshop.subscribe` | Subscribe/unsubscribe/install/update | P0 | `partial` (M1h/M6a): local VDF subscription-state monitoring with honest Steam-managed subscription actions; the optional Steam bridge remains a separate dependency decision. |
| `library.local` | Local projects/files | P0 | Add without copying where safe; track missing/removable paths. |
| `library.metadata` | Title, author, tags, preview, rating/favorites | P1 | `partial` (M6a): bounded offline metadata cache (daemon `workshop-metadata-v1.json`) keeps title/kind/tags across unmounts and restarts; Steam canonical links via the open-in-Steam affordance; author/rating fields remain SDK work. |

## Wallpaper customization

| ID | Native concept | Target | Linux/KDE behavior |
|---|---|---|---|
| `property.color` | Color/scheme color | P0 | Native KDE color control with exact value round-trip. |
| `property.slider` | Bounded numeric slider | P0 | Slider plus typed value where precision matters. |
| `property.bool` | Checkbox | P0 | Native check box/switch according to semantics. |
| `property.combo` | Labeled value list | P0 | Preserve hidden values and translated labels. |
| `property.text` | Text input | P0 | Preserve Unicode and length; never interpret as shell input. |
| `property.file` | User-selected image/video | P1 | Portal/KDE picker, sandbox grant, fallback when missing. |
| `property.directory` | Watched media directory | P1 | Explicit grant and bounded file-change events. |
| `property.unknown` | Future/unknown property types | P0 | Preserve raw value and metadata; show read-only unsupported state. |
| `preset.local` | Save/apply named presets | P1 | Per-wallpaper named presets and default/reset behavior. |
| `property.live-update` | Apply properties while running | P0 | Versioned worker event without renderer restart where supported. |

## Runtime wallpaper APIs

| ID | Native concept | Target | Linux/KDE behavior |
|---|---|---|---|
| `runtime.pointer-position` | Cursor position/parallax | P0 | M1d-B transport and the M1e Plasma display bridge implement normalized, generation-bound, click-through positions; backend manifest/UI evidence still required before claiming full parity. |
| `runtime.pointer-buttons` | Cursor enter/leave/down/up/click | P1 | Interaction mode preserves Plasma context/edit gestures. |
| `runtime.audio-scene-16-32-64` | SceneScript stereo frequency buffers | P1 | PipeWire FFT resampled to documented 16/32/64 resolutions. |
| `runtime.audio-web-64` | Web stereo 64-band audio listener | P1 | Compatible 128-value callback cadence with bounded rate. |
| `runtime.pause` | Pause/unpause callback and frozen time | P0 | Worker pause plus web/scene notification semantics. |
| `runtime.fps` | Global FPS setting | P0 | General property/event and enforced worker frame budget. |
| `runtime.time` | Runtime, frame time, time of day | P1 | Monotonic timing and local-time mapping. |
| `runtime.screen` | Screen/canvas size and orientation | P0 | Logical/physical size, scale, rotation, and span mapping. |
| `runtime.media-metadata` | Track title/artist/album | P1 | MPRIS metadata; missing fields remain optional. |
| `runtime.media-artwork` | Album art and extracted colors | P1 | MPRIS artwork with cached, bounded decode and color extraction. |
| `runtime.media-playback` | Playing/paused/stopped and timeline | P1 | MPRIS state/position/duration where the player provides it. |
| `runtime.user-shortcut` | User-configured wallpaper shortcut | P2 | Explicit allowlisted actions; no arbitrary process execution. |
| `runtime.scenescript` | Wallpaper Engine SceneScript language/API | Backend-dependent | Publish implemented classes/events per backend; never claim blanket support. |
| `runtime.web-api` | Wallpaper Engine global JS listeners | P1 | Compatibility shim with API-version report and console diagnostics. |

## Displays, playlists, and playback policy

| ID | Native concept | Target | Linux/KDE behavior |
|---|---|---|---|
| `display.independent` | Different item/playlist per monitor | P0 | M1e provides the thin display package, but stable Plasma output identity, manager assignment, and hotplug recovery remain required. |
| `display.span` | One wallpaper across displays | P1 | Unified logical canvas with mixed-scale/rotation validation. |
| `display.clone` | Same wallpaper on selected displays | P1 | Shared or independent renderer selected by capability/performance. |
| `display.profile` | Named multi-monitor profile | P1 | Transactional snapshot and explicit remap after topology changes. |
| `playlist.ordered-shuffle` | Ordered/shuffled playlists | P0 | M5g–M5h provide bounded ordered/seeded-shuffle selection, no immediate repeat, deterministic unavailable/quarantine skipping, and bounded runtime history; M5k persists definitions and per-playlist snapshots in the daemon with restart/hotplug/suspend/missing-library recovery (`scripts/smoke-playlist-restart.sh`). Renderer assignment remains open. |
| `playlist.timer` | Duration and wallpaper transitions | P0 | M5g–M5h persist bounded settings and implement monotonic pause-aware decisions; M5k adds daemon restart recovery with re-anchored remaining durations (downtime not charged) and the fault-gated suspend simulation. Renderer transitions remain open. |
| `playlist.rules` | Time/day/application-driven selection | P1 | M5i–M5j resolve bounded time/day, application, window, session, battery, and power snapshots; desktop signal adapters, persistence, and saved-profile selection remain open. |
| `playback.keep-running` | Continue normally | P0 | M5i defines the bounded policy action; signal integration and renderer execution remain open. |
| `playback.mute` | Keep rendering but mute wallpaper | P0 | M5i defines the policy action; renderer audio mute and its separate audio-response permission remain open. |
| `playback.pause` | Freeze while retaining resources | P0 | M5i defines the policy action; worker simulation/media suspension remains open. |
| `playback.stop` | Stop and free resources | P0 | M5i defines the policy action; supervised teardown with last-known-good retention remains open. |
| `playback.conditions` | Fullscreen/maximized/focused/audio/application rules | P1 | M5i resolves bounded desktop/session/battery/power/application snapshots; KWin, MPRIS/PipeWire, logind, idle, battery, and power-profile adapters remain open. |

## System integration and deliberately different behavior

| ID | Native concept | Target | Linux/KDE behavior |
|---|---|---|---|
| `control.cli` | Pause/play/stop/mute/next/apply/properties | P1 | Stable `kwe` CLI and D-Bus API, including per-output targeting. |
| `control.shortcuts` | Global shortcuts | P1 | Register through KDE's shortcut facilities. |
| `system.screensaver` | Separate screensaver wallpapers/profiles | Deferred | Requires a safe KDE lock-screen/screensaver design. |
| `system.desktop-icons` | Show/hide desktop icons | Unsupported | Plasma owns desktop containment and icons. |
| `system.led` | Razer/Corsair lighting | P2/backend-dependent | Future explicit OpenRGB-style plugin, off by default. |
| `system.editor` | Create/edit/publish wallpapers | Deferred | Viewer/manager first; do not imply official editor compatibility. |
| `system.mobile` | Android companion transfer | Deferred | Not part of the KDE desktop 1.0 goal. |

## How parity is proven

Every capability moves from planned to implemented only when it has:

1. a renderer or service capability-manifest entry;
2. an original synthetic fixture that exercises success and failure behavior;
3. automated protocol/state tests and, for rendering, an image/event oracle;
4. a UI presentation for supported, partial, unavailable, and failed states;
5. backend/version/hardware evidence recorded in the compatibility database;
6. documentation of intentional semantic differences from Wallpaper Engine.

Unknown fields and callbacks must be logged at a bounded rate and preserved
where possible. They must not crash a renderer or be silently presented as
supported.
