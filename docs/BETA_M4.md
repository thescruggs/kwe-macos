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

## Run the suites

```sh
scripts/smoke-apply.sh       # M4a: live apply smoke; SKIPPED with exit 0
                             #   unless KWE_LIVE_APPLY=1 (M4d flips it on);
                             #   the M4a run is read-only against the live
                             #   Plasma session — no wallpaper switch
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
