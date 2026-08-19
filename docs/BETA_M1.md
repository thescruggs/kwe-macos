# Beta M1 supervised video renderer

M1 proves the wallpaper contract end to end: a supervised, kind-specific
renderer that decodes real video in its own process and publishes validated
frames through the shared frame protocol. M1a generalized the renderer
contract (per-kind spawn, env allowlist, resource limits, bounded stderr
ring, audio/media plumbing); M1b adds the libmpv video renderer itself; M1d
adds the bounded PipeWire audio capture worker (`kwe-audio-worker`), the
daemon-side capture management (`--audio-capture`, `audio.status`), and the
real producer behind the `audio_bands` wire type. M1e closes the milestone:
the video kind gets its own process ceiling (the global `RLIMIT_NPROC`
default cannot host a desktop session), the `mpv` crate is removed in favor
of explicit FFI, the video lane gains a deterministic pixel oracle, and the
M1 exit gate is evidenced.

## Goal

`kwe-video-renderer` decodes video via libmpv's software render API
(`MPV_RENDER_API_TYPE_SW`) and publishes BGRA8888 premultiplied frames into
the shared `SharedFrameWriter` mapping, paced to the requested fps, with a
keepalive re-publish so a paused video never trips the supervisor's
frame-timeout. It is a supervised worker: same argv/env/rlimit contract as
the test renderer, same fault flags, media-state control from the daemon
(`playing`/`paused`/`stopped`), bounded input reads, and graceful SIGTERM.
Nothing is loaded into plasmashell.

## Run the suites

```sh
scripts/smoke-video.sh        # M1b + M1e: video renderer through the daemon,
                              #   incl. the deterministic pixel oracle
scripts/smoke-supervisor.sh   # M1a: supervisor fault/recovery contract
scripts/smoke-audio.sh        # M1d: bounded audio capture through the daemon
```

All three scripts build the workspace, use a private temporary socket/runtime/
state tree, generate synthetic fixtures with ffmpeg (never committed), and
remove everything on exit. They do not install a wallpaper or touch the
running Plasma session. `smoke-audio.sh` additionally creates an isolated
null sink (pactl module-null-sink or pw-cli adapter node) and directs the
capture worker at it, so the user's real default sink is never touched; it
prints `SKIPPED` and exits 0 when pw-record/pw-dump are missing, no PipeWire
control tool is available, or no reachable PipeWire session exists (checked
with a `pactl info` / `pw-cli info` probe).

## Acceptance evidence

Validated on 2026-08-18 (mpv 1:0.41.0, libmpv.so.2.5.0, CachyOS).

### M1a — renderer contract (commits `cd2d61e`, `62bdbdc`)

| Case | Expected containment | Result |
|---|---|---|
| healthy | live frames plus static fallback | reached `live`; valid P6 last-good persisted |
| explicit stop | bounded terminate and reap | stopped with no remaining worker |
| ignores `SIGTERM` | forced-kill fallback | process group killed and reaped after test grace |
| frame stall | progress deadline | three bounded attempts, then `frame_timeout` quarantine |
| corrupt header | protocol rejection | three bounded attempts, then `invalid_frame` quarantine |
| abrupt exit | exit observation | three bounded attempts, then `process_exit` quarantine |
| pre-frame stall | startup deadline | three bounded attempts, then `startup_timeout` quarantine |
| explicit retry | user-authorized recovery | quarantine cleared; unchanged identity reached `live` |
| daemon killed | no orphan renderer | parent-death signal removed the live worker |
| daemon restart | persistent safety record | unchanged identity remained quarantined |
| bounded stderr | 64 KiB ring per tick | tail surfaced on `renderer.status` for the live worker |
| handoff ack | generation-gated display | acknowledged handoff, pre-ack failure rolled back |
| memory pressure | contained and rolled back | forced allocation rejected, active worker preserved |
| forced kill | bounded stop grace | killed and reaped; `forced_kill_count` 1 |

### M1b — video renderer

