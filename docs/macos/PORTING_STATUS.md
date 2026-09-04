# macOS porting status

Living record of what builds, what runs, and what is stubbed. Update with
every macOS-facing commit. Plan: `MacOS-Port-Plan.md`.

## Gate decisions (2026-09-04, proceeding on the plan's recommendations)

| Gate | Decision |
|---|---|
| G1 fork mechanics | Seams authored in this fork first as isolated, cfg-gated, Linux-neutral commits; offered upstream as cherry-picks (`kwe-platform` crate). |
| G2 content | Bring-your-own-folder (`STEAM_ROOT` / Steam library folders) is the supported contract; SteamCMD flow documented; no downloader shipped. |
| G3 display agent | Qt Quick + ObjC++ shim reusing `org.kde.kwe.display` verbatim. |
| G4 manager UI | QQC2 rewrite of the QML pages, Kirigami dropped in the fork. |
| G5 floor | macOS 14+, arm64 first. |
| G6 web sandbox | `sandbox-exec` profile + Chromium's own sandbox. |
| G7 audio | Core Audio process tap primary; BlackHole documented fallback. |
| G8 scene backend | MoltenVK; no Metal rewrite. |

## Crate matrix (cargo check, target aarch64-apple-darwin)

| Crate | Cross-check | Notes |
|---|---|---|
| kwe-platform | ok | new; Linux behavior byte-identical, Darwin substitutes documented per function |
| kwe-core | ok | Steam roots per platform |
| kwe-frame-protocol, kwe-input-protocol, kwe-report-protocol | ok | unchanged |
| kwe-cdp | ok | socketpair via kwe-platform |
| kwe-cli | ok | reports dir via kwe-platform |
| kwe-daemon | ok | pre_exec containment, rlimit type, peer creds, socket/state dirs via kwe-platform; macOS apply backend `macos_desktop.rs` (CoreGraphics displays + Plasma-script emulation, persisted); macOS worker env passthrough (DYLD_FALLBACK_LIBRARY_PATH, VK_ICD_FILENAMES, VK_DRIVER_FILES, TMPDIR) |
| kwe-test-renderer, kwe-video-renderer, kwe-web-renderer | ok | worker-side parent guard; web renderer on macOS runs the browser under `sandbox-exec` (generated SBPL: no writes outside its profile dir, no home reads except the content root, no network unless granted) + Chromium's own sandbox; browser from `KWE_CHROMIUM` or /Applications; `KWE_WEB_SANDBOX=off` for diagnosis |
| kwe-scene-renderer, kwe-shader-compiler, kwe-vulkan | ok (type-check only) | C build scripts need Xcode CLT on the Mac; VK_KHR_portability_enumeration + VK_KHR_portability_subset enabled when advertised (MoltenVK) |
| kwe-audio-worker | ok | macOS capture = `ffmpeg -f avfoundation` on a loopback device (`KWE_AUDIO_DEVICE`, default "BlackHole 2ch"; `brew install ffmpeg blackhole-2ch`, route output via a Multi-Output Device); Core Audio process tap still planned |
| kwe-mpv | ok | build.rs adds Homebrew link search |

## Manager (MP-7)

Built without KF6 Kirigami: `apps/kwe-manager/kirigami-shim` provides the
subset of `org.kde.kirigami` the manager's QML imports (ApplicationWindow,
Page, ScrollablePage, NavigationTabBar, Action, InlineMessage, MessageType,
Units, Theme, Heading, PlaceholderMessage, Icon, SearchField). The QML pages
stay byte-identical to upstream, so upstream UI changes merge cleanly.
`KWE_MANAGER_KIRIGAMI_SHIM` (default ON on macOS) selects it; the style
defaults to Fusion on macOS. macOS branches in C++: daemon activation via
`launchctl kickstart`/`bootstrap`, the "display bridge" is the presence of
`kwe-display-macos` (safe mode unavailable), last-good frame under
`~/Library/Application Support/kwe/state`. Verified on Linux offscreen
against a live daemon (97 items, screenshot reviewed); named theme icons
render blank without a Freedesktop icon theme (text labels carry meaning).

