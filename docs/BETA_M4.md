# Beta M4 live wallpaper apply and safe-mode restore

M4 gives the daemon a live-apply transaction: the client names an output and
a wallpaper id, and the daemon validates against the catalog, starts the
renderer through the existing supervisor, waits (bounded) for promotion to a
live phase, persists the assignment in a bounded state store, and switches
the Plasma desktop's wallpaper plugin via the KDE wallpaper scripting API —
never through a shell, never embedding wallpaper content in the script.
Every switch is reversible: `wallpaper.restore` reverts the saved previous
plugin/config-group/image, or falls back to the stock `org.kde.image` plugin
with a stock image that exists on this system. **M4a is the daemon-side slice
of BETA_M4 in the beta plan. Out of scope here:** the Manager UI (M4b),
playlist assignment (M4c), and live enablement plus the live smoke flip
(M4d) — smoke-apply stays gated behind `KWE_LIVE_APPLY=1` and never switches
a live wallpaper in M4a.

## Research findings (M4a, read-only live probes)

Recorded verbatim on the reference system (CachyOS, Plasma 6.7.4 on Wayland,
NVIDIA, user qcv123). All probes were read-only `evaluateScript` calls plus
config-file reads; nothing was changed in the live Plasma config in M4a.

- **Session type:** Wayland (`XDG_SESSION_TYPE=wayland`). Plasma 6 has no
  X11-specific wallpaper paths; the scripting surface is the same.
- **Shell and bus names:** the qdbus binary on this system is `qdbus6`
  (there is no `qdbus`); the daemon resolves `qdbus` then `qdbus6` from
  `PATH` at call time. The Plasma shell registers the D-Bus service
  **`org.kde.plasmashell`** (all lowercase) at `/PlasmaShell` — the Plasma 5
  era `org.kde.PlasmaShell` alias no longer exists, and the daemon's
  `--plasma-shell-service` default matches the real name.
- **`evaluateScript` returns only the `print()`/`printError()` buffer.**
  A read-back probe must end in `print(JSON.stringify(...))`; the reply the
  daemon parses is exactly that JSON. The switch scripts need no read-back
  and deliberately print nothing.
- **Plasma 6 desktop scripting has no `screens()`/`outputs()`.** The
  available globals are `desktops()`, `desktopById`, `screenForConnector(name)`
  (returns the screen id, `-1` for an unknown connector), and
  `knownWallpaperPlugins`.
- **`desktopForScreen(-1)` SIGSEGVs plasmashell** (verified on 6.7.4: the
  shell crashed, pid 1871). The daemon never uses it; the probe template and
  the output mapping rely on `desktops()` indices and `screen` values only.
- **Per-desktop scripting surface:** `wallpaperPlugin` (get/set),
  `currentConfigGroup` (get/set, array form such as
  `["Wallpaper","org.kde.image","General"]`), `readConfig`/`writeConfig`,
  `id`, `screen`.
- **Desktop ↔ output mapping on this machine (verbatim):**

  | Connector | screenForConnector | screen | desktop index | containment id | wallpaper plugin |
  |---|---|---|---|---|---|
  | DP-1 | 0 | 0 | 1 | 111 | `org.kde.kwe.wallpaper` |
  | (orphan) | — | -1 | (none) | 105 | `org.kde.image` |

  Desktop 105 is orphaned (screen -1) and is never selectable by an output
  name; the mapping skips orphans. Desktop 108 carries a
  `com.github.catsout.wallpaperEngineKde` config group in `appletsrc`
  (`SteamLibraryPath`, `WallpaperSource`, `WallpaperWorkShopId=904545875`,
  from `/media/crushinator`) but no live plugin assignment — evidence the
  plugin is installed while the daemon's `org.kde.kwe.wallpaper` owns the
  live desktop. `appletsrc [Containments][111]` retains the
  `[Wallpaper][org.kde.image][General] Image=file:///usr/share/wallpapers/cachyos-wallpapers/Cachy depths 5K.png`
  group under the switched plugin — Plasma keeps the previous plugin's
  config group intact, which is exactly what the restore path reuses.
- **The WE package needs nothing beyond plugin assignment.** The
  `org.kde.kwe.wallpaper` package's `contents/ui/main.qml` declares no
  config keys; assigning `d.wallpaperPlugin = "org.kde.kwe.wallpaper"` is
  the complete activation. The renderer's actual wallpaper content is the
  validated catalog item passed to `renderer.start`, never Plasma config.
- **Bridge discovery path:** the daemon socket defaults to
  `$XDG_RUNTIME_DIR/kwe/daemon-v1.sock`, i.e.
  `/run/user/1000/kwe/daemon-v1.sock` for user 1000 (`main.rs
  default_socket_path`). Clients (the M4b manager) probe that path first.
