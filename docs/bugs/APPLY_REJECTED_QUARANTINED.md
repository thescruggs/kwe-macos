# Bug: Apply fails with "renderer rejected the start (quarantined)"

- **Reported:** 2026-08-22 (user: "When I try to apply a scene I get this
  error: Applying failed: renderer rejected the start (quarantined)")
- **Severity:** High user-facing — the wallpaper cannot be applied, the
  message names no cause, and nothing in the UI can clear it. On this
  machine it hits every web wallpaper and the one scene that was tried
  under the stale daemon.
- **Status:** DIAGNOSED 2026-08-22, three causes confirmed by measurement
  (below), fix planned as B4 in `AI-Skills/BETA_PLAN.md`. Not fixed.

## Symptom

`wallpaper.apply` returns `apply_failed` with detail
`renderer rejected the start (quarantined)`; the manager prints it verbatim
(`apps/kwe-manager/src/applyclient.cpp:510`). The previous wallpaper stays.

## Evidence

`~/.local/state/kwe/supervisor-v1.json` holds three quarantined records
(`failures: 3`, `max_failures` default 3):

| id | kind | last_detail (trimmed) | updated |
|---|---|---|---|
| 1652229298 "[E] Rainbow stars" | scene | `exit_code_74 … renderer.scene.model_layer_skip count=2 … renderer.scene.no_drawable_content objects=3 model_layers=2 particle_systems=1 layers=0` | 16:07 |
| 1747779570 "2AM Cyberpunk City" | web | `exit_code_73 … renderer.web.backend_reject detail=browser bootstrap failed; chromium stderr tail:` (empty) | 16:06 |
| 2646399969 "All-in-one Raindrops Night" | web | same as above | 12:09 |

Journal (`kwe-daemon[2888]`): `event=renderer.quarantined` for each id; each
web attempt is preceded by `systemd-coredump: Process N (chromium) … SIGTRAP
si_code SI_KERNEL` — 10 chromium coredumps today, none before today
(30-day window). The crashing processes are the browser main process (our
argv) and its `--type=zygote` child.

## Root causes (three, layered)

### 1. The unit's `TasksMax=96` is too small for chromium 151.0.7922.173 (web lane)

- `packaging/systemd/kwe-daemon.service:57` — `TasksMax=96` ("64 was too
  tight for the beta").
- chromium was upgraded 2026-08-22 11:33 (`151.0.7922.137 → .173`); the
  first crash is 12:08. No other relevant change in the window.
- Measured with `kwe-web-renderer --probe` (spawns the real sandboxed
  browser with the production argv) inside transient units:
  - `systemd-run -p TasksMax=96 … --probe` → `backend_reject exit_code=73`
    (also with the unit's full hardening set).
  - `TasksMax=128`, `160`, `192`, `256`, `512`, `infinity` → probe passes.
  - One probe alone peaks at ≥53 tasks (sampled at 200 ms, so the true
    startup peak is higher); in the real unit the daemon (5), the audio
    worker and the outgoing renderer share the same 96.
- Ruled out: `RLIMIT_AS` (web kind already gets 131072 MiB; the browser
  needs >96 GiB and <112 GiB today), `RLIMIT_NPROC`, `NOFILE=1024`,
  `FSIZE`, `CORE=0`, stripped env (`env -i … --probe` passes), the bwrap
  namespace (both reported pages `--dump-dom` fine inside the production
  bwrap argv), and the content itself.
- Each attempt dies at bootstrap (`find_page_target` never sees a target)
  → exit 73 → `ProcessExit` failure → three strikes → quarantine.

### 2. The running daemon is the release-4 binary; the renderers are release 5 (scene lane)

- `systemctl --user show kwe-daemon -p ExecMainStartTimestamp` →
  14:27:13; `pacman.log` → `kde-wallpaper-engine -4 → -5` at 15:56:16.
  Nothing restarted the unit (`post_upgrade` only prints a message).
- The `-5` daemon refuses scene 1652229298 at preflight with a named reason
  (`kwe preflight --path …/1652229298/scene.pkg` → `"scene draws nothing in
  this build: 2 model layer(s) need scene3d …; 1 particle system(s)
  reference external particle files …"`, the B2 contract). The `-4` daemon
  has no such check, so it spawned the `-5` `kwe-scene-renderer`, which
  applied the same rule itself and exited 74 (by design, B2 "worker
  re-checks"). Three times → quarantine.
- This is B1's sibling: B1 required re-enabling the unit after upgrade;
  here the upgrade requires a restart and nothing performs or enforces it.
  Any daemon/renderer version skew reproduces some variant of this.

### 3. Quarantine policy and UX turn both into a dead end

- `crates/kwe-daemon/src/supervisor.rs:1578` — every `FailureKind`
  counts toward `max_failures`, including the worker's deliberate
  refusals (73 `backend_reject`, 74 `no_drawable_content`). A refusal is
  "this environment/build cannot run this content", not "this content
  crashed"; retrying it twice more then banning the content is wrong on
  both counts.
- `supervisor.rs:335` — the record identity is `id:hash:kind`. It carries
  no renderer/daemon build identity, so a quarantine earned under one
  build survives an upgrade that fixes the cause (M3h will ship scene3d and
  1652229298 stays banned; the TasksMax fix ships and both web ids stay
  banned). Records never expire.
- `crates/kwe-daemon/src/apply.rs:1670-1676` — `complete_apply` maps
  `Quarantined`/`RolledBack` to the bare phase name and drops
  `WorkerStatus.last_failure_detail`, which holds the actual reason.
- `wallpaper.apply` always issues `ControlCommand::Start`;
  `renderer.retry` (`main.rs:839`, `start_selected(spec, true)`) is the
  only path that clears a record and no client calls it. The manager has
  no retry/clear affordance.
- `crates/kwe-web-renderer/src/main.rs:620-632` — the stderr ring is only
  drained after `BrowserSession` is built, so a bootstrap failure always
  reports `chromium stderr tail:` empty. The one diagnostic that would
  have named cause 1 is structurally blank.

## Observation filed separately

Inside the production bwrap argv (`--disable-gpu`, headless) wallpaper
2646399969 renders its `<p class="nosupport">Sorry, but your browser does
not support WebGL!</p>` branch. WebGL availability in the web lane is a
compatibility question, not part of this bug.

## Workaround (manual, until B4 lands)

1. `systemctl --user edit kwe-daemon` → `[Service]` `TasksMax=512`
   (drop-in), then `systemctl --user daemon-reload`.
2. `systemctl --user restart kwe-daemon` (also picks up the `-5` binary).
3. Clear the records: stop the unit, remove the three entries from
   `~/.local/state/kwe/supervisor-v1.json` (or the file — last-good frames
   and `forced_kill_count` are the only other contents), start the unit.
   Alternatively `kwe daemon-call --method renderer.retry` with the start
   spec, which clears only that identity.

## Fix (planned — B4)

See `AI-Skills/BETA_PLAN.md` "Open work queue" B4 for the slices and
acceptance tests.
