# User experience design

## Product standard

The application should feel native to Plasma rather than like a Windows or web
application transplanted onto KDE. Follow the KDE Human Interface Guidelines,
Kirigami conventions, Breeze styling, the active color scheme, the system font,
the user's icon theme, localization, accessibility settings, and reduced-motion
preference.

KDE's HIG summarizes the desired balance as simple by default and powerful when
needed: <https://develop.kde.org/hig/>. Kirigami supplies responsive pages,
cards, actions, form layouts, and settings components:
<https://develop.kde.org/docs/getting-started/kirigami/>.

Do not visually clone Wallpaper Engine. Preserve familiar concepts and
workflows while expressing them with native KDE controls and terminology.

## Information architecture

Primary destinations:

1. **Library** — installed Workshop and local wallpapers.
2. **Workshop** — browse, filter, subscribe, and monitor downloads.
3. **Playlists** — create, reorder, schedule, and assign collections.
4. **Displays** — per-output wallpaper, span/clone/group, and saved profiles.
5. **Activity** — downloads, renderer events, compatibility tests, and recent
   automatic recoveries.
6. **Settings** — playback, quality, audio/input permissions, storage,
   backends, shortcuts, and diagnostics.

Diagnostics should be easy to reach but should not dominate healthy everyday
use. Safe mode must be available from both the UI and CLI.

## Core workflows

### First run

1. Welcome and explain ownership/content boundaries in one short page.
2. Auto-detect Steam and all libraries; let the user correct paths.
3. Run GPU, renderer, PipeWire, and Plasma integration checks.
4. Install/enable the thin Plasma package only with explicit confirmation.
5. Render a synthetic canary, verify fallback recovery, then show the Library.

Each check shows `Ready`, `Limited`, or `Action needed`, with a short reason and
one primary action. Advanced logs stay collapsed.

### Library and Workshop

- Responsive thumbnail grid on wide windows; compact list alternative.
- Persistent search with chips for type, installed/subscribed, compatibility,
  audio, interaction, resolution/aspect, tags, and backend.
- Stable sorting and selection when metadata or downloads update.
- Cards show title, preview, type, installation state, and a text/icon
  compatibility badge. Hover is supplementary; every action works by keyboard.
- Installed and Workshop items use the same details page and property editor.
- Subscription and downloads expose progress, pause/cancel when supported, and
  an offline state without blocking access to installed content.

### Wallpaper details

Use a responsive master/detail layout:

- large preview with play/pause, mute, interaction test, and safe canary action;
- title, author, Workshop link, type, tags, content/update state;
- primary **Apply** action with display/profile target next to it;
- generated native editors for user properties, with Reset, Save preset, and
  unsaved-change indication;
- clear compatibility summary followed by expandable feature-level details;
- quarantine/recovery history with **Test again** and **Export report** actions.

Applying an untested or partially compatible wallpaper invokes a canary, not a
frightening generic confirmation dialog. Only permissions or known material
risk require confirmation.

### Playlists and displays

- Playlist editing supports drag-and-drop plus keyboard Move up/down actions.
- Show total duration, transition, shuffle/repeat behavior, and unavailable or
  quarantined entries before save.
- Display overview mirrors the physical layout reported by Plasma and identifies
  outputs by stable connector/EDID-derived identity, friendly name, scale, and
  orientation.
- Saved profiles are snapshots of per-display wallpaper/playlist assignments.
- Hotplug does not silently overwrite a saved profile; show the proposed mapping.

### Recovery

When a wallpaper fails, keep the desktop usable and show a passive notification:

`“Seascape was stopped after the renderer failed. The previous wallpaper was restored.”`

Offer **Details** and **Test again**; never show raw signals or stack traces as
the primary message. Repeated failures add a `Quarantined` badge and explain
what will permit another automatic test (content, renderer, driver, or settings
change).

## Compatibility language

Use consistent states everywhere:

- **Compatible** — exercised automatically on this backend/hardware.
- **Expected to work** — required features are implemented but this exact item
  has not completed a canary here.
- **Partial** — usable with named missing or altered behavior.
- **Incompatible** — a required capability is unavailable.
- **Quarantined** — this content hash repeatedly failed on this system.
- **Unknown** — insufficient metadata or no matching capability evidence.

These states require an icon and text, never color alone. A details popover
shows backend, last test, hardware context, and missing capability IDs.

## Native KDE integration

- Use `KConfig`-compatible settings exposure where practical and KDE standard
  shortcuts/actions.
- Integrate notifications through the desktop notification service and open the
  relevant Activity/details page from the notification.
- Expose playback and active wallpaper state over D-Bus for KRunner, scripts,
  and future Plasma widgets.
- Use portals or KDE file dialogs for user-selected files and directories.
- Respect power profiles, session lock, idle state, color scheme, locale,
  fractional scale, output rotation, and reduced motion.
- Never hide or replace Plasma desktop icons; the wallpaper stays beneath the
  containment.

## Accessibility and quality gates

- Full operation with keyboard only; predictable focus order and visible focus.
- Accessible names/descriptions for icon-only actions, previews, badges, and
  progress indicators.
- Screen-reader announcements for download completion, apply/rollback, and
  quarantine without announcing every frame or progress tick.
- No information encoded only by color, animation, hover, or sound.
- Test Breeze Light/Dark, high contrast, custom accent colors, 100/125/150/200/
  250% scale, long translations, RTL layout, touch, and reduced motion.
- Avoid modal-dialog chains. Preserve user selection and scroll position after
  updates, errors, navigation, and window resize.
- UI remains responsive while scanning the 92-item local baseline and a
  10,000-item synthetic catalog; image decode and metadata work never block the
  GUI thread.
- Visual regression tests cover the six primary destinations and every standard
  state: loading, empty, offline, degraded, permission-required, and failure.

