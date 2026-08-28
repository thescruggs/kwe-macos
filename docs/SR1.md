# SR-1 — report protocol, daemon validation, and version-skew safety
(decomposition)

Parent epic: `docs/Scene-Rendering-Plan.md` §5.3 ("Structured reports") and
§8 SR-1. Approved 2026-08-28 (plan gate §11.3 froze `docs/SCENE_CAPABILITIES.md`
as v1, unblocking this epic — see `docs/SR0.md`'s closing status for the
corpus evidence that informed the freeze).

Child order: SR-1a → SR-1b → SR-1c → SR-1d → SR-1e. Each child is one
mergeable slice with its own implementation and adversarial-review passes,
following the same template as `docs/SR0.md`.

## SR-1a — report protocol v1 doc + codec crate

```text
Task:            Define and implement the report-FD wire format (frame header,
                 stream caps, kind table) and the scene-inspection-v1 record
                 schema/digest validator, as a new library crate plus doc. No
                 daemon/inspector/CLI behavior change in this slice.
Milestone/Slice: SR-1a
Goal:            Freeze the wire bytes and the v1 record schema BEFORE any code
                 wires a real fd to a real pipe (SR-1b) or makes a policy decision
                 from a report (SR-1c) — so those slices implement against a fixed
                 target instead of designing the format under implementation
                 pressure. docs/REPORT_PROTOCOL_V1.md is the frozen reference;
                 crates/kwe-report-protocol is its codec.
Outcome:         New crate crates/kwe-report-protocol (library, no new external
                 dependencies — serde_json/sha2/hex/thiserror are all already
                 workspace deps used by sibling protocol crates):
                 - FrameKind {SceneInspectionV1, SceneRenderReportV1, Unknown(u8)}
                   with from_u8/as_u8.
                 - Frame {kind, payload} -- the codec's only output shape, for
                   every kind including Unknown (payload is handed back, not
                   discarded, so a caller is never retroactively broken by a
                   future kind it does not recognize yet).
                 - write_frame(writer, kind, payload) -- refuses payload > 64 KiB
                   and an Unknown kind with io::ErrorKind::InvalidInput.
                 - FrameReader<R>::next_frame() -> Result<Option<Frame>, FrameError>
                   -- Ok(None) only on a clean EOF at a frame boundary; every other
                   EOF is a typed TruncatedHeader/TruncatedPayload. Enforces
                   MAX_FRAMES_PER_STREAM (16), MAX_TOTAL_PAYLOAD_BYTES (1 MiB), and
                   MAX_PAYLOAD_BYTES (64 KiB) per frame, all reader-side and
                   independent of what the payload itself claims. The byte cap is
                   checked before the frame-count cap so TotalBytesExceeded stays
                   independently reachable given this protocol's exact constants
                   (16 x 64 KiB == 1 MiB exactly -- see docs/REPORT_PROTOCOL_V1.md's
                   own note on this).
                 - validate_inspection(payload) -> Result<Value, ValidationError>:
                   schema + capabilities_schema tag checks, every required
                   top-level (and one-level-nested) field's presence and JSON
                   type (MissingField vs WrongType always distinguished), and
                   digest verification using the exact canonicalization rule
                   kwe-scene-inspector's build_record already uses (digest set to
                   "", serde_json::to_vec, SHA-256, hex -- deterministic only
                   because serde_json's Value is a BTreeMap here; documented as a
                   workspace-wide constraint that preserve_order must stay off).
                 New docs/REPORT_PROTOCOL_V1.md: wire format table, kind table
                 (kind 2 reserved, carried opaquely), the stream-cap
                 limit-1/limit/limit+1 table with the pairwise-constant
                 coincidence explained, the --report-fd convention and ownership
                 rules (daemon creates/owns the read end, closes both ends on
                 generation change; documented for SR-1b, not implemented here),
                 the five intended report-policy reason codes for SR-1c
                 (report-malformed/-missing/-duplicate/-late/-unavailable), the
                 full scene-inspection-v1 field list and its v0 -> v1 mapping
                 table, and an explicit note that scene-render-report-v1 (kind 2)
                 is reserved until its own producer lands.
In scope:        crates/kwe-report-protocol (new crate + workspace member),
                 docs/REPORT_PROTOCOL_V1.md (new), docs/SR1.md (new, this file).
Out of scope:    Any daemon/inspector/CLI code change (crates/kwe-daemon,
                 crates/kwe-scene-inspector, crates/kwe-cli are all untouched);
                 wiring --report-fd into spawn_worker (SR-1b); daemon-side
                 malformed/missing/duplicate/late policy (SR-1c); the
                 scene-render-report-v1 schema itself (reserved, no producer yet).
Acceptance tests:        20 tests in crates/kwe-report-protocol: round-trip 1..=3
                         frames of both known kinds; writer refuses an Unknown
                         kind, reader yields FrameKind::Unknown from hand-crafted
                         bytes and continues to the next frame; frame-count cap at
                         15/16/17 (ok/ok/FrameCountExceeded); total-payload cap
                         just-under/at/over 1 MiB (ok/ok/TotalBytesExceeded);
                         payload_len at 65535/65536/65537
                         (ok/ok/PayloadOversize); bad magic, nonzero flags,
                         nonzero reserved, truncated header at every 1..12-byte
                         prefix (12 sub-cases), truncated payload -- each its own
                         typed error, no panic; 5 fixed-seed pseudo-random 4 KiB
                         buffers through FrameReader -- never panics; a golden
                         valid scene-inspection-v1 record (correct digest) ->
                         Ok; each of the 12 required top-level/nested fields
                         removed individually -> MissingField naming that exact
                         dotted path; wrong types (content.source_bytes, required,
                         backend) -> WrongType; wrong schema and
                         capabilities_schema strings -> WrongSchema/
                         WrongCapabilitiesSchema; backend accepts both null and an
                         object; a flipped digest -> DigestMismatch; a non-object
                         top-level value and syntactically invalid JSON -> handled
                         typed, not a panic; an oversized (256 KiB) buffer handled
                         gracefully with no frame-cap involved (validate_inspection
                         assumes the caller already capped it).
                         cargo fmt/clippy/test --workspace green; ./scripts/check.sh
                         green (no Rust behavior changed outside the new crate).
Failure/recovery tests:  Covered by Acceptance tests above -- every corruption/cap
                         case is a typed FrameError/ValidationError variant, never
                         a panic (random-bytes fuzzing-style test additionally
                         proves this over pseudo-random input, not just crafted
                         cases).
Upstream/provenance:     Original; wire-format and validation style mirrors this
                         workspace's existing protocol crates
                         (kwe-frame-protocol/kwe-input-protocol), not any external
                         project.
Commands run and results: cargo fmt --all -- clean.
                         cargo clippy --workspace --all-targets -- -D warnings --
                         clean.
                         cargo test --workspace -- 797 passed, 0 failed
                         (kwe-report-protocol 20, new; up from 777 at the SR-0
                         baseline).
                         ./scripts/check.sh -- green end-to-end, including the
                         C++/QML build and qml-typecheck.
Open risks:              scene-render-report-v1 (kind 2) has no schema yet --
                         validate_inspection only covers kind 1; a kind-2 producer
                         will need its own validator function when it lands,
                         following the same digest/field-presence pattern.
                         The report-FD ownership rules and the five SR-1c reason
                         codes are documented in docs/REPORT_PROTOCOL_V1.md as a
                         forward-looking contract, not yet implemented or tested
                         against a real pipe/fd -- SR-1b/c must confirm the
                         documented behavior matches what actually gets built.
Commit(s):               1c2b65e
```

## SR-1b — scene-inspection-v1 over the report FD

