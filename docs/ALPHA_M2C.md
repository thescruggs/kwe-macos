# Alpha M2c — native metadata inspection

M2c expands the detail pane with safe, read-only project metadata:

- Scanner tags are carried through the catalog model without executing or
  loading wallpaper content.
- The detail pane presents tags alongside the Workshop ID, type, preview, and
  compatibility explanation.
- Preview loading remains asynchronous and local-file based, with the existing
  placeholder fallback for missing previews.

Applying and renderer execution remain intentionally disabled until the
isolated display bridge and supervised playback worker are ready.
