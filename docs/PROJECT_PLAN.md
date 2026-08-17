# Project plan

## Product goal

Deliver a polished Plasma 6 experience for local and Steam Workshop wallpapers
with:

- a native Kirigami gallery with search, filters, previews, properties, and
  per-monitor assignment;
- a Breeze-native, keyboard-accessible interface that follows KDE's Human
  Interface Guidelines and remains usable from 100–250% display scale;
- Workshop discovery, subscribe/unsubscribe, installation progress, and
  offline metadata;
- video, scene, and sandboxed web wallpaper backends;
- playlists, schedules, shuffle/history, and multi-monitor groups;
- opt-in mouse interaction and PipeWire-based audio responsiveness;
- compatibility with Wallpaper Engine user properties, presets, runtime
  callbacks, media integration, display profiles, and playback policies;
- crash containment, automatic rollback, quarantine, and useful compatibility
  reports.

The first supported platform is Plasma Wayland on Arch/CachyOS. X11, lock
screen integration, and non-KDE desktops are explicitly later work.

`docs/UX_DESIGN.md` is the UI contract. `docs/FEATURE_COMPATIBILITY.md` is the
feature-parity contract. A feature is not complete merely because a renderer
can draw it; its configuration, compatibility status, and failure behavior
must also be understandable in the UI.

## Current development-machine baseline

Observed on 2026-08-16:

- CachyOS kernel 7.1.8, Plasma 6.7.4, Qt 6.11.1, Wayland;
- Intel UHD 630 plus NVIDIA RTX 3070;
- Steam library at `/media/crushinator`;
- Wallpaper Engine app ID `431960` installed;
- 92 local Workshop projects: 60 scene, 20 video, 9 web, and 3 with missing or
  unrecognized type metadata;
- PipeWire 1.6.8 and WirePlumber 0.5.15;
- `nvidia-smi` currently cannot communicate with the driver even though the
  loaded module and userspace package both report 610.57.04; an unsandboxed
  Vulkan probe does enumerate the RTX 3070 with Vulkan 1.4.341 and driver
  610.57.04, while the Intel GPU is not exposed by the current Vulkan loader.

The NVIDIA management-tool discrepancy and missing Intel Vulkan device are
preflight issues, not reasons to couple recovery to Plasma. Software Vulkan
remains the CI smoke path; NVIDIA is the current native hardware lane.

## Scope boundaries

### Version 1

- Plasma 6 Wayland, x86_64, Arch/CachyOS package and user-local development
  install.
- Installed Steam libraries plus an integrated Workshop browser.
- Video, scene, and web wallpaper support through isolated backends.
- Multi-monitor layouts, playlists, input, audio response, pause policies,
  diagnostics, and safe recovery.
- The native-compatibility targets marked P0 or P1 in
  `docs/FEATURE_COMPATIBILITY.md`.

### Deferred

- Wallpaper creation/editor features.
- Publishing Workshop items.
- X11 and other desktop environments.
- Lock-screen or SDDM integration.
- Redistributing proprietary Wallpaper Engine runtime assets.
- Perfect compatibility with every undocumented scene or web API.
- Application-type wallpapers that execute arbitrary downloaded programs.
- Hardware lighting integration, except through a future opt-in OpenRGB-style
  backend.
- Renderer and frame-transport optimization beyond release-blocking budgets.
  Profile-driven work is tracked in
  `docs/backlog/POST_RELEASE_RENDERER_OPTIMIZATION.md` and begins after the
  initial release; the mmap renderer path remains the correctness baseline.

## Delivery milestones

Each milestone must end in a runnable vertical slice and satisfy its safety
gate before work begins on the next risky renderer feature.

### M0 — Bootstrap and contracts

Alpha 0.1 now supplies the Cargo/CMake workspace, defensive local indexer,
bounded service protocol, original Vulkan preflight, and first runnable
Kirigami gallery. Remaining M0 work includes full D-Bus contracts, packaging,
visual baselines, and the complete multi-page prototype.

- Initialize Git, choose the final project name, and adopt Apache-2.0 for new
  original components unless a dependency review requires a split package.
- Add CMake/Cargo workspace scaffolding, formatting, linting, unit-test, and
  Arch `PKGBUILD` skeletons.
