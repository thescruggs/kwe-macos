# Scene capability taxonomy — v1 (frozen)

**Status:** **v1, frozen 2026-08-28** under plan gate §11.3 (maintainer go
2026-08-27: "continue on and proceed through all steps"). The ID set and
naming rules below are stable; additions are allowed (additive, new rows
start `planned`), renames/removals require a v2 with an alias table. No ID
in this file is a support claim; rows advance only through the six-step
parity ladder (`docs/FEATURE_COMPATIBILITY.md` §"Parity ladder").

**SR-1c:** `kwe-core`'s `SCENE_CAPABILITIES_IMPLEMENTED` (every row this file
marks `experimental`) and `SCENE_CAPABILITIES_LIMITATION_TOLERATED`
(`crates/kwe-core/src/capabilities.rs`) are a mechanical projection of this
file's status column — the daemon's scene apply gate
(`wallpaper.apply`, `docs/SUPERVISOR_API_V1.md`) classifies a scene's
`required` capabilities against them. Every status change here must update
those consts in lockstep (SR-16's evidence DB is planned to replace both
with a real database).

Corpus-informed freeze decisions (SR-0d baseline, 60 local scenes):

- root-level `camera` (unread in all 60 items — the S7 orthogonalprojection
  root cause) maps to the existing `scene.camera` row, which covers the 2D
  orthographic projection use as well as true 3D cameras; no new ID.
- `parallaxDepth`/`copybackground`/`locktransforms`/`solid` are object
  composition/hit-test facets of `scene.layer.image` and
  `scene.input.hit-test`, not new IDs (SR-5/SR-9 scope).
- `scene.texture.compressed` stays a separate row (SR-0a's fold-into-`.texv`
  question resolved: keep, since compressed payloads have their own decode
  path and failure modes).

## Naming and stability rules

- IDs are lowercase, dot-separated, `scene.` rooted; families use one more
  segment (`scene.texture.texv`), never deeper than three segments.
- An ID names an author-visible behavior (what a wallpaper can require), not
  an implementation module. Renaming after SR-1 freeze requires a schema
  version bump and an old→new alias table.
- Status vocabulary matches `FEATURE_COMPATIBILITY.md`: `planned` (no code),
  `experimental` (code exists, no parity evidence), `partial`, `supported`,
  `renderer-dependent`, `unsupported`. This file never holds `supported`.
- Every ID advances individually; parent public IDs (`content.scene2d`,
  `content.scene3d`, `runtime.scenescript`) are computed rollups and are never
  set directly.

## Taxonomy

Evidence column: what the parity ladder minimally requires for this ID beyond
the standard six steps. All rows additionally require a synthetic success and
failure fixture.

### Package, assets, textures

| ID | Parent | Definition | Evidence | Draft status |
|---|---|---|---|---|
| `scene.package` | `content.scene2d` | Bounded `scene.pkg`/`scene.json` container reading with entry/byte/depth caps | Malformed/oversized/truncated fixtures refuse typed | experimental |
| `scene.asset-vfs` | `content.scene2d` | All asset resolution through the confined VFS with logical IDs; no ambient paths after load | Traversal/symlink/case/Unicode escape fixtures fail closed | planned (SR-2) |
| `scene.texture.static` | `content.scene2d` | PNG/JPEG/WebP decode within dimension/byte bounds | Decode-bomb fixture bounded | experimental |
| `scene.texture.texv` | `content.scene2d` | TEXV container versions/formats declared per version | Per-version fixture; unsupported version detected at inspection | experimental |
| `scene.texture.compressed` | `content.scene2d` | Compressed payloads inside TEXV (DXT families) | Per-format pixel oracle | experimental |
| `scene.texture.animated` | `content.scene2d` | Multi-frame/spritesheet textures with declared timing | Frame-timing oracle vs shared clock | experimental |

### Layers

| ID | Parent | Definition | Evidence | Draft status |
|---|---|---|---|---|
| `scene.layer.image` | `content.scene2d` | Image/composition layer: order, 2D transform, tint, alpha | Multi-layer order/transform pixel oracle | experimental |
| `scene.layer.text` | `content.scene2d` | Shaped Unicode text layers (SR-6 scope) | Latin/RTL/CJK/combining layout+pixel fixtures | experimental (one-codepoint placement only) |
| `scene.layer.video` | `content.scene2d` | Video-textured layers under the shared scene clock | Loop/rate/seek/pause fixtures; decoder-fault degrade | experimental |
| `scene.layer.sound` | `content.scene2d` | Sound layers under grant/mute policy | No-grant and muted states; no autoplay without grant | planned (SR-7) |

