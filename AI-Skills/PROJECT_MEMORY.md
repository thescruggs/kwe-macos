# AI-Skills — Project Memory

**Current as of:** 2026-08-20 (session 7). Read `INSTRUCTIONS.md` first; `BETA_PLAN.md` is the living plan.

## Current repo state

- **Repo:** `/home/qcv123/gitProjects/KDE-Wallpaper-Engine` — KDE Plasma 6 Wallpaper Engine-compatible experience for Arch/CachyOS. **Beta work happens in per-slice worktrees** (`/home/qcv123/gitProjects/kwe-*`, deleted after merge); the trunk branch is `fix/qt611-gallery-delegates` (no upstream).
- **History (all ff-merged into the trunk):** `1833331` Qt 6.11 fix → `db8563d` AI-Skills → BETA_M1 (M1a–M1e) → BETA_M2 (M2a–M2e, `dc125d8`) → M3a–M3f (`d4ea8c2`+`5abc800`; M3g–M3k paused by maintainer reorder) → M4a `cffb7cd`+`f3376c7` → M4b `eb83966`+`e6152bd` → M4c `e5d69e0`+`520b2d7`. HEAD = `520b2d7` + docs commit; working tree clean; the `kwe-m4c` worktree is removed after merge. Full per-slice detail lives in BETA_PLAN.md's change log.
- **Installed:** `kde-wallpaper-engine-0.1.0.alpha.1-1` built from local HEAD and installed on this CachyOS machine (2026-08-18). Alpha gallery works; Apply disabled; video/web/scene shown as "planned".
- **Graphify:** `graphify-out/` knowledge graph rebuilt 2026-08-18 (1,497 nodes / 3,224 edges / 70 communities). Query with `graphify query "<question>"`. Known integrity notes: 180 dangling-endpoint edges, `metadata.json` produces zero AST nodes.

## Mission & iron rules (full text in AGENTS.md)

- No wallpaper parser/renderer/browser/Steam SDK/audio processor may execute in `plasmashell`.
- One issue-sized vertical change per task with acceptance + failure criteria (template: `AI-Skills/TASK_TEMPLATE.md`).
- Separate branches/worktrees per agent/task; implementation and adversarial review are separate passes.
- THIRD_PARTY.yml provenance entry **before** dependent code merges. GPL references = behavior ideas only (ADR 0001), never copied code.
- Synthetic fixtures only; never real Workshop payloads as committed fixtures.
- Bound every queue/allocation/retry/wait/log. Renderer failure must never restart Plasma.
- Compatibility claims need the 6-step parity ladder (docs/FEATURE_COMPATIBILITY.md:109-120); never blanket-claim.

## Commands that work (verified 2026-08-18)

```bash
./scripts/check.sh                       # fmt + clippy -D warnings + cargo test/build + cmake build + qmllint + diagnose + kwe-vulkan
cargo test --workspace                   # Rust unit tests
cd build/cmake && ctest                  # C++ tests (4 targets)
qmllint -I /usr/lib/qt6/qml -I build/cmake/apps/kwe-manager apps/kwe-manager/qml/*.qml
KWE_RUN_WORKSHOP_CACHE_SMOKE=1 ./scripts/check.sh   # smoke suites are opt-in via KWE_RUN_*_SMOKE=1
scripts/smoke-supervisor.sh              # fault-injection (headless-safe)
target/debug/kwe diagnose                # catalog diagnostics
cd packaging && makepkg -f              # release package from local HEAD (commit first!)
KWE_FORCE_AUR_SOURCE=1 makepkg --printsrcinfo   # regenerate .SRCINFO for AUR
```
Install: `sudo pacman -U packaging/kde-wallpaper-engine-0.1.0.alpha.1-1-x86_64.pkg.tar.zst` (sudo needs a password — user runs it).
Qt is 6.11.1; `kwe-package-installer-test` needs `Qt6::Qml` linked (fixed in 1833331).

## User decisions (2026-08-18, locked)

1. **Scene renderer = full SceneScript engine**, original implementation (QuickJS via rquickjs — MIT, has memory-limit + interrupt controls). Reported per-class/backend-dependent, never blanket.
2. **Web backend = headless Chromium via DevTools Protocol** in the existing bwrap sandbox (frames via `Page.startScreencast` over `--remote-debugging-pipe`).
3. **Live Plasma apply authorized** on this machine, including live-session tests. AGENTS.md's no-live-session rule is explicitly waived by the maintainer for this project's apply work (BETA_M4); destructive tests must still never restart plasmashell.

