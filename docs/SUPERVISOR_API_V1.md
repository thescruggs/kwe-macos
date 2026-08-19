# Renderer supervisor API v1

## Boundary

`kwe-daemon` owns renderer launch, health observation, termination, restart,
and quarantine. The renderer remains an independently killable process and the
display client receives only a validated frame-file path and recovery status.
No renderer command is executed through a shell.

The alpha control transport remains the bounded newline-delimited JSON protocol
documented in `PROTOCOL_V1.md`. These additive methods use version `1`:

- `renderer.start`
- `renderer.status`
- `renderer.stop`
- `renderer.retry`
- `renderer.ack`
- `renderer.input`
- `audio.forward` *(BETA_M1a)*
- `media.state` *(BETA_M1a)*
- `audio.status` *(BETA_M1d)*

## Start and retry

`renderer.start` accepts:

```json
{
  "wallpaper_id": "synthetic-canary",
  "content_hash": "sha256-placeholder",
  "width": 960,
  "height": 540,
  "fps": 30,
  "kind": "test",
  "content": "/path/to/content"
}
```

*(M1a: `kind` and `content` replaced the alpha's `scene_path`.)* `kind` names
the renderer family — `test`, `video`, `web`, or `scene` — and defaults to
`test`. `content` is the validated content path: a video file for `video`, a
directory with an `index.html` for `web`, and a scene file for `scene`. Test
takes no content; every other kind requires it, and a kind/content mismatch is
rejected. Content is validated before launch: video runs the static
`preflight_video` (regular non-symlink file, allowlisted container extension
`mp4|webm|mkv|mov|avi|wmv|flv|m4v|ogv`, ≤ 2 GiB), and its rejection reasons
propagate as the `invalid_params` detail; web runs `preflight_web` with no
permission grants (network stays disabled); scene keeps its existing
`preflight_scene`. Decode failures and a known duration over 24 h are the
worker's job: `kwe-video-renderer` rejects them with exit 73, folded into the
failure record as `exit_code_73` (an unreadable duration fails open).

Identity components are restricted to 1–128 ASCII letters, digits, `.`, `_`,
and `-`. Frame protocol v1 bounds dimensions and allocation size; FPS is
bounded to 1–240.

`renderer.start` refuses to launch an unchanged quarantined identity.
`renderer.retry` explicitly clears that identity's failure record and starts a
new bounded attempt. A changed content hash naturally receives a new failure
budget.

Synthetic `test_fault` parameters are rejected unless the daemon was launched
with `--allow-test-faults`. They are development-only and support
`startup_hang`, `hang`, `corrupt`, `exit`, `ignore_term_hang`, and
`memory_pressure`. The last form also requires a bounded `mib` value.
*(M1a: `stderr_lines` — ask the test renderer to print N diagnostic lines at
startup — is gated by the same flag.)*

## Status

The result contains:

```json
{
  "phase": "live",
  "kind": "test",
  "wallpaper_id": "synthetic-canary",
  "content_hash": "sha256-placeholder",
  "pid": 1234,
  "frame_file": "/run/user/1000/kwe/renderers/frame-1000-1.bin",
  "last_good_file": "/home/user/.local/state/kwe/last-good-a.ppm",
  "sequence": 42,
  "failures": 0,
  "restart_count": 0,
  "forced_kill_count": 0,
  "last_failure": null,
  "last_failure_detail": null,
  "resource_limits": {
    "address_space_mib": 4096,
    "file_size_mib": 160,
    "open_files": 256,
    "processes": 1024,
    "core_dump_bytes": 0
  },
  "stderr_tail": ["event=renderer.stderr_line index=99"],
  "stderr_dropped_bytes": 0
}
```

*(M1a: `kind`, `stderr_tail`, and `stderr_dropped_bytes` were added;
`resource_limits` now reports the active worker's per-kind budget.)* `kind`
names the supervised renderer family. `stderr_tail` holds at most the newest
64 lines / 16 KiB of renderer diagnostics (newest last) and
`stderr_dropped_bytes` counts bytes evicted from that ring — diagnostics only,
never parsed as commands. `audio_pending`, `audio_coalesced`, `media_pending`,
and `media_coalesced` mirror the pointer counters for the two control streams
below.

Phases are `idle`, `starting`, `canary`, `live`, `restarting`, `awaiting_ack`,
`rolled_back`, `stopped`, and `quarantined`. Stable failure classes are
`startup_timeout`, `frame_timeout`, `invalid_frame`, `process_exit`, and
`launch_failed`, and `resource_limit`.

The PID/frame/sequence fields always describe the active display source.
Candidate state is separate in `requested_*`, `candidate_pid`,
`candidate_frame_file`, and `candidate_sequence`. During a successful swap,
`previous_pid`, `previous_frame_file`, `display_generation`, and
`awaiting_display_ack` describe the bounded handoff.

The display client acknowledges a mapped generation with:

```json
{"generation": 2}
```

A stale or future generation is rejected without changing process state. A
matching `renderer.ack` commits the candidate's static fallback and reaps the
previous worker. If acknowledgement never arrives, the same action occurs
after the bounded handoff timeout. A new candidate cannot start while a
handoff remains unacknowledged.

## Normalized pointer position

`renderer.input` accepts `generation`, `phase`, `x`, and `y`. The generation
must exactly match the current promoted display generation; phase is `enter`,
`move`, or `leave`; coordinates must be finite normalized values in `0..=1`.
The daemon quantizes coordinates and routes the bounded event only to the
active renderer. Candidates and retired workers receive no input.

*(M1a: the optional `button` field — `primary`, `secondary`, or `middle` —
passes through to the wire `button_event`, which requires a `down`/`up`
phase; phase names `down` and `up` are accepted alongside `enter`/`move`/
`leave`.)*

Status exposes `input_sequence`, `input_ack_sequence`, `input_pending`,
`input_coalesced`, `input_protocol_errors`, `pointer_inside`, `pointer_x`, and
`pointer_y`. Sequence numbers report accepted and renderer-observed events;
coordinates are the quantized unsigned 16-bit values. See
`docs/INPUT_PROTOCOL_V1.md`.

The status path is diagnostic, not a heartbeat from the display client. The
daemon watches frame progression even when no client is connected.

## Audio and media control

*(BETA_M1a: daemon-side producers/forwarders for the two media wire types.)*

`audio.forward` accepts `generation` and a stereo `frame` of `f32` bands per
channel. Band counts must be exactly 16, 32, or 64 (values finite, `0..=1`):

```json
{
  "generation": 4,
  "frame": {"left": [0.1, 0.1, 0.1, 0.1, 0.1, 0.1, 0.1, 0.1, 0.1, 0.1, 0.1, 0.1, 0.1, 0.1, 0.1, 0.1], "right": [0.1, 0.1, 0.1, 0.1, 0.1, 0.1, 0.1, 0.1, 0.1, 0.1, 0.1, 0.1, 0.1, 0.1, 0.1, 0.1]}
}
```

`media.state` accepts `generation` plus `playback` (`playing`, `paused`, or
`stopped`) and optional `title`, `artist`, `album`, `position_seconds`, and
`duration_seconds` (finite, `0..=86400`):

```json
{
  "generation": 4,
  "playback": "paused",
  "title": "Track",
  "position_seconds": 12.5
}
```

Both are encoded through the versioned protocol types in
`docs/INPUT_PROTOCOL_V1.md` — band counts and value ranges are rejected by the
protocol constructors before any worker message exists. The generation must
match the current promoted display generation exactly (like pointer input).
A stale generation or an absent promoted renderer fails the request with
`{"error": "supervisor_failed", "detail": ...}` (the `detail` names the
reason, e.g. `audio frame display generation is stale or invalid`); retry
only after re-reading `renderer.status` for the current `display_generation`.
Each stream is latest-wins: one pending frame per stream, replaced — not
queued — when the worker pipe is under backpressure, counting into
`audio_coalesced` / `media_coalesced`.

### BETA_M1d: the daemon's audio capture producer

`kwe-daemon --audio-capture` spawns `kwe-audio-worker` (default
`kwe-audio-worker` beside the daemon executable; `--audio-worker <path>` and
`--audio-capture-node <id>` override the binary and the PipeWire capture
target). The worker is managed like a renderer child — own process group,
`no_new_privs`, parent-death signal, bounded SIGTERM-then-SIGKILL stop — but
with two differences: it is restarted on *any* unexpected exit (at most 3
restarts within a rolling 10-minute window; beyond that it is disabled for the
daemon's lifetime with a one-time log), and it inherits the daemon environment
because PipeWire capture needs the session's `XDG_RUNTIME_DIR`.

`audio.status` (no params) reports:

```json
{"enabled": true, "pid": 1234, "restarts": 0, "disabled_reason": null}
```

`enabled` mirrors the `--audio-capture` flag; `pid` is the live worker (or
`null`); `restarts` counts bounded respawns; `disabled_reason` is
`"too_many_restarts"` once the budget is exhausted. Without
`--audio-capture`, `enabled` is `false` and the daemon never spawns the
worker.

Producer contract (`kwe-audio-worker`):

- It pushes at most `--max-fps` frames per second (default 30), one request
  per connection, each envelope `{"version":1,"id":N,"method":"audio.forward",
  "params":{"generation":G,"frame":{"left":[...],"right":[...]}}}`.
- The `generation` is learned from `renderer.status` and refreshed whenever a
  push is rejected with `supervisor_failed` (stale generation). While no
  renderer has ever been promoted (`display_generation` 0) the worker holds a
  single latest frame and re-polls `renderer.status` on a bounded interval;
  it does not spam rejections at the daemon.
- While the daemon runs with `--audio-capture` and no renderer is promoted,
  the daemon converts its own worker's
  `{"error": "supervisor_failed", "detail": "no promoted renderer is
  available for audio forwarding"}` responses into `{"ok": true,
  "result": {"status": "dropped"}}` — a silent latest-wins drop with
  rate-limited daemon logging. Every other caller (and every other failure
  detail, including stale generations) keeps the `supervisor_failed` error
  shape unchanged.
- Worker exit codes: 0 graceful SIGTERM, 74 capture-node resolution failure
  (pw-dump missing/unparsable/no sink), 75 capture failure (pw-record missing,
  failed to start, or died). The daemon's restart policy treats every exit
  while running as unexpected except its own shutdown SIGTERM.

## Lifecycle and recovery

The daemon starts each worker in a new process group with `no_new_privs` and a
Linux parent-death signal. It opens only the configured renderer executable
for the requested kind — a missing kind binary fails the launch closed rather
than falling back to another renderer — and passes bounded arguments directly.
*(M1a: `--content <path>` follows `--fps` for kinds with content.)* The
inherited environment is replaced by a per-kind allowlist: every renderer gets
`PATH=/usr/bin:/usr/sbin:/bin` and its own private `HOME` at
`<daemon runtime dir>/home-<launch_serial>` (created chmod 0700 per launch —
web renderers hold a profile lock under `$HOME`, so a shared `HOME` would
make the canary and active worker contend during handoff); the web kind
additionally inherits the daemon's `XDG_RUNTIME_DIR` (Chromium needs it;
video/scene/test deliberately do not get it). Renderer stderr is piped into a
bounded ring (64
lines / 16 KiB, drained nonblocking per supervisor tick and once more after
exit) and surfaced through `renderer.status`; on an unexpected exit the
drained tail (last 8 lines) is folded into the failure `detail` so crash
diagnostics survive the worker teardown. It cannot grow logs or feed the
control stream. Before exec it also applies finite address-space, output-file,
descriptor, and UID-scoped process ceilings and disables core dumps. Any
failure to install the policy fails the launch.

Per-kind policies replace the alpha's single set: startup timeouts default to
6 s for video and 10 s for web (Chromium's cold start), everything else keeps
the global 3 s; resource limits default to the global budget
(address-space 4096 MiB, file 160 MiB, 256 descriptors, 1024 processes) except
web, which needs a 16384 MiB virtual address space and 1024 descriptors
because V8 reserves a 4 GiB cage before `main`, and video, which overrides the
process ceiling with `--renderer-video-processes` (default 32768 — the top of
the validated range). The video override exists because the kernel's
`RLIMIT_NPROC` check counts every thread of the uid (`user->processes`), so the
global 1024 ceiling guards the whole desktop, not the worker, and a normal
desktop session commonly runs more than 1024 threads — libmpv's thread
creation then fails with EAGAIN and `mpv_create` hangs in its failure path.
Per-renderer protection comes from `RLIMIT_AS` plus the supervisor timeouts
(startup/frame/handoff), not from `NPROC`. The daemon flags
`--renderer-video-startup-timeout-ms`, `--renderer-web-startup-timeout-ms`,
`--renderer-web-address-space-mib`, `--renderer-web-open-files`, and
`--renderer-video-processes` tune these; frame timeouts and the canary stay
global. The web kind currently keeps the global 1024-process ceiling and is
expected to need its own knob when the Chromium worker lands (M2) — Chromium
spawns a process tree, and the same uid-wide `RLIMIT_NPROC` math applies. Per-kind renderer binaries default
to `kwe-<kind>-renderer` beside the daemon executable (`--renderer-video`,
`--renderer-web`, `--renderer-scene` override; `--renderer` keeps meaning the
test kind).

Before promotion, the daemon requires at least three advancing frames across a
bounded canary interval. A failed candidate is restarted and quarantined
without replacing a healthy active source. On promotion, the old source remains
available until display acknowledgement. Failure before acknowledgement
restores the old worker and retains its static fallback.

On process termination, the daemon sends `SIGTERM`, waits for a bounded grace period,
escalates the entire worker process group to `SIGKILL`, and reaps the child.
Automatic restarts use a bounded delay and failure count. The default third
equivalent failure persists a quarantine record and prevents another launch.

The state file is `supervisor-v1.json`, capped at 1 MiB and 256 identities in a
private state directory. *(M1a: identity keys are kind-qualified
`wallpaper_id:content_hash:kind` so a failing video cannot quarantine the same
id/hash under web or scene; pre-M1a records keyed `wallpaper_id:content_hash`
still match on lookup and migrate onto the qualified key on the next failure.)*
The latest acknowledged stable BGRA frame is converted to a bounded
P6 PPM still and atomically stored in alternating `last-good-a.ppm` and
`last-good-b.ppm` slots. The JSON state pointer changes only after the inactive
slot is complete, so the previously acknowledged still survives an interrupted
commit. These static files remain usable across daemon restart and do not
depend on the live mmap.

## Current limits

- The generated and video renderers are the production-shaped workers
  exercised here; web and scene binaries arrive in later milestones. Video
  content validation is path-level in the daemon but decode-level in
  `kwe-video-renderer` (exit 73 on unreadable media), and the video lane runs
  end-to-end in `scripts/smoke-video.sh` including a deterministic pixel
  oracle (docs/BETA_M1.md, M1e acceptance).
- The packaged systemd unit bounds aggregate memory, swap, CPU, and task usage
  (raised in M1a to fit daemon + audio worker + Chromium);
  GPU-specific budgets and a stable seccomp allowlist remain later hardening
  work.
- The M1e Plasma display bridge consumes this local JSON contract in isolated
  staging/offscreen tests but is not installed or enabled on the live desktop;
  no D-Bus interface is exposed yet.
- The alpha status/ack exchange still uses the local JSON socket; a future
  stable display contract may move this control path to D-Bus.
