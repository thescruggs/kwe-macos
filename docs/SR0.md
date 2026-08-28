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
Commit(s):               ea88e72 (skeleton), 97268f9 (R1/R2 review fix), 95f7f97
                         (review: in-flight guard created before the thread spawn)
```

## SR-0c — scene.json object-family inventory in the isolated inspector

```text
Task:            Fill required/detected/unknown in the scene-feature-inventory-v0
                 record from a bounded raw walk of scene.json, entirely inside
                 kwe-scene-inspector's isolated process.
Milestone/Slice: SR-0c
Goal:            Turn the SR-0b skeleton's always-empty inventory into a real one for
                 the object family (layers/particles/effects), so the record starts
                 answering "what does this scene actually declare" instead of only
                 "does it hash and parse" — without touching the daemon, the CLI, or
                 the record schema itself.
Conductor scope decision: SR0.md's original SR-0c line named "scene.json objects +
                 materials first". Narrowed at kickoff to the OBJECTS family only:
                 materials require resolving referenced files (model -> material ->
                 texture) through the pkg/scene-dir/assets-root lookup chain, which
                 this slice's isolated-process, no-asset-resolution design does not
                 have — it is its own follow-up slice.
Outcome:         New crates/kwe-scene-inspector/src/inventory.rs: parses scene.json
                 with serde_json, then walks exactly two levels (root, then
                 objects[i]) against two static known-key tables (ROOT_KEYS =
                 {general, objects}; OBJECT_KEYS = the field-name literals every
                 parser in this workspace already reads on an objects[i] entry, plus
                 sound/light/model — this SR-0c task's own named discriminators,
                 not yet read by any parser elsewhere). Classifies each object by a
                 discriminating field in EXACTLY kwe_core::sceneobjects's priority
                 order (image/model > video > particle > text; sound and light —
                 not classified anywhere else in this workspace — stay last),
                 independently adds scene.effects for any object with a non-empty
                 effects array, and counts an object with none of those as
                 unknown.objects. `required` collects capabilities from active
                 objects only (no visible field, or visible:true; a property-bound
                 (object-valued) visible counts as active per WE's user-property
                 convention, deferring resolution of the bound value to SR-11).
                 Unknown root/object keys and type mismatches (root not an object,
                 objects not an array, a non-object entry) are counted, never
                 dropped, with a bounded top-K-smallest sample-path list
                 (max_samples 16, max_sample_path_bytes 128) kept memory-bounded
                 regardless of how many candidates are offered. max_objects_walked
                 (4096) and a wall-clock deadline (checked every 256 objects) both
                 stop the walk early and mark every affected list truncated, adding
                 "objects-cap"/"timeout" to limits_hit.
                 main.rs wiring: after a successful hash, JsonDir re-reads the same
                 scene.json file bounded to max_bytes (simplest-correct, per the
                 task); Pkg locates and bounded-reads the scene.json entry through
                 kwe-core's real PkgReader/scene_json_entry/MAX_SCENE_JSON_BYTES (the
                 exact sequence kwe-scene-renderer's load_scene and kwe-core's
                 preflight_pkg both already use — no new pkg parser), adding
                 scene.package to BOTH detected and required on a successful entry
                 read (even when the bytes then fail to parse as JSON — the entry
                 was still read, and rendering it would still require scene.package;
                 the pkg container format is unconditionally required independent
                 of any object's visibility, docs/SCENE_CAPABILITIES.md's
                 scene.package taxonomy row) and answering incompatible/parse-error
                 with limits_hit "pkg-no-scene-json" when the entry is
                 missing/unreadable (scene.package never appears there — no entry
                 was read), or "pkg-scene-json-oversize" when it exceeds the 16 MiB
                 cap. A JSON syntax failure is the one error inventory_scene_json
                 reports itself (parse-error); every other malformed shape is
                 counted, never rejected.
                 Review fix (R1/R2, one follow-up commit): R1 — `video` was missing
                 as a classification discriminator; the original SR-0c task's own
                 §1 field list omitted it (an oversight, not the intentional
                 materials-style narrowing this task's Open risks originally
                 guessed it might be — see Commit(s)), unlike kwe_core::sceneobjects
                 real classifier, which does detect it. Added at the SAME priority
                 position that classifier gives it (between image and particle).
                 R2 — scene.package was only in detected, not required; fixed as
                 described above.