| Case | Expected | Result |
|---|---|---|
| live start (default renderer path) | `kwe-video-renderer` resolved beside the daemon | phase `live`, kind `video`, content_hash, sequence advancing, last-good P6 |
| paused media state | keepalive re-publish | `pause=true` applied; `input_ack_sequence > 0` (ack round-trip); sequence still advances every interval; failures 0 |
| playing media state | resume | phase `live`, failures 0 |
| stopped media state | pause + seek 0 | `applied=Stop` on the stderr ring; keepalive keeps the sequence advancing; failures 0 |
| kill -9 of active worker | one failure + auto-restart | `process_exit signal_9` recorded in the restart window; new pid promoted; never quarantined |
| repeated kill -9, no intervening success | three-failure budget | quarantined with failures 3; `renderer.start` for the identity refused (phase `quarantined`) |
| missing content path | path-level rejection | `invalid_params`, nothing spawned |
| 64 KiB garbage content | backend rejection | worker exits 73; active base preserved; phase `rolled_back`, `last_failure_detail` names `exit_code_73` (M1c note: the smoke fixture now ends in `.mp4` — see the M1c table for the extension-reject split) |
| graceful SIGTERM | stop without restart | standalone: exit 0, `producer_state` `Stopping` (the daemon records no failure on graceful stops, so `last_failure` is not smoke-assertable); smoke: phase `stopped`, no worker, daemon healthy |
| input channel | decode + ack | `pointer_position`, `media_state`, `audio_bands` acked on stdout; malformed/junk lines ignored silently |
| media-state mapping | pause/seek | `paused`→pause, `playing`→pause=false, `stopped`→pause + seek 0 |
| unit tests | 8 in-crate | rgb24→BGRA exact bytes, keepalive decision, media mapping, fault-flag exit table, duration-bound decision (M1c) |

### M1c — static video preflight + catalog compatibility flip

| Case | Expected | Result |
|---|---|---|
| allowlisted extension (mp4/webm/mkv/mov/avi/wmv/flv/m4v/ogv, case-insensitive) | static preflight passes | unit: all 9 extensions pass; `.MP4` normalized to `video-mp4` |
| disallowed extension | rejected with reason | unit `unsupported video extension`; supervisor `validate()` bails with the same reason; daemon `renderer.start` → `invalid_params` before any spawn |
| missing file | rejected with reason | unit `cannot stat video`; daemon `invalid_params` (M1b case 7 unchanged) |
| symlink | rejected | unit + supervisor test, `video entry must not be a symlink` |
| oversized (> 2 GiB) | rejected | sparse `set_len` fixture — no big allocation |
| directory entry | rejected | unit, `video entry must be a regular file` |
| canonicalized spawn path | `into_validated` stores the resolved path | M1a TOCTOU fix preserved (supervisor unit test) |
| catalog video row | `planned` → renderer-dependent | `RendererDependent` + "libmpv worker with software fallback; static video preflight"; scan unit test added |
| CLI `kwe preflight --video` | JSON report, exit 2 on unsafe | mirrors `--path`; exactly one of `--path`/`--video` required |
| smoke split | extension reject vs worker-side rejection | case 8 renamed to `garbage.mp4` → worker exit 73 → `rolled_back`; case 9 `.bin` → `invalid_params` with preflight reason; case 10 `long-duration.mp4` (>24 h, generated with a `setpts` retimestamp) → worker exit 73 → `rolled_back` |
| duration bound | known duration over 24 h rejected by the worker | unit `duration_decision` (24 h passes, 24 h + 1 s rejected, unknown fails open); smoke case 10 asserts `last_failure_detail` names both `exit_code_73` and the 24 h diagnostic |

Whole-workspace gates: `cargo fmt --all -- --check`, `cargo clippy
--workspace --all-targets -- -D warnings`, and `cargo test --workspace
--all-targets` are clean (119 tests); `smoke-video.sh` (10 cases) and
`smoke-supervisor.sh` (15 cases) both pass.

### M1d — bounded audio capture

