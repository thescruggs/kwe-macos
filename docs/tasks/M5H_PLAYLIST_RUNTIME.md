# M5h task contract: monotonic playlist runtime

## Goal and user-visible outcome

Provide the daemon-facing runtime contract that turns M5g playlist settings
into deterministic, pause-aware decisions with bounded history. Temporary
missing/quarantined content produces an explicit degraded outcome and can
recover when availability changes; the runtime never spins or launches a
renderer itself.

## Scope

In scope:

- caller-supplied monotonic millisecond time with regression rejection;
- start, wait, advance, pause, resume, exhausted, and no-eligible decisions;
- duration deadlines with checked/saturating arithmetic;
- bounded history sufficient for every allowed playlist entry;
- non-repeat shuffle exhaustion without replay;
- immediate advancement when the current item becomes unavailable;
- recovery when a previously unavailable unplayed item returns;
- deterministic Rust unit tests for every state and boundary.

Out of scope:

- wall-clock schedules and desktop/environment policy inputs;
- renderer start/stop/apply, display assignment, or transition rendering;
- persistence across daemon restart;
- manager UI changes;
- live Plasma modification.

## Files and modules

- `crates/kwe-core/src/playlist_runtime.rs`
- `crates/kwe-core/src/lib.rs`
- M5 project, compatibility, and alpha documentation

## Acceptance and failure criteria

- Every operation is bounded by the 1,024-entry playlist limit.
- Time regression is rejected without changing the current selection.
- Pause freezes the remaining duration; resume establishes a new bounded
  deadline from that remaining duration.
- Repeat-off shuffle plays every eligible item at most once and reports
  exhausted.
- A current item newly marked unavailable is not returned as waiting.
- With no eligible items, the runtime returns an explicit no-eligible/exhausted
  decision and can reconsider on a later tick.
- No renderer, daemon socket, filesystem, or Plasma state is touched.

## Protocol, compatibility, and recovery impact

No protocol change. This is an internal prerequisite for `playlist.timer`,
`playlist.ordered-shuffle`, and pause-aware playback policy. The explicit
decision enum is the future service boundary and prevents ambiguous retries.

## Provenance

Original implementation using Rust standard-library collections only. No new
dependencies or upstream source use.
