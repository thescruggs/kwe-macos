# Alpha M4c — normalized audio-frame protocol

M4c defines the bounded renderer-worker audio contract without opening an
audio device:

- Stereo frames support exactly 16, 32, or 64 normalized bands.
- Values are finite and constrained to `0..=1`.
- Sequence numbers are non-zero and frames use the existing bounded newline
  JSON transport.
- No raw PCM leaves the future audio worker, and PipeWire capture is not yet
  enabled.
