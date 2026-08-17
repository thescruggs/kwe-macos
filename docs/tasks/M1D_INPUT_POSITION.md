# M1d-B task contract: normalized pointer position

## Goal and user-visible outcome

An interactive wallpaper can receive scale-independent pointer position while
the display surface remains a thin client and Plasma retains every button,
touch, context-menu, icon-selection, and edit-mode gesture. Stale display
clients must not be able to route events to a replacement renderer.

## Scope

In scope:

- capability `runtime.pointer-position` with normalized coordinates;
- passive enter, move, and leave observation in the standalone Qt display
  harness;
- generation-bound `renderer.input` requests on daemon protocol v1;
- a bounded, versioned, newline-delimited daemon-to-worker input channel;
- nonblocking writes with a single latest-event pending slot;
- bounded renderer acknowledgements and input diagnostics;
- synthetic end-to-end tests for accepted input, worker observation, stale
  generation rejection, malformed coordinates, and active-only routing;
- reusable display-surface behavior suitable for the later Plasma package;
- documentation and provenance updates.

Out of scope:

- mouse buttons, clicks, scrolling, touch, long press, drag, keyboard, or
  tablet pressure;
- enabling interaction without the user's eventual per-wallpaper permission;
- changing frame protocol v1 or renderer performance;
- installing or loading a Plasma wallpaper package;
- Wallpaper Engine callback parity beyond pointer position.

## Acceptance and failure criteria

- Coordinates are finite, normalized to `[0, 1]`, quantized deterministically
  to 16 bits, and bounded to one protocol message of at most 256 bytes.
- Every request includes the current display generation. Missing, zero, stale,
  or future generations are rejected without writing to any worker.
- Candidates and retired workers never receive input; only the promoted active
  worker may receive it.
- The worker pipe is nonblocking. Backpressure replaces at most one pending
  event and cannot stall the daemon, renderer watchdog, or display client.
- Renderer acknowledgement parsing is bounded and malformed output cannot grow
  daemon memory or crash the supervisor.
- The Qt surface accepts no mouse buttons and observes only hover position, so
  it cannot consume Plasma-reserved clicks or long presses.
- The isolated integration matrix proves that the generated renderer observes
  a valid event and that stale/malformed requests are refused.
- No operation touches the live Plasma session.

## Accessibility, compatibility, and provenance

Pointer interaction is supplementary; every manager action remains available
to keyboard and assistive technology. This implements only
`runtime.pointer-position`; `runtime.pointer-buttons` remains planned P1 and
must use a separate explicit interaction-mode contract.

The input wire format and implementation are original. Idea-level upstream
references remain recorded in `THIRD_PARTY.yml`; no code or protocol is copied
or adapted.