In scope:        crates/kwe-scene-inspector/src/inventory.rs (new),
                 crates/kwe-scene-inspector/src/main.rs (wiring),
                 crates/kwe-scene-inspector/Cargo.toml (new kwe-core path
                 dependency — internal, not an external crate), docs/SR0.md,
                 docs/SUPERVISOR_API_V1.md.
Out of scope:    Materials (deferred, see the conductor note above); any reference
                 resolution (image/model/particle-file paths against pkg/scene-dir/
                 assets-root); the daemon (crates/kwe-daemon) and CLI
                 (crates/kwe-cli) — neither changed, and none of their existing
                 tests changed either (the daemon's passthrough test already only
                 checks the schema tag on a fake record); any SCENE_CAPABILITIES.md
                 status change (inventory detection is not a support claim).
Acceptance tests:        crates/kwe-scene-inspector/src/inventory.rs (9): golden
                         (one visible image object with id, one visible:false text
                         object, one particle object with a non-empty effects array,
                         one unclassifiable object, one visible video object, one
                         unknown root key — exact detected counts/ids; required has
                         scene.layer.image, scene.layer.video, scene.particle,
                         scene.effects but NOT scene.layer.text — updated by the
                         R1 review fix to add the video object/assertions);
                         same input twice is byte-identical (Inventory PartialEq);
                         objects-not-an-array is an unknown type, not a parse
                         failure; 4096+50 objects stops the walk at the cap with
                         every detected list truncated; invalid JSON syntax is the
                         one Err(Parse); a 10_000-deep nested array under a known key
                         never reaches this module's own walk and — measured
                         directly on this workspace's serde_json 1.0.151 — already
                         fails to parse at all ("recursion limit exceeded", no stack
                         growth), so the test asserts that actual outcome rather than
                         the task's literal "parses via serde_json" expectation (see
                         Open risks); an expired deadline stops the walk exactly like
                         the objects cap; sound/light classify and are never
                         double-counted as unknown keys; a non-object objects[]
                         entry is skipped and counted, never a parse failure.
                         crates/kwe-scene-inspector/src/main.rs (+1 new test, plus
                         assertions added to two existing ones; 20 total in the
                         crate after the review fix): a JsonDir scene populates
                         required/detected/unknown in the actual emitted record;
                         malformed scene.json is incompatible/parse-error with
                         content.hash still populated (hashing ran before the
                         inventory parse did); build_record given identical inputs
                         (including two independently-run Inventory walks) produces
                         byte-identical records including the digest, isolated from
                         bounds.wall_ms's real run-to-run variance; a pkg carrying
                         scene.json with one image object detects scene.package and
                         scene.layer.image in BOTH detected and required (R2 review
                         fix: required assertion added); a pkg with no scene.json
                         entry is incompatible/parse-error with limits_hit
                         ["pkg-no-scene-json"] and neither detected nor required
                         carries scene.package (R2 review fix: boundary assertion
                         added); new (R2 review fix)
                         pkg_with_unparseable_scene_json_still_requires_scene_package
                         — a pkg whose scene.json entry reads but is not valid JSON
                         still answers incompatible/parse-error with required ==
                         detected == ["scene.package"] alone.
                         Full existing suites stay green: cargo test --workspace,
                         cargo clippy --workspace --all-targets -D warnings,
                         cargo fmt --all --check, ./scripts/check.sh. The daemon's
                         existing scene.inspect passthrough test is unchanged and
                         still passes (its fake inspector emits its own
                         schema-tagged record; it never runs the real binary).
Failure/recovery tests:  Covered by the acceptance tests above: parse-error,
                         objects-cap, timeout, and the two pkg-scene-json failure
                         modes are all typed, bounded outcomes, never a hang or a
                         panic.
