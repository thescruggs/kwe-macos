# Report protocol v1

## Purpose

`docs/Scene-Rendering-Plan.md` §5.3 ("Structured reports") calls for
versioned `scene-inspection-v1` and `scene-render-report-v1` JSON records
delivered over "a dedicated inherited report socket/file descriptor with a
versioned length-delimited envelope, not worker stdout or stderr" — moving
structured reporting off the channels a renderer/inspector process already
uses for other things (stdout carries the v1 input-acknowledgement contract;
stderr is a bounded diagnostic ring, never parsed as data). This document is
that envelope's wire format, frozen for `kwe-report-protocol` (SR-1a). The
containment/FD plumbing that gets a byte stream from a child process to this
codec, and the malformed/missing/duplicate policy for the one-shot
`scene.inspect` path specifically, is SR-1b (implemented); the broader
daemon-side policy for what a malformed/missing/duplicate/late report means
for an APPLY decision (a long-lived renderer worker, not the one-shot
inspector) is SR-1c.

Cross-references: `docs/SCENE_CAPABILITIES.md` (the frozen v1 capability ID
taxonomy this record's `required`/`detected` arrays draw from) and
`docs/SUPERVISOR_API_V1.md` (the daemon's existing JSON-line control-socket
API, `scene.inspect` in particular — unrelated transport, same daemon).

## Codec vs. policy

This crate (`kwe-report-protocol`) only defines and codecs bytes:

- the frame wire format (below);
- `FrameReader`, which surfaces frames in the order they arrive and
  enforces only the stream-shape bounds (frame count, total bytes, per-frame
  bytes) — never a semantic judgment about what a duplicate or out-of-order
  frame means;
- `validate_inspection`, a pure function from bytes to a validated
  `scene-inspection-v1` JSON value or a typed shape/digest error.

Everything about what to DO with a stream — is a second `scene-inspection-v1`
frame a protocol violation (duplicate) or a legitimate resend; does an apply
gate on a malformed report; what timeout makes a report "late" — is SR-1c's
daemon-side policy, built on top of this codec, not inside it. This split
mirrors the rest of the workspace's protocol crates
(`kwe-frame-protocol`/`kwe-input-protocol` codec bytes/shared-memory layout;
the daemon and worker own what those bytes mean).

## Report FD convention

- The daemon creates a pipe, keeps the read end, and passes the write end's
  fd number to the child as `--report-fd <n>` (the child `dup2`s its own
  write end onto `<n>` before it needs it, then closes the original).
- The daemon owns the read end exclusively: no other code reads from it, and
  it is closed (along with the child's write end, via normal process
  teardown) on every generation change — a retired worker's report stream is
  never read after its replacement is promoted.
- The child writes zero or more frames, then closes its write end (a normal,
  intentional close — not a crash) once it has nothing more to report for
  this generation.
- stdout's existing input-acknowledgement contract (`kwe-input-protocol`,
  newline-delimited JSON `input_ack` messages) is unchanged — reports do
  not travel there, and nothing about moving reports to a dedicated FD
  touches the stdout contract.

