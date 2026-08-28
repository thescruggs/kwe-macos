# Shader helper protocol v1

## Purpose

`docs/Scene-Rendering-Plan.md` §4.3/§8 (SR-3, "killable shader service")
calls for shader compilation to run in a separate, killable helper process
rather than in-process inside the scene renderer — the same containment
philosophy `kwe-scene-inspector` (SR-0b) already applies to scene inventory:
a bounded, watchdog-guarded child the daemon can kill without touching the
renderer it protects. This document is that helper's wire contract, frozen
for `kwe-shader-compiler` (SR-3a). `docs/SR3.md` records the child slice
list and the conductor decisions this document implements; this file is the
protocol reference other slices (and other readers) come back to.

Cross-references: `docs/REPORT_PROTOCOL_V1.md` (the `KWR1` frame codec this
document reuses byte-for-byte — same header shape, same `kwe-report-protocol`
crate, same `FrameReader`/`write_frame` functions; only the KIND namespace
and the stream-level caps differ, both documented below) and
`docs/Scene-Rendering-Plan.md` §4.3 (the killable-shader-service goal this
protocol serves).

## Relationship to the report-FD protocol — shared codec, unrelated channels

`docs/REPORT_PROTOCOL_V1.md`'s "Codec vs. policy" split applies here
unchanged: `kwe-report-protocol`'s `FrameReader`/`write_frame`/`StreamCaps`
only define and codec bytes. But the CHANNEL this protocol runs over is a
different shape from the report-FD channel, not a variant of it:

| | Report-FD channel | Shader helper channel |
|---|---|---|
| Direction | One-way: child → daemon | Two-way: daemon/caller → helper (request), helper → caller (response) |
| Transport | A dedicated pipe FD, `--report-fd <n>` | The helper process's own stdin (request) and stdout (response) — no extra FD |
| Cardinality | Zero or more frames, then the child closes its write end | Exactly one request frame in, exactly one response frame out (SR-3a decision (c); see "One serial request per process" below) |
| Owner of the wire-shape caps | `StreamCaps::REPORT` (`FrameReader::new`, unchanged since SR-1a) | `StreamCaps::SHADER_REQUEST` / `StreamCaps::SHADER_RESPONSE` (`FrameReader::with_caps`, new in SR-3a) |
| Kind namespace | 1-2 (frozen), 3-15 reserved | 16-18 (this document) |

Only the wire FORMAT (the 12-byte `KWR1` header, the codec crate, the
"additive `Unknown`-kind" evolution rule) and the general containment
philosophy (bounded, watchdog-guarded, killable) are shared. Nothing about
report-FD's duplicate/late/missing POLICY (SR-1c) applies to this protocol;
the shader helper's own policy is decision (c) below and whatever SR-3b's
renderer-side spawn/containment/reaping wiring adds on top of it.

## Wire format

Identical to `docs/REPORT_PROTOCOL_V1.md`'s: a frame is a 12-byte header
(`b"KWR1"` magic, 1-byte `kind`, 1-byte `flags` which MUST be `0`, 2
reserved bytes which MUST be `0`, a little-endian `u32` `payload_len` capped
at 65,536) followed by `payload_len` raw bytes. `flags`/reserved-nonzero,
bad magic, a truncated header/payload, and an oversize `payload_len` are all
the SAME typed `FrameError` variants `docs/REPORT_PROTOCOL_V1.md` documents
— this protocol does not redefine wire-level error handling, only which
kinds and caps apply to its own two channels.

## Kinds