## Plan status (see BETA_PLAN.md for detail)

| Milestone | Status | Next slice |
|---|---|---|
| BETA_M1 — contract generalization + video renderer | done (M1a–M1e, 2026-08-19) | — |
| BETA_M2 — web renderer (Chromium+CDP) | done (M2a–M2e, 2026-08-19) | — |
| BETA_M3 — scene renderer (QuickJS + ash, slices a–k) | M3a–M3f done; M3g–M3k paused until after M4 (maintainer reorder) | resume after M4 |
| BETA_M4 — live apply + manager UI (pulled ahead of M3g–M3k) | M4a–M4c done | M4d (live enablement + smoke-live-apply) |
| BETA_M5 — beta release 0.1.0-beta.1 | pending | last |

Known code gaps the plan resolves (details in BETA_PLAN.md §Found gaps): renderer stderr discarded (G1), env_clear breaks helpers (G2), RLIMIT_AS too small for Chromium (G3), systemd MemoryMax=1G too small (G4), 3s/2s timeouts too tight for real backends (G5), playlists never start renderers (G7), grants not daemon-enforced (G8), missing THIRD_PARTY entries for shipped mpv/bwrap/chromium previews (G9).

## Architecture cheat sheet (for sub-agents)

- **Daemon** (`crates/kwe-daemon`): JSON-RPC over Unix socket (`$XDG_RUNTIME_DIR/kwe/daemon-v1.sock`), one request per connection. Supervisor thread spawns ONE renderer binary per slot: argv `--output <frame.bin> --width --height --fps` (+fault flags), `env_clear()`, stdin=input pipe, stdout=ack pipe, stderr=/dev/null; pre-exec setpgid/PDEATHSIG/no_new_privs/rlimits. Canary = sequence≥3 & ≥1s → AwaitingAck (5s) → promote. Failures → rollback to last-good PPM → quarantine after 3 (key: `wallpaper_id:content_hash`).
- **Frame protocol** (`crates/kwe-frame-protocol`): BGRA8888 premultiplied, 64-byte header `KWEFRM1`, 2-slot seqlock, file = 64+2·w·h·4 ≤512MiB. `SharedFrameWriter::create/publish/set_state`, `SharedFrameReader::snapshot`. C++ consumer `FrameItem` uses pread (no mmap), Frozen after 1.5s.
- **Input protocol** (`crates/kwe-input-protocol`): NDJSON ≤4096B on stdin; `pointer_position` (phases, u16 coords, buttons), renderer acks `input_ack` on stdout; `audio_bands` (16|32|64 f32 bands) and `media_state` wire types exist; since M1d the daemon forwards `audio.forward`/`media.state` RPCs (generation-gated) and `kwe-audio-worker` is the real producer behind `audio_bands` (latest-wins, queue of 1).
- **kwe-core**: `scan.rs` (ProjectKind + per-kind Compatibility — the "planned" strings live at scan.rs:393-411), `preflight_scene` (size/ext checks only), `webpreflight.rs`, `websandbox.rs` (bwrap command builder), `audio.rs` (`analyze_stereo`), `playlist.rs`, `policy.rs` (PermissionPolicy, unconsumed by daemon).
- **kwe-test-renderer**: the contract reference — paced publish loop + InputChannel + fault flags (exit 70/71/72 semantics). New renderers copy it.
- **Manager** (`apps/kwe-manager`): Qt6/QML; Apply hardcoded disabled (WallpaperDetail.qml:98-107); no C++ apply path. VideoPreview spawns mpv, WebPreview spawns bwrap+chromium (both unsandboxed-by-supervisor, both missing THIRD_PARTY entries).
- **Display bridge** (`modules/org/kde/kwe/display` + `plasma/wallpapers/org.kde.kwe.wallpaper`): staged, NOT live-enabled. DisplaySession polls `renderer.status`, sends `renderer.ack`.

## Session log

