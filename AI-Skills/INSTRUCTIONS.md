# AI-Skills — Standing Instructions for All Agents

**Read this file at the start of every session in this repository, before doing any work.**
It is the human maintainer's operating directive for multi-agent collaboration on this project.

## The directive (verbatim, from the maintainer, 2026-08-18)

> This project will be worked on by multiple types of AI agents. Create an
> AI-Skills folder and save project memory, most recent plan and any other
> info that may be useful for another agent to utilize. Claude acts as the
> orchestrator and uses less costly sub-agents to complete work. Mark up the
> plan as you go and document any changes you find during the process to the
> plan.

## Operational rules

1. **Orchestration.** Claude (or whichever orchestrating agent owns a session)
   decomposes work and delegates implementation to less costly sub-agents.
   The orchestrator: keeps the plan current, reviews sub-agent output, runs
   verification, and commits. Sub-agents: implement one well-specified task
   each, never two agents on the same files (AGENTS.md rule).

2. **The plan is a living document.** `AI-Skills/BETA_PLAN.md` is the canonical
   working plan. As work progresses:
   - Mark milestone/sub-slice status (`pending → in_progress → done`) inline.
   - Record every deviation, discovered gap, design change, or new fact in the
     **## Change log** section at the top of the plan, dated, with the reason.
   - If a plan decision turns out wrong, strike it and record the replacement —
     never silently rewrite history.
   - At the end of each work session, update `AI-Skills/PROJECT_MEMORY.md`
     (session log + current state) so the next agent resumes without re-research.

3. **Project memory.** `AI-Skills/PROJECT_MEMORY.md` holds everything an agent
   new to this repo needs: current branch/commit state, build & test commands,
   iron rules, conventions, and a session log. Update it before finishing any
   session that changed repo state.

4. **Repo-wide rules still bind.** AGENTS.md (work decomposition, adversarial
   review as a separate pass, provenance ledger before dependent code merges,
   synthetic fixtures only, bounded everything) and the docs/ contracts take
   precedence over convenience. AI-Skills never overrides them; it operationalizes.

5. **Handoff hygiene.** Every task handoff answers: what was done, what passed,
   what is open, what changed in the plan. Commits carry that in the message;
   the session log carries the rest.

## File map (this folder)

| File | Purpose | Updated when |
|---|---|---|
| `INSTRUCTIONS.md` | This directive (read first) | Maintainer changes it |
| `PROJECT_MEMORY.md` | Repo state, commands, conventions, session log | Every session |
| `BETA_PLAN.md` | Living beta plan with change log | Every plan change |
| `TASK_TEMPLATE.md` | Issue-sized task contract template (per AGENTS.md) | Rarely |

## How this gets read every session

- AGENTS.md (repo root, auto-loaded by agent tools) points here.
- Orchestrator memory (Claude Code user memory) also points here.
- If your agent tool loads neither: read AGENTS.md first, then this file.
