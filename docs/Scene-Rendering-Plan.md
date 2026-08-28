# Scene-Rendering-Plan

**Status:** Approved by the maintainer 2026-08-27 as written (decision gates
§11.1–.7 accepted with the plan's recommendations; per-slice dependency and
provenance gates in §11.4–.5 still require their named per-slice approvals
before dependent code merges). Work order: SR-0 first; S7d visual expansion
stays paused per gate 7. SR-0 decomposition: `docs/SR0.md` — **SR-0 complete
2026-08-28** (inspector + object-family inventory + corpus baseline: 60/60
local scenes inventoried, unknown-key data confirms the root `camera` gap in
all 60). **SR-1 complete 2026-08-28** (`docs/SR1.md`): taxonomy frozen v1
(§11.3, maintainer go), report protocol v1 + report FD, scene apply gate
(refuse-before-replace + apply-with-limitations), version-skew matrix,
manager result states. Open risks carried: no inspection cache; limitations
not persisted; ~~playlist lane ungated~~ (resolved 2026-08-28, SR-1c2:
`docs/SR1.md`); render-report kind 2 reserved.
**SR-2a–c complete 2026-08-28** (`docs/SR2.md`): confined VFS (no callers
yet), typed SceneIr + unknown bags, scene.json loading through the IR with
the legacy parser kept as a compiled differential oracle — 60/60 corpus
parse parity; corpus frame sweep clean modulo documented wall-clock sim
jitter (SR-2c2 STOP report). **SR-3a–c complete 2026-08-28**
(`docs/SR3.md`): helper protocol (kinds 16/17/18), `kwe-shader-compiler`
binary, renderer wiring with in-thread fallback, material-shader family
compiling in the killable helper with byte-identical SPIR-V. SR-1 risks
further closed by SR-1c3 (limitations persisted on assignments).
Shipped for testing as pkgrel 21 (built 2026-08-28 ~05:00).
Next: SR-3d reflection spike (blocked on the §11.4 dependency decision),
remaining SR-2 family migrations, SR-1 inspection cache.

**Baseline:** repository HEAD `93bae3b284d6221a7dd3b328eb12569edfa12524`, 2026-08-23.

**Scope:** complete scene-wallpaper rendering program: 2D, 3D, shaders,
effects, particles, text, video/sound layers, puppet/animation, SceneScript,
properties, input/audio/media integration, compatibility evidence, recovery,
and production hardening.

**Required invariant:** no wallpaper parser, renderer, shader compiler, browser,
Steam SDK, video/audio decoder, or audio processor may execute in
`plasmashell`.

## 1. Executive decision

Keep the scene backend as an independently supervised **Vulkan renderer**.
Do not add an OpenGL backend now and do not replace the renderer with an
in-process upstream plugin.

Vulkan remains the best fit because the project already has an offscreen
`ash` renderer, deterministic llvmpipe tests, explicit synchronization, and a
future path to DMA-BUF without changing the Plasma safety boundary. Open
Wallpaper Engine informs the render-graph design, but is not evidence that this
project can attain equivalent coverage, performance, or safety. Adding OpenGL
would double the shader, synchronization, recovery, and parity-test surface
without addressing the current architectural bottlenecks.

However, the current renderer must be treated as an **early experimental 2D
baseline**, not as a nearly complete renderer. Recent S1-S7 work added useful
format and draw paths, but acceptance of a wallpaper or compilation of some
material layers does not prove visual compatibility. Flat model quads are not
3D support; silent shader defaults are not shader parity; and a non-blank frame
is not a fidelity oracle.

Before adding more isolated fixes, build a renderer-v2 foundation with:

1. a bounded, typed scene intermediate representation (IR) that preserves
   unknown data;
2. a capability inventory tied to every parsed object, pass, and script API;
3. shader compilation in a killable helper process, with reflection and a
   versioned cache;
4. an explicit render graph with resource lifetimes, regions, feedback, and
   barriers;
5. structured inspection and render reports that the daemon and manager can
   explain;
6. feature-specific synthetic fixtures and image/event oracles;
7. private corpus measurements reported as coverage data, never as blanket
   compatibility claims.

The CPU-readback `FRAME_PROTOCOL_V1` path remains the correctness baseline.
Zero-copy transport and HDR/color metadata come only after renderer fidelity
and recovery are stable.

## 2. What was reviewed

### Repository evidence

- Required project documents: `PROJECT_PLAN.md`, `ARCHITECTURE.md`,
  `UX_DESIGN.md`, `FEATURE_COMPATIBILITY.md`, `PROVENANCE.md`, the protocol
  documents, ADR 0001/0003/0004, `BETA_PLAN.md`, `BETA_M3.md`, and
  `SCENE_FORMAT_V1.md`.
- Current implementation and tests in `kwe-core`, `kwe-scene-renderer`,
  `kwe-daemon`, frame/input protocols, the Plasma display bridge, and
  `scripts/smoke-scene.sh`.
- Local Git history through S1-S7/S7c. No Git remote is configured in this
  checkout, so project-change review used the complete local commit history.
- The existing Graphify map was queried for scene parsing, scripting,
  compositor ownership, and recovery paths. It confirmed that `SceneWorker`
  owns a large amount of orchestration and `LayerRenderer` is a high-degree
  dependency hub.

### External sources

Wallpaper Engine's official documentation is authoritative for user-facing
semantics, but it does not document the private package and converted-asset
formats. Linux projects are reverse-engineering evidence, not official
specifications.

