# Bug: Gallery lists no wallpapers when the user daemon is not running

- **Reported:** 2026-08-19 (user regression report, post test-build install)
- **Severity:** High user-facing (empty gallery reads as total data loss)
- **Status:** OPEN → fix on branch `beta-fix-daemon-activation`

## Symptom

kwe-manager opens with an empty installed gallery. No wallpapers listed,
no crash. The alpha package (same manager code) listed all 92 projects
yesterday.

## Root cause (verified)

1. `systemctl --user status kwe-daemon` → **inactive (dead) since 02:59**
   (clean SIGTERM at session teardown; 7h41m uptime before that).
2. `packaging/systemd/kwe-daemon.service` has `Restart=on-failure` — a clean
   stop is not a failure, so systemd did not bring it back; the unit did not
   come up again on the next session in this case.
3. `apps/kwe-manager/src/main.cpp` contains **no daemon activation path**
   (no spawn, no systemctl, no D-Bus activation) — the manager only connects
   to the existing socket. With the daemon down the catalog request fails
   and the gallery is empty.
4. Not a code regression in the catalog pipeline: `systemctl --user start
   kwe-daemon` → "kwe-daemon ready: 92 projects" and `scripts/smoke-ui.sh`
   passes against it (manager reaches Ready with totalCount > 0).

## Evidence

```
$ systemctl --user status kwe-daemon
○ kwe-daemon.service ... Active: inactive (dead) since Wed 2026-08-19 02:59:28 CDT
$ systemctl --user start kwe-daemon && ... status
● Active: active (running) ... kwe-daemon ready: 92 projects; socket /run/user/1000/kwe/daemon-v1.sock
$ ./scripts/smoke-ui.sh   # offscreen manager + fresh daemon
kwe-daemon ready: 92 projects   # exit 0 (manager Ready + totalCount > 0)
```

## Required fix

1. **Manager activation:** on startup, if the daemon socket is absent,
   activate the user service (prefer `systemctl --user start kwe-daemon`
   via bounded QProcess; graceful fallback when systemd is unavailable —
   no error storm, visible actionable state in the UI).
2. **Unit hardening:** `Restart=always` (StartLimitBurst/IntervalSec already
   bound restart loops; explicit stops still win) so the daemon survives
   clean stops too.
3. Regression coverage: extend `scripts/smoke-ui.sh` with a daemon-down
   case — start the manager with no daemon running, assert the daemon gets
   activated and the gallery reaches Ready within a bounded time.

## Acceptance

- Unit tests for the activation decision logic (socket probe → activate
  once → bounded retries → give up with actionable error).
- smoke-ui daemon-down case green; existing smoke suites unaffected.
- Independent adversarial review of the diff (per AGENTS.md).
