# SR-3 — killable shader service (decomposition)

Parent epic: `docs/Scene-Rendering-Plan.md` §4.3 ("Shader compiler helper")
and §8 SR-3. Filed after SR-2's scene.json/typed-IR slices merged (trunk
`f5095ff`) and SR-1's own follow-up gate/persistence slices (SR-1c2, SR-1c3)
closed out — SR-3 begins the NEXT plan area: moving wallpaper-provided
shader preprocessing/compilation out of the renderer thread into a
separate, killable `kwe-shader-compiler` process (plan §4.3's exact
wording), mirroring the containment shape SR-0b already proved for scene
inventory (`kwe-scene-inspector`).

Child order: SR-3a → SR-3b → SR-3c → SR-3d → SR-3e. Each child is one
mergeable slice with its own implementation, following the same
"containment and bounds first, no premature integration" discipline SR-0
established for the inspector.

- **SR-3a — protocol + binary skeleton** (this document's filled contract
  below). The wire format (`docs/SHADER_HELPER_PROTOCOL_V1.md`) and a
  standalone `kwe-shader-compiler` binary that speaks it — bounded,
  watchdog-guarded, killable — but does no compilation and has no daemon
  caller yet. SR-0b's own precedent: get the containment shape right and
  independently testable before anything depends on it.
