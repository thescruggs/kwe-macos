# ADR 0004: dynamically loaded thin Plasma display bridge

- Status: accepted for Alpha M1e
- Date: 2026-08-16

## Decision

Ship frame validation, copied-frame presentation, bounded display status/ack,
and passive normalized pointer forwarding as the dynamically loadable
`org.kde.kwe.display` Qt QML module. Compose those types from an original Plasma
6 `Plasma/Wallpaper` package rooted in `WallpaperItem`.

The package may observe and present daemon state but may not start, retry,
parse, render, browse, capture audio, schedule, or persist wallpaper state.
When control IPC fails, it keeps any private last-good frame, disables new
input, reports a text-and-icon degraded state, and retries status at a fixed
bounded cadence.

The Plasma-facing CPU fallback reads a complete selected slot with bounded
`pread` between two generation checks. It does not map the renderer-mutable file
into `plasmashell`; a worker truncation therefore produces a rejected short read
instead of a process-fatal mapped-file fault.

The first release installs a normal dynamic QML plugin beneath Qt's QML import
directory and the wallpaper package beneath Plasma's wallpaper data directory.
The standalone preview links the same backing target, so its frame and input
fault tests exercise the code that the wallpaper package imports.

## Rationale and provenance

KDE documents wallpaper plugins as QML packages and exposes `WallpaperItem`
through `org.kde.plasma.plasmoid`. Qt documents a separate backing library plus
small generated plugin as the recommended reusable-module structure. These are
public interface references; the implementation and package QML are original.
No upstream wallpaper source was copied or adapted.

Keeping the module small does not make its process trusted. Every path, byte
count, protocol field, timeout, and event queue remains bounded because a bug
in this module can still affect `plasmashell`.

## Consequences

The CPU-copy frame path remains the correctness baseline and costs memory
bandwidth. DMA-BUF optimization can be added later without changing the rule
that renderer and parser code stays external. Manager-owned package enabling,
per-output assignment, live Plasma PID survival tests, and safe-mode restoration
remain later gates; M1e intentionally performs only isolated staging and
offscreen execution.
