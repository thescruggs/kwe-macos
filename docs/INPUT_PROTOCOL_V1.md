# Normalized input protocol v1

## Boundary

Input travels from the thin display surface to `kwe-daemon` through the local
daemon API, then from the supervisor to only the promoted active renderer over
a private pipe. It is deliberately separate from frame protocol v1: frame
transport remains renderer-to-display, while input is display-to-renderer and
has different backpressure semantics.

The current slice implements only capability `runtime.pointer-position`.
Scrolling, touch, long press, drag, and keyboard input are absent.
*(BETA_M1a: the pointer `button` field passes through to the wire
`button_event`, and `audio_bands` / `media_state` gained a daemon-side
producer/forwarder — see below — while the renderer-side consumers arrive
with the video/web/scene binaries.)*

## Display-to-daemon request

The additive daemon method is `renderer.input`:

```json
{
  "generation": 4,
  "phase": "move",
  "x": 0.25,
  "y": 0.75
}
```

`phase` is `enter`, `move`, or `leave`. Coordinates must be finite and in the
closed interval `[0, 1]`, measured across the displayed wallpaper image after
letterboxing. Every request must name the exact non-zero promoted display
generation. This rejects delayed events after replacement and prevents a
candidate or retired worker from receiving active-desktop input.

## Daemon-to-renderer wire message

The supervisor quantizes each coordinate to an unsigned 16-bit integer using
nearest rounding and sends one compact JSON line:

```json
{"version":1,"type":"pointer_position","sequence":12,"phase":"move","x":16384,"y":49151}
```

Messages are capped at 256 bytes and written in one nonblocking `write(2)`.
Linux pipe atomicity therefore prevents partial message interleaving. If the
pipe is full, the supervisor retains only one latest pending event. A newer
event replaces it and increments `input_coalesced`; no unbounded motion queue
can form.

The renderer acknowledges observation on its reserved control stdout:

```json
{"version":1,"type":"input_ack","sequence":12}
```

The daemon reads at most 4096 acknowledgement bytes per supervisor tick, keeps
at most 1024 unterminated bytes, rejects unknown fields/types/versions, and
accepts only monotonic acknowledgements no newer than the latest sent event.
Renderer stderr is piped into a bounded daemon-side ring (64 lines / 16 KiB,
drained nonblocking) and surfaced in `renderer.status`; it is diagnostics only
and cannot enter this control stream.

## Audio and media control

*(BETA_M1a: daemon-side producers/forwarders on the same control pipe.)*

The supervisor writes the versioned `audio_bands` and `media_state` wire types
(see `docs/SUPERVISOR_API_V1.md` for the daemon API that feeds them) through
the same nonblocking, latest-wins path as pointer input — one pending frame
per stream, replaced rather than queued under backpressure, never parsed as
commands. `audio_bands` carries exactly 16, 32, or 64 `f32` bands per channel
(values finite, `0..=1`). Renderers that cannot handle a message type ignore
it at the framing boundary.

```json
{"version":1,"type":"audio_bands","sequence":4,"left":[0.1,0.1,0.1,0.1,0.1,0.1,0.1,0.1,0.1,0.1,0.1,0.1,0.1,0.1,0.1,0.1],"right":[0.1,0.1,0.1,0.1,0.1,0.1,0.1,0.1,0.1,0.1,0.1,0.1,0.1,0.1,0.1,0.1]}
{"version":1,"type":"media_state","sequence":4,"playback":"paused","title":"Track"}
```

The wire `sequence` carries the daemon's promoted display generation — NOT an
input-protocol monotonic sequence. Renderers must not validate these fields
for monotonicity or ordering (a later capture can legitimately carry a lower
`sequence` after a rollback), and must not reject an `audio_bands` /
`media_state` message whose `sequence` is lower than the previous one:
staleness is enforced daemon-side before writing, exactly like pointer input.

## Display behavior

The standalone Qt surface maps pointer positions only inside the actual image
destination. It emits enter/leave transitions at the letterbox boundary and
rate-limits moves to approximately 60 Hz. It explicitly accepts no mouse
buttons and no touch events. This is the reusable behavior contract for the
future Plasma package: right-click, long press, desktop icons, and edit-mode
gestures stay with Plasma.

## Failure behavior

- No active renderer or a mismatched generation rejects the API request.
- Invalid coordinates are rejected before a worker message is created.
- Input-pipe failure fails that request but never blocks frame supervision.
- Malformed or excessive renderer acknowledgement data is discarded with a
  bounded diagnostic counter.
- A renderer that stops advancing frames is still handled by the normal
  watchdog, rollback, and quarantine path.

This protocol and implementation are original; no upstream input protocol or
source code was copied or adapted.
