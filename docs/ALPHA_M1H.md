# Alpha M1h — local Workshop state tracking

M1h reads Steam's local `appworkshop_431960.acf` manifests and joins that
subscription state with the existing defensive Workshop directory scan.

Each catalog item now reports one of these bounded states:

- `local` — local content exists but is not listed as subscribed;
- `subscribed_installed` — Steam lists the item and local files are present;
- `subscribed_missing` — Steam lists the item but the local project directory
  is unavailable.

Missing subscribed items remain visible as actionable catalog entries instead
of disappearing. The scanner is read-only: Steam still owns authentication,
subscription, and download operations. The manager can open the canonical item
in Steam, then rescan to observe the resulting local state.