- **Stock restore images verified present on this system** (first match
  wins; all absent is still a valid restore — `org.kde.image` falls back to
  its theme default):
  - `/usr/share/wallpapers/cachyos-wallpapers/Cachy depths 5K.png`
  - `/usr/share/wallpapers/cachyos-wallpapers/Abstract.png`
  - `/usr/share/wallpapers/Next/contents/images/Next.jpg`

## M4a — daemon live-apply transaction (this commit)

### Transaction semantics

`wallpaper.apply` runs one bounded transaction under an in-process lock:

1. **Validate params** per the StartSpec rules (kind/content match, bounds)
   — failures are `invalid_params` before anything else runs.
2. **Catalog lookup** — the wallpaper id must name a usable item (kind not
   `Invalid`/`Unknown`, `workshop_state` `local` or `subscribed_installed`).
   Miss → `apply_unknown_wallpaper`; kind mismatch → `apply_incompatible`.
3. **Enumerate outputs** (fresh probe — the 5 s cache does not apply) and
   resolve the output name; miss → `output_missing`.
4. **Start the renderer** through the existing supervisor with the catalog
   content, then **wait bounded (15 s) for promotion** to `Live` or
   `AwaitingAck` (polling `renderer.status` every 200 ms). Completion is
   defined by *promotion*, not by display acknowledgement: the daemon
   switches the Plasma config as soon as the renderer is live, and the
   frame display contract handles acknowledgement in its own lane.
   Rollback/quarantine before promotion → `apply_failed`.
5. **Persist the assignment** (with the current Plasma wallpaper config
   captured from the enumeration probe as `previous`).
6. **Switch the Plasma config** via
   `qdbus <service> /PlasmaShell evaluateScript <script>` — spawned with no
   shell (argv only), `stdin` closed, stdout/stderr capped at 64 KiB, killed
   at the 5 s deadline, and post-exit drained briefly (no zombie). The
   script is a **pure function of the desktop array index and the validated
   plugin name**, built by Rust and unit-tested for exact strings and
   escaping; wallpaper content never reaches the script.

Failure at any step after a successful start rolls back: the supervisor
stops the started renderer and the assignment store is reverted, then the
failure maps to `apply_failed` (or `shell_unreachable` when the switch
itself failed) with bounded detail. A concurrent apply while one is in
flight → `apply_busy` immediately.

`wallpaper.restore {output}`: looks up the saved `previous`
(plugin/config-group/image); with no assignment it restores the stock
`org.kde.image` + `["Wallpaper","org.kde.image","General"]` group plus the
first present stock image path (recorded above, or no `Image` write if none
exists), and reports which mode ran (`assignment` or `stock`). Restore
**always succeeds** on a real output; only an unknown output name fails
(`output_missing`).

### Exact apply script templates (verbatim from `apply.rs`)

Apply (switch) script — `apply_script(desktop_index, plugin)`:

```js
var d = desktops()[1]; d.wallpaperPlugin = "org.kde.kwe.wallpaper";
```

Restore script — `restore_script(desktop_index, plugin, config_group, image)`
(image omitted when `null`):

```js
var d = desktops()[1];
d.currentConfigGroup = ["Wallpaper", "org.kde.image", "General"];
d.writeConfig("Image", "file:///usr/share/wallpapers/cachyos-wallpapers/Cachy depths 5K.png");
d.wallpaperPlugin = "org.kde.image";
```

The read-only enumeration probe — one fixed template, connector names
interpolated only after daemon-side identity validation (`screenForConnector`
returns -1 for unknown connectors; an all-disconnected system gets
`var c = {};`). **The daemon never interpolates live data into this
template** — the desktop loop is a constant, `desktopForScreen` is never
touched:

```js
var d = desktops();
var out = [];
for (var i = 0; i < d.length; i++) {
  var image = null;
  var wp = d[i].wallpaperPlugin;
  if (/^[A-Za-z0-9._-]+$/.test(wp)) {
    var g = d[i].currentConfigGroup;
    try {
      d[i].currentConfigGroup = ["Wallpaper", wp, "General"];
      image = d[i].readConfig("Image");
    } catch (e) { }
    d[i].currentConfigGroup = g;
  }
  out.push({index: i, id: d[i].id, screen: d[i].screen, wp: wp, image: image});
}
var c = {"DP-1": screenForConnector("DP-1")};
print(JSON.stringify({desktops: out, connectors: c}));
```

Injection safety is a stated invariant: every interpolated value is
identity-validated (1–128 ASCII alnum/`._-`), the image is JS-escaped
(`\\`, `"`, `\n`, `\r`, `\u2028`, `\u2029`) and size-bounded (≤ 4096 chars),
and the unit tests assert hostile `wallpaper_id`/`image` values never reach
a script in raw form. The daemon does not use `desktopForScreen` anywhere.

### Assignments store (`assignments-v1.json`)

