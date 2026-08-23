# Bug: the gallery shifts wallpapers back and forth every few seconds

- **Reported:** 2026-08-22 (user, while preparing to test the `-5` package:
  "there is a noticable refresh that shifts the wallpapers on the selection
  screen back and forth every few seconds")
- **Severity:** Medium user-facing — nothing breaks, but the selection grid
  is visibly unstable: cards move under the pointer, which makes clicking
  the wallpaper you meant to click a matter of timing.
- **Status:** FIXED 2026-08-23 in two parts — (2) the background auto-refresh now runs SILENTLY (`CatalogClient::beginSilent`: the state stays Ready, so the gallery sections bound to `catalogClient.state` no longer hide/show every 5 s; the maintainer still saw the shift after part 1 because this state flip, not only the model reset, moved the layout), and (1) the smallest fix from the list below:
  `CatalogModel::replaceFromCatalog` keeps the last applied `items` array and
  returns before `beginResetModel` when the refreshed payload is identical,
  so the 5 s auto-refresh no longer rebuilds the grid unless the catalog
  actually changed. Confirmed by the maintainer's report ("every couple of
  seconds it refreshes") matching the 5 s cadence; the targeted-signal diff
  and the cadence revisit remain optional follow-ups.

## Suspected cause (to confirm)

`CatalogClient` runs an unconditional 5-second auto-refresh:

- `apps/kwe-manager/src/catalogclient.cpp:10` —
  `constexpr int AutomaticRefreshMilliseconds = 5000;`
- `catalogclient.cpp:31-37` — a coarse `QTimer` at that interval calls
  `refresh()` whenever the client is not already `Loading`, from
  construction onward, whether or not anything changed on disk.

Every reply then goes through `CatalogModel`'s only update path, which is a
**full model reset** — `catalogmodel.cpp:119-128`: `beginResetModel()`,
replace the whole item vector, `endResetModel()`. A reset invalidates every
delegate and every index, so the view rebuilds its delegates and
re-evaluates its content position. That is the classic source of exactly
this symptom: a grid that twitches, re-lays out, or slides back to a
recomputed position on a fixed cadence.

"Back and forth" specifically suggests the view is settling to a different
`contentY` after the rebuild and then being nudged back — worth capturing
before and after values rather than guessing.

## What to measure first

1. Confirm the cadence matches: the shift interval should be ~5 s and should
   change if `AutomaticRefreshMilliseconds` is changed. If it does not, the
   timer is not the cause and this doc is wrong.
2. Log `contentY` / `currentIndex` around `endResetModel()` to see whether
   position is lost, restored, or oscillating.
3. Check whether the refresh reply is even different from the last one. If
   the catalog bytes are identical, the reset is pure waste.

## Fix directions (decide at fix time)

1. **Do not reset on an unchanged catalog.** Compare the incoming payload
   (or a hash of it) with the last applied one and return early when equal.
   This alone would stop the twitch in the common idle case, and it is the
   smallest change.
2. **Stop resetting the whole model for a changed catalog.** Diff the item
   list by stable identity and emit targeted insert/remove/`dataChanged`
   signals, so unaffected delegates and the scroll position survive.
   Larger, and the right long-term shape.
3. **Reconsider the cadence.** A 5 s poll of the whole catalog exists to
   notice Workshop installs/downloads; a longer interval, or pausing it
   while the user is interacting (or while a detail pane is open), costs
   nothing in practice.
4. Whatever lands, keep the explicit refresh action and the error-path
   backoff (`setState`, `catalogclient.cpp:157-165`) working as they do now.

## Notes

- The auto-refresh is deliberate and predates this report; the defect is
  what the refresh does to the view, not that it exists.
- Adjacent, not the same thing: `WallpaperDetail.qml:109` runs its own
  permission refresh timer. Rule it out during step 1 rather than assuming.
