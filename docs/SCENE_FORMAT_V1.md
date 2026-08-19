# Scene format v1 and SceneScript API

Status: **implemented (M3a)** for the items marked below; everything else in
the table is planned and reserved, not implemented. This document describes
what the M3a worker (`kwe-scene-renderer`, the daemon's `scene` kind)
accepts and what script surface it runs. The scene entry format is the
foundation slice of the original SceneScript engine per ADR 0001.

## Provenance

This is an original implementation. The scene format and script API are
inspired by the behavior of open-wallpaper-engine and linux-wallpaperengine
(and through them, the original wallpaper engine scene format), which are
consulted **as behavior references only** — for the shape of `scene.json`
and the script entry points `init` / `update` / `resized`. No code is
copied from either project or from wallpaper engine itself: the schema
parser, the QuickJS engine wrapper, and the Vulkan compositor are written
for this crate (SPDX Apache-2.0 headers). The GPL-licensed reference
projects contribute no code to this repository (see THIRD_PARTY.yml for the
actual third-party components: rquickjs 0.12.2 + vendored quickjs-ng, both
MIT).

## scene.json

A UTF-8 JSON file, at most **16 MiB** (`MAX_SCENE_JSON_BYTES`). The root
must be an object. Parse failures — unreadable file, invalid JSON, wrong
shape, out-of-range values, or a `script` reference that is missing,
non-string, escapes the content root, is not a `.js` file, or exceeds the
2 MiB cap — are backend rejections: a bounded stderr diagnostic
(`event=renderer.scene.backend_reject kind=... detail=...`) and **exit 73**
before the canary, so the supervisor records `exit_code_73` and rolls back.

```json
{
  "general": {
    "clearcolor": [0.1, 0.1, 0.1, 1.0],
    "resolution": [1920, 1080],
    "fps": 30,
    "script": "script.js"
  }
}
```

| Field | Type | Default | Meaning in M3a |
|---|---|---|---|
| `general` | object (optional) | `{}` | Scene-wide settings. Must be an object when present. |
| `general.clearcolor` | `[r, g, b, a]` of finite floats in `0.0..=1.0`, exactly 4 entries | `[0, 0, 0, 1]` | The color the worker clears every frame — unless the script writes `Engine.clearcolor`, which is read back after every `update()`. |
| `general.resolution` | `[w, h]` of integers in `1..=8192`, exactly 2 entries (optional) | none | Parsed and validated, but **non-binding in M3a**: the worker always renders at the daemon-requested `--width`/`--height`. A mismatch is logged once (`event=renderer.scene.resolution scene=... requested=...`), not an error. |
| `general.fps` | finite float in `(0.0, 240.0]` (optional) | none | Same: parsed and validated, non-binding hint; a mismatch is logged (`event=renderer.scene.fps`), not an error. The pacing always comes from the daemon's `--fps`. |
| `general.script` | string (optional) | none | A path **relative to the scene.json's directory**, resolved against the canonicalized content root so symlinks cannot escape it. Must end in `.js` (a `.pkg` reference is explicitly rejected — the archive reader is M3b), must exist, be a regular file, and be at most **2 MiB** (`MAX_SCRIPT_BYTES`). |

Anything else in `general` or at the root is ignored (future slices: layers,
effects, properties). Unknown top-level structure never fails the parse.

## Script execution model

One QuickJS runtime + context per worker (rquickjs 0.12.2, MIT, vendored
quickjs-ng 0.15.1 — THIRD_PARTY.yml). Bounds:

| Bound | Value | Behavior |
|---|---|---|
| heap | 64 MiB (`Runtime::set_memory_limit`) | JS "Out of memory" exception → bounded `memory_limit` diag → **exit 71** (the renderer never survives an OOM; it cannot render meaningfully) |
| stack | 4 MiB (`Runtime::set_max_stack_size`) | runaway recursion → contained exception, renderer keeps the last state |
| per-update budget | 8 ms soft / 33 ms hard (wall clock in the interrupt callback — rquickjs exposes no step counts; docs/BETA_M3.md risk 1) | soft: frame skipped, bounded `event=renderer.scene.script_timeout kind=soft`; hard: uncatchable exception, counted as `hard_timeout`; the renderer always keeps publishing the last good frame |
| `dt` | clamped to `[0.0, 1.0]` | a hung producer cannot feed a huge dt downstream |
| console | 30 lines per 10 s window, 512 bytes per line | `event=renderer.scene.console`, `console_dropped` on overflow |

Script exceptions are contained: caught, counted (`script_errors` in
`event=renderer.complete`), logged at most once per error class per 30 s
window, and never kill the renderer. `Engine.clearcolor` reads that fall
back on the current color keep a throwing `update()` from corrupting state.

## SceneScript API coverage matrix

Status key: **implemented (M3a)** — in this slice and covered by tests;
*planned* — reserved for M3b–M3k; **not in scope** — explicitly out of the
beta scope. API items follow the behavior of the reference implementations
(see Provenance), restricted to what a worker can do with no window and no
image compositing in M3a.

### Globals

| API | Status | Notes |
|---|---|---|
| `Engine.frametime` | **implemented (M3a)** | seconds since the previous update, per update (number) |
| `Engine.fps` | **implemented (M3a)** | the pacing the daemon asked for, fixed (number) |
| `Engine.resolution` | **implemented (M3a)** | `{x, y}` — the pixel size the worker renders at, fixed (read-only object) |
| `Engine.clearcolor` | **implemented (M3a)** | `{r, g, b, a}` (0..1 floats). **M3a-only bridge, not a wallpaper-engine API**: the worker reads it back after every `update()` and clears the frame to it. Planned to move to `thisScene.clearcolor` once scene objects arrive. Read-back falls back to the current color on non-finite/missing values. |
| `console.log / info / warn / error` | **implemented (M3a)** | rate-bounded, truncated to 512 bytes, surfaced on the worker's stderr ring |
| `thisScene` | *planned* | with scene objects (M3c–M3k) |
| wallpaper-engine globals beyond `Engine.*` | *planned* / **not in scope** | nothing else exists in M3a; each API joins via the coverage matrix here when implemented |

### Entry points

| API | Status | Notes |
|---|---|---|
| `init()` | **implemented (M3a)** | called once at script load, after evaluation; exceptions are contained (the script is still driven) |
| `update(dt)` | **implemented (M3a)** | called once per paced step, `dt` in seconds, clamped to `[0.0, 1.0]`; the return value is ignored (the renderer reads `Engine.clearcolor` back); a missing `update` renders the current color forever |
| `resized(w, h)` | **implemented (M3a)** | called once at script load with the daemon-provided size; dimensions are fixed in M3a — there is no live-resize path (docs/BETA_M3.md risk 7) |

### Scene objects and render model

| API | Status | Notes |
|---|---|---|
| layers, effects, text, particles, 3D models, properties | *planned* (M3c–M3k) | the parse tolerates extra keys but renders none of them |
| `.pkg` archives | *planned* (M3b) | rejected today with a bounded diagnostic |
| image assets | *planned* | no asset loading in M3a; the clear pass is the only draw |
| audio/pointer/media input in script | *planned* | the worker receives and acks the wire inputs (M1a plumbing, unchanged) but exposes none of them to the script in M3a |

## Output

Frames are premultiplied BGRA8888 through the shared frame mapping
(docs/FRAME_PROTOCOL_V1.md): a 64-byte `KWEFRM1` header, two BGRA8888
slots, generation-toggle publishing, keepalive re-publish so a script that
never changes the color cannot trip the supervisor's frame timeout. The
Vulkan attachment is `B8G8R8A8_UNORM` when supported (both validated
drivers) with an `R8G8B8A8_UNORM` fallback; the channel conversion is
identity for `B8G8R8A8` readback (bytes are already B,G,R,A) and a
[2,1,0] permutation for `R8G8B8A8`, both premultiplied with
`(v*a+127)/255` rounding — byte-exact per unit test.

## See also

- docs/BETA_M3.md — the M3a slice: goal, acceptance evidence, exit codes,
  open risks (interrupt-budget deviation, llvmpipe determinism, reader
  staleness, loader lifetime).
- docs/adr/0001-original-vulkan-renderer.md — the architecture this slice
  implements (ADR 0001 is binding).