- Define versioned D-Bus control and display protocols.
- Create a clickable Kirigami UI prototype for the first-run, gallery,
  wallpaper-details, display, playlist, Workshop, and recovery flows.
- Establish automated accessibility and visual-regression baselines for Breeze
  Light, Breeze Dark, high contrast, fractional scaling, and right-to-left text.
- Turn the P0/P1 rows in `docs/FEATURE_COMPATIBILITY.md` into tracked issues
  with test fixtures and capability IDs.
- Add ADRs for process isolation, licensing boundaries, frame transport, and
  Steam integration.
- Add synthetic wallpaper fixtures; never commit Workshop content.

Exit gate: clean build/test on a fresh CachyOS or Arch container plus a native
Plasma development machine. The UI prototype must pass a human workflow review
before implementation locks in its navigation model.

### M1 — Safe display vertical slice

M1a implements and fault-tests the external generated producer, bounded
double-buffered mmap fallback, and standalone thin-consumer prototype. M1b now
moves lifecycle/watchdog ownership into `kwe-daemon`, adds bounded
terminate/kill/reap and restart, persists quarantine and a static last-known-good
fallback, and exposes the alpha display-control status. M1c now adds
transactional canary promotion and an acknowledged display generation. M1d
now has its resource-containment slice: per-renderer limits, aggregate systemd
budgets, diagnostics, and deterministic memory-pressure rollback. M1d-B adds
generation-bound normalized pointer position with passive hover-only display
semantics. M1e adds the reusable QML module, bounded display status/ack client,
and minimal Plasma wallpaper package, staged and exercised offscreen. M1f now
adds manager-owned validation, transactional user-local package staging, and
reversible safe mode. Applying the package plus the explicitly authorized live
Plasma PID-survival gate remain open.

- Build the user daemon/supervisor and thin Plasma wallpaper package.
- Render a generated test pattern in a separate worker.
- Transfer frames through a bounded buffer, starting with a portable shared
  memory fallback and then DMA-BUF where supported.
- Forward normalized pointer input without swallowing Plasma's right-click or
  long-press behavior.
- Persist and display a last-known-good still frame when the daemon disappears.

Exit gate: kill, hang, and OOM the renderer repeatedly; the `plasmashell` PID
must remain unchanged and the desktop must remain operable.

### M2 — Local library and video MVP

- M2a now validates primary and `libraryfolders.vdf` Steam roots, deduplicates
  canonical paths, and exposes per-library installation/Workshop availability.
- M2b adds persistent favorites, a favorites filter, deterministic sorting, and
  accessible card-level favorite controls to the manager gallery.
- M2c adds safe read-only project tags to the detail pane while keeping preview
  loading local, asynchronous, and isolated from renderer execution.
- M2d adds a supervised `mpv` preview child with safe local-path validation and
  hardware-decoding fallback for video entries.
- Discover every Steam library by parsing `libraryfolders.vdf`; canonicalize
  symlinks and removable paths.
- Parse `appmanifest_431960.acf`, `appworkshop_431960.acf`, and each local
  `project.json` defensively.
- Add a native Kirigami gallery, local thumbnails, search/filter/sort, and
  per-monitor apply.
- Add the responsive master/detail flow, compatibility badges, favorite state,
  keyboard selection, preview controls, and unsaved-property indication from
  `docs/UX_DESIGN.md`.
- Generate native editors for supported Wallpaper Engine user-property types
  and persist per-wallpaper presets without discarding unknown properties.
- Run video through a supervised `mpv`/libmpv worker with hardware-decoding
  fallback.
- Add the SQLite compatibility database and report export.

Exit gate: the current 92-item corpus indexes without a crash; malformed and
missing projects appear as actionable errors rather than disappearing. All
gallery and details actions are usable without a mouse and expose accessible
names.

### M3 — Scene backend

- M3a adds an explicit Vulkan renderer capability manifest; device probing no
  longer implies scene, shader, input, or audio support.
- M3b adds bounded static scene preflight and structured failure reasons before
  a renderer worker can be launched.
- M3c gates supervisor starts/retries on optional scene preflight and preserves
  the existing bounded persisted quarantine state for runtime failures.
- M3d surfaces bounded scanner diagnostics in the gallery and detail pane so
  unsafe content is visible before any Apply action.
