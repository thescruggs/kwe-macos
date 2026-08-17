# M5i task contract: bounded playback policy resolver

## Goal and user-visible outcome

Define one deterministic policy contract for fullscreen, maximized, session
lock, idle, battery, power-saver, and focused-application signals. Conflicting
rules resolve to a clear keep-running, mute, pause, or stop/free-memory action
without directly controlling a renderer.

## Scope

In scope:

- serializable policy snapshot, trigger, rule, and action types;
- bounded rule count and string sizes;
- exact, case-normalized desktop-application identity matching;
- deterministic safety precedence: stop, pause, mute, keep running;
- explicit matched-rule IDs for diagnostics;
- validation and unit tests for malformed, conflicting, and inactive rules.

Out of scope:

- KWin, logind, UPower, PowerProfiles, MPRIS, or D-Bus subscriptions;
- time/day schedules and display profiles;
- applying an action to a renderer;
- persistence or manager controls;
- live Plasma changes.

## Files and modules

- `crates/kwe-core/src/policy.rs`
- `crates/kwe-core/src/lib.rs`
- M5 project, compatibility, and alpha documentation

## Acceptance and failure criteria

- At most 128 rules are accepted; IDs and application identities are bounded.
- Unknown JSON enum values fail deserialization rather than defaulting to a
  permissive action.
- Multiple active rules always resolve with documented safety precedence and
  bounded matched-rule diagnostics.
- Application matching is exact after ASCII lowercase normalization; empty or
  overlong identities are rejected.
- An inactive or empty rule set returns the configured bounded default.
- Tests do not connect to a session service or control a renderer.

## Protocol, compatibility, and recovery impact

No service protocol changes. This advances the four P0 playback action IDs and
the P1 conditions model, but does not claim integration until signal adapters,
persistence, and supervised action execution exist.

## Provenance

Original Rust implementation with existing serde support. No dependency or
upstream-source change.
