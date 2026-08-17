# Alpha M5i — playback policy resolver

M5i defines the bounded, side-effect-free policy contract for fullscreen,
maximized, session lock, idle, battery, power-saver, and focused-application
signals. Rules select one of keep running, mute, pause, or stop/free-memory.

When several active rules conflict, the safety precedence is stop, pause, mute,
then keep running. The configured default applies only when no rule matches.
Decisions include up to 128 matched rule IDs for bounded diagnostics. Rule IDs
and desktop application identities are validated, and application matching is
exact after ASCII case normalization.

This slice does not subscribe to desktop services or apply the decision to a
renderer. Those adapters remain recovery-gated work. Unit tests advance
`playback.keep-running`, `playback.mute`, `playback.pause`, `playback.stop`, and
`playback.conditions` without claiming end-to-end parity.
