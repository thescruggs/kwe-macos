# Alpha M4b — web wallpaper preflight

M4b adds a non-executing web-wallpaper safety check:

- `kwe web-preflight --path <directory>` requires a regular `index.html`.
- HTML is bounded to 16 MiB and must contain an HTML root.
- Requested `pointer`, `audio`, and `network` permissions are reported, but
  network access remains disabled by default.
- No Chromium/CEF process is launched yet.

This establishes the input contract before a sandboxed browser worker is added.