| Source | Reference/snapshot | Allowed use |
|---|---|---|
| [Wallpaper Engine SceneScript reference](https://docs.wallpaperengine.io/en/scene/scenescript/reference.html), [IEngine](https://docs.wallpaperengine.io/en/scene/scenescript/reference/class/IEngine.html), [3D models](https://docs.wallpaperengine.io/en/scene/models/introduction.html), [shaders](https://docs.wallpaperengine.io/en/scene/shader/overview.html), [particles](https://docs.wallpaperengine.io/en/scene/particles/introduction.html), and [user properties](https://docs.wallpaperengine.io/en/scene/userproperties/overview.html) | accessed 2026-08-23; revalidate for each implementation slice | Semantic authority for author-visible behavior; no private format claim |
| [Almamu/linux-wallpaperengine](https://github.com/Almamu/linux-wallpaperengine/tree/b016d7d1fdcf4e5fd2f9c9fa420a8aaa07fee02d) | `b016d7d1fdcf4e5fd2f9c9fa420a8aaa07fee02d`, GPL-3.0-or-later | Idea, behavior, protocol-compatible, or reviewed adaptation/copy with file-level provenance |
| [waywallen/open-wallpaper-engine](https://github.com/waywallen/open-wallpaper-engine/tree/aa01954fe13d78c2aab8232396631b834605b068) | `aa01954fe13d78c2aab8232396631b834605b068`, GPL-2.0-only | Idea and failure-mode reference only; no code adaptation into this GPL-3 project without separate permission |
| [waywallen](https://github.com/waywallen/waywallen/tree/7d99f7f15f560cc0b20acd0003d6031b6df22c45) | `7d99f7f15f560cc0b20acd0003d6031b6df22c45`, MIT | Manager/daemon/plugin lifecycle and UX ideas; adaptation only after ledger review |
| [waywallen-display](https://github.com/waywallen/waywallen-display/tree/15be2a96c73a56ad6dd00cbe9d36aae29d35ab6d) | `15be2a96c73a56ad6dd00cbe9d36aae29d35ab6d`, MIT | External display/zero-copy synchronization idea; do not adopt its ABI casually |
| [jagrat7/linux-wallpaper-engine](https://github.com/jagrat7/linux-wallpaper-engine/tree/1694475868521830259963f54f8717f7042c24c3) | `1694475868521830259963f54f8717f7042c24c3`, MIT | Gallery, compatibility, property, and workflow ideas; not a renderer backend |

`open-wallpaper-engine` is particularly valuable as architectural evidence:
it separates package/scene types, runtime systems, a dependency graph,
resource planning, Vulkan passes, scripting, particles, text, and properties.
Its GPL-2.0-only implementation cannot be copied into this project, and its
C++20-module/custom-build dependency stack is not proposed as a runtime
dependency.

## 3. Current baseline: useful but narrow

### Process and data flow

```text
manager / CLI
    │  daemon RPC v1
    ▼
kwe-daemon ── static preflight, policy, canary, rollback, quarantine
    │  supervised argv + bounded stdin/stdout/stderr + private HOME + rlimits
    ▼
kwe-scene-renderer
    ├─ package / JSON / model / material / effect parsing
    ├─ QuickJS state and CPU particle simulation
    ├─ libmpv video layers
    ├─ shader preprocessing + shaderc
    └─ offscreen Vulkan composition + CPU readback
            │ FRAME_PROTOCOL_V1, premultiplied BGRA8888
            ▼
thin Plasma display bridge ── validated frame copy + normalized pointer relay
```

This boundary is correct and must remain. The problem is internal coupling:
`main.rs` performs loading, asset resolution, effect planning, media staging,
shader/material setup, and worker orchestration; `vulkan.rs` owns device setup,
allocations, uploads, fixed pipelines, material pipelines, effect targets,
effect actions, draw ordering, readback, and teardown. Both are several
thousand lines and make correctness fixes increasingly cross-cutting.

### Implemented experimental paths

- bounded `scene.json` and `scene.pkg` reading;
- PNG/JPEG/WebP and a subset of TEXV texture formats;
- image, text, video, and partial particle objects;
- object order, 2D transforms, five fixed-function blend modes, tint and
  brightness;
- model→material→texture resolution, but model geometry is still a shared
  flat quad;
- a compatibility shader preprocessor, runtime GLSL→SPIR-V compilation, up to
  eight texture slots, and a fixed uniform block;
- bounded effect targets/pass chains and partial `_rt_FullFrameBuffer`
  behavior;
- a limited QuickJS bridge for update timing and some layer/text/particle
  properties;
- supervised failure, last-good frame retention, rollback, refusal, and
  quarantine.

### Gaps that block broad compatibility

1. No true mesh, puppet, skinning, skeletal animation, 3D camera, depth,
   lighting, shadow, fog, volumetric, or PBR path.
2. The shader preprocessor scrapes declarations and substitutes many unknown
   uniforms with zero/identity values. This can produce visually wrong frames
   that look like successful rendering.
3. Shader timeouts detach the compiler thread; a pathological compiler call is
   not actually canceled and can retain CPU/memory until the renderer exits.
4. Effect execution remains a specialized linear action list. S7d still has
   known failures for ping-pong base seeding, waterwaves/foliagesway,
   sub-region passthrough copies, and a flat-gray scene.
5. Internal effect-pass `_rt_FullFrameBuffer` consumers do not yet have full
   same-frame object-order semantics.
6. Particles implement only a subset and intentionally approximate timing,
   randomness, turbulence, control points, transforms, and renderer types.
7. SceneScript lacks the official object/lifecycle model, per-object scripts,
   dynamic assets/layers/geometry, timers, storage, input/audio/media,
   properties, and most classes/events.
8. Text has no shaping, bidi/RTL, complex-script, color-font, or proven real
   corpus layout compatibility.
9. Preflight is mostly structural and can disagree with actual asset decode,
   shader compile, GPU features, or hidden render results.
10. The probe reports only `vulkan+quickjs` and basic device facts. There is no
    versioned feature/backend capability manifest.
11. Diagnostics are mostly bounded stderr strings, not a structured report the
    manager can reliably explain.
12. Existing 60-item private corpus measurements prove admission and selected
    pixel changes, not visual parity.

## 4. Target architecture

```text
                           ┌─────────────────────────────┐
manager / CLI ◄────────────┤ structured compatibility UI │
                           └──────────────┬──────────────┘
                                          │ RPC v1 + additive scene reports
                           ┌──────────────▼──────────────┐
                           │ kwe-daemon                  │
                           │ policy / assignment /       │
                           │ supervision / rollback only │
                           └───────┬───────────┬─────────┘
                                   │           │
                        inspect RPC│           │renderer contract
                                   ▼           ▼
                     ┌────────────────┐  ┌─────────────────────────┐
                     │ isolated scene  │  │ isolated scene worker   │
                     │ inspector       │  │                         │
                     │ bounded parse + │  │ asset VFS → typed IR    │
                     │ capability plan │  │                         │
                     └────────────────┘  │        ↓                │
                                        │ runtime systems          │
                                        │ script/props/input/audio │
                                        │        ↓                │
                          killable IPC ┌─┴─────────────┐          │
                                      │ shader helper │          │
                                      └─┬─────────────┘          │
                                        │ reflected SPIR-V         │
                                        ▼                         │
                                  render graph + resource plan     │
                                        ↓                         │
                                  Vulkan backend + readback        │
                                        │                         │
                                  structured render report         │
                                        └────────────┬────────────┘
                                                     │ frame v1
                                                     ▼
                                            thin Plasma bridge
```

The inspector is a separately killable `kwe-scene-inspector` worker, launched
and supervised by the daemon with the renderer's containment or stricter:
private disposable HOME/runtime directory, process-group kill and reap,
parent-death behavior, closed ambient file descriptors, rlimits, bounded IPC,
no network, no input/audio/media capture, and no persistent script/property/
storage effects. Inspection and hidden rendering use disposable state and are
not the authoritative Apply canary. The daemon validates and reduces their
reports; the manager and CLI never parse hostile wallpaper content.

### 4.1 Asset VFS and typed scene IR

- Resolve all package, scene-directory, Wallpaper Engine asset-root, and
  explicitly granted property assets through one confined VFS interface.
- Use normalized logical asset IDs, not ambient filesystem paths, after load.
- Enforce source bytes, decompressed bytes, entry count, path depth, file count,
  texture dimensions/mips/frames, model vertices/indices/bones, shader text,
  and aggregate CPU/GPU budgets at this boundary.
- Parse into versioned typed IR nodes: scene, object hierarchy, image/text/
  video/sound/particle/model/light/camera, material, pass, effect, render
  target, animation, script binding, property binding, and unknown node.
- Preserve raw unknown keys/values and stable object IDs. Unknown fields are
  inventory data, not parser failures and not silently discarded.
- Full parsing stays in the scene inspector/worker process. The daemon may use
  only a small bounded summary schema.

### 4.2 Runtime systems

- Keep authored state separate from mutable runtime state.
- Use explicit phases: load → bind → init scripts → apply initial properties →
  fixed-step simulation → before-render synchronization → graph execution →
  publish → teardown.
- Schedule timers, SceneScript updates, timeline animation, particles, video,
  audio response, media events, and property updates through bounded queues.
- Define one authoritative coordinate model for scene, object-local, camera,
  screen, and pointer spaces. Convert only at named boundaries.

### 4.3 Shader compiler helper

- Move wallpaper-provided preprocessing/compilation out of the renderer thread
  into `kwe-shader-compiler`, a child in the renderer's process group with
  parent-death signal, cleared environment, no filesystem access except
  provided bytes, and stricter memory/CPU/file limits.
- Use one serial request at a time, bounded source/include/output sizes, a total
  per-scene compilation budget, and a hard process kill on timeout.
- Return SPIR-V plus reflected descriptors, vertex inputs, uniforms, and
  required feature flags. Never infer a shader interface solely from textual
  declaration order.
- Cache success and bounded failure by source/include hashes, combos/constants,
  compiler/preprocessor ABI, renderer build, target Vulkan version, GPU/driver
  feature class, and asset-root version.
- Cache writes are atomic, size-bounded, private, and LRU-pruned. A crash,
  timeout, or validation failure is never cached as a permanent global result.

### 4.4 Render graph and resource planner

- Compile IR into explicit passes with read/write resources, subresource
  regions, formats, clear/load/store semantics, alpha convention, and object
  order.
- Distinguish same-frame edges from declared previous-frame feedback. Reject
  accidental cycles and uninitialized reads before Vulkan commands are built.
- Represent base material, effect passes, named FBOs, ping-pong targets,
  `_rt_FullFrameBuffer`, copy/swap commands, bloom/postprocess, shadows, and
  final composition in the same graph.
- Compute resource lifetimes and aggregate memory before allocation. Resource
  aliasing is a later optimization and must never precede correctness.
- Emit a bounded graph summary and Graphviz text artifact only on an explicit
  daemon-requested local diagnostic operation. Write through a daemon-created
  private file descriptor; cap nodes, edges, and bytes; use logical IDs/reason
  codes rather than paths or titles; never include copyrighted asset bytes or
  execute an external `dot` process.

### 4.5 Vulkan backend

- Vulkan 1.2 remains the primary contract; llvmpipe is the required software
  correctness lane. Record actual device, driver, supported formats/features,
  and compiler target in every report.
- Wrap raw `ash` handles into narrow RAII resource types. Device/queue/resource
  ownership and drop order must be explicit and unit-testable.
- Track CPU staging, device-local images/buffers, descriptors, pipelines, and
  render targets against a per-worker budget before every allocation.
- Any fence timeout or device loss exits the worker through a typed failure;
  the daemon retains the last-good frame and owns retry/quarantine policy.
- Do not attempt unlimited GPU retries. A single software fallback attempt may
  be offered only after explicit policy/UI design and must use a new backend
  identity for quarantine and cache keys.

## 5. Compatibility and user-visible contract

### 5.1 Capability IDs

Keep existing public IDs and add scene sub-capabilities so the manager can say
what is missing instead of reporting only `content.scene2d=partial`.

| Existing IDs affected | Proposed scene sub-capabilities |
|---|---|
| `content.scene2d`, `content.scene3d` | `scene.package`, `scene.asset-vfs`, `scene.texture.*`, `scene.layer.image`, `scene.layer.text`, `scene.layer.video`, `scene.layer.sound` |
| `runtime.scenescript` | `scene.script.lifecycle`, `.objects`, `.dynamic-assets`, `.dynamic-geometry`, `.timers`, `.storage`, `.modules` |
| `runtime.pointer-position`, `runtime.pointer-buttons` | `scene.input.hit-test`, `.cursor-events`, `.control-points` |
| `runtime.audio-scene-16-32-64`, `runtime.media-*` | `scene.audio-buffers`, `.audio-response`, `.media-events`, `.album-art` |
| `property.*`, `property.live-update` | `scene.property.binding`, `.texture`, `.directory`, `.unknown-preservation` |
| `runtime.screen`, `runtime.time`, `runtime.fps`, `runtime.pause` | `scene.resize`, `.timeline`, `.simulation-pause` |
| `content.scene2d`, `content.scene3d` | `scene.material`, `.shader`, `.effects`, `.render-target`, `.blend`, `.particle`, `.puppet`, `.model3d`, `.camera`, `.lighting`, `.shadow`, `.postprocess`, `.physics` |

Exact names and schema version are approved in SR-1 before code uses them.

### 5.2 Result states

Every inspection and Apply result must use text plus an icon; color is
supplementary only.

- **Compatible:** every required capability has completed the six-step parity
  ladder on this backend class.
- **Expected to work:** all required capabilities are implemented, but this
  exact content/backend combination has not been rendered before.
- **Partial:** known optional/degraded paths exist; list each affected object
  and capability before Apply.
- **Incompatible:** an active required object/pass/API cannot be represented.
- **Backend unavailable:** Vulkan/compiler/asset root is unavailable.
- **Failed and rolled back:** hidden render or live canary failed; previous
  wallpaper remains.
- **Quarantined:** repeated content/backend-specific runtime failure; include
  reason and a deliberate Retry action.
- **Unknown:** inspection was canceled, timed out, or exceeded a safe bound.

Silent flat-quad, zero-uniform, dropped-pass, missing-particle, or missing-layer
fallbacks must not be described as Compatible. For a partial scene, default to
keeping the current wallpaper and offer **Apply with limitations** only after
the limitations are visible.

Apply policy is deterministic and non-bypassable:

- security, resource-bound, containment, parse-integrity, protocol, and active
  dependency failures always block Apply;
- an active visual object, graph dependency, required script API, or required
  property binding blocks Apply unless a synthetic fixture proves a meaningful
  bounded fallback and the capability rule explicitly permits it;
- an optional enhancement may allow Apply with limitations only when omitting
  it cannot blank the scene, break object ordering/dependencies, invalidate
  interaction or properties, or misrepresent a 2D/3D compatibility claim;
- the full inspector-side decision records the rule ID plus object/pass/API
  evidence. The manager may present that decision but cannot weaken it.

### 5.3 Structured reports

Define versioned `scene-inspection-v1` and `scene-render-report-v1` JSON schemas,
each capped at 64 KiB and containing:

- content hash, renderer build/ABI, asset-root identity, GPU/driver, compiler;
- detected and exercised capability IDs;
- object/pass/script counts and enforced bounds;
- compile/fallback/skip/refusal counts with stable reason codes;
- first-frame and steady-frame health, frame time, peak tracked CPU/GPU bytes;
- phase and typed failure/recovery action;
- bounded/redacted diagnostics, never asset bytes, arbitrary paths, script
  source, or raw audio.

Reports travel over a dedicated inherited report socket/file descriptor with a
versioned length-delimited envelope, not worker stdout or stderr. Stdout keeps
its v1 input-acknowledgement contract unchanged. The daemon owns both ends,
closes them on generation change, caps message count and bytes, validates
ordering/content/version, and treats malformed, missing, duplicate, or late
reports as typed failures. Old workers report `report=unavailable`; no policy
decision is reconstructed from stderr. SR-1 must test old/new daemon, worker,
and display-bridge upgrade/downgrade and canary rollback combinations.

Report projection is deterministic: collect counts and the Apply decision
first; sort samples by stable logical object/pass/API ID; retain the first N;
set `truncated=true`; include omitted counts and reason histograms; and bind the
projection to content/build/backend with a digest. A truncated UI projection
never changes the complete inspector-side Apply decision.

## 6. Dependency review gate

No new dependency is approved by this plan. Each proposed addition requires a
maintainer decision, exact version/license/transitive review, packaging impact,
`THIRD_PARTY.yml`, and an ADR or task note before dependent code merges.

| Dependency | Purpose | Recommendation |
|---|---|---|
| `ash` 0.38 | Vulkan FFI | Retain; required in worker only |
| `rquickjs` 0.12.2 and its currently pinned QuickJS engine | SceneScript | Retain provisionally; add official API conformance and stricter host/timer quotas |
| `shaderc` + system libshaderc | Current GLSL→SPIR-V | Retain initially, but isolate in killable helper; do not expand in-thread use |
| SPIR-V reflection library, exact project TBD | Descriptor/input/uniform reflection | Proposed SR-3 spike; prefer Apache-2.0/MIT, small audited surface |
| SPIRV-Tools / `spirv-val`, exact packaging TBD | Test/load validation and diagnostics | Proposed as build/test or helper-only dependency; measure startup cost |
| `glam`, exact version TBD | Matrices, quaternions, transforms, camera/skinning math | Proposed for SR-2/SR-13; MIT OR Apache-2.0 review required |
| system FreeType + HarfBuzz + Fontconfig, or a reviewed pure-Rust shaping stack | Correct shaping, bidi/complex scripts, font selection | SR-6 decision spike; prefer the smallest maintained stack that passes fixtures |
| `image`, `texture2ddecoder`, `lz4_flex` | Existing bounded decode paths | Retain; expand formats only with bounds and fixtures |
| libmpv / `kwe-mpv` | Video textures | Retain as optional scene feature; software correctness path first |
| `gpu-allocator` or Vulkan Memory Allocator | Suballocation | Not proposed until measurements show fragmentation/driver allocation pressure |
| `petgraph` | Render DAG | Optional spike only; a small original bounded graph may be safer and easier to audit |

Explicitly rejected for the core scene backend at this stage:

- `wgpu`/`vulkano`: no demonstrated compatibility benefit, less direct control
  over external memory/sync and exact pipeline behavior;
- Assimp or generic FBX import: Workshop payloads contain Wallpaper Engine's
  converted model/puppet formats; a large generic parser adds attack surface
  without solving the real format;
- OpenGL fallback: doubles the parity/recovery matrix;
- linking or embedding Almamu/linux-wallpaperengine: monolithic OpenGL/window/
  audio/input dependencies conflict with the isolation and packaging goals;
- copying Open Wallpaper Engine or RainyPixel GPL-2.0-only code: license
  incompatible with this GPL-3.0-or-later project without separate permission;
- CEF/Electron/Steamworks.js in the scene worker: irrelevant to native scene
  rendering and materially broadens risk.

A move from the engine used by `rquickjs` to QuickJS-ng is not implied by this
plan. It would be a separate runtime/dependency migration requiring exact ABI,
license, security, memory/interrupt, conformance, packaging, and rollback
review.

## 7. Evidence strategy

### Synthetic fixtures

- Fixtures are original and minimal; never commit a Workshop payload, official
  Wallpaper Engine runtime asset, extracted shader, texture, model, or script.
- Provide builders for package entries, TEXV variants, materials, effects,
  render graphs, particle components, puppet/model buffers, SceneScript
  modules, properties, and malformed boundary cases.
- Every parser or crash fix adds a minimized synthetic regression fixture.

### Image and event oracles

- Byte-exact: clear, channel order, premultiplication, copy, simple blend,
  fixed transforms, deterministic texture decode.
- Small tolerance: driver-dependent blend rounding and filtered sampling.
- Perceptual/structural: font rasterization, advanced effects, particles,
  lighting, animation; pin tolerance, mask, capture time, and backend facts.
- Event traces: script lifecycle, property deltas, input hit testing, timers,
  media, pause/resume, animation, and particle control points.
- Each oracle reopens and snapshots the frame mapping according to
  `FRAME_PROTOCOL_V1`; never retain a stale reader across generations.

### Private corpus harness

The local 60-scene corpus may be used only as an uncommitted compatibility
lab. Record metadata-only results:

- format/version and feature histogram;
- inspected, loadable, hidden-rendered, canary-live, and sustained-live counts;
- shader/pass/texture/object coverage and exact fallback reasons;
- nonblank status as a health signal, never a fidelity claim;
- optional locally stored reference/candidate comparisons, never committed;
- renderer/compiler/GPU/driver/asset-root versions and deterministic capture
  timing.

Do not publish Workshop titles, source assets, screenshots, or scripts without
separate permission. Workshop IDs may appear only in local diagnostic records
under the existing content policy.

### Hardware/backend matrix

Required before a capability becomes Supported:

1. llvmpipe Vulkan correctness lane;
2. the development NVIDIA RTX 3070 lane;
3. at least one Mesa hardware lane (AMD or Intel) before 1.0 scene claims;
4. Vulkan validation-layer run for synthetic fixtures;
5. device-loss/fence-timeout/shader-helper failure injection;
6. Plasma PID unchanged through every destructive worker test.

## 8. Ordered implementation program

The SR entries below are **program gates/epics, not directly assignable AI
tasks**. Before implementation, each must be decomposed using
`AI-Skills/TASK_TEMPLATE.md` into the smallest independently mergeable vertical
slice with one observable behavior. Each child contract names its exact files,
ADR/protocol versions, feature-capability IDs, acceptance and explicit failure
tests, required commands, recovery/compatibility impact, upstream revision/
path and allowed-use type, UI/accessibility states, and dependency decision.
Each child has separate mapping, implementation, adversarial review, and
integration passes in its own worktree; no two agents edit the same files.
Every merged child updates this plan's status, `AI-Skills/BETA_PLAN.md`,
`PROJECT_MEMORY.md`, compatibility evidence, and provenance as applicable.

Minimum decomposition before an epic can be approved:

| Epic | Independently reviewed child sequence |
|---|---|
| SR-0 | capability taxonomy/schema; isolated inspector containment; one loader inventory adapter; private corpus metadata runner |
| SR-1 | report envelope/FD; worker report producer; daemon validator/policy; old/new adapter matrix; one manager result-state flow |
| SR-2 | confined VFS; immutable IR core/unknown bag; old→new differential adapter; migrate one loader family per child; orchestration/backend module extraction separately |
| SR-3 | helper protocol; helper containment/reaping; one shader preprocessing family; reflection/validation; bounded cache |
| SR-4 | graph validation/resource budget core; one existing FBO path; one internal full-frame-buffer path; one feedback/ping-pong path; migrate each remaining effect family separately |
| SR-5 | one texture format/version family; animated texture timing; one blend family; one material-combo family; one effect family; hierarchy/parallax composition |
| SR-6 | dependency decision; shaping/layout; font resolution; atlas/cache; script mutation and manager diagnostics |
| SR-7 | shared clock; animated textures; one video lifecycle; sound policy; timeline family; decoder fault/recovery |
| SR-8 | component inventory/parser; fixed-step core; one emitter/initializer/operator/renderer family per child; children/events; control points; audio response |
| SR-9 | input-v2 envelope/generation rules; time/pause/resize; passive pointer relay; worker hit testing; audio bands; media metadata/artwork |
| SR-10 | API inventory generator; lifecycle/scheduler; one host-object family per child; modules; dynamic asset registration; timers; storage |
| SR-11 | property schema/unknown round-trip; one control/binding family; grant-backed asset properties; live transaction; persistence/presets |
| SR-12 | one binary format parser/version; static puppet mesh; skeleton/skinning; morph/mask; animation/attachments; physics separately |
| SR-13 | one mesh format/version; transform hierarchy; camera; depth/culling; one material family; model animation separately |
| SR-14 | one light family; one PBR family; shadow resource/pass; fog; reflection; postprocess family; volumetric and physics separately |
| SR-15 | color/alpha metadata; pacing; memory telemetry; frame-v2 negotiation; DMA-BUF export; bridge import/fallback; fault matrix |
| SR-16 | capability evidence audit; installed-package lane; provenance audit; accessibility state family; release matrix |

For every child that changes manager or Apply behavior, the task contract must
mark loading, success, empty/no-applicable-features, asset-root-unavailable or
offline, canceled, degraded/partial, actionable failure/rollback, and
quarantine as implemented, tested, or explicitly not applicable with a reason.
It also defines focus preservation, screen-reader announcement text,
keyboard-accessible Retry/Cancel/Retain actions, and the post-cancel state.

### Wave A — truthful baseline and architecture

#### SR-0 — Reproducible baseline and feature inventory

**Outcome:** A non-rendering, non-mutating-against-content `kwe scene-inspect`
baseline reports what each scene requires and what the current build can
actually exercise. It never changes wallpaper content, assignment, properties,
or presets. No renderer behavior changes.

**In scope:** capability taxonomy draft; package/scene/object/material/effect/
shader/particle/script feature inventory; metadata-only corpus harness; current
S7d regression captures; current-vs-old-plan discrepancy list.

**Out of scope:** new rendering, UI redesign, live Plasma mutation, committing
corpus data. It may create only daemon-owned, private, size/retention-bounded
report/cache files described by the approved inspector contract.

**Files/modules:** new bounded inspection module/CLI surface in `kwe-core` and
`kwe-cli`; `scripts/scene-corpus-*`; docs only. Full hostile parsing must run in
the isolated inspector described in section 4, not the daemon, manager, or CLI.

**Acceptance:** inventory is deterministic; unknown keys/types are counted;
every item receives required capability IDs; 60-item local run completes with
per-item time/byte bounds; no source bytes leave the machine; S7d cases have
reproducible local diagnostic records.

**Failure/recovery:** malformed/oversized/slow input yields Unknown or
Incompatible without daemon hang or allocation spike; cancel terminates child
and deletes partial report.

**Tests/commands:** unit property/fuzz-style boundary tests, CLI golden JSON,
`cargo test --workspace`, `./scripts/check.sh`; private corpus command reported
separately.

**Provenance:** official docs for semantic names; format classifications cite
exact Almamu files/commit if adapted.

**UX/accessibility:** schema includes text reason, icon key, object identifier,
and recovery action; no color-only state.

**Capabilities:** all proposed `scene.*` IDs begin as experimental/planned;
this slice makes no support claim.

#### SR-1 — Capability/report protocols and staged preflight

**Outcome:** Versioned `scene-inspection-v1`, `scene-render-report-v1`, and a
scene capability manifest replace stderr phrase matching and structural-only
preflight decisions.

**In scope:** schema, dedicated inherited report FD/envelope, deterministic
64 KiB projection/truncation, stable reason codes, backend identity, static
inspection → bounded load/compile → hidden first-frame stages, caching and
invalidation rules, daemon/manager mapping.

**Out of scope:** new scene features and zero-copy transport.

**Files/modules:** protocol docs; `kwe-daemon` apply/status paths; manager
renderer-status/detail presentation; scene worker/inspector reporting.

**Acceptance:** old workers produce `report=unavailable` without stderr phrase
matching; new worker reports never exceed cap; complete inspector decisions
cannot be weakened by projection/truncation; Apply refuses required missing
features before replacing the live wallpaper; stale cache invalidates on
content/build/assets/compiler/GPU changes.

**Failure/recovery:** killed inspector/compiler/hidden renderer leaves previous
wallpaper and returns a typed timeout/crash/resource result; cache corruption is
ignored and atomically rebuilt.

**Tests:** protocol round-trip/unknown-field/oversize tests; daemon state tests;
manager loading/partial/unavailable/canceled/failed states; hidden-render
kill/hang/OOM smoke.

**Provenance:** original protocol; upstream projects idea-only.

**UX/accessibility:** status text and icon, expandable missing-feature list,
Retain current wallpaper, Retry, and Apply with limitations where allowed.

**Capabilities:** establishes evidence storage for `content.scene2d`,
`content.scene3d`, `runtime.scenescript`, and all scene sub-capabilities.

#### SR-2 — Asset VFS, typed IR, and module split

**Outcome:** Current behavior is reproduced through a typed scene IR and
smaller modules; `main.rs` becomes worker orchestration and `vulkan.rs` becomes
backend execution rather than the scene model.

**In scope:** confined VFS; asset IDs; IR and unknown-field bags; authored vs
runtime state; object hierarchy and stable IDs; split loader/runtime/backend;
coordinate-space contract; compatibility adapter from current structures.

**Out of scope:** new visual features, renderer protocol break, model/puppet
implementation.

**Files/modules:** new `asset`, `ir`, `runtime`, `report`, and backend modules in
`kwe-core`/`kwe-scene-renderer`; current scene/model/effect loaders migrated in
small commits.

**Acceptance:** all existing synthetic oracles are unchanged within their
pinned tolerances; unknown fields survive IR load/report round-trip; no ambient
path access after VFS construction; object order/hierarchy is deterministic.

**Failure/recovery:** malformed references, cycles, missing parents, duplicate
IDs, and cap overflow produce typed local failures; worker rollback behavior is
unchanged.

**Tests:** old/new IR differential tests, hostile VFS paths, hierarchy cycles,
unknown preservation, full scene smoke on llvmpipe/NVIDIA.

**Provenance:** record any parser adaptation per exact upstream file. Proposed
`glam` dependency requires approval here.

**UX/accessibility:** no visible regression; reports become more specific.

**Capabilities:** no row advances; this is a prerequisite for every scene ID.

#### SR-3 — Killable shader service, reflection, and cache

**Outcome:** A shader cannot strand a compiler thread or make an unreflected
pipeline; every fallback has a stable reason and affected pass.

**In scope:** helper binary/process contract; preprocessing ABI; shaderc
isolation; SPIR-V validation/reflection spike; total scene compile budget;
success/failure cache; shader family inventory.

**Out of scope:** expanding shader language semantics beyond fixtures.

**Files/modules:** new compiler helper crate/binary; `shaderpre.rs`,
`materialshader.rs`, supervisor child containment, package dependencies, report
schema.

**Acceptance:** timeout kills the helper; no detached compiler threads;
descriptor/attribute layouts come from reflection; cache hit is byte-identical;
cache key changes on every ABI/backend input; unsupported uniforms do not
silently become Compatible.

**Failure/recovery:** compile crash/hang/OOM or invalid SPIR-V degrades/refuses
the affected scene according to inspection policy, never hangs the renderer;
one bounded helper restart at most.

**Containment:** daemon kill/reap covers the entire worker process group; the
worker reaps the helper on normal teardown; only explicitly listed IPC/cache
FDs survive exec; helper environment and CPU/address-space/file/output/process
limits are stricter than the renderer's. Cache ownership, parsing, atomic
replacement, permissions, total size, and eviction are part of the contract.

**Tests:** shader bombs, include explosion, invalid interface, descriptor cap,
compiler crash, cold/warm/stale/poisoned cache, kill at every compile/cache
phase, renderer crash, daemon generation change, concurrent Apply cancellation,
orphan-process checks, sustained process/thread count.

**Provenance/dependencies:** retain shaderc; approve exact reflection and
optional SPIRV-Tools versions before merge; update package and ledger.

**UX/accessibility:** show “custom shader unsupported/failed” with pass/object,
not “scene failed.”

**Capabilities:** `scene.shader`, `scene.material`, `scene.effects` remain
Partial until family fixtures pass.

#### SR-4 — Render graph and resource planner

**Outcome:** Base draws, effects, FBOs, copies, feedback, postprocess, and final
composition use one validated graph with explicit resources and barriers.

**In scope:** graph IR; same-frame/previous-frame edges; object order; regions;
clear/load/store/alpha rules; cycle/uninitialized-read detection; resource
budgets/lifetimes; Graphviz diagnostics; migration of current S5/S7 actions.

**Out of scope:** resource aliasing optimization, shadows/3D passes, DMA-BUF.

**Files/modules:** new render-graph/resource modules; migrate effect planning
from `main.rs` and command recording from `vulkan.rs`.

**Acceptance:** existing simple scenes are visually unchanged; S7d ping-pong
base seed, waterwaves/foliagesway, sub-region copy, flat-gray, and internal
`_rt_FullFrameBuffer` cases receive minimal synthetic fixtures and pass; every
read has one defined writer or explicit previous-frame seed.

**Failure/recovery:** accidental cycles, feedback hazards, excessive targets,
and memory-plan overflow fail before Vulkan submission and retain last-good.

**Tests:** graph unit tests, barrier/region/cycle tests, multi-object and
multi-effect pixel oracles, validation layers, injected allocation/fence/device
loss.

**Provenance:** OWE render graph is idea-only (GPL-2.0-only); adapted effect
semantics may use pinned Almamu sources with `Borrowed-From`.

**UX/accessibility:** report the exact unsupported/hazardous pass and whether a
reduced-fidelity apply is possible.

**Capabilities:** foundation for `scene.effects`, `scene.render-target`,
`scene.postprocess`, and later 3D passes.

### Wave B — broad and honest 2D coverage

#### SR-5 — 2D images, compositions, materials, and effects

**Outcome:** Common 2D scene families render through the v2 graph with no
silent flat-quad or zero-uniform success.

**In scope:** image/composition/fullscreen layers; hierarchy; parallax/depth
parallax; all observed blend modes with explicit fallbacks; animated textures;
TEXV format/frame/mip coverage; UV transforms; material constants/combos;
effect families; bloom and postprocess used by 2D scenes.

**Out of scope:** puppet deformation, full 3D lighting, editor-only custom
system-shader replacement.

**Files/modules:** asset/texture/material/effect IR, texture decoder/uploader,
2D runtime, render passes, compatibility fixtures.

**Acceptance:** one original fixture per supported texture/blend/material/
effect family; unsupported family is detected during inspection; cross-feature
composition fixtures; private corpus shows no regression and materially fewer
unexplained fallbacks.

**Failure/recovery:** corrupt mips/frames, decode bombs, missing stock assets,
shader failure, and VRAM cap produce typed per-object outcomes and no blank
promotion.

**Tests:** unit decode bounds, image oracles, render-graph effects, scale modes,
multi-output aspect, llvmpipe/NVIDIA/Mesa evidence.

**Provenance:** extend exact TEXV/material/effect Almamu citations; never commit
stock assets or Workshop shaders.

**UX/accessibility:** pre-Apply list of missing stock assets and reduced effects;
degraded badge persists after Apply.

**Capabilities:** may advance individual `scene.texture.*`, `.layer.image`,
`.material`, `.blend`, `.effects`; `content.scene2d` stays Partial.

#### SR-6 — Text layout and font fidelity

**Outcome:** Text layers support shaped Unicode rather than one-codepoint glyph
placement.

**In scope:** dependency spike; shaping, kerning, bidi/RTL, combining marks,
CJK fallback, line breaks/alignment, font styles, bounded atlas/cache, script
text mutation, scale/DPI behavior; color-font policy documented.

**Out of scope:** arbitrary rich-text/HTML unless observed and specified.

**Files/modules:** replace/extend `text.rs` and font resolver; package deps;
text IR/runtime/reporting.

**Acceptance:** Latin/Arabic/Hebrew/CJK/combining/emoji-policy fixtures; stable
layout metrics and pixel masks; bounded fallback selection; no full filesystem
font rescan per worker/frame.

**Failure/recovery:** hostile font, huge glyph/line count, atlas exhaustion,
font removal, and rebuild storms degrade the text object only and remain
bounded.

**Tests:** parser/raster/shaping unit tests, pixel/layout oracles, 2 rebuilds/s
or revised measured bound, memory accounting, fallback diagnostics.

**Provenance/dependencies:** maintainer chooses reviewed system FreeType+
HarfBuzz+Fontconfig or pure-Rust stack; update ledger/package first.

**UX/accessibility:** report substituted/missing font by family without exposing
private paths; text scaling remains legible in manager previews.

**Capabilities:** `scene.layer.text`, relevant SceneScript text APIs.

#### SR-7 — Video, sound, timelines, and animated textures

**Outcome:** Time-based 2D assets share one pause/seek/rate/loop clock and never
desynchronize the compositor or bypass grants.

**In scope:** VideoLayer controls; video textures; sound-layer policy; timeline
animations; texture sequences; deterministic pause/resume; media-state fanout;
bounded decode/upload; audio mute separate from audio-response permission.

**Out of scope:** arbitrary network sources, embedded executable codecs,
unbounded concurrent decoders.

**Files/modules:** video runtime, new animation/timeline runtime, sound policy,
input messages/reports, render-graph external texture nodes.

**Acceptance:** loop/rate/seek/pause fixtures; two-decoder cap behavior; texture
sequence frame oracle; no-audio and muted-audio states; timeline event trace;
shared scene clock under FPS changes.

**Failure/recovery:** corrupt/stalled decoder, seek storm, dimension change,
source cap, and device upload failure skip/degrade the object without wedging
the worker.

**Tests:** libmpv synthetic media, fault injection, pause/keepalive, color/alpha
conversion, last-good and cleanup checks.

**Provenance:** existing libmpv ledger; any animation-format adaptation is
file-level recorded.

**UX/accessibility:** audio/mute/permission and decoder-limit states are textually
explained before Apply.

**Capabilities:** `scene.layer.video`, `.layer.sound`, `.timeline`,
`runtime.pause`, media IDs.

#### SR-8 — Particle runtime completeness

**Outcome:** Particles use a component IR with explicit supported emitters,
initializers, operators, renderers, child systems, control points, and audio
response—no ad hoc “mostly works” claim.

**In scope:** observed official component vocabulary; sprites, animated
spritesheets, trails; transforms; deterministic fixed-step policy; 8 control
points driven by passively relayed pointer state; children/events; audio
response; 2D and perspective flags; prewarm and aggregate budgets.

**Out of scope:** GPU simulation until CPU profile proves it necessary;
unobserved editor-only components may remain explicit Partial.

**Files/modules:** particle parser/IR/runtime/render passes, pointer/audio
bindings, reports.

**Acceptance:** one numeric and/or image fixture per supported component;
unsupported names are enumerated during inspection; parent/child/control-point
order is deterministic; particle cap and prewarm time are bounded globally and
per system.

**Failure/recovery:** component cycles, event storms, NaN/overflow, extreme dt,
collision/boid complexity, and particle count pressure cannot exceed step/time/
memory budgets.

**Tests:** 300+ frame deterministic traces, max-bound hostile chains, pointer
and audio event oracles, draw/blend/lighting fixtures, kill/restart cleanup.

**Provenance:** official docs for semantics; exact Almamu particle files for any
adaptation. OWE remains idea-only.

**UX/accessibility:** enumerate missing particle renderer/operator and likely
visual impact; audio/pointer requirements visible.

**Capabilities:** `scene.particle`, input/audio IDs, later `content.scene3d` for
perspective/lighting variants.

### Wave C — full runtime and customization

#### SR-9 — Input, audio, media, time, resize, and hit testing

**Outcome:** Promoted scene workers receive complete, generation-safe runtime
signals and own bounded object hit testing; Plasma remains a passive relay.

**In scope:** pointer enter/leave/move/down/up/click, buttons, normalized/world
coordinates, solid-object hit tests, 16/32/64 stereo audio views, time-of-day,
runtime rollover, FPS, live resize, pause, media status/properties/artwork/
timeline, general language setting.

**Out of scope:** raw audio, global keyboard capture, arbitrary shortcuts,
filesystem/process access, OS pointer lock/grab, global pointer capture, or
event swallowing in the Plasma bridge.

**Protocols:** define additive `INPUT_PROTOCOL_V2` messages while accepting v1;
latest-wins/coalescing and ack semantics are explicit and bounded.

**Acceptance:** only promoted generation receives input; disconnect resets
state; hit testing follows visible/solid/object transforms; exact band mapping;
resize emits once per accepted geometry change; pause freezes correct clocks.

**Failure/recovery:** malformed/oversized/flooded messages drop with bounded
diagnostic; stale generations cannot mutate active state; album art decode is
size/format bounded.

**Tests:** protocol fuzz/state tests, pointer event traces, audio vectors, MPRIS
missing-field cases, resize/restart, Plasma gesture/context-menu preservation.

**Provenance:** original protocol; official SceneScript semantics.

**UX/accessibility:** interaction mode must not steal standard Plasma actions;
permission and unavailable-signal states are explained. Non-granted scenes are
hover-only. Any later click interaction requires a separately approved user
grant, an exact allowed-button/gesture policy that preserves Plasma right-click,
long-press, and edit-mode behavior, an immediate keyboard-accessible disable
action, and a keyboard-accessible alternative to pointer-only actions.

**Capabilities:** runtime pointer/audio/time/screen/media/pause IDs.

#### SR-10 — SceneScript lifecycle and object model

**Outcome:** SceneScript is implemented against the official API inventory,
with per-object/per-field module ownership and bounded host calls.

**In scope:** ECMAScript module loading; `engine`, `input`, `thisScene`,
`thisLayer`, `thisObject`, `shared`, console; init/update/destroy/resize/
properties/general/cursor/media events; vectors/matrices; layer/effect/material/
particle/video/sound/model interfaces as their renderer capabilities exist;
dynamic assets/layers; timers; local storage quotas; module helpers.

**Out of scope:** arbitrary network, filesystem, process execution, unsupported
official editor-only APIs. User shortcuts require a later explicit allowlist.

**Files/modules:** replace narrow `js.rs` proxy architecture with generated or
table-driven host bindings tied to capability IDs; runtime scheduler and report.

**Acceptance:** official API inventory is machine-readable; every member is
Supported/Partial/Unavailable with tests; lifecycle ordering is deterministic;
multiple scripts have correct `this*` identity and shared state; dynamic object
creation respects asset registration and budgets.

**Failure/recovery:** infinite loop, recursion, heap exhaustion, timer explosion,
host-call storm, exception, module cycle, and teardown race are contained;
hard OOM/timeout produces typed worker failure, not Plasma impact.

**Authority/persistence:** storage is daemon-namespaced by wallpaper and user,
quota/version bounded, unavailable during inspection, inaccessible across
wallpapers, and cleared/migrated by an explicit uninstall/upgrade policy.
Property file/directory access is represented by daemon-owned grant handles,
not ambient paths. Dynamic assets register VFS logical IDs against per-scene
and lifetime byte/count budgets, with no ambient filesystem fallback.

**Tests:** API conformance fixtures, event traces, timer quotas, object lifetime,
denied host actions, memory/interrupt behavior, restart/storage migration.

**Provenance:** official `lib.sceneScript.d.ts`/docs are semantic source;
QuickJS dependency remains MIT. Any Almamu behavior adaptation is individually
recorded.

**UX/accessibility:** list missing API class/event before Apply; runtime script
failure exposes bounded diagnostic and Retry/disable-scripting option only when
the scene can still render meaningfully.

**Capabilities:** `runtime.scenescript` advances only per class/event, never as
a blanket flag.

#### SR-11 — User properties, persistence, and presets

**Outcome:** Properties round-trip exactly, apply live transactionally, and
remain preserved when unknown.

**In scope:** color, slider, bool, combo, text, texture/file, directory, group,
unknown; script properties; material/effect bindings; per-wallpaper defaults,
current values, reset, named presets; property delta protocol; explicit file/
directory grants and enumeration caps.

**Out of scope:** arbitrary executable user shortcuts; editor/publishing.

**Files/modules:** property schema/IR, daemon persistence and migration,
scene runtime bindings, manager Kirigami controls, protocol v2.

**Authority:** file/directory properties resolve only through revocable,
daemon-owned grant handles mapped to VFS logical IDs. Renderer-visible ambient
paths, cross-wallpaper storage, and inspection-time persistence are forbidden.

**Acceptance:** exact hidden combo values/Unicode/numeric precision preserved;
unknown metadata survives edits; only changed values are delivered after the
initial full event; failed live update rolls back property/runtime state;
presets are atomic.

**Failure/recovery:** missing/revoked asset, oversized text/directory, symlink/
path swap, invalid value, and renderer restart have deterministic fallback and
no grant escape.

**Tests:** all property types, unknown round-trip, persistence migration,
directory cap, grant revoke, live delta ordering, restart and rollback.

**Provenance:** official property docs; original persistence/protocol.

**UX/accessibility:** native labeled controls, keyboard navigation, focus order,
screen-reader names, typed value entry where precision matters, reset/preset
confirmation, supported/partial read-only unknown state.

**Capabilities:** all `property.*`, `property.live-update`, SceneScript property
events.

### Wave D — puppet, animation, and true 3D

#### SR-12 — Puppet mesh, skeleton, morph, and attachments

**Outcome:** 2D puppet-backed images render their actual deformation or
explicitly use a visible quad fallback; raw mesh data is never mistaken for
parity.

**In scope:** bounded MDLV/MDAT/MDMP format research; mesh/index buffers;
skeleton/bones; weights; animation layers/events; morphs; masks; attachments;
bone simulation; timeline and SceneScript bindings.

**Out of scope:** generic FBX import and editor authoring.

**Files/modules:** model/puppet parsers and IR, animation runtime, Vulkan mesh/
skin/morph passes, SceneScript animation interfaces.

**Acceptance:** one original fixture per accepted version/feature; vertex/index/
bone/weight bounds; correct quad fallback for unsupported puppet version;
geometry, mask, animation, and attachment image/event oracles.

**Failure/recovery:** malformed offsets/counts/cycles/NaNs, excessive bones/
morphs, and simulation instability produce typed object degradation/refusal,
never unsafe reads or GPU hangs.

**Tests:** truncation at every binary boundary, fuzz/property tests, skinning/
morph pixels, animation ordering, fallback alignment, validation layers.

**Provenance:** exact reverse-engineered formats require a research note and
file-level Almamu citations for adaptations; OWE fixtures/code cannot be used.

**UX/accessibility:** “puppet fallback” is visible before and after Apply.

**Capabilities:** `scene.puppet`, `.animation`, `.physics`, SceneScript animation
classes.

#### SR-13 — 3D meshes, cameras, depth, and model materials

**Outcome:** `content.scene3d` begins with real geometry and perspective—not
flat quads.

**In scope:** converted model mesh versions; vertex/index/normal/tangent/UV/
color/skin data; object/local/world hierarchy; orthographic and perspective
cameras; near/far/FOV/aspect; depth/cull/front-face; static and animated model
instances; PBR texture slots and base model shader families.

**Out of scope:** advanced shadows/volumetrics/physics until SR-14.

**Files/modules:** model parser/IR, transform/camera runtime, mesh resource
planner, depth targets, reflected model pipelines, SceneScript model APIs.

**Acceptance:** cube/occlusion/perspective/camera-switch/normal-map fixtures;
correct depth and culling on llvmpipe/NVIDIA/Mesa; 2D scenes containing a 3D
model keep their 2D camera semantics; no `.obj/.mtl` detour is used as proof of
Workshop format support.

**Failure/recovery:** huge/degenerate/non-finite meshes, unsupported format,
missing material, shader failure, and depth allocation failure are detected
before draw and preserve last-good.

**Tests:** binary parser bounds, transform math, depth pixels, camera event
traces, vertex format reflection, device loss, validation layers.

**Provenance/dependencies:** exact format citations; `glam` approval; no Assimp.

**UX/accessibility:** name the exact missing mesh/camera/material feature;
flat-quad fallback cannot be labeled 3D-compatible.

**Capabilities:** individual `scene.model3d`, `.camera`, `.material3d`; overall
`content.scene3d` remains Partial.

#### SR-14 — Lights, shadows, fog, reflections, postprocess, and 3D physics

**Outcome:** Major official 3D presentation systems execute as render-graph
passes with explicit quality/budget controls.

**In scope:** point/spot/directional lights; cookies; PBR/rim/toon/emissive;
shadow maps/atlas; reflection/environment maps; distance fog; bloom/HDR-like
internal range; volumetric lighting; model animation attachments; bounded
physics/bone simulation; particle lighting/refract in perspective.

**Out of scope:** system-wide HDR output transport (SR-15), ray tracing,
editor-only features.

**Files/modules:** light/scene IR, render graph passes/resources, quality policy,
physics runtime, model/particle shader families, capability report.

**Acceptance:** fixture and image oracle for every supported light/shadow/fog/
postprocess family; quality levels have deterministic resource plans; mixed 2D/
3D composition preserves alpha/object order; private corpus improvement is
reported by feature, not only by apply count.

**Failure/recovery:** too many lights/shadows, atlas overflow, expensive
volumetrics, simulation instability, and GPU budget exceed degrade by a
documented quality ladder or refuse before submission—never silently disable.

**Tests:** golden scenes, budget boundaries, validation layers, shader-helper
faults, sustained 10-minute run, pause/resume, device loss.

**Provenance:** official docs for semantics; pinned, file-level adaptation only
from license-compatible upstream.

**UX/accessibility:** show renderer-dependent quality reductions and controls;
do not encode quality only by color.

**Capabilities:** `scene.lighting`, `.shadow`, `.fog`, `.reflection`,
`.postprocess`, `.physics`; eligible `content.scene3d` claim only after the
six-step ladder per feature.

### Wave E — production transport, performance, and release evidence

#### SR-15 — Color, pacing, memory, and optional zero-copy transport

**Outcome:** Correct rendering remains stable across output sizes/FPS and can
optionally avoid CPU readback without weakening recovery.

**In scope:** real elapsed/scene time uniforms; per-output pacing; resize;
explicit SDR color-space/alpha metadata; damage/unchanged-frame behavior;
tracked CPU/VRAM peaks; pipeline/shader cache telemetry; optional DMA-BUF +
sync-fd negotiation behind `FRAME_PROTOCOL_V2`; v1 fallback always available.

**Out of scope:** mandatory zero-copy, compositor-specific hacks, unproven HDR
claims.

**Files/modules:** frame protocol v2 ADR, Vulkan export/sync, display bridge
import, daemon negotiation, performance harness.

**Acceptance:** v1 and v2 produce equivalent pixels; failed import/fence falls
back to v1 without restarting Plasma; 30/60/120 FPS pacing and multi-output
sizes are stable; cache/memory stay within configured bounds.

**Failure/recovery:** invalid FD/metadata, missing extension, stale/reused fence,
consumer crash, producer crash, GPU reset, and resize race are fault-injected;
last-good/fallback path remains.

**Tests:** protocol fuzz, FD ownership, synchronization, pixel equivalence,
hotplug/scale/rotation, sustained resource telemetry, Plasma PID unchanged.

**Provenance/dependencies:** waywallen-display is idea/protocol reference; any
MIT adaptation needs exact revision/path and review.

**UX/accessibility:** backend/transport shown in diagnostics only unless it
affects quality/recovery; software fallback is plainly identified.

**Capabilities:** `runtime.fps`, `runtime.screen`, display scaling plus backend
evidence; no scene feature advances solely from performance work.

#### SR-16 — Compatibility release gate and documentation consolidation

**Outcome:** Scene rendering ships only with an auditable per-feature claim,
recovery record, and accessible manager experience.

**In scope:** consolidate superseded `BETA_M3`/`SCENE_FORMAT_V1`/
`FEATURE_COMPATIBILITY` statements; capability database; renderer probe;
installed-package test; diagnostics/export; user guide; known-differences list;
license/provenance audit; release matrix.

**Out of scope:** claiming perfect compatibility or committing real Workshop
fixtures.

**Acceptance:** every claimed capability meets all six parity steps; all
skipped tests are explicit; clean package contains every required helper and
runtime dependency; installed-layout hidden render works; llvmpipe, NVIDIA, and
Mesa records exist; recovery actions work without Plasma restart.

**Failure/recovery:** missing helper/compiler/assets/driver and stale cache/
daemon/package versions have actionable diagnostics; safe mode and retry are
verified from installed binaries.

**Tests:** full format/lint/unit/integration/fault/compatibility/package suites;
privacy review of reports; accessibility review; live smoke only with explicit
maintainer authorization.

**Provenance:** `THIRD_PARTY.yml` exact revisions/paths/notices complete;
`Borrowed-From` comments audited; GPL-2-only sources remain idea-only.

**UX/accessibility:** keyboard and screen-reader pass; loading/success/empty/
offline/canceled/degraded/failed/quarantined states; text+icon compatibility;
limitations explained before Apply.

**Capabilities:** only rows with complete evidence advance. Remaining features
stay Partial, Renderer-dependent, or Unsupported with exact reasons.

## 9. Global bounds to design before implementation

Existing bounds remain until a task changes them with measurements. SR-1/SR-2
must publish a single table for:

- package entries/source/decompressed bytes and nested-reference depth;
- scene objects, hierarchy depth, dependencies, and unknown fields;
- texture source/decoded/VRAM bytes, dimensions, mips, frames, and total count;
- model vertices, indices, attributes, meshes, bones, weights, morphs;
- shaders, include depth/count/bytes, compile time, SPIR-V bytes, pipelines;
- render targets/passes/actions/regions, total attachment bytes and feedback;
- text layers/chars/glyphs/font files/atlas bytes/rebuild rate;
- video/sound decoders, source/cache/frame bytes and event rates;
- particle systems/live particles/components/children/prewarm/steps per frame;
- scripts, heap/stack/host calls/timers/modules/log lines/storage bytes;
- input/audio/media/property queues and message bytes/rates;
- frame dimensions/mapping bytes, fence waits, retries, reports and logs;
- disk caches and diagnostic retention.

Every bound defines behavior at `limit-1`, `limit`, and `limit+1`: accept,
degrade, refuse, or terminate. “Best effort” without a deterministic boundary
is not acceptable for untrusted content.

## 10. Required adversarial review checklist for every slice

- Can any new parser/compiler/decoder/script execute in `plasmashell` or the
  manager? If yes, reject the design.
- Can a worker child/helper outlive its process group or parent?
- Can a timeout leave a running thread/process/GPU submission?
- Can an asset path escape the package/root/grant through traversal, symlink,
  hard link, rename, case, Unicode, or archive ambiguity?
- Can counts/sizes multiply after decompression, decode, tessellation,
  animation, particle spawn, script creation, or render-target planning?
- Can an unknown uniform/component/object be presented as Supported?
- Can a cache entry cross content, build, compiler, assets, GPU, or driver
  identities incorrectly?
- Can graph feedback, aliasing, barriers, alpha conventions, color formats, or
  object order produce a plausible but wrong “success”?
- Does device loss/fence timeout preserve last-good without restarting Plasma?
- Does rollback clean private HOME, staged media, helper processes, cache temp
  files, FDs, Vulkan objects, and assignments?
- Are diagnostics bounded, structured, redacted, and actionable?
- Are loading, success, empty, offline, canceled, degraded, failure, and
  quarantine states covered where applicable?
- Is compatibility communicated with text and icons, not color alone?
- Is each claim backed by a synthetic success+failure fixture, automated
  image/event oracle, UI state, hardware facts, and documented semantic
  differences?
- Are upstream revision, license, exact files, allowed use, notices, ledger,
  and nearby comments correct before code merges?

## 11. Decision gates for maintainer review

No implementation should begin until the maintainer approves these in order:

1. **Architecture:** Vulkan-only renderer-v2 foundation; v1 frame transport
   retained; no OpenGL/runtime-upstream backend.
2. **Truth policy:** unsupported active features default to retaining the
   current wallpaper; “Apply with limitations” is explicit.
3. **Protocol:** capability IDs and inspection/render report schemas.
4. **Dependencies:** exact choices for math, SPIR-V reflection/validation, and
   text shaping; shaderc stays isolated.
5. **Provenance:** amend ADR 0001's pre-relicense language, correct/pin ledger
   entries, and keep GPL-2-only sources idea-only.
6. **Reference evidence:** approve the private corpus harness and any local
   official-client screenshot/reference workflow without committing content.
7. **Work order:** SR-0 → SR-4 foundation before S7d visual feature expansion
   resumes, then broad 2D/runtime, puppet/3D, and transport/release waves. A
   narrow pre-SR-4 safety/honesty lane remains open only to prevent blank or
   misleading promotion, add typed diagnostics, or refuse hazardous/incorrect
   paths, with a synthetic regression fixture and no new compatibility claim.

## 12. Definition of done for “proper scene rendering”

The program is not done when all local wallpapers merely Apply. It is done for
a declared release scope when:

- the complete declared feature inventory is inspected before Apply;
- every implemented feature has the six-step parity evidence;
- unsupported or approximate behavior is explicit per object/pass/API;
- the render graph has no unmodeled read/write/feedback path;
- shader/script/parser/decoder/GPU failure is bounded and recoverable;
- 2D and 3D claims correspond to real geometry/camera/render semantics;
- private corpus results report feature and visual coverage, not admission;
- llvmpipe, NVIDIA, and Mesa backend records exist;
- package/upgrade/restart/safe-mode/rollback paths work from installed files;
- the Plasma bridge remains a thin validated frame consumer and Plasma never
  needs a restart after renderer failure;
- provenance and user-visible limitations are complete and auditable.

Until then, `content.scene2d`, `content.scene3d`, and
`runtime.scenescript` remain Partial/Renderer-dependent at the appropriate
feature granularity.
