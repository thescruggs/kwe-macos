# Bug: Apply is dead — "No display outputs are available yet"

- **Reported:** 2026-08-22 (user report against the installed
  `kde-wallpaper-engine 0.1.0.alpha.1-2` package, binaries built 10:42 from
  `fix/qt611-gallery-delegates` @ `c4a8d80`)
- **Severity:** High user-facing (the entire BETA_M4 live-apply lane is
  unreachable from the UI; the failure is silent — no error text anywhere)
- **Status:** FIXED on branch `fix/qml-type-registration` (worktree
  `/home/qcv123/gitProjects/kwe-qmlreg`); verified against the live daemon —
  see "Fix" below.

## Fix (2026-08-22)

1. **Type registration (`apps/kwe-manager/src/*.h`, `apps/kwe-manager/CMakeLists.txt`).**
   `QML_ELEMENT` beside `QML_UNCREATABLE` on all five clients, plus
   `target_include_directories(kwe-manager PRIVATE src)` — the generated
   `kwe-manager_qmltyperegistrations.cpp` includes each class as
   `<applyclient.h>` and did not compile without it. The generated qmltypes
   now lists all five types, and a manager run logs **zero** ReferenceErrors.
   `import org.kde.kwe` was added to `GalleryPage.qml` and
   `WallpaperDetail.qml`: the engine resolves a module's own types implicitly
   even from the `qml/` subdirectory (measured — the import is not required at
   runtime), but qmllint does not, and the new static gate depends on it.
