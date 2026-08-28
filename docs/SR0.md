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

## SR-0d — Private corpus metadata runner

- `scripts/scene-corpus-inventory.sh` + CLI surface: run the inspector over
  the local 60-item corpus, metadata-only records (feature histogram, unknown
  counts, per-item time/bytes), no source bytes leave the machine, nothing
  committed.
- Also captures the current S7d failure cases as reproducible local
  diagnostic records (plan §8 SR-0 in-scope item).
