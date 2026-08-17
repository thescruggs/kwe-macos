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

## Start and retry

`renderer.start` accepts:

```json
{
  "wallpaper_id": "synthetic-canary",
  "content_hash": "sha256-placeholder",
  "width": 960,
  "height": 540,
  "fps": 30
}
```

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

## Status

The result contains:

```json
{
  "phase": "live",
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
  }
}
```

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

Status exposes `input_sequence`, `input_ack_sequence`, `input_pending`,
`input_coalesced`, `input_protocol_errors`, `pointer_inside`, `pointer_x`, and
`pointer_y`. Sequence numbers report accepted and renderer-observed events;
coordinates are the quantized unsigned 16-bit values. See
`docs/INPUT_PROTOCOL_V1.md`.

The status path is diagnostic, not a heartbeat from the display client. The
daemon watches frame progression even when no client is connected.

## Lifecycle and recovery

The daemon starts each worker in a new process group with `no_new_privs` and a
Linux parent-death signal. It opens only its configured renderer executable,
passes bounded arguments directly, clears the inherited environment, and
discards worker standard streams in this alpha to prevent pipe or log growth.
Before exec it also applies finite address-space, output-file, descriptor, and
UID-scoped process ceilings and disables core dumps. Any failure to install the
policy fails the launch.

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
private state directory. The latest acknowledged stable BGRA frame is converted to a bounded
P6 PPM still and atomically stored in alternating `last-good-a.ppm` and
`last-good-b.ppm` slots. The JSON state pointer changes only after the inactive
slot is complete, so the previously acknowledged still survives an interrupted
commit. These static files remain usable across daemon restart and do not
depend on the live mmap.

## Current limits

- The generated renderer is the only production-shaped worker exercised here.
- The packaged systemd unit bounds aggregate memory, swap, CPU, and task usage;
  GPU-specific budgets and a stable seccomp allowlist remain later hardening
  work.
- The M1e Plasma display bridge consumes this local JSON contract in isolated
  staging/offscreen tests but is not installed or enabled on the live desktop;
  no D-Bus interface is exposed yet.
- The alpha status/ack exchange still uses the local JSON socket; a future
  stable display contract may move this control path to D-Bus.