**Implemented (SR-1b):** the one-shot `scene.inspect` path
(`crates/kwe-daemon/src/inspect.rs`'s `run_inspection`/`supervise`).
`kwe-scene-inspector` gained `--report-fd <n>` (absent: unchanged v0-on-
stdout behavior; present: nothing on stdout, one `scene-inspection-v1` frame
on `n`). The daemon creates the pipe with `libc::pipe2(..., O_CLOEXEC)` and,
in the child's `pre_exec`, `dup2`s the write end onto a fixed fd 3 —
inserted into the exact same closure that already runs
`setpgid`/`PR_SET_PDEATHSIG`/the parent-pid check/`PR_SET_NO_NEW_PRIVS`/
`apply_resource_limits` (mirroring `supervisor::spawn_worker`'s pre_exec
block) — then reads the resulting stream with `FrameReader` once the child
has exited 0. `docs/SUPERVISOR_API_V1.md`'s "Scene inspection" section is
the RPC-facing description of the resulting reason codes.

**Not yet implemented:** the long-lived RENDERER worker's own report stream
(`supervisor::spawn_worker`, `renderer.start`) is a separate, still-future
wiring untouched by SR-1b. `report-late` (only meaningful once there is an
apply-window deadline to be late against, which the one-shot inspector call
does not have — it has only its own wall-clock timeout, already `timeout`)
and `report-unavailable` (defined below for a renderer WORKER predating the
report FD; the one-shot inspector's own old-binary skew resolves
differently — see "Version skew") remain unimplemented/unused by SR-1b.

Reason codes SR-1c/a future renderer-worker report slice are expected to use
(named here so this document is the single place a future reader learns the
full intended vocabulary, even where only the inspector path implements it
today):

| Reason code | Meaning |
|---|---|
| `report-malformed` | A frame or its payload failed a typed check (bad magic/flags/reserved, an oversize/truncated frame, a stream-cap violation, or a `scene-inspection-v1` payload that fails `validate_inspection`). Implemented (SR-1b, `scene.inspect`). |
| `report-missing` | The child exited 0 but its report stream contained zero frames of the expected kind (either no frames at all, or only frames of a kind the daemon does not act on — e.g. `Unknown`). Implemented (SR-1b, `scene.inspect`). |
| `report-duplicate` | A second frame of a kind that must appear at most once in a stream arrived (daemon policy, not a codec-level concept — see "Codec vs. policy"). Implemented (SR-1b, `scene.inspect`). |
| `report-late` | A report arrived after the daemon's own deadline for it (a generation/apply-window boundary, not a wire-level timeout this crate knows about). Not yet implemented — no code path has an apply-window deadline to compare against yet. |
| `report-unavailable` | A renderer WORKER predates the report FD entirely (an old binary that never gets `--report-fd`); the daemon must not reconstruct a policy decision from stdout/stderr in this case (plan §5.3). Not yet implemented (no renderer-worker report stream exists yet); the one-shot inspector's analogous old-binary case resolves as `inspector-failed` instead — see "Version skew". |

### Version skew (SR-1b: one case implemented and tested; SR-1d builds the fuller matrix)

An old `kwe-scene-inspector` binary that predates `--report-fd` rejects the
daemon's `--report-fd 3` argument the way clap itself rejects any unknown
flag: a usage error printed to stderr, exit 2. The daemon always passes
`--report-fd` unconditionally — there is no daemon-side fallback to the old
stdout contract — so this resolves as `inspector-failed`, with that usage
error visible in `stderr_tail`, never a crash and never a decision
reconstructed from stdout. This is the only skew combination SR-1b
implements and tests
(`crates/kwe-daemon/src/inspect.rs`'s
`old_inspector_without_report_fd_support_is_inspector_failed`); daemon and
inspector ship in the same package, so this is a partial-upgrade window
(the package half-updated, or an operator pointing `--inspector` at a stray
old binary), not a supported long-term compatibility mode. SR-1d is the
fuller old/new daemon x worker x display-bridge upgrade/downgrade/
canary-rollback matrix plan §5.3 calls for.

## Wire format

A report stream is a unidirectional child→daemon byte stream. It is a
sequence of frames; nothing precedes the first frame and nothing follows the
last one except EOF (the child closing its write end).

Frame = a 12-byte header, then `payload_len` bytes of payload.

| Bytes | Field | Value |
|---|---|---|
| 0..4 | magic | `b"KWR1"` |
| 4 | `kind` | `u8` — see Kinds below |
| 5 | `flags` | `u8`, MUST be `0` in v1 — nonzero is a typed error (`BadFlags`), not a silently-ignored bit field |
| 6..8 | reserved | `u16`, little-endian, MUST be `0` — nonzero is a typed error (`BadReserved`) |
| 8..12 | `payload_len` | `u32`, little-endian, `<= 65536` |
| 12..12+payload_len | payload | `payload_len` raw bytes |

All multi-byte integers are little-endian, matching every other binary
protocol in this workspace (`kwe-frame-protocol`'s shared-frame header).
`flags` and the reserved bytes being hard requirements (not "ignore unknown
bits") is deliberate: v1 has no defined flag semantics, so a nonzero value
can only mean either a version this reader does not understand or a
corrupted stream, and both cases should fail typed rather than silently
proceed as if the flag meant nothing.

## Kinds

| `kind` | Name | Payload | Status |
|---|---|---|---|
| 1 | `scene-inspection-v1` | JSON, `validate_inspection`-checked | Defined by this document |
| 2 | `scene-render-report-v1` | JSON | Reserved. Its producer arrives with the render-report slices (plan §5.3's "phase and typed failure/recovery action", frame timing, etc.); this codec already carries kind 2 opaquely (the payload passes through `Frame.payload` untouched), so no codec change is needed when that producer lands — only a schema/validator, the same shape `validate_inspection` is for kind 1. |
| anything else | — | opaque | `FrameReader` reads the frame (so the stream stays correctly positioned for the next one), returns it as `Frame { kind: FrameKind::Unknown(n), payload }`, and does not attempt to interpret the payload. This is what makes the wire format additive: a daemon built against this document can read a stream from a future writer that emits a kind it has never heard of, without losing its place in the stream or corrupting its accounting of the stream caps below. Whether an `Unknown` frame in a given stream is fine (forward-compatible metadata) or a protocol violation (SR-1c never expects a kind 3 yet) is daemon policy, not this crate's job. |

Duplicate-kind policy (e.g. two `kind = 1` frames in one stream) is
explicitly daemon policy (`report-duplicate` above), not a codec concern:
`FrameReader::next_frame` returns every well-formed frame it reads, in
arrival order, with no memory of which kinds it has already seen. SR-1c
decides, at the daemon layer, whether a repeated kind is acceptable.

## Stream caps (reader-enforced)

`FrameReader` enforces these bounds itself, independent of anything the
payload claims about itself:

| Cap | Value | Constant |
|---|---|---|
| Frames per stream | 16 | `MAX_FRAMES_PER_STREAM` |
| Total payload bytes per stream | 1 MiB (1,048,576) | `MAX_TOTAL_PAYLOAD_BYTES` |
| Payload bytes per frame | 64 KiB (65,536) | `MAX_PAYLOAD_BYTES` |

An `Unknown`-kind frame still counts against every one of these caps — a
flood of frames in a kind this reader does not recognize is not a way around
the bounds.

Per plan §9 ("Every bound defines behavior at `limit-1`, `limit`, and
`limit+1`: accept, degrade, refuse, or terminate"), the behavior at each
boundary, and the crate test that proves it:

| Bound | limit-1 | limit | limit+1 | Test |
|---|---|---|---|---|
| Frames per stream | 15: accept | 16: accept | 17: `FrameCountExceeded` | `frame_count_cap_is_enforced_at_the_boundary` |
| Total payload bytes | just under 1 MiB: accept | exactly 1 MiB: accept | over 1 MiB: `TotalBytesExceeded` | `total_payload_cap_is_enforced_at_the_boundary` |
| Payload bytes per frame | 65,535: accept | 65,536: accept | 65,537: `PayloadOversize` | `payload_len_cap_is_enforced_at_the_boundary` |

One numeric coincidence worth documenting explicitly:
`MAX_FRAMES_PER_STREAM * MAX_PAYLOAD_BYTES == MAX_TOTAL_PAYLOAD_BYTES`
exactly (16 * 64 KiB = 1 MiB). This means a stream cannot exceed the
total-byte cap without also being at frame 17 or later — the two caps are
not independent at the boundary. `FrameReader::next_frame` checks the byte
cap before the frame-count cap specifically so `TotalBytesExceeded` stays
independently reachable (a stream of 16 max-size frames plus one more small
frame reports `TotalBytesExceeded`, not `FrameCountExceeded`, even though it
is technically also the 17th frame); a stream of 17+ frames that never
approaches the byte cap still reports `FrameCountExceeded` as expected,
since its running total stays low. See the test's own comment for the
worked example.

## `scene-inspection-v1` schema

`scene-inspection-v1` is SR-0's draft `scene-feature-inventory-v0`
(`docs/SCENE_CAPABILITIES.md`) frozen and extended for the report FD. It
differs from v0 in exactly three ways:

1. `"schema"` is `"scene-inspection-v1"` instead of
   `"scene-feature-inventory-v0"`.
2. A new required `"capabilities_schema"` field, always
   `"scene-capabilities-v1"` — names which frozen `docs/SCENE_CAPABILITIES.md`
   taxonomy version the record's capability IDs were drawn from, independent
   of the record's own wire schema version.
3. A new required, nullable `"backend"` field (`null` or a JSON object) —
   reserved for renderer backend/GPU/driver identity (plan §5.3: "GPU/driver,
   compiler"); `null` until a producer populates it (kwe-scene-inspector, a
   pure classification/hash tool, has no backend of its own and always emits
   `null`).

Every other field, and its meaning, is unchanged from v0.

### v0 → v1 field mapping

| v0 (`scene-feature-inventory-v0`) | v1 (`scene-inspection-v1`) | Change |
|---|---|---|
| `schema` | `schema` | Value changes to `"scene-inspection-v1"` |
| — | `capabilities_schema` | New, always `"scene-capabilities-v1"` |
| `content` | `content` | Unchanged shape |
| `inspector` | `inspector` | Unchanged shape |
| `outcome` | `outcome` | Unchanged |
| `reason` | `reason` | Unchanged |
| `required` | `required` | Unchanged |
| `detected` | `detected` | Unchanged |
| `unknown` | `unknown` | Unchanged shape |
| `bounds` | `bounds` | Unchanged shape |
| — | `backend` | New, `null` or an object |
| `digest` | `digest` | Same computation rule, now over the v1 record (see below) |

### Full field list (as `validate_inspection` checks it)

```json
{
  "schema": "scene-inspection-v1",
  "capabilities_schema": "scene-capabilities-v1",
  "content": { "hash": "sha256:...", "source_bytes": 0, "kind": "pkg|json-dir" },
  "inspector": { "build": "dev", "abi": 0 },
  "outcome": "inventoried | unknown | incompatible",
  "reason": "ok | timeout | oversize | parse-error | ...stable reason codes",
  "required": ["scene.layer.image", "...sorted, deduplicated capability IDs"],
  "detected": [
    { "capability": "scene.effects", "count": 0,
      "objects": ["...first N stable logical IDs, sorted"], "truncated": false }
  ],
  "unknown": {
    "keys": 0, "types": 0, "objects": 0,
    "samples": ["...first N key paths, sorted"], "truncated": false
  },
  "bounds": { "wall_ms": 0, "peak_bytes": 0, "limits_hit": ["...reason codes"] },
  "backend": null,
  "digest": "..."
}
```

`validate_inspection` checks, top to bottom: `schema` equals
`SCENE_INSPECTION_SCHEMA`; `capabilities_schema` equals
`SCENE_CAPABILITIES_SCHEMA`; `content` is an object with `hash` (string),
`source_bytes` (number), `kind` (string); `inspector` is an object with
`build` (string), `abi` (number); `outcome` and `reason` are strings;
`required` and `detected` are arrays (element shape is not itself validated
by this function — that is SR-1c's or the consumer's job once a
capability/record-level policy exists); `unknown` is an object with numeric
`keys`/`types`/`objects`, an array `samples`, and a boolean `truncated`;
`bounds` is an object with numeric `wall_ms` and an array `limits_hit`
(`peak_bytes` is not separately required by this check); `backend` is
`null` or an object; `digest` is a string. A missing field and a
present-but-wrong-type field are always distinguished (`MissingField(path)`
vs. `WrongType(path)`, where `path` is the dotted field path, e.g.
`"content.source_bytes"`).

### Digest rule

The digest is the hex-encoded SHA-256 of the record serialized with its own
`"digest"` field set to `""` — byte-for-byte the same rule
`kwe-scene-inspector`'s `build_record` already uses
(`crates/kwe-scene-inspector/src/main.rs`):

```
digest = hex(sha256(serde_json::to_vec(record_with_digest_field_set_to_empty_string)))
```

This is deterministic only because `serde_json::Value`'s object
representation is a `BTreeMap` in this workspace, so `serde_json::to_vec`
always emits object keys in sorted order regardless of the order they were
inserted in. This rule requires the `preserve_order` `serde_json` feature to
stay off across the whole workspace (it is off today — no crate enables
it); turning it on anywhere in the dependency graph would make key order,
and therefore the digest, insertion-order dependent instead of a pure
function of the record's content, silently breaking every already-computed
digest's verifiability. `kwe-report-protocol`'s own crate doc carries the
same warning next to `validate_inspection`.

`validate_inspection` does not itself enforce the 64 KiB payload cap — it
assumes its caller already read the payload through a capped path
(`FrameReader`, or SR-1c's daemon-side frame handling), and degrades
gracefully rather than unsoundly on an uncapped buffer regardless
(`serde_json::from_slice` parses or errors on any input size; it never
panics).