State dir: `assignments-v1.json`, mirroring the grants store pattern —
bounded BTreeMap store, `atomic_write` (temp + rename), quarantine-on-corrupt
(rename to `<file>.invalid-<secs>-<nanos>`, pruned to 8 files), and
`deny_unknown_fields` on every record. Bounds: ≤ 16 assigned outputs, ≤
1 MiB file, content/image strings ≤ 4096 chars. Validation covers identity
parts, kind (the `test` kind is never assignable), dimensions 1–8192, fps
1–240, and config groups of 1–4 identity parts.

```json
{
  "schema_version": 1,
  "outputs": {
    "DP-1": {
      "wallpaper_id": "1",
      "kind": "scene",
      "content": "/steam/workshop/content/431960/1/scene.json",
      "width": 960,
      "height": 540,
      "fps": 30,
      "applied_at_unix_seconds": 1787188000,
      "previous": {
        "wallpaper_plugin": "org.kde.image",
        "config_group": ["Wallpaper", "org.kde.image", "General"],
        "image": "file:///usr/share/wallpapers/cachyos-wallpapers/Cachy depths 5K.png"
      }
    }
  }
}
```

### RPC surface

All four methods live in `docs/SUPERVISOR_API_V1.md`:

- `wallpaper.outputs {}` — live enumeration: `kscreen-doctor` (bounded,
  ANSI-stripped parse) for geometry/enabled/connected plus one read-only
  `evaluateScript` probe for the desktop mapping. Cached 5 s per call,
  **never cached indefinitely** (a hotplug must not go unseen forever); the
  apply transaction always probes fresh. No args.
- `wallpaper.apply` — params `output`, `wallpaper_id`, `kind`, `content`,
  optional `width` (960)/`height` (540)/`fps` (30).
- `wallpaper.restore` — params `output`.
- `wallpaper.assignments {}` — the full store, bounded.

Error codes: `invalid_params`, `apply_unknown_wallpaper`,
`apply_incompatible`, `output_missing`, `apply_busy`, `shell_unreachable`,
`apply_failed`, `restore_failed` (all with bounded detail except
`apply_busy`). Without an apply handle (daemon built without the feature),
all four methods fail closed with `apply_unavailable`.

Daemon flags: `--plasma-shell-service` (default `org.kde.plasmashell`),
`--qdbus-binary`, `--kscreen-doctor-binary` (default `kscreen-doctor`),
`--apply-probe-timeout-ms` (5000, range 500–30000),
`--apply-promotion-timeout-ms` (15000, range 1000–60000).

## M4b — manager Apply UI (this commit)

M4b is the manager slice of BETA_M4: the details pane drives the daemon's
M4a transaction through a new `ApplyClient` (`apps/kwe-manager/src/
applyclient.{h,cpp}`, exposed as the `applyClient` context property), with an
output picker, a gated Apply action, and safe-mode restore. No wallpaper is
switched during development: M4b runs no live tests — the C++ test suite
drives a stub daemon only, and the live smoke stays gated behind
`KWE_LIVE_APPLY=1` until M4d.

### The client (`ApplyClient`)

Mirrors `PermissionsClient`'s QLocalSocket pattern: one request per
connection, newline-delimited JSON, bounded responses; requests queue
(bounded, 64) and retry with exponential backoff (5 s → 30 s) so an
apply/restore survives a daemon restart. The lane is **strictly
serialized** — the daemon rejects a concurrent apply with `apply_busy` and
restore takes no transaction lock, so the client never issues a second
`wallpaper.*` operation while one is in flight, and the UI disables the
picker and both actions whenever `busy` holds.

State machine (`State` enum, reachable from QML as `ApplyClient.Applied`
etc.):

| State | Meaning | UI |
|---|---|---|
| `Idle` | nothing in flight; last op (or its mirror refresh) succeeded | picker + actions enabled when eligible |
| `ListingOutputs` | `wallpaper.outputs` in flight | picker disabled, "Enumerating display outputs…" |
| `Applying` | `wallpaper.apply` in flight | Apply shows "Applying…", disabled |
| `Restoring` | `wallpaper.restore` in flight | Restore shows "Restoring…", disabled |
| `Applied` | apply succeeded (fields: `appliedWallpaperId`, `appliedOutput`) | Positive InlineMessage "Applied \<title\> to \<output\>" |
| `Failed` | apply/restore failed (or enumeration, with Try Again hidden) | Error InlineMessage with the mapped detail; Try Again only when `failedMethod` is set |

Result honesty rules: a result belongs to the operation that produced it —
`resetStatus()` runs on selection change, a new user-facing operation clears
the previous confirmation, and the Applied message only shows when the
current selection still matches `appliedWallpaperId`/`appliedOutput`, and
changing the picker selection runs `resetStatus()`. The
assignment mirror (`refreshAssignments()`/`assignments`, auto-refreshed
after every successful apply/restore) is a background lane: its failures
never clobber the user-facing state — neither the daemon-answered failure
path nor a socket-level loss while the mirror is in flight.

Daemon error mapping (actionable text, detail included when the daemon
bounded one):

