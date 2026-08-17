# Alpha 0.1 developer guide

Alpha 0.1 is a safe library-browser vertical slice. It discovers installed
Wallpaper Engine Workshop projects, exposes them through a bounded local API,
shows them in a native Kirigami gallery, and validates the Vulkan device path.
It does **not** install a Plasma wallpaper package or apply a wallpaper.

## What works

- Steam library discovery through `libraryfolders.vdf`, including the current
  `/media/crushinator` library;
- defensive indexing of scene, video, web, unknown, missing, and malformed
  projects without loading wallpaper code;
- local previews, title/ID search, type filtering, explicit compatibility
  status, loading/empty/service-error states, and keyboard-focusable cards;
- a versioned newline-delimited JSON API over a mode-`0600` Unix socket;
- Vulkan loader/device/queue and DMA-BUF extension preflight, including logical
  device creation in an isolated process;
- synthetic tests for nested VDF, path traversal, malformed/missing metadata,
  packed scenes, size limits, and API framing.

“Apply” is intentionally disabled. Rendering inside `plasmashell` would defeat
the recovery architecture, while an unvalidated frame bridge would make a bad
wallpaper capable of taking down the desktop.

## Build and run

Requirements on Arch/CachyOS: Rust stable, CMake, Ninja, Qt 6 Core/Gui/QML/
Quick/Network, Kirigami 6, a Vulkan loader, and a Vulkan driver.

```sh
cargo build --workspace
cmake -S . -B build/cmake -G Ninja -DCMAKE_BUILD_TYPE=Debug
cmake --build build/cmake --parallel
./scripts/dev-run.sh
```

The script starts only the development user daemon and gallery. It does not
modify Plasma. To use separate terminals:

```sh
target/debug/kwe-daemon
build/cmake/apps/kwe-manager/kwe-manager
```

Useful diagnostics:

```sh
target/debug/kwe diagnose
target/debug/kwe-vulkan --json
target/debug/kwe scan --output build/catalog.json
```

Run the full development check, or opt into its offscreen daemon/UI round trip:

```sh
./scripts/check.sh
KWE_RUN_UI_SMOKE=1 ./scripts/check.sh
```

## Current-machine validation

On 2026-08-16 the indexer loaded all 92 installed projects: 60 scenes, 20
videos, 9 web projects, and 3 unknown types. All 92 had usable previews and no
item-level parser diagnostics. The offscreen manager then loaded that catalog
through the v1 daemon API successfully. A native KDE dark-style visual snapshot
was inspected for card layout, preview loading, compatibility icon/text,
empty-detail guidance, and the 92-item count.

The native Vulkan worker enumerated the NVIDIA GeForce RTX 3070 and llvmpipe,
created a logical graphics device on both, and found external-memory FD,
DMA-BUF external memory, and external-semaphore FD support on both. Intel UHD
630 remains absent from the Vulkan loader and is recorded as a system
diagnostic rather than assumed available.

## Known alpha limits

- no Steam Workshop browsing or subscription yet; installed content only;
- no video, scene, or web playback yet;
- no Plasma frame bridge, playlists, properties, pointer input, or audio
  response yet;
- no persistent SQLite compatibility history yet;
- metadata hashes cover `project.json`, not the complete Workshop payload;
- the manager uses the Alpha v1 socket API, which is intentionally small and
  will be replaced by the versioned D-Bus control API before 1.0.

## Recovery and cleanup

Closing the gallery does not alter the desktop. Stop `kwe-daemon` to stop the
development service. Its socket is removed on a normal exit; a stale socket is
replaced only when it is actually a Unix socket. Build products stay under
`target/` and `build/`.
