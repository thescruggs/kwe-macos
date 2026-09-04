# MacOS-Port-Plan

**Status:** Maintainer said "proceed with the plan" 2026-09-03 (evening,
CDT); gates §6 taken on the plan's recommendations and recorded in
`PORTING_STATUS.md`. Fork created 2026-09-04 at
`github.com/thescruggs/kwe-macos` (private), upstream = the Linux checkout.
MP-0 done; MP-2 seam landed (`kwe-platform`), whole workspace cross-checks
for `aarch64-apple-darwin`. Each milestone gets an independent review; show
stoppers are fixed, everything else documented here and continued.

**Baseline:** upstream trunk `fix/qt611-gallery-delegates` at `1cbc08f`
(pkgrel 22), 2026-09-03.

**Scope:** a separate macOS fork that runs Wallpaper Engine Workshop
wallpapers (video, web, scene) as the desktop picture on macOS, reusing the
upstream daemon/worker/protocol design. Upstream (this repository) stays
Linux/Plasma-only.

**Required invariant (macOS form):** no wallpaper parser, renderer, shader
compiler, browser, Steam SDK, video/audio decoder, or audio processor may
execute in the display agent process. The display agent is the macOS
analogue of the thin Plasma bridge; renderers stay separate, killable,
supervised processes.

## 1. Executive decision

- Fork, do not branch: a new repository with this repo as `upstream` remote.
- Port, do not rewrite: keep Rust crates, protocols, transaction model,
  supervisor, and recovery semantics. Replace only the platform layer.
- Keep Vulkan via MoltenVK. A native Metal renderer is a non-goal.
- New code on macOS is confined to: a display agent, a `platform::macos`
  module in the daemon/core, an audio capture worker, a launchd/bundle
  packaging tree, and a manager UI skin.

## 2. Fork strategy

- Repository: new repo (name TBD, e.g. `kwe-macos`), created by cloning this
  checkout; remote `upstream` = this repo (GitHub once pushed).
- Branches: fork `main` = shipping; `sync/upstream-<date>` for merges.
- Sync policy: `git merge upstream/<trunk>` at each upstream pkgrel; never
  cherry-pick renderer work by hand.
- Conflict minimization (see gate G1):
  - fork-only directories never conflict: `apps/kwe-display-macos/`,
    `crates/kwe-audio-worker-macos/`, `packaging/macos/`, `docs/macos/`;
  - platform code goes behind `#[cfg(target_os)]` in `platform/{linux,macos}.rs`
    modules rather than inline edits;
  - upstream Linux behavior stays byte-identical; the fork never edits
    Linux branches of a cfg.
- Upstream seam commits (recommended, G1): a few behavior-neutral commits
  land upstream first (extract `platform` modules, `ShellBackend` trait in
  apply lane, `AudioCapture` trait). Each approved individually per AGENTS.md.
  Without them every daemon change upstream conflicts in the fork.
- Provenance: `THIRD_PARTY.yml` carried forward; MoltenVK, SwiftShader,
  BlackHole (if used) added with license entries before dependent code merges.

## 3. Portability inventory

