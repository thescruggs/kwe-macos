# Beta M1 supervised video renderer

M1 proves the wallpaper contract end to end: a supervised, kind-specific
renderer that decodes real video in its own process and publishes validated
frames through the shared frame protocol. M1a generalized the renderer
contract (per-kind spawn, env allowlist, resource limits, bounded stderr
ring, audio/media plumbing); M1b adds the libmpv video renderer itself.

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
scripts/smoke-video.sh        # M1b: video renderer through the daemon
scripts/smoke-supervisor.sh   # M1a: supervisor fault/recovery contract
```

Both scripts build the workspace, use a private temporary socket/runtime/
state tree, generate synthetic fixtures with ffmpeg (never committed), and
remove everything on exit. They do not install a wallpaper or touch the
running Plasma session.

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
| 64 KiB garbage content | backend rejection | worker exits 73; active base preserved; phase `rolled_back`, `last_failure_detail` names `exit_code_73` |
| graceful SIGTERM | stop without restart | standalone: exit 0, `producer_state` `Stopping` (the daemon records no failure on graceful stops, so `last_failure` is not smoke-assertable); smoke: phase `stopped`, no worker, daemon healthy |
| input channel | decode + ack | `pointer_position`, `media_state`, `audio_bands` acked on stdout; malformed/junk lines ignored silently |
| media-state mapping | pause/seek | `paused`→pause, `playing`→pause=false, `stopped`→pause + seek 0 |
| unit tests | 7 in-crate | rgb24→BGRA exact bytes, keepalive decision, media mapping, fault-flag exit table |

Whole-workspace gates: `cargo fmt --all -- --check`, `cargo clippy
--workspace --all-targets -- -D warnings`, and `cargo test --workspace
--all-targets` are clean (111 tests); `smoke-video.sh` (8 cases) and
`smoke-supervisor.sh` (15 cases) both pass.

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
6. **M1c boundary.** `preflight_video` (media-level validation of the
   content path) is deliberately deferred to M1c; M1b validates content only
   at the path level, so a corrupt file reaches the worker and fails closed
   with exit 73 (verified by the garbage-content case).

## Renderer exit codes

| Code | Meaning | Supervisor mapping |
|---|---|---|
| 0 | graceful stop (SIGTERM) | normal stop |
| 70 | `--exit-after` synthetic fault | `process_exit` failure |
| 71 | `--memory-pressure-after` allocation denied | `process_exit` failure |
| 72 | memory-pressure allocation unexpectedly succeeded | `process_exit` failure |
| 73 | backend rejection (decode/render unusable, incl. after `--hwdec=no` retry) | `exit_code_73` in `last_failure_detail` |
| signal | killed (e.g. kill -9) | `process_exit` with `signal_9` |
