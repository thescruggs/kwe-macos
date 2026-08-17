# Provenance and third-party policy

We may learn from open-source projects, adapt compatible code, or execute a
separately installed renderer. Those are different acts and must be recorded
accurately.

## Required record

Add every material upstream influence to `THIRD_PARTY.yml` before merging code
that depends on it. Record:

- project name and canonical URL;
- exact commit/tag and upstream file paths;
- SPDX license and a link/copy of its notice;
- use type: `idea`, `protocol-compatible`, `adapted`, `copied`, `dependency`,
  or `separate-process-backend`;
- local files affected and a concise description of modifications;
- reviewer and date.

For `adapted` or `copied` code, place a nearby source comment such as:

```cpp
// SPDX-FileCopyrightText: upstream authors and KDE Wallpaper Engine contributors
// Borrowed-From: waywallen-display@<commit>:plugins/qt/<path> (MIT)
// Adaptation: validates frame metadata and adds the shared-memory fallback.
```

An idea-level influence belongs in the ADR or module documentation, not a
misleading copyright header. Never copy code and label it only as an idea.

## Initial upstream ledger

| Project | License observed | Intended use |
|---|---|---|
| KDE Plasma `WallpaperItem` and `Plasma/Wallpaper` package interface | GPL-2.0-or-later implementation; GFDL-1.3 documentation | Public interface and package-layout reference for the original thin M1e package; no implementation code copied or adapted. |
| `waywallen/waywallen-display` | MIT | Idea-level reference for the external-daemon boundary, future DMA-BUF synchronization, and thin Plasma surface. The current mmap wire format and implementation are original. |
| `waywallen/waywallen` | MIT | UX and daemon/plugin architecture reference; inspect exact commits before adapting code. |
| `jagrat7/linux-wallpaper-engine` | MIT | Gallery, playlist, compatibility, and Steamworks.js workflow reference. |
| `RainyPixel/wallpaper-engine-kde-plugin` and `catsout` upstream | GPL-2.0 | Compatibility and failure-mode reference only unless a deliberately GPL component is created. Do not paste it into permissively licensed core code. |
| `waywallen/open-wallpaper-engine` | GPL-2.0 | Behavior, format, and failure-mode reference only for the original renderer; no copied/adapted code. |
| `Almamu/linux-wallpaperengine` | GPL-3.0 | Behavior and format reference only; do not link or copy into permissive core code. |
| Valve Steamworks SDK / `ISteamUGC` | proprietary SDK terms | Optional isolated Steam bridge only after distribution review. |

License labels above are planning observations and must be rechecked at the
selected commit. Valve specifically warns that copyleft combinations with the
Steamworks SDK can be problematic; keeping the Steam bridge and GPL renderer
backends in separate processes/packages is an architectural boundary, not a
substitute for a release-time license review:
<https://partner.steamgames.com/doc/sdk/uploading/distributing_opensource>.

## Content policy

- Do not commit, upload, or redistribute Workshop items or Wallpaper Engine
  runtime assets.
- Tests use synthetic fixtures with original tiny media/assets.
- Local compatibility scans may refer to Workshop IDs and content hashes.
- Bug reports include metadata and minimized synthetic reproductions whenever
  possible, not copyrighted payloads.
