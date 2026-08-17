# M1c task contract: transactional canary and display handoff

## Goal and user-visible outcome

Applying a candidate must never stop a healthy active renderer before the
candidate proves it can produce stable frames. A failed candidate is retried
within its own bounded budget and then quarantined while the active renderer
and its frame path remain unchanged. A successful candidate is promoted with a
monotonic display generation and a bounded acknowledgement window during which
the previous frame mapping remains available.

## Scope

In scope:

- at most one active, one candidate, and one briefly retired worker;
- a bounded canary interval and minimum advancing-frame requirement;
- candidate-only restart and quarantine accounting;
- active-preserving rollback on candidate startup, frame, protocol, or exit
  failure;
- atomic in-daemon promotion and static fallback persistence;
- `renderer.ack` for display-generation acknowledgement;
- bounded previous-worker handoff lifetime when acknowledgement never arrives;
- status fields that distinguish active, candidate, and previous display state;
- synthetic end-to-end tests proving PID and frame-path continuity.

Out of scope:

- changing renderer drawing, frame pacing, or mmap protocol v1;
- transitions, cross-fades, DMA-BUF, or performance optimization;
- systemd/cgroup/seccomp resource enforcement;
- installing or loading a Plasma wallpaper package;
- multi-output transaction coordination.

## Acceptance and failure criteria

- First start becomes active only after the configured canary interval and at
  least three advancing frames.
- Starting a candidate leaves the active PID, frame file, and display
  generation unchanged throughout canary.
- A bad candidate reaches quarantine after the bounded failure count while the
  original active worker continues advancing.
- Explicit retry can promote the identity after it becomes healthy.
- Promotion increments the display generation, reports the prior frame path,
  and retains the prior process until the matching `renderer.ack` or bounded
  handoff timeout.
- Stale/future acknowledgements are rejected and cannot stop any worker.
- Stop and daemon shutdown terminate and reap active, candidate, and retired
  process groups.
- No operation touches the live Plasma session.

## Protocol, compatibility, and provenance

The newline-delimited daemon protocol remains version 1 with additive fields
and the additive `renderer.ack` method. Frame protocol v1 is unchanged. No new
Wallpaper Engine capability ID is claimed. The implementation is original;
upstream projects remain idea-level process-boundary references only.

