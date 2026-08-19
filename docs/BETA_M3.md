# Beta M3 SceneScript engine foundation

M3 starts the original SceneScript engine: a supervised worker that runs a
scene.json descriptor plus a QuickJS script and publishes rendered frames
through the shared frame protocol, entirely inside its own process (ADR
0001 — the original Vulkan renderer is law; nothing is loaded into
plasmashell). M3a builds the foundation slice: the scene entry format, the
bounded script engine, and the offscreen Vulkan compositor that clears a
frame to the script-driven color and publishes it. M3b adds the original
scene.pkg archive reader so packaged wallpapers (the Steam Workshop shape:
all 60 corpus wallpapers are packages, none ship a bare scene.json) run
end-to-end. M3c adds 2D image layers: a textured-quad compositor (per-layer
push-constant transforms, src-over blending, draw order = scene.json
order), bounded image decoding from the content root or the package entry
table, and the `Scene.getLayer` layer proxy so scripts can move, resize,
rotate, fade, and hide layers at runtime. The rest of the scene surface
(effects, text, particles, 3D, user properties — M3d–M3k) and any manager
changes are deliberately out of scope.

## Goal

`kwe-scene-renderer` runs as the daemon's `scene` kind. It parses a
`scene.json` descriptor (≤ 16 MiB), evaluates the referenced script
(`general.script`, ≤ 2 MiB, must stay inside the content root) in a
per-worker QuickJS runtime (rquickjs 0.12.2, MIT — see THIRD_PARTY.yml;
heap cap 64 MiB, stack cap 4 MiB), calls `init()`, then `update(dt)` on the
pacing cadence with an 8 ms soft / 33 ms hard per-update wall-clock budget;
the same hard budget also guards the load phase (eval/init()/resized()) — a
load-phase abort disables the script and the renderer keeps publishing the
scene's clear color,
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

### M3b — scene.pkg archive reader

