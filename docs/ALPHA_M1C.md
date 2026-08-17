# Alpha M1c transactional canary

M1c prevents a candidate wallpaper from replacing a healthy active renderer
until the candidate produces advancing frames for a bounded canary interval.
The frame renderer and mmap protocol remain unchanged, and no Plasma plugin is
installed.

## Transaction

1. Keep the active worker and published display generation unchanged.
2. Start one candidate with its own frame mapping and failure budget.
3. Require at least three advancing frames across the configured canary time.
4. On failure, stop only the candidate, retry within its budget, and quarantine
   it without disturbing the active worker.
5. On success, publish a new monotonic display generation while retaining the
   previous worker and mapping.
6. Commit the new static fallback and stop the previous worker only after the
   matching `renderer.ack`, or after the bounded handoff timeout.
7. If the promoted worker fails before acknowledgement, restore the previous
   worker, publish another monotonic generation, and charge the failed identity.

At most one active, one candidate, and one retired handoff worker can exist.
Supervised mappings are additionally limited to 128 MiB each.

## Acceptance evidence

Validated on 2026-08-16 with `scripts/smoke-supervisor.sh`:

| Case | Result |
|---|---|
| failing canary | active PID, frame path, and display generation remained unchanged through quarantine |
| healthy replacement | new generation published; previous PID and mapping remained until acknowledgement |
| stale acknowledgement | rejected without stopping either worker |
| matching acknowledgement | fallback committed atomically and previous process reaped |
| post-promotion/pre-ack exit | previous worker restored and previous static fallback retained |
| missing acknowledgement | bounded timeout committed the healthy promotion and reaped the previous worker |
| candidate retry backoff | active watchdog continued running during the delay |

All earlier startup, hang, corruption, exit, forced-kill, parent-death, retry,
and persistent-quarantine cases remain in the same suite.

## Remaining M1 work

- package the daemon as a hardened systemd user service and enforce memory,
  CPU, process, and log budgets;
- add a controlled memory-pressure/OOM fault lane;
- define normalized pointer messages and preserve Plasma desktop gestures;
- build the minimal Plasma 6 display bridge and connect its mapping swap to
  `display_generation`/`renderer.ack`;
- run destructive desktop tests while asserting that the `plasmashell` PID and
  desktop interactions remain unchanged.