| Date | Session | What happened | State after |
|---|---|---|---|
| 2026-08-16 | Qwen (prior) | Wrote fix/qt611-gallery-delegates: Qt 6.11 gallery/detail fix + uncommitted hardening changes | Uncommitted diff on branch |
| 2026-08-18 | Claude (this) | Resumed the fix: fixed test target Qt6::Qml link, verified (check.sh, ctest, qmllint, smoke×3), committed `1833331`, built+installed alpha package. Authored beta plan with user decisions. Rebuilt graphify graph. Created AI-Skills setup per maintainer directive. | HEAD 1833331, alpha installed, beta plan pending |
| 2026-08-18 | Claude + sub-agents | BETA_M1a orchestrated: sonnet sub-agent implemented the per-kind renderer contract in worktree `beta-m1a-renderer-contract`; separate Explore reviewer found 12 findings (5 must-fix); same implementer fixed all; verified 104 tests + 18 smoke cases; ff-merged as `cd2d61e`+`62bdbdc`. Key resulting contracts: StartSpec kind/content, per-kind paths/timeouts/limits, per-worker HOME (0700, `runtime/home-<serial>`), bounded stderr ring in status + exit-stderr in failure detail, `audio.forward`/`media.state` with generation-gated forwarding, kind-qualified quarantine identity with legacy migration, systemd 3G/400%/96. Next: BETA_M1b (`kwe-video-renderer`, libmpv). | HEAD 62bdbdc, M1a done |
| 2026-08-18 | Claude + sub-agents | BETA_M1b done (`7a2b402`+`3e83d2e`): `kwe-video-renderer` (libmpv SW render API, bgr0→BGRA8888 premultiplied, paced publish, keepalive, media-state pause/seek, exit 70/71/72/73), smoke-video.sh through the daemon. Review: 3 must-fix + 4 recommended, all fixed; M1a bug surfaced (media/audio acks rejected) fixed with last-wins ack acceptance. | M1b done |
| 2026-08-18/19 | Claude + sub-agents | BETA_M1c done (`51cb469`+`219ebbd`): static `preflight_video` (extension allowlist, ≤2 GiB, non-symlink), scan Video→RendererDependent, `kwe preflight --video`, smoke split (extension reject vs worker-side exit 73); 24 h duration bound implemented for real (fails open on unreadable). BETA_M1d done (`b208465`+`2b2ebd8`): `kwe-audio-worker` + daemon `--audio-capture`/`audio.status` (3 restarts/10 min, then disable), SO_PEERCRED silent-drop. Review: 2 HIGH fixed (nonblocking pw-dump read; ack ceiling max() never decreases). | M1c+M1d done |
| 2026-08-19 | Claude (this session) | BETA_M1e close-out in worktree `kwe-m1e` (branch `beta-m1e-m1-evidence`): per-kind video NPROC knob `--renderer-video-processes` (default 32768, video kind only; smoke workaround removed); `mpv` crate dropped (explicit `extern "C"` + `#[link(name = "mpv")]`, Cargo.lock −201 lines, rust-mpv THIRD_PARTY entry removed); deterministic pixel oracle (solid `#3366CC` mp4 through the daemon, seqlock frame-file parse, 9 pixels within 2 of expected BGRA, tolerance 4; empirical: libmpv aspect-letterboxes); `--probe` → `{"backend":"libmpv","client_api_version":"2.5","libmpv_supports_sw_render":true}`; `kwe diagnose` video lane; FEATURE_COMPATIBILITY content.video implemented; M1 exit gate (92-item corpus clean, 0 global diagnostics); docs updated (SUPERVISOR_API_V1, BETA_M1, FEATURE_COMPATIBILITY, THIRD_PARTY). Gates: 146 tests, smoke-video 11 / supervisor 17 / audio 5. Commit `feat(m1e): ...` at session end. | BETA_M1 done; next BETA_M2a |
| 2026-08-20 | Claude Fable + sub-agents (session 7) | **M4c done** (`e5d69e0` feat by prior session + this session's review cycle): adversarial reviewer (sonnet) found 1 MUST-FIX (user-apply precedence: Busy-as-failure backoff + TOCTOU displacing a fresh user renderer) + 4 recommended + 3 nits; implementer (sonnet) fixed all 8 in `520b2d7` — post-lock `foreign_renderer_live` check with non-failure `ApplyError::Yielded`, apply lane moved to a dedicated bound-1 worker thread (tick thread/API never blocks), stale-store output fallthrough, `Wait` verdict during supervisor recovery, rollback doc honesty; deviation: nothing-live+gate-closed verdict Yield→Hold (anti-storm). Verifier re-review: all CLOSED, 0 new MUST-FIX. Orchestrator independently ran check.sh + smoke-playlist-restart (green, 128 daemon/458 workspace tests), ff-merged, updated plan + this file. NOTE: sessions 2026-08-19 (BETA_M2, M3a–M3f, M4a, M4b) updated BETA_PLAN.md's change log but not this file — see the change log for those details. | M4c merged; M4d next |