| Component | Linux dependency | macOS replacement | Risk |
|---|---|---|---|
| `kwe-core` scan | `~/.local/share/Steam`, `~/.steam` | `~/Library/Application Support/Steam`; `libraryfolders.vdf` parse unchanged | low |
| `kwe-core` paths | XDG dirs | `~/Library/Application Support/kwe`, `~/Library/Caches/kwe`, `~/Library/Logs/kwe`, `$TMPDIR` | low |
| `kwe-core` websandbox | `bwrap` | `sandbox-exec` profile (deprecated API, still functional) + Chromium built-in sandbox | med |
| `kwe-core` mpris/pipewire probes | `qdbus6`, `pw-dump` | cfg-out; MPRIS → `MPNowPlayingInfoCenter` later (non-goal for v1) | low |
| `kwe-daemon` supervisor | `PR_SET_PDEATHSIG`, `NO_NEW_PRIVS`, `RLIMIT_AS`, `pipe2`, `SOCK_CLOEXEC` | kqueue `EVFILT_PROC NOTE_EXIT` or inherited-pipe EOF for parent death; `pipe`+`FD_CLOEXEC`; RSS watchdog via `proc_pid_rusage` (RLIMIT_AS unenforced on Darwin); other rlimits kept | med |
| `kwe-daemon` socket auth | `SO_PEERCRED` | `getpeereid`/`LOCAL_PEERCRED` | low |
| `kwe-daemon` apply lane | `kscreen-doctor -o`, `qdbus` `evaluateScript` | `ShellBackend::MacDisplayAgent`: outputs from agent (`NSScreen` list + UUIDs), switch = agent command; reset = `NSWorkspace.setDesktopImageURL` | med |
| `kwe-daemon` systemd unit | user unit, `TasksMax`, memory limits | launchd `LaunchAgent` (`KeepAlive`, `ProcessType Background`, `SoftResourceLimits`); aggregate memory via daemon-side watchdog | med |
| `kwe-frame-protocol` | mmap file in `XDG_RUNTIME_DIR` | unchanged; path under `$TMPDIR` | low |
| `kwe-input-protocol` | Unix socket | unchanged | low |
| `kwe-video-renderer`, `kwe-mpv` | system `libmpv.so` | Homebrew `mpv` (`libmpv.dylib`), software render API unchanged | low |
| `kwe-web-renderer`, `kwe-cdp` | `chromium` + bwrap, fds 3/4 | Chrome/Chromium.app headless, `--remote-debugging-pipe`; fd inheritance via `posix_spawn` file actions | med |
| `kwe-scene-renderer`, `kwe-vulkan` | Vulkan 1.2/1.3, `external_memory_dma_buf`, `external_semaphore_fd` | MoltenVK: `VK_KHR_portability_enumeration` + `VK_KHR_portability_subset`; cfg-out dma-buf; verify 1.3 feature use; IOSurface zero-copy later via `VK_EXT_metal_objects` | high |
| `kwe-shader-compiler` | system shaderc | Homebrew `shaderc` | low |
| Deterministic render tests | llvmpipe/lavapipe | SwiftShader (CPU Vulkan) or skip-on-mac with Linux goldens as reference | med |
| `kwe-audio-worker` | PipeWire `pw-record` | new worker: Core Audio process tap (macOS 14.2+), fallback BlackHole loopback; same FFT-bin output | med |
| `org.kde.kwe.display` QML module | Plasma wallpaper package | reused verbatim inside the display agent (G3) | low |
| Plasma package | `plasmashell` | `kwe-display-macos`: desktop-level `NSWindow` per screen | high |
| `kwe-manager` | Kirigami, `systemctl --user` | QQC2 macOS style (G4); `launchctl kickstart`; issue reports under `~/Library/Logs/kwe/reports` | med |
| Packaging | PKGBUILD | `.app` bundles, LaunchAgent plist, codesign + notarize, Homebrew tap | med |
| Content acquisition | Steam client installs WE + Workshop | WE is Windows-only; Steam.app will not fetch its Workshop items; SteamCMD `workshop_download_item 431960` + `app_update 431960` (windows depot) for `assets/` | **blocking** |

## 4. Display agent design (fork-only)

- One process, one desktop-level window per `NSScreen`:
  - level `kCGDesktopWindowLevel` (below Finder icons at
    `kCGDesktopIconWindowLevel`);
  - `collectionBehavior`: `canJoinAllSpaces | stationary | ignoresCycle`;
  - `ignoresMouseEvents = true`; `hasShadow = false`; not activating.
- Frame path: reuse `displaysession.cpp`/`frameitem.cpp` logic (bounded
  positioned reads, generation ack, freeze last-good) unchanged (G3).
- Input: `NSEvent.addGlobalMonitorForEvents(.mouseMoved)` → normalized
  positions on the existing input protocol; no Accessibility prompt expected
  for mouse-moved (verify in spike S-A).
- Output identity: `CGDisplay` UUID as the connector name the daemon persists.
- Events the agent must survive: screen hotplug/reconfigure, Spaces switch,
  sleep/wake, login, Stage Manager, Sonoma "click wallpaper to show desktop".