- **SR-3b — worker-side spawn/containment/reaping.** The daemon (or
  renderer worker — TBD by that slice) actually spawns
  `kwe-shader-compiler`, wires its stdin/stdout to a real request/response
  exchange, and owns the AUTHORITATIVE kill (the process-level bound
  SR-3a's own watchdog explicitly defers to — see
  `docs/SHADER_HELPER_PROTOCOL_V1.md`'s Watchdog section). Mirrors
  `supervisor::spawn_worker`'s / `inspect.rs`'s existing containment
  patterns (`setpgid`, `PR_SET_PDEATHSIG`, resource limits, pipe2
  `O_CLOEXEC`) rather than inventing new ones.
- **SR-3c — first preprocessing family through the helper.** Picks ONE
  existing shader-preprocessing path in `kwe-scene-renderer`
  (`materialshader.rs`/`shaderpre.rs` — S2/S4b's in-thread `shaderc` calls)
  and routes it through the helper end to end: a real `shaderc` compile,
  real SPIR-V chunks (kind 18, reserved by SR-3a), real error propagation.
  This is also where the "one process per request vs. a long-lived
  serial-loop worker" open question (SR-3a decision (c), recorded below)
  gets resolved — against REAL measured spawn cost vs. real compilation
  latency, not a guess.
- **SR-3d — reflection/validation spike.** SPIR-V reflection (descriptors,
  vertex inputs, uniforms, required feature flags — plan §4.3: "never infer
  a shader interface solely from textual declaration order") returned
  alongside the compiled module. The `reflection` response field SR-3a
  reserved (`docs/SHADER_HELPER_PROTOCOL_V1.md`) is this slice's to define.
- **SR-3e — bounded cache.** Plan §4.3's cache keyed by source/include
  hashes, combos/constants, compiler/preprocessor ABI, renderer build,
  target Vulkan version, GPU/driver feature class, and asset-root version;
  atomic/size-bounded/private/LRU-pruned writes; a crash/timeout/validation
  failure never cached as a permanent result. The `cache_key` response
  field SR-3a reserved is this slice's to define.

## SR-3a — the helper protocol + binary skeleton

Conductor decisions (verbatim):

- **(a)** The helper reuses `kwe-report-protocol`'s 12-byte `KWR1` frame
  wire format on BOTH directions (stdin = requests, stdout = responses)
  with a new kind namespace: 16 = shader-compile-request (JSON), 17 =
  shader-compile-response (JSON), 18 = spirv-chunk (raw binary,
  repeatable). Documented in a new `docs/SHADER_HELPER_PROTOCOL_V1.md`
  (cross-linking `docs/REPORT_PROTOCOL_V1.md`; kinds <16 remain
  report-namespace, the codec is shared, the CHANNELS are unrelated).
- **(b)** `FrameReader`'s stream caps become configurable:
  `FrameReader::with_caps(reader, StreamCaps { max_frames,
  max_total_payload_bytes })`, the existing `new()` keeping today's report
  defaults byte-identically (16 frames / 1 MiB). Helper channel caps:
  requests 4 frames/1 MiB; responses 132 frames/8 MiB (one 17-frame + up
  to 128 spirv chunks + slack). Per-frame 64 KiB cap stays universal.
- **(c)** One serial request per helper PROCESS in this skeleton (plan
  §4.3 "one serial request at a time"): read exactly one kind-16 request,
  answer, exit 0. Long-lived serial-loop operation is a later decision
  when SR-3c measures spawn cost — recorded as an explicit OPEN QUESTION
  (`docs/SHADER_HELPER_PROTOCOL_V1.md`'s "One serial request per process"
  section), not decided here.

```text
Task:            The shader helper's wire protocol (kinds/caps/schemas) and
                 a standalone binary that speaks it: containment and bounds
                 first, SR-0b-style — no shaderc migration yet (SR-3c), no
                 renderer/daemon integration yet (SR-3b).
Milestone/Slice: SR-3a
Goal:            Freeze the wire format and prove a killable, bounded,
                 protocol-correct helper process exists and is
                 independently testable BEFORE anything depends on it —
                 the same "protocol/skeleton before integration" discipline
                 SR-1a (report protocol) and SR-0b (inspector) both used.
Outcome:         crates/kwe-report-protocol/src/lib.rs: FrameKind gains
                 ShaderCompileRequestV1 (16)/ShaderCompileResponseV1 (17)/
                 SpirvChunkV1 (18); a new StreamCaps{max_frames,
                 max_total_payload_bytes} struct with three named consts
                 (REPORT -- byte-identical to the old fixed behavior,
                 SHADER_REQUEST, SHADER_RESPONSE); FrameReader gains
                 with_caps(reader, StreamCaps) alongside the now-
                 with_caps(reader, StreamCaps::REPORT)-delegating new()
                 (proven byte-identical by
                 new_and_with_caps_report_are_byte_identical);
                 FrameError::FrameCountExceeded/TotalBytesExceeded widen
                 from unit variants to {max: usize} struct variants so
                 their message reflects whichever StreamCaps was actually
                 configured (the crate's only 2 existing test call sites
                 updated to match, no other repo code matched on these
                 variants by name). New validate_shader_compile_request
                 (payload, max_source_bytes) and
                 validate_shader_compile_response(payload) functions,
                 same "distinguish MissingField from WrongType" style as
                 the existing validate_inspection, plus 4 new bound
                 constants (MAX_SHADER_INCLUDES=32,
                 MAX_SHADER_INCLUDE_BYTES=64 KiB, MAX_SHADER_COMBOS=128,
                 MAX_SHADER_DEFINES=128) and 2 schema-tag constants.
                 New crate crates/kwe-shader-compiler (workspace member,
                 dependency-light: kwe-report-protocol + serde_json only,
                 per the task): reads exactly one kind-16 frame from stdin
                 (StreamCaps::SHADER_REQUEST, a DeadlineReader wrapper
                 checking a --max-wall-ms wall-clock deadline before every
                 read), validates it, and -- in this skeleton, always --
                 responds kind-17 {"status":"unimplemented",
                 "reason":"skeleton"} and exits 0. --max-source-bytes
                 (default 262144, today's real shader-source cap) and
                 --max-wall-ms (default 10000, mirrors the inspector's own
                 flag/default) are hand-rolled flags (no clap, per the
                 task's dependency-light instruction). A wrong-kind first
                 frame, any FrameError, a validate_shader_compile_request
                 failure, or trailing bytes after the one request
                 (decision (c): treated as excess/a protocol violation,
                 the STRICTER of the two options the task named, chosen so
                 a caller-side bug sending >1 request per process
                 invocation is never silently masked as success) all
                 respond kind-17 {"status":"protocol-error",
                 "reason":"<code>"} (best-effort -- a write failure on the
                 way out is swallowed) and exit 65. A deadline expiry
                 exits 64 SILENTLY (no response attempted; the daemon-side
                 kill is documented as the authoritative bound, not built
                 until SR-3b). Empty stdin (clean EOF, nothing ever sent)
                 exits 66 with no response frame. packaging/PKGBUILD
                 installs the binary next to kwe-scene-inspector, no
                 pkgrel bump (still 20).
                 docs/SHADER_HELPER_PROTOCOL_V1.md (new): the full wire
                 contract -- kinds/caps/schemas, the reserved kind-18
                 (spirv chunk) and reserved response fields
                 (spirv_chunk_count for SR-3c, reflection for SR-3d,
                 cache_key for SR-3e, all explicitly marked unimplemented),
                 the protocol-error reason-code table, the exit-code
                 table, and the watchdog's documented soft-backstop
                 limitation. docs/REPORT_PROTOCOL_V1.md: a cross-link
                 paragraph, the Kinds table extended with a 3-15-reserved
                 row plus the 16/17/18 rows pointing at the new doc, and
                 the Stream caps section updated to describe with_caps/
                 StreamCaps alongside the still-byte-identical new()
                 default.
In scope:        crates/kwe-report-protocol/src/lib.rs (kinds, StreamCaps,
                 with_caps, the two widened FrameError variants, the two
                 new validators + their bound constants, 17 new tests),
                 crates/kwe-shader-compiler (new crate: Cargo.toml,
                 src/main.rs, tests/protocol.rs -- 9 unit + 9 integration
                 tests), Cargo.toml (workspace member), packaging/PKGBUILD
                 (install line, no pkgrel bump), docs/
                 SHADER_HELPER_PROTOCOL_V1.md (new), docs/
                 REPORT_PROTOCOL_V1.md (cross-link + kind/caps table
                 updates), docs/SR3.md (this document).
Out of scope:    Any shaderc dependency or real compilation (SR-3c's own
                 scope -- this crate has zero shaderc/kwe-core/kwe-vulkan
                 dependency, by the task's explicit instruction). Any
                 renderer/daemon integration: no daemon code spawns this
                 binary yet, no kwe-scene-renderer code calls it, nothing
                 in supervisor.rs/apply.rs changed (SR-3b's scope). The
                 spirv-chunk-v1 (kind 18) producer, the reflection block,
                 and the cache-key field -- all reserved/documented, none
                 implemented (SR-3d/SR-3e's own scope respectively). The
                 long-lived serial-loop question (decision (c) -- an
                 explicit open question, not answered here). A genuinely
                 preemptive watchdog (thread/signal/poll-based) -- the
                 soft "checked between reads" backstop the task itself
                 specified, matching the inspector's own precedent and the
                 explicit "daemon-side kill is authoritative later;
                 document" instruction.
Acceptance tests:        kwe-report-protocol: 37 tests (up from 20) --
                         shader_kinds_round_trip_and_map_to_their_wire_bytes;
                         3 with_caps boundary tests (frame count at
                         limit-1/limit/limit+1 for a CUSTOM StreamCaps,
                         total bytes at a custom boundary that does NOT
                         coincide with the frame-count boundary the way
                         the report channel's own 16x64KiB==1MiB does,
                         and the universal per-frame cap staying
                         unrelaxed under a deliberately generous
                         StreamCaps); new_and_with_caps_report_are_
                         byte_identical (proves decision (b)'s "existing
                         new() keeping today's report defaults
                         byte-identically" claim, not just asserting it);
                         11 validate_shader_compile_request/response tests
                         (golden records, every required field missing,
                         wrong types, wrong schema, stage enum, source
                         length at the caller-supplied boundary, includes
                         at the 32-entry/64-KiB-per-entry boundaries plus
                         a non-string include value, combos/defines past
                         128, not-an-object/invalid-JSON).
                         kwe-shader-compiler: 18 tests (9 unit: flag
                         parsing defaults/overrides/missing-value/
                         non-numeric/unrecognized, DeadlineReader before/
                         after its deadline, the bounded() UTF-8-safe
                         truncation helper at a multibyte char boundary;
                         9 integration, driven against the REAL compiled
                         binary via CARGO_BIN_EXE_kwe-shader-compiler --
                         valid request -> unimplemented + exit 0;
                         wrong-kind first frame -> protocol-error + exit
                         65; a second request frame -> excess-request,
                         proving the process does NOT already commit to
                         the success response before noticing; oversize
                         source; malformed JSON; 33 includes; garbage
                         stdin (bad-magic, not a panic); empty stdin ->
                         exit 66, no response frame; the watchdog on an
                         ALREADY-expired deadline (--max-wall-ms 0, a
                         deterministic proof -- Instant::now() strictly
                         advances past a deadline computed at 0ms offset
                         by the time the first read's check runs, no
                         sleep-and-poll race needed) -> exit 64, no
                         response frame).
                         913 workspace tests total, up from 878.
                         cargo fmt --all -- clean.
                         cargo clippy --workspace --all-targets -- -D
                         warnings -- clean.
                         cargo test --workspace -- 913 passed, 0 failed.
                         ./scripts/check.sh -- exit 0, green end-to-end,
                         including the C++/QML build/qml-typecheck
                         (untouched by this slice) and the new crate's
                         own cargo test lane.
Failure/recovery tests:  Every ShaderRequestError/FrameError variant has a
                         reason-code mapping exercised by at least one
                         integration test (oversize source, malformed
                         JSON, too-many-includes, wrong-kind, garbage
                         stdin/bad-magic, excess-request); empty stdin and
                         watchdog expiry are their own distinct exit
                         codes/tests, not folded into the generic
                         protocol-error path.
Upstream/provenance:    Original; the wire format is an additive extension
                         of this repo's own kwe-report-protocol (SR-1a),
                         the containment shape (self-watchdog,
                         bounded reads, exit-code taxonomy) mirrors
                         kwe-scene-inspector's own SR-0b precedent
                         directly -- no third-party source consulted.
Commands run and results: cargo fmt --all -- clean.
                         cargo clippy --workspace --all-targets -- -D
                         warnings -- clean.
                         cargo test --workspace -- 913 passed, 0 failed.
                         ./scripts/check.sh -- exit 0, green end-to-end.
Open risks:              The long-lived serial-loop question (decision
                         (c)) is explicitly unresolved -- SR-3c inherits
                         it, backed by real spawn-cost/compile-latency
                         measurement once a real shaderc call exists to
                         measure against.
                         The watchdog's documented soft-backstop
                         limitation (cannot preempt an in-flight blocking
                         read) is real, not just a caveat -- SR-3b's own
                         daemon-side kill is what actually closes this
                         gap; until that slice lands, a hung helper
                         process (e.g. a caller that opens the pipe but
                         never writes AND never closes it) can only be
                         reaped by whatever process supervision already
                         exists outside this crate (there is none yet,
                         since nothing spawns this binary in production
                         code today).
                         combos/defines VALUES are not yet interpreted or
                         shape-checked (only entry COUNT is bounded) --
                         deliberate, since this skeleton does not know
                         what a combo/define configures yet; SR-3c is
                         expected to add that check once it does, to the
                         same validate_shader_compile_request function
                         rather than a new one.
STOP findings:           None. No STOP condition was named for this task,
                         and none was found: the crate stayed dependency-
                         light as instructed (kwe-report-protocol +
                         serde_json only, verified by reading
                         Cargo.toml's own dependency list back), no
                         existing FrameError match site outside this
                         crate's own tests broke (grepped the whole repo
                         for FrameError::/FrameCountExceeded/
                         TotalBytesExceeded before widening the variants),
                         and the decision-(c) "which of the two options"
                         choice was made explicitly (protocol-error,
                         documented above) rather than left ambiguous.
Commit(s):               31b2c10
```

## SR-3b — worker-side helper spawn/containment/reaping (zero behavior change)

Filed after SR-2c2's investigation (docs/SR2.md) closed the frame-
divergence build blocker, trunk `9271cbf`.

Conductor decisions (verbatim):

- **(a)** This slice wires the plumbing only. The renderer spawns
  `kwe-shader-compiler`, sends a real kind-16 request for each material-
  shader compile, receives the skeleton's `unimplemented` response, and
  FALLS BACK to today's in-thread compile path — so every scene renders
  byte-identically to trunk. SR-3c flips the first family to actually use
  helper results.
- **(b)** Helper lifecycle in this slice: spawn-per-request (one-shot,
  matching the skeleton's serial contract), lazily on first shader
  compile, never on scenes without material shaders. Spawn cost is
  amortization work for SR-3c's measurement — recorded again as an open
  question below (same one SR-3a already recorded).
- **(c)** Failure policy: ANY helper problem (missing binary, spawn
  failure, protocol error, timeout, crash) → log one bounded stderr line
  and fall back in-thread. The helper can only ever make things slower in
  this slice, never break rendering.

```text
Task:            Wire a real client for SR-3a's shader-compile helper into
                 the renderer (spawn/containment/reaping, a real kind-16
                 request per material compile) with ZERO behavior change:
                 every outcome falls through to the existing in-thread
                 compile path unconditionally.
Milestone/Slice: SR-3b
Goal:            Prove the helper is REACHABLE and SAFE (a real process
                 spawned, contained, and always reaped, under a real
                 renderer worker's constraints) before SR-3c makes
                 rendering actually depend on its answer -- the same
                 "plumbing before behavior" staging SR-1b (report FD
                 wiring) used ahead of SR-1c's policy.
Outcome:         New crates/kwe-scene-renderer/src/shader_helper.rs:
                 ShaderHelper{path: Option<PathBuf>, timeout: Duration,
                 + 4 AtomicBool log-dedup flags} and ShaderCompileRequest
                 {stage, source}. compile(&self, &request) -> HelperOutcome
                 spawns a FRESH helper process per call (decision (b)),
                 writes one kind-16 frame, closes stdin (so the helper
                 observes the clean EOF its own one-request contract
                 expects), drains stdout/stderr nonblocking under a
                 deadline (mirrors kwe-daemon::inspect::supervise's loop
                 shape exactly), and classifies the result. Containment,
                 adapted from kwe-daemon::inspect's one-shot supervision
                 for a RENDERER WORKER (not the daemon) spawning ITS OWN
                 child: no setpgid (plan SS4.3 -- the helper stays in the
                 renderer's own process group so the daemon's existing
                 group-kill already covers it; this is also why this
                 module's timeout-kill uses kill(pid, ...) directly, never
                 kill(-pid, ...), which would signal the renderer itself);
                 PR_SET_PDEATHSIG SIGKILL + a parent-pid check;
                 PR_SET_NO_NEW_PRIVS; env_clear() + HOME only (copied from
                 the renderer's own environment -- no PATH, the helper is
                 invoked by explicit path and execs nothing itself); a
                 STRICTER pre_exec rlimit floor than the renderer's own
                 (address space 512 MiB, file size 16 MiB, 32 open files;
                 RLIMIT_NPROC deliberately left unset/inherited -- the
                 renderer has no way to know the daemon's configured
                 process-count budget, so guessing a number risks being
                 MORE permissive than the daemon's own floor). Timeout
                 kill is SIGTERM, a 200ms grace, then SIGKILL (a fake
                 helper that ignores SIGTERM is still reaped -- tested).
                 HelperOutcome{Unimplemented, ProtocolError(String),
                 Unavailable(String), Timeout, Compiled{spirv, response}
                 (reserved for SR-3c, never constructed this slice)}. One
                 bounded stderr diagnostic per outcome CLASS per process
                 (4 AtomicBool flags on ShaderHelper, not per-compile).
                 Integration point (task's own STOP condition did not
                 trigger -- exactly one choke point exists):
                 main.rs's compile_one_material, the SOLE production
                 caller of materialshader::compile_stage for BOTH stages
                 (verified: every OTHER compile_stage call site in the
                 repo is #[cfg(test)]). shaderpre::preprocess has ALREADY
                 spliced every #include into vertex_pre.source/
                 fragment_pre.source by the time compile_one_material
                 reaches this point, so the wire request's own
                 includes/combos/defines fields are always sent empty --
                 documented explicitly, since a FUTURE caller (there is
                 none yet) that has NOT already spliced would need to
                 populate them instead. shader_helper.compile(...) is
                 called immediately before EACH compile_stage call
                 (vertex, then fragment); its result is discarded (`let _
                 = ...`) with a `// SR-3c consumes Compiled here instead
                 of ignoring it` marker at each site -- compile_stage's
                 own call, arguments, and error handling are completely
                 untouched. ShaderHelper is constructed ONCE in main()
                 and threaded by shared reference through
                 compile_material_layers (new trailing parameter) into
                 compile_one_material (new trailing parameter) -- 2 levels
                 of threading, both call sites of compile_one_material
                 updated identically (the base-material path and the
                 effect-pass path).
                 New CLI flags: --shader-helper <path> (absent -> resolved
                 beside the renderer's own executable via current_exe(),
                 mirroring kwe-daemon::main::default_inspector_path
                 EXACTLY, including NOT exists()-checking the sibling
                 path -- an actually-missing binary is discovered via
                 Command::spawn's own ENOENT, classified Unavailable, the
                 same way a misconfigured explicit --shader-helper is);
                 --shader-helper-timeout-ms (default 10000, bounded
                 100..=30000, kwe_report_protocol-analogous constants
                 exported from shader_helper.rs).
                 Daemon: SupervisorConfig gains shader_helper_path:
                 Option<PathBuf>, resolved by a new
                 default_shader_helper_path() (byte-for-byte the same
                 shape as default_inspector_path -- sibling of the
                 DAEMON's own executable) and overridable by a new daemon
                 CLI flag --shader-helper (mirroring the existing
                 --inspector/--renderer-scene precedent: every daemon-
                 managed binary path is overridable). spawn_worker passes
                 --shader-helper <path> to the child ONLY for
                 RendererKind::Scene (proven by a new test spawning a
                 fake argv-dumping renderer for both Scene and Video
                 kinds with the SAME configured helper path -- Scene gets
                 the flag, Video never does). Confirmed (not assumed):
                 RendererKind::Scene is NOT run inside bwrap by the
                 daemon -- only RendererKind::Web's own chromium child
                 gets bwrap sandboxing (supervisor.rs's --allow-network/
                 bwrap handling is gated on RendererKind::Web
                 specifically) -- so there is no sandbox boundary this
                 helper spawn needs to additionally cross.
                 packaging/PKGBUILD already installs kwe-shader-compiler
                 (SR-3a) -- nothing to add here.
In scope:        crates/kwe-scene-renderer/src/shader_helper.rs (new),
                 crates/kwe-scene-renderer/src/main.rs (mod, 2 new CLI
                 flags, ShaderHelper construction + threading through
                 compile_material_layers/compile_one_material, the two
                 compile() call sites, set_nonblocking widened to
                 pub(crate) for reuse), crates/kwe-scene-renderer/
                 Cargo.toml (kwe-report-protocol path dependency, no new
                 external crate), crates/kwe-daemon/src/supervisor.rs
                 (SupervisorConfig.shader_helper_path, the spawn_worker
                 arg, 1 new test, 3 test-fixture field additions),
                 crates/kwe-daemon/src/main.rs (--shader-helper flag,
                 default_shader_helper_path, wired into the production
                 SupervisorConfig, 3 test-fixture field additions),
                 crates/kwe-daemon/src/playlist_session.rs (1 test-
                 fixture field addition), docs/SR3.md (this section).
Out of scope:    Any real shaderc/spirv consumption of a helper response
                 (SR-3c's own scope -- Compiled is reserved, never
                 constructed). The long-lived serial-loop question
                 (decision (b), recorded again below -- SR-3a already
                 deferred it, still deferred). Reflection/cache (SR-3d/
                 SR-3e). A genuinely preemptive PDEATHSIG test (documented
                 -only, per the task's own allowance -- see
                 shader_helper.rs's own module doc for why: it would
                 require killing the test harness process itself).
Acceptance tests:        kwe-scene-renderer: 8 new shader_helper.rs
                         tests -- valid unimplemented response (fake
                         python helper, KWR1 over stdin/stdout);
                         garbage response -> ProtocolError, not a panic;
                         a hung fake helper -> Timeout, reaped
                         (kill(pid,0) fails afterward) within
                         deadline+grace; a fake that IGNORES SIGTERM
                         still dies to SIGKILL (proves the escalation
                         path actually reaches SIGKILL, not just that a
                         plain hang eventually times out); a missing
                         binary path, and a helper with no path at all
                         (current_exe-resolution-equivalent) -> both
                         Unavailable; the fallback-equivalence proof
                         (decision (a)'s central claim, proven not
                         assumed): materialshader::compile_stage's OWN
                         output is byte-identical whether ShaderHelper is
                         configured-but-missing (--shader-helper
                         /nonexistent) or entirely unconfigured (no
                         flag); the real-binary path, spawning the ACTUAL
                         compiled kwe-shader-compiler (via a target-dir
                         path convention this test introduces, since
                         cross-crate CARGO_BIN_EXE does not exist --
                         skips gracefully with a printed note if the
                         binary was not already built, mirroring this
                         repo's other opt-in/skip-if-prerequisite-missing
                         tests) and confirming it answers Unimplemented --
                         proving this module's wire client and SR-3a's
                         own binary genuinely agree on the protocol, not
                         just against fake scripts.
                         kwe-daemon: 1 new supervisor.rs test --
                         --shader-helper is passed for RendererKind::Scene
                         only, proven against a real (fake) spawned
                         renderer that dumps its own argv, for BOTH Scene
                         and Video kinds with the SAME configured helper
                         path (two isolated runtimes, avoiding any
                         canary/handoff interaction between the two
                         spawns -- not the same runtime spawning twice).
                         922 workspace tests total, up from 913.
                         cargo fmt --all -- clean.
                         cargo clippy --workspace --all-targets -- -D
                         warnings -- clean.
                         cargo test --workspace -- 922 passed, 0 failed.
                         KWE_SCENE_IR_PARITY_DIR=<real corpus> cargo test
                         -p kwe-scene-renderer ir_parity_corpus --
                         --ignored -- "60/60 item(s) parity-passed"
                         (unchanged -- this slice touches no scene-
                         parsing code).
                         ./scripts/check.sh -- exit 0, green end-to-end.
                         scripts/smoke-scene.sh -- every case passes
                         unchanged, INCLUDING every material-shader-
                         dependent oracle (S2/S3/S4/S5a/S5b) -- the
                         strongest available proof that decision (a)'s
                         "zero behavior change" claim holds in practice,
                         not just in unit tests: these oracles assert
                         exact pixel values that would move if the
                         compile_one_material integration point changed
                         anything observable.
Failure/recovery tests:  Every HelperOutcome variant except the reserved
                         Compiled has a dedicated test (garbage/timeout/
                         SIGTERM-ignored/missing-binary/no-path); the
                         fallback-equivalence test IS the "zero behavior
                         change" failure/recovery proof the task asked
                         for, since it exercises the SAME code path a
                         real helper failure would (Unavailable) and
                         confirms it changes nothing downstream.
Upstream/provenance:    Original; the containment shape is a direct,
                         cited adaptation of kwe-daemon::inspect's own
                         SR-0b/SR-1b precedent (not a new design) --
                         explicitly adapted for a renderer-worker-spawns-
                         its-own-child shape rather than copied verbatim,
                         with every departure (no setpgid, no separate
                         HOME dir, stricter/narrower rlimits, HOME-only
                         env) documented at the point it differs.
Commands run and results: cargo fmt --all -- clean.
                         cargo clippy --workspace --all-targets -- -D
                         warnings -- clean.
                         cargo test --workspace -- 922 passed, 0 failed.
                         KWE_SCENE_IR_PARITY_DIR=<real corpus> ir_parity_
                         corpus -- 60/60 parity-passed.
                         ./scripts/check.sh -- exit 0.
                         scripts/smoke-scene.sh -- all cases passed.
Open risks:              The long-lived serial-loop question (decision
                         (b)) is STILL unresolved -- SR-3a recorded it
                         first, this slice's own spawn-per-request client
                         is what SR-3c will actually measure spawn cost
                         against once it has a real compile to time.
                         RLIMIT_NPROC is left unset for the helper
                         (documented above) -- a build with an extremely
                         tight daemon-configured process budget gets
                         whatever ceiling the renderer itself inherited,
                         not a stricter helper-specific one; closing this
                         would need the daemon to pass its own
                         RendererResourceLimits down to the renderer
                         (new plumbing, out of scope here).
                         The PDEATHSIG mechanism itself is proven by
                         precedent (the identical call already works in
                         kwe-daemon::inspect production) rather than by a
                         test in THIS module (documented-only, per the
                         task's own allowance) -- a genuinely orphaned-
                         helper scenario has never been directly observed
                         under test for this specific spawn site.
STOP findings:           None -- the task's own STOP condition (no single
                         compile choke point, or the renderer already
                         holding shaderc state that makes a pre-compile
                         helper call structurally awkward) did not
                         trigger: compile_one_material is the single,
                         easily-verified choke point (grepped every
                         compile_stage call site in the repo; every one
                         outside this function is #[cfg(test)]), and
                         `shaderc::Compiler` is a lazily-initialized,
                         stateless-per-call OnceLock the helper call sits
                         cleanly beside rather than inside.
Commit(s):               (fill in after commit)
```