```text
Task:            Wire kwe-report-protocol's frame codec onto a real fd for the
                 one-shot scene.inspect path: kwe-scene-inspector gains
                 --report-fd, the daemon creates/owns the pipe and reads the
                 result off it instead of stdout, and the report-policy reason
                 codes docs/REPORT_PROTOCOL_V1.md named (report-malformed/
                 -missing/-duplicate) become real daemon behavior for this path.
Milestone/Slice: SR-1b
Goal:            Prove the SR-1a wire format and schema actually work end to end
                 over a real pipe between two real processes, for the one
                 existing report producer/consumer pair (kwe-scene-inspector /
                 crates/kwe-daemon/src/inspect.rs) — before SR-1c generalizes
                 the policy layer to a long-lived renderer worker's own report
                 stream and an apply-gate decision.
Outcome:         crates/kwe-scene-inspector: new optional --report-fd <n> flag,
                 validated (0/1/2 rejected as a usage error, exit 2, before
                 inspection starts) up front. Absent: byte-identical to before
                 -- the v0 record line on stdout (scripts/scene-corpus-
                 inventory.sh and any manual invocation untouched). Present:
                 NOTHING on stdout; the record is built in the
                 scene-inspection-v1 shape (schema string, +capabilities_schema,
                 +nullable backend:null) and written as exactly one kind-1 frame
                 via kwe_report_protocol::write_frame to a File built from the
                 raw fd (OwnedFd::from_raw_fd -- SAFETY documented: the daemon
                 contract guarantees this fd is a pipe write end owned
                 exclusively by this process), then flushed and dropped (closing
                 it, which is what lets the daemon's reader see EOF). A write/
                 flush failure exits 74, the same class as unwritable stdout
                 (that constant's doc comment now covers both channels).
                 build_record is parameterized by a new RecordFormat{V0,V1} enum
                 threaded through the whole call chain (inspect_input ->
                 json_dir_record/pkg_record -> inventoried_record ->
                 build_record, and bound_report's own oversize fallback) rather
                 than duplicated -- the digest is computed exactly once, over
                 whichever shape was actually built.
                 crates/kwe-daemon/src/inspect.rs: run_inspection now creates
                 the report pipe with libc::pipe2(..., O_CLOEXEC); the child's
                 pre_exec dup2's the write end onto a fixed fd 3 (inserted into
                 the SAME closure that already runs setpgid/PR_SET_PDEATHSIG/
                 the parent-pid check/PR_SET_NO_NEW_PRIVS/apply_resource_limits,
                 mirroring supervisor::spawn_worker's pre_exec exactly) and
                 --report-fd 3 is always passed. Verified before implementing:
                 fd 3 is free at that point in every launch -- std's own stdio
                 setup (0/1/2) runs before pre_exec, and every OTHER fd this
                 daemon process holds open is O_CLOEXEC (grepped every
                 OpenOptions::open in the crate; std sockets/pipes default to it
                 too) -- so no STOP condition was hit. The daemon (parent)
                 closes its own copy of the write end immediately after spawn,
                 regardless of spawn outcome. supervise drains the report fd
                 exactly like stdout was drained before (nonblocking, bounded
                 accumulation) at a new cap (kwe_report_protocol's
                 MAX_TOTAL_PAYLOAD_BYTES + MAX_FRAMES_PER_STREAM headers + 1);
                 exceeding it -> report-oversize (the same reason stdout
                 flooding already used -- stdout is STILL drained and bounded
                 too, defensively, since a misbehaving/old-format child could
                 otherwise deadlock on a full pipe, but its content is no
                 longer parsed as the result). finalize is rewritten: nonzero
                 exit keeps inspector-failed + stderr_tail UNCHANGED (report
                 bytes ignored -- this is also how an old, pre---report-fd
                 inspector resolves, since it rejects the unknown flag with a
                 clap usage error, exit 2); on exit 0, the accumulated report
                 bytes are parsed with FrameReader: zero scene-inspection-v1
                 frames (including "only Unknown-kind frames arrived") ->
                 report-missing; exactly one, validate_inspection-checked ->
                 the validated record verbatim; two or more -> report-duplicate;
                 any FrameError/ValidationError -> report-malformed with a
                 bounded (256-byte) detail plus stderr_tail. The old stdout-
                 JSON-parsing path is fully removed.
In scope:        crates/kwe-scene-inspector/src/main.rs (RecordFormat, --report-fd,
                 the report-FD write path), crates/kwe-scene-inspector/Cargo.toml
                 (kwe-report-protocol dependency), crates/kwe-scene-inspector/
                 tests/report_fd.rs (new integration test binary),
                 crates/kwe-daemon/src/inspect.rs (pipe creation, pre_exec dup2,
                 supervise/drain_report, finalize rewrite), crates/kwe-daemon/
                 Cargo.toml (kwe-report-protocol dependency), docs/
                 SUPERVISOR_API_V1.md, docs/REPORT_PROTOCOL_V1.md, docs/SR1.md.
Out of scope:    The long-lived renderer worker's own report stream
                 (supervisor::spawn_worker, renderer.start) -- untouched;
                 report-late and the renderer-worker sense of report-unavailable
                 (no apply-window deadline or renderer-worker report stream
                 exists yet to make either concept real -- SR-1c); the fuller
                 old/new upgrade/downgrade/canary matrix (SR-1d, one skew case
                 covered here as a down payment); the manager UI (SR-1e).
Acceptance tests:        crates/kwe-scene-inspector: 3 new unit tests
                         (report_fd_aliasing_stdio_is_rejected;
                         v1_record_adds_capabilities_schema_and_null_backend_and_validates,
                         cross-checked against kwe_report_protocol::validate_inspection
                         itself; v0_and_v1_builds_agree_on_every_field_except_the_
                         documented_three) -- 23 total in the bin, up from 20.
                         2 new integration tests in tests/report_fd.rs, spawning
                         the REAL compiled binary via
                         env!("CARGO_BIN_EXE_kwe-scene-inspector") (verified
                         empirically that Cargo keeps the hyphen in that env var
                         name, not underscore): --report-fd present -> stdout
                         empty, exactly one kind-1 frame arrives over a real
                         pipe (CLOEXEC cleared pre-spawn on the inherited write
                         end), validate_inspection accepts it, outcome
                         inventoried; no flag -> stdout carries the v0 line,
                         schema v0, no capabilities_schema/backend fields.
                         crates/kwe-daemon: inspect.rs tests grew from 5 to 12 --
                         the original 5 kept (one repurposed: the fake that used
                         to print a v0 record to stdout now proves
                         report-missing, since fd 3 gets nothing), plus new:
                         flooded_report_fd_is_refused_as_report_oversize;
                         unknown_kind_frame_then_nothing_is_report_missing;
                         duplicate_kind_one_frames_is_report_duplicate;
                         garbage_bytes_on_report_fd_is_report_malformed;
                         invalid_inspection_payload_is_report_malformed;
                         old_inspector_without_report_fd_support_is_inspector_failed
                         (argparse without --report-fd, reproducing a real clap
                         usage-error rejection). The valid-report fake now
                         constructs a v1 record and computes its digest in
                         python (sort_keys=True, separators=(",", ":")) --
                         empirically verified to match serde_json::to_vec's
                         canonical form byte-for-byte (the daemon accepts it).
                         165 kwe-daemon tests total (up from 158), including the
                         unrelated scene.inspect RPC/concurrency tests unchanged.
                         scripts/smoke-scene-corpus.sh (KWE_RUN_SCENE_CORPUS_SMOKE=1)
                         still green -- it only exercises the no-flag stdout path.
                         cargo fmt/clippy/test --workspace green (809 passed, 0
                         failed, up from 797). ./scripts/check.sh green end to
                         end including the C++/QML build and qml-typecheck.
Failure/recovery tests:  Covered by Acceptance tests above -- every report-FD
                         failure mode (oversize, missing, duplicate, malformed)
                         and the old-binary skew case are typed, reaped, HOME-
                         dir-cleaned outcomes, never a hang, crash, or
                         reconstructed-from-stdout guess.
Upstream/provenance:     Original; the pre_exec dup2 sequence mirrors
                         supervisor::spawn_worker's existing containment exactly,
                         just inserting one more libc call into the same closure.
Commands run and results: cargo fmt --all -- clean.
                         cargo clippy --workspace --all-targets -- -D warnings --
                         clean.
                         cargo test --workspace -- 809 passed, 0 failed.
                         KWE_RUN_SCENE_CORPUS_SMOKE=1 ./scripts/smoke-scene-corpus.sh
                         -- passed standalone.
                         ./scripts/check.sh -- green end-to-end, including the
                         C++/QML build and qml-typecheck.
Open risks:              report-late and the renderer-worker's report-unavailable
                         remain unimplemented (docs/REPORT_PROTOCOL_V1.md's
                         reason-code table now marks each row's status
                         explicitly) -- neither has a real deadline/worker-report
                         stream to attach to yet.
                         SR-1b covers exactly one version-skew combination (old
                         inspector binary + new daemon); SR-1d is the fuller
                         matrix the plan's §5.3 closing sentence calls for
                         (old/new daemon, worker, and display-bridge upgrade/
                         downgrade/canary rollback).
                         The report-FD wiring pattern (pipe2 + pre_exec dup2 to
                         a fixed fd) now exists in exactly one place
                         (inspect.rs); if/when a renderer-worker report stream
                         lands (SR-1c+), it should reuse this same pattern
                         rather than re-deriving it, but the two pre_exec
                         closures are not (yet) factored into one shared helper
                         -- a small duplication, deliberately not addressed here
                         to keep this slice's diff to the one call site the task
                         specified.
Commit(s):               9b75367
```