- Reset/safe mode: `kwe safe-mode` restores the saved `NSWorkspace` desktop
  image URL per screen and quits the agent.

## 5. Milestones

Each slice is issue-sized per AGENTS.md; acceptance is stated, failure is the
inverse. Spikes (§7) precede MP-2.

- **MP-0 Fork bootstrap** — done 2026-09-04 (repo, CI, toolchain doc, status doc, cross-check script)
  - create repo, `upstream` remote, CI on `macos-14`/`macos-15` runners;
  - toolchain doc: Homebrew `rust cmake ninja pkg-config qt@6 mpv shaderc
    molten-vk vulkan-headers vulkan-loader`, Chromium cask, Xcode CLT;
  - `cargo check --workspace` matrix on macOS; record per-crate failures in
    `docs/macos/PORTING_STATUS.md`.
  - Accept: CI green on the portable crate subset; status doc complete.
- **MP-1 Content acquisition gate** — in progress (gate G2 decided: bring-your-own-folder; SteamCMD doc pending)
  - macOS Steam roots; `STEAM_ROOT` override honored;
  - documented SteamCMD flow for Workshop items and the `assets/` depot;
  - `kwe scan` lists items on a Mac.
  - Accept: at least one video, web, and scene item indexed on macOS.
- **MP-2 Portable core** — done 2026-09-04 (kwe-platform seam; Linux tests green; Darwin cross-check clean)
  - `platform` modules for paths, peer creds, cloexec pipes/sockets;
  - `kwe-core`, protocols, `kwe-cli`, `kwe-report-protocol`,
    `kwe-scene-inspector`, `kwe-shader-compiler`, `kwe-cdp`, `kwe-mpv`,
    `kwe-test-renderer` build and pass unit tests on macOS.
  - Accept: `cargo test` passes for those crates on arm64.
- **MP-3 Daemon** — done 2026-09-04 (macos_desktop backend, launchd LaunchAgent + dev install scripts, env passthrough); RSS watchdog deferred to MP-9
  - supervisor platform layer (parent death, rlimit subset, RSS watchdog);
  - `ShellBackend` trait: `Plasma` (upstream) vs `MacDisplayAgent`;
  - LaunchAgent plist; socket under `~/Library/Application Support/kwe`;
  - `smoke-supervisor.sh` fault matrix passes with `kwe-test-renderer`.
  - Accept: hang/exit/corrupt-header faults recover; no orphans after
    `kill -9` of the daemon.
- **MP-4 Display agent** — code complete 2026-09-04 (Qt + ObjC++ shim; Linux offscreen smoke); macOS behavior unverified until spike S-A runs on hardware
  - Qt Quick app + ObjC++ shim, or Swift app (per G3);
  - desktop-level windows per screen, frame reuse, input monitor;
  - `smoke-plasma-display.sh` equivalent: test pattern on desktop, freeze on
    hang, survive hotplug and sleep/wake.
  - Accept: pattern visible behind icons on two screens; last-good frame
    retained across a renderer kill.
- **MP-5 Renderers** — pending
  - 5a video: libmpv software render, `smoke-video.sh` passes;
  - 5b web: Chrome headless + CDP pipe, `sandbox-exec` profile,
    `smoke-web.sh` + `smoke-web-compromise.sh` pass or document deviations;
  - 5c scene: MoltenVK enumeration/portability subset, dma-buf cfg-out,
    `kwe-vulkan` preflight reports Metal device; scene corpus sweep run,
    diffs vs Linux goldens triaged (visual parity, not byte identity).
  - Accept: one wallpaper of each family applied live on a Mac.
- **MP-6 Audio** — pending (gate G7)
  - Core Audio process-tap worker; permission UX (microphone/audio TCC);
    BlackHole fallback; `smoke-audio.sh` equivalent.
  - Accept: audio-reactive scene responds to system audio.
- **MP-7 Manager** — pending (gate G4)
  - QML skin per G4; launchd activation; reports path; previews;
  - `.app` bundle with embedded QML modules.
  - Accept: gallery, apply, reset, playlist, issue report work end to end.