Upstream/provenance:     Original; the object-classification priority order mirrors
                         `kwe_core::sceneobjects::classify_scene_object` (itself
                         original, researched from the WE object model — see that
                         module's own docs).
Commands run and results: cargo fmt --all -- clean.
                         cargo clippy --workspace --all-targets -- -D warnings --
                         clean.
                         cargo test --workspace -- 777 passed, 0 failed
                         (kwe-scene-inspector 20 after the R1/R2 review fix, up
                         from 19 at the first SR-0c commit and 5 at the SR-0b
                         skeleton; kwe-daemon 158, unchanged).
                         ./scripts/check.sh -- green end-to-end, including the
                         C++/QML build, qml-typecheck, kwe diagnose, and
                         kwe-vulkan --json.
Open risks:              The task's literal entry-point signature
                         (`fn inventory_scene_json(bytes, caps) -> Inventory`) cannot
                         represent a JSON parse failure distinctly from a
                         successfully-parsed empty scene, nor take a deadline —
                         implemented as `Result<Inventory, InventoryError>` with an
                         added `deadline: Instant` parameter instead; see the
                         module's own "Deviation" doc section.
                         The task's derivation method ("enumerate the serde field
                         names of the existing scene structs") does not apply
                         mechanically: no file in this workspace parses scene.json
                         through `#[derive(Deserialize)]` structs — every field is
                         read via raw `serde_json::Value::get("...")` navigation.
                         ROOT_KEYS/OBJECT_KEYS instead enumerate the actual
                         string-literal keys the existing code reads (documented in
                         inventory.rs next to each table); SR-2's typed IR is still
                         the eventual authority either way.
                         `kwe_core::pkg::testutil::PkgWriter` (the pkg fixture
                         builder the task asked to reuse) is `#[cfg(test)]
                         pub(crate)` inside kwe-core, so it is not visible to this
                         crate's own tests; a small local `build_pkg` mirroring its
                         exact byte layout was written for fixture-building only —
                         every fixture is still read exclusively through the real
                         `kwe_core::PkgReader`.
                         The task's hostile fixture (d) expected a 10_000-deep
                         nested array to "parse via serde_json"; measured directly,
                         it does not (serde_json's built-in recursion guard rejects
                         it before this module ever sees it). The test asserts the
                         actual, safer-than-expected behavior instead.
                         RESOLVED by the R1 review fix: `video` was missing as a
                         classification discriminator (the task's §1 list omitted
                         it, unlike `kwe_core::sceneobjects`'s own classifier, which
                         does detect video objects), and this doc flagged it in
                         case it was an oversight rather than an intentional
                         narrowing alongside materials. The conductor confirmed it
                         was an oversight; `video` is now a discriminator, inserted
                         at the same priority position (between `image` and
                         `particle`) `classify_scene_object` gives it.
Commit(s):               1783148, 4304b25 (R1/R2 review fix)
```

## SR-0d — private corpus metadata inventory runner

```text
Task:            scripts/scene-corpus-inventory.sh: run kwe-scene-inspector over a
                 directory of Workshop items, write bounded metadata-only NDJSON
                 records and a deterministic aggregate summary, nothing committed.
Milestone/Slice: SR-0d
Goal:            Turn SR-0b/c's per-item inspector into a corpus-wide local lab tool
                 the maintainer can point at the real 60-item Workshop corpus (or any
                 other) to see the object-family inventory's aggregate shape —
                 outcome/reason histogram, detected/required capability counts,
                 unknown-key samples — without any of it ever leaving the machine or
                 landing in the repo.
Conductor scope decisions: (1) This script invokes kwe-scene-inspector DIRECTLY
                 under `timeout`, not through the daemon's scene.inspect RPC
                 (crates/kwe-daemon/src/inspect.rs). It is an uncommitted-output
                 local lab harness over the maintainer's own corpus, not a
                 production code path: the inspector is still its own process with
                 its own bounds (byte/time caps, the 64 KiB report cap), but it does
                 NOT get the daemon's containment (private HOME, PDEATHSIG, rlimits,
                 process-group kill, the single-in-flight gate). Daemon-grade
                 containment for inspection remains the production path
                 (scene.inspect); this script trades that off deliberately for a
                 maintainer-only convenience over trusted local content.
                 (2) The original SR-0d line "captures the current S7d failure cases
                 as reproducible local diagnostic records" is narrowed OUT of this
                 slice: those diagnoses and maintainer reports already exist locally
                 (~/.local/share/kwe/reports/, project memory notes,
                 docs/s7-report-root-causes.md-style notes) — a renderer-side
                 capture harness is not inventory work and was never implemented
                 here.
