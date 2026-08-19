# AI contributor workflow

## Mission and non-negotiable rule

Build a resilient KDE Plasma 6 Wallpaper Engine-compatible experience for
Arch/CachyOS. No wallpaper parser, renderer, browser, Steam SDK, or audio
processor may execute in `plasmashell`.

Read `docs/PROJECT_PLAN.md`, `docs/ARCHITECTURE.md`, `docs/UX_DESIGN.md`,
`docs/FEATURE_COMPATIBILITY.md`, and `docs/PROVENANCE.md` before changing
architecture, UI, compatibility behavior, or dependencies.

## Multi-agent coordination

Read `AI-Skills/INSTRUCTIONS.md` before starting any work — it holds the
maintainer's standing directive for multi-agent sessions (orchestrator uses
less costly sub-agents; the plan is a living document; mark it up and log
changes as you go). `AI-Skills/PROJECT_MEMORY.md` carries current repo state
and the session log; `AI-Skills/BETA_PLAN.md` is the canonical working plan;
`AI-Skills/TASK_TEMPLATE.md` is the task-contract form for every delegated task.

## Work decomposition

Every AI task must be an issue-sized vertical change with:

- goal and user-visible outcome;
- acceptance and explicit failure criteria;
- files/modules it may change;
- commands/tests required;
- relevant ADR/protocol versions;
- upstream sources that may be consulted and their allowed use type;
- recovery and compatibility impact.
- UI states and accessibility acceptance criteria for user-facing work;
- feature-capability IDs affected and their parity-test evidence.

Do not ask two agents to edit the same files. Use a separate Git worktree and
branch per agent/task. Integrate through reviewed commits, not shared uncommitted
state.

## Suggested roles

1. Mapper/planner: traces current ownership and writes the task contract.
2. Implementer: makes the smallest change and tests it locally.
3. Adversarial reviewer: looks for Plasma coupling, hangs, unbounded resources,
   unsafe paths, licensing omissions, and missing rollback.
4. Integration tester: runs fault injection and records hardware/backend facts.
5. Human maintainer: approves protocol, dependency, license, and release changes.

One agent can fill several roles, but implementation and adversarial review
should be separate passes.

## Handoff template

```text
Task:
Outcome:
In scope:
Out of scope:
Files/modules:
Acceptance tests:
Failure/recovery tests:
Upstream/provenance constraints:
Commands run and results:
Open risks:
Commit(s):
```

## Engineering rules

- Preserve process boundaries; do not move work into the Plasma plugin for
  convenience or performance.
- Prefer versioned, testable messages over shared internal data structures.
- Bound queues, allocations, retries, subprocess waits, log volume, and frame
  dimensions.
- Parse untrusted metadata without exceptions escaping a service boundary.
- Use Kirigami, Qt Quick Controls, KDE icon names, system fonts, system colors,
  and standard actions. Do not hard-code a custom visual theme into core flows.
- Every user-facing asynchronous operation needs loading, success, empty,
  offline, canceled, degraded, and actionable failure behavior as applicable.
- Do not communicate compatibility using color alone; use text and icons, and
  explain renderer-dependent limitations before Apply.
- Preserve unknown Wallpaper Engine properties and metadata when editing values.
- A compatibility claim requires a synthetic fixture and automated test; use
  `partial` or `renderer-dependent` when parity has not been demonstrated.
- Add a synthetic regression fixture for every parser or crash fix.
- Never use a real Workshop payload as a committed fixture.
- Update `THIRD_PARTY.yml` and nearby `Borrowed-From` comments as required by
  `docs/PROVENANCE.md`.
- Run format, lint, unit, integration, and relevant fault-injection tests before
  handoff. Report skipped tests explicitly.
- Do not restart or modify the user's live Plasma session unless the task
  explicitly authorizes an interactive system test.

## Recommended AI loop

1. Create or select one milestone issue.
2. Ask the mapping agent to identify boundaries and tests; use Graphify after
   the repository contains code to keep the architecture map current.
3. Human approves the issue contract for dependencies, protocols, or destructive
   live-desktop tests.
4. Implement in a worktree with targeted tests.
5. Run a separate review prompt using the diff plus architecture, UX,
   compatibility, and provenance docs.
6. Run integration/fault tests, attach logs and compatibility records.
7. Merge only when recovery behavior and provenance are documented.
