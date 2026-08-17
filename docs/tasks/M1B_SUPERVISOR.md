# M1b task contract: renderer supervision

## Goal and user-visible outcome

Move generated-renderer lifecycle ownership into `kwe-daemon`. A renderer that
hangs, corrupts its frame transport, exits, or ignores a graceful stop is
terminated and reaped without involving Plasma. Repeated equivalent failures
are persisted as a quarantine record, and the most recent validated frame is
stored as a static fallback.

## Scope

In scope:

- one bounded supervisor thread owned by `kwe-daemon`;
- versioned start, status, stop, and explicit retry control methods;
- startup and frame-progress deadlines;
- bounded automatic restart followed by content-identity quarantine;
- bounded graceful termination with forced-kill escalation;
- private runtime frame files and persistent JSON recovery state;
- a portable static last-known-good PPM image;
- synthetic fault injection available only behind an explicit daemon flag.

Out of scope:

- changes to renderer drawing, pacing, frame layout, or performance;
- DMA-BUF, systemd transient units, cgroup resource limits, or seccomp;
- production scene/video/web renderer selection;
- the Plasma wallpaper package or any live-desktop modification;
- user-facing recovery notifications.

## Files and modules

- `crates/kwe-daemon/src/supervisor.rs`
- `crates/kwe-daemon/src/main.rs`
- `crates/kwe-cli/src/main.rs`
- `crates/kwe-test-renderer/src/main.rs`
- `scripts/smoke-supervisor.sh`
- M1 architecture, alpha, and backlog documentation

## Acceptance and failure tests

- A healthy worker reaches `live`, advances frames, and creates a valid static
  fallback without restarting.
- Stop sends `SIGTERM`, waits for a bounded grace period, escalates to
  `SIGKILL` when necessary, and always reaps the child.
- A startup timeout, stale frame, invalid frame header, or abnormal exit is
  recorded with a stable failure reason.
- Three failures for the same wallpaper/content identity produce a persisted
  `quarantined` state and no fourth automatic launch.
- Explicit retry clears that identity's quarantine; changing the content hash
  naturally uses a separate failure budget.
- Commands, frame sizes, channel capacity, deadlines, restart count, paths,
  JSON state size, and fallback image size are bounded.
- The complete workspace format, lint, unit, build, QML, and fault-smoke checks
  pass. The live Plasma session is not touched.

## Protocol, compatibility, and recovery impact

The daemon API remains newline-delimited JSON version 1 and gains methods under
the `renderer.*` namespace. Frame protocol v1 is unchanged. This milestone is
recovery infrastructure and does not claim a new Wallpaper Engine capability
ID. The future display bridge may retain the daemon-provided frame path and
last-good still, but it must remain independently safe if the daemon exits.

## Provenance

The implementation is original. Existing upstream entries in
`THIRD_PARTY.yml` remain idea-level architecture and failure-mode references;
no upstream supervisor code is copied or adapted.