## SR-1c — daemon validation policy + the scene apply gate

**Scope note:** the conductor narrowed this slice to the apply-gate half of
the preview below — deciding how an inspection result gates
`wallpaper.apply` (plan §5.2/§5.3's "Apply refuses required missing
features before replacing the live wallpaper"). The renderer-worker's own
report-FD stream (`report-late`, the renderer-worker sense of
`report-unavailable`, extending SR-1b's pipe2/pre_exec pattern to
`supervisor::spawn_worker`) is explicitly OUT of scope here and remains
open (see Open risks) — it was previewed as part of "SR-1c" before this
slice's conductor decisions were made; a later slice picks it up.

Conductor policy decisions (verbatim):

- **(a)** Two capability sets gate Apply, single-sourced in kwe-core.
  Missing-and-blocking → refuse before any renderer starts;
  missing-but-tolerated → apply proceeds and the status carries the
  limitation list. With today's sets, every one of the 60 local corpus
  scenes keeps applying — this slice is behavior-neutral on the current
  corpus and only refuses genuinely unrepresentable content.
- **(b)** Inspection outcome `unknown` (timeout, report-*,
  inspector-unavailable) NEVER blocks Apply in SR-1c — proceed exactly as
  today, attaching the detail. Blocking on infrastructure failure would
  regress applies when the inspector is missing; plan §5.2's Unknown state
  maps to "proceed with note" at this stage.
- **(c)** No inspection caching in SR-1c (each scene apply pays the
  inspection, corpus median 406 ms / max 8.5 s, within the 60 s apply
  window). Cache + invalidation is a recorded open risk for a later SR-1/
  SR-2 slice.

```text
Task:            Daemon validation policy + the scene apply gate (staged
                 preflight): decide how (and whether) an SR-1b scene
                 inspection gates wallpaper.apply, and implement it as a
                 fail-closed refusal before any renderer/canary starts or
                 the current wallpaper is touched.
Milestone/Slice: SR-1c
Goal:            Turn SR-0's frozen capability taxonomy and SR-1b's working
                 inspection pipe into an actual apply-time decision, without
                 regressing any of the 60 local corpus scenes and without
                 blocking applies on inspector infrastructure failures.
Outcome:         crates/kwe-core/src/capabilities.rs (new module, declared/
                 re-exported in lib.rs next to the other scene-concept
                 modules): SCENE_CAPABILITIES_IMPLEMENTED (the 21 taxonomy
                 rows docs/SCENE_CAPABILITIES.md marks experimental,
                 cross-checked against the doc -- exact match, no
                 discrepancy) and SCENE_CAPABILITIES_LIMITATION_TOLERATED
                 (["scene.layer.sound", "scene.lighting"]), both sorted and
                 disjoint (unit-tested), plus a corpus-neutrality test
                 hardcoding the SR-0d 2026-08-28 corpus's required-capability
                 histogram ids.
                 crates/kwe-daemon/src/apply.rs: ApplyConfig/ApplyHandle/
                 ApplyService::new gain an inspect_config: InspectConfig
                 field, threaded exactly like scene_assets_dir already is
                 (main.rs builds one InspectConfig and clones it into both
                 the direct scene.inspect path and this new field -- the
                 apply gate's inspection is the SAME inspection, not a
                 second copy of the containment config). ApplyHandle::apply
                 gains a new stage, RendererKind::Scene only, inserted after
                 old_assignment is captured (the last point before any
                 renderer/wallpaper touch) and before complete_apply (where
                 renderer.start actually happens) -- no reordering of any
                 existing step was needed, so the task's STOP condition did
                 not trigger. The stage runs inspect::run_inspection and
                 branches on outcome: inventoried computes
                 missing = required - IMPLEMENTED, blocking = missing -
                 TOLERATED, limitations = missing (intersect) TOLERATED;
                 blocking non-empty returns a new ApplyError::CapabilityGate
                 immediately (nothing touched yet); otherwise limitations
                 (possibly empty) is threaded into a new StartSpec/
                 WorkerStatus field, capability_limitations: Vec<String>
                 (added the same way scaling/fps already ride from StartSpec
                 through ActiveWorker.spec into the status() builder), and
                 merged into the success JSON as "limitations". incompatible
                 also returns CapabilityGate, with missing: [] and
                 inspection_reason: Some(reason). unknown (any reason)
                 proceeds exactly as before the gate existed, merging
                 {"inspection": "unavailable", "inspection_reason": reason}
                 into the success result. ApplyError::CapabilityGate's
                 .code() deliberately returns the SAME wire string
                 "apply_incompatible" the pre-existing Incompatible(String)
                 variant already used for catalog-kind mismatches -- two
                 different Rust variants/detail shapes, one wire category
                 from an API consumer's point of view (documented in both
                 the variant's doc comment and SUPERVISOR_API_V1.md). A new
                 ApplyError::extra_fields() -> Option<Value> method (None
                 for every other variant) carries the structured missing/
                 inspection_reason payload; main.rs's apply_call() merges it
                 as top-level siblings of "error"/"detail" -- mirroring
                 apply_quarantined's flat response shape/mechanism rather
                 than nesting an object inside "detail". Non-scene kinds
                 (video, web) take zero code path through the new stage.
In scope:        crates/kwe-core/src/capabilities.rs (new), crates/kwe-core/
                 src/lib.rs (module + re-export), crates/kwe-daemon/src/
                 apply.rs (ApplyError::CapabilityGate + extra_fields, the
                 gate stage in apply(), InspectConfig plumbing,
                 with_inspect_config test hook), crates/kwe-daemon/src/
                 main.rs (apply_call() extra-fields merge, InspectConfig
                 cloned into ApplyConfig, the StartSpec/RendererStartParams
                 TryFrom site, 7 new gate tests), crates/kwe-daemon/src/
                 supervisor.rs (StartSpec.capability_limitations,
                 WorkerStatus.capability_limitations, the status() builder
                 line -- and every existing StartSpec test-fixture literal,
                 mechanically updated), crates/kwe-daemon/src/
                 playlist_session.rs (one fabricated_status test fixture),
                 docs/SUPERVISOR_API_V1.md, docs/SR1.md, docs/
                 SCENE_CAPABILITIES.md.
Out of scope:    The renderer-worker's own report-FD stream (report-late,
                 renderer-worker report-unavailable, supervisor::
                 spawn_worker/renderer.start) -- previewed under "SR-1c"
                 before this slice's conductor decisions narrowed it; still
                 open, a later slice's territory. Persisting
                 capability_limitations into the assignment (assignments-v1.
                 json) so it survives a daemon restart -- transient/status-
                 only in this slice (open risk below). Gating the PLAYLIST
                 apply lane's own transaction (apply.rs's
                 PlaylistApplyLane::apply_playlist impl, a separate method
                 from ApplyHandle::apply) -- the task named "wallpaper.apply"
                 specifically; a playlist-driven advance onto a scene wallpaper
                 does not go through this gate in this slice (open risk
                 below). Inspection caching (decision (c), explicit open
                 risk). scene.inspect's own RPC behavior -- unchanged; the
                 gate calls the same run_inspection function scene.inspect
                 already used, it does not touch that RPC path.
Acceptance tests:        crates/kwe-core: 2 new unit tests in capabilities.rs
                         (implemented_and_tolerated_are_sorted_and_disjoint;
                         every_capability_the_local_corpus_actually_
                         required_is_covered, hardcoding the SR-0d
                         2026-08-28 corpus's required histogram ids) -- 160
                         total, up from 158.
                         crates/kwe-daemon: 7 new tests in main.rs's test
                         module (scene_apply_gate_refuses_a_missing_required_
                         capability; scene_apply_gate_proceeds_with_a_
                         tolerated_limitation;
                         scene_apply_gate_refuses_content_the_inspector_
                         itself_rejects; scene_apply_gate_proceeds_when_the_
                         inspector_hangs; scene_apply_gate_proceeds_with_an_
                         unconfigured_inspector;
                         scene_apply_gate_runs_no_inspection_for_a_video_
                         kind_apply (asserted via a marker file the fake
                         inspector would have written if wrongly invoked);
                         scene_apply_gate_retry_re_runs_the_gate_no_negative_
                         caching) -- 172 total, up from 165. Fake inspectors
                         reuse the report-FD wire-format helper SR-1b's
                         inspect.rs test module established (duplicated, not
                         imported -- that module's copy is private to its own
                         test module).
                         818 workspace tests total, up from 809 (158+2 =
                         160 kwe-core, 165+7 = 172 kwe-daemon, the rest
                         unchanged).
                         cargo fmt/clippy/test --workspace green.
                         ./scripts/check.sh green end to end, including the
                         C++/QML build and qml-typecheck.
                         scripts/smoke-scene-corpus.sh (the synthetic 4-item
                         smoke) still green, unaffected -- this slice touches
                         nothing on that path.
Failure/recovery tests:  Covered by Acceptance tests above -- missing-
                         blocking, inspector-refused-content, inspector-hang-
                         under-a-short-wall-timeout, unconfigured-inspector,
                         and the negative-caching check (a fixed inspector on
                         retry actually applies) are all typed, bounded
                         outcomes, never a hang or a silent pass-through.
Upstream/provenance:     Original; the gate reuses SR-1b's run_inspection/
                         InspectConfig verbatim (no new containment code) and
                         mirrors apply_quarantined's existing wire-response
                         construction for the new CapabilityGate variant.
Commands run and results: cargo fmt --all -- clean.
                         cargo clippy --workspace --all-targets -- -D warnings
                         -- clean.
                         cargo test --workspace -- 818 passed, 0 failed.
                         ./scripts/smoke-scene-corpus.sh -- passed standalone.
                         ./scripts/check.sh -- green end-to-end, including the
                         C++/QML build and qml-typecheck.
Open risks:              ~~capability_limitations is transient (StartSpec/
                         WorkerStatus only): a daemon restart loses it even
                         though the underlying scene is unchanged --
                         SR-1e/SR-11 territory (persist alongside the
                         assignment).~~ RESOLVED by SR-1c3 (docs/SR1.md's
                         SR-1c3 addendum) -- capability_limitations is now
                         persisted into the assignment record itself and
                         survives a restart via wallpaper.assignments (the
                         daemon still does not auto-reapply a direct,
                         non-playlist assignment on restart -- see SR-1c3
                         for which of the two worlds this build is in).
                         No inspection cache (decision (c)): every scene
                         apply pays the inspection cost again, including a
                         plain retry -- acceptable at today's corpus timing
                         (median 406 ms / max 8.5 s inside a 60 s apply
                         window) but a cache + invalidation design is
                         deferred to a later SR-1/SR-2 slice.
                         The playlist apply lane (PlaylistApplyLane::
                         apply_playlist) does not run the gate -- a
                         playlist-driven advance onto a scene wallpaper that
                         requires an unimplemented capability is not refused
                         the way a direct wallpaper.apply call is. Narrow
                         and deliberate (the task scoped this slice to
                         "wallpaper.apply" specifically), but worth closing
                         in a follow-up so the gate is not bypassable via the
                         playlist. RESOLVED by SR-1c2 (docs/SR1.md's SR-1c2
                         addendum below) -- the playlist lane now runs the
                         identical gate, single-sourced via
                         scene_capability_gate.
                         The renderer-worker's own report-FD stream
                         (report-late/report-unavailable, extending SR-1b's
                         pattern to spawn_worker) remains unimplemented --
                         previewed under "SR-1c" before this slice's
                         narrower conductor decisions; still a later slice's
                         territory.
Commit(s):               5069b0f
```

## SR-1c2 — the playlist apply lane runs the same scene capability gate

A small follow-up slice closing SR-1c's recorded open risk "playlist lane
ungated" (above), filed after SR-2c merged (trunk `5b80f1d`).

Conductor decisions (verbatim):

- **(a)** Same classification (IMPLEMENTED/TOLERATED via kwe-core), same
  decision (b) proceed-on-unknown. But the FAILURE handling differs by
  lane nature: a playlist entry whose scene is gate-refused
  (blocking-missing or inspector-refused content) is SKIPPED — the
  playlist advances to the next entry exactly the way it already handles
  an entry whose apply fails (mirror that existing skip/advance path;
  find it in playlist_session.rs / the playlist lane in apply.rs). A
  refusal must never wedge or stop the playlist.
- **(b)** The skip is diagnosable: reuse whatever per-entry
  failure/diagnostic record the playlist lane already keeps (event log
  line at minimum: `event=playlist.entry_gate_refused wallpaper=<id>
  missing=<csv>`), and the limitations list rides into the started
  worker's StartSpec.capability_limitations exactly like the direct lane.

```text
Task:            The playlist apply lane runs the SR-1c scene capability
                 gate. Single-sourced classification; only the refusal's
                 handling differs by lane (direct: fail closed; playlist:
                 skip-and-advance).
Milestone/Slice: SR-1c2
Goal:            Close SR-1c's own recorded gap: a playlist-driven advance
                 onto a scene requiring an unimplemented capability was
                 not refused the way a direct wallpaper.apply is,
                 bypassing the gate entirely via the playlist.
Outcome:         crates/kwe-daemon/src/apply.rs: SR-1c's gate
                 classification (previously inlined in ApplyHandle::
                 apply) factored into a new private GateOutcome/
                 scene_capability_gate(inspect_config, content) function
                 -- same three-way branch (inventoried -> blocking refuse/
                 limitations proceed; incompatible -> refuse; unknown ->
                 proceed with notes), returning Result<GateOutcome,
                 ApplyError> where GateOutcome{limitations, notes} mirrors
                 what the direct lane's own gate_notes/spec.
                 capability_limitations assignment already did. Both
                 ApplyHandle::apply (direct lane, unchanged behavior --
                 all 7 existing SR-1c gate tests pass unmodified) and the
                 new call in PlaylistApplyLane::apply_playlist (playlist
                 lane) call this SAME function, RendererKind::Scene only,
                 placed at the identical point in each transaction: right
                 after old_assignment is captured (the rollback target)
                 and before complete_apply (renderer.start). A refusal
                 returns the SAME ApplyError::CapabilityGate either way --
                 un-rolled-back (nothing touched yet in either lane) --
                 only the CALLER'S handling of that Err diverges.
                 crates/kwe-daemon/src/playlist_session.rs: SessionRuntime
                 gains gate_refused_ids: BTreeSet<String> (mirrors
                 quarantined_ids structurally), fed into unavailable_for
                 alongside it -- once an id lands there the decision
                 engine (PlaylistRuntime::tick, kwe-core) routes around it
                 exactly the way it already routes around a crash-
                 quarantined one (existing quarantined_entry_is_never_
                 applied test), so the playlist advances. Unlike
                 quarantine (known upfront, refreshed live from the
                 supervisor) a gate refusal is discovered REACTIVELY: the
                 first apply attempt for that entry still happens once,
                 fold_apply_completions's new Err(ApplyError::
                 CapabilityGate{missing, ..}) arm records the id and logs
                 event=playlist.entry_gate_refused wallpaper=<id>
                 missing=<csv> (decision (b)) -- no backoff/failure count
                 (the gate's answer cannot change on a retry timer the way
                 a transient failure might). reset_apply (playlist
                 switch/deactivation) clears gate_refused_ids: a fresh
                 activation deserves a fresh evaluation.
                 A genuine ordering bug found by this slice's own new
                 test (entry_gate_refused_is_skipped_and_the_playlist_
                 advances first failed with 2 attempts recorded for the
                 refused entry, not 1): fold_apply_completions was called
                 from inside maybe_apply, AFTER tick_session had already
                 computed that tick's `decision`/wallpaper_id from a STALE
                 (pre-fold) unavailable set -- so the very tick that
                 learned of a refusal could still re-dispatch the SAME
                 entry once more before the NEXT tick's fresh unavailable
                 finally excluded it. Fixed by moving the
                 fold_apply_completions() call from maybe_apply to the top
                 of tick_session (right after refresh_quarantine, before
                 unavailable/decision are computed), so decision itself
                 already reflects the freshest fold within the same tick
                 -- not a playlist-lane-specific fix, a general session
                 correctness fix this slice's new adversarial test
                 happened to be the first to exercise (the pre-existing
                 quarantine tests never hit it because quarantine is known
                 BEFORE the first tick, never learned reactively mid-
                 session).
                 docs/SUPERVISOR_API_V1.md: new "SR-1c2" paragraph under
                 the SR-1c gate section. docs/SR1.md (this section) + the
                 SR-1c/epic-close open-risk lines annotated resolved, not
                 deleted. docs/Scene-Rendering-Plan.md's status line
                 annotated the same way.
In scope:        crates/kwe-daemon/src/apply.rs (GateOutcome/
                 scene_capability_gate factored out; apply_playlist's new
                 gate stage + gate_notes merge into its Ok result), crates/
                 kwe-daemon/src/playlist_session.rs (gate_refused_ids,
                 unavailable_for, reset_apply, fold_apply_completions's
                 new CapabilityGate arm, the fold_apply_completions
                 ordering fix, RecordingLane's new gate_refusing builder
                 for the session-level test), crates/kwe-daemon/src/
                 main.rs (4 new playlist-lane gate-classification tests
                 mirroring the existing SR-1c direct-lane ones),
                 docs/SUPERVISOR_API_V1.md, docs/SR1.md, docs/
                 Scene-Rendering-Plan.md.
Out of scope:    Persisting capability_limitations into the assignment
                 (still SR-1c's own open risk, unchanged). Inspection
                 caching (still SR-1c decision (c)'s open risk,
                 unchanged). Re-evaluating a gate refusal without a
                 playlist deactivate/reactivate or a daemon restart --
                 gate_refused_ids has no live-refresh oracle the way
                 quarantined_ids does (open risk below). The renderer-
                 worker's own report-FD stream (unrelated, still SR-1c's
                 own recorded gap).
Acceptance tests:        crates/kwe-daemon: 1 new playlist_session::tests
                         test (entry_gate_refused_is_skipped_and_the_
                         playlist_advances -- 2-entry-visible playlist,
                         RecordingLane.gate_refusing(&["1"]), asserts entry
                         2 is applied, entry 1 is attempted exactly once
                         (not retried), and SessionStatus.unavailable_ids
                         surfaces the refusal) + 4 new main.rs tests
                         calling PlaylistApplyLane::apply_playlist directly
                         against a REAL ApplyHandle + fake inspector,
                         mirroring the SR-1c direct-lane tests exactly:
                         scene_apply_gate_refuses_a_missing_required_
                         capability_through_the_playlist_lane (blocking,
                         CapabilityGate{missing} returned, no switch script
                         runs, supervisor stays Idle),
                         scene_apply_gate_proceeds_with_a_tolerated_
                         limitation_through_the_playlist_lane (limitations
                         in the result + WorkerStatus.
                         capability_limitations), scene_apply_gate_
                         proceeds_when_the_inspector_hangs_through_the_
                         playlist_lane (decision (b), no double-wait),
                         scene_apply_gate_runs_no_inspection_for_a_video_
                         kind_playlist_entry (marker file never written).
                         876 workspace tests total, up from 871.
                         cargo fmt --all -- clean.
                         cargo clippy --workspace --all-targets -- -D
                         warnings -- clean.
                         cargo test --workspace -- 876 passed, 0 failed.
                         ./scripts/check.sh -- exit 0, green end to end.
Failure/recovery tests:  Covered by the acceptance tests above -- the
                         blocking-refuse case IS the failure/recovery
                         case for this slice (nothing touched, supervisor
                         stays Idle, the playlist keeps running against
                         the next entry instead of wedging).
Upstream/provenance:    Original; the factored scene_capability_gate is a
                         byte-for-byte extraction of SR-1c's own existing
                         classification logic (no behavior change to the
                         direct lane, proven by its 7 pre-existing tests
                         passing unmodified) -- no third-party source
                         consulted.
Commands run and results: cargo fmt --all -- clean.
                         cargo clippy --workspace --all-targets -- -D
                         warnings -- clean.
                         cargo test --workspace -- 876 passed, 0 failed.
                         ./scripts/check.sh -- exit 0, green end-to-end.
Open risks:              gate_refused_ids has no live-refresh oracle (unlike
                         quarantined_ids, which the session re-polls from
                         the supervisor every tick): once an entry is
                         gate-refused it stays excluded until the playlist
                         is deactivated/reactivated or the daemon
                         restarts, even if a later build implements the
                         missing capability. Acceptable for this slice
                         (the gate's answer genuinely does not change on
                         its own) but worth a follow-up if a future slice
                         wants "capability added, playlist entry becomes
                         eligible again without a manual reactivate."
                         SR-1c's own two still-open risks (no inspection
                         cache, capability_limitations not persisted) are
                         unchanged by this slice and apply to the playlist
                         lane's gate calls identically.
STOP findings:           None. The fold_apply_completions ordering bug
                         (see Outcome above) was found and fixed within
                         this slice, not left as a STOP -- it was a small,
                         contained, mechanically obvious reordering with
                         no architectural ambiguity, and the direct-lane
                         gate's own placement (already the last point
                         before any touch) was reachable in the playlist
                         lane without any service-ownership restructuring
                         (the task's own STOP condition: apply_playlist
                         already owns self.inspect_config, self.
                         complete_apply, and old_assignment the same way
                         apply() does -- no new plumbing was needed).
Commit(s):               d73934c
```

## SR-1c3 — persist capability_limitations into assignments

A small follow-up slice closing SR-1's "limitations not persisted" open
risk (restart visibility for the manager's limitations notice), filed
after SR-1c2 merged (trunk `d73934c`).

**Which world:** verified before writing any code (main.rs's `fn main()`
has exactly one call site to `handle.apply(...)`, the `wallpaper.apply` RPC
dispatch — nothing at daemon startup iterates the assignment store and
re-applies anything). A direct, non-playlist `wallpaper.apply` assignment
is **NOT auto-reapplied on daemon restart** — this slice does not change
that and does not build one (the task's own explicit boundary). The
PLAYLIST session is the one exception, and it already worked correctly
before this slice: its own restart-restore mechanism (`playlist_session.rs`,
unrelated to `AssignmentStore`, proven by the pre-existing
`playlist_restart_restore_reapplies_the_entry_once` test) re-runs the REAL
`apply_playlist` transaction on restore, which recomputes
`capability_limitations` from a fresh gate call the normal way — so
"persistence" in this slice is specifically about the DIRECT lane's
assignment record: it makes `wallpaper.assignments` report the right
notice for an output the daemon has not re-rendered since a restart, and
lets a manual re-apply (or the playlist's own restore) re-derive it
naturally, exactly as the task specified.

```text
Task:            Persist capability_limitations into the assignment record
                 (mirroring F1's scaling/F2's fps precedent exactly:
                 additive #[serde(default)] field, old records load with
                 the type's default) so wallpaper.assignments survives a
                 daemon restart with the notice intact.
Milestone/Slice: SR-1c3
Goal:            Close SR-1's recorded "limitations not persisted" open
                 risk without building an auto-reapply mechanism this
                 slice never asked for.
Outcome:         crates/kwe-daemon/src/apply.rs: Assignment gains
                 capability_limitations: Vec<String> (#[serde(default)],
                 same additive pattern as F1's scaling field immediately
                 above it in the struct -- doc comment cites it
                 explicitly). complete_apply (the ONE construction site
                 shared by both ApplyHandle::apply and PlaylistApplyLane::
                 apply_playlist -- SR-1c2 already made this shared) sets
                 it from spec.capability_limitations.clone(), which both
                 lanes already populate from the shared
                 scene_capability_gate outcome (SR-1c/SR-1c2) before
                 complete_apply runs -- covers both lanes for free, no
                 lane-specific code needed. Because complete_apply always
                 constructs a FRESH Assignment on every successful apply
                 (store.set replaces, never merges), a later
                 fully-compatible apply on the same output naturally
                 replaces a prior non-empty list with an empty one --
                 also for free, no explicit "clear" step needed.
                 wallpaper.assignments (apply.rs's assignments()) already
                 serializes the whole Assignment via serde, so the new
                 field appears in the RPC response with zero additional
                 code.
                 6 pre-existing Assignment{...} struct-literal test
                 fixtures across apply.rs/main.rs mechanically updated
                 (Rust requires every field; #[serde(default)] only
                 affects deserialization, not struct literals) --
                 seed_assignment (main.rs) split into
                 seed_assignment/seed_assignment_with_limitations so a new
                 test can seed a NON-empty prior list and prove it gets
                 replaced.
                 Manager: audited whether kwe-manager consumes
                 wallpaper.assignments for detail-page UI state today. It
                 does not: ApplyClient::m_assignments (populated by
                 refreshAssignments()) is exposed as a QVariantMap
                 property but grep across every .qml file shows it is
                 NEVER read -- WallpaperDetail.qml's limitations
                 InlineMessage (the exact UI the task named) reads only
                 rendererStatus.capabilityLimitations (a LIVE
                 renderer.status poll), and its own existing code comment
                 already named this precise gap ("capability_limitations
                 is not persisted into the assignment, so a restart clears
                 this notice"). Per the task's explicit branching
                 instruction ("if the manager does NOT consume assignments
                 there today, leave the UI alone and record the gap"), NO
                 QML/C++ changes were made -- building new
                 assignments-consumption plumbing was out of scope for
                 this slice by the task's own words, and is recorded as an
                 open risk below instead.
In scope:        crates/kwe-daemon/src/apply.rs (Assignment field +
                 complete_apply + 2 new tests + 1 fixture update),
                 crates/kwe-daemon/src/main.rs (4 fixture updates,
                 seed_assignment split, 2 new tests: one persist/clear
                 round trip through the direct lane's RPC surface, one
                 confirming the playlist lane persists it too),
                 docs/SUPERVISOR_API_V1.md (the assignment record's field
                 list + the SR-1c gate section's persistence note),
                 docs/SR1.md (this section + SR-1c's/the epic-close open
                 risk lines annotated resolved, not deleted).
Out of scope:    Any daemon-side auto-reapply of a direct (non-playlist)
                 assignment on restart -- verified NOT how this build
                 works (see "Which world" above) and explicitly excluded
                 by the task. Manager UI changes (see Outcome above -- the
                 manager does not consume assignments for UI state today,
                 so none were made; docs/SR1.md's open risk below records
                 this as the natural next step if a future slice wants the
                 InlineMessage to survive a restart in the UI too, not
                 just in the RPC response). docs/Scene-Rendering-Plan.md's
                 status line (left to the conductor per the task).
Acceptance tests:        crates/kwe-daemon: 2 new apply.rs tests --
                         old_records_without_capability_limitations_load_
                         with_an_empty_list (a hand-written pre-SR-1c3
                         JSON record, neither scaling nor
                         capability_limitations present, loads without
                         quarantine and both default correctly) plus the
                         existing a_corrupt_file_is_quarantined_and_the_
                         store_starts_fresh fixtures (which already omit
                         both fields and already passed, now doubling as
                         an implicit backward-compat proof). 2 new main.rs
                         tests -- scene_apply_persists_and_then_clears_
                         capability_limitations_on_a_second_apply (a
                         tolerated-limitation apply, wallpaper.assignments
                         shows it; a second, fully-compatible apply on the
                         SAME output, wallpaper.assignments shows an empty
                         list -- REPLACED, not merged, not stale) and an
                         extension of scene_apply_gate_proceeds_with_a_
                         tolerated_limitation_through_the_playlist_lane
                         (asserts handle.assignments() carries it after a
                         playlist-lane apply too). 182 kwe-daemon tests
                         (up from 180). Full workspace count in Commands
                         below.
                         cargo fmt --all -- clean.
                         cargo clippy --workspace --all-targets -- -D
                         warnings -- clean.
                         cargo test --workspace -- 878 passed, 0
                         failed.
                         ./scripts/check.sh -- exit 0, green end to end.
Failure/recovery tests:  old_records_without_capability_limitations_load_
                         with_an_empty_list IS the failure/recovery case
                         for this slice's own field (a legacy record must
                         load, never quarantine, on the missing key) --
                         covered above, not duplicated here.
Upstream/provenance:    Original; the additive #[serde(default)] pattern
                         is a direct, cited mirror of F1's own scaling
                         field (apply.rs's Assignment struct, same doc-
                         comment convention) -- no third-party source
                         consulted.
Commands run and results: cargo fmt --all -- clean.
                         cargo clippy --workspace --all-targets -- -D
                         warnings -- clean.
                         cargo test --workspace -- 878 passed, 0
                         failed.
                         ./scripts/check.sh -- exit 0, green end-to-end.
Open risks:              The manager's detail-page limitations InlineMessage
                         still reads only the LIVE rendererStatus.
                         capabilityLimitations poll, not applyClient.
                         assignments -- so a user reopening the manager
                         after a daemon restart (before anything
                         re-renders) still will not SEE the notice in the
                         UI, even though wallpaper.assignments now carries
                         the data needed to show it. Closing this needs
                         the manager to actually start consuming
                         wallpaper.assignments for UI state, which it does
                         not do anywhere today (a bigger change than this
                         slice's own scope) -- a natural SR-1c4-shaped
                         follow-up.
                         Direct (non-playlist) assignments are still never
                         auto-reapplied on daemon restart (confirmed, not
                         changed, by this slice) -- the persisted field
                         only helps a client that reads
                         wallpaper.assignments; the actual renderer stays
                         down until a client re-applies or the playlist
                         session's own restore kicks in.
                         SR-1c's other still-open risk (no inspection
                         cache) is unchanged by this slice.
STOP findings:           None. The task's own two forks (which
                         auto-reapply world we are in; whether the manager
                         consumes assignments today) were both resolved by
                         direct verification (grep + reading main.rs's
                         startup path; grep across every manager .qml
                         file) before any code was written, exactly as the
                         task asked -- no code-level STOP condition was
                         named for this slice and none was found.
Commit(s):               (fill in after commit)
```

## SR-1d — report/inspector version-skew matrix

**Scope note:** this slice covers the one-shot `kwe-scene-inspector`'s
version-skew combinations only (the plan §5.3 acceptance row "old/new
adapter matrix", for the pieces that exist today). The renderer-worker's
own report-FD stream does not exist yet (SR-1c's scope note, `docs/SR1.md`
above); a canary/display-bridge upgrade-downgrade matrix has nothing to
attach to until that lands, so it stays out of scope here too — the
"canary rollback" acceptance below is the apply-transaction rollback path
(promotion timeout → stop the renderer, restore the previous wallpaper),
not a canary-generation display-bridge handoff.

```text
Task:            Close the gaps in the report/inspector version-skew matrix:
                 document every daemon/inspector-vintage combination the
                 report-FD path can be exercised under, name the test that
                 proves each row (or mark it documented-only with why), and
                 add the end-to-end tests the existing SR-1b/SR-1c coverage
                 did not already reach.
Milestone/Slice: SR-1d
Goal:            Turn plan §5.3's "SR-1 must test old/new daemon, worker,
                 and display-bridge upgrade/downgrade and canary rollback
                 combinations" into an auditable table with a named proof
                 per row, for the one-shot inspector path that exists today
                 -- and, per this slice's own scope note above, explicitly
                 flag the renderer-worker/display-bridge half as still
                 open rather than silently skip it.
Outcome:         docs/REPORT_PROTOCOL_V1.md: the old "Version skew" prose
                 subsection (SR-1b, one case) is replaced with a
                 "Version-skew matrix" table of 8 rows, each with its
                 proving test named or (2 rows) marked documented-only with
                 the cited rationale (the pre_exec PDEATHSIG containment
                 block for daemon-death-mid-inspection; finalize's
                 report-only parse path for report-vs-stdout precedence).
                 Two stale "see Version skew" cross-references and one
                 stale "SR-1c/a future renderer-worker report slice"
                 mention (now that SR-1c's own scope note records that it
                 did NOT touch the renderer-worker stream) were corrected
                 in the same pass since this slice was already editing the
                 surrounding prose.
                 3 new tests close the gaps the existing SR-1b/SR-1c
                 coverage did not reach: an apply-level (not just
                 inspect-level) old-inspector-binary test, a binary-
                 replaced-on-disk-mid-uptime test (no caching), and a
                 gate-passes-then-renderer-fails rollback test proving the
                 apply gate's presence does not change the existing
                 promotion-timeout rollback path or its "previous wallpaper
                 survives" guarantee.
                 No production code changed -- every row's behavior already
                 existed from SR-1a/SR-1b/SR-1c; this slice is tests +
                 documentation only, per the conductor's framing of this as
                 a small slice.
In scope:        docs/REPORT_PROTOCOL_V1.md (the matrix table + the two
                 stale cross-reference fixes), docs/SR1.md, crates/
                 kwe-daemon/src/inspect.rs (1 new test:
                 replaced_binary_on_disk_is_picked_up_without_caching),
                 crates/kwe-daemon/src/main.rs (2 new tests:
                 scene_apply_gate_proceeds_with_an_old_inspector_binary,
                 scene_apply_gate_pass_then_renderer_failure_rolls_back_to_
                 previous, plus their fake-inspector-old-binary helper).
Out of scope:    Any production code change (STOP condition per the task:
                 "if a test exposes a real bug, STOP and report it... rather
                 than fixing it in this slice" -- no bug was found, so this
                 did not trigger; see "STOP findings" below). The renderer-
                 worker's own report-FD stream and any canary/display-
                 bridge upgrade-downgrade matrix built on top of it (no
                 such stream exists yet -- SR-1c's scope note). Inspection
                 caching (still absent, SR-1c decision (c), unchanged here).
Acceptance tests:        crates/kwe-daemon: 175 tests total, up from 172 --
                         scene_apply_gate_proceeds_with_an_old_inspector_
                         binary and
                         scene_apply_gate_pass_then_renderer_failure_rolls_
                         back_to_previous in main.rs's test module,
                         replaced_binary_on_disk_is_picked_up_without_
                         caching in inspect.rs's test module (the fake
                         inspector reuses inspect.rs's private
                         PYTHON_WRITE_FRAME_HELPER directly, since this test
                         lives in the same module -- no duplication needed
                         here, unlike main.rs's own copy from SR-1c).
                         821 workspace tests total, up from 818 (172+3 = 175
                         kwe-daemon; every other crate unchanged).
                         cargo fmt/clippy/test --workspace green.
                         ./scripts/check.sh green end to end, including the
                         C++/QML build and qml-typecheck.
Failure/recovery tests:  scene_apply_gate_pass_then_renderer_failure_rolls_
                         back_to_previous IS the failure/recovery test for
                         this slice: a gate PASS followed by a renderer that
                         never promotes still rolls back exactly like
                         before the gate existed (apply_failed, the
                         never-promoting renderer stopped, the seeded PRIOR
                         assignment for the output verified unchanged
                         afterward via wallpaper.assignments) -- the plan's
                         "killed inspector/hidden renderer leaves previous
                         wallpaper" acceptance, for the pieces that exist
                         today.
Upstream/provenance:     Original; every new test's fake-inspector/fake-
                         renderer fixtures mirror an existing SR-1b/SR-1c
                         fixture pattern exactly (named in each test's doc
                         comment) rather than inventing a new one.
Commands run and results: cargo fmt --all -- clean.
                         cargo clippy --workspace --all-targets -- -D warnings
                         -- clean.
                         cargo test --workspace -- 821 passed, 0 failed.
                         ./scripts/check.sh -- green end-to-end, including the
                         C++/QML build and qml-typecheck.
Open risks:              The renderer-worker's own report-FD stream and the
                         canary/display-bridge half of plan §5.3's matrix
                         remain unimplemented -- this slice's matrix table
                         says so explicitly per row rather than implying
                         full coverage.
                         The "report FD vs stdout both written" row's
                         "report wins the parse" half is documented-only
                         (a structural fact about finalize's code, not
                         exercised by a dedicated test); if finalize is ever
                         refactored to read stdout for anything, this
                         guarantee should get an explicit regression test
                         at that point.
STOP findings:           None. No test written for this slice exposed a
                         production bug -- every row in the matrix already
                         behaved as documented before this slice; the gaps
                         closed were missing PROOF (an apply-level test, a
                         binary-replacement test, a gate-plus-rollback
                         test), not missing behavior.
Commit(s):               abc29df
```

## SR-1e — manager result-state flow

**Scope note:** the preview above talked about surfacing the FULL
inventoried/incompatible/unknown/report-policy vocabulary; the conductor's
actual SR-1e task narrowed this to the two states SR-1c's apply gate
already made real for the manager to show: the missing-feature refusal
(`apply_incompatible` with `missing`) and the applied-with-limitations
notice (`limitations` / `capability_limitations`). The broader
inventoried/incompatible/unknown vocabulary is daemon-internal
(`docs/REPORT_PROTOCOL_V1.md`) and was never meant to reach the manager
verbatim — only its two apply-time consequences do.

```text
Task:            Manager result-state flow for the SR-1c apply gate: show
                 the missing-feature refusal with friendly capability names
                 (never a bare id list) and hide Try Again for it (retry
                 cannot help); show a persistent, non-blocking notice when a
                 scene applied despite tolerated-missing capabilities.
Milestone/Slice: SR-1e (final SR-1 child)
Goal:            Close the loop plan SR-1 opened: SR-1c's daemon-side gate
                 is invisible to a user until the manager explains WHY a
                 wallpaper was refused (in their words, not a dotted
                 capability id) and WHAT changed about a wallpaper that did
                 apply.
Outcome:         apps/kwe-manager/src/applyclient.h/.cpp: consumeResponse()
                 extracts the new top-level `missing` array alongside
                 error/detail (the same flat-sibling shape apply_quarantined
                 already used) and threads it into finish()/mapError() as a
                 new parameter; m_lastFailedMissing stores it (mirroring
                 m_lastFailedQuarantined's storage, write-only in this slice
                 -- nothing currently reads it back the way retry() reads
                 the quarantine flag, since apply_incompatible is never
                 retried). mapError()'s apply_incompatible branch: non-empty
                 missing -> "This wallpaper needs features this version
                 does not support yet: <friendly names>. Your current
                 wallpaper is unchanged.", friendly names via a new
                 Q_INVOKABLE static ApplyClient::friendlyCapabilityName(id)
                 (the task's exact 12-entry table, tr()'d per entry,
                 unrecognized ids pass through verbatim); empty missing
                 (the pre-existing kind-mismatch shape) keeps today's
                 generic message unchanged. Try Again: verified
                 failedMethod (not m_lastFailedQuarantined) is what
                 WallpaperDetail.qml's Try Again action actually gates
                 (`visible: applyClient.failedMethod !== ""`) and that it
                 was a GENERIC retry affordance for every Apply/Restore
                 failure, not quarantine-only -- so finish()'s failure
                 branch now computes `retryable = method == Apply &&
                 errorCode != "apply_incompatible"` and only sets
                 failedMethod to "apply" when retryable, gating off the
                 WHOLE apply_incompatible code (both the missing-feature
                 shape and the pre-existing kind-mismatch shape -- retry
                 cannot help either one).
                 apps/kwe-manager/src/rendererstatus.h/.cpp: a new
                 capabilityLimitations QStringList property, added to the
                 existing renderer.status poll exactly like
                 phase/wallpaperId/detail already flow (a JSON array read
                 off `status`, change-detected alongside the other three
                 fields, one statusChanged() emission) -- empty by default
                 (absent field, non-scene wallpaper, or nothing tolerated).
                 apps/kwe-manager/qml/WallpaperDetail.qml: one new
                 Kirigami.InlineMessage (Information severity, matching the
                 page's existing InlineMessage pattern exactly -- no
                 explicit icon.name, relying on Kirigami's built-in
                 per-severity icon, which is what every sibling message on
                 this page already does) placed right after the "Applied
                 %1 to %2" success message. Deliberately re-derived from
                 rendererStatus.capabilityLimitations (not a one-shot toast
                 tied to applyClient's transient applied* fields): it
                 reads correctly again after reopening the manager, and
                 disappears only when the daemon's own status no longer
                 reports a limitation. No close button (passively
                 informative, not dismissible) -- the "quarantined"
                 InlineMessage in GalleryPage.qml is the closest existing
                 behavioral analog (status-derived, not a one-shot
                 confirmation) and follows the same no-close-button
                 convention; a close button bound to a live QML expression
                 would silently break the binding on first dismiss, a
                 pre-existing footgun this slice did not need to introduce
                 into a new message.
                 Mapping-exposure decision (task's explicit either/or):
                 picked the Q_INVOKABLE-on-ApplyClient option over
                 duplicating the table in RendererStatus/a status model --
                 smaller diff (one method, reused by both the refusal
                 message and the limitations notice, one source of truth)
                 vs. a second copy of a 12-entry tr() table that could drift
                 from the first. applyClient is already a global QML
                 context property reachable from every page, so
                 WallpaperDetail.qml calls
                 applyClient.friendlyCapabilityName(id) directly on each
                 rendererStatus.capabilityLimitations entry.
                 docs/BETA_M4.md's manager apply-message table gained two
                 rows (the missing-feature apply_incompatible shape; the
                 limitations success notice). docs/SUPERVISOR_API_V1.md
                 needed no changes -- every field name exposed
                 (missing/limitations/capability_limitations) already
                 matches what SR-1c documented there.
                 No Rust code changed in this slice (task's own expectation
                 confirmed by inspection: `git status` shows only
                 apps/kwe-manager/** and docs/** touched).
In scope:        apps/kwe-manager/src/applyclient.h, apps/kwe-manager/src/
                 applyclient.cpp, apps/kwe-manager/src/rendererstatus.h,
                 apps/kwe-manager/src/rendererstatus.cpp, apps/kwe-manager/
                 qml/WallpaperDetail.qml, apps/kwe-manager/tests/
                 applyclienttest.cpp (extended: Fail gained a `missing`
                 field, 2 new tests), apps/kwe-manager/tests/
                 rendererstatustest.cpp (NEW -- no dedicated RendererStatus
                 test file existed before this slice; 2 new focused tests),
                 apps/kwe-manager/tests/CMakeLists.txt (new
                 kwe-renderer-status-test target, mirroring
                 kwe-apply-client-test's registration exactly), docs/
                 BETA_M4.md, docs/SR1.md.
Out of scope:    Persisting capability_limitations into the assignment so
                 the limitations notice survives a daemon restart -- SR-1c's
                 recorded open risk, unchanged here (the QML doc comment at
                 the new InlineMessage names it explicitly). Any Rust
                 change (none was needed; the daemon-side fields this slice
                 consumes were all already shipped by SR-1c). The broader
                 inventoried/incompatible/unknown vocabulary surfacing
                 (this slice's scope note above).
Acceptance tests:        apps/kwe-manager: kwe-apply-client-test grew from
                         25 to 27 (2 new:
                         missingFeatureRefusalNamesFriendlyCapabilitiesAndHidesTryAgain,
                         emptyMissingKeepsTheGenericIncompatibleMessageAndHidesTryAgain),
                         both passing against the existing StubDaemon
                         (extended with a `missing` field on Fail). A new
                         kwe-renderer-status-test target (4 slots: init/
                         cleanup + 2 tests --
                         capabilityLimitationsRoundTripsFromTheStatusJson,
                         capabilityLimitationsIsEmptyWhenTheFieldIsAbsent),
                         a minimal QLocalServer stand-in for renderer.status
                         mirroring StubDaemon's style.
                         qmllint clean via scripts/qml-typecheck.sh (used
                         directly and via ./scripts/check.sh): "no
                         unresolved types".
                         ctest: all 10 manager test targets (including the
                         2 touched and the 1 new one) pass; cd build/cmake
                         && ctest green end to end.
                         cargo fmt/clippy/test --workspace green (rust
                         untouched, confirmed unaffected).
                         ./scripts/check.sh green end to end, including the
                         C++/QML build and qml-typecheck.
Failure/recovery tests:  missingFeatureRefusalNamesFriendlyCapabilitiesAndHidesTryAgain
                         covers both halves of the UI rule at once: the
                         friendly-name mapping (including an id the table
                         does not recognize, proving it passes through
                         verbatim rather than being hidden) and the
                         Try-Again-hidden guarantee, in the same assertion
                         pass, against a real ApplyClient/StubDaemon round
                         trip rather than a mocked mapError() call.
Upstream/provenance:     Original; every new QML/C++ pattern (InlineMessage
                         shape, the daemon-json -> member -> notify ->
                         property flow, the StubDaemon/StubStatusDaemon test
                         style) mirrors an existing one in the same files
                         rather than inventing a new convention.
Commands run and results: cmake --build build/cmake --parallel -- clean.
                         scripts/qml-typecheck.sh -- "no unresolved types".
                         cd build/cmake && ctest -- all 10 targets passed.
                         cargo fmt --all -- --check -- clean (no rust
                         changes).
                         cargo clippy --workspace --all-targets -- -D
                         warnings -- clean.
                         cargo test --workspace -- 821 passed, 0 failed
                         (unchanged from SR-1d).
                         ./scripts/check.sh -- green end-to-end, including
                         the C++/QML build and qml-typecheck.
Open risks:              capabilityLimitations (and therefore the
                         applied-with-limitations notice) is transient: it
                         reads empty again after a daemon restart even
                         though the underlying scene's capability gap is
                         unchanged, because SR-1c never persisted
                         capability_limitations into the assignment. Noted
                         explicitly at the QML message and here; carried
                         forward from SR-1c's own recorded open risk rather
                         than fixed in this UI-only slice.
                         m_lastFailedMissing is currently write-only (stored
                         per the task's instruction, mirroring
                         m_lastFailedQuarantined's storage pattern, but
                         nothing reads it back the way retry() reads the
                         quarantine flag) -- harmless today since
                         apply_incompatible is never retried, but a future
                         slice that wanted to re-show the missing list
                         without a fresh daemon round trip has somewhere to
                         read it from already.
STOP findings:           None -- no bug was found in the daemon-side SR-1c
                         work this slice builds on; every change here is
                         additive manager-side UI/tests.
Commit(s):               13fd25a
```

## SR-1 epic — COMPLETE

All five children (SR-1a report protocol v1 doc + codec crate, SR-1b
report-FD wiring inspector+daemon, SR-1c the scene apply gate, SR-1d the
version-skew matrix, SR-1e the manager result-state flow) are merged:
SR-1a `1c2b65e`, SR-1b `9b75367`, SR-1c `5069b0f`, SR-1d `abc29df`, SR-1e
`13fd25a`. Plan §5.3/§8's SR-1 acceptance bullets are
met by the pieces that exist today: a real report-FD wire format and
schema (SR-1a) actually carrying a `scene-inspection-v1` record between
two real processes (SR-1b), a fail-closed apply-time decision built on it
(SR-1c) that a user can see and understand (SR-1e), with the old/new
adapter matrix for the one-shot inspector path documented and tested
(SR-1d). Recorded open risks carried forward past the epic boundary, none
of them silently dropped: **no inspection cache** (SR-1c decision (c) —
every scene apply re-pays the inspection; corpus timing keeps this inside
the apply window today, but a cache design is still open);
~~**limitations not persisted**~~ (capability_limitations/`limitations`
was transient daemon- and manager-side; a restart lost the notice until
the next apply — SR-1c and SR-1e both flagged this at the point it
mattered) — **RESOLVED by SR-1c3** (above): capability_limitations is now
persisted into the assignment record and survives a restart via
`wallpaper.assignments`, though the manager's own detail-page UI does not
yet read that field (SR-1c3's own recorded gap: it never consumed
`wallpaper.assignments` for UI state to begin with, so this slice did not
wire new plumbing onto it — see SR-1c3's addendum);
~~**playlist lane ungated**~~ (`PlaylistApplyLane::apply_playlist` did not
run the SR-1c gate — a playlist-driven advance onto a scene requiring an
unimplemented capability was not refused the way a direct
`wallpaper.apply` is, SR-1c's recorded gap) — **RESOLVED by SR-1c2**
(above): the playlist lane now runs the identical, single-sourced gate;
only the refusal's handling differs (skip-and-advance instead of a hard
failure); and **render-report kind 2
reserved but unused** (`docs/REPORT_PROTOCOL_V1.md`'s `SceneRenderReportV1`
frame kind, and the renderer-worker's own report-FD stream generally —
`report-late`/the renderer-worker sense of `report-unavailable` — never
landed in SR-1; SR-1c and SR-1d both scoped themselves away from it
explicitly rather than silently missing it). None of these four risks
block using what SR-1 shipped; each is a named, findable next step rather
than an undocumented gap. Two of the four (playlist lane ungated,
limitations not persisted) are now closed by follow-up addenda (SR-1c2,
SR-1c3, both above); their history is annotated above, not deleted, per
this doc's standing convention for a closed risk. The other two (no
inspection cache, render-report kind 2) remain open.