## Hardware-verify list (blocking a "done" on the Mac)

1. Desktop window sits under Finder icons and survives wake/Space change
   (agent re-asserts level every 5 s).
2. Mouse-moved global monitor works without an Accessibility prompt.
3. `sandbox-exec` profile lets Chromium bootstrap; if web wallpapers fail,
   bisect with `KWE_WEB_SANDBOX=off` (open question: whether Seatbelt's
   `network*` deny also blocks Chromium's own new local IPC sockets).
4. MoltenVK: `kwe-vulkan` lists the Apple GPU; scene corpus behaviour.
5. ffmpeg/BlackHole capture feeds audio-reactive scenes.
6. Homebrew Qt: `cmake -DCMAKE_PREFIX_PATH=$(brew --prefix qt@6)` configures
   the manager + agent; `smoke-display-agent.sh` passes offscreen.

## Review log

- 2026-09-04 MP-4 review (independent, sonnet): no show-stoppers. Fixed:
  `platform_mac.mm` now compiled with ARC (the App Nap token and the event
  monitor were unretained under MRC), dead local monitor removed, desktop
  level/back order re-asserted every 5 s (Finder redraws its desktop window
  at the same level after wake/relaunch/Space change), exact-geometry match
  refuses ambiguous (mirrored) outputs.
- 2026-09-04 CI-fix batch review (independent, sonnet): one show-stopper
  — the "named setrlimit" error wrapper dropped the raw errno (std hands
  a failing pre_exec closure to the parent as errno only), which would
  have turned every real Linux rlimit failure into "Invalid argument";
  reverted to the raw error, the per-step containment test in
  `kwe-platform` is the naming tool. Merge surface noted: supervisor.rs,
  shader_helper.rs (upstream is active there), test-only edits in
  main.rs/audio.rs/apply.rs.
- 2026-09-04 MP-5b/MP-6 review (independent, sonnet): two show-stoppers
  fixed — the CDP page match failed on percent-encoded spaces (default
  Steam path has "Application Support"; the renderer now also compares the
  percent-decoded URL), and the Seatbelt profile denied a browser under
  ~/Applications its own bundle (bundle re-allowed) and read access to the
  resolved temp tree (`/private/var/folders` now allowed for reads too,
  profile dir canonicalised). A Linux-literal PATH assertion that would have
  failed on the macOS CI runner now uses the platform constant. Open:
  `(deny network*)` vs Chromium local IPC (hardware-verify item 3).

- 2026-09-04 MP-3 review (independent, sonnet): no show-stoppers. Fixed:
  desktop-state persistence now uses the crate's `atomic_write` (unique
  temp, 0600, fsync; `wallpaper.restore` runs without the apply lock so
  two switches can race), state load opens with `O_NOFOLLOW` and bounds
  the open descriptor, and the smoke-fallback comment now says a stubbed
  macOS run must override `--kscreen-doctor-binary` too. Noted, not
  changed: CoreGraphics display order is re-read per call inside one
  transaction (verification catches a mid-transaction reorder as a
  rollback, not a silent wrong-display switch); no macOS stock image
  (restore is a plugin reset with no image).

- 2026-09-04 MP-2 review (independent, sonnet): no show-stoppers. Fixed:
  audio worker's `pw-record` child had silently gained `PR_SET_NO_NEW_PRIVS`
  (now `Containment::ParentOnly`); socket-path error string restored
  verbatim; `kwe-mpv/build.rs` gates on `CARGO_CFG_TARGET_OS` only. Nits
  addressed: allocation caveat documented, guard-thread spawn failure logged.

## Display agent (MP-4)