### Rendering pipeline

| ID | Parent | Definition | Evidence | Draft status |
|---|---|---|---|---|
| `scene.material` | `content.scene2d` | Material constants/combos/texture slots via reflected interfaces | Combo-family fixtures; unknown uniform never silently zeroed into a Compatible claim | experimental |
| `scene.shader` | `content.scene2d` | Wallpaper shader preprocess+compile in the killable helper with reflection | Shader-bomb kill; reflected-vs-scraped differential | experimental (in-thread, scraped) |
| `scene.blend` | `content.scene2d` | Author-selectable blend modes with explicit per-mode fallbacks | Per-mode pixel oracle | experimental (5 fixed modes) |
| `scene.effects` | `content.scene2d` | Per-object effect pass chains through the validated render graph | S7d regression fixtures (ping-pong seed, sub-region copy, full-frame order) | experimental |
| `scene.render-target` | `content.scene2d` | Named FBOs, ping-pong, `_rt_FullFrameBuffer`, copy/swap semantics | Uninitialized-read and feedback-cycle rejection | experimental |
| `scene.postprocess` | `content.scene2d` | Scene-wide bloom/postprocess passes | Graph fixture + pixel oracle | planned (SR-4/5) |
| `scene.particle` | `content.scene2d` | Particle component vocabulary (emitters/initializers/operators/renderers/children/control points) | Per-component numeric or image fixture; unsupported components enumerated at inspection | experimental (subset) |

### 3D, puppet, presentation (Wave D)

| ID | Parent | Definition | Evidence | Draft status |
|---|---|---|---|---|
| `scene.model3d` | `content.scene3d` | Real mesh geometry from converted model formats (never the flat quad) | Cube/occlusion fixtures; flat-quad fallback never labeled 3D | planned |
| `scene.camera` | `content.scene3d` | Orthographic/perspective cameras, near/far/FOV/aspect | Camera-switch event+pixel trace | planned |
| `scene.material3d` | `content.scene3d` | Model shader families and PBR texture slots | Normal-map fixture on 3 backends | planned |
| `scene.puppet` | `content.scene2d` | Puppet mesh/skeleton/skinning/morph/mask deformation | Binary-boundary fuzz; visible quad fallback state | planned |
| `scene.animation` | `content.scene2d` | Timeline/skeletal animation layers and events | Deterministic event trace | planned |
| `scene.lighting` | `content.scene3d` | Point/spot/directional lights, cookies | Per-family image oracle | planned |
| `scene.shadow` | `content.scene3d` | Shadow map/atlas passes | Atlas-overflow degrade ladder | planned |
| `scene.fog` | `content.scene3d` | Distance fog | Image oracle | planned |
| `scene.reflection` | `content.scene3d` | Reflection/environment maps | Image oracle | planned |
| `scene.physics` | `content.scene3d` | Bounded bone/physics simulation | Instability containment fixture | planned |

### SceneScript

| ID | Parent | Definition | Evidence | Draft status |
|---|---|---|---|---|
| `scene.script.lifecycle` | `runtime.scenescript` | init/update/destroy/resize + event ordering per official model | Lifecycle event trace, multi-script identity | experimental (update timing only) |
| `scene.script.objects` | `runtime.scenescript` | `engine`/`thisScene`/`thisLayer`/`thisObject`/`shared` object model | API-inventory conformance fixtures | experimental (narrow proxy) |
| `scene.script.dynamic-assets` | `runtime.scenescript` | Script-registered assets via VFS logical IDs under budgets | Budget-overflow refusal | planned |
| `scene.script.dynamic-geometry` | `runtime.scenescript` | Script-created layers/geometry | Creation/teardown trace under caps | planned |
| `scene.script.timers` | `runtime.scenescript` | Timer APIs under quota | Timer-explosion containment | planned |
| `scene.script.storage` | `runtime.scenescript` | Daemon-namespaced quota-bounded local storage | Cross-wallpaper isolation; inspection-time unavailable | planned |
| `scene.script.modules` | `runtime.scenescript` | ECMAScript module loading/helpers | Module-cycle containment | planned |