- M3e adds read-only manager visibility for daemon renderer status and
  quarantine state, including bounded last-failure detail.
- Define a renderer capability manifest and adapter API.
- Build the original Rust/`ash` Vulkan scene worker behind the renderer
  capability manifest. Use Open Wallpaper Engine and `linux-wallpaperengine`
  only as documented behavioral/format references unless a later, separately
  reviewed licensing decision explicitly changes that boundary.
- Add static preflight checks, an offscreen canary render, heartbeat, resource
  limits, crash-signature grouping, and content-hash quarantine.
- Add backend/version/GPU-specific compatibility overrides.
- Report renderer support at the feature level, including SceneScript, shader,
  particle, model, input, audio, and property capabilities.

Exit gate: a bad scene automatically rolls back to the last-known-good image,
is quarantined after a bounded retry count, and produces a reproducible report.

### M4 — Web, input, and audio

- M4a adds an explicit, bounded permission declaration layer for network,
  pointer, and audio requests; unknown permissions are never granted.
- M4b adds bounded, non-executing web preflight with network disabled by
  default and explicit permission reporting.
- M4c defines bounded stereo 16/32/64-band normalized audio frames for future
  PipeWire workers and renderer adapters.
- M4d adds a bounded stereo analysis core that converts short PCM windows into
  normalized band frames without exposing raw audio to renderers.
- M4e adds a non-capturing PipeWire capability probe for diagnostics; audio
  capture remains opt-in and unavailable until the worker boundary is complete.
- M4f adds an allowlisted permission policy that intersects user grants with
  wallpaper requests before future workers can activate capabilities.
- M4g adds persistent per-wallpaper grant/revoke controls in the manager; no
  worker consumes a grant before its sandbox boundary is implemented.
- M4h adds a tested Bubblewrap/Chromium sandbox command builder with read-only
  content and network isolation by default.
- M4i wires that sandbox into an optional manager web-preview action; it remains
  isolated from Plasma and does not apply wallpapers.
- M4j extends the bounded pointer protocol with explicit primary/secondary/
  middle button events while preserving passive hover behavior.
- M4k defines bounded media-session metadata and timeline messages for a future
  MPRIS adapter without connecting to D-Bus yet.
- M4l adds a non-invasive MPRIS service probe for diagnostics without playback
  control or metadata subscription.
- Run each web wallpaper in a separate CEF/Chromium-derived sandbox process.
- Default to no network and no access to the home directory; expose explicit
  per-wallpaper permissions.
- Implement pointer transforms for output scale/rotation and an interaction
  mode that preserves normal desktop actions.
- Capture opt-in audio through PipeWire, perform bounded FFT processing in a
  worker, and send only normalized bins to renderers.
- Map Wallpaper Engine web and SceneScript audio/input/pause callbacks to Linux
  equivalents, including the documented 16/32/64-band scene buffers and stereo
  64-band web audio data.
- Map supported media-session callbacks to MPRIS metadata, artwork, playback
  state, and timeline data.

Exit gate: renderer compromise tests cannot read arbitrary home files or crash
Plasma; disabling audio tears down capture immediately.

### M5 — Playlists and policy engine

- M5a adds a bounded playlist data contract with deterministic ordered and
  seeded-shuffle selection.
- M5b adds atomic bounded JSON persistence for playlist definitions with
  fail-closed handling of malformed stores.
- M5c adds a bounded manager playlist controller and QML playlist selection
  surface backed by user-local settings.
- M5d adds create/select/add controls for playlists while keeping display
  assignment and renderer activation disabled.
- M5e persists playlist contents with deduplication and bounded removal APIs;
  renderer assignment remains disabled.
- M5f adds persistent shuffle/repeat settings to the playlist editor.
- M5g adds bounded duration/transition settings, backward-compatible playlist
  migration, and deterministic selection that skips unavailable or
  quarantined entries without immediate shuffle repeats.
- M5h adds a monotonic pause-aware playlist runtime with bounded history,
  explicit exhausted/degraded outcomes, and safe recovery from catalog reorder
  or temporarily unavailable content.
- M5i adds a bounded side-effect-free playback policy resolver with explicit
  keep-running, mute, pause, and stop/free-memory actions for desktop, session,
  battery, power, and focused-application signals.