| Wire code | User-facing |
|---|---|
| `output_missing` | "Output not found: \<name\>" |
| `apply_busy` | "Another apply is in progress; wait for it to finish." |
| `apply_unknown_wallpaper` | "This wallpaper is not available to apply: \<id\>" |
| `apply_incompatible` | "This wallpaper cannot be applied in its current form: \<detail\>" |
| `shell_unreachable` | "The Plasma desktop could not be reached: \<detail\>; nothing was changed." |
| `apply_failed` | "Applying failed: \<detail\>" |
| `restore_failed` | "Restoring the previous wallpaper failed: \<detail\>" |
| `invalid_params` / `apply_unavailable` | rejected-request / no-apply-lane wording |

### The UI flow (WallpaperDetail.qml)

- **Output picker** — a ComboBox over `applyClient.outputs`, loaded on
  demand when the details pane becomes visible (`wallpaper.outputs` is
  cached 5 s daemon-side, so re-listing is cheap and picks up hotplugs);
  disabled + explanatory text while empty or listing, and disabled during
  any in-flight operation.
- **Apply button** — enabled only when (a) an output is selected,
  (b) the kind is `video`/`web`/`scene` with a resolvable catalog content
  path (entry file for video/scene, content root for web — the content
  root now flows from `CatalogModel::ContentRootRole` as a URL through
  `WallpaperCard`/`WallpaperSelection`), (c) compatibility is
  `renderer_dependent`, and (d) no apply-lane operation is in flight.
  Text "Apply to \<output\>", busy text "Applying…".
- **Preflight line** — the existing compatibility InlineMessage
  (compatibility label + `compatibility_detail` from the catalog) serves
  as the per-kind preflight summary; a hint label states which gate blocks
  Apply when it is disabled.
- **Failure** — Error InlineMessage with the mapped daemon detail (text +
  icon; never color alone) and a **Try Again** affordance that re-runs
  *exactly* the operation that failed (`retry()` re-sends the recorded
  apply or restore, or re-runs the enumeration), never a different one.
  Enumeration failures have no recorded target to replay, so Try Again
  stays hidden there (`failedMethod` is only set for apply/restore) — and
  changing the output picker clears any stale failure so retry can never
  replay a recorded output the UI no longer shows.
- **Safe mode** — a "Reset to image wallpaper" action on the same page,
  always available when an output is selected, calling
  `applyClient.restoreWallpaper`; the label names the stock fallback
  because on an output this client never applied to, the daemon resets to
  the stock image rather than a saved "previous" wallpaper. Success shows
  the Information InlineMessage "Restored the image wallpaper on
  \<output\>" (with a "(stock image fallback)" suffix when the daemon
  restored the stock image rather than a saved assignment).
- **Gallery banner** — the alpha "Applying stays disabled" framing is
  replaced by the honest current state; the playlist frame's stale
  "Display assignment is not enabled yet" line now reads "Playlist display
  assignment arrives in a later milestone".

### Test surface

`kwe-apply-client-test` (stub daemon answering the `wallpaper.*` wire
protocol, mirroring `kwe-permissions-client-test`): listOutputs round-trip,
apply success state machine, web content-root param, apply failure surfaces
the daemon detail, restore round-trip (stock + assignment modes), the error
mapping table (incl. `invalid_params`), queue serialization (second op
waits, drain in order after a daemon loss), queue bound (66th op fails
immediately, least-urgent drop on daemon loss), assignments round-trip,
background-failure isolation (daemon-answered and socket-level loss while
the mirror is in flight), failed-enumeration retry (Try Again hidden,
`retry()` re-lists), retry re-runs the exact failed op, resetStatus,
invalid-input rejection without traffic. `smoke-ui.sh` is intentionally
unchanged (the live smoke flips in M4d).

## M4c — playlist renderer assignment (this commit)

When the playlist session's active entry changes — timer advance, policy
switch, manual play, resume-after-restart — the session drives the **apply
transaction** (M4a) for the entry's wallpaper on the assigned output. No new
RPC method: the surface stays `playlist.*` + the M4a `wallpaper.*` lane; the
output is a daemon flag, not a client param.

### Output scoping

Saved profiles are snapshots of **per-display** wallpaper/playlist
assignments (docs/UX_DESIGN.md), so the session carries
`--playlist-output <output>` and, when unset, resolves the output **at apply
time** in the lane:

1. the last assigned output whose wallpaper is a member of the active
   playlist (the assignment store is the source of per-display intent), else
2. the first enabled and connected output from a fresh enumeration, else
3. `output_missing`.

### Transition semantics

Entry changes are **hard cuts through the apply transaction**: the M4a
transaction displaces the session's own stale renderer (stop + start +
switch) and the supervisor's keepalive covers the gap. Crossfade transitions
remain open (they need two live renderers on one output).

### Failure and precedence

- Quarantined/unavailable entries are skipped before the lane runs (the
  existing `try_quarantined_ids` decision path).