A `--content` ending in `.pkg` is opened with the original bounded
`kwe_core::pkg::PkgReader` (kwe-core, no unsafe, TOCTOU-safe: symlink-
metadata check, `O_NOFOLLOW|O_CLOEXEC`, fd `fstat` re-check, all reads
pinned to the fd). Verified corpus layout: u32 LE magic length (8) +
`PKGV` + 4 ASCII digits (any version — the 60-package corpus spans 20
distinct versions: **PKGV0001, PKGV0002, PKGV0004, PKGV0005, PKGV0007,
PKGV0009, PKGV0011–PKGV0024**; the QuickBMS writeup's "PKGV0001, PKGV0006
and so on are all the same format" holds) + u32 LE entry count + per entry
{u32 LE path length, UTF-8 path, u32 LE offset relative to the data section
start, u32 LE size} + raw concatenated payloads. Bounds: magic ≤ 32 B,
entries ≤ 65536, path ≤ 512 B, total payload ≤ 512 MiB, per-entry read cap
64 MiB, scene.json cap 16 MiB, script cap 2 MiB. The 512 MiB package cap
rejects larger real packages at preflight (`invalid_params`); none of the
60 corpus packages exceeds it (max ≈ 172 MiB — the 3765081478 probe is
142 MiB). Preflight (structural, no decompression) closes M1 finding G12
("pkg unvalidated"): every scene pkg is validated at `renderer.start`
before any worker spawns, and — preflight/worker cap parity — an
oversized scene.json (16 MiB) or script (2 MiB) entry is refused statically
as `invalid_params` instead of bouncing workers.

The renderer locates exactly one `scene.json` entry — the exact basename,
case-insensitive, with at most one leading directory component
(`scene.json`, `dir/scene.json`; `myscene.json` and `a/b/scene.json` do
not count — `kwe_core::scene_json_entry`, shared with preflight); a
`scene.pkg` entry with no `scene.json` is refused — "nested scene.pkg
inside the package is not supported" — and a package with two `scene.json`
entries is refused, decompresses it if needed (bounded mid-stream decode —
the declared size is never trusted) and feeds the existing M3a scene parse.
`general.script` names an entry inside the package (exactly one match, no
absolute/`..`/NUL/backslash paths); the script is extracted to a private
0700 directory under the worker's own HOME. The worker removes that
directory on its graceful exit path; a stale directory left by a hard kill
is replaced by the pid-recycle retry, so a restarted worker with a
recycled pid never bounces on `AlreadyExists`.

Honest finding: the corpus shows **no compression at all** — all 3128
payloads are raw, and the format has no per-entry compression flag. The
LZ4 frame path (`lz4_flex`, MIT, THIRD_PARTY.yml) is therefore a defensive
detector for payloads beginning `04 22 4D 18`, exercised by unit tests and
the smoke suite's optional `lz4` CLI case, while raw payloads are the
corpus-proven primary. Raw fallback policy: a payload that begins with the
frame magic but does not decode as a frame is treated as raw (one bounded
diagnostic line); an over-cap decompression is never downgraded.

### M3c — 2D image layers

An `objects` entry with an `image` property becomes a textured quad in the
compositor's draw list, in scene.json order — later objects draw over
earlier ones; no z-sorting, matching the original's array-order drawing.
Each layer is a unit quad of 6 fan-ordered vertices
`[v0,v1,v2, v0,v2,v3]` (v0=(-0.5,-0.5,0,0), v1=(0.5,-0.5,1,0),
v2=(0.5,0.5,1,1), v3=(-0.5,0.5,0,1)) drawn as **two** 3-vertex
`TRIANGLE_LIST` draws: a 4-vertex draw emits only the first triangle (the
quad's right half) and empirically a single multi-primitive draw rasterizes
only its first primitive on both llvmpipe and NVIDIA, so the split is
required to tile the full quad. The vertex shader has **no y-flip**: scene
y grows down while NDC y grows up, so scene y=0 maps to NDC y=-1, the
framebuffer's bottom; `VK_IMAGE_TILING_OPTIMAL` color attachments come back
bottom-first in the readback (row 0 = the scene's top row), so rendering
the scene bottom-first delivers it upright — proven on both drivers with a
shader that skipped the transform (the readback mirrored it).

Per-layer push constants are 48 bytes: m0=(a,c,tx,0), m1=(b,d,ty,alpha),
viewport=(w,h,0,0); `world = mat2(m0.xy, m1.xy) * pos + vec2(m0.z, m1.z)`
maps a layer-local point through rotate × scale × size and the translation.
The layer origin is the quad center (WE's "center" alignment), scene (0,0)
is the frame center, +y down; angles are radians in the file and degrees
in the `Scene.getLayer` API; `size` [0,0] defaults to the decoded texture
dimensions. Each layer has its own descriptor set (combined image sampler,
linear min/mag, clamp-to-edge) from a bounded pool of at most `MAX_LAYERS`
(256) sets, so a runtime texture swap can land per layer later — planned,
not implemented: the M3c `image` property is load-time only.

Blending is src-over by default: color SRC_ALPHA / ONE_MINUS_SRC_ALPHA,
alpha ONE / ONE_MINUS_SRC_ALPHA. The alpha factor must be ONE — SRC_ALPHA
double-scaled the layer alpha (191/255 over an opaque destination produced
143/255 instead of 191/255). The fragment shader multiplies the decoded
alpha: `outColor = vec4(c.rgb, c.a * pc.m1.w)` (straight source). Oracle,
byte-exact on both drivers: opaque texel (64,103,142,255) at layer alpha
191/255 over a zero clear blends to stored (48,77,106,191), which the
readback premultiply converts to BGRA (79,58,36,191). A non-default
`blendMode` is parsed and, when it is not 0, emits one bounded diagnostic
per scene; only src-over is rendered until M3d.

Image sources resolve root-relative (symlink-escape-validated) from the
content root, or by path from the package entry table. Decoding is bounded
(`kwe-scene-renderer/src/textures.rs`, the `image` crate with png/jpeg/webp
features only): dimension ≤ 8192, pixels ≤ 16_777_216, source bytes and
decoded allocation ≤ 64 MiB each, ≤ 256 MiB total across all layers;
RGBA8 → R8G8B8A8_UNORM is an identity mapping. A missing, unreadable, or
undecodable image is **not fatal**: the layer is skipped with a bounded
diagnostic (`event=renderer.scene.layer_skip layer=...`) and the rest of
the scene renders. More than 256 image layers is a backend rejection:
`scene.json "objects" has N image layers, over the 256 layer cap` → exit 73
before the canary.

`Scene.getLayer(name | index)` returns a Layer proxy or null for an
unknown layer; `Scene.getLayerCount()` returns the image-layer count. The
proxy exposes `name` (read-only), `alpha`, `visible`, `angles{x,y,z}`,
`origin`, `scale{x,y}`, `size{x,y}` — all read/write, with the same clamp
rules as `Engine.clearcolor`: non-finite → 0, alpha 0..=1, magnitude
≤ 1e6, size ≥ 0, `visible = value != 0.0`. Proxies are rebuilt every
`update()`, so scripted motion reapplies per frame while the image source
itself stays load-time-only (planned slice).

Corpus findings (60 real packages, 685 image-bearing objects): 432 carry a
`colorBlendMode` — 410×0, 30×11, 6×30, 4×6, 2×24, 1×1, 1×7, 1×9, 1×12 —
all rendered as src-over until M3d. **All 685 image references point at
model `.json` files** (WE's animated-model format, M3h), so no corpus
wallpaper yet exercises the decoded-texture path. 46% of `alpha` and 43%
of `visible` fields are property-wrapped; the M3c parser unwraps the
`{"user": ..., "value": ...}` wrapper at load time for image-layer fields
(user properties themselves are M3j), while the property-wrapped
*clearcolor* form (1 of 60) remains rejected per the M3b finding.

## Run the suites

```sh
scripts/smoke-scene.sh       # M3a..M3c: scene renderer through the daemon,
                             #   scripted-color oracle, containment, the
                             #   scene.pkg lanes, the M3c image-layer cases
                             #   (a)-(f), plus a standalone llvmpipe lane
scripts/smoke-corpus-pkg.sh  # M3b evidence: preflight over real Workshop
                             #   scene packages (KWE_CORPUS_DIR); SKIPPED
                             #   with exit 0 when unset/missing
scripts/smoke-video.sh       # unchanged (M1 regression lane)
scripts/smoke-supervisor.sh  # unchanged (M1a regression lane)
```

`smoke-scene.sh` builds the workspace, uses a private temporary
socket/runtime/state tree, generates the scene.json + script.js fixtures
and the M3c solid-PNG images at runtime (never committed), and removes
everything on exit. It does not install a wallpaper or touch the running
Plasma session; a `pgrep -x plasmashell` pid guard asserts the suite never
touches an existing plasmashell.

## Acceptance evidence

Validated on 2026-08-19 (CachyOS, NVIDIA GeForce RTX 3070 discrete GPU for
the daemon lane, llvmpipe software rasterizer for the standalone lane).

### M3a — scene engine core (this commit)

| Case | Expected containment | Result |
|---|---|---|
| workspace gates | `cargo fmt --all -- --check`; `cargo clippy --workspace --all-targets -- -D warnings`; `cargo test --workspace --all-targets` | clean; 290 tests pass (84 in `kwe-core` — 22 pkg reader + 3 pkg preflight — and 43 in `kwe-scene-renderer`, incl. the clearcolor string-form tests) |
| daemon lane: live start | `kind:"scene"`, content hash, advancing sequence, failures 0, frame file present | reached `live` on the discrete GPU (B8G8R8A8_UNORM) |
| frame oracle (daemon lane) | scripted clear color in the shared frame: init()-pinned g/b, sawtooth r from `update()` | two center-pixel samples 1.5 s apart: R 137 → 74 (delta 63 ≥ 32), G/B pinned by init |
| throwing script | contained: renderer stays live, sequence advances, diagnostics bounded | `event=renderer.scene.script_error` in the ring, 1–2 lines (30 s re-report window), failures 0 |
| kill -9 | one failure (`process_exit`, `signal_9`), auto-restart, promotion clears the record | restarted live, new pid, failures 0 |
| three kills | quarantined; `renderer.start` refused for the identity | failures 3, phase `quarantined`, refused |
| garbage scene.json | passes static preflight, worker rejects before the canary | exit 73 → `rolled_back`, `exit_code_73` in the detail, base worker stays live |
| missing script file | same backend rejection | exit 73 → `rolled_back`, `exit_code_73` |
| real QuickJS heap-cap OOM | script allocates past 64 MiB in init() (one oversized allocation, rejected at the allocation check) | exit 71 → `rolled_back` with `resource_limit` / `memory_allocation_denied` — the daemon's unconditional exit-71 mapping, not the test-fault path |
| load-phase busy loop | `function init(){while(true){}}` must not hang the worker | unit `busy_loop_init_is_contained_by_load_budget`: abort within 2 s, `script_timeout kind=hard`, script disabled, static color published; standalone-verified |
| plasmashell pid guard | no plasmashell touched | pid unchanged across the suite |
| final stop | graceful stop, health ok | phase `stopped`, pid null |
| standalone llvmpipe lane | worker directly under `VK_ICD_FILENAMES` + `--device llvmpipe` | scripted-color oracle passes (R 5 → 197), SIGTERM exit 0, `Stopping` state (3) in the header, `event=renderer.complete frames=... script_errors=0 soft_timeouts=0 hard_timeouts=0` |
| device diagnostics | bounded stderr lines | `event=renderer.scene.device name=... kind=... format=...` on both drivers |

### M3b — scene.pkg reader, packaged-scene renderer, corpus tolerance (this commit)

| Case | Expected containment | Result |
|---|---|---|
| corpus preflight (reproducible) | `KWE_CORPUS_DIR=<steam workshop 431960 dir> scripts/smoke-corpus-pkg.sh` runs preflight over every scene.pkg and prints version/count/safe stats; SKIPPED (exit 0) when the env var is unset/missing | **60/60 safe** (`format: "scene-package"`): 20 distinct PKGV versions (0001, 0002, 0004, 0005, 0007, 0009, 0011–0024), 3128 entries, sizes 0.3–172 MiB, zero standalone scene.json in the corpus — every wallpaper is a package; none exceeds the 512 MiB cap (larger real packages are refused at preflight) |
| pkg reader unit tests | round-trip, truncation at every boundary, bad magic, unsupported version, count overflow (65537 rejected / 65536 accepted), oversized entry (64 MiB cap), path traversal (`../evil`, absolute, NUL, backslash rejected), decompression bomb (100 MiB zeros stopped at the cap mid-stream), symlinked package refused, raw-fallback round-trip, `read_entry_raw` refuses compressed | 26 tests in `kwe-core::pkg` (incl. `scene_json_entry` heuristic) + 4 pkg branches in `preflight`, all green |
| daemon lane: packaged e2e | `scene.pkg` with a `scene.json` (string-form clearcolor — the corpus shape) + `script.js` entry | reached `live`, `event=renderer.scene.pkg entries=2 script_entry=true` in the ring, extracted script drives the oracle: R 146 → 78 (delta 68 ≥ 32), init-pinned g/b |
| pkg with LZ4-frame script entry | a script entry re-encoded as an LZ4 frame (`lz4 -z -q -c`) decompresses and runs; absent CLI prints `SKIPPED (lz4 CLI not found)` | passed (optional `lz4` CLI case; the frame detector is the corpus-honest defensive path — real packages are raw) |
| raw fallback | a payload with the LZ4 frame magic that does not decode is treated as raw (one bounded diagnostic), never a read failure | unit `invalid_lz4_frame_falls_back_to_raw`: read succeeds and returns the stored bytes; over-cap bomb stays a `bounds` error |
| corrupt magic / truncated table / traversal pkg | static preflight rejects before any worker spawns | daemon RPC `ok:false`, `.result.error: "invalid_params"`, detail `scene preflight rejected ... scene package is invalid` (G12 closed — the path is validated, not just trusted) |
| nested `scene.pkg` | no `scene.json` entry; refusal is a backend rejection | worker exits 73 → `rolled_back`, `last_failure_detail` carries `exit_code_73` and "nested scene.pkg inside the package is not supported"; base worker stays live |
| preflight/worker cap parity | oversized scene.json (16 MiB) or script (2 MiB) entry caught statically, same resolution rules as the worker | unit `rejects_oversized_pkg_entries_at_preflight` + smoke case 3f: `invalid_params` with "scene.json entry ... over the 16777216 byte cap" / "script entry ... over the 2097152 byte cap", no worker bounce |
| extracted-script lifecycle | worker removes its own `kwe-scene-script-<pid>` dir on graceful exit; a stale dir (pid recycled after a daemon restart) is replaced, never a brick; a stale symlink dir is refused | unit `cleanup_script_dir_removes_only_worker_dirs` (foreign dirs untouched), `extract_script_replaces_stale_pid_dir`, `extract_script_refuses_stale_symlink_dir`; `event=renderer.scene.script_dir_cleanup` on the graceful path |
| clearcolor corpus form | real wallpapers serialize clearcolor as `"r g b"`, not an array | **59 of 60** corpus scenes are string-form (58 exact three-token, one five-digit precision, e.g. `"0.7 0.7 0.7"`), 0 arrays; the parser now accepts both forms (string → alpha 1.0), the property-wrapped object form (1 of 60) stays rejected until user properties (M3c+); two new unit tests |
| corpus render probe | a real pkg (3765081478, PKGV0024, 264 entries, 142 MiB — measured 147 939 386 bytes) driven through the renderer | preflight passes; parse reaches clearcolor (previously exited 73 on the array-only shape — the probe that surfaced the string-form finding) |
| workspace gates | fmt, clippy `-D warnings`, full test suite | clean, 296 tests pass (87 `kwe-core`, 46 `kwe-scene-renderer`) |
| regressions | video + supervisor suites | `smoke-video.sh` exit 0 (oracle deviation 2 ≤ 4), `smoke-supervisor.sh` exit 0 |
| plasmashell pid guard | no plasmashell touched | pid unchanged across the suite |

### M3c — image layers and Scene.getLayer (this commit)

| Case | Expected containment | Result |
|---|---|---|
| workspace gates | `cargo fmt --all -- --check`; `cargo clippy --workspace --all-targets -- -D warnings`; `cargo test --workspace --all-targets` | clean; 335 tests pass, 81 in `kwe-scene-renderer` (6 layer-math + 5 texture-bounds + 37 scene-parse + 19 Layer-proxy + 14 earlier) |
| device tests (both drivers) | `KWE_TEST_DEVICE`-gated renders: fullscreen red 1×1 (`isolated_draw`), 2×2 four-color orientation quad (`quad_orientation`), straight-alpha blend (`blend_partial_alpha`) | byte-exact on llvmpipe and NVIDIA RTX 3070: every pixel [0,0,255,255]; at(8,8)/(56,8)/(8,40)/(56,40) = blue/green/red/white; every pixel (79,58,36,191) |
| smoke (a): two layers, scene order | red 64×64 then blue 32×32 centered; draw order = scene.json order | daemon lane: (90,55)=(0,0,255,255) red, (140,79)=(255,0,0,255) blue, (150,85)=(255,0,0,255) blue — byte-exact, tol 0 |
| smoke (b): src-over blend | blue 64×64 at alpha 191/255 over the red layer | (80,45)=(79,58,36,191) tol 1 — the ONE-factor oracle |
| smoke (c): reversed order | same scene with the objects array reversed | (140,79)=(0,0,255,255) — blue now on top, array order wins |
| smoke (d): missing image | `image: "broken.png"` that does not exist | renderer stays `live`, sequence advances, stderr ring carries `event=renderer.scene.layer_skip layer=broken`; remaining layers render |
| smoke (e): 257 layers | 257 image objects | `rolled_back`, `last_failure_detail` carries `exit_code_73` and `over the 256 layer cap`; pid unchanged from the previous worker — no bounce |
| smoke (f): scripted move | script sets `mark.origin.x=60, origin.y=34, size 40×22` on the blue layer every frame | (140,79)=(255,0,0,255) blue (moved onto the red region), (158,85)=(255,0,0,255), (90,55)=(0,0,255,255) red — the proxy read/write + clamp round-trip |
| layer cap / clamp units | 256 accepted, 257 rejected; clamps: non-finite→0, alpha 0..=1, \|v\| ≤ 1e6, size ≥ 0, `visible` boolean coercion | `exactly_256_image_layers_accepted`, `rejects_257th`; clamp tests in layers.rs + js.rs |
| corpus stats | image-bearing objects, blendMode histogram, property-wrapping rates | 685 objects; 432 with `colorBlendMode` (410×0, 30×11, 6×30, 4×6, 2×24, 1×1, 1×7, 1×9, 1×12); **all 685 reference model `.json` files (M3h)**; 46% alpha / 43% visible property-wrapped |
| regressions | video + supervisor suites | `smoke-video.sh` exit 0 (deviation 2 ≤ 4), `smoke-supervisor.sh` exit 0 |
| plasmashell pid guard | no plasmashell touched | pid unchanged across the suite |

## Renderer exit codes

| Code | Meaning | Supervisor mapping |
|---|---|---|
| 0 | graceful stop (SIGTERM) | normal stop |
| 70 | `--exit-after` synthetic fault | `process_exit` failure |
| 71 | memory denied (QuickJS heap cap hit, or `--memory-pressure-after`) | `resource_limit` failure (`memory_allocation_denied`), mapped unconditionally — any worker exiting 71 declares a resource limit, test fault or not |
| 72 | memory-pressure allocation unexpectedly succeeded | `process_exit` failure |
| 73 | backend rejection: scene parse (bad JSON, wrong shape — incl. the property-wrapped clearcolor form until M3c+, script non-string or over caps, **more than 256 image layers — `over the 256 layer cap`**), pkg shape (no `scene.json` entry, several `scene.json` entries, **nested `scene.pkg`**), missing/unreadable script, Vulkan device/compositor unusable, sustained render failure streak | `exit_code_73` in `last_failure_detail` |
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
   (bounded, fatal); the supervisor maps any worker exit 71 to
   `resource_limit` unconditionally, so the scene worker's heap-cap hit and
   the test renderer's `--memory-pressure-after` fault land in the same
   failure class (both lanes exercised by the smoke suites). The unit
   tests keep the exit-71 decision pure.
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
9. **LZ4 is defensive, not corpus-proven.** No compression flag exists in
   the format and all 3128 corpus payloads are raw; the LZ4 frame path
   (magic `04 22 4D 18` at payload start) exists to tolerate hypothetical
   compressed packages and is exercised only by unit tests and the smoke
   suite's optional `lz4` CLI case. If a future corpus discovery shows
   real compressed payloads, the frame detector is the hook — bounded
   decode is already enforced mid-stream.
10. **Property-wrapped clearcolor (1 of 60 corpus scenes) is refused.**
    One real wallpaper (id 2120316749) serializes clearcolor as
    `{"user": ..., "value": ...}`; the parser rejects it with a Shape
    error (exit 73) until user-property support lands in M3c+, when the
    wrapper can be unwrapped. The 59/60 string-form scenes — the
    dominant shape — are fully accepted, so the refusal is a known,
    documented minority.
