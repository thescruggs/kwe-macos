# Bug: no display outputs enumerated after a reboot

- **Reported:** 2026-08-22 (user report: "the last fix seems to break on
  reboot, after a reboot there is no display outputs enumerated")
- **Severity:** High user-facing — Apply is unreachable again on every fresh
  boot, which is the only state a normal user ever starts from.
- **Status:** FIXED on branch `beta-b1-display-env` (`3d29272` fix + `1d332a9`
  review fixes), ff-merged into the trunk `fix/qt611-gallery-delegates`.
  Verified against the real session — see "Fix" below. The reboot-path
  confirmation is the maintainer's, on their own schedule.
- **Relation to `APPLY_NO_OUTPUTS.md`:** this is a *different* defect that the
  earlier fix could not have caught. `a747064` fixed the manager side (QML
  type registration + the enumeration trigger); this one is on the daemon
  side and only appears when the daemon is started by systemd at boot.

## Fix (2026-08-22)

Two independent fixes for the same failure — the first makes existing installs
and hand-started daemons work with no user action, the second makes the boot
path correct by construction.

1. **Lazy display-environment recovery
   (`crates/kwe-daemon/src/apply.rs`).** When the daemon's own environment
   names no display, the enumeration probe recovers one from
   `systemctl --user show-environment` — the same environment a restart of the
   unit would have inherited — and passes it to the `kscreen-doctor` child.
   Resolution is lazy and per call, mirroring the documented `resolve_qdbus`
   contract: the daemon may legitimately start before any session exists, and
   enumeration only ever runs once somebody is logged in and asking. Only
   *successful* recoveries are cached, and a child that fails while using one
   drops it, so a session that restarted under a different display self-heals.
   Recovered values are validated (bounded length, printable ASCII, no
   whitespace or quoting characters) before they reach a child. The recovery
   child is bounded to `min(probe timeout, 1500 ms)`: it sits on a path that
   already spends the probe deadline twice, answering a manager request with
   its own 10 s deadline.

   `evaluate_script` deliberately does not use any of this — `qdbus` is a
   `QCoreApplication` and reaches plasmashell over the session bus with no
   display at all. Only the KScreen enumeration needs one.

   Two measurements are recorded in the code so they are not re-litigated:
   `WAYLAND_DISPLAY` alone is sufficient, and `QT_QPA_PLATFORM=offscreen` must
   never be substituted for a real display — `kscreen-doctor` then **hangs**
   until killed instead of aborting, turning a fast clear failure into a probe
   timeout.

   New `--systemctl-binary` flag (default `systemctl` on PATH), matching the
   existing `--qdbus-binary` / `--kscreen-doctor-binary` so tests and smokes
   can stub the recovery.

2. **Unit ordering (`packaging/systemd/kwe-daemon.service`).**
   `PartOf=graphical-session.target`, `After=graphical-session.target`, and
   `WantedBy=graphical-session.target` in place of `default.target` — the
   pattern `systemd.special(7)` prescribes for session-scoped services, and
   what every Plasma unit on this machine uses. The daemon now starts with a
   display in reach and stops with the session it belongs to. `Restart=always`
   stays, with its rationale corrected: session teardown is `PartOf=`'s job
   now, and Restart is only about crashes.

   **Existing installs must re-enable the unit once** — the old symlink lives
   in the user's own `~/.config/systemd/user/default.target.wants/` and keeps
   starting the daemon too early:

   ```sh
   systemctl --user disable kwe-daemon.service
   systemctl --user enable --now kwe-daemon.service
   ```

   Said in both places that speak to users: the README and the pacman
   `post_upgrade` hook.

3. **An honest error (`apply.rs`, `apps/kwe-manager/src/applyclient.cpp`).**
   When no display can be recovered at all, `wallpaper.outputs` now answers
   with a new `display_unavailable` code carrying the restart the user can
   run, instead of the generic `shell_unreachable` behind an empty picker.
   The manager maps it to that message; unknown codes already degrade to
   `code: detail`, so the addition is safe in both directions. Recorded in
   `docs/SUPERVISOR_API_V1.md`.

## Gates (2026-08-22)

- **Nine unit tests** over the resolver: `show-environment` parsing (real
  shape, systemd's quoting, absolute socket paths), rejection of empty,
  whitespace-bearing, quote-bearing, control and over-length values, the
  inherit short-circuit, successful-only caching, cache invalidation after a
  failed child, the recovered value actually reaching the child's environment,
  the bounded recovery budget, and `display_unavailable` surviving the mapping
  to the wire while every other probe error stays `shell_unreachable`.
- **A stripped-environment case in `scripts/smoke-apply.sh`** (the read-only
  live lane): the daemon is started under `env -i` with only what the unit
  provides — `HOME`, `PATH`, `XDG_RUNTIME_DIR`, `DBUS_SESSION_BUS_ADDRESS` —
  and must enumerate the real connector. **This case reproduces B1 without a
  reboot**, confirmed by running it against the pre-fix binary: it answers
  `shell_unreachable` / `kscreen-doctor exited signal: 6 (SIGABRT)`. A
  negative control in the same lane stubs the recovery to a session with no
  display and asserts `display_unavailable` with an actionable detail.
- **A unit-file assertion in `scripts/check.sh`**, since the ordering half has
  no runtime test: the unit must keep all three graphical-session directives
  and must not reinstall into `default.target`. Verified both ways — green on
  the fixed unit, and it names the missing directive on a reverted one.
- `./scripts/check.sh` exit 0 (fmt/clippy/137 daemon tests/workspace build/
  CMake/qml-typecheck/diagnose), `ctest` 8/8, `KWE_LIVE_APPLY=1
  smoke-apply.sh` green including both new cases.

Also fixed in passing: exec'ing a just-written stub races other test threads'
forks and fails with `ETXTBSY`. This surfaced in the *pre-existing*
`external_evaluator_runs_the_script_as_its_single_argument` test once the new
tests added spawn traffic; stub creation now waits that window out, and the
old test uses the same helper.

## Still open

The reboot itself. Everything above is verified on the running session and by
a case that reproduces the boot environment, but the real boot path is proven
only by booting. After the next reboot, `wallpaper.outputs` should return the
connector with no manual restart, and
`systemctl --user show -p ActiveEnterTimestamp kwe-daemon.service` should land
after `graphical-session.target`.

## Symptom

After a reboot the manager's output picker is empty again. The daemon answers
the enumeration RPC, but with an error:

```
$ kwe daemon-call --socket $XDG_RUNTIME_DIR/kwe/daemon-v1.sock --method wallpaper.outputs
{"ok": false, "result": {"detail": "kscreen-doctor exited signal: 6 (SIGABRT) (core dumped)",
                         "error": "shell_unreachable"}}
```

## Root cause (confirmed)

`QdbusShellProbe::system_outputs` (`crates/kwe-daemon/src/apply.rs:685`) shells
out to `kscreen-doctor -o`, inheriting the daemon's environment.
`packaging/systemd/kwe-daemon.service` is `WantedBy=default.target` with no
ordering against the graphical session, so at boot the unit starts **before**
Plasma imports the session environment into the systemd user manager. The
daemon process therefore has `DBUS_SESSION_BUS_ADDRESS` and `XDG_RUNTIME_DIR`
but **no `WAYLAND_DISPLAY` and no `QT_QPA_PLATFORM`**. `kscreen-doctor` is a Qt
program: with no Wayland display in the environment it falls back to the `xcb`
plugin, fails to load it, and aborts.

Evidence, all from the live machine on 2026-08-22:

```
# the daemon started at boot (PID 1644, 12:10:35) — env has no display
$ tr '\0' '\n' < /proc/1644/environ | grep -Ei 'WAYLAND|DISPLAY|QT_QPA'
(nothing)

# but the user manager does have it — Plasma imported it at login, after the unit started
$ systemctl --user show-environment | grep -E 'WAYLAND_DISPLAY|XDG_SESSION_TYPE'
WAYLAND_DISPLAY=wayland-0
XDG_SESSION_TYPE=wayland

# journal for the boot-started daemon
kscreen-doctor[10614]: could not connect to display
kscreen-doctor[10614]: Could not load the Qt platform plugin "xcb" in "" even though it was found.
systemd-coredump[10618]: Process 10614 (kscreen-doctor) terminated abnormally

# a restart AFTER login inherits the imported env and enumeration works
$ systemctl --user restart kwe-daemon.service && kwe daemon-call ... --method wallpaper.outputs
{"ok": true, "result": {"outputs": [{"name": "DP-1", "desktop_id": 111, "enabled": true, ...}]}}
```

That restart is the current workaround: `systemctl --user restart kwe-daemon`
after logging in.

## Why the gates missed it

Every apply/enumeration test to date ran against a daemon started from a
terminal inside the logged-in session (`smoke-apply.sh`, `smoke-live-apply.sh`,
and the manual verification of `a747064`), which inherits a full session
environment. The boot-ordering path — systemd starting the unit before the
session env import — has never been exercised. `smoke-playlist-restart.sh`
substitutes a fake `kscreen-doctor`, so it cannot see this either.

## Fix directions considered (kept for the record)

1. **Unit ordering (primary).** Make `kwe-daemon.service` part of the
   graphical session: `After=graphical-session.target`,
   `PartOf=graphical-session.target`, `WantedBy=graphical-session.target`
   instead of `default.target`. Then the unit starts after Plasma's
   `import-environment`, and it stops with the session too. Needs verification
   that the KDE session actually reaches `graphical-session.target` on this
   machine before Plasma imports the environment.
2. **Fail soft, not silent (independent of 1).** `shell_unreachable` from a
   probe that aborted for want of a display should map to an actionable
   message in the manager, not an empty picker with no explanation — the
   `outputsListed` work in `a747064` gives it somewhere to land.
3. **Do not depend on the boot-time environment.** Options: resolve
   `WAYLAND_DISPLAY` from the systemd user manager at probe time, or replace
   the `kscreen-doctor` shell-out with the KScreen D-Bus interface, which is
   reachable over the session bus the daemon already has. Prefer this if (1)
   proves fragile across display managers.
4. **A gate that would have caught it.** A smoke case that runs enumeration
   with a stripped environment (`env -i` plus only what the unit provides) and
   asserts an actionable outcome, plus a reboot-path assertion in
   `smoke-live-apply.sh`.
