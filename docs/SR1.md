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
Commit(s):               <filled after commit; same commit as this file>
```

## SR-1c — daemon validation, policy, and the apply gate

- Wires `report-late` (an apply-window deadline for a renderer worker's own
  report) and the renderer-worker sense of `report-unavailable`
  (`docs/REPORT_PROTOCOL_V1.md`) into real daemon behavior — SR-1b already
  implemented `report-malformed`/`report-missing`/`report-duplicate` for the
  one-shot `scene.inspect` path.
- Extends the report-FD wiring pattern SR-1b established
  (`crates/kwe-daemon/src/inspect.rs`) to the long-lived renderer worker
  (`supervisor::spawn_worker`, `renderer.start`), which SR-1b left untouched.
- Decides how (and whether) an inspection/render result gates
  `wallpaper.apply` — the "typed failure/recovery action" plan §5.3 calls for.

## SR-1d — old/new version-skew matrix

- Tests every combination of old/new daemon, old/new worker/inspector
  binary, and canary rollback the plan's §5.3 closing sentence names
  ("SR-1 must test old/new daemon, worker, and display-bridge upgrade/
  downgrade and canary rollback combinations") — an old worker with no
  `--report-fd` support must resolve to `report-unavailable`, never a crash
  or a reconstructed-from-stderr guess.

## SR-1e — manager result-state flow

- Surfaces the daemon's report-derived result state (inventoried/incompatible/
  unknown, and the typed report-policy reasons above) through to
  `apps/kwe-manager`'s UI, so a user sees why a wallpaper was refused instead
  of a bare failure.
