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
Open risks:              capability_limitations is transient (StartSpec/
                         WorkerStatus only): a daemon restart loses it even
                         though the underlying scene is unchanged --
                         SR-1e/SR-11 territory (persist alongside the
                         assignment).
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
                         playlist.
                         The renderer-worker's own report-FD stream
                         (report-late/report-unavailable, extending SR-1b's
                         pattern to spawn_worker) remains unimplemented --
                         previewed under "SR-1c" before this slice's
                         narrower conductor decisions; still a later slice's
                         territory.
Commit(s):               5069b0f
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
Commit(s):               (fill in after commit)
```

## SR-1e — manager result-state flow

- Surfaces the daemon's report-derived result state (inventoried/incompatible/
  unknown, and the typed report-policy reasons above) through to
  `apps/kwe-manager`'s UI, so a user sees why a wallpaper was refused instead
  of a bare failure.