- A failed apply rolls back exactly like `wallpaper.apply` — renderer
  stopped if it is still ours, assignment store reverted, Plasma config
  untouched (a failure before the switch step leaves the previous plugin in
  place; the switch step never ran). The previous renderer is **not** kept
  live: the display freezes on the supervisor's last-known-good frame for
  that wallpaper until the session's next successful apply. The failure
  logs one bounded line and backs off exponentially — 1 s doubling to a
  30 s cap.
- The apply runs on a dedicated worker thread (one apply at a time, bound
  queue of 1); the session tick thread never blocks on a transaction, so
  `playlist.*` RPCs stay responsive while an apply is in flight. The lane
  shares the apply transaction's single lock: a user apply in flight wins
  the slot. A `Busy` from that lock — or a foreign renderer that became
  live between the session's verdict and the lock (the lane re-checks
  supervisor state after taking the lock, closing the TOCTOU window) — is a
  **transient yield, never a failure**: no backoff is armed, and the yield
  clears any previously-armed backoff, so re-assertion is prompt once the
  user's renderer stops.
- While a foreign renderer is live the session **yields** (user choice
  wins); it re-asserts its entry when nothing is live (manual stop, crash)
  or when the entry changes (its own stale renderer is displaced).
- Crash-restore: when the supervisor is recovering the requested wallpaper
  (Restarting/RolledBack) but the active worker renders a different
  wallpaper (or none), the session does **not** claim the entry satisfied
  and does not dispatch a competing apply — it waits while the supervisor's
  own bounded recovery or quarantine resolves it (a quarantine then flows
  through the existing skip logic).
- Restart restore re-applies the entry **once**: the supervisor is fresh and
  idle at boot, so the session's first tick sees no live renderer and drives
  the lane (the store is the source for restore, the supervisor is the
  source of live).

### Test surface

Unit (stub lane, no live Plasma): the apply-verdict pure matrix
(satisfied/own-stale-displace/foreign-yield across every live phase,
manual-stop re-assert, restart restore), backoff bounds (1 s → 30 s cap),
entry change applies through the lane with the configured output and full
entry set, quarantined entries never reach the lane, failing applies back
off without storming (3 failing attempts in 3.6 s, not dozens), restart
restore re-applies once. Integration (real lane, real supervisor, fake
scene renderer, stub switch boundary): entry changes drive the real
transaction (store write + switch script per entry), user apply takes
precedence and the session re-asserts after stop, restart re-applies once
(exactly one switch script on the second daemon). Smoke: scenario 9 of
`scripts/smoke-playlist-restart.sh` runs the real daemon with the fake
scene renderer and a stubbed Plasma boundary and asserts the store and
switch-script counts across a timer advance and a restart.

## M4d — live enablement (this commit)

M4d turns the M4a transaction into a real desktop switch on this machine and
proves it with a DESTRUCTIVE live smoke (`scripts/smoke-live-apply.sh`,
opt-in via `KWE_RUN_LIVE_APPLY_SMOKE=1`). The smoke runs the REAL daemon
against the REAL Plasma session (authorized; AGENTS.md's no-live-session
rule is waived for BETA_M4), switches the live desktop to real video and web
wallpapers, fails a hostile scene with containment, and restores — with the
plasmashell PID asserted unchanged across every destructive step.

### The enablement decision

The installed `org.kde.kwe.wallpaper` plugin's `DisplaySession` polls the
daemon at the default runtime socket (`$XDG_RUNTIME_DIR/kwe/daemon-v1.sock`).
A smoke that runs its own daemon on a private socket (the smoke-apply
pattern) would therefore switch the desktop's plugin but never show frames:
the plugin would keep talking to the idle system daemon. So **the M4d smoke
daemon binds the REAL runtime socket**: the system `kwe-daemon.service` is
stopped first (if running), the smoke daemon takes the socket, and the exit
trap kills the smoke daemon, restores the pre-test wallpaper plugin/config,
and restarts the system daemon. This exactly mirrors the real deployment —
the daemon the plugin connects to is the daemon that applied the wallpaper —
and "frames reach the desktop" is proven by the plasmashell process holding
the smoke renderer's frame file open (`/proc/<plasmashell-pid>/fd`) while the
frame sequence advances.

The pre-test wallpaper plugin/config is captured at script start with the
documented read-only probe (never `desktopForScreen`) and restored on EVERY
exit path (trap, registered for EXIT/INT/TERM, idempotent and bounded).
The desktop ends the run exactly as it began.

**Suite split decision:** `smoke-apply.sh`'s `KWE_LIVE_APPLY=1` lane stays
the READ-ONLY live lane (enumeration + fail-closed error cases); the
DESTRUCTIVE live lane is `smoke-live-apply.sh`. They are intentionally
separate suites.

**Machine state investigated and handled honestly:** desktop 111 on this
machine already carried `org.kde.kwe.wallpaper` (the M4a research found the
same) with the system daemon idle and no assignments. The pre-test capture
therefore records plugin `org.kde.kwe.wallpaper`, and the smoke's restore
(assignment mode) returns exactly to it — the "already assigned" state is
treated as the pre-test baseline, not assumed to be `org.kde.image`.