`apps/kwe-display-macos`: Qt Quick, one `QQuickView` per `QScreen` reusing
`org.kde.kwe.display` (DisplaySession/FrameSurface/InputClient) verbatim;
`platform_mac.mm` sets the AppKit window level (`kCGDesktopWindowLevel`),
all-Spaces/stationary collection behavior, click-through, accessory
activation policy, App Nap opt-out, and a global mouse-moved monitor for
passive pointer forwarding. Screen ↔ output identity is a geometry match
against the daemon's `wallpaper.outputs` (both sides derive geometry from
CoreGraphics display bounds). Builds on Linux as a windowed harness; the
offscreen smoke `scripts/macos/smoke-display-agent.sh` proves frame
display + display-generation ack against a real daemon.

Unverified on macOS (spike S-A): window ordering under Finder icons on
14/15, Sonoma "click wallpaper to reveal desktop", Stage Manager, sleep/wake,
whether the mouse-moved global monitor needs a TCC prompt.

## macOS CI: green (GitHub macos-14 runner, run 33835238015, 2026-09-04)

`rust-macos` (whole workspace builds; every portable crate's tests pass
with Homebrew shaderc/mpv/MoltenVK), `qt-macos` (agent + manager build
against Homebrew qt@6; daemon + kwe-test-renderer + kwe-display-macos
offscreen smoke passes: frame shown, display generation acknowledged) and
the Linux seam guard all pass. Scene-renderer tests (need a Vulkan device)
are built, not run, on the runner.

## macOS CI findings (how it got there)

The first real macOS execution. Fixed from its logs so far:
`CGDisplayCreateUUIDFromDisplayID` needs ColorSync linked; an ARC-disallowed
`WId` cast in the AppKit shim; scan tests comparing `/var` vs
`/private/var` temp paths; `/bin/false` and systemd-specific assertions in
daemon tests. Whole Rust workspace **builds** on macOS with Homebrew
shaderc/mpv/MoltenVK. Then pinned by a per-step diagnostic test: Darwin refuses
`setrlimit(RLIMIT_AS)` with `EINVAL`, which failed every worker spawn
(renderers quarantined, agent smoke saw no frame). The daemon and the
shader helper now skip `RLIMIT_AS` on macOS (never enforced there; the
resident-set watchdog in MP-9 is the substitute) and keep the other four
limits.

## Runtime status on a Mac

**macOS CI (GitHub macos-14, 2026-09-04): the offscreen display-agent smoke
passes** — kwe-daemon starts, spawns kwe-test-renderer, the agent opens and
validates the frame file, shows a frame, and acknowledges the display
generation, all on real macOS. Homebrew Qt configures and builds the agent
and the manager (Kirigami compatibility module). Nothing has been shown on
a real macOS desktop yet. First test target on the Mac:

```sh
packaging/macos/install-dev.sh            # daemon as LaunchAgent
cmake -S . -B build/agent -G Ninja -DCMAKE_BUILD_TYPE=Release -DCMAKE_PREFIX_PATH="$(brew --prefix qt@6)"
cmake --build build/agent --parallel
scripts/macos/smoke-display-agent.sh build/agent
# then, live: start a test renderer and cover all screens
target/release/kwe daemon-call --method renderer.start --params '{"wallpaper_id":"t","content_hash":"t","width":1920,"height":1080,"fps":30}'
build/agent/apps/kwe-display-macos/kwe-display-macos --cover-all
```

## Behavior differences vs Linux (by design)

- No `PR_SET_PDEATHSIG`: workers arm a kqueue guard on their parent pid.
- No `PR_SET_NO_NEW_PRIVS`.
- `RLIMIT_AS` is refused by XNU (`EINVAL`) and skipped; the supervisor instead kills a worker whose resident set exceeds `address_space_mib` (checked every tick, `ResourceLimit` failure, same strike/restart path). Address-space overcommit without touching pages is therefore not bounded on macOS.
- `pipe`/`socketpair` + `fcntl(FD_CLOEXEC)` instead of atomic `*_CLOEXEC`.
- Paths: socket `~/Library/Application Support/kwe/daemon-v1.sock`,
  state `~/Library/Application Support/kwe/state`, reports
  `~/Library/Application Support/kwe/reports`. XDG variables still win when
  set.
