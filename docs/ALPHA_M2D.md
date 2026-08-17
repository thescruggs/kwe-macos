# Alpha M2d — supervised video preview

M2d adds a safe video-preview path for catalog items whose declared entry is a
local video file:

- The manager starts a dedicated `mpv` child process rather than embedding a
  decoder in the Qt process.
- `--no-config` prevents user configuration from changing the preview's safety
  boundary; `--hwdec=auto-safe` enables hardware decoding with mpv fallback.
- Paths must be local, regular, readable files and are passed after `--` to
  prevent option injection.
- The child can be stopped from the detail pane and unexpected exits are shown
  as a non-fatal warning.

This is preview-only. It does not apply a wallpaper or connect the process to
Plasma, and therefore remains safe to test before the renderer bridge exists.