### Input, audio, media, runtime signals

| ID | Parent | Definition | Evidence | Draft status |
|---|---|---|---|---|
| `scene.input.hit-test` | `runtime.pointer-position` | Worker-side visible/solid object hit testing | Transform-aware hit trace | planned (SR-9) |
| `scene.input.cursor-events` | `runtime.pointer-position` | enter/leave/move/down/up/click delivery, generation-safe | Stale-generation rejection | experimental (position relay only) |
| `scene.input.control-points` | `runtime.pointer-position` | Pointer-driven particle control points (8) | Deterministic control-point trace | planned (SR-8) |
| `scene.audio-buffers` | `runtime.audio-scene-16-32-64` | 16/32/64 stereo band views to script/shader/particles | Exact band-mapping vectors | experimental (wire type only) |
| `scene.audio-response` | `runtime.audio-scene-16-32-64` | Author-visible audio-reactive behavior (particles/materials) | Audio-vector image/numeric oracle | planned |
| `scene.media-events` | `runtime.media-*` | Media status/properties/timeline events into script | MPRIS missing-field fixtures | planned |
| `scene.album-art` | `runtime.media-*` | Bounded album-art texture delivery | Size/format-bomb refusal | planned |
| `scene.resize` | `runtime.screen` | Live resize with one event per accepted geometry change | Resize/restart trace | planned |
| `scene.timeline` | `runtime.time` | Time-of-day/timeline-driven animation on the shared clock | Clock-injection fixture | planned |
| `scene.simulation-pause` | `runtime.pause` | Pause freezes the correct clocks (sim/video/particles) deterministically | Pause/resume determinism trace | experimental |

### Properties

| ID | Parent | Definition | Evidence | Draft status |
|---|---|---|---|---|
| `scene.property.binding` | `property.live-update` | Property values bound into materials/effects/scripts, live transactional apply | Delta ordering + rollback fixture | experimental |
| `scene.property.texture` | `property.file` | User texture/file properties via revocable grant handles | Grant-revoke fallback | planned |
| `scene.property.directory` | `property.directory` | Watched directory properties under enumeration caps | Cap + symlink-swap fixtures | planned |
| `scene.property.unknown-preservation` | `property.unknown` | Unknown property types round-trip untouched | Byte-exact round-trip | experimental |

## Inventory record — draft schema `scene-feature-inventory-v0`

Emitted per inspected item by SR-0b/c/d; ≤ 64 KiB; deterministic. This v0
shape is what the current binary emits on stdout; SR-1 freezes its successor
`scene-inspection-v1` (same fields plus provenance/backend identity) in
`docs/REPORT_PROTOCOL_V1.md` and moves it onto the dedicated report FD.
Mirrors the projection rules of plan §5.3.

```json
{
  "schema": "scene-feature-inventory-v0",
  "content": { "hash": "sha256:…", "source_bytes": 0, "kind": "pkg|json-dir" },
  "inspector": { "build": "git-sha", "abi": 0 },
  "outcome": "inventoried | unknown | incompatible",
  "reason": "ok | timeout | oversize | parse-error | …stable reason codes",
  "required": ["scene.layer.image", "…sorted, deduplicated capability IDs"],
  "detected": [
    { "capability": "scene.effects", "count": 0,
      "objects": ["…first N stable logical IDs, sorted"], "truncated": false }
  ],
  "unknown": {
    "keys": 0, "types": 0, "objects": 0,
    "samples": ["…first N key paths, sorted"], "truncated": false
  },
  "bounds": { "wall_ms": 0, "peak_bytes": 0, "limits_hit": ["…reason codes"] },
  "digest": "sha256 over content+inspector+projection"
}
```

Rules: unknown keys/types are counted, never dropped or treated as parse
failure; `required` derives only from *active* objects/passes/APIs; sample
lists sort by stable logical ID, retain the first N, and set `truncated` with
omitted counts; a truncated projection never changes an inspector-side
decision; no titles, paths, asset bytes, or script source appear in the
record.