2. **Enumeration trigger (`WallpaperDetail.qml`).** `ensureOutputs()` is now
   called from `Component.onCompleted` as well as `onVisibleChanged`, so a
   pane created already visible still enumerates, and a listing skipped
   because the lane was busy is re-armed from `onBusyChanged` — guarded on
   `outputsListed` (a daemon that truthfully reports zero outputs is asked
   once, not in a loop) and on `Failed` (an error stays on screen for Try
   Again instead of being retried behind the user's back).
3. **The silent client paths (`applyclient.{h,cpp}`).**
   - New `outputsListed` property: an enumeration that answers with nothing
     now signals and sets an explanatory message instead of returning early on
     the unchanged empty list. The picker's label and tooltip say which of the
     three states holds (enumerating / none exist / not enumerated yet).
   - `resetStatus()` only clears when the lane is settled, so a selection or
     combo change no longer erases a failure that is still queued.
   - New per-request deadline (10 s; 30 s for apply, above the daemon's 15 s
     promotion wait): an accepted-but-unanswered request fails with a message
     instead of leaving the client busy forever. A deadline miss is **not**
     replayed automatically — the daemon may still be running the transaction
     it never answered — so Try Again drives it.
   - `failedMethod` now emits `resultChanged` when it is set after
     `clearResult()`; the Try Again affordance's binding never re-evaluated.
4. **Packaging (`packaging/PKGBUILD`, `packaging/.SRCINFO`).** `kscreen` and
   `qt6-tools` added to `depends` — the enumeration shells out to
   `kscreen-doctor` and `qdbus6`.
5. **Gates.**
   - New `scripts/qml-typecheck.sh`, called from `scripts/check.sh` in place of
     the Qt 5 `qmllint` invocation: it resolves the Qt 6 qmllint itself,
     asserts the built qmltypes registers all five types, and fails on any
     unresolved *type* (an unqualified identifier starting with an uppercase
     letter). Unqualified *instances* are the context-property design and are
     only counted. Verified both ways — green on the fixed tree, and it names
     the exact lines when the registration or the import is removed.
   - `scripts/smoke-ui.sh` forces Qt diagnostics to stderr, captures the
     manager output per case, and fails on any QML diagnostic. Recorded honestly
     as a backstop: the offscreen platform never exposes the window, so
     bindings that only evaluate on a real render stay quiet there — the static
     gate above is the one that catches this class of bug.
6. **Regression tests (`apps/kwe-manager/tests/applyclienttest.cpp`, 22 pass).**
   `emptyEnumerationIsAnAnswerNotSilence`,
   `resetStatusKeepsTheErrorOfAnOperationStillPending`,
   `unansweredRequestFailsAtItsDeadline`. The stub daemon's `reset()` now
   restores its default output list.

### Verification

- `ctest` in `build/cmake`: 8/8 pass (22 apply-client cases).
- `scripts/qml-typecheck.sh`: 5 types registered, no unresolved types.
- `scripts/smoke-ui.sh`: passes both cases.
- Live, through a logging relay on the real daemon socket: the manager issues
  `wallpaper.outputs`, the daemon answers with `DP-1`, and the run logs no QML
  errors at all.
- Not covered here: clicking Apply on the live desktop. The lane below the
  picker is unchanged by this fix and is covered by `scripts/smoke-live-apply.sh`.

## Symptom

On the wallpaper details pane the "Apply to display" combo box is empty and
disabled. Hovering it shows the tooltip *No display outputs are available
yet* (`WallpaperDetail.qml:215-217`). No error banner, no "Enumerating display
outputs…" text, no way forward.

## Root cause — two independent defects, both required to explain what is seen

### 1. `wallpaper.outputs` is never requested (why the list is empty)

`listOutputs()` has exactly one call site — `WallpaperDetail.qml:49-58`:

```qml
onVisibleChanged: {
    if (visible) {
        refreshPermissions();
        if (!applyClient.busy)
            applyClient.listOutputs();
    }
}
```

The pane's `visible` is bound to `detailsVisible`
(`WallpaperDetail.qml:15`, fed by `GalleryPage.qml:404`
`Window.window !== null && Window.window.width >= Kirigami.Units.gridUnit * 48`).
`Item.visible` **defaults to true**, and when that binding's first evaluation
also yields true there is no property change, so `onVisibleChanged` never
fires and the request is never issued.

Measured on an instrumented build of this exact revision (temporary
`console.log` in `Component.onCompleted` / `onVisibleChanged`, since reverted —
the worktree is clean):

```
KWEDBG detailsVisibleChanged= true visible= false      # instance A (hidden page)
KWEDBG visibleChanged= false busy= false
KWEDBG completed visible= false detailsVisible= true
KWEDBG detailsVisibleChanged= true visible= true       # instance B (the visible one)
KWEDBG completed visible= true detailsVisible= true    # ← no visibleChanged, ever
```

There are two `WallpaperDetail` instances (the Installed and Workshop pages
each embed one). The one the user actually interacts with is instance B: it
completes already visible, so its handler never runs. Instance A's handler
runs only with `visible == false`, which does nothing.

Corroborated end-to-end by tapping the manager's socket traffic through a
logging relay (`kwe-manager --socket <tap>`, 30 s, real Wayland session, the
packaged binary):

```
5 "method":"catalog"      6 "method":"renderer.status"      # and nothing else
```

No `wallpaper.outputs`, and no `permissions.get` either — `refreshPermissions()`
sits in the same dead handler. A second run of the freshly built binary
behaved identically (`catalog` ×4, `playlist.list` ×1, `renderer.status` ×5).

The `busy` guard at `WallpaperDetail.qml:55-56` is a second latent trap: a
failed request stays queued through the 5 s→30 s retry backoff
(`applyclient.cpp:317, 341-346`), so any visibility transition during that
window is also swallowed, with nothing to reschedule it.

**The daemon is not at fault.** Called directly against the live socket it
answers correctly and fast:

```
$ kwe daemon-call --socket $XDG_RUNTIME_DIR/kwe/daemon-v1.sock \
    --method wallpaper.outputs --params '{}'
{"ok":true,"result":{"outputs":[{"name":"DP-1","screen":0,"desktop_id":111,
  "desktop_index":1,"enabled":true,"connected":true,
  "geometry":[0,0,2926,823],
  "wallpaper_plugin":"org.kde.kwe.wallpaper",
  "config_group":["Wallpaper","org.kde.kwe.wallpaper","General"],
  "image":null}]},"version":1}
```

Both probe legs work under the unit's sandbox: `kscreen-doctor -o` parses
cleanly through `parse_kscreen_doctor` (`apply.rs:844-883`, ANSI stripped,
`Output: 1 DP-1 <uuid>` → `DP-1`), and the `evaluateScript` probe reaches
`org.kde.plasmashell` via the `qdbus`→`qdbus6` fallback (`apply.rs:642-648`;
only `qdbus6` exists on this system).

### 2. No C++ type is registered with QML (why the failure is invisible)

`ApplyClient`, `CatalogClient`, `PermissionsClient`, `PackageInstaller` and
`DaemonActivator` all carry `QML_UNCREATABLE` **without `QML_ELEMENT`**
(`apps/kwe-manager/src/applyclient.h:29` and the four siblings).
`QML_UNCREATABLE` alone only emits
`Q_CLASSINFO("QML.Creatable","false")`; `qmltyperegistrar` skips a class that
never declares itself an element. The generated type file is empty:

```
$ cat build/cmake/apps/kwe-manager/org/kde/kwe/kwe-manager.qmltypes
...
Module {}
```

and the generated `qmldir` lists only the five `.qml` files. Introduced by
`1833331` ("fix: QML enum access and daemon hardening from adversarial
review"), which correctly moved every binding to the type-based enum access
Qt 6.11 requires (`catalogClient.Loading` → `CatalogClient.Loading`) but never
registered the types. The comment at `apps/kwe-manager/CMakeLists.txt:33-38`
asserts the registration exists.

Every affected binding throws at runtime. Over 6 hours of normal use the
session logged 2060 of them:

```
$ journalctl --user -u "app-org.kde.kwe@*" | grep ReferenceError | sort | uniq -c
 412 GalleryPage.qml:89|121|272|390|393: ReferenceError: CatalogClient is not defined  (×5 lines)
   4 WallpaperDetail.qml:222|231|270|280|301: ReferenceError: ApplyClient is not defined
   4 GalleryPage.qml:95|101|282|283|291|297:  ReferenceError: PackageInstaller is not defined
   4 GalleryPage.qml:136|148|155:             ReferenceError: DaemonActivator is not defined
```

In the apply lane this erases exactly the diagnostics that would have
explained defect 1:

- `WallpaperDetail.qml:222-224` — the status label (`ApplyClient.ListingOutputs`)
  renders no text at all, so neither "Enumerating display outputs…" nor
  "No display outputs are available." is ever shown.
- `WallpaperDetail.qml:301` — the error banner (`ApplyClient.Failed`) can
  never become visible, so a daemon `shell_unreachable` detail would also be
  invisible.
- The combo's tooltip at `:215-217` references no type, which is why it is the
  only string that survives — and the only thing the user can see.

A failing `wallpaper.outputs`, a successful-but-empty one, and a request that
was never sent are all indistinguishable in the UI today.

### Secondary: even after registration, the QML files may still need the import

The QML files live one directory below the module root
(`qrc:/qt/qml/org/kde/kwe/qml/`; the subdirectory's generated `qmldir`
contains only a `prefer` line) and none of them `import org.kde.kwe`. Verify
type resolution from that subdirectory after adding `QML_ELEMENT`; add the
explicit import if it does not resolve.

## Fix requirements

1. **Register the types.** `QML_ELEMENT` beside `QML_UNCREATABLE` in
   `applyclient.h`, `catalogclient.h`, `permissionsclient.h`,
   `packageinstaller.h`, `daemonactivator.h`; add `import org.kde.kwe` to the
   QML files if the subdirectory does not resolve them. Acceptance: the built
   `kwe-manager.qmltypes` lists all five, and a manager run logs zero
   `ReferenceError`.
2. **Trigger the enumeration on creation, not only on a visibility edge.**
   Call `listOutputs()` from `Component.onCompleted` when already visible (or
   drive it from `WallpaperSelection` / an `ApplyClient` self-refresh), and
   give the `busy` skip a rescheduler so a request dropped during retry
   backoff is retried when the lane goes idle.
3. **Make the silent client paths speak.**
   - `applyclient.cpp:374-388` — an `ok:true` response with an empty (or
     missing) `outputs` array returns early with no signal and no message;
     it must set a distinguishable "the service reports no outputs" state.
   - `applyclient.cpp:92-97` — `resetStatus()` calls `setErrorMessage({})`
     unconditionally, so a selection change (`WallpaperDetail.qml:65`) or a
     combo index change (`:213`) erases a failure message that may still be
     in flight, leaving `Failed` with empty text.
   - No per-request timeout exists (only the reconnect timer,
     `applyclient.h:136`): a daemon that accepts the connection and never
     writes a newline leaves the client `ListingOutputs`/busy forever.
4. **Packaging.** `packaging/PKGBUILD:40-46` `depends=()` lists neither
   `qt6-tools` (`qdbus6`) nor `kscreen` (`kscreen-doctor`). Both happen to be
   installed here; on a clean machine the apply lane fails with
   `shell_unreachable` — and, per defect 2, shows nothing.

## Why no gate caught this

- **`scripts/check.sh:14-19` lints with the wrong binary.** `/usr/bin/qmllint`
  is `qmllint 1.0` from **qt5-declarative**; it cannot resolve a single Qt 6
  type and exits 0 on everything. The Qt 6 linter is not on `PATH`
  (`/usr/lib/qt6/bin/qmllint`, 6.11.1) and flags this bug today:
  165 `[unqualified]` warnings across the manager QML, including
  `WallpaperDetail.qml:222:41` on `ApplyClient.ListingOutputs`.
  → Pin the Qt 6 binary and fail on `[unqualified]`.
- **Nothing asserts the QML module actually registers its C++ types.**
  → Assert the built `kwe-manager.qmltypes` is not `Module {}`.
- **`scripts/smoke-ui.sh` passes while every binding throws** (the same gap
  recorded for the Qt 6.11 delegate fix: the smoke checks only `Ready` +
  item count, and QML errors are merely logged).
  → Fail the UI smoke on any `ReferenceError` in the manager's stderr.
- **The daemon logs nothing on the enumeration path** (`apply.rs` has one
  unrelated `eprintln!`), and the manager has no logging categories at all,
  so neither side leaves evidence. The two `event=api.client_error detail=request
  ended without a newline` entries per session are unrelated noise:
  `daemonactivator.cpp:17-21` connects and disconnects without writing, and
  `1833331` made the daemon reject a request with no trailing newline.

## Evidence trail

| Probe | Result |
|---|---|
| `kwe daemon-call … wallpaper.outputs` | `ok:true`, `DP-1` with full metadata |
| Socket relay, packaged manager, 30 s | `catalog`, `renderer.status` only — no `wallpaper.outputs` |
| Instrumented build, `onVisibleChanged` trace | visible instance completes `visible=true`; handler never runs |
| `kwe-manager.qmltypes` | `Module {}` |
| Qt 6.11 `qmllint` | 165 `[unqualified]`, incl. `ApplyClient.ListingOutputs` |
| `journalctl --user -u "app-org.kde.kwe@*"` | 2060 `ReferenceError` in 6 h |
