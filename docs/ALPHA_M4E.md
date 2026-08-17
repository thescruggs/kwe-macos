# Alpha M4e — PipeWire capability probe

M4e adds `kwe audio-status`, a bounded environment probe that asks `pw-cli`
whether the user session exposes a PipeWire control socket. It reports only
availability, server version when present, and a bounded diagnostic. It does
not open a capture stream, request audio permissions, or collect user audio.
