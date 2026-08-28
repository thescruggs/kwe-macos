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
Commit(s):               <filled after commit; same commit as this file>
```

## SR-1b — inspector/daemon report-FD wiring

- `kwe-scene-inspector` gains `--report-fd <n>` and emits its
  `scene-inspection-v1` record as one `kwe-report-protocol` frame on that fd
  instead of (or alongside, transitionally) stdout.
- The daemon (`crates/kwe-daemon/src/inspect.rs`/`supervisor.rs`) creates the
  pipe, dup2's the write end into the child before exec, owns the read end
  exclusively, and closes both ends on generation change — mirroring the
  existing stdin/stdout/stderr pipe wiring in `spawn_worker`.
- No daemon-side policy decision yet (that is SR-1c): SR-1b's daemon just
  reads frames off the fd with `kwe-report-protocol::FrameReader` and can log
  what it received.

## SR-1c — daemon validation, policy, and the apply gate

- Wires `report-malformed`/`report-missing`/`report-duplicate`/`report-late`/
  `report-unavailable` (`docs/REPORT_PROTOCOL_V1.md`) into real daemon
  behavior: a bounded read-with-timeout loop over the report fd,
  `validate_inspection` on every `scene-inspection-v1` frame, and the policy
  decisions the codec deliberately left out (duplicate-kind handling,
  lateness relative to the daemon's own deadline).
- Decides how (and whether) an inspection result gates `wallpaper.apply` —
  the "typed failure/recovery action" plan §5.3 calls for.

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