| `kind` | Name | Payload | Direction |
|---|---|---|---|
| 16 | `shader-compile-request-v1` | JSON, `validate_shader_compile_request`-checked | Caller → helper (helper's stdin) |
| 17 | `shader-compile-response-v1` | JSON, `validate_shader_compile_response`-checked | Helper → caller (helper's stdout) |
| 18 | `spirv-chunk-v1` | Raw binary (NOT JSON) — one chunk of a compiled SPIR-V module | Helper → caller (helper's stdout), repeatable |

Kinds 1-15 remain the report-FD namespace (`docs/REPORT_PROTOCOL_V1.md`);
this protocol never emits or expects them. An `Unknown`-kind frame on either
channel is read (so the stream stays positioned correctly for whatever
follows) but not interpreted — the same additive-evolution rule
`docs/REPORT_PROTOCOL_V1.md` documents for its own kind 2/3+.

### Kind 18 (`spirv-chunk-v1`) — RESERVED, no producer yet

SR-3a's helper never emits this kind (it never compiles anything — see
"SR-3a skeleton scope" below). Recorded here now so the wire shape is
frozen before a producer exists, per this workspace's "reserve, don't
retrofit" convention (mirrors how `docs/REPORT_PROTOCOL_V1.md` froze kind 2
ahead of its own producer):

- Payload is the raw SPIR-V binary bytes for one 64 KiB-or-smaller chunk of
  a compiled module — NOT wrapped in JSON, unlike every other kind this
  workspace defines. A module larger than one frame's 64 KiB cap is split
  across multiple kind-18 frames, emitted in order, immediately after the
  kind-17 response frame that announces how many chunks to expect (a
  reserved response field — see below).
- Ordering/reassembly rule (reserved, to be fixed by whichever slice adds
  the producer): kind-18 frames on one response stream concatenate, in
  arrival order, into the complete SPIR-V module. No chunk index field is
  planned — arrival order on a single serial exchange is the reassembly
  key, matching the "one serial request per process" model (decision (c)).

## Stream caps

Both channels use `kwe-report-protocol`'s `FrameReader::with_caps` (never
`FrameReader::new`, which is the report-FD channel's own `StreamCaps::REPORT`
defaults) — SR-3a decision (b):

