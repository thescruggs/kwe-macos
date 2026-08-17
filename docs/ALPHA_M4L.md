# Alpha M4l — MPRIS capability probe

M4l adds `kwe media-status`, which enumerates `org.mpris.MediaPlayer2.*`
services through the user D-Bus session and returns bounded diagnostics. It
does not control playback, read metadata, or subscribe to media events yet.