- M5j adds deterministic time/day policy windows, including cross-midnight
  matching and fail-closed caller-provided local clock snapshots.
- Add ordered and shuffled playlists, duration, transition, history, and
  per-output/group assignment.
- Add time/day, battery, fullscreen, session-lock, and idle policies.
- Add Wallpaper Engine-equivalent keep-running, mute, pause, and stop/free-memory
  actions plus per-application wallpaper, playlist, and display-profile rules.
- Skip quarantined or unavailable items deterministically.
- Store desired state transactionally in SQLite WAL mode.

Exit gate: daemon restart, monitor hotplug, sleep/resume, and missing removable
Steam libraries recover without losing playlist state.

### M6 — Integrated Steam Workshop

- M1g now ships the reliable fallback that opens an item in the Steam client and
  watches Steam's local Workshop manifests for progress.
- M1h now joins those manifest states with the defensive local project scan,
  keeping subscribed-but-missing items visible and recoverable.
- M1i adds a bounded manager refresh loop and dismissible catalog-change
  notification so Steam download state changes appear without manual polling.
- M1j adds optional bounded byte/percentage progress metadata and a distinct
  downloading state for the manager cards.
- M1k adds bounded Workshop change history and adaptive retry backoff, while
  ignoring volatile scan timestamps so automatic refreshes stay quiet when
  nothing meaningful changed.
- Complete a legal/technical spike for an optional Steam bridge using
  `ISteamUGC` with an owned Wallpaper Engine installation.
- Add paginated query, details/thumbnails, subscribe/unsubscribe, item state,
  download progress, update notification, and offline cache.
- Use one consistent card/detail model across Installed and Workshop views so
  subscribing never sends the user through a second, visually unrelated UI.
- Keep the Steam bridge in its own permissively licensed process/package; do
  not expose Steam credentials or native SDK handles to the UI or daemon.

Exit gate: Steam closed/offline, library unmounted, subscription canceled, and
download interrupted all produce recoverable UI states.

### M7 — Hardening and release

- Add parser fuzzing, renderer fault injection, GPU/backend matrix tests,
  translation/accessibility review, power/performance budgets, and upgrade
  migration tests.
- Complete the P0/P1 compatibility matrix and publish honest renderer-specific
  coverage; never market a partially emulated API as fully compatible.
- Run usability tests for first-run setup, finding/applying a wallpaper,
  changing properties, building a playlist, resolving incompatibility, and
  entering safe mode.
- Provide `kwe diagnose`, `kwe test-wallpaper`, `kwe safe-mode`, and
  `kwe export-report` commands.
- Package signed Arch artifacts and an AUR recipe; document uninstall and a
  recovery path that restores KDE's image wallpaper without editing config by
  hand.

Exit gate: release checklist passes on Intel Mesa and supported NVIDIA driver
lanes with no `plasmashell` restart during the destructive test suite.

## Compatibility record

Key compatibility identity:

`Workshop ID + content hash + renderer/backend version + GPU/driver + Plasma/Qt`

Store:

- detected type, required features, and preflight warnings;
- canary and live run results, exit status/signal, heartbeat timeout, and a
  normalized log signature;
- last-known-good backend/settings and user overrides;
- quarantine state, retry count, workaround, and linked upstream/local issue;
- a privacy-reviewed export that excludes Steam credentials and copyrighted
  wallpaper payloads.

## Definition of done for every feature

- Acceptance criteria and failure behavior are written before implementation.
- Unit tests cover parsing/state logic; integration tests cover its process or
  D-Bus boundary.
- Logs have stable event names and contain the Wallpaper ID/content hash when
  relevant.
- No unbounded allocation, queue, retry loop, or renderer wait.
- A renderer/backend failure cannot require restarting Plasma.
- New UI follows `docs/UX_DESIGN.md`, supports keyboard navigation and screen
  readers, and has loading, empty, offline, degraded, and error states.
- New native behavior updates the capability registry and
  `docs/FEATURE_COMPATIBILITY.md` with automated evidence.
- New borrowed or adapted work satisfies `docs/PROVENANCE.md`.
- User documentation, recovery instructions, and Arch packaging impact are
  updated in the same change.
