# M6a task contract: Workshop view and offline metadata cache

## Goal and user-visible outcome

The manager gains a Workshop destination that lists subscribed items with
the same cards and details page as the Installed gallery, and subscribed
items keep their titles, tags, and kind across daemon restarts and
unmounted Steam libraries. Subscription management itself remains
honestly Steam-managed until the ISteamUGC bridge decision.

## Scope

In scope:

- `kwe-daemon`: bounded `workshop-metadata-v1.json` cache in `state_dir`
  (atomic-write pattern, 0600): per-subscribed-item title/kind/tags/
  preview availability/metadata hash/state/progress/last-seen. After
  every scan the cache fills placeholder `subscribed_missing` entries,
  synthesizes items for subscriptions whose library vanished
  (`workshop.offline_metadata` diagnostic), touches live subscriptions so
  they never age out, and drops ids absent from every scan for 30 days.
  Corrupt/oversized cache files are quarantined `.invalid-*`; the daemon
  keeps serving.
- Manager: `WallpaperCard`, `WallpaperDetail`, and a `WallpaperSelection`
  singleton extracted from `Main.qml`; a reusable `GalleryPage`; a
  Kirigami NavigationTabBar with Installed and Workshop destinations; a
  second `WallpaperFilterModel` in `workshopView` mode (subscribed items
  only) with a positive Installed badge; identical search/type/sort/
  favorites/playlist/rescan/safe-mode surface, keyboard navigation,
  accessibility names, and placeholder/error states on both pages.
- Smoke suite `scripts/smoke-workshop-cache.sh` covering snapshot,
  unmount recovery, restart persistence, remount recovery, and
  corrupt-cache quarantine.

Out of scope:

- ISteamUGC bridge, paginated remote Workshop query, in-app subscribe/
  unsubscribe (all remain Steam-managed; the "Open in Steam Workshop"
  affordance is the bridge);
- downloading real Steam payloads from the cache;
- live Plasma modification;
- the M2 SQLite compatibility database.

## Files and modules

- `crates/kwe-daemon/src/workshop_cache.rs` (new), `persist.rs`,
  `playlist_session.rs`, `main.rs`
- `apps/kwe-manager/qml/{Main,GalleryPage,WallpaperCard,WallpaperDetail,
  WallpaperSelection}.qml`
- `apps/kwe-manager/src/{catalogmodel.h,catalogmodel.cpp,main.cpp,
  CMakeLists.txt}`
- `scripts/smoke-workshop-cache.sh` (new), `scripts/check.sh`
- M6 project, compatibility, and alpha documentation

## Acceptance and failure criteria

- After unmounting the Steam library and rescanning, previously
  subscribed items remain visible as `subscribed_missing` with their
  cached title/tags/kind and the `workshop.offline_metadata` diagnostic.
- The restored metadata survives a daemon restart; remounting the library
  returns items to `subscribed_installed` with on-disk metadata.
- An unsubscribed id fades from the cache after 30 days without appearing
  in any scan; a live subscription never ages out while missing.
- A corrupt cache file is quarantined `.invalid-*` and the daemon keeps
  serving `health`, `catalog`, and `renderer.*`.
- Both destinations pass qmllint, the offscreen UI smoke, and keyboard/
  accessibility parity review.

## Protocol, compatibility, and recovery impact

No protocol change; the catalog JSON carries cached metadata through the
existing `catalog` method. `workshop.browse` advances to
`partial` (local subscription visibility + Steam-client fallback; no
remote pagination) and `library.metadata` to `partial` (offline metadata
cache with Steam canonical links via the open-in-Steam affordance).

## Provenance

Original implementation with no new dependencies or upstream source use.
