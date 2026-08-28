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
Commit(s):               cbffa67
```

## SR-0b — Isolated scene inspector containment (skeleton)

```text
Task:            Stand up the kwe-scene-inspector one-shot binary and its daemon-
                 supervised scene.inspect RPC, with no scene parsing yet.
Milestone/Slice: SR-0b
Goal:            A bounded, containable path from "here is a scene entry" to "here is
                 one JSON scene-feature-inventory-v0 line" — classify the input, hash
                 bounded bytes, emit an empty inventory — so SR-0c only has to fill in
                 required/detected/unknown against an already-safe process boundary.
Outcome:         New crate crates/kwe-scene-inspector (binary): classifies a `scene.pkg`
                 file or a `scene.json`-containing directory, streams a bounded SHA-256
                 (64 KiB chunks, byte cap, self-watchdog wall-clock backstop), and emits
                 one draft scene-feature-inventory-v0 JSON line on stdout (<= 64 KiB,
                 digest = SHA-256 of the record with digest:""). Unrecognized input,
                 oversize, timeout, and internal I/O error all answer typed and exit 0;
                 only an unwritable stdout exits 74.
                 New crates/kwe-daemon/src/inspect.rs: `run_inspection` spawns the
                 inspector under the exact spawn_worker containment (private 0700 HOME
                 under runtime_dir, env_clear() + the shared {HOME,PATH} allowlist,
                 stdin null / stdout+stderr piped, the same pre_exec block — setpgid,
                 PR_SET_PDEATHSIG SIGKILL, parent-pid check, PR_SET_NO_NEW_PRIVS,
                 apply_resource_limits reusing the scene renderer kind's ceilings).
                 Drains stdout (capped at 64 KiB+1, over → report-oversize) and a bounded
                 stderr tail under a configurable wall-clock deadline (default 10 s); on
                 expiry SIGKILLs the child's process group and reaps it (timeout); a
                 nonzero exit or a report that fails to parse/match the schema tag
                 answers inspector-failed with the stderr tail; every path removes the
                 HOME dir. `apply_resource_limits`, the `RendererResourceLimits` fields,
                 `env_allowlist`, `cleanup_renderer_home`, `set_nonblocking`, and
                 `signal_process_group` in supervisor.rs are now `pub(crate)` so
                 inspect.rs reuses them instead of re-implementing containment.
                 New RPC `scene.inspect {"path": "..."}` in main.rs's dispatch: rejects
                 an empty or relative path as `invalid_params` before it ever reaches
                 `inspect`; otherwise returns `run_inspection`'s result verbatim. New CLI
                 flags `--inspector`/`--inspector-wall-timeout-ms` on kwe-daemon (default
                 binary path resolves beside the daemon executable, like every renderer
                 kind, but tolerates a missing daemon-exe resolution instead of failing
                 daemon startup — the inspector is optional/experimental). New
                 `kwe scene-inspect --socket ... --path ...` CLI subcommand; the inline
                 daemon-RPC client logic in kwe-cli's DaemonCall arm is factored into a
                 shared `call_daemon` helper both subcommands now use.
                 packaging/PKGBUILD installs kwe-scene-inspector beside kwe-scene-renderer
                 (no pkgrel bump — nothing is enabled by this change; the daemon still
                 answers scene.inspect only when a caller asks for it, and the record
                 schema is draft, not exposed anywhere else yet).
In scope:        crates/kwe-scene-inspector (new crate + workspace member), crates/kwe-
                 daemon/src/inspect.rs (new) + main.rs (scene.inspect dispatch, CLI flags,
                 InspectConfig wiring) + supervisor.rs (visibility bumps only, no behavior
                 change), crates/kwe-cli/src/main.rs (SceneInspect subcommand, call_daemon
                 helper), packaging/PKGBUILD, docs/SR0.md, docs/SUPERVISOR_API_V1.md.
Out of scope:    Any scene/model/effect parsing (SR-0c); filling required/detected/
                 unknown; corpus runs (SR-0d); moving the report off stdout onto a
                 dedicated FD/envelope (deferred to SR-1, see Open risks).
Acceptance tests:        kwe-scene-inspector (5): a small scene.json directory hashes
                         identically across two runs (content.kind=json-dir, sha256:-
                         prefixed hash); a 0 MiB cap against a nonempty file refuses
                         incompatible/oversize; an empty directory and a non-.pkg file
                         both refuse incompatible/unrecognized-input; a normal record
                         and an artificially oversized one (forced through the
                         report-oversize fallback) both stay <= 64 KiB serialized; an
                         already-expired wall-clock deadline yields unknown/timeout.
                         crates/kwe-daemon inspect.rs (5, python3 fake inspectors mirroring
                         the FAKE_SCENE_RENDERER technique in main.rs's own tests): a
                         well-behaved fake's report passes through verbatim and its HOME
                         dir is removed; a fake that sleeps forever is killed at the
                         wall-clock deadline (kill(pid,0) fails afterward, HOME dir gone);
                         a fake that floods stdout past 64 KiB answers report-oversize; a
                         fake that exits 1 with stderr text answers inspector-failed
                         carrying that text in stderr_tail; an unconfigured inspector_path
                         answers inspector-unavailable without spawning anything.
                         crates/kwe-daemon main.rs (1): scene.inspect with an empty or a
                         relative path, and with an unknown params field, all answer
                         invalid_params before touching the inspect config (mirrors the
                         permissions.get bad-input test).
                         Full existing suites stay green: cargo test --workspace,
                         cargo clippy --workspace --all-targets -D warnings,
                         cargo fmt --all --check, ./scripts/check.sh.
Failure/recovery tests:  Covered by the acceptance tests above (timeout/oversize/failed/
                         unavailable are all typed failure-recovery paths, not
                         exceptions); every path in inspect.rs reaps the child and
                         removes the HOME dir regardless of outcome.
Upstream/provenance:     Original; containment is the same design as
                         supervisor::spawn_worker (THIRD_PARTY.yml note there covers the
                         process-isolation goal's inspiration, not this code).
Commands run and results: cargo fmt --all -- clean.
                         cargo clippy --workspace --all-targets -- -D warnings -- clean.
                         cargo test --workspace -- 761 passed, 0 failed (kwe-scene-inspector
                         5, kwe-daemon 157 including the 6 new scene.inspect tests).
                         ./scripts/check.sh -- green end-to-end, including the C++/QML
                         build, qml-typecheck, kwe diagnose, and kwe-vulkan --json.
Open risks:              The report travels on stdout in this skeleton (a chatty child
                         process writing to its own stdout before the real report line
                         would corrupt the protocol); SR-1 moves it to a dedicated report
                         FD/envelope so stdout stops being trusted framing. `inspector.build`
                         is a literal "dev" placeholder — there is no option_env! git-sha
                         mechanism anywhere in this workspace to mirror for a standalone
                         one-shot binary, and the daemon's own build_identity (binary
                         size+mtime across every renderer path) has no equivalent a
                         subprocess can compute about itself; SR-1 should pick one real
                         mechanism. content.kind is "" (not "pkg"/"json-dir") for
                         unrecognized-input, io-error, and timeout-before-classification
                         records — the draft schema in SCENE_CAPABILITIES.md only shows
                         the two real values, so this is an interim convention pending
                         SR-1. digest omits a "sha256:" prefix (unlike content.hash) since
                         the task spec's literal wording was "hex SHA-256 over the
                         serialized record" — SR-1 should confirm this is intended.
Commit(s):               <filled after commit; same commit as this file>
```

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
