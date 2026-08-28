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
Commit(s):               6cd7e76
```

## SR-3c — material shaders compile in the killable helper (byte-identical SPIR-V, in-thread fallback)

Filed after SR-3b merged, trunk `6cd7e76`.

Conductor decisions (verbatim):

- **(a)** The helper links the SAME `shaderc` crate/system `libshaderc`
  the renderer uses, invoked with EXACTLY the options
  `materialshader::compile_stage` passes (same target env/version, same
  optimization level, same everything — factor the option-building into a
  shared `kwe-core` or duplicated-with-lockstep-comment location; prefer
  sharing via `kwe-core` only if that doesn't drag `shaderc` into
  `kwe-core`'s dependency set — otherwise duplicate the option list with a
  lockstep comment both sides, and a test in the RENDERER asserting
  in-thread vs helper SPIR-V byte-equality keeps them honest).
- **(b)** Payoff being bought: a compile that hangs/explodes now dies with
  the killable helper process instead of detaching a compiler thread
  inside the renderer (the SR-3 headline). The in-thread path REMAINS as
  automatic fallback (still has the detach flaw — later SR-3 children
  remove in-thread entirely; noted below).
- **(c)** Rollout: helper path ON by default when the binary resolves
  (SR-3b already passes the flag); ANY failure/mismatch-capable condition
  falls back in-thread per SR-3b's policy, so worst case equals today.

```text
Task:            Route the FIRST shader-preprocessing family
                 (materialshader.rs's in-thread shaderc calls) through
                 SR-3b's helper client end to end: a real shaderc compile
                 in kwe-shader-compiler, real kind-18 SPIR-V chunks, the
                 renderer consuming a real Compiled result. Oracle:
                 byte-identical SPIR-V vs. the in-thread path.
Milestone/Slice: SR-3c
Goal:            Prove the killable-helper payoff (decision (b)) for real:
                 a GLSL compile that would have detached an unjoined
                 thread inside the renderer (materialshader.rs's own
                 documented flaw) now runs inside a process the caller can
                 kill outright, with ZERO observable difference in what
                 gets drawn -- the same "prove, don't assume" byte-
                 equality discipline this session already used for SR-2c's
                 corpus parity and SR-3b's fallback-equivalence claim.
Outcome:         kwe-core gains a new module, shader_compile_spec.rs (3
                 plain string consts: TARGET_ENV="vulkan",
                 TARGET_ENV_VERSION="1.2", OPTIMIZATION_LEVEL="zero", plus
                 ENTRY_POINT="main") -- decision (a)'s "prefer kwe-core
                 only if that doesn't drag shaderc into kwe-core's
                 dependency set" resolved by NOT putting any shaderc TYPE
                 in kwe-core (it is depended on by kwe-daemon/kwe-cli/
                 kwe-audio-worker/kwe-web-renderer/kwe-scene-inspector --
                 nearly every workspace binary): each shaderc-linking
                 crate (kwe-scene-renderer, kwe-shader-compiler) maps
                 these plain values onto its OWN shaderc::CompileOptions
                 locally. The byte-equality oracle test (below) is what
                 actually keeps the two mappings honest, not the shared
                 constants alone (the task's own fallback for exactly this
                 case).
                 kwe-shader-compiler gains a shaderc.workspace=true
                 dependency (SAME version/source the renderer already
                 pins in the root Cargo.toml -- no new external dependency)
                 and kwe-core (for the spec constants). compile_source
                 (new, src/main.rs): builds shaderc::CompileOptions from
                 kwe-core's constants, calls compile_into_spirv SYNCHRONOUSLY
                 on the helper's own main thread -- unlike
                 materialshader::compile_stage's own with_timeout
                 thread-spawn wrapper, no internal timeout machinery is
                 needed here (decision (b)'s payoff: the CALLER's
                 process-level kill, SR-3b's shader_helper.rs, is now the
                 bound, not a detached thread inside this process). A
                 shaderc CompileOptions/Compiler construction failure is
                 folded into the SAME "compile-error" result as a GLSL
                 failure (documented, not a new wire status). On success:
                 kind-17 {"schema":...,"status":"ok","spirv_chunks":<n>,
                 "spirv_total_bytes":<n>} followed by exactly <n> kind-18
                 spirv-chunk-v1 frames, each at most MAX_PAYLOAD_BYTES (64
                 KiB, .chunks(MAX_PAYLOAD_BYTES) -- no extra bound needed
                 since that cap is already universal on this wire), raw
                 shaderc::CompilationArtifact::as_binary_u8 bytes
                 (native-endian u32 words -- the helper is always a CHILD
                 of its caller, same host/architecture, so no conversion
                 is ever needed on either side). On a compile failure:
                 kind-17 {"status":"compile-error","log":"<bounded to
                 kwe-report-protocol::MAX_SHADER_COMPILE_ERROR_LOG_BYTES,
                 4 KiB>"} -- exit 0 either way (task text: "a compile
                 error is a RESULT, not a helper failure").
                 kwe-report-protocol: validate_shader_compile_request
                 gains an OPTIONAL top-level "options" object
                 ({"target_env","target_env_version","optimization_level"},
                 each a bounded string, MAX_SHADER_OPTION_STRING_BYTES=128
                 -- additive, schema stays "shader-compile-request-v1" per
                 the task's "version the schema additively, keep v1 name";
                 absent means "use the compiling side's own defaults",
                 kwe-core's constants on both ends). validate_shader_
                 compile_response's single uniform shape (schema/status/
                 reason) becomes STATUS-DEPENDENT: "unimplemented"/
                 "protocol-error"/any other status keeps the original
                 reason-required shape; "ok" requires spirv_chunks/
                 spirv_total_bytes (both bounded -- MAX_SPIRV_CHUNKS=131,
                 one less than StreamCaps::SHADER_RESPONSE's 132-frame
                 budget since that budget also covers the kind-17 header
                 itself; MAX_SPIRV_TOTAL_BYTES=StreamCaps::SHADER_RESPONSE's
                 own 8 MiB total, refusing an over-claim BEFORE the caller
                 ever tries reading that many kind-18 frames -- this is
                 what makes the "header says 200 chunks" failure-injection
                 test refuse immediately rather than only failing once a
                 malicious helper actually streams 200 real frames);
                 "compile-error" requires a bounded "log". New
                 ShaderRequestError::OptionOversize and 3 new
                 ShaderResponseError variants (TooManySpirvChunks/
                 SpirvTotalBytesTooLarge/LogOversize).
                 kwe-scene-renderer/src/shader_helper.rs: HelperOutcome
                 gains CompileError(String) (no longer reserved/unused)
                 and Compiled{spirv,response} is now actually constructed
                 (finalize's "ok"/"compile-error" match arms; a new
                 reassemble_ok function reads exactly spirv_chunks kind-18
                 frames, validates the ACTUAL count/total read back
                 against the header's claim -- task item 2's "validate
                 count/total vs the kind-17 header; mismatch ->
                 ProtocolError -> fallback" -- before converting bytes to
                 Vec<u32> via chunks_exact(4).map(u32::from_ne_bytes)). A
                 new ShaderHelper::compile_stage_or_fallback(request,
                 label) -> Result<Vec<u32>, materialshader::CompileError>
                 is the new call-site entry point: Compiled -> Ok(spirv)
                 directly (skip in-thread); CompileError(log) ->
                 Err(CompileError::Failed(log)) WITHOUT retrying in-thread
                 (the same GLSL fails identically a second time -- task
                 item 4's "the SAME error path the in-thread compile error
                 takes"); every other outcome -> calls materialshader::
                 compile_stage in-thread, unchanged from SR-3b. main.rs's
                 two call sites (compile_one_material, vertex+fragment)
                 collapse from "call compile() and discard, then always
                 call compile_stage()" to one compile_stage_or_fallback
                 call each -- the Err-arm handling (fallback_reasons bump,
                 bounded diagnostic, return None) is UNCHANGED text,
                 proving it does not care which path produced the error.
                 The request's "options" is now always populated from
                 kwe-core's constants (task item 2's "populate the
                 request's options from the same values compile_stage
                 uses") -- the helper does NOT parse this field back into
                 its own compile behavior (it always uses its own copy of
                 the same kwe-core constants instead); documented as a
                 deliberate simplicity choice, not an oversight, since
                 parsing untrusted wire JSON back into shaderc enum
                 selection would be one more place the two sides could
                 silently diverge, and the byte-equality oracle test is
                 what actually proves the two sides agree, not the wire
                 field. A new compiled_count: AtomicU64 field plus a
                 Drop impl on ShaderHelper prints one bounded
                 "event=shader_helper.compiled count=<n>" line at process
                 teardown, only when count > 0 (task item 3's smoke
                 evidence line) -- KEPT permanently past this slice
                 (decision recorded below), not removed after verifying.
                 docs/SHADER_HELPER_PROTOCOL_V1.md: the request/response
                 schema sections rewritten for the "options" field and the
                 4 response shapes (was 2); the reason-code table gains
                 option-oversize; the "SR-3a skeleton scope" section
                 becomes "Implementation status by slice" (SR-3a/3b/3c
                 summarized in order, matching this document's own
                 evolving style). packaging/PKGBUILD: comment-only updates
                 (the shaderc depends/makedepends entries already covered
                 kwe-shader-compiler once it started linking it -- no new
                 entry, no pkgrel bump, confirmed by reading the arrays
                 back).
In scope:        crates/kwe-core/src/{lib.rs,shader_compile_spec.rs} (new
                 module, 4 consts, no shaderc dependency), crates/
                 kwe-shader-compiler/{Cargo.toml,src/main.rs} (shaderc +
                 kwe-core dependencies, compile_source/respond_ok/
                 respond_compile_error, 2 new unit tests), crates/
                 kwe-shader-compiler/tests/protocol.rs (a realistic
                 #version-carrying fixture replacing SR-3a's bare `void
                 main(){}`, 2 new integration tests: ok+real-spirv,
                 compile-error), crates/kwe-report-protocol/src/lib.rs
                 (options validation, status-dependent response
                 validation, 4 new bound constants, 4 new error variants,
                 9 new tests), crates/kwe-scene-renderer/src/
                 shader_helper.rs (Compiled/CompileError consumption,
                 compile_stage_or_fallback, reassemble_ok, the options
                 payload field, the compiled_count Drop line, 5 test
                 replacements/additions: the real-binary differential
                 oracle across 4 representative shaders, a compile-error
                 fallback test, a dies-mid-chunks test, an oversized-claim
                 test -- replacing SR-3b's now-stale "real binary answers
                 unimplemented" test), crates/kwe-scene-renderer/src/
                 main.rs (the two compile_one_material call sites
                 collapsed to compile_stage_or_fallback), docs/
                 SHADER_HELPER_PROTOCOL_V1.md, packaging/PKGBUILD
                 (comments only), docs/SR3.md (this section).
Out of scope:    The long-lived serial-loop question (decision (c), SR-3a)
                 -- STILL not resolved: this slice makes the per-request
                 compile real but does not measure spawn cost against
                 compile latency in production and does not build a
                 serial-loop mode; recorded again as an open risk below,
                 now with no more excuses left to defer it behind ("no
                 real compile to measure yet" is no longer true).
                 SPIR-V reflection (SR-3d) and the bounded cache (SR-3e) --
                 both still untouched, their reserved response fields
                 (reflection, cache_key) still absent from every response
                 this helper emits. An env-gated double-compile diff in
                 PRODUCTION code (decision recorded in-line: tests carry
                 the differential, production carries fallback only -- see
                 Open risks for the honesty-boundary this leaves).
                 Removing the in-thread compile_stage path entirely --
                 decision (b) explicitly keeps it as fallback; a LATER
                 SR-3 slice removes it once the helper path is trusted
                 enough (still has the with_timeout detached-thread flaw
                 materialshader.rs's own doc comment names).
Acceptance tests:        kwe-report-protocol: 46 tests (up from 37) -- 5
                         new "options" tests (absent is valid, present-
                         and-complete round-trips, wrong type, all-3-
                         required-together at every single-field-missing
                         combination, bounded-at-the-byte-boundary); 4 new
                         response tests ("ok" requires both fields (with
                         each individually missing) and has no "reason";
                         spirv_chunks bounded at MAX_SPIRV_CHUNKS/+1;
                         spirv_total_bytes bounded; "compile-error"
                         requires a bounded "log" and has no "reason").
                         kwe-shader-compiler: 21 tests (up from 18) -- 2
                         new unit (compile_source compiles valid GLSL to
                         SPIR-V starting with the correct magic number,
                         skip-if-libshaderc-unavailable; a GLSL syntax
                         error is Err not a panic); integration tests
                         updated for realistic #version-carrying sources
                         (SR-3a's bare "void main(){}" fixture doesn't
                         compile under a Vulkan target -- Desktop GLSL
                         needs #version >= 140 -- discovered by running
                         the OLD fixture through the new real compile
                         path and reading the actual shaderc error, not
                         guessed), plus 2 new integration tests: a valid
                         request now gets "ok" + real SPIR-V (magic-number-
                         checked, chunk-count/total-bytes cross-checked
                         against the header) rather than "unimplemented";
                         bad GLSL gets "compile-error" + exit 0, not
                         protocol-error/non-zero.
                         kwe-scene-renderer: 370 tests (up from 367, 1
                         ignored unchanged) -- the differential oracle
                         (real_helper_binary_produces_byte_identical_
                         spirv_to_in_thread_compile) runs 4 representative
                         shaders (task item 3's own list) THROUGH THE REAL
                         shaderpre::preprocess pipeline first (a raw,
                         un-preprocessed source lacks the #version
                         shaderpre injects, so this is what compile_stage
                         actually ever sees in production) then through
                         BOTH the real helper binary and the in-thread
                         path, asserting byte-identical Vec<u32> SPIR-V:
                         (1) plain_quad.frag (materialshader::tests::
                         compile_round_trip_produces_spirv's own source),
                         (2) combo.frag (a [COMBO] LIGHTING=1 override
                         that changes which #if branch compiles --
                         shaderpre::tests::
                         material_combo_override_wins_over_shader_
                         default's pattern), (3) include.frag (a real
                         #include splice whose function is actually
                         CALLED, not just present -- shaderpre::tests::
                         include_resolves_and_inlines's pattern made
                         load-bearing), (4) plain_quad.vert (shaderpre::
                         tests::
                         vertex_shader_is_not_wrapped_for_premultiplication's
                         source) -- all 4 pass, skip-with-note if
                         kwe-shader-compiler is not already built (the
                         SR-3b convention) or if libshaderc is
                         unavailable. 3 new failure-injection tests (task
                         item 4): helper_compile_error_surfaces_as_the_
                         same_compile_stage_error_shape (bad GLSL through
                         the REAL helper -> compile_stage_or_fallback's
                         Err arm is CompileError::Failed, same as an
                         in-thread failure); helper_that_dies_mid_chunks_
                         is_a_protocol_error_and_falls_back_in_thread (a
                         fake helper declares 2 spirv_chunks, writes zero
                         -> ProtocolError, then the FULL compile_stage_or_
                         fallback pipeline still produces a real compile
                         via fallback -- "the layer still draws"
                         literally proven, not just asserted);
                         oversized_spirv_chunk_claim_is_refused_by_the_
                         response_cap (a fake helper claims 200 chunks in
                         its header -> ProtocolError from kwe-report-
                         protocol's own MAX_SPIRV_CHUNKS check, refused
                         BEFORE any kind-18 frame is read -> fallback
                         still compiles). The stale SR-3b test asserting
                         the real binary answers "unimplemented" was
                         REMOVED (no longer true: the real binary now
                         compiles) rather than left to bit-rot.
                         937 workspace tests total, up from 922.
                         cargo fmt --all -- clean.
                         cargo clippy --workspace --all-targets -- -D
                         warnings -- clean.
                         cargo test --workspace -- 937 passed, 0 failed.
                         KWE_SCENE_IR_PARITY_DIR=<real corpus> ir_parity_
                         corpus -- 60/60 item(s) parity-passed (unaffected
                         -- this slice touches no scene-parsing code).
                         ./scripts/check.sh -- exit 0, green end-to-end.
                         scripts/smoke-scene.sh -- exit 0, every case
                         passes unchanged INCLUDING the pixel-exact
                         material-shader oracles (S2/S3/S4/S5a/S5b) --
                         and, verified directly (a scratch copy of the
                         script with its cleanup trap disabled, run once
                         to inspect the preserved per-case renderer logs
                         before they were deleted -- smoke-scene.sh
                         redirects each standalone renderer's OWN stderr
                         to a per-case log file inside its own mktemp
                         root, not to the script's own stdout/stderr, so
                         "eyeball it in the smoke output" required
                         looking at those files specifically): the
                         teardown line actually fired in every one of the
                         5 material-shader cases -- standalone-s2-
                         material.log count=2, standalone-s3-effects.log
                         count=4, standalone-s4-material.log count=2,
                         standalone-s5a-effects.log count=4, standalone-
                         s5b-ffb.log count=4 -- and zero non-success
                         shader_helper_outcome diagnostic lines appeared
                         anywhere (every compile in every case succeeded
                         via the real helper, none fell back). Remove-or-
                         keep decision: KEEP the teardown line permanently
                         (not a one-off verification aid) -- a standing,
                         bounded, opt-in-cost (only prints when count>0)
                         signal that the helper path is actually being
                         used in the field is worth more than the
                         negligible log-line cost.
Failure/recovery tests:  helper_compile_error_surfaces_as_the_same_
                         compile_stage_error_shape (bad GLSL -> identical
                         caller-visible Err shape, no in-thread retry);
                         helper_that_dies_mid_chunks_is_a_protocol_error_
                         and_falls_back_in_thread (a crashed/truncated
                         helper mid-response -> ProtocolError -> the FULL
                         pipeline still compiles via fallback, not just
                         the raw HelperOutcome classified correctly);
                         oversized_spirv_chunk_claim_is_refused_by_the_
                         response_cap (a dishonest chunk-count claim ->
                         refused by kwe-report-protocol's own cap ->
                         fallback); the compile_source unit test proving a
                         GLSL syntax error is Err, never a panic.
Upstream/provenance:    Original; the option-sharing design (plain consts
                         in kwe-core, no shaderc type there) and the
                         synchronous-compile-no-internal-timeout choice
                         (decision (b)'s payoff, contrasted explicitly
                         against materialshader::compile_stage's own
                         documented with_timeout/detached-thread flaw) are
                         both original engineering judgment calls for this
                         slice, not copied from any external source.
Commands run and results: cargo fmt --all -- clean.
                         cargo clippy --workspace --all-targets -- -D
                         warnings -- clean.
                         cargo test --workspace -- 937 passed, 0 failed
                         (an unrelated environment issue surfaced first:
                         /tmp had filled to 100% with ~76,500 leftover
                         per-test scratch directories accumulated across
                         many past sessions' cargo test runs, causing 60
                         StorageFull failures in kwe-core's pkg/scan/vfs/
                         preflight/playlist tests -- confirmed via the
                         exact panic messages (Os { code: 28, kind:
                         StorageFull }) and the directory-name patterns
                         (kwe-daemon-cache-*, kwe-scene-test-*, etc. --
                         this project's own test fixtures, not third-party
                         or another user's data); cleared, then a clean
                         rerun passed 0 failed -- unrelated to this
                         slice's own changes, noted here for the record
                         since it briefly looked like a regression).
                         KWE_SCENE_IR_PARITY_DIR=<real corpus> ir_parity_
                         corpus -- 60/60 parity-passed.
                         ./scripts/check.sh -- exit 0.
                         scripts/smoke-scene.sh -- exit 0, all cases
                         passed, helper-active evidence confirmed (above).
Open risks:              The long-lived serial-loop question (decision
                         (c), first recorded SR-3a) is STILL unresolved --
                         this slice had a real compile to measure spawn
                         cost against and did not use the opportunity;
                         genuinely deferred again, not forgotten.
                         Honesty boundary (recorded per the task's own
                         instruction): no env-gated double-compile diff
                         exists in PRODUCTION code -- a silent helper/
                         in-thread divergence introduced later (e.g. a
                         shaderc version drift between the two crates'
                         Cargo.lock resolutions, or a future edit to one
                         side's option-building that forgets the other)
                         would go UNNOTICED in the field. Mitigated by:
                         identical options (kwe-core's shared constants),
                         identical shaderc version/source (workspace-
                         pinned), and this slice's own differential-oracle
                         tests -- but those tests only run in CI/dev, not
                         in production.
                         [RESOLVED — SR-3c2] The helper does not actually
                         CONSUME the wire request's "options" field for
                         its own compile behavior (documented above) -- it
                         is populated for self-description/audit but
                         currently advisory only; a future caller wanting
                         genuinely different options than kwe-core's fixed
                         defaults would need the helper to start honoring
                         it.
                         RLIMIT_NPROC still left unset for the helper
                         (SR-3b's own open risk, unchanged this slice).
                         The PDEATHSIG mechanism remains proven by
                         precedent rather than a direct test in this
                         module (SR-3b's own open risk, unchanged).
STOP findings:           None. Neither STOP condition named for this task
                         triggered: in-thread vs. helper SPIR-V IS byte-
                         identical for all 4 representative fixtures
                         (asserted, not just visually spot-checked), and
                         adding shaderc to the kwe-shader-compiler crate
                         did NOT break the workspace build graph (verified
                         by a clean cargo build/test/clippy across the
                         whole workspace -- kwe-core, the crate nearly
                         everything else depends on, stayed shaderc-free
                         by design, so no other crate's dependency graph
                         changed at all).
Commit(s):               45b4b67
```

## SR-3c2 — the helper compiles with the wire options it receives (honesty-boundary close-out)

Post-build slice (lands for pkgrel 22), filed after SR-3c merged and
pkgrel 21 built, trunk `437863d`.

Conductor decisions (verbatim):

- Small slice: close the SR-3c honesty boundary. The helper must COMPILE
  WITH the wire `options` it receives, not its own copy of the constants.
- ABSENT options (or absent individual fields) keep today's behavior:
  `kwe-core`'s `shader_compile_spec` constants as defaults — byte-
  compatible with every existing caller/test.
- An options VALUE outside the known vocabulary (unknown target env
  string, unknown opt level) → `{"status":"protocol-error","reason":
  "bad-options"}` exit 65, never a silent fallback to defaults.
- Renderer side already sends the populated options (SR-3c) — verify, no
  change expected.

```text
Task:            Make kwe-shader-compiler actually compile WITH the wire
                 request's "options" object instead of its own hardcoded
                 copy of kwe-core's constants (SR-3c's own documented
                 honesty-boundary gap) — an out-of-vocabulary value
                 refuses (protocol-error), never silently falls back.
Milestone/Slice: SR-3c2
Goal:            Close the exact gap SR-3c's own open risks section named:
                 "the helper does not actually CONSUME the wire request's
                 options field for its own compile behavior... it is
                 populated for self-description/audit but currently
                 advisory only." After this slice the wire field is no
                 longer advisory.
Outcome:         kwe-report-protocol: validate_shader_compile_request's
                 "options" handling relaxes from "all three sub-fields
                 required together when the object is present" to "each
                 sub-field independently optional" -- a present-but-
                 partial (or even empty {}) options object is now valid,
                 matching the task's "absent individual fields keep
                 today's behavior" requirement (this MUST be possible for
                 per-field defaulting to ever be exercised by a real
                 caller). Shape checking (string, bounded to
                 MAX_SHADER_OPTION_STRING_BYTES) is unchanged for
                 whichever fields ARE present; VALUE vocabulary is still
                 not this crate's concern (unchanged from SR-3c).
                 kwe-shader-compiler gains resolve_wire_options(wire_options:
                 Option<&Value>) -> Result<ResolvedOptions, ()>: each of
                 the 3 wire sub-fields independently defaults to
                 kwe-core::shader_compile_spec's own constant when absent
                 (TARGET_ENV="vulkan", TARGET_ENV_VERSION="1.2",
                 OPTIMIZATION_LEVEL="zero"), then each resolved string is
                 matched against the vocabulary this crate actually
                 supports: target_env must be "vulkan" (the only target
                 this codebase ever compiles for); target_env_version
                 must be one of "1.0".."1.4" (shaderc::EnvVersion's own
                 Vulkan1_0..=Vulkan1_4 variants); optimization_level must
                 be one of "zero"/"size"/"performance" (shaderc::
                 OptimizationLevel's three variants). Any present value
                 outside this vocabulary is Err(()) -- run() turns this
                 into respond_protocol_error("bad-options") (exit 65)
                 BEFORE compile_source (and therefore before any shaderc::
                 Compiler/CompileOptions call) is ever reached, keeping
                 "bad-options" cleanly distinct from "compile-error"
                 (reserved for a real GLSL/shaderc failure). compile_source
                 now takes &ResolvedOptions instead of hardcoding
                 kwe-core's constants directly -- entry_point stays fixed
                 to kwe-core::ENTRY_POINT (not on the wire; nothing to
                 negotiate, unchanged from SR-3c).
                 kwe-scene-renderer: VERIFIED, no code change -- SR-3c
                 already populates "options" from kwe-core::
                 shader_compile_spec on every request (shader_helper.rs's
                 compile_inner), which is exactly the helper's own default
                 vocabulary, so every request this renderer builds
                 resolves successfully; the renderer never sends a value
                 outside the known vocabulary, so "bad-options" can never
                 fire against it. The comment at the "options" JSON
                 literal (previously: "the helper does not actually parse
                 these back into shaderc types... not because the
                 helper's compile behavior depends on it today") is
                 updated to say the opposite, now true.
In scope:        crates/kwe-report-protocol/src/lib.rs (the "options"
                 per-field-optional relaxation, 2 test replacements),
                 crates/kwe-shader-compiler/src/main.rs
                 (ResolvedOptions/resolve_wire_options, compile_source's
                 new parameter, run()'s new bad-options branch, 6 new
                 unit tests), crates/kwe-shader-compiler/tests/protocol.rs
                 (4 new integration tests), crates/kwe-scene-renderer/src/
                 shader_helper.rs (comment-only, at the "options" JSON
                 literal), docs/SHADER_HELPER_PROTOCOL_V1.md (the
                 `options` field section rewritten: per-field-optional,
                 the vocabulary table, "bad-options" in the reason-code
                 table), docs/SR3.md (SR-3c's own open-risk line
                 annotated RESOLVED, this section).
Out of scope:    entry_point stays a fixed kwe-core constant, not added to
                 the wire schema -- the task named "target env/version,
                 optimization level, entry point" as the four kwe-core
                 constants this slice is ABOUT, but only three of them
                 have ever been wire fields (entry_point is a
                 compile_into_spirv PARAMETER, not a shaderc::
                 CompileOptions setting, and nothing in this codebase
                 varies it) -- widening the wire schema to add a 4th
                 negotiable field was not asked for and would be scope
                 creep for a "small slice". The long-lived serial-loop
                 question (SR-3a decision (c)) -- still unresolved, not
                 touched. Reflection/cache (SR-3d/SR-3e) -- untouched. The
                 env-gated double-compile diff in PRODUCTION code (SR-3c's
                 other honesty-boundary item, tests-only) -- SR-3c2 closes
                 the OPTIONS gap specifically; the double-compile-diff gap
                 is a SEPARATE, still-open item (not this task's scope).
Acceptance tests:        kwe-report-protocol: 47 tests (up from 46) -- the
                         old "all three required together" test replaced
                         by shader_request_options_partial_object_is_valid
                         (each single field missing, AND an empty {}
                         object, all valid) plus a new wrong-type-on-a-
                         present-field test.
                         kwe-shader-compiler: 30 tests (up from 21) -- 6
                         new unit tests (no-options resolves to exactly
                         kwe-core's defaults; a partial object defaults
                         only the ABSENT fields; every documented
                         vocabulary value for every field is accepted;
                         an unknown value for ANY field is Err;
                         optimization_level_actually_changes_the_compiled_spirv,
                         the task's own "do not fake it" instruction taken
                         literally -- a shader with an unused variable and
                         a loop whose result is multiplied by a compile-
                         time 0.0 (both dead code at higher optimization),
                         compiled at "zero" vs "performance", empirically
                         confirmed to produce DIFFERENT SPIR-V (1132 bytes
                         vs 304 bytes on this shaderc build) BEFORE the
                         test was written, not assumed); 4 new integration
                         tests against the REAL binary: explicit options
                         equal to kwe-core's constants byte-identical to
                         no options at all (defaults-wiring proof); the
                         SAME optimization-sensitive fixture through the
                         real binary, default vs "performance", SPIR-V
                         differs (the wire-consumption proof, at the
                         actual protocol boundary, not just the Rust
                         function); an unrecognized target_env ->
                         protocol-error/bad-options/exit 65; an
                         unrecognized optimization_level -> the same
                         (covers more than one option field).
                         kwe-scene-renderer: unchanged at 370 tests (1
                         ignored) -- EVERY SR-3c differential/failure-
                         injection test (the 4-shader byte-equality
                         oracle, the compile-error/dies-mid-chunks/
                         oversized-claim failure-injection tests) reran
                         and stayed green UNTOUCHED, confirming the wire
                         options now genuinely being consumed by the
                         helper does not disturb byte-identity (expected:
                         this renderer always sends kwe-core's own
                         defaults, which is exactly what the helper used
                         to hardcode -- SR-3c2 changes WHERE the values
                         come from, not WHAT they are, for this caller).
                         947 workspace tests total, up from 937.
                         cargo fmt --all -- clean.
                         cargo clippy --workspace --all-targets -- -D
                         warnings -- clean.
                         cargo test --workspace -- 947 passed, 0 failed.
                         ./scripts/check.sh -- exit 0 (one retry needed:
                         the first run hit a single unrelated flaky
                         failure, js::tests::soft_budget_skips_frame_then_recovers,
                         a wall-clock script-execution-budget test with no
                         connection to this slice's files; confirmed
                         flaky by re-running it alone immediately after,
                         which passed cleanly, then re-running the whole
                         script for a clean end-to-end record).
Failure/recovery tests:  bad_options_value_is_a_protocol_error_exit_65 and
                         bad_optimization_level_is_also_a_protocol_error
                         (kwe-shader-compiler/tests/protocol.rs) -- an
                         out-of-vocabulary value never reaches shaderc,
                         never becomes "compile-error", always exits 65
                         with reason "bad-options". resolve_wire_options_
                         rejects_an_unknown_value_for_any_field (unit
                         level, all three fields covered independently).
Upstream/provenance:    Original; the vocabulary (Vulkan-only target env,
                         shaderc's own EnvVersion/OptimizationLevel
                         variant sets) is read directly from the
                         `shaderc` crate's own enum definitions
                         (shaderc-0.10.1/src/lib.rs), not guessed or
                         copied from elsewhere.
Commands run and results: cargo fmt --all -- clean.
                         cargo clippy --workspace --all-targets -- -D
                         warnings -- clean.
                         cargo test --workspace -- 947 passed, 0 failed.
                         ./scripts/check.sh -- exit 0 (after the flaky-
                         test retry described above).
Open risks:              entry_point remains fixed/not wire-configurable
                         (documented above as an explicit scope decision,
                         not an oversight) -- if a future caller ever
                         needs a different entry point, the wire schema
                         would need a genuinely new field, not a reuse of
                         this slice's plumbing.
                         The env-gated double-compile diff in PRODUCTION
                         code remains absent (SR-3c's OTHER honesty-
                         boundary item, explicitly out of scope here) --
                         a silent divergence between the renderer's
                         in-thread fallback path and the helper's
                         now-wire-driven compile would still go unnoticed
                         in the field; mitigated the same way SR-3c
                         recorded (identical options, identical shaderc
                         version, CI-only differential-oracle tests).
                         The long-lived serial-loop question (SR-3a
                         decision (c)) is STILL unresolved.
                         RLIMIT_NPROC and the PDEATHSIG-by-precedent gap
                         (SR-3b's own open risks) are unchanged.
STOP findings:           None. No STOP condition was named for this task.
Commit(s):               a7b3c4c
```
