# Alpha M6a — Workshop view and offline metadata cache

M6a ships the Steam-SDK-free half of the Workshop milestone: a Workshop
destination in the manager that uses exactly the same cards and details
page as the Installed gallery, and a bounded offline metadata cache so
subscriptions stay informative when Steam libraries are unmounted or the
daemon restarts.

The manager now has two destinations behind a Kirigami tab bar. Installed
shows every indexed item; Workshop shows only subscribed items —
installed, downloading, or awaiting download — with the same search,
type filter, sorting, favorites, playlist controls, and details pane,
plus a positive Installed badge. Selection state is shared, so switching
views never changes how details are presented. Subscription management
stays in Steam: the details page opens the canonical Workshop item in the
Steam client, and the Workshop view's empty state says so plainly. No
in-app subscribe is claimed until an optional Steam bridge is approved
separately.

The daemon persists a `workshop-metadata-v1.json` cache in its private
state directory: per-subscribed-item title, kind, tags, preview
availability, metadata hash, state, progress, and last-seen time. After
every scan the cache fills placeholder `subscribed_missing` entries,
restores items whose library vanished (marked with a
`workshop.offline_metadata` diagnostic), keeps live subscriptions from
ever aging out, and drops ids absent from every scan for 30 days — so a
canceled subscription fades deterministically. Corrupt cache files are
quarantined `.invalid-*` and never take the daemon down.

Run the recovery matrix with:

```sh
scripts/smoke-workshop-cache.sh
```
