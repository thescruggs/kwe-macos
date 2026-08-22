# Bug: no display outputs enumerated after a reboot

- **Reported:** 2026-08-22 (user report: "the last fix seems to break on
  reboot, after a reboot there is no display outputs enumerated")
- **Severity:** High user-facing — Apply is unreachable again on every fresh
  boot, which is the only state a normal user ever starts from.
- **Status:** DIAGNOSED, root cause confirmed live. Not yet fixed.
- **Relation to `APPLY_NO_OUTPUTS.md`:** this is a *different* defect that the
  earlier fix could not have caught. `a747064` fixed the manager side (QML
  type registration + the enumeration trigger); this one is on the daemon
  side and only appears when the daemon is started by systemd at boot.

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

## Fix directions (to be decided at fix time)

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
