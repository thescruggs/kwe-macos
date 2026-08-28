# SR-0 — Reproducible baseline and feature inventory (decomposition)

Parent epic: `docs/Scene-Rendering-Plan.md` §8 SR-0. Approved 2026-08-27.
Child order: SR-0a → SR-0b → SR-0c → SR-0d. Each child is one mergeable slice
with its own implementation and adversarial-review passes.

## SR-0a — Scene capability taxonomy and inventory schema (docs only)

```text
Task:            Draft the scene sub-capability taxonomy and the inventory record schema.
Milestone/Slice: SR-0a
Goal:            One authoritative draft naming every scene.* capability ID, its parent
                 public ID, definition, and evidence requirement, plus the JSON shape a
                 scene feature inventory emits — so SR-0b–d and SR-1 code against named
                 IDs instead of ad hoc strings.
Outcome:         docs/SCENE_CAPABILITIES.md (taxonomy draft v0, all rows
                 experimental/planned, no support claims); inventory record draft schema
                 in the same file; PROJECT_MEMORY log row. SR-1 approves exact names and
                 schema version before any code uses them.
In scope:        docs/SCENE_CAPABILITIES.md, docs/SR0.md, AI-Skills/PROJECT_MEMORY.md.
Out of scope:    Any code, FEATURE_COMPATIBILITY row changes, renderer/daemon/manager
                 behavior, corpus runs.
Acceptance tests:        doc lists every ID from plan §5.1 plus the Wave C/D IDs
                         (.animation, .material3d, .fog, .reflection); every ID has
                         parent, definition, evidence column; naming/stability rules
                         stated; inventory schema covers unknown-field counting and
                         per-item bounds.
Failure/recovery tests:  n/a (docs).
Upstream/provenance:     Official Wallpaper Engine docs for semantic names only; no code.
Commands run and results:none required (docs only); markdown reviewed by hand.
Open risks:              ID names are draft until the SR-1 freeze; texture-family
                         membership (`scene.texture.compressed`) may be folded
                         into `.texv` if inspection shows no independent use.
Commit(s):               <filled after commit; same commit as this file>
```

## SR-0b — Isolated scene inspector containment (skeleton)

- `kwe-scene-inspector` binary: daemon-supervised, renderer containment or
  stricter (private HOME, process-group kill/reap, PDEATHSIG, closed FDs,
  rlimits, no network), bounded request/response over inherited FDs.
- Does nothing yet but accept a path, enforce byte/time bounds, and emit an
  empty inventory record conforming to the SR-0a schema.
- Acceptance: kill/hang/oversize tests; cancel deletes partial output; Plasma
  and daemon unaffected by inspector death.
- Implementation may be delegated (patch spec → sonnet) once SR-0a merges.

## SR-0c — One loader inventory adapter

- Reuse existing `kwe-core` scene/model/effect parsing read-only to emit the
  feature inventory for one family (scene.json objects + materials first):
  detected features → required capability IDs, unknown keys/types counted,
  never dropped silently.
- Deterministic output; golden JSON test; boundary fixtures (malformed,
  oversized, deep nesting) yield Unknown/Incompatible, no hang.

## SR-0d — Private corpus metadata runner

- `scripts/scene-corpus-inventory.sh` + CLI surface: run the inspector over
  the local 60-item corpus, metadata-only records (feature histogram, unknown
  counts, per-item time/bytes), no source bytes leave the machine, nothing
  committed.
- Also captures the current S7d failure cases as reproducible local
  diagnostic records (plan §8 SR-0 in-scope item).