| Channel | `StreamCaps` constant | Max frames | Max total payload bytes | Rationale |
|---|---|---|---|---|
| Request (helper's stdin) | `StreamCaps::SHADER_REQUEST` | 4 | 1 MiB (1,048,576) | One kind-16 frame is the ENTIRE request (decision (c): a request never spans multiple frames) plus slack for a misbehaving caller to be caught by a typed cap instead of an unbounded read. |
| Response (helper's stdout) | `StreamCaps::SHADER_RESPONSE` | 132 | 8 MiB (8,388,608) | One kind-17 response frame plus up to 128 kind-18 SPIR-V chunks (a future compiling helper's shape, reserved now — see kind 18 above) plus slack. |

The per-frame cap (64 KiB, `MAX_PAYLOAD_BYTES`) is universal and unaffected
by either `StreamCaps` value — see `docs/REPORT_PROTOCOL_V1.md`'s own
Stream caps section for the shared boundary-test table
(`with_caps_frame_count_is_enforced_at_the_configured_boundary`,
`with_caps_total_bytes_is_enforced_at_the_configured_boundary`,
`with_caps_never_relaxes_the_universal_per_frame_cap`, all in
`kwe-report-protocol`).

## One serial request per process (decision (c))

SR-3a's skeleton — and, until a later slice's own measurement-backed
decision, every shader helper process — handles exactly ONE request per
process invocation:

1. Read exactly one frame from stdin.
2. If it is not kind 16, or is malformed/oversize/exceeds
   `StreamCaps::SHADER_REQUEST`, or fails `validate_shader_compile_request`:
   respond with a `status: "protocol-error"` kind-17 frame (when a response
   is still possible — see "Exit codes" below) and exit.
3. If it IS a valid kind-16 request: check for ANY further bytes on stdin.
   Their presence — whether they form a second valid request frame or not —
   is treated as a protocol violation (`"excess-request"`), not silently
   ignored. This is the STRICTER of the two options SR-3a's task
   explicitly weighed (silently reading-one-then-ignoring-the-rest vs.
   refusing) — chosen because a caller that sends more than one request per
   process invocation is itself violating the one-request contract this
   protocol defines, and staying silent about that would hide a caller-side
   bug (e.g. a future daemon wiring that forgets to spawn a fresh process
   per request) behind an apparently-successful exchange.
4. Otherwise, respond with the one valid answer and exit.

**Open question (recorded here, not decided):** whether a LATER helper
keeps a process alive across multiple SERIAL requests (a long-lived
serial-loop mode: read request, respond, read the next request, ...,
instead of one process per request) is explicitly deferred. SR-3a's task
names this directly: "Long-lived serial-loop operation is a later decision
when 3c measures spawn cost." Nothing in this document's wire format
forecloses that option — a serial-loop helper would simply repeat the
"read one kind-16 frame, write one kind-17 (+ kind-18s) response" exchange
without exiting between them, which is why the excess-bytes check above is
scoped to "this process invocation," not to the protocol itself. Until that
decision lands, every SR-3-derived helper implementation should assume
one-request-per-process (this document's own `kwe-shader-compiler`
skeleton does).

## `shader-compile-request-v1` schema (kind 16)

```json
{
  "schema": "shader-compile-request-v1",
  "stage": "vertex | fragment",
  "source": "...GLSL source, <= --max-source-bytes (default 262144, today's 256 KiB shader cap)",
  "includes": { "name.glsl": "...file contents, <= 64 KiB each, <= 32 entries" },
  "combos": { "COMBO_NAME": "...shape not yet interpreted by this crate, <= 128 entries" },
  "defines": { "DEFINE_NAME": "...shape not yet interpreted by this crate, <= 128 entries" },
  "options": {
    "target_env": "vulkan",
    "target_env_version": "1.2",
    "optimization_level": "zero"
  }
}
```

`validate_shader_compile_request(payload, max_source_bytes)` (`kwe-report-
protocol`) checks, top to bottom: `schema` equals
`SHADER_COMPILE_REQUEST_SCHEMA`; `stage` is a string equal to `"vertex"` or
`"fragment"` (any other value, including a differently-cased or unknown
stage name, is `InvalidStage`); `source` is a string no longer than the
CALLER-supplied `max_source_bytes` bound (a runtime parameter — the
helper's own `--max-source-bytes` flag — not a fixed protocol constant,
unlike the bounds below); `includes` is an object with at most
`MAX_SHADER_INCLUDES` (32) entries, each value a string of at most
`MAX_SHADER_INCLUDE_BYTES` (64 KiB, 65,536); `combos` and `defines` are each
an object with at most `MAX_SHADER_COMBOS`/`MAX_SHADER_DEFINES` (128)
entries — their VALUES are not otherwise interpreted by this crate (a
later slice, once it defines what a combo/define actually configures,
decides the value shape and adds that check here rather than to a new
function, keeping this validator the single source of truth for the
request schema).

All 6 SR-3a fields are REQUIRED (unlike `scene-inspection-v1`, which
inherited its shape from an existing v0 record with its own history, this
schema is new in SR-3a — every field is spelled out explicitly rather than
defaulting `includes`/`combos`/`defines` to "absent means empty," so a
caller's request is always a complete, self-describing record).

**`options` (SR-3c, additive — still schema `shader-compile-request-v1`):**
OPTIONAL, and (SR-3c2) each of its three sub-fields (`target_env`,
`target_env_version`, `optimization_level`) is INDEPENDENTLY optional too
— absent, whether the whole object or one field within a present object,
means "use the compiling side's own default for that field" — today,
`kwe-core::shader_compile_spec`'s constants. Each present field is a
string of at most `MAX_SHADER_OPTION_STRING_BYTES` (128). This crate
(`kwe-report-protocol`) only checks SHAPE here (string, bounded length)
— it has no opinion on which target env/optimization level a caller may
ask for.

**As of SR-3c2, the helper ACTUALLY COMPILES WITH these values** —
`kwe-shader-compiler::resolve_wire_options` maps each present sub-field
onto a real `shaderc::TargetEnv`/`EnvVersion`/`OptimizationLevel`, falling
back per-field to `kwe-core::shader_compile_spec`'s constant for whatever
is absent. The VALUE vocabulary this crate accepts (checked by the
helper, not by `kwe-report-protocol`'s structural validator): `target_env`
must be `"vulkan"` (the only target this codebase ever compiles for);
`target_env_version` must be one of `"1.0"`/`"1.1"`/`"1.2"`/`"1.3"`/
`"1.4"` (`shaderc::EnvVersion`'s own `Vulkan1_0..=Vulkan1_4` variants);
`optimization_level` must be one of `"zero"`/`"size"`/`"performance"`
(`shaderc::OptimizationLevel`'s three variants). A PRESENT value outside
this vocabulary is `{"status": "protocol-error", "reason": "bad-options"}`
— see the reason-code table below — checked BEFORE any `shaderc` call is
attempted, and never silently defaulted: the caller asked for something
specific, and answering with something else would be worse than an
explicit refusal.

`kwe-scene-renderer` always populates this field from `kwe-core::
shader_compile_spec` (never omits it), which is exactly the helper's own
default vocabulary — every request this renderer builds resolves
successfully; `"bad-options"` can only fire for a caller sending a value
outside this fixed vocabulary, which this renderer never does.

## `shader-compile-response-v1` schema (kind 17)

Four shapes, all still schema `shader-compile-response-v1` (status-
dependent fields, per `validate_shader_compile_response`'s own status
branch — see below):

```json
{"schema": "shader-compile-response-v1", "status": "unimplemented", "reason": "skeleton"}
{"schema": "shader-compile-response-v1", "status": "protocol-error", "reason": "<code>"}
{"schema": "shader-compile-response-v1", "status": "ok", "spirv_chunks": 1, "spirv_total_bytes": 512}
{"schema": "shader-compile-response-v1", "status": "compile-error", "log": "...bounded to 4 KiB, see below"}
```

`validate_shader_compile_response(payload)` checks `schema` equals
`SHADER_COMPILE_RESPONSE_SCHEMA`, that `status` is a present string, and
then STATUS-DEPENDENT required fields — the `status` enum itself is still
not CLOSED (the same "additive, don't retroactively break an older
reader" principle `FrameKind::Unknown` follows, now applied per-status: an
unrecognized status falls back to the original `"unimplemented"`/
`"protocol-error"` shape, `"reason"` required):

- `"unimplemented"` / `"protocol-error"` / any other/future status:
  `"reason"` required (SR-3a's original shape, unchanged).
- `"ok"` (SR-3c, first compiling helper): `"spirv_chunks"` (SR-3a's own
  reservation named this field `spirv_chunk_count`; SR-3c uses the
  shorter `spirv_chunks` instead — a deliberate rename, not an oversight,
  matching `spirv_total_bytes`'s own naming) and `"spirv_total_bytes"`
  required, both non-negative integers, bounded by
  `MAX_SPIRV_CHUNKS` (131, one less than `StreamCaps::SHADER_RESPONSE`'s
  132-frame budget — that budget also covers this kind-17 header frame
  itself) and `MAX_SPIRV_TOTAL_BYTES` (`StreamCaps::SHADER_RESPONSE`'s own
  8 MiB total-payload budget). An over-claim on EITHER field is refused
  right here, at header-validation time — a dishonest/buggy helper
  declaring e.g. 200 chunks is rejected before the caller ever tries to
  read that many kind-18 frames. No `"reason"` field.
- `"compile-error"` (SR-3c): `"log"` required, a string of at most
  `MAX_SHADER_COMPILE_ERROR_LOG_BYTES` (4 KiB) — `shaderc`'s own error
  text is not otherwise length-bounded, so this keeps the eventual
  caller-side diagnostic bounded without a separate truncation step at
  the call site. A compile error is a RESULT, not a protocol failure
  (SR-3c task text) — it gets its own shape rather than being shoehorned
  into `"reason"`. No `"reason"` field.

`kwe-shader-compiler`'s own producer, by build:

- SR-3a's skeleton only ever emitted `unimplemented`/`protocol-error` —
  no `shaderc` dependency existed yet.
- SR-3c (current): every structurally valid request actually compiles.
  `"ok"` on success — the kind-17 header is followed by exactly
  `spirv_chunks` kind-18 `spirv-chunk-v1` frames (raw binary, each at
  most `MAX_PAYLOAD_BYTES`/64 KiB, in order — SPIR-V is a stream of
  native-endian 32-bit words, and both the helper and its caller always
  run on the same host/architecture, since the helper is a child of its
  caller, so no endian conversion happens anywhere on this path),
  concatenating back to exactly `spirv_total_bytes` bytes. `"compile-
  error"` on a `shaderc` failure (bad GLSL, or `shaderc`/`libshaderc`
  itself unavailable in the helper process) — the helper's `compile_
  source` function, not this validator, decides `"ok"` vs `"compile-
  error"`; see `kwe-shader-compiler/src/main.rs`'s own doc comment.
  `"unimplemented"` remains a real POSSIBLE outcome (not removed) since
  an older helper binary — `--shader-helper` can point at any binary on
  disk — would still answer that way.

### Reserved response fields (SR-3d/SR-3e, not implemented here)

Named now so a later slice's addition is additive against a frozen shape,
not a redesign — neither of these exist in any response `kwe-shader-
compiler` emits today:

| Field | For | Reserved meaning |
|---|---|---|
| `reflection` | SR-3d (reflection/validation spike) | An object describing the compiled module's resource bindings/push-constant layout/etc. — the shape SR-3d's own spike defines; `null` or absent until then. |
| `cache_key` | SR-3e (bounded cache) | A stable digest identifying this EXACT request (source + stage + includes + combos + defines, canonicalized) — lets a caller key its own cache without re-deriving the digest rule itself. Reserved; SR-3e's own contract fixes the exact canonicalization (expected to mirror `scene-inspection-v1`'s digest rule: serialize with a placeholder, SHA-256, hex). |

## Protocol error reason codes

Emitted in a `status: "protocol-error"` response's `"reason"` field
(`kwe-shader-compiler`'s own `reason_for_frame_error`/
`reason_for_request_error` functions build these):

| Reason | Cause |
|---|---|
| `bad-magic` / `bad-flags` / `bad-reserved` / `payload-oversize` / `truncated-header` / `truncated-payload` / `frame-count-exceeded` / `total-bytes-exceeded` / `io-error` | A wire-level `FrameError` reading the request frame — see `docs/REPORT_PROTOCOL_V1.md`'s own error table for what each means. |
| `wrong-kind` | The first frame read was not kind 16. |
| `malformed-json` | The kind-16 payload is not valid JSON. |
| `not-an-object` | The payload parses as JSON but is not a JSON object. |
| `wrong-schema` | `"schema"` is present but not `"shader-compile-request-v1"`. |
| `missing-field:<path>` | A required field (or nested field, e.g. `stage`) is absent. |
| `wrong-type:<path>` | A required field is present with the wrong JSON type. |
| `invalid-stage` | `"stage"` is neither `"vertex"` nor `"fragment"`. |
| `source-oversize` | `"source"` exceeds `--max-source-bytes`. |
| `too-many-includes` | `"includes"` has more than 32 entries. |
| `invalid-include:<name>` | An `"includes"` entry is not a string, or exceeds 64 KiB (name truncated to 128 bytes on a UTF-8 boundary in the diagnostic — never unbounded, never a panic on a multi-byte character). |
| `too-many-combos` / `too-many-defines` | `"combos"`/`"defines"` has more than 128 entries. |
| `option-oversize:<path>` | (SR-3c) An `"options"` sub-field (`options.target_env`/`options.target_env_version`/`options.optimization_level`) exceeds `MAX_SHADER_OPTION_STRING_BYTES` (128 bytes). |
| `bad-options` | (SR-3c2) A PRESENT `"options"` sub-field's VALUE is outside the vocabulary `kwe-shader-compiler` supports (see the `options` field description above) — a structurally valid, in-bounds string that this crate still refuses to compile with. Checked by the helper itself, not `kwe-report-protocol`'s structural validator. |
| `excess-request` | Bytes remained on stdin after the one request this process reads (decision (c)). |

A `"compile-error"` response (SR-3c) is a SEPARATE status, not a
`protocol-error` reason code — see the response schema section above; it
carries its own `"log"` field, never a `"reason"`.

## Exit codes

| Code | Meaning | Response frame? |
|---|---|---|
| 0 | One valid request answered. | Yes — `status: "ok"` or `"compile-error"` (SR-3c, the real compile outcomes) or `"unimplemented"` (an older helper binary still answering the SR-3a way). |
| 2 | Malformed command-line invocation (`--max-wall-ms`/`--max-source-bytes` missing a value, a non-numeric value, or an unrecognized flag). Defensive/test-only in practice — the daemon controls argv. | No. |
| 64 | Self-watchdog deadline expired (see below). | **No — silent.** |
| 65 | A protocol violation (see the reason-code table above). | Yes, when possible — `status: "protocol-error"`. |
| 66 | Clean EOF with zero bytes ever read: nothing was sent, so there is nothing to respond to. | No. |

## Watchdog

`--max-wall-ms <n>` (default 10,000, mirrors `kwe-scene-inspector`'s own
flag/default) bounds the whole exchange with a wall-clock deadline computed
once at process start. The deadline is checked BEFORE every read attempt
(`DeadlineReader` in `kwe-shader-compiler`) — this is a SOFT backstop, not a
hard guarantee:

- It reliably fires when the deadline has already passed before a read is
  attempted (including, deterministically, a `--max-wall-ms 0` invocation —
  the deadline equals the moment it was computed, so the very first read's
  check always observes real time having moved past it) or when data
  arrives slowly enough that successive read calls each get a chance to
  check the clock in between.
- It CANNOT preempt a single `read()` syscall already blocked inside the
  OS — an empty pipe with no writer, or a writer that stalls mid-frame with
  no further bytes, keeps the helper blocked past its own deadline
  regardless. Closing that gap needs a genuinely preemptive mechanism (a
  second thread, a signal, a `poll`/`select` with a timeout) this skeleton
  deliberately does not add.
- **The caller-side kill (SR-3b: `kwe-scene-renderer`'s own
  `shader_helper.rs`, spawn-per-request, no `setpgid` — see below) is the
  AUTHORITATIVE bound.** This watchdog is exactly the same class of soft
  backstop `kwe-scene-inspector`'s own `--max-wall-ms` already is for the
  daemon's scene-inspection path — real protection against a truly hung
  child still requires the parent process to hold the kill switch.

Expiry exits 64 SILENTLY — no response frame is attempted, on the reasoning
that a process which has already blown its own time budget should not
spend more of it constructing a response nobody may still be waiting to
read (and the caller learns the outcome from the exit code / its own
timeout regardless — SR-3b's client classifies this as `HelperOutcome::
Timeout` either way, whether the watchdog fires first or the caller's own
deadline does).

## Implementation status by slice

This document describes the FULL protocol surface `kwe-shader-compiler`
implements as of SR-3c:

- **SR-3a** built the skeleton: every structurally valid request got
  `{"status": "unimplemented", "reason": "skeleton"}`, no `shaderc`
  dependency existed in the crate, nothing was ever compiled.
- **SR-3b** wired the first real caller: `kwe-scene-renderer`'s own
  `shader_helper.rs` spawns this binary per material-shader compile,
  contained the way a renderer worker can contain its own child (no
  `setpgid` — the helper stays in the renderer's process group so the
  daemon's existing group-kill already covers it; see `docs/SR3.md`'s
  SR-3b section for the full containment writeup) — but FELL BACK to the
  existing in-thread `shaderc` compile on every outcome (`unimplemented`
  included), so trunk still rendered byte-identically. The daemon itself
  never calls this binary directly; it only resolves `kwe-scene-
  renderer`'s sibling path and passes it down via `--shader-helper` for
  `RendererKind::Scene` workers, the same way it hands the renderer its
  other binary paths.
- **SR-3c (current)** makes the compile real: the helper links the SAME
  `shaderc` crate/version `kwe-scene-renderer` does (workspace-pinned),
  invoked with options shared via `kwe-core::shader_compile_spec`'s plain
  constants (see `docs/SR3.md`'s SR-3c section) so the two compile paths
  are provably byte-identical, not just assumed to be. A GLSL failure is
  `"compile-error"` (a RESULT, not a helper failure — exit 0 either way);
  a `shaderc`/`libshaderc` setup failure in the helper process ALSO
  surfaces as `"compile-error"` (documented above), not a new status. The
  renderer's `ShaderHelper::compile_stage_or_fallback` consumes `"ok"`
  directly (skip the in-thread compile) and surfaces `"compile-error"`
  through the SAME `Err` path an in-thread compile failure already takes
  (no retry in-thread — the same GLSL fails identically a second time);
  every OTHER outcome (`Unimplemented`/`ProtocolError`/`Unavailable`/
  `Timeout`) still falls back in-thread, unchanged from SR-3b. The
  long-lived serial-loop question (decision (c), SR-3a) remains
  explicitly UNRESOLVED — SR-3c did not measure spawn cost against real
  compile latency in production; see `docs/SR3.md`'s SR-3c open risks.

Tests in THIS crate still drive the compiled binary directly via
`CARGO_BIN_EXE_kwe-shader-compiler`, the same pattern `kwe-daemon`'s own
tests use to drive a real `kwe-scene-inspector` subprocess;
`kwe-scene-renderer`'s own tests additionally exercise the real binary
cross-crate via a target-dir path convention (skip-with-note if not
already built), including a differential oracle comparing the helper's
SPIR-V against the in-thread path's for several representative shaders
(plain quad, combo/define, `#include`-spliced, vertex+fragment).
