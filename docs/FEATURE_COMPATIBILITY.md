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
| `content.video` | Video wallpapers | P0 | `partial` (M1e): the video pipeline itself is implemented — supervised `kwe-video-renderer` publishes BGRA8888 premultiplied frames via libmpv's software render API (`--hwdec=auto-safe`, one bounded retry with `--hwdec=no`, then exit 73) through the shared frame protocol, with static preflight (allowlisted container extension, ≤ 2 GiB, non-symlink) and quarantine on repeated failure; the worker independently bounds decode (known duration over 24 h fails closed with exit 73, unreadable duration fails open). Backend evidence: mpv 1:0.41.0 / libmpv client API 2.5 (`kwe-video-renderer --probe` → `{"backend":"libmpv","client_api_version":"2.5","libmpv_supports_sw_render":true}`; `kwe diagnose` prints the same lane); machine: NVIDIA GeForce RTX 3070, Vulkan 1.4.341, logical device created, DMA-BUF extensions present, llvmpipe fallback (`kwe-vulkan --json`). Fixture evidence: `scripts/smoke-video.sh` (11 cases) — deterministic pixel oracle (solid `#3366CC` 64x64 mp4 through the full daemon pipeline, frame file parsed per FRAME_PROTOCOL_V1.md with a seqlock snapshot, 11 sampled pixels — 9 content within worst deviation 2 of the expected BGRA `(0xCC,0x66,0x33,0xFF)` plus 2 letterbox-bar samples requiring black — tolerance 4), duration bound (> 24 h rejected, exit 73), garbage-content rollback (`garbage.mp4` → `rolled_back` with `exit_code_73`). Two parity-ladder steps remain unmet and keep this row `partial`: step 1 (a capability-manifest entry — `kwe-video-renderer` emits none; a manifest-bearing `--probe` is a candidate follow-up) and step 4 (UI presentation for supported/partial/unavailable/failed states, scoped to the M4 UI milestone). Semantic differences: `audio_bands` is received and acked but not consumed by video wallpapers; a paused/settled video keepalive-re-publishes the last frame (never an empty frame); bgr0 is converted to BGRA8888 premultiplied (alpha 0xFF); `loop-file=inf` loops content; known duration over 24 h fails closed, unreadable duration fails open; aspect-mismatched video is aspect-fit letterboxed into the target canvas (e.g. a 1:1 clip in a 16:9 target renders a centered square with black corners). |
| `content.scene2d` | Packed 2D scenes | P0, backend-dependent | `partial` (S1): supervised Vulkan scene worker implements clearcolor, image/text/particle layers, researched blend modes, synthetic VideoLayer textures via libmpv (≤2 concurrent, software-only, local-file/package sources, bad/capped layer skip), and — new in S1 — model layers: a model instance (`image` → a `.json` model file, the WE solid-model architecture) draws its material's first texture as a textured quad through the TEXV0005 container decoder (`crates/kwe-scene-renderer/src/texv.rs`, adapted from `Almamu/linux-wallpaperengine`, GPL-3.0-or-later — see THIRD_PARTY.yml) and the model→material→texture resolver (`crates/kwe-core/src/scenemodel.rs`, same provenance), looked up against scene.pkg entries, the scene directory, and the configured Wallpaper Engine assets root in that order. Mesh/puppet geometry, custom material shaders, effect passes, and combos are parsed and recorded but not yet acted on (S1 draws a flat quad, not the material's real shader). Evidence: `scripts/smoke-scene.sh` M3a–M3g daemon + llvmpipe lanes, the S1 case (a synthetic pkg model layer with a generated TEXV0005 solid-colour texture applies through the daemon and the frame oracle samples that colour), and scene/texv/scenemodel unit tests; NVIDIA RTX 3070 and llvmpipe readback oracles are recorded in `docs/BETA_M3.md`. A scene that declares objects and can draw NONE of them in this build is REFUSED, not applied: preflight (file and package lanes) answers `invalid_params` naming each missing feature, the worker re-checks the same rule before its first publish (exit 74), and the manager reports it as a feature gap with the current wallpaper left in place (B2, `scripts/smoke-scene.sh` B2 a/b/d). Honest scope on the 60-package local corpus, preflighted with `--assets-dir <the local Wallpaper Engine assets install>`: **59 of 60 scenes now apply** (up from 14 of 60 pre-S1 — the 46 refused scenes were, without exception, scenes whose only visuals were model layers). The one remaining refusal (workshop id 1652229298) is honest, not a bug: its model layer references the runtime render-target name `_rt_FullFrameBuffer` (a full-screen post-process/copybackground effect layer, not a static `.tex` asset) plus a particle system whose definition lives in an external particle file — both genuinely out of S1's scope (effects/render-targets, external particle files). Model-layer resolution outcomes are counted and reported (`event=renderer.scene.model_texture_skip count=N` for layers whose texture failed to resolve or decode). Known gap: a layer whose supported content fails to decode at runtime still degrades to nothing rather than refusing (the skip-never-reject contract), so a scene can still apply blank if all of its decodable references are broken. The capability manifest and manager UI presentation remain deferred, so this is not full `content.scene2d` or SceneScript parity. |
| `content.scene3d` | 3D models, cameras, lights, shaders | P1, backend-dependent | `partial` (S1, honestly scoped): a model instance's base texture draws as a flat textured quad (see `content.scene2d` — the two rows share one implementation, since Wallpaper Engine stores every visual, 2D included, as a model). There is **no mesh, no puppet geometry, no 3D camera, no custom shader, no effect pass, and no combo evaluation** — the resolver parses and records `shader`/`combos`/`blending`/`cullmode`/`depthtest`/the remaining texture slots (`ResolvedModel`, `crates/kwe-core/src/scenemodel.rs`) purely for a future slice; none of it affects what draws today. A scene that is nothing but unresolvable model/effect layers is refused at preflight instead of applying as a blank desktop (B2). Report mesh/shader/effect support individually as later slices land. |
| `content.web` | HTML/JS/WebGL wallpapers | P1 | `partial` (M2e): the web pipeline itself is implemented — a supervised `kwe-web-renderer` runs one headless Chromium inside a bwrap sandbox (ro-binds for `/usr` `/etc` `/lib` `/lib64` `/bin` `/sbin`, content at `/wallpaper`, throwaway tmpfs profile, `--unshare-net` unless the daemon's per-wallpaper network grant allows, M2c), captures the page over the CDP pipe (`Page.screencastFrame` jpeg q80 with the per-frame ack contract, pinned in BETA_M2.md), decodes under hard caps (8192 px per dimension / 64 MiB alloc / 16 777 216 pixels — failures are counted and skipped, never published), and publishes BGRA8888 frames through the shared frame protocol; pointer lines dispatch as CDP `Input.dispatchMouseEvent` in layout CSS pixels; `audio.forward` frames evaluate `window.audio_web([...])` at most 30/s and only under the audio grant; a page-independent heartbeat (`Runtime.evaluate("1+1")`) exits 73 after consecutive failures, and static pages keepalive-re-publish the last frame (never an empty one). The catalog row is `RendererDependent` with the honest detail "sandboxed Chromium worker; network and audio off until granted" (scan.rs, pinned by the `marks_web_projects_renderer_dependent` scan test). Capability-manifest evidence: `kwe-web-renderer --probe` boots the real sandboxed browser and verifies three boot-class round trips (Browser.getVersion; a one-frame `Page.startScreencast` capture received and acked; a `Runtime.evaluate("1+1")` heartbeat answering 2) → `{"backend":"chromium","browser_version":"Chrome/151.0.7922.137","heartbeat":true,"heartbeat_value":"2","protocol_version":"1.3","sandbox":"bwrap","screencast":"jpeg-q80","screencast_frames":1}` in ≈0.6 s (15 s budget; missing bwrap → `detail=spawning bwrap`, exit 73); `kwe diagnose` prints the same lane (Report/Missing/Failed/Hung). Backend evidence: Chromium 151.0.7922.137, bubblewrap 0.11.2 (both pinned in BETA_M2.md §8.2); the V8 sandbox's ~98 GiB VA floor drives the web kind's 128 GiB RLIMIT_AS default (NPROC 32768); decode latencies 60 µs (160x90) / 1435 µs (960x540). Fixture evidence: `scripts/smoke-web.sh` (11 cases: canary promote, grant-painted network control and revocation, static keepalive, pointer oracle, audio-grant gating with acks advancing, kill -9 recovery, missing-root rejection, busy-loop exit 73 rollback as a refusal (B4: reported, not restarted, not counted), quarantine after three candidate kills, wedged-page heartbeat exit-73 cycles, plasmashell pid unchanged) + `scripts/smoke-web-compromise.sh` (4 attempts x 2 scenarios, frame-oracle and `/proc` argv proofs, plasmashell pid unchanged). Five of the six parity-ladder steps are met: 1 (capability-manifest entry — the `--probe`), 2 (synthetic fixtures for success and failure), 3 (automated protocol/state tests plus the frame oracle), 5 (backend/version/hardware evidence), 6 (semantic differences below); step 4 (UI presentation for supported/partial/unavailable/failed states) is scoped to the M4 UI milestone, exactly as recorded for `content.video`. Semantic differences: headless=new ignores `--window-size` (the 500x3 layout surface is screencast aspect-fit to a 160x1 JPEG whose single area-averaged row is duplicated across all frame rows — the y-axis carries no information, so fixtures paint markers in viewport X fractions spanning the full canvas height); a page painting identical pixels every frame stops the compositor and rAF — animations must change pixels per frame; a still page re-publishes its last frame at the pacing deadline and the heartbeat bounds the stopped-painting blind spot (a wedged main thread exits 73); `audio_web` is invoked at most 30/s (rate-limit diagnostics) and only with the audio grant (frames for a non-granted wallpaper are dropped silently latest-wins and counted `audio_grant_dropped`); media-state messages are acknowledged but not consumed (no media-session UI binding in M2); local file access (BETA B6, 2026-08-22): chromium runs with `--allow-file-access-from-files`, so a wallpaper's own images are same-origin for WebGL textures and same-directory XHR/fetch resolve — without it real Workshop wallpapers (e.g. 2646399969) render black; the cost, stated plainly, is that wallpaper JS can read any `file:` path that exists inside the namespace — the content root and the read-only system binds (`/etc/passwd`, fontconfig, the throwaway profile) — nothing under the user's home is bound and leaving the sandbox still needs the network grant; the sandbox boundary is now asserted page-observably by `smoke-web-compromise.sh` attempt 2 (a host canary outside the binds is unreachable, its traversal has no target) with attempt 3 as the reads-work control; narrowing the `/etc` bind is the recorded follow-up (`docs/bugs/WEB_FILE_ACCESS_BLACK_CANVAS.md`); `--enable-unsafe-swiftshader` keeps software WebGL when chromium removes the deprecated silent fallback; cookie persistence: the profile is a fresh tmpfs per launch — localStorage written in one supervised run is absent from the next (verified with a two-run fixture), and document.cookie does not round-trip on `file://` in Chromium 151 at all (set + read-back in the same load returns empty), so the cookie boundary is stronger than the profile alone. |
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
| `runtime.audio-web-64` | Web stereo 64-band audio listener | P1 | `partial` (M2e): the M1d `kwe-audio-worker` captures stereo audio through PipeWire, the daemon forwards 128-value stereo 64-band frames to the web worker, and the worker evaluates them as `window.audio_web([...])` at most 30/s (rate-limit diagnostics) — and only while the wallpaper's audio grant is set (M2c): frames for a non-granted wallpaper are dropped silently latest-wins and counted (`audio_grant_dropped`), never evaluated, so the grant gate is part of the callback contract, not a delivery hint. Evidence: smoke-web case 4 (audio grant gates delivery — dropped without the grant, acks advance with it, zero protocol errors) and smoke-audio cases 1–3 (capture spawns under `--audio-capture`, `audio_bands` acked at the promoted display generation, `renderer.stop` produces only the rate-limited drop note with no error storm). Step 4 (UI presentation) remains M4 as for the other rows. |
| `runtime.pause` | Pause/unpause callback and frozen time | P0 | Worker pause plus web/scene notification semantics; M3g scene media transport consumes latest-wins play/pause/stop for open VideoLayers (per-layer script controls remain deferred). |
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
| `display.scaling` | Per-wallpaper scaling mode (aspect / fill / stretch) | P0 | `supported` (F1, 2026-08-22): `wallpaper.apply {"scaling"}` persisted per output, passed to the renderer (`--scaling`: libmpv letterbox / `panscan=1.0` / `keepaspect=no`; the scene compositor maps the declared scene rectangle onto the canvas by the mode; web pages lay out at the canvas size) and to the Plasma plugin through `renderer.status` (`FrameSurface.scaling`: fit / cover+clip / stretch, pointer normalised against the same rectangle). The canvas itself now follows the output geometry (aspect kept, long edge ≤ 2560, `apply::frame_size_for`) instead of a fixed 960x540, so on a matching-aspect output the plugin step is the identity and the renderer decides what an aspect-mismatched wallpaper does. Manager: a three-way selector beside the output picker. Evidence: `kwe-frame-mapping-test` (destination rectangle per mode, pointer normalisation under fill), daemon RPC test (derived canvas 2926x823 → 2560x720, `scaling` in argv/status/assignment, unknown mode rejected), scene `world_extent` unit test, manager `scalingModeTravelsOnlyWhenNotDefault`. Not yet: a live pixel oracle per mode through the full pipeline (planned with the F1 smoke lane), and the letterbox colour stays black (theme-following is a sub-decision in the backlog doc). |
| `display.independent` | Different item/playlist per monitor | P0 | M1e provides the thin display package, but stable Plasma output identity, manager assignment, and hotplug recovery remain required. |
| `display.span` | One wallpaper across displays | P1 | Unified logical canvas with mixed-scale/rotation validation. |
| `display.clone` | Same wallpaper on selected displays | P1 | Shared or independent renderer selected by capability/performance. |
| `display.profile` | Named multi-monitor profile | P1 | Transactional snapshot and explicit remap after topology changes. |
| `playlist.ordered-shuffle` | Ordered/shuffled playlists | P0 | M5g–M5h provide bounded ordered/seeded-shuffle selection, no immediate repeat, deterministic unavailable/quarantine skipping, and bounded runtime history; M5k persists definitions and per-playlist snapshots in the daemon with restart/hotplug/suspend/missing-library recovery (`scripts/smoke-playlist-restart.sh`). M4c implements renderer assignment: on an entry change (timer advance, policy switch, manual play, resume-after-restart) the session drives the shared apply transaction — a hard cut — for the resolved output (`--playlist-output`, else the last playlist-assigned output, else the first enabled and connected), skipping quarantined/unavailable entries and backing off exponentially on failure (rollback parity with `wallpaper.apply`: renderer stopped if ours, store reverted — the previous renderer is not kept live). |
| `playlist.timer` | Duration and wallpaper transitions | P0 | M5g–M5h persist bounded settings and implement monotonic pause-aware decisions; M5k adds daemon restart recovery with re-anchored remaining durations (downtime not charged) and the fault-gated suspend simulation. Renderer transitions are implemented as hard cuts through the apply transaction (M4c); crossfade transitions remain open. |
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