### The smoke case table (observed, 2026-08-20, Plasma 6.7.4 Wayland)

| Case | Step | Expected containment | Observed result |
|---|---|---|---|
| — | pre-test capture | plasmashell PID + target output (DP-1) plugin/config recorded; system daemon state recorded | plasmashell 919019; DP-1 → desktop index 1, plugin `org.kde.kwe.wallpaper`, image none; system daemon running |
| 1 | VIDEO apply (synthetic `#3366CC` mp4, 320x180) | transaction ok, assignment persists, plugin `org.kde.kwe.wallpaper`, plasmashell consumes the renderer's frame file, frames advance, PID unchanged | apply ok; store `DP-1 → 1` kind video; probe plugin `org.kde.kwe.wallpaper`; plasmashell opened `frame-<daemon>-1.bin`; frame pixel (50,102,203) vs expected (51,102,204) within tolerance; PID 919019 |
| 2 | WEB apply (synthetic self-contained page, 320x180) | same as case 1 for the web kind | apply ok; store `DP-1 → 2`; probe plugin `org.kde.kwe.wallpaper`; plasmashell opened `frame-<daemon>-2.bin`; frames advancing; PID unchanged |
| 3 | BAD SCENE apply (`{"general": 42}` — passes preflight, renderer rejects at parse, exit 73) | renderer fails, transaction rolls back (`apply_failed`), desktop stays operable, no plasmashell crash, previous assignment preserved | `apply_failed`; plugin stays `org.kde.kwe.wallpaper`; renderer not live; store still records the web assignment (rollback preserved it); PID unchanged |
| 4 | RESTORE | assignment-mode restore to the pre-test plugin/config, store cleared | mode `assignment`; restored plugin `org.kde.kwe.wallpaper`; store cleared; final probe identical to pre-test |
| 5 | end state | desktop exactly as it began; plasmashell PID unchanged across the whole run | pre-test and final probes byte-identical; plasmashell PID 919019 throughout |
| — | failure/recovery | a failed assertion still restores the desktop | injected mid-run assertion failure → exit 1 → trap restored the wallpaper, restarted the system daemon, removed the smoke root, plasmashell unchanged |

The whole suite runs in ~4 s on this machine (renderer cold starts are fast
here) and is safe to re-run: the pre-test capture is taken fresh every run
and the trap restore is idempotent.

## Run the suites

```sh
scripts/smoke-apply.sh       # M4a: READ-ONLY live apply smoke; SKIPPED
                             #   with exit 0 unless KWE_LIVE_APPLY=1; never
                             #   switches a live wallpaper (M4d decision:
                             #   this lane stays read-only)
scripts/smoke-live-apply.sh  # M4d: DESTRUCTIVE live smoke — applies real
                             #   video/web wallpapers + a hostile scene to
                             #   the live desktop, then restores; requires
                             #   KWE_RUN_LIVE_APPLY_SMOKE=1 (authorized on
                             #   this machine only); plasmashell PID must be
                             #   unchanged throughout
scripts/smoke-playlist-restart.sh  # M5k regression + M4c scenario 9:
                             #   renderer assignment through the real apply
                             #   transaction with the fake scene renderer
                             #   and a stubbed Plasma boundary
scripts/smoke-supervisor.sh  # unchanged (M1a regression lane)
scripts/smoke-video.sh       # unchanged (M1 regression lane)
```

`smoke-apply.sh` builds the workspace, uses a private temporary
socket/runtime/state tree with a fixture Steam root (one subscribed scene
project), seeds `assignments-v1.json` before daemon start, and removes
everything on exit. The M4a case set: `wallpaper.outputs` (live read-only
enumeration, prints each output for the record), apply with an unknown
wallpaper id → `apply_unknown_wallpaper`, apply to a nonexistent output →
`output_missing`, seeded-assignment round-trip via `wallpaper.assignments`,
and restore on a nonexistent output → `output_missing`. The
restore-to-image path (which writes live wallpaper config) is deliberately
deferred to M4d's live smoke; its contract is documented above and exercised
in the daemon RPC tests with a stub shell.

## Acceptance evidence

Validated on 2026-08-19 (CachyOS, Plasma 6.7.4 Wayland; shared
`CARGO_TARGET_DIR` at the KDE-Wallpaper-Engine tree).

### M4a — daemon live-apply transaction (this commit)

