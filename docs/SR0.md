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
                 Adversarial review (fix commit, same slice): R1 — the accept loop
                 (`for connection in listener.incoming()`) calls handle_client inline with
                 no per-connection thread, so scene.inspect's up-to-30 s wait was blocking
                 every other RPC. handle_client now special-cases scene.inspect before the
                 generic process_request call: params validate inline and fast
                 (unchanged behavior, factored into a shared validate_scene_inspect_params
                 used by both this path and process_request's own arm, which stays for the
                 direct-call RPC unit tests); a static INSPECT_IN_FLIGHT AtomicBool bounds
                 real inspection work to exactly one dedicated thread at a time — a second
                 request while one is running answers inspector-busy immediately, inline,
                 no second process; the winning request's thread runs run_inspection and
                 writes the response itself (write_response factors the exact
                 serialization handle_client always used), clearing the gate via an
                 InspectInFlightGuard Drop impl (so a panic cannot leave it stuck) right
                 after run_inspection finishes and before the response is sent, so the
                 gate is provably clear by the time a caller can observe that response.
                 R2 — inspect.rs's drain_stderr_tail read in an unbounded loop bounded
                 only by WouldBlock/EOF, so a child continuously flooding stderr could
                 starve supervise's try_wait/deadline check indefinitely (the tail-trim
                 bounds memory, not time). Capped to STDERR_DRAIN_CHUNKS_PER_TICK (16)
                 4 KiB chunks per call — at most 64 KiB of stderr read per supervise tick,
                 always falling through to the deadline check; drain_stdout needed no
                 change (its oversize cutoff already bounds it).
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
                         Review fix (R1) crates/kwe-daemon main.rs (1):
                         concurrent_scene_inspect_calls_serialize_through_the_single_inspection_gate
                         — two threads race a barrier into dispatch_scene_inspect against a
                         hang fake over real UnixStream::pair() sockets; exactly one of the
                         two responses is reason=timeout and the other is
                         reason=inspector-busy (order not presumed, asserted as a sorted
                         pair); a third call afterward gets timeout again, not busy,
                         proving the gate clears. (The coordinator's request to also assert
                         a cheap RPC like health completes while an inspection is in flight
                         was not implemented: main.rs has no test harness that drives
                         handle_client over a real UnixListener/accept loop — every existing
                         test calls process_request/handle_client-adjacent helpers directly
                         — so that variant was skipped per the "don't build a new harness"
                         instruction; the barrier test above is the full R1 coverage.)
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
                         cargo test --workspace -- 762 passed, 0 failed (kwe-scene-inspector
                         5, kwe-daemon 158 including the 6 scene.inspect tests from the
                         first commit plus the R1 concurrency test from the fix commit).
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
Commit(s):               ea88e72 (skeleton); <filled after the R1/R2 fix commit>
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