- **MP-8 Packaging** — pending
  - `.app` bundles for manager + agent, LaunchAgent, codesign + notarize,
    Homebrew tap formula/cask, uninstall script restoring desktop images;
  - arm64 first; universal binary after MP-9.
  - Accept: clean install on a fresh macOS user account; uninstall leaves no
    LaunchAgent or desktop-level window.
- **MP-9 Hardening** — pending
  - battery/low-power pause (extends F3 pause-when-covered), App Nap opt-out
    for renderers, display reconfigure storms, TCC denial states, login-item
    ordering, memory watchdog calibration.

## 6. Decision gates (maintainer)

- **G1 Fork mechanics.** Pure fork vs upstream seam commits.
  Recommendation: allow cfg-gated, behavior-neutral seam commits upstream,
  each individually approved. Cuts merge conflict surface sharply.
- **G2 Content acquisition.** SteamCMD-documented flow vs Steamworks bridge
  vs bring-your-own-folder only. Recommendation: bring-your-own-folder as
  the supported contract, SteamCMD flow documented, no downloader shipped in
  v1. Legal/ToS review of the SteamCMD depot fetch before publishing docs.
- **G3 Display agent stack.** Qt Quick + ObjC++ shim (reuses the validated
  `org.kde.kwe.display` module verbatim) vs Swift app (reimplements the
  frame reader). Recommendation: Qt + shim; Swift only if the desktop-level
  window misbehaves under Qt's NSWindow management in spike S-A.
- **G4 Manager UI.** Keep Kirigami (MacPorts `kf6-kirigami` or KDE Craft;
  Homebrew has no KF6) vs rewrite the five QML files on QQC2 macOS style.
  Recommendation: QQC2 rewrite; C++ clients unchanged.
- **G5 Floor.** Recommendation: macOS 14+, Apple Silicon first.
- **G6 Web sandbox.** `sandbox-exec` profile vs Chromium sandbox only.
  Recommendation: both; profile denies home except content root and the
  profile dir, denies network unless granted.
- **G7 Audio.** Core Audio taps (14.2+) vs ScreenCaptureKit vs BlackHole.
  Recommendation: taps primary, BlackHole documented fallback.
- **G8 Scene backend.** MoltenVK vs Metal rewrite. Recommendation: MoltenVK;
  Metal rewrite is a non-goal for the fork.

## 7. Verification spikes (before MP-2, each bounded to one day)

- **S-A** desktop-level NSWindow behind icons on macOS 14 and 15 from a Qt
  Quick window; icons clickable; survives Spaces, sleep, "show desktop".
- **S-B** `kwe-vulkan` preflight and `kwe-scene-renderer` offscreen on
  MoltenVK; list unsupported features/extensions actually requested.
- **S-C** SteamCMD on macOS downloads one Workshop item and the WE `assets/`
  depot with an owning account.
- **S-D** Chrome headless with `--remote-debugging-pipe` on fds 3/4 spawned
  from Rust via `posix_spawn` file actions; `kwe-cdp` handshake succeeds.
- **S-E** Core Audio process tap captures default-output audio in Rust
  (`coreaudio-sys`) with the TCC prompt observed once.
- **S-F** `sandbox-exec` profile launches Chrome headless on macOS 15.

## 8. Known risks

- Content acquisition is the only blocker without an engineering answer;
  if G2 lands on "no supported path", the fork has no users.
- MoltenVK gaps (no geometry shaders, format/blend subset, Vulkan 1.3
  partial) may refuse scenes that Linux accepts; the scene apply gate already
  reports limitations, so this degrades honestly.
- `RLIMIT_AS` is unenforced on Darwin; containment relies on the RSS
  watchdog and per-process kill, weaker than the Linux slice.
- No llvmpipe determinism on macOS; byte-identity sweeps run only on Linux CI.
- Apple desktop behaviors change per release; the agent needs a per-release
  smoke on each new macOS beta.
- TCC prompts (audio, maybe Accessibility) are user-visible friction and
  must have explicit denied states in the manager.

## 9. Non-goals

- Metal renderer, Windows/iOS, Mac App Store (App Sandbox forbids the
  process model), Steam credential handling, redistribution of Wallpaper
  Engine assets.