| Case | Expected containment | Result |
|---|---|---|
| workspace gates | `cargo fmt --all -- --check`; `cargo clippy --workspace --all-targets -- -D warnings`; `cargo test --workspace --all-targets` | clean; all unit and RPC tests pass (17 apply unit tests + 16 wallpaper.* RPC tests) |
| assignment store | round-trip, bounds (hostile output names, test kind, 17th output), corruption quarantine (oversize, wrong schema, unknown fields, test-kind record → `.invalid-` files), failed save keeps the previous file | all pass |
| script builders (pure) | exact strings, escaping, injection test (hostile `wallpaper_id`/content with quotes/backslashes never reach a script), probe template exactness | all pass |
| output enumeration | `kscreen-doctor` parse (geometry, ANSI, garbage), probe reply parse (`deny_unknown_fields`), connector→desktop mapping (orphan skipped), cache freshness boundaries | all pass |
| RPC: apply happy path | real supervisor + fake scene renderer promote to `Live`; exact switch script recorded; assignment persisted with `previous`; no scene content in the script | passes (`event=renderer.promoted generation=1 wallpaper_id=1`) |
| RPC: rollback | switch failure → `shell_unreachable`, supervisor `Stopped`, assignment dropped | passes |
| RPC: fail-closed | unknown wallpaper id, unknown output, incompatible kind, invalid params, concurrent apply → `apply_busy`, no apply handle → `apply_unavailable` | all pass |
| RPC: restore | stock fallback (mode `stock`, first present image, or no `Image` write), assignment revert (mode `assignment`, `old.png` restored, store cleared), unknown output → `output_missing` | all pass |
| smoke-apply (live) | `KWE_LIVE_APPLY=1`: read-only enumeration shows the real outputs; error cases fail closed; seeded assignments round-trip | all pass — `output DP-1 screen=0 desktop_id=111 plugin=org.kde.kwe.wallpaper`; "all apply smoke cases passed" |
| smoke-apply (gated) | without the env flag: SKIPPED note, exit 0 | passes |
| live config | no wallpaper switch executed by M4a (M4d flips the gate) | no `evaluateScript` switch against the live session; only read-only enumeration |

### M4b — manager Apply UI (this commit)

| Case | Expected containment | Result |
|---|---|---|
| workspace gates | `cargo fmt --all -- --check`; `cargo clippy --workspace --all-targets -- -D warnings`; `cargo test --workspace --all-targets` | clean; all unit/RPC tests pass |
| manager build | `cmake -S . -B build/cmake -G Ninja -DCMAKE_BUILD_TYPE=Debug && cmake --build build/cmake --parallel` | builds clean |
| ApplyClient unit suite | `ctest` `kwe-apply-client-test`: listOutputs round-trip; apply state machine (Idle→Applying→Applied, wire params incl. web content root); apply failure surfaces the daemon detail + `failedMethod`; restore round-trip (stock/assignment modes); error mapping table (`output_missing`→"Output not found", `apply_busy`, `apply_unknown_wallpaper`, `shell_unreachable`, `apply_failed`, `restore_failed`, `apply_unavailable`, `invalid_params`); serialized queue (second op waits; daemon loss re-queues in order); queue bound (66th op immediate failure, least-urgent drop); assignments round-trip; background mirror failures isolated (daemon-answered and socket loss while the mirror is in flight); failed enumeration retryable via `retry()` with `failedMethod` empty (Try Again hidden); retry re-runs the exact failed op; resetStatus; invalid input rejected without traffic | all pass |
| qmllint | `qmllint -I /usr/lib/qt6/qml -I build/cmake/apps/kwe-manager apps/kwe-manager/qml/*.qml` | clean |
| smoke-ui | `./scripts/smoke-ui.sh` unchanged (no apply-flow assertions here by design; M4d adds the live smoke) | passes |
| UI honesty | alpha "Applying disabled" banner and stale "Display assignment is not enabled yet" line replaced with the true current state; Apply gated on kind/content/compatibility/output | verified in QML + docs |
| live config | M4b executes no live wallpaper switch (development-only stub tests) | no `evaluateScript` switch against the live session |

### M4c — playlist renderer assignment (this commit)

