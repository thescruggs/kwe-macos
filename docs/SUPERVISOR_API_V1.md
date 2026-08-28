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
- `permissions.get` *(BETA_M2c)*
- `permissions.set` *(BETA_M2c)*
- `permissions.list` *(BETA_M2c)*
- `wallpaper.outputs` *(BETA_M4a)*
- `wallpaper.apply` *(BETA_M4a)*
- `wallpaper.restore` *(BETA_M4a)*
- `wallpaper.assignments` *(BETA_M4a)*

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

*(M1a: `kind` and `content` replaced the alpha's `scene_path`.)* *(F1: an
optional `"scaling": "aspect"|"fill"|"stretch"` (default `aspect`) is passed
to every renderer as `--scaling`; the video renderer letterboxes/crops/
stretches the clip, the scene renderer maps the declared scene rectangle
onto the canvas by it, web and test accept it for the uniform argv.)*
`kind` names
the renderer family — `test`, `video`, `web`, or `scene` — and defaults to
`test`. `content` is the validated content path: a video file for `video`, a
directory with an `index.html` for `web`, and a scene file for `scene`. Test
takes no content; every other kind requires it, and a kind/content mismatch is
rejected. Content is validated before launch: video runs the static
`preflight_video` (regular non-symlink file, allowlisted container extension
`mp4|webm|mkv|mov|avi|wmv|flv|m4v|ogv`, ≤ 2 GiB), and its rejection reasons
propagate as the `invalid_params` detail; web runs `preflight_web` with no
permission grants (network stays disabled); scene keeps its existing
`preflight_scene`. *(M2c: the web preflight does not consider grants — the
per-wallpaper network grant is applied at spawn, see [Permission
grants](#permission-grants).)* Decode failures and a known duration over 24 h are the
worker's job: `kwe-video-renderer` rejects them with exit 73, folded into the
failure record as `exit_code_73` (an unreadable duration fails open).

Identity components are restricted to 1–128 ASCII letters, digits, `.`, `_`,
and `-`. Frame protocol v1 bounds dimensions and allocation size; FPS is
bounded to 1–240.

`renderer.start` refuses to launch an unchanged quarantined identity.
`renderer.retry` explicitly clears that identity's failure record and starts a
new bounded attempt. A changed content hash naturally receives a new failure
budget. Failure records are scoped to the build that earned them (BETA B4,
`build_id` in `supervisor-v1.json`); a daemon whose own or renderer
binaries changed drops them at load, logging
`event=renderer.quarantine_reset reason=build_changed dropped=N`.

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
  "quarantined": false,
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
below. *(M2c: `audio_grant_dropped` counts audio frames silently dropped
because the active wallpaper has no audio grant — see Permission grants; it is
a lifetime counter, reset on daemon restart, not per worker.)*

Phases are `idle`, `starting`, `canary`, `live`, `restarting`, `awaiting_ack`,
`rolled_back`, `stopped`, and `quarantined`. Stable failure classes are
`startup_timeout`, `frame_timeout`, `invalid_frame`, `process_exit`,
`launch_failed`, `resource_limit`, and `refused` (BETA B4: a candidate's
exit 73/74 before first publish — reported, never restarted, never
counted). `quarantined` (B4) is true when the requested identity's
persisted record is quarantined; a quarantined `renderer.start` leaves the
record's `last_failure`/`last_failure_detail` in the status so the caller
can say why. `scaling` (F1) is the active worker's scaling mode (the
requested one while nothing is live) — the plugin reads it for the frame →
output mapping.

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

## Permission grants

*(BETA_M2c: daemon-owned per-wallpaper permission grants.)*

The daemon owns each wallpaper's permission record in `permissions-v1.json`,
stored in the private state directory beside `supervisor-v1.json`. A record
grants or denies three capabilities — `network`, `audio`, and `pointer` — as
booleans, all three stored. The file is bounded (≤ 256 records, 1 MiB) and
written atomically; a corrupt file is renamed aside
(`permissions-v1.json.invalid-<unix_seconds>-<unix_nanos>`) and the store
starts empty with a one-time log. Invalid siblings are pruned to the newest
8 (shared `persist` policy), so repeated corruption cannot accumulate
quarantine files without bound. Identity keys follow the same rule as
`renderer.start` (1–128 ASCII letters, digits, `.`, `_`, `-`).

The effective record defaults to the documented policy for every wallpaper
without a record: **network off, audio off, pointer on**. Pointer stays on
because interactivity is core wallpaper behavior; the pointer grant exists for
future stricter modes and is not enforced yet.

- `permissions.get` `{"wallpaper_id": "..."}` → the effective record,
  `{"granted": {"network": false, "audio": false, "pointer": true}}`
  (defaults when no record exists). An invalid `wallpaper_id` fails with
  `invalid_params`.
- `permissions.set` `{"wallpaper_id": "...", "network": true}` → patch
  semantics: only the provided fields change, the rest keep their current
  values (or the defaults); the answer is the new effective record,
  `{"granted": {...}}`. Unknown fields are rejected, and the store is bounded:
  the 257th record fails with `permissions_failed` naming the safety limit.
- `permissions.list` → `{"grants": {"<wallpaper_id>": {...}}}` — every stored
  record (≤ 256).

### Enforcement

- **Network**: the per-wallpaper network grant is the only path to
  `--allow-network` for the web worker — the M2b per-request `allow_network`
  test hook is removed (the parameter is now rejected as an unknown field). At
  spawn the supervisor appends `--allow-network` only when the wallpaper's
  grant record allows it; without the grant the bwrap sandbox runs
  `--unshare-net` (no access to the network namespace, not even loopback).
  Revocation takes effect on the next `renderer.start` for that identity.
- **Audio**: capture is global — `kwe-audio-worker` keeps running and
  capturing — but the grant gates *delivery*: `audio.forward` frames for the
  active worker's wallpaper without the audio grant are dropped silently
  (latest-wins, bounded-rate logging) and counted in `audio_grant_dropped`
  (see Status). Granting audio resumes delivery immediately; no worker
  restart is needed.
- **Pointer**: pass-through stays enabled by default; the pointer grant is
  reserved for future stricter modes and is not enforced yet.

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

*(M2c: the audio grant gates delivery — see Permission grants. Frames for a
wallpaper whose record denies audio are dropped before they reach the worker
pipe, counted in `audio_grant_dropped`; the worker itself keeps running, and
the ack protocol is unaffected.)*

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

## Wallpaper apply and assignment control *(BETA_M4a)*

The live-apply lane gives clients a bounded transaction that maps an output
name to a catalog wallpaper: validate, start the renderer through the
supervisor, wait (bounded) for promotion to a live phase, persist the
assignment, then switch the Plasma wallpaper plugin via the KDE wallpaper
scripting API. The transaction semantics, exact script templates, store
format, and safe-mode restore contract are documented in
`docs/BETA_M4.md`; this section is the wire contract.

- `wallpaper.outputs` — live output enumeration. No params. Returns
  `{"outputs": [{"name", "screen", "desktop_id", "desktop_index",
  "geometry", "enabled", "connected", "wallpaper_plugin", "config_group",
  "image"}]}`. The enumeration combines one bounded `kscreen-doctor` run
  (geometry/enabled/connected) with one read-only `evaluateScript` probe
  (desktop mapping) and is cached 5 s per call — never indefinitely, and
  the apply transaction always probes fresh.
- `wallpaper.apply` — params `{"output", "wallpaper_id", "kind", "content",
  "width"?, "height"?, "fps"?, "retry"?, "scaling"?}` (`fps` 30 default;
  `retry` false; `scaling` `aspect`). **F1 (2026-08-22):** `width`/`height`
  omitted → the frame canvas is derived from the output's geometry (its own
  aspect, long edge capped at 2560, even pixels, never below 64; no geometry
  → the legacy 960x540); explicit values are used as given, bounded by the
  frame protocol as before. `scaling` is `aspect` (fit, letterbox — the
  pre-F1 behaviour), `fill` (crop to cover) or `stretch`; it is passed to
  the renderer as `--scaling`, reported by `renderer.status` (`scaling`) for
  the display plugin's frame → output mapping, and persisted in the
  assignment (`assignments-v1.json`, additive field, older records read as
  `aspect`). The success result's `applied` record carries `width`,
  `height` and `scaling` as resolved. `kind`/`content` follow the
  `renderer.start` rules; the `test` kind is not assignable. Completes on
  renderer *promotion* (phase `live` or `awaiting_ack`), not on display
  acknowledgement. Success returns the persisted assignment with
  `applied_at_unix_seconds`. `retry: true` (BETA B4) clears this
  wallpaper/content/kind identity's failure record before starting —
  exactly what `renderer.retry` does — for a client that saw
  `apply_quarantined` and wants to try anyway; it never clears anything
  else.