Outcome:         New scripts/scene-corpus-inventory.sh (bash, set -euo pipefail):
                 --corpus-dir (required), --inspector (default
                 $CARGO_TARGET_DIR-or-target/debug/kwe-scene-inspector beside the repo
                 root), --out (default
                 ${XDG_DATA_HOME:-$HOME/.local/share}/kwe/corpus/<UTC
                 yyyymmdd-HHMMSS>/, chmod 700), --per-item-timeout-s (15),
                 --max-source-mib (512, passed through). Discovers immediate
                 subdirectories of corpus-dir, sorted (find -mindepth 1 -maxdepth 1 |
                 sort -z, never recursing deeper): a symlinked item dir is detected
                 with `-L` BEFORE anything that would traverse it and recorded
                 skipped-symlink; scene.pkg present -> inspect that file; else
                 scene.json present -> inspect the item directory; else recorded
                 skipped — none of these three ever runs the inspector except the
                 scene.pkg/scene.json cases. Each inspected item runs `timeout
                 <n>s <inspector> --input <path> --max-source-mib <n>`; a single
                 python3 wrapper (defined once, invoked via `python3 -c` per item —
                 avoids the heredoc-consumes-stdin trap of `python3 - <<EOF`)
                 constructs one NDJSON line per item with a uniform shape ({"item",
                 "status": inspected|skipped|skipped-symlink, "exit", "timed_out",
                 "record", ["stdout_invalid"]}), so malformed inspector stdout
                 becomes record:null + stdout_invalid:true instead of corrupting the
                 file, and unusual item basenames can never break JSON escaping.
                 stderr is captured separately, truncated to 4 KiB, one file per
                 item under <out>/stderr/. A per-item failure/timeout never aborts
                 the run (bracketed set +e/set -e around the capture); the script
                 exits nonzero only for usage errors, a missing/non-executable
                 inspector binary, or an unwritable out dir.
                 New scripts/scene-corpus-summarize.py (python3 stdlib only —
                 argparse/json/statistics/collections/datetime): reads records.ndjson,
                 writes <out>/summary.json (sort_keys=True, byte-deterministic for
                 identical input) with corpus_items (total/skipped/inspected), an
                 outcome:reason histogram, timed_out/stdout_invalid counts, detected
                 {capability: {items, total_count}}, required {capability: items},
                 an unknown aggregate (keys/types/objects totals + top-20
                 item-frequency-sorted sample paths), a limits_hit histogram,
                 wall_ms {max, median}, source_bytes {max, total}, and the sorted set
                 of inspector.build values seen — no titles, no absolute paths, item
                 basenames (Workshop IDs) only. Also prints a compact stdout table,
                 with each histogram sorted by count desc then name asc (summary.json
                 itself stays alphabetically key-sorted for diffability; the two
                 outputs sort differently on purpose — see the script's own docstring).
                 New scripts/smoke-scene-corpus.sh, gated
                 KWE_RUN_SCENE_CORPUS_SMOKE=1 exactly like every other opt-in smoke
                 suite (scripts/check.sh): builds a synthetic 4-item corpus (one dir
                 item with a visible image object, one dir item with a visible text
                 object — deliberately different capabilities so both show up
                 distinctly — one item with neither scene.pkg nor scene.json, one
                 symlinked item dir), builds kwe-scene-inspector first, runs the
                 real inventory script against the fixture with an explicit
                 --inspector, and asserts records.ndjson has exactly 4 wrapped lines
                 with the right statuses, summary.json exists with matching
                 corpus_items/outcome counts, and the detected histogram carries both
                 scene.layer.image and scene.layer.text at items=1. Wired into
                 scripts/check.sh next to the other KWE_RUN_*_SMOKE gates. pkg-kind
                 coverage is NOT duplicated here — it already lives in
                 crates/kwe-scene-inspector/src/main.rs's pkg_* Rust unit tests
                 (SR-0c); this smoke only proves the shell/python harness's own
                 wrapping, skip, and symlink logic end to end.
In scope:        scripts/scene-corpus-inventory.sh (new), scripts/scene-corpus-summarize.py
                 (new), scripts/smoke-scene-corpus.sh (new), scripts/check.sh (one
                 new KWE_RUN_SCENE_CORPUS_SMOKE gate), docs/SR0.md. No Rust code
                 changed.
Out of scope:    The S7d capture harness (conductor scope decision 2, above); running
                 the real Workshop corpus (the conductor runs that separately after
                 merge — this report does not include real corpus output); any
                 daemon/CLI change (the script talks to the inspector binary
                 directly, per conductor scope decision 1); docs/SUPERVISOR_API_V1.md
                 and docs/SCENE_CAPABILITIES.md (untouched, per the task).
Acceptance tests:        scripts/smoke-scene-corpus.sh (KWE_RUN_SCENE_CORPUS_SMOKE=1):
                         4/4 NDJSON lines with the right statuses (2 inspected, 1
                         skipped, 1 skipped-symlink); both inspected items answer
                         inventoried/ok; summary.json's corpus_items
                         (total=4/inspected=2/skipped=2) and the
                         inventoried:ok=2 outcome count match; the detected histogram
                         carries scene.layer.image and scene.layer.text each at
                         items=1. Manual runs during implementation additionally
                         verified: an empty corpus dir exits 0 with an all-zero
                         summary (no bash unbound-array error under `set -u`, bash
                         5.3); a malformed scene.json item (invalid JSON) yields
                         outcome incompatible/reason parse-error in the record and
                         does not abort the run; --corpus-dir omitted, a nonexistent
                         --corpus-dir, and a missing --inspector binary each exit
                         nonzero with a clear message (2, 2, 1 respectively); the out
                         dir is created 0700.
                         cargo fmt/clippy/test --workspace: unchanged from trunk (no
                         Rust files touched) — 777 passed, 0 failed, both before and
                         after this slice.
                         ./scripts/check.sh: green both without
                         KWE_RUN_SCENE_CORPUS_SMOKE (default, matching every other
                         opt-in smoke) and with it set to 1 (includes the C++/QML
                         build and qml-typecheck both times).
Failure/recovery tests:  A per-item inspector failure/timeout is captured and
                         recorded, never aborts the corpus run (verified via the
                         malformed-JSON manual case above, which the harness's own
                         set +e/set -e bracketing around the timeout capture keeps
                         from tripping `set -e`); usage/missing-binary/unwritable-out
                         errors exit nonzero with a message naming the problem.
Upstream/provenance:     Original; style-matched to scripts/smoke-corpus-pkg.sh and
                         scripts/scene-corpus-byte-identity-sweep.sh (jq for JSON
                         assertions, mktemp -d + trap cleanup, KWE_RUN_*_SMOKE gating)
                         and scripts/frame-read.py (python3 stdlib, non-executable,
                         invoked as `python3 <path>`).
Commands run and results: shellcheck: NOT INSTALLED on this machine (`command -v
                         shellcheck` and `pacman -Q shellcheck` both fail) — could not
                         run it; both new shell scripts were syntax-checked with
                         `bash -n` (clean) and exercised manually instead (see
                         Acceptance tests).
                         python3 -m py_compile scripts/scene-corpus-summarize.py --
                         clean.
                         cargo fmt --all -- --check -- clean (no Rust changed).
                         cargo clippy --workspace --all-targets -- -D warnings --
                         clean (no Rust changed).
                         cargo test --workspace -- 777 passed, 0 failed (identical to
                         the SR-0c baseline).
                         KWE_RUN_SCENE_CORPUS_SMOKE=1 ./scripts/smoke-scene-corpus.sh
                         -- passed standalone.
                         ./scripts/check.sh -- green both with and without
                         KWE_RUN_SCENE_CORPUS_SMOKE=1.
Open risks:              shellcheck was not available to verify the two new shell
                         scripts against its lint rules; only manual review + bash -n
                         + actual execution covered them. A maintainer with
                         shellcheck installed should run it once before or after the
                         first real corpus pass.
                         summary.json's "median"/other statistics.median() output can
                         serialize as a JSON float even for integer-valued input
                         (Python averages the two middle values for an even-length
                         sample) — cosmetic only, not a correctness issue, but worth
                         knowing when diffing summaries across runs with different
                         item counts.
                         This tool is explicitly less contained than production
                         scene.inspect (conductor scope decision 1) — it must never be
                         pointed at untrusted content, only the maintainer's own local
                         corpus.
Commit(s):               41cdc97
```

Real-corpus run (conductor, 2026-08-28, local lab, metadata only): 92 items,
60 inspected, all 60 `inventoried:ok`, 0 timeouts, 0 invalid reports, no
limits hit; wall_ms median 406.5 / max 8541 (a 180 MB pkg hash), 1.42 GB
total source bytes. Required histogram: scene.package 60, scene.layer.image
60 (685 objects), scene.effects 48 (541), scene.particle 33 (315),
scene.layer.sound 19, scene.layer.text 14; scene.layer.video and
scene.lighting absent from this corpus. unknown.keys=5234 — top paths:
root `camera` (60 items — matches the S7 orthogonalprojection root cause),
`parallaxDepth` (48), `copybackground` (46), `locktransforms` (46),
`solid` (39); unknown objects 20. Records under
`~/.local/share/kwe/corpus/20260828-013020/` (uncommitted).

## SR-0 epic status

Complete 2026-08-28: SR-0a cbffa67; SR-0b ea88e72/97268f9/95f7f97; SR-0c
1783148/4304b25; SR-0d 41cdc97 + the corpus baseline above. Next: SR-1
(report protocols + capability/schema freeze — the freeze itself is the
§11.3 maintainer decision, informed by the corpus unknown-key data).