| Case | Expected containment | Result |
|---|---|---|
| workspace gates | `cargo fmt --all -- --check`; `cargo clippy --workspace --all-targets -- -D warnings`; `cargo test --workspace --all-targets` | clean; all unit/RPC tests pass (121 daemon tests, incl. 6 new apply-verdict/backoff unit tests, 3 new playlist integration tests, and 1 new external-evaluator test) |
| apply verdict (pure) | desired-live satisfied (all live phases); own-stale renderer displaced; foreign renderer yields (all live phases); idle/stopped/quarantined re-assert once the backoff gate opens; closed gate holds the previous assignment | all pass |
| backoff bounds | 1 s → 2 s → 4 s → 8 s → 16 s → 30 s cap; failing lane makes exactly the failing attempts within the window, never a storm; retry succeeds after the gate opens | all pass |
| entry change (unit) | timer advance applies the next entry through the lane with the configured output and the full entry set | passes |
| quarantine skip | persisted quarantine record for entry 1 → the lane only ever sees entry 2 | passes |
| restart restore (unit) | second session on the same state re-applies the restored entry once | passes |
| integration: entry changes | real transaction: fake scene renderer `Live` on 1 → 2 → 3 across real timer advances; assignment store tracks DP-1; exactly 3 switch scripts, no steady-state churn | passes |
| integration: user precedence | user apply of another wallpaper displaces the session; the session yields (no extra switch while the user's renderer is live); after `renderer.stop` the session re-asserts its entry through the lane | passes |
| integration: restart restore | second daemon on the same state re-applies the restored entry exactly once (one switch script) and stays live | passes |
| smoke-playlist-restart | M5k scenarios 1–8 regression + M4c scenario 9: renderer.status wallpaper/kind, assignments store (DP-1), timer-advance flip, restart re-applies once, switch-script counts | all pass |
| live config | M4c executes no live wallpaper switch (stub `--plasma-switch-command` in every smoke; live enablement is M4d) | no `evaluateScript` switch against the live session |

### M4d — live enablement (this commit)

| Case | Expected containment | Result |
|---|---|---|
| workspace gates | `cargo fmt --all -- --check`; `cargo clippy --workspace --all-targets -- -D warnings`; `cargo test --workspace --all-targets`; `./scripts/check.sh` | clean (see the acceptance gates section below) |
| pre-test capture | plasmashell PID (exactly one), target output plugin/config via the read-only probe, system daemon state recorded before any change | DP-1 → desktop index 1, plugin `org.kde.kwe.wallpaper`, image none; system `kwe-daemon.service` running |
| case 1: video apply (live) | apply ok; store `DP-1 → 1` kind video; probe plugin `org.kde.kwe.wallpaper`; plasmashell holds the renderer frame file open; frame pixel matches `#3366CC`; PID unchanged | all pass (pixel (50,102,203) vs (51,102,204), tolerance 6) |
| case 2: web apply (live) | apply ok; store `DP-1 → 2`; plugin `org.kde.kwe.wallpaper`; plasmashell holds the web frame file open; frames advancing; PID unchanged | all pass |
| case 3: hostile scene (live containment) | renderer rejects the scene (exit 73) → `apply_failed`; desktop stays on the kwe plugin; renderer not live; rollback preserves the previous assignment; PID unchanged | all pass |
| case 4: restore | assignment-mode restore to the pre-test plugin/config; store cleared; final probe identical to pre-test | all pass |
| case 5: end state | plasmashell PID unchanged across every destructive step; desktop exactly as it began | pre-test and final probes byte-identical; PID 919019 throughout |
| failure/recovery | a failed assertion still restores the desktop; trap idempotent | injected mid-run assertion failure → exit 1 → wallpaper restored, system daemon restarted, smoke root removed, plasmashell unchanged |
| system daemon | the smoke daemon takes the real socket and the system service is restored on exit | system daemon stopped during the run, restarted by the trap (new MainPID); socket ownership returned to the system daemon |

## Open risks

- `evaluateScript` traffic depends on the shell being reachable and
  responsive; the daemon bounds every call (5 s, kill, 64 KiB caps) and
  fails closed (`shell_unreachable`), but a wedged plasmashell leaves apply
  unavailable until it recovers — restore is the manual recovery lane and
  also fails closed, so a wedged shell is visible in the error codes rather
  than silent.
- The `desktopForScreen(-1)` crash hazard is documented above and excluded
  by design (unit tests pin the probe template); a future Plasma release
  changing the desktop API surface would need the probe template updated in
  lockstep.
- M4a assigns plugins but never writes `lastScreen`/containment geometry;
  multi-monitor reordering between apply and restore is resolved by
  connector name, and a replug that reorders desktops is re-probed fresh on
  every apply.
- The restore contract depends on Plasma retaining the previous plugin's
  config group after a switch, which is observed behavior on 6.7.4
  (appletsrc retains the `[Wallpaper][org.kde.image][General]` group under
  the kwe plugin); a Plasma change that prunes orphaned groups would leave
  restore to the stock-image fallback instead of the saved image.
- A user `wallpaper.apply` runs synchronously on the daemon's single accept
  loop for the transaction duration (M4c moved only the playlist lane onto
  its worker thread), so the live plugin's 500 ms `renderer.status` polls can
  time out during a multi-second apply and show a transient "service
  unavailable" state, then recover once the renderer is live. Bounded and
  self-healing; moving the user apply onto the same worker lane is a
  follow-up.
- The M4d smoke stops/restarts the system `kwe-daemon.service` to take the
  real socket. The EXIT/INT/TERM trap always restores it; a SIGKILL of the
  smoke script itself (untrappable) would leave the service stopped until
  the next run's fresh capture or a manual `systemctl --user start
  kwe-daemon`.
- Frames-reach-desktop is asserted via `/proc/<plasmashell-pid>/fd` (the
  plugin holds the frame file open) plus the frame sequence advancing; this
  relies on plasmashell running un-sandboxed (no `PrivateTmp`) so it can
  open the renderer's frame file — true on this system, and the reason the
  smoke refuses to run without a live plasmashell.
