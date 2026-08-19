# Bug: Gallery lists no wallpapers when the user daemon is not running

- **Reported:** 2026-08-19 (user regression report, post test-build install)
- **Severity:** High user-facing (empty gallery reads as total data loss)
- **Status:** FIXED (commit `f9875a73c3603d35a983c6cd443322e109ebe449`, branch `beta-fix-daemon-activation`)

## Fix (2026-08-19)

1. **Manager activation (`apps/kwe-manager/src/daemonactivator.{h,cpp}`).**
   On startup the manager probes the daemon socket
   (`$XDG_RUNTIME_DIR/kwe/daemon-v1.sock`, same resolution as before, now
   passed to a new `DaemonActivator`): if the socket is present it proceeds
   as today; if absent it runs the activation command through a bounded
   `QProcess` — `systemctl --user start kwe-daemon` by default — 10 s
   timeout per attempt, SIGTERM then SIGKILL after a 1 s grace, no shell,
   max 3 attempts with 1 s → 2 s backoff, then a `Failed` state with the
   actionable message "The background service is not running. Run
   `systemctl --user start kwe-daemon`." One activation cycle per manager
   run; the QML retry action (`Start service`) re-invokes `activate()`.
   The manager never spawns kwe-daemon directly — daemon lifecycle stays
   with systemd. The spawned command inherits the manager environment plus
   `KWE_DAEMON_SOCKET`, so tests/smoke can stub the activation. The
   activation command is injectable via a new `--daemon-activation-command
   <path>` manager CLI flag (default: systemctl as above).
   The catalog load and the activation compose: `DaemonActivator::activated`
   is emitted once the socket is observed, and main.cpp refreshes the
   catalog immediately instead of waiting for the client's exponential
   retry backoff. The manager UI never blocks — the existing
   Error/Loading states show while activation runs, plus two Kirigami
   InlineMessages in GalleryPage: Information while `Activating`, Error
   with the retry action when `Failed`. The stale catalog hint that told
   users to run `systemctl --user enable --now` was replaced by a neutral
   "service is not running" note (DaemonActivator owns the guidance now).
2. **Unit hardening (`packaging/systemd/kwe-daemon.service`).**
   `Restart=on-failure` → `Restart=always` with a comment: clean stops
   (logout teardown) must not leave the desktop without its service;
   `StartLimitBurst=5`/`StartLimitIntervalSec=30s` bound restart loops, and
   an explicit `systemctl --user stop kwe-daemon` still wins. All other
   hardening directives unchanged.
3. **Regression coverage.**
   - `scripts/smoke-ui.sh` now runs a daemon-down case first: no daemon on
     the smoke socket, manager launched offscreen with
     `--smoke-test-ms 3000 --daemon-activation-command <stub>`; the stub
     plays the systemctl role against a smoke daemon (never the user's real
     unit) and records its pid. Asserts the manager exits 0 (catalog Ready
     with totalCount > 0), the daemon socket appeared, and the daemon is
     alive. The pre-running-daemon case runs unchanged afterwards.
   - New C++ test target `kwe-daemon-activator-test` (same style as
     `kwe-workshop-client-test`): activate-when-absent (exactly one
     invocation, socket path delivered via `KWE_DAEMON_SOCKET`),
     no-activate-when-present, bounded attempts then actionable failure,
     QProcess timeout kill (child proven dead), manual retry after failure.
     The tests use stub scripts — no systemctl needed.
   - Evidence: `scripts/smoke-ui.sh` exits 0 with both cases; ctest
     includes the new test; cargo fmt/clippy/test and qmllint green
     (see acceptance run below).

## Symptom

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
