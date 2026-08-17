# Alpha M4a — explicit wallpaper permissions

M4a adds the permission boundary for future web, input, and audio workers:

- Project metadata may request only the bounded `network`, `pointer`, and
  `audio` capabilities.
- Unknown permission names are ignored rather than granted.
- Requested permissions are preserved in the catalog and shown in the detail
  pane as not granted in this alpha.
- No worker or Plasma bridge consumes these permissions yet; this is an
  explicit declaration layer before sandboxed execution is enabled.