- `wallpaper.restore` — params `{"output"}`. Reverts the saved previous
  plugin/config-group/image, or restores the stock `org.kde.image` plugin
  with the first present stock image when no assignment exists; returns
  `{"output", "mode": "assignment"|"stock", "wallpaper_plugin",
  "image"}`. Always succeeds on a real output.
- `wallpaper.assignments` — the full bounded assignment store. No params.

Error responses (all fail closed; detail is bounded): `invalid_params`,
`apply_unknown_wallpaper`, `apply_incompatible`, `output_missing`,
`apply_busy` (no detail), `shell_unreachable`, `display_unavailable`,
`apply_failed` (already rolled back), `apply_quarantined`, `service_stale`,
`restore_failed`, and `apply_unavailable` when the daemon has no apply lane.

`apply_quarantined` (BETA B4): the supervisor's persisted record for this
identity is quarantined — `max_failures` strikes under the running build.
Detail: `disabled after N failures under this build; last failure:
<record detail>`. Nothing was started and no shell script ran. Failure
records are scoped to the build that earned them (`build_id` in
`supervisor-v1.json`: daemon version + executable stamp + every renderer
binary's size/mtime); a daemon built differently drops them at load
(`event=renderer.quarantine_reset`), so an upgrade never inherits a ban.
Renderer *refusals* — a candidate exiting 73 (`backend_reject`) or 74
(`no_drawable_content`) before its first publish — are not strikes at all:
the supervisor reports `last_failure: "refused"` with the worker's detail,
schedules no restart, and persists nothing (phase `stopped`, or
`rolled_back` when a previous renderer is live); the apply lane reports
them as `apply_failed` with that detail.

`service_stale` (BETA B4): the daemon's own executable was replaced on disk
after it started (package upgrade without a restart). `wallpaper.apply` is
refused with the restart command in the detail; `health` carries
`service_stale: true`. Nothing else is gated (the playlist lane keeps
running its own applies).

`display_unavailable` is the narrow case where the enumeration never ran
because no display server was in reach — a daemon started before its desktop
session, which cannot enumerate outputs and reports so with an actionable
detail rather than an empty list. The daemon first tries to recover a display
environment from the systemd user manager; this code means that failed too.

The switch/restore/probe scripts are executed with
`qdbus <service> /PlasmaShell evaluateScript <script>` — no shell, argv
only, bounded 5 s deadline, 64 KiB output caps; the daemon never embeds
wallpaper content in a script, and the pure script builders are
unit-tested for exact strings and escaping.

*(BETA_M4c:)* `--plasma-switch-command <path>` replaces the **whole**
Plasma shell evaluation boundary — enumeration and switch scripts alike —
with `<path> <script>` run through the same bounded machinery (5 s
deadline, 64 KiB caps, no shell, no environment mutation). Integration
smokes stub the boundary with it so no live session is touched; live
enablement (BETA_M4d) leaves it unset and runs the real qdbus.

*(BETA_M4c:)* `--playlist-output <output>` — the output playlist-driven
assignments target. The playlist session drives the same apply transaction
on entry changes (timer advance, policy switch, manual play,
resume-after-restart); there is **no new RPC method** — the output is a
daemon flag, not a client param. When unset, the lane resolves the output
at apply time: the last assigned output whose wallpaper is a member of the
active playlist, else the first enabled and connected output, else
`output_missing`. A failed playlist apply rolls back exactly like
`wallpaper.apply` (renderer stopped if ours, assignment store reverted; the
display freezes on the supervisor's last-known-good frame until the next
successful apply) and backs off exponentially (1 s doubling to a 30 s cap);
while a foreign (user) renderer is live the session yields to it — a `Busy`
from the shared transaction lock or a foreign renderer live after the lock
is a transient yield, never a failure, and clears any armed backoff.

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
6 s for video, 6 s for scene (two sequential libmpv VideoLayer opens), and 10
s for web (Chromium's cold start); everything else keeps the global 3 s;
resource limits default to the global budget
(address-space 4096 MiB, file 160 MiB, 256 descriptors, 1024 processes) except
web and video. *(M2b:)* web overrides all three — a 131072 MiB virtual
address space, 1024 descriptors, and a 32768-process ceiling — because
Chromium 151's V8 sandbox reserves ~53 GiB of virtual address space per
process at exec, and the DevTools pipe bootstrap fails *silently* (no stderr,
the browser just never answers the pipe) whenever `RLIMIT_AS` sits below a
~98 GiB budget floor; the old 16384 MiB budget SIGTRAPs the browser at exec.
The budget is purely virtual — resident RSS stays ~250 MB per browser process
(measured) — so resident protection comes from the supervisor timeouts and,
at runtime, from the systemd `MemoryMax` of the containing unit. The video
override remains the process ceiling with `--renderer-video-processes`
(default 32768 — the top of the validated range); the web process ceiling
exists because spawning the bwrap sandbox forks a new process tree and the
kernel's `RLIMIT_NPROC` check counts every thread of the uid
(`user->processes`), so the global 1024 ceiling guards the whole desktop, not
the worker, and a normal desktop session commonly runs more than 1024 threads
— libmpv's thread creation then fails with EAGAIN and `mpv_create` hangs in
its failure path, and bwrap's fork fails with EAGAIN the same way (measured).
The scene kind also overrides the process ceiling with
`--renderer-scene-processes` (default 32768) because it may own two libmpv
cores; its file-size ceiling remains 160 MiB, matching bounded package-video
extraction. Per-renderer protection comes from `RLIMIT_AS` plus the supervisor timeouts
(startup/frame/handoff), not from `NPROC`. The daemon flags
`--renderer-video-startup-timeout-ms`, `--renderer-web-startup-timeout-ms`,
`--renderer-web-address-space-mib`, `--renderer-web-open-files`,
`--renderer-video-processes`, `--renderer-web-processes`, and
`--renderer-scene-processes` tune these; `--renderer-scene-startup-timeout-ms`
tunes the scene load budget;
frame timeouts and the canary stay global. *(M2b:)* web renderers also take
`--renderer-web-heartbeat-ms` (default 5000) and
`--renderer-web-heartbeat-max-failures` (default 3): the worker probes the
page's renderer main thread every interval and exits 73 after consecutive
failures, so a page wedged after first paint cannot hide behind the keepalive
re-publication forever (docs/BETA_M2.md §5.3). Per-kind renderer binaries default
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

## Scene inspection *(draft, SR-0b/SR-0c/SR-1a/SR-1b)*

*(`docs/SR0.md` SR-0b/SR-0c and `docs/SR1.md` SR-1a/SR-1b: a one-shot,
daemon-supervised scene inventory inspector. Full wire format, stream caps,
and the `scene-inspection-v1` schema are `docs/REPORT_PROTOCOL_V1.md`,
frozen by SR-1a; this section covers only the `scene.inspect` RPC surface.
Names/the schema version and the known-key tables `inventory.rs` uses to
tell an unrecognized `scene.json` field from a recognized one (SR-2's typed
IR becomes the eventual authority) may still change pre-SR-1 freeze of the
RPC itself — see the breaking-change note below.)*

- `scene.inspect` `{"path": "/absolute/path/to/scene"}` → the inspector's
  validated `scene-inspection-v1` record verbatim. `path` must be a
  non-empty absolute path — a relative or empty path fails `invalid_params`
  before the daemon touches its inspector configuration.
- **SR-1b breaking change of the draft RPC surface** (allowed pre-SR-1
  freeze): the record's `"schema"` field is now `"scene-inspection-v1"`,
  not SR-0c's `"scene-feature-inventory-v0"` — any RPC consumer matching on
  the literal schema string must update. The record also gains
  `"capabilities_schema"` and a nullable `"backend"` field (see
  `docs/REPORT_PROTOCOL_V1.md`'s v0 → v1 mapping table). Every other field
  is unchanged.
- The daemon spawns `kwe-scene-inspector --input <path> --max-wall-ms <ms>
  --report-fd 3` under the same containment `renderer.start` gives every
  renderer worker: a private per-launch `HOME` (0700, removed on every exit
  path), `env_clear()` plus the shared `{HOME, PATH}` allowlist,
  `setpgid(0, 0)`, `PR_SET_PDEATHSIG` SIGKILL, a parent-pid check,
  `PR_SET_NO_NEW_PRIVS`, and the scene renderer kind's resource limits
  (never less contained than the renderer it stands in for). Unlike a
  renderer worker this is one bounded blocking call, not a supervised
  long-lived process: stdin is closed, stdout/stderr/the report FD are
  drained under a wall-clock deadline (default 10 s,
  `--inspector-wall-timeout-ms`), and the child is always reaped before the
  RPC answers.
- **The report itself now arrives over a dedicated report FD, not stdout**
  (`docs/REPORT_PROTOCOL_V1.md`, SR-1b): the daemon creates a pipe, dup2's
  the write end onto fd 3 in the child's `pre_exec` (before exec — see that
  document's "Report FD convention" section for the full ownership/closing
  rules), and reads exactly one `scene-inspection-v1` frame back through
  `kwe-report-protocol`'s `FrameReader`. stdout is still piped and drained
  (bounded, same cap as before) purely so a misbehaving or pre-SR-1b
  inspector binary cannot deadlock on a full pipe — its content is never
  parsed as the result anymore.
- `scene.inspect` is single-in-flight: the daemon runs at most one
  inspection at a time, on a dedicated thread, so it never blocks the
  single-threaded accept loop or any other RPC (`renderer.status`,
  `wallpaper.apply`, the pointer/audio relays) for the up-to-30-s duration
  a slow or hung inspection can take. Param validation (the `path` checks
  above) still answers inline and immediately. A `scene.inspect` that
  arrives while another is already running answers `inspector-busy`
  immediately instead of queuing or running a second inspector process;
  the gate clears as soon as the in-flight inspection's result is known,
  so the next call after that runs a real inspection again.
- Every non-success path answers a typed `{"outcome": "unknown", "reason":
  "..."}` result instead of an RPC-level error, so `scene.inspect` itself
  always succeeds (`"ok": true`) once its input validates — the record's own
  `outcome`/`reason` fields carry the result:
  - `inspector-unavailable`: no inspector binary configured, it failed to
    spawn, or the report pipe itself could not be created.
  - `inspector-busy`: another inspection is already in flight (see above).
  - `timeout`: the wall-clock deadline expired; the inspector's whole
    process group is SIGKILLed and reaped.
  - `report-oversize`: stdout OR the report FD exceeded its bound — the
    report FD's bound is `kwe-report-protocol`'s own stream caps
    (`MAX_TOTAL_PAYLOAD_BYTES` plus every frame's header bytes,
    `docs/REPORT_PROTOCOL_V1.md`); a report this large means the child
    misbehaved.
  - `inspector-failed`: a nonzero exit (report bytes ignored entirely in
    this case); carries a bounded (512-byte, lossy-UTF8) `stderr_tail`.
    This is also how an inspector binary that predates `--report-fd`
    resolves: it rejects the unknown flag with a clap usage error (exit 2,
    message on stderr), landing here with that usage error visible in
    `stderr_tail` (the version-skew window between an old inspector and a
    new daemon — daemon and inspector ship in the same package, so this is
    only a partial-upgrade window; SR-1d builds the fuller old/new matrix).
  - `report-missing` (SR-1b): the child exited 0 but its report stream
    contained zero `scene-inspection-v1` frames — either nothing at all, or
    only frames of a kind this daemon does not act on (an `Unknown` kind, or
    `scene-render-report-v1` before it has its own consumer).
  - `report-duplicate` (SR-1b): two or more `scene-inspection-v1` frames
    arrived in one stream — the codec itself does not adjudicate this
    (`docs/REPORT_PROTOCOL_V1.md`: "duplicate-kind policy ... is daemon
    policy, not codec"); this is that policy.
  - `report-malformed` (SR-1b): the report stream failed a codec-level check
    (bad magic/flags/reserved, a truncated frame, a stream-cap violation) or
    its `scene-inspection-v1` payload failed `validate_inspection` (wrong
    schema, a missing/wrong-typed field, or a digest mismatch); carries a
    bounded (256-byte) `detail` naming the specific failure plus the
    `stderr_tail`.
  - `parse-error` (SR-0c): the content hashed successfully but its
    `scene.json` (the file itself, or the package's `scene.json` entry) is
    not valid JSON, or — packages only — no `scene.json` entry could be
    located or read at all (`bounds.limits_hit` then also carries
    `pkg-no-scene-json`), or the entry exceeds the 16 MiB descriptor cap
    (`pkg-scene-json-oversize`).
  - On success, the record's own `outcome` is `inventoried` (hashed and
    inventoried successfully) or `incompatible` (unrecognized input, the
    input exceeded the inspector's own byte cap, or `parse-error` above).
    SR-0c (object-family only — see `docs/SR0.md`; materials are a
    follow-up slice) fills an `inventoried` record's `required`/`detected`/
    `unknown` from a bounded raw walk of `scene.json`'s `objects[]` array:
    `detected` names each capability found (`scene.layer.image`,
    `scene.layer.video`, `scene.layer.text`, `scene.particle`,
    `scene.layer.sound`, `scene.lighting`, `scene.effects`, plus
    `scene.package` for a package whose `scene.json` entry read
    successfully — even when its content then failed to parse) with a count
    and a bounded, sorted sample of logical object ids; `required` is the
    subset of those capabilities carried by at least one *active* object (no
    `visible` field, `visible: true`, or a property-bound `visible` value —
    WE's user-property convention, resolved later by SR-11) — except
    `scene.package`, which `required` always carries for a package whose
    entry read successfully: the container format is unconditionally
    required to render a pkg scene at all, independent of any object's
    visibility; `unknown` counts every root/object key and shape this pass
    does not recognize, never silently dropping one, with its own bounded
    sample list. Every list has a `truncated` flag and the walk itself is
    bounded (4096 objects, a wall-clock deadline checked periodically),
    both surfaced through `bounds.limits_hit` (`objects-cap`, `timeout`)
    exactly like the other caps above.
- No renderer worker state is touched by `scene.inspect`; it shares nothing
  with the active/candidate worker the rest of this document describes.

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
