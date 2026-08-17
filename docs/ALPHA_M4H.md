# Alpha M4h — web sandbox command boundary

M4h adds a tested Bubblewrap/Chromium command builder for future web previews:

- Wallpaper content is mounted read-only at `/wallpaper`.
- Home-directory access is not mounted.
- A private temporary directory is provided for browser state.
- Network isolation is enabled unless a future policy explicitly allows it.
- The command is printed for inspection; the manager does not launch it yet.