Validated on 2026-08-19 (PipeWire 1:1.6.8-1.1, CachyOS; capture directed at
a null sink, never the user's default).

| Case | Expected | Result |
|---|---|---|
| worker start | `--audio-capture` spawns the worker | `audio.status` `enabled: true`, live `pid`, `restarts` 0; the null sink name is passed through as `--audio-capture-node` (pw-dump resolution skipped) |
| renderer active | frames flow through the daemon | `input_ack_sequence` follows the promoted `display_generation` (advances across a stop/start generation bump); `input_protocol_errors` stays 0 |
| renderer stopped | silent latest-wins drop | `renderer.stop` produces no `event=api.client_error` storm; the daemon log carries only the rate-limited `event=audio.forward.dropped` note (1–10 lines over a 2 s window) |
| kill -9 worker | bounded restart | `restarts` 1, a new live `pid` replaces the killed one |
| SIGTERM daemon | graceful stop | worker logs `event=audio.worker.stopped` and its pid vanishes; no `forced_kill` line (exit-0 evidence — a non-child's exit code is not directly observable) |
| unit tests | 13 in-crate | parameter bounds, emission cadence, pw-dump sink resolution, response/refresh decisions, queue-of-1 latest-wins, stderr-ring budgets, restart-window pruning, restart-budget disable, shutdown reap, status shape |

M1d gates: `cargo fmt --all -- --check`, `cargo clippy --workspace
--all-targets -- -D warnings`, and `cargo test --workspace --all-targets`
are clean (132 tests); `smoke-audio.sh` (5 cases) passes, and
`smoke-video.sh` / `smoke-supervisor.sh` still pass.

### M1e — close-out: per-kind NPROC, mpv crate removal, pixel oracle

Validated on 2026-08-19 (mpv 1:0.41.0, libmpv.so.2.5.0, CachyOS).

| Case | Expected | Result |
|---|---|---|
| per-kind process ceiling | `--renderer-video-processes` (default 32768) applies to the video kind only; test/web/scene keep the global 1024; CLI override respected | unit `per_kind_process_ceiling_applies_only_to_the_video_kind` (Video 32768, Test/Web/Scene 1024, override 4096 honored, web address-space 16384 preserved) and `video_nproc_default_sits_above_the_desktop_thread_ceiling` (defaults 1024/32768); `smoke-video.sh` runs with no `--renderer-processes` override — the video lane itself is the proof (open risk 1, resolved) |
| mpv crate removal | no `mpv` dependency; API-version diagnostic unchanged; workspace builds and links | Cargo.toml drops `mpv 0.2.3`; Cargo.lock regenerated (201 deletions: mpv 0.2.3 and its 2016-era tree — num 0.1.43, rustc-serialize, rand 0.3, enum_primitive, log 0.3.9, num-bigint/complex/integer/iter/rational/traits); `mpv_client_api_version` declared in the same `extern "C"` block as the render API with `#[link(name = "mpv")]` explicit; `mpv_api=2.5` at renderer start unchanged (open risk 3, resolved) |
| pixel oracle | deterministic solid-color mp4 (64x64, 5 s, `color=c=0x3366CC:s=64x64:r=30`) through the full daemon pipeline; shared frame file parsed per FRAME_PROTOCOL_V1.md (seqlock snapshot); 9 sampled pixels within a documented delta | `ORACLE-OK worst_channel_deviation=2 tolerance=4`; observed BGRA (expected `(0xCC, 0x66, 0x33, 0xFF)` for `#3366CC`): (80,45)=`cb 66 31 ff`, (40,22)=`ca 66 32 ff`, (120,67)=`cb 66 32 ff`, (120,22)=`ca 66 32 ff`, (40,67)=`cb 65 32 ff`, (36,1)=`ca 66 32 ff`, (123,88)=`cb 66 32 ff`, (36,88)=`cb 66 32 ff`, (123,1)=`cb 66 32 ff` (hex, BGRA order); worst per-channel deviation 2, tolerance 4; alpha exact 0xFF. Empirical: libmpv aspect-letterboxes the 1:1 fixture into the 16:9 (160x90) target — content region x∈[35,125), full height, black corners — so the oracle samples inside the fitted region (documented semantic difference in FEATURE_COMPATIBILITY.md) |
| `--probe` | `kwe-video-renderer --probe` prints JSON and exits 0, no device needed | `{"backend":"libmpv","client_api_version":"2.5","libmpv_supports_sw_render":true}`, exit 0; `client_api_version` decodes via `MPV_MAKE_VERSION` (unit `api_version_decodes_like_the_removed_mpv_crate`); SW-render bound is libmpv ≥ 0.33 (unit `sw_render_support_bound_is_libmpv_033_or_newer`) |
| `kwe diagnose` video lane | reports the backend beside the binary | `video backend: {"backend":"libmpv","client_api_version":"2.5","libmpv_supports_sw_render":true}`; missing binary degrades to a bounded note |
| backend/hardware evidence | recorded | mpv 1:0.41.0; libmpv client API 2.5 (probe); `--hwdec=auto-safe` with one bounded retry with `--hwdec=no`; machine GPU lane from `kwe-vulkan --json`: NVIDIA GeForce RTX 3070 (discrete, Vulkan 1.4.341, logical device created, all external-memory/DMA-BUF extensions present), llvmpipe fallback, loader 1.4.357 |
| M1 exit gate — corpus | 92-item catalog indexes without crash; malformed/missing projects surface as actionable errors | `kwe diagnose` with the real catalog: scene 60, video 20, web 9, unknown 3, invalid 0; subscribed 0, awaiting download 0; 0 global diagnostics; malformed/missing entries are actionable `invalid_params` failures (smoke-video cases: `garbage.mp4` → worker exit 73 → `rolled_back` with `exit_code_73`; `.bin` → preflight `invalid_params`; missing path → `invalid_params`, nothing spawned) |
| M1 exit gate — UI | keyboard-usable gallery/detail actions | not in M1 scope: no gallery or detail surface exists in this milestone; carried forward to the M4 UI milestone honestly (gates, grants, C++/manager, and live Plasma are likewise out of scope here) |

M1e gates: `cargo fmt --all -- --check`, `cargo clippy --workspace
--all-targets -- -D warnings`, and `cargo test --workspace --all-targets`
are clean (146 tests; kwe-video-renderer 10 in-crate, kwe-daemon 59);
`smoke-video.sh` (11 cases, incl. the oracle) passes, and
`smoke-supervisor.sh` (17 cases) / `smoke-audio.sh` (5 cases) still pass.

## Failure and recovery cases

- **hwdec failure**: the session starts with `--hwdec=auto-safe`; an init or
  playback failure is retried exactly once with `--hwdec=no` (one bounded
  stderr line `event=renderer.video.hwdec_fallback`). A second failure is a
  backend rejection: bounded stderr diagnostic and exit 73, which the
  supervisor folds into `last_failure_detail=exit_code_73` and rolls back to
  the active base worker.
- **Keepalive**: if no new frame arrives within one pacing interval
  (e.g. paused, or a stalled decoder), the last published frame is
  re-published with a new sequence. An empty frame is never published.
- **Malformed SW frame**: the exact-size check against
  `FrameSpec.pixel_bytes` fails closed — the frame is skipped and counted;
  diagnostics are rate-limited (`event=renderer.video.invalid_frame`) so a
  pathological decoder cannot flood the 64 KiB stderr ring.
- **Format mismatch**: the worker requests bgr0 (the SW API's native
  little-endian BGRA byte layout, which renders as-is); the converter keeps
  defensive arms for rgb24, 0bgr, rgb0, and 0rgb in case a libmpv version
  answers with a different layout. Any other format is a backend rejection
  (exit 73). All accepted formats convert to BGRA8888 premultiplied
  (alpha 0xFF).
- **Startup hang**: a worker that neither exits nor publishes is killed by
  the bounded startup timeout, counted, and restarted up to the quarantine
  budget — recovery never restarts Plasma.

## Open risks

1. **RLIMIT_NPROC vs desktop thread counts (verified root cause).** The
   kernel's `RLIMIT_NPROC` check compares against `user->processes`, which
   counts every thread of the uid, not just processes. On this desktop uid
   1000 runs ~1265 threads, so the daemon's default `--renderer-processes`
   ceiling of 1024 makes every `pthread_create` fail with EAGAIN, and libmpv
   0.41's `mpv_create` failure path hangs forever inside
   `mp_shutdown_clients` (a condvar loop that never exits because the core
   thread never started) — the worker then dies to the bounded startup
   timeout and is quarantined after three failures. `smoke-video.sh` passes
   `--renderer-processes 4096` for the video lane; the daemon's production
   default should be revisited (M1e) so video rendering works on any desktop
   session, not only ones with fewer than 1024 threads.

   **M1e: resolved.** The video kind now carries its own process ceiling,
   `--renderer-video-processes`, defaulting to 32768 (the top of the
   validated range) and applied only to the video kind's rlimits; test, web,
   and scene keep the global 1024 (docs/SUPERVISOR_API_V1.md, per-kind
   limits). The `--renderer-processes 4096` workaround is gone from
   `smoke-video.sh` — the lane running unmodified is the proof (M1e
   acceptance, case 1). Per-renderer protection still comes from `RLIMIT_AS`
   plus the supervisor timeouts, not from `NPROC`.
2. **Address-space budget.** The test renderer's 384 MiB default kills
   libmpv silently (SIGSEGV from the nvidia VA-API mappings, which measure
   ~1–2 GiB of virtual address space). The video smoke uses 2048 MiB and the
   daemon's production video default is 4096 MiB, both verified. A future
   renderer kind must not inherit the 384 MiB test default.
3. **Direct FFI against system libmpv.** The `mpv` crate (0.2.3, pinned,
   MIT) cannot host the render API: its builder calls `mpv_initialize`
   before exposing the handle, and libmpv 0.41 aborts when a render context
   is created after initialization (empirically verified). The render API is
   therefore bound directly in `mpv_ffi` against the system libmpv
   (THIRD_PARTY.yml, revision 0.41.0). The binary hard-requires the system
   `libmpv.so`; a distro without the SW render API (pre-0.33) fails closed
   with exit 73 rather than misbehaving. The crate is used only for the
   client API-version diagnostic (`mpv_api=2.5`).

   **M1e: crate removed.** The `mpv` crate and its 2016-era dependency tree
   (201 deletions from Cargo.lock: mpv 0.2.3, num 0.1.43, rustc-serialize,
   rand 0.3, enum_primitive, log 0.3.9, num-bigint/complex/integer/iter/
   rational/traits) are gone; the diagnostic is now `mpv_client_api_version`
   declared in the same `extern "C"` block that binds the render API, with
   `#[link(name = "mpv")]` carried explicitly (THIRD_PARTY.yml, libmpv
   entry). Diagnostic behavior is unchanged: `mpv_api=2.5` at start, and the
   new `--probe` report (M1e acceptance).
4. **libmpv 0.41 `mpv_create` failure-path hang.** Independent of the NPROC
   trigger above, libmpv's shutdown loop can spin forever if
   `pthread_create` fails during client setup. The worker is safe only
   because the supervisor enforces bounded startup timeouts; a libmpv fix or
   preflight thread-count probe should be considered before the live-apply
   milestone.
5. **Keepalive vs frame timeout.** A paused video keeps publishing at the
   pacing interval, which the supervisor's frame timeout (default 1 s) must
   accommodate; the smoke exercises pause with the test timings. Tuning of
   interval × fps against the frame timeout is a live-apply concern.
6. **M1c boundary — resolved.** M1b's temporary path-level content check
   was replaced in M1c by the static `preflight_video` (regular non-symlink
   file, allowlisted container extension, ≤ 2 GiB). Decode and duration
   bounds are the worker's job: after the file loads, the renderer reads the
   mpv `duration` property (bounded `MPV_EVENT_FILE_LOADED` wait) and
   rejects a known duration over 24 h with a bounded stderr diagnostic and
   exit 73, failing open when the duration is unreadable (some containers).
   A corrupt file inside an allowlisted extension therefore still reaches
   the worker and fails closed with exit 73 (smoke case 8, `garbage.mp4`);
   media with a known duration over 24 h does the same (smoke case 10,
   `long-duration.mp4`), while a disallowed extension is now rejected before
   any worker spawns (smoke case 9).

## Renderer exit codes

| Code | Meaning | Supervisor mapping |
|---|---|---|
| 0 | graceful stop (SIGTERM) | normal stop |
| 70 | `--exit-after` synthetic fault | `process_exit` failure |
| 71 | `--memory-pressure-after` allocation denied | `process_exit` failure |
| 72 | memory-pressure allocation unexpectedly succeeded | `process_exit` failure |
| 73 | backend rejection (decode/render unusable, incl. after `--hwdec=no` retry, or a known duration over 24 h) | `exit_code_73` in `last_failure_detail` |
| signal | killed (e.g. kill -9) | `process_exit` with `signal_9` |

## Audio worker exit codes

The daemon restarts `kwe-audio-worker` on any unexpected exit (own process
group, `no_new_privs`, parent-death **SIGTERM** — deliberately not SIGKILL,
so a crashed daemon cannot orphan `pw-record`), at most 3 times within a
rolling 10-minute window; beyond that the worker is disabled for the daemon's
lifetime (`audio.status.disabled_reason` = `"too_many_restarts"`, logged
once). The worker pushes at most `--max-fps` frames per second and holds
only the latest window internally, so restarts are cheap and lossless by
design (latest-wins).

| Code | Meaning | Daemon mapping |
|---|---|---|
| 0 | graceful stop (SIGTERM; pw-record stopped first) | normal stop |
| 74 | capture-node resolution failure (pw-dump missing, unparsable, or no default sink / monitor node found) | restart (bounded), then disable |
| 75 | capture failure (pw-record missing, failed to start, or died) | restart (bounded), then disable |
| signal | killed (e.g. kill -9) | restart (bounded), then disable |
