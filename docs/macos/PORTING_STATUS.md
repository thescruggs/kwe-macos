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
| kwe-daemon | ok | pre_exec containment, rlimit type, peer creds, socket/state dirs via kwe-platform; **apply lane still Plasma-only** (MP-3) |
| kwe-test-renderer, kwe-video-renderer, kwe-web-renderer | ok | worker-side parent guard added; web renderer still spawns `bwrap` (MP-5b) |
| kwe-scene-renderer, kwe-shader-compiler, kwe-vulkan | ok (type-check only) | C build scripts need Xcode CLT on the Mac; no portability-enumeration yet (MP-5c) |
| kwe-audio-worker | ok | still PipeWire-only at runtime (MP-6) |
| kwe-mpv | ok | build.rs adds Homebrew link search |

## Runtime status on a Mac

Nothing verified on real hardware yet. First test target: `kwe-daemon` +
`kwe-test-renderer` + the display agent (MP-4) showing the test pattern.

## Behavior differences vs Linux (by design)

- No `PR_SET_PDEATHSIG`: workers arm a kqueue guard on their parent pid.
- No `PR_SET_NO_NEW_PRIVS`.
- `RLIMIT_AS` is set but unenforced by XNU; RSS watchdog pending (MP-9).
- `pipe`/`socketpair` + `fcntl(FD_CLOEXEC)` instead of atomic `*_CLOEXEC`.
- Paths: socket `~/Library/Application Support/kwe/daemon-v1.sock`,
  state `~/Library/Application Support/kwe/state`, reports
  `~/Library/Application Support/kwe/reports`. XDG variables still win when
  set.
