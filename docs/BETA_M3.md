# Beta M3 SceneScript engine foundation

M3 starts the original SceneScript engine: a supervised worker that runs a
scene.json descriptor plus a QuickJS script and publishes rendered frames
through the shared frame protocol, entirely inside its own process (ADR
0001 — the original Vulkan renderer is law; nothing is loaded into
plasmashell). M3a builds the foundation slice: the scene entry format, the
bounded script engine, and the offscreen Vulkan compositor that clears a
frame to the script-driven color and publishes it. The archive reader
(M3b), the rest of the scene surface (layers, effects, text, particles, 3D,
properties — M3c–M3k), and any manager changes are deliberately out of
scope.

## Goal

`kwe-scene-renderer` runs as the daemon's `scene` kind. It parses a
`scene.json` descriptor (≤ 16 MiB), evaluates the referenced script
(`general.script`, ≤ 2 MiB, must stay inside the content root) in a
per-worker QuickJS runtime (rquickjs 0.12.2, MIT — see THIRD_PARTY.yml;
heap cap 64 MiB, stack cap 4 MiB), calls `init()`, then `update(dt)` on the
pacing cadence with an 8 ms soft / 33 ms hard per-update wall-clock budget,
and renders each step offscreen with Vulkan: a W×H `COLOR_OPTIMAL`
attachment, a fullscreen triangle pipeline, image→buffer copy→map→BGRA
convert, published as premultiplied BGRA8888 through the
`SharedFrameWriter` (2-slot seqlock, header, keepalive, producer states).
The clear color is the scene's `general.clearcolor` unless the script
writes `Engine.clearcolor`, which is read back after every `update()`.
SIGTERM stops gracefully (`Stopping` state, exit 0); a resource denial
exits 71; an unusable scene or backend exits 73 before the canary.

The supervisor contract is unchanged: same argv/env/rlimit shape as the
other workers, bounded stdin input (pointer, audio bands, media state — all
acked, none validated for sequence monotonicity per the M1a review
decision), bounded stderr ring, fault flags, canary promote, quarantine.
The daemon's scene support (kind, `ContentSpec::Scene`, `--content` spawn,
default renderer path `kwe-scene-renderer` beside the daemon, kind-qualified
quarantine identity) predates M3a and is exercised unchanged.

## Run the suites

```sh
scripts/smoke-scene.sh       # M3a: scene renderer through the daemon,
                             #   scripted-color oracle, containment, plus a
                             #   standalone llvmpipe lane
scripts/smoke-video.sh       # unchanged (M1 regression lane)
scripts/smoke-supervisor.sh  # unchanged (M1a regression lane)
```

`smoke-scene.sh` builds the workspace, uses a private temporary
socket/runtime/state tree, generates the scene.json + script.js fixtures at
runtime (never committed), and removes everything on exit. It does not
install a wallpaper or touch the running Plasma session; a `pgrep -x
plasmashell` pid guard asserts the suite never touches an existing
plasmashell.

## Acceptance evidence

Validated on 2026-08-19 (CachyOS, NVIDIA GeForce RTX 3070 discrete GPU for
the daemon lane, llvmpipe software rasterizer for the standalone lane).

### M3a — scene engine core (this commit)

| Case | Expected containment | Result |
|---|---|---|
| workspace gates | `cargo fmt --all -- --check`; `cargo clippy --workspace --all-targets -- -D warnings`; `cargo test --workspace --all-targets` | clean; 254 tests pass (32 in `kwe-scene-renderer`) |
| daemon lane: live start | `kind:"scene"`, content hash, advancing sequence, failures 0, frame file present | reached `live` on the discrete GPU (B8G8R8A8_UNORM) |
| frame oracle (daemon lane) | scripted clear color in the shared frame: init()-pinned g/b, sawtooth r from `update()` | two center-pixel samples 1.5 s apart: R 137 → 74 (delta 63 ≥ 32), G/B pinned by init |
| throwing script | contained: renderer stays live, sequence advances, diagnostics bounded | `event=renderer.scene.script_error` in the ring, 1–2 lines (30 s re-report window), failures 0 |
| kill -9 | one failure (`process_exit`, `signal_9`), auto-restart, promotion clears the record | restarted live, new pid, failures 0 |
| three kills | quarantined; `renderer.start` refused for the identity | failures 3, phase `quarantined`, refused |
| garbage scene.json | passes static preflight, worker rejects before the canary | exit 73 → `rolled_back`, `exit_code_73` in the detail, base worker stays live |
| missing script file | same backend rejection | exit 73 → `rolled_back`, `exit_code_73` |
| plasmashell pid guard | no plasmashell touched | pid unchanged across the suite |
| final stop | graceful stop, health ok | phase `stopped`, pid null |
| standalone llvmpipe lane | worker directly under `VK_ICD_FILENAMES` + `--device llvmpipe` | scripted-color oracle passes (R 5 → 197), SIGTERM exit 0, `Stopping` state (3) in the header, `event=renderer.complete frames=... script_errors=0 soft_timeouts=0 hard_timeouts=0` |
| device diagnostics | bounded stderr lines | `event=renderer.scene.device name=... kind=... format=...` on both drivers |

