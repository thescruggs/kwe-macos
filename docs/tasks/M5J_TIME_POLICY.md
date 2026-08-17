# M5j task contract: bounded time/day policy windows

## Goal and user-visible outcome

Playback policies can express deterministic local time/day windows, including
windows that cross midnight, without letting the policy core read wall-clock
state itself.

## Scope

In scope:

- seven-bit Monday-first day masks;
- start-inclusive/end-exclusive local-minute windows;
- cross-midnight matching against the start day;
- optional caller-provided weekday/minute snapshot fields;
- validation and boundary tests.

Out of scope:

- timezone lookup, daylight-saving conversion, or system clock access;
- calendar/holiday rules;
- desktop signal adapters, persistence, UI, or renderer execution.

## Files and modules

- `crates/kwe-core/src/policy.rs`
- M5 project, compatibility, and alpha documentation

## Acceptance and failure criteria

- Weekdays are 0–6 (Monday–Sunday), minutes are 0–1,439, and day masks use no
  bits outside the low seven.
- Empty masks and equal start/end values are rejected as ambiguous.
- Cross-midnight windows match late time on the selected start day and early
  time on the following day.
- Missing local-clock fields make a time rule inactive, not permissive.
- Invalid caller snapshots fail closed before any decision is returned.

## Protocol, compatibility, and recovery impact

No protocol or process change. This advances `playlist.rules`; system time is a
future versioned adapter input and clock changes cannot enter this pure core
except through an explicit snapshot.

## Provenance

Original implementation with no new dependencies or upstream source use.
