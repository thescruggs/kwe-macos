# Task contract template (per AGENTS.md "Work decomposition")

Copy and fill in for every issue-sized task delegated to a sub-agent or shared with another AI.

```text
Task:            <one line: what the task accomplishes>
Milestone/Slice: <e.g. BETA_M1a>
Goal:            <goal and user-visible outcome>
Outcome:         <what "done" looks like, including docs/evidence files to update>
In scope:        <files/modules the task MAY change>
Out of scope:    <explicitly excluded, esp. live Plasma session unless authorized>
Acceptance tests:        <commands + expected results>
Failure/recovery tests:  <fault cases and required behavior>
Upstream/provenance:     <allowed upstream sources + use type; THIRD_PARTY entries required>
Commands run and results:<filled at handoff>
Open risks:              <filled at handoff>
Commit(s):               <filled at handoff>
```

Rules for sub-agents:
- Work in a separate git worktree/branch for the task; never share uncommitted state with another agent.
- Match existing code style (comments, naming, error handling) — see the file(s) named in "In scope".
- Bound everything: queues, allocations, retries, waits, log volume. Add a synthetic regression fixture for every parser/crash fix.
- UI changes: Kirigami + standard components + icon names; accessibility (text/icon, never color alone); loading/success/empty/offline/failure states for async ops.
- Do not restart or modify the live Plasma session unless the task explicitly authorizes it.
- Handoff = this template filled in + `./scripts/check.sh` green + relevant smoke suites run.
