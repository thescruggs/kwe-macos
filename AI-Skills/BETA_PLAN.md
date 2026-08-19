# KDE Wallpaper Engine — Beta 0.1: Video, Web, and Scene Renderers + Live Apply

> **LIVING PLAN** — this file is the canonical working plan for the beta effort.
> Mark milestone/sub-slice status inline as work progresses and record every
> deviation, discovery, or design change in the **Change log** below (dated).
> Update `PROJECT_MEMORY.md` at the end of every session. See `INSTRUCTIONS.md`
> for the maintainer's multi-agent operating directive.

## Change log

- 2026-08-18 — Plan authored (user decisions locked: full SceneScript engine; Chromium+CDP web; live apply authorized). Moved from `docs/BETA_PLAN.md` to `AI-Skills/BETA_PLAN.md` as the living plan. No slices started.
- 2026-08-18 — AI-Skills setup committed (`db8563d`). **BETA_M1a started** on branch `beta-m1a-renderer-contract` (worktree `/home/qcv123/gitProjects/kwe-m1a`), implemented by a sub-agent under orchestration; review agent pass follows. Scope note for M1a: `preflight_video` stays deferred to M1c — video content validation in M1a is path-level only (exists, regular, non-symlink), documented as temporary.
- 2026-08-18 — **BETA_M1a done** (commits `cd2d61e` feat + `62bdbdc` review fixes, merged ff into `fix/qt611-gallery-delegates`). Independent adversarial review found 12 findings (5 must-fix): exit-stderr was unreachable in the failure path → folded into failure detail; shared `HOME=/tmp` would break web→web handoff → per-worker `HOME=<runtime>/home-<launch_serial>` (0700); preflight ran twice per start (once blocking the supervisor loop) → single `into_validated()` in the RPC layer; two doc/implementation mismatches fixed. Design decision confirmed by review: `media_state`/`audio_bands` wire `sequence` carries the daemon display generation (staleness rejected daemon-side; retry after re-reading `renderer.status`). Extra decisions recorded: quarantine identity is now `wallpaper_id:content_hash:kind` with legacy `id:hash` fallback+migration; per-tick stderr drain budget raised to 64 KiB; `renderer.status` kind falls back active→requested→test. **Note for M3 renderer work:** renderers must NOT validate monotonicity on audio/media `sequence` (it is a generation, not an input sequence).
- 2026-08-18 — **BETA_M1b started** (branch `beta-m1a-renderer-contract` done; worktree for `beta-m1b-video-renderer`). **Plan deviation (provenance rule):** mpv/libmpv + `mpv`-crate THIRD_PARTY entries move from M1e **into M1b** — PROVENANCE.md requires ledger entries *before* dependent code merges, and M1b merges libmpv code. Environment verified: mpv 1:0.41.0, libmpv.so.2, ffmpeg present; smoke fixtures are runtime-generated per existing script pattern.
- 2026-08-18 — **M1b review findings recorded (M1e scope additions):** (a) RLIMIT_NPROC is uid-wide (counts all session threads), so the renderer NPROC limit guards the whole desktop, not the worker — add a **per-kind video NPROC knob (default 32768)** in M1e, keep 1024 for other kinds, and rely on RLIMIT_AS + supervisor timeouts for per-renderer protection (the M1b smoke's `--renderer-processes 4096` workaround is temporary). (b) In M1e, **drop the `mpv` crate** (used only for a one-line API-version diagnostic) in favor of one more `extern "C"` in the worker's FFI module — removes a 2016-era dep tree (rustc-serialize 0.3, rand 0.3, num 0.1).
- 2026-08-18 — **BETA_M1b done** (`7a2b402` feat + `3e83d2e` review fixes, rebased ff into main). Review found 3 must-fix (memory-pressure exit 72 parity, unbounded InputChannel::pending, SW_STRIDE c_int) + 4 recommended (smoke ack/stopped/SIGTERM assertions, deterministic quarantine loop, doc overclaims), all fixed. **M1a bug surfaced by M1b smoke:** the daemon never recorded `input_sequence` for media/audio forwards, so every media/audio ack was rejected as a protocol error — fixed with last-wins ack acceptance (media sequence is the display generation, so repeats are legal). Worker always requests bgr0; other format arms are defensive. Exit codes: 0 normal / 70 exit-after / 71-72 resource / 73 backend_reject.
- 2026-08-18 — **M1c + M1d started in PARALLEL** (disjoint files: M1c = kwe-core preflight.rs + supervisor.rs + kwe-cli; M1d = new kwe-audio-worker crate + daemon main.rs/audio.rs). **Plan deviation (provenance rule):** PipeWire tools (pw-record/pw-dump, separate-process-backend) THIRD_PARTY entries move from M1e **into M1d** — same before-merge rule as the mpv entries. M1d audio capture is gated by daemon `--audio-capture` flag + active renderer (per-wallpaper audio grants land in M2c). M1e then absorbs the recorded review items: per-kind video NPROC knob (32768) and dropping the `mpv` crate.
- 2026-08-18/19 — **M1c done** (`51cb469` + `219ebbd`): preflight_video (extension allowlist, ≤2 GiB, non-symlink; validate-then-canonicalize preserved), scan Video→RendererDependent, `kwe preflight --video`, smoke split. Review caught a plan-contract gap: the 24 h duration bound was claimed but never implemented → implemented for real in the worker; the fixer also found `MPV_FORMAT_DOUBLE` was wrong (int64 enum value) reading garbage — fixed against client.h. Exit 73 now covers decode failure AND >24 h duration; unreadable duration fails open.
- 2026-08-18/19 — **M1d done** (`b208465` + `2b2ebd8`): kwe-audio-worker (pw-dump resolution, pw-record --raw capture, analyze_stereo 64-band frames at ≤30/s, generation learned from renderer.status with backoff, bounded queue of 1), daemon --audio-capture/--audio-worker/--audio-capture-node + audio.rs manager (3 restarts per 10 min) + audio.status + SO_PEERCRED silent-drop for the managed worker. Empirical: monitor nodes are on-demand in pw-dump (target the Audio/Sink node; "Monitor of" fallback); default.audio.sink metadata is `{"name": X}`. Review found 2 HIGH: blocking pw-dump read defeated the 5 s deadline (fixed: nonblocking) and audio forwards RESET the pointer ack ceiling (fixed: max() ceiling, never decreases; new pointer-traffic smoke case proves acks past the generation with 0 protocol errors).
- 2026-08-19 — **M1e done** (branch `beta-m1e-m1-evidence`, commit `feat(m1e): video parity evidence, per-kind NPROC, and mpv crate removal`): (a) per-kind video NPROC knob `--renderer-video-processes` (default 32768, top of validated range) applied only to the Video kind's rlimits — test/web/scene keep the global 1024; smoke-video.sh's `--renderer-processes 4096` workaround removed (the unmodified lane is the proof); open risk 1 resolved. (b) `mpv` crate dropped — `mpv_client_api_version()` declared in the worker's existing `extern "C"` block with explicit `#[link(name = "mpv")]`; Cargo.lock regenerated (−201 lines: mpv 0.2.3 + 2016-era tree); THIRD_PARTY rust-mpv entry removed, libmpv notes updated; open risk 3 resolved. (c) Pixel oracle: deterministic `#3366CC` 64x64 mp4 through the full daemon pipeline, bounded seqlock frame-file parse, 9 sampled pixels — observed BGRA `(0xCC,0x66,0x33,0xFF)` ± 2 (tolerance 4); empirical finding: libmpv aspect-letterboxes mismatched content (1:1 clip in 16:9 target → centered 90x90, black corners) — documented as a semantic difference. (d) `kwe-video-renderer --probe` → `{"backend":"libmpv","client_api_version":"2.5","libmpv_supports_sw_render":true}` (exit 0, no device); `kwe diagnose` gained a video backend lane. (e) M1 exit gate: 92-item corpus indexes cleanly via `kwe diagnose` (scene 60, video 20, web 9, unknown 3, invalid 0; 0 global diagnostics); malformed/missing projects surface as actionable errors; keyboard-usable gallery/detail carried to M4 UI honestly. Gates: fmt/clippy clean, 146 tests, smoke-video 11 / smoke-supervisor 17 / smoke-audio 5 all pass.
- 2026-08-19 — **M1e review fixes** (`ba0a288`): oracle now samples the black letterbox bars too (exact black, deviation 0 — the letterbox semantic difference is machine-verified, not just observed); kwe-cli video probe reports Missing/Failed/Hung distinctly; `content.video` honestly marked **partial (M1e)** — the two unmet ladder steps are the capability-manifest entry (candidate: manifest-bearing `--probe`, follow-up) and UI presentation (scoped to M4). Recorded for M2: the web kind still has global NPROC 1024 — will need its own knob when the Chromium worker lands.
- 2026-08-19 — **BETA_M2a started** (branch `beta-m2a-cdp-spike`): CDP pipe-framing spike against the installed Chromium + `crates/kwe-cdp` client. M2 slices follow: M2b kwe-web-renderer, M2c daemon grants, M2d preview fix + compromise tests, M2e evidence.
- 2026-08-19 — **M2a done** (`e54049b` + `85696eb`). Empirical contract pinned in docs/BETA_M2.md: NUL-framed JSON on fds 3/4; screencast hard-stalls after exactly 3 unacked frames (kMaxScreencastFramesInFlight=2); first frame 20–53 ms after startScreencast; jpeg ~500 B at 160×90 q80; ~50 ms teardown on pipe close. Real bug found: `dup2(x,x)` doesn't clear FD_CLOEXEC (chromium's fcntl startup check fails silently). Review: 3 must-fix (wedged-write WouldBlock never fired — now pinned by test; parent fd leak masked EOF detection; order-dependent fd shuffle — fixed order-independently, and the F3 rewrite's own regression was caught by the smoke suite) + 6 recommended, all fixed. 38 kwe-cdp unit tests.
- 2026-08-19 — **BETA_M2b started** (branch `beta-m2b-web-renderer`). **Plan deviation (provenance rule):** Chromium + bubblewrap THIRD_PARTY entries (planned for M2d) move **into M2b**, plus a new `image`-crate entry (JPEG decode) — the worker code that spawns them merges in M2b, and PROVENANCE.md requires entries before dependent code merges.
- 2026-08-19 — **M2b discovery (design change):** Chromium 151's V8 sandbox reserves a ~100 GiB VIRTUAL address floor — the M1a web default of 16384 MiB RLIMIT_AS kills Chromium outright (implementer measured: no mapping fails below ~96G, the DevTools handler just never answers). Decision: web kind address-space default becomes **131072 MiB (128 GiB)**; VA is free, real protection stays with systemd MemoryMax (RSS ~250 MB) + supervisor timeouts. Docs updated in the slice.
- 2026-08-19 — **M2b done** (`095ac91` + `3f20e8b`, merged as `9cd123f`). kwe-web-renderer: bwrap-sandboxed Chromium (ro-binds /usr /etc /lib /lib64 /bin /sbin + content /wallpaper + tmpfs profile), CDP screencast → JPEG decode → BGRA publish with keepalive, pointer via Input.dispatchMouseEvent, audio via guarded `audio_web` evaluate, web NPROC 32768. Review (3 must-fix): sandbox-integrity case was vacuous twice over (wrong probe box + file:// fetch fails regardless of netns) → rewritten network-dependent with a positive control through an --allow-test-faults hook; docs caps mismatch fixed; CLI builder fixed. **Major empirical discovery:** headless=new IGNORES --window-size (surface 500×3!) and Page.startScreencast aspect-fits the surface into maxWidth/maxHeight — a 160×1 JPEG possible; fixtures must paint in viewport fractions and the worker's stretch policy handles degenerate sources. **Heartbeat added (promoted from review):** page-independent Runtime.evaluate("1+1") probe → consecutive failures → exit 73; keepalive can no longer mask a dead stream forever. Wedge-case expectation corrected: post-promotion failures retry forever by design (promotion clears the record) — heartbeat guarantees restart, not quarantine. 201 tests, smoke-web 11/11.
- 2026-08-19 — **Plan deviation (user-requested):** packaging pulled forward from M5 — the PKGBUILD now installs kwe-video-renderer, kwe-web-renderer, kwe-audio-worker so test builds can drive video/web wallpapers from the installed package. M5 keeps .SRCINFO regen + version bump + installed-layout smoke.

## Status at a glance

| Milestone | Status |
|---|---|
| BETA_M1 (contract + video) | done (M1a–M1e; see change log) |
| BETA_M2 (web) | M2a+M2b done, M2c in_progress, M2d–M2e pending |
| BETA_M3 (scene, a–k) | pending |
| BETA_M4 (live apply) | pending |
| BETA_M5 (release) | pending |

## Context

The alpha (0.1.0-alpha.1) is installed and running, but video/web/scene wallpapers show as "planned" — no real renderers exist, and Apply is hardcoded disabled. The user wants all three renderers completed and the project upgraded to a beta that actually applies wallpapers to the live Plasma desktop.

**User decisions:** (1) Scene = **full SceneScript engine**, original implementation per ADR 0001 (GPL projects are behavior references only); (2) Web = **headless Chromium via DevTools Protocol** in the existing bwrap sandbox; (3) **Live Plasma apply is explicitly authorized** on this CachyOS machine, including live-session tests.

**Iron rules (AGENTS.md + docs):** no parsing/rendering in plasmashell; every milestone is a runnable vertical slice with acceptance + failure tests and a `docs/BETA_*.md` evidence file; THIRD_PARTY.yml entry **before** dependent code merges; synthetic fixtures only; bounded everything (queues, allocs, retries, logs); renderer failure never restarts Plasma; FEATURE_COMPATIBILITY parity ladder (6-step evidence) updated in the same change; implementation and adversarial review as separate passes; worktree + branch per milestone.

## Key design decisions (verified against code)

- **D1 — JS engine: QuickJS(-ng) via `rquickjs` (MIT).** ES2023 level, has `JS_SetMemoryLimit`, interrupt handler (step budget), small footprint. Boa too slow for 60fps `update()`; Duktape too old; V8 too heavy. Execution contract: 64 MiB heap cap, 8 ms soft / 33 ms hard budget per `update()`; script exceptions log at bounded rate and keep last state — never kill the renderer.
- **D2 — Shaders:** internal passes GLSL→SPIR-V at build time (glslang, precompiled bytes in crate); wallpaper-provided shaders compiled at runtime via `naga` GLSL subset, fail-closed to default material, manifest `scene3d.shaders=subset`.
- **D3 — Video: libmpv linked into `kwe-video-renderer`** (LGPL-2.1+ dynamic, separate-process worker), `MPV_RENDER_API_TYPE_SW` render API (no EGL), `--hwdec=auto-safe` with one documented software retry. Keepalive re-publish loop so paused video never trips frame-timeout.
- **D4 — Web: `--remote-debugging-pipe` over a socketpair** (sandbox has `--unshare-net`; a debug *port* would be unreachable). `Page.startScreencast` JPEG → decode (image crate) → BGRA publish; **must ack `Page.screencastFrameAck`** or capture stalls. Pointer via `Input.dispatchMouseEvent` (real CSS :hover/:active); audio via `Runtime.evaluate` of the WE `audio_web` callback (128 floats, ≤30/s). Chromium flags add `--no-sandbox` (bwrap is the sandbox), `--disable-dev-shm-usage`, `--disable-gpu`, `--headless=new`. One-day CDP framing spike first; port+`DevToolsActivePort` bind is the fallback.
- **D5 — Audio worker: separate `kwe-audio-worker`** spawned by the daemon; captures via `pw-record --raw` (resolved once through bounded `pw-dump`), reuses `kwe_core::audio::analyze_stereo` (audio.rs:10-27), pushes 64-band frames over a socket; daemon forwards to active renderer latest-wins (pointer pattern, supervisor.rs:817-823). Opt-in: `--audio-capture` + per-wallpaper audio grant; teardown immediate on stop.
- **D6 — Renderer contract generalization:** `StartSpec` (supervisor.rs:114-123) gains `kind: RendererKind` + `content: ContentSpec` (Video{path}/Web{root}/Scene{path}); per-kind renderer paths, timeouts, resource limits, and env allowlist in `SupervisorConfig`; argv adds `--content <path>`; stderr piped into a bounded ring surfaced in `renderer.status`.

## Found gaps (resolved in the milestones)

| # | Gap | Fix |
|---|---|---|
| G1 | `stderr(Stdio::null())` (supervisor.rs:692) | M1: pipe + bounded ring → `WorkerStatus.stderr_tail` |
| G2 | `env_clear()` breaks bwrap/Chromium/mpv helpers | M1: per-kind env allowlist (HOME=/tmp, PATH, XDG_RUNTIME_DIR) |
| G3 | RLIMIT_AS 4096 MiB < V8's 4 GiB virtual cage | M1: per-kind limits (web address_space 16384 MiB, open_files 1024) |
| G4 | systemd MemoryMax=1G too small for daemon+audio+renderers | M1: MemoryMax=3G, CPUQuota=400%, TasksMax=96 |
| G5 | 3s startup / 2s frame timeouts vs Chromium cold start | M1: per-kind startup timeouts (web 10s) + keepalive re-publish |
| G7 | playlist_session never starts renderers | M4 |
| G8 | Permission grants exist only in manager-local state | M2: daemon-owned `permissions-v1.json` + `permissions.get/set` |
| G9 | Shipped mpv/bwrap/chromium previews have no THIRD_PARTY entries; preview chromium flags broken under bwrap | M1/M2: ledger entries; fix webpreview.cpp args |
| G10 | scan.rs "planned" rows (scan.rs:393-411) | flip to `RendererDependent` with honest details in M1/M2/M3 |
| G12 | preflight_scene accepts any JSON object; pkg unvalidated | M3: pkg magic/entry validation in preflight |

## Milestones (each ends runnable with `docs/BETA_*.md` evidence + green `./scripts/check.sh`)

### BETA_M1 — Contract generalization + VIDEO renderer (~2.6k LOC)
- **M1a Contract:** supervisor.rs (StartSpec/ContentSpec, per-kind config map, spawn argv/env/stderr ring, ControlCommand AudioFrame/MediaState variants), main.rs (kind/content params, `audio.forward`/`media.state` RPC), SUPERVISOR_API/INPUT_PROTOCOL doc updates. Tests: existing smoke-supervisor.sh unchanged; kind dispatch, stderr ring bounds, coalescing.
- **M1b `crates/kwe-video-renderer`:** libmpv SW render API → BGRA → SharedFrameWriter paced loop; InputChannel copy from test-renderer (main.rs:79-139, 168-228) extended for media_state (pause/seek); keepalive re-publish; fault flags preserved. `scripts/smoke-video.sh`: canary promotes, kill→rollback, corrupt→quarantine, paused→keepalive.
- **M1c preflight_video** (kwe-core preflight.rs pattern) + scan.rs Video→RendererDependent + synthetic video fixtures (never Workshop payloads).
- **M1d `crates/kwe-audio-worker`** + daemon audio forwarding module. Accept: ≤30/s 64-band frames to active renderer; no grant → no capture; teardown ≤1s.
- **M1e Close-out (done 2026-08-19):** per-kind video NPROC knob (default 32768, video kind only) resolving the uid-wide RLIMIT_NPROC issue; `mpv` crate removal (explicit FFI + `#[link(name = "mpv")]`); deterministic pixel oracle in smoke-video.sh (observed BGRA within 2 of expected, tolerance 4); `kwe-video-renderer --probe` + `kwe diagnose` video lane; FEATURE_COMPATIBILITY `content.video` evidence and M1 exit-gate summary in docs/BETA_M1.md. (THIRD_PARTY mpv/libmpv entries moved into M1b; PipeWire entries into M1d, per the provenance rule. The planned systemd bump stays with the release milestone.)

### BETA_M2 — WEB renderer (~2.9k LOC)
- **M2a** CDP spike on installed Chromium 151 (pipe framing) + `crates/kwe-cdp` client (bounded 4 MiB messages, timeouts, fake-peer unit tests).
- **M2b `crates/kwe-web-renderer`:** extend websandbox.rs builder (headless/debug flags, window-size, pipe fds); screencast→BGRA keepalive loop; pointer via CDP; audio injection. Failure tests incl. omitted screencastFrameAck → stall → rollback. Compromise tests: fetch('/etc/passwd') and external fetch both fail (ro-bind + unshare-net).
- **M2c Grant enforcement:** daemon `grants.rs` + `permissions-v1.json` + `permissions.get/set`; network grant toggles `--unshare-net`; manager delegates (WallpaperDetail.qml:60-66).
- **M2d** `kwe preflight --web`, fix webpreview.cpp chromium flags, THIRD_PARTY entries (Chromium BSD-3, bubblewrap LGPL) **before merge**, smoke-web.sh + compromise script.
- **M2e** FEATURE_COMPATIBILITY `content.web` + `runtime.audio-web-64` evidence; scan.rs Web→RendererDependent; docs/BETA_M2.md.

### BETA_M3 — SCENE renderer: SceneScript engine + Vulkan compositor (largest; slices M3a–M3k, ~11.8k LOC)
Format research contract first (docs/BETA_M3.md + docs/SCENE_FORMAT_V1.md): scene.json schema + SceneScript API matrix derived from official docs + behavior references only (no copied code). pkg = `PKGV####` magic, length-prefixed entry table, LZ4 data (`lz4_flex`, bounded entry count/sizes).

- **M3a** `crates/kwe-scene-renderer`: device setup reusing kwe-vulkan probe patterns, QuickJS runtime, full InputChannel, keepalive loop, fault flags; scene.json parse (16 MiB bound); init/update/resized callbacks; script timeout counters. Failure: throw→bounded log; OOM→exit-71 path→ResourceLimit.
- **M3b** original pkg reader (kwe-core/src/pkg.rs) + preflight pkg validation; corpus smoke (all 60 local scenes preflight without crash).
- **M3c** 2D image layers: renderpass/quad pipeline, texture upload, transforms, opacity/draw order; image-oracle tests; manifest `scene2d=partial`.
- **M3d** blend modes + color effects (WE set) via pipeline blending; oracle per mode.
- **M3e** TextLayer via stb_truetype, bounded glyph atlas.
- **M3f** ParticleSystem CPU-sim, bounded count (~4096).
- **M3g** VideoLayer textures via libmpv (≤2 concurrent).
- **M3h** scene3d P1: .obj/.mtl parser, camera/lights, naga shader subset with fail-closed default.
- **M3i** AudioAnalyser (16/32/64 bands from daemon 64-band frames) + mouse/buttons/keyboard; `runtime.audio-scene-16-32-64` evidence.
- **M3j** Properties: IProperty objects, `property_set` wire message (additive, protocol doc update), per-wallpaper persistence; manager property UI deferred to M4 (flagged in docs).
- **M3k** Exit gate: bad scene → rollback → quarantine → reproducible report (`renderer_report` side file ≤8 KiB read by daemon); destructive suite (infinite loop, OOM, shader bomb, corrupt/oversized pkg); `runtime.scenescript` per-class coverage matrix; `plasmashell` PID unchanged.

### BETA_M4 — LIVE APPLY (~2.4k LOC)
- **M4a** daemon `apply.rs`: `assignments-v1.json`, `wallpaper.apply/restore/assignments`; apply transaction = preflight → canary → promote → ack; Plasma config write via bounded `org.kde.PlasmaShell.evaluateScript` (daemon-constructed strings only, never wallpaper content); safe-mode restore to org.kde.image. Research slice pins the exact desktop-id↔output script first.
- **M4b** manager Apply UI: enable WallpaperDetail.qml:98-107, per-kind preflight summary, permission grants (daemon-backed), output picker, safe-mode wiring, accessible loading/success/degraded/failed states.
- **M4c** playlist renderer assignment/transitions (playlist_session calls supervisor.start, skips quarantined).
- **M4d** live enablement + `scripts/smoke-live-apply.sh` (authorized): apply video/web/bad-scene on the live session, rollback + safe-mode assertions, `plasmashell` PID unchanged across every destructive step; evidence in docs/BETA_M4.md.

### BETA_M5 — RELEASE (~1.2k LOC)
- Version `0.1.0-beta.1`: Cargo.toml:14 workspace, CMakeLists.txt:3, PKGBUILD pkgver+pkgrel, `.SRCINFO` regen via `KWE_FORCE_AUR_SOURCE=1 makepkg --printsrcinfo`, README Beta 0.1 section, docs/BETA_0_1.md guide, manager About page + app strings.
- M7 CLI: `kwe test-wallpaper`, `kwe safe-mode`, `kwe export-report` (privacy-reviewed), `kwe diagnose` renderer lanes.
- Completed THIRD_PARTY ledger (quickjs/rquickjs, lz4_flex, image, stb_truetype, naga, glslang, chromium, bubblewrap, mpv family, PipeWire) + license review sign-off.
- FEATURE_COMPATIBILITY full refresh (honest rows; scene2d/3d partial-subset; per-class scenescript matrix); PROJECT_PLAN.md statuses amended (M2-M7, ordering note).
- Packaging: install all four renderer binaries + audio worker; installed-layout smoke; Intel Mesa + NVIDIA lane evidence (machine recorded: NVIDIA via loader).

## Rollout order

M1 → M2 → M3a-b → M3c-e → M3f-g → M3h-j → M3k → M4 → M5. Total ≈ 17–19 kLOC across nine merge trains. Each milestone: new git branch/worktree, implementation + separate adversarial review pass (AGENTS.md), docs/BETA_*.md evidence, THIRD_PARTY ledger before merge.

## Verification

Per milestone: `cargo test --workspace`, `./scripts/check.sh` (fmt/clippy/build/qmllint), new smoke script per milestone (smoke-video.sh, smoke-web.sh + compromise, scene destructive suite, smoke-live-apply.sh), image-oracle tests for rendering, `plasmashell` PID unchanged during all destructive tests, package rebuild + install smoke at M5. Live-session tests only in M4 (explicitly authorized).

## Notes

- `runtime.pointer-buttons` stays P1: wire plumbing in M1a, consumer wiring in M3i.
- Post-release optimization backlog (mmap/DMA-BUF) stays deferred; pread/shmem baseline is the correctness path.
- If `--remote-debugging-pipe` framing misbehaves on the installed Chromium, documented fallback: port 0 + `--bind` of a runtime dir into the sandbox + read DevToolsActivePort.