## Renderer exit codes

| Code | Meaning | Supervisor mapping |
|---|---|---|
| 0 | graceful stop (SIGTERM) | normal stop |
| 70 | `--exit-after` synthetic fault | `process_exit` failure |
| 71 | memory denied (QuickJS heap cap hit, or `--memory-pressure-after`) | `process_exit` failure (`resource_limit`) |
| 72 | memory-pressure allocation unexpectedly succeeded | `process_exit` failure |
| 73 | backend rejection: scene parse (bad JSON, wrong shape, script non-string or over caps), missing/unreadable script, Vulkan device/compositor unusable, sustained render failure streak | `exit_code_73` in `last_failure_detail` |
| signal | killed (e.g. kill -9) | `process_exit` with `signal_9` |

A script exception is *not* an exit path: the exception is caught, counted
(`script_errors` in `event=renderer.complete`), logged once per error class
per 30 s window, and the renderer keeps publishing the last good frame. A
hard budget hit raises an uncatchable exception that is recognized and
counted as a `hard_timeout`, not a script error.

## Open risks

1. **Wall-clock interrupt budget (documented deviation).** The task spec
   asks for a per-update budget; rquickjs 0.12.2 exposes no interpreter
   step/instruction counter (QuickJS's `js_interrupt` callback is the only
   hook), so the budget is enforced with a wall clock inside the interrupt
   callback: 8 ms soft (frame skipped, bounded `script_timeout` diag) /
   33 ms hard (interrupt → uncatchable exception). A single script can
   still monopolize the *renderer's* thread between two checkpoints of a
   pathological busy loop for longer than the soft budget; the supervisor's
   frame timeout remains the outer bound.
2. **llvmpipe determinism.** The daemon lane picks the discrete GPU when
   present; the standalone lane pins `VK_ICD_FILENAMES` to lvp and
   `--device llvmpipe`, and the acceptance suite requires the llvmpipe lane
   to pass (the software rasterizer is deterministic for a clear pass and
   the channel-order conversion is byte-exact by unit test). A machine with
   only llvmpipe runs both lanes on it; a machine with neither fails the
   suite loudly rather than silently passing.
3. **Vulkan loader lifetime (resolved, fragile).** ash 0.38's `Entry`
   holds the dlopen guard for `libvulkan.so.1`; device function pointers
   mix raw ICD entries with loader trampolines, so dropping the last
   `Entry` mid-run makes teardown SIGSEGV inside the loader. The renderer
   therefore keeps `_entry: Entry` alive for the struct's lifetime,
   declared last so it drops after the explicit `Drop` body. Any refactor
   that moves the entry must keep it alive as long as the device.
4. **Pipeline-bind VUID (resolved).** Drawing without a bound pipeline
   faults: llvmpipe SIGSEGVs in a worker thread, NVIDIA reports device
   lost at the fence wait. `cmd_bind_pipeline` is recorded every frame
   before the draw; both failures were found and fixed with the isolated
   `KWE_TEST_DEVICE`-gated render test in vulkan.rs.
5. **Frame-file reader staleness.** During M3a debugging, the smoke's
   scene oracle opened the shared frame file once and read it with seek +
   partial reads; after the first read it kept seeing a frozen generation
   and frozen pixels while the producer was publishing normally (verified
   by whole-file reads of the same file in the same run). The oracle now
   re-opens the file and reads it whole per snapshot attempt — the
   established `frame-read.py` pattern. The M1 video oracle has the same
   latent pattern but was benign there (static content); a future oracle
   for dynamic content must use whole-file re-opens.
6. **Memory-limit recognition.** QuickJS raises a JS "Out of memory"
   exception (not an allocation error) when the 64 MiB heap cap is hit, so
   the limit is recognized from the exception message and exits 71
   (bounded, fatal); the memory-pressure fault flag is also exercised by
   `smoke-supervisor.sh`'s test-renderer lane. The unit tests keep the
   exit-71 decision pure.
7. **`resized(w, h)` has no live path in M3a.** It is called once with the
   daemon-provided size at script load; dimensions are fixed for the
   worker's lifetime (the supervisor restarts a renderer whose geometry
   changes). The scene's own `general.resolution` and `general.fps` are
   parsed, validated, and logged when they mismatch the daemon's request,
   but do not override it.
8. **Scene/script bounds are static, not semantic.** A scene.json can
   reference a 2 MiB script of pure busy loops; the interrupt budget and
   the supervisor timeouts contain it, but a "hello-world-but-spinning"
   scene consumes a full core at 33 ms hard-budget skips. Acceptable for
   M3a's contained worker; a per-script CPU budget is a live-apply concern.
