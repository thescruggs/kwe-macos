# Scene format v1 and SceneScript API

Status: **implemented (M3a)** for the items marked below, with the
**scene.pkg archive reader (M3b)** added for the items marked M3b;
everything else in the table is planned and reserved, not implemented. This
document describes what the worker (`kwe-scene-renderer`, the daemon's
`scene` kind) accepts and what script surface it runs. The scene entry
format is the foundation slice of the original SceneScript engine per ADR
0001.

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
| `general.clearcolor` | `[r, g, b, a]` of finite floats in `0.0..=1.0`, exactly 4 entries, **or** the string form `"r g b"` (three space-separated finite floats in `0.0..=1.0`, alpha implied `1.0`) | `[0, 0, 0, 1]` | The color the worker clears every frame — unless the script writes `Engine.clearcolor`, which is read back after every `update()`. The string form is what Wallpaper Engine actually writes: **59 of 60** corpus scene.json entries use it (e.g. `"clearcolor": "0.7 0.7 0.7"`, one with five-digit precision); the property-wrapped object form `{"user": ..., "value": ...}` (1 of 60) stays rejected until user properties arrive in M3c+. |
| `general.resolution` | `[w, h]` of integers in `1..=8192`, exactly 2 entries (optional) | none | Parsed and validated, but **non-binding in M3a**: the worker always renders at the daemon-requested `--width`/`--height`. A mismatch is logged once (`event=renderer.scene.resolution scene=... requested=...`), not an error. |
| `general.fps` | finite float in `(0.0, 240.0]` (optional) | none | Same: parsed and validated, non-binding hint; a mismatch is logged (`event=renderer.scene.fps`), not an error. The pacing always comes from the daemon's `--fps`. |
| `general.script` | string (optional) | none | A path **relative to the scene.json's directory** (file scenes) or **an entry path inside the package** (pkg scenes, M3b). File scenes: resolved against the canonicalized content root so symlinks cannot escape it; must end in `.js` (a `.pkg` reference is rejected — the archive itself is only consumable through the M3b reader), must exist, be a regular file, and be at most **2 MiB** (`MAX_SCRIPT_BYTES`). Pkg scenes: see "scene.pkg" below. |

Anything else in `general` or at the root is ignored (future slices:
effects, properties — image layers are M3c). Unknown top-level structure
never fails the parse.

## Image layers (M3c)

`objects` is an array of layer objects drawn in **scene.json order** — the
compositor's draw order: the layer listed last draws on top, src-over
blending by default with per-layer blend modes (M3d). An object is an image layer exactly when
it carries an `image` field; everything else (particles, audio, text —
M3d+) is ignored. A reference ending in `.json` is a model instance under
the WE solid-model architecture — **620 of the 685 corpus image
references point at model `.json` files, the other 65 carry a null image
value; none point at a real texture** — so no corpus wallpaper yet
exercises the decoded-texture path. Model references are skipped BEFORE
any validation (a malformed model layer can never reject the scene),
without a diagnostic, and are not counted toward the layer cap, until
models arrive (M3h). At most **256 image layers** (`MAX_LAYERS`); a scene
with 257 is a Shape rejection (exit 73, "over the 256 layer cap").

```json
{
  "general": {"clearcolor": [0, 0, 0, 1], "resolution": [160, 90], "fps": 30},
  "objects": [
    {"name": "bg", "image": "textures/red.png",
     "origin": [0, 0], "size": [160, 90], "alpha": 1.0, "visible": true},
    {"name": "mark", "image": "textures/blue.png",
     "origin": [60, 34], "size": [40, 22]}
  ]
}
```

| Field | Type | Default | Meaning in M3c |
|---|---|---|---|
| `name` | string, **required** | — | the layer's name for `Scene.getLayer(name)`; a missing or non-string name rejects the layer entry |
| `image` | string | none | the image reference (see "Image sources" below); a non-string value makes the object inert (skipped, not rejected) |
| `origin` | `[x, y]` (2 or 3 entries) | `[0, 0]` | the layer's **center** in scene units (WE alignment "center", the default); scene (0,0) is the frame center, +y down; z is unused by 2D rendering |
| `angles` | `[rx, ry, rz]` (2 or 3 entries) | `[0, 0, 0]` | **radians in the file**, converted to degrees at parse (the script API speaks degrees); corpus-verified: exact π, none at 90/180 |
| `scale` | `[sx, sy]` (2 or 3 entries) | `[1, 1]` | relative scale about the origin; negative values mirror |
| `size` | `[w, h]` (exactly 2 entries) | `[0, 0]` | the size in scene units the texture is drawn at; `[0, 0]` (absent) takes the decoded texture's own dimensions at load |
| `alpha` | float in `0.0..=1.0` | `1.0` | straight layer alpha; out-of-range or non-finite rejects the scene (like clearcolor) |
| `visible` | boolean | `true` | an invisible layer draws nothing |
| `colorBlendMode` | integer (alias `blendMode`) | `0` | the corpus key; a per-layer blend mode from the researched table in "Blend modes and color effects (M3d)" below. Corpus (re-scan): 432 of 685 image-bearing objects carry it — 410×0, 6×11, 6×30, 4×6, 2×24, 1×1, 1×7, 1×9, 1×12 (sums to 432) — the rest omit it (normal). A non-numeric value is tolerated (normal), never a rejection; an undecodable but numeric value clamps to normal with a bounded one-time diagnostic |
| `brightness` | float or numeric string | `1.0` | multiplies the sampled RGB before blending (see M3d); the default 1.0 is the identity (WE-verified), the clamp range 0..=10 is a design decision; out-of-range and non-finite values clamp, never a rejection; a non-numeric type is a Shape rejection |
| `tint` | `[r, g, b]` or `[r, g, b, a]` of floats in `0.0..=1.0` (alias `color` — the WE file key, vec3 or vec4; `tint` takes precedence when both are present) | `[1, 1, 1, 1]` | multiplied onto the sampled RGBA before blending (see M3d); a 3-component value implies alpha 1; per-component clamped (non-finite → 1.0), never a rejection |

Property-wrapped values (`{"user": ..., "value": ...}` — how the editor
serializes user-bindable fields; corpus re-scan: **70% (315/447) of image
layers carrying `alpha` and 49% (276/568) of those carrying `visible`**
are wrapped) are unwrapped to their initial `value`; the wrapper's user
binding is M3j, and a wrapped scalar without a `value` rejects like any
malformed scalar.

The transform model: a layer is a rectangle of `size` scene units centered
on `origin`, drawn as two fan-ordered triangles (one unit quad, 6
vertices of pos+uv — `TRIANGLE_LIST` primitives (v0,v1,v2) and (v0,v2,v3)
tile the full quad). Per layer:
`world = R(θz)·diag(scale·size)·pos + origin` for `pos ∈ [-0.5, 0.5]²` —
rotation and scale happen about the origin, in that order; the z-angle
rotates 2D layers (radians in the file, degrees in the API). The
compositor pushes 64 bytes per layer (M3d grew the block from 48 to 64):
m0 = (a, c, tx, 0), m1 = (b, d, ty, alpha·tint.a), viewport = (w, h, 0, 0),
effects = (brightness, tint.r, tint.g, tint.b). The first 48 bytes are
byte-identical to the M3c layout, so the vertex shader reads the same
offsets; `world = mat2(m0.xy, m1.xy)·pos + (m0.z, m1.z)`, and the fragment
shader multiplies the texture's alpha by m1.w (the layer alpha folded with
the tint alpha host-side) and the sampled RGB by the effects vector,
**before** blending.

Blending is src-over by default (the Normal variant): color
ONE / ONE_MINUS_SRC_ALPHA, alpha ONE / ONE_MINUS_SRC_ALPHA — the fragment
shader outputs straight color, so the attachment stores the straight
composite and the readback's premultiplication is applied exactly once, at
the protocol boundary (see Output); the source alpha is never scaled by
itself (a 191/255 layer over an opaque destination stays 191/255, not
143/255, and a translucent layer is not darkened by a second alpha
multiply). Blend oracle, byte-exact on both drivers: opaque texel
(64,103,142,255) at layer alpha 191/255 over a zero clear is delivered as
premultiplied BGRA (106,77,48,191). A non-default `colorBlendMode` selects
one of the fixed-function variants in "Blend modes and color effects
(M3d)" below. The model math is byte-tested: identity exact, quarter-turn
axis mapping, corner positions for known inputs.

## Blend modes and color effects (M3d)

Per-layer color blending in the Wallpaper Engine sense: a `colorBlendMode`
enum rendered through fixed-function Vulkan blending, two color-effect
fields, and a fixed ordering — **the effects apply to the sampled texel in
the fragment shader; the blend mode combines the result with the frame in
the pipeline's blend state** (the shader never blends; the blend state
never scales colors).

### The researched colorBlendMode table

WE serializes `colorBlendMode` as an integer. The editor dropdown exposes
exactly {Normal, Multiply, Add, Screen, Subtract} (wpdoc UI strings; Steam
patch note: "standard Photoshop blend modes"; rendered by the proprietary
`ApplyBlending` shader, type `imageblending`), which pins the five-mode
FAMILY — it says nothing about the integers behind them. No public WE
shader source exists, so the value→name mapping below is a corpus-histogram
HYPOTHESIS: 0 dominates (410 of 432 occurrences) and is the editor's
default for new objects (verified); 1/6/7/9 are assigned to the remaining
four dropdown names as the best fit — the histogram's non-zero values that
round out the dropdown family (the five decoded values cover 17 of the 22
non-zero occurrences). The other 15 occurrences — 11, 30, 24, 12 — sit
OUTSIDE the dropdown family, so the original evidently tolerates integers
its own editor cannot produce (ours clamp to Normal with a diagnostic).
The formulas below are the oracle ground truth, byte-validated on the
llvmpipe lane.

Evidence grades: **verified** = a public source pins the value (0 only);
**decoded** = the best-fit corpus-histogram hypothesis, consistent with
the dropdown family but not independently confirmable; **undecoded** = no
credible hypothesis and not expressible in fixed-function Vulkan blending
(kept for corpus tolerance).

| Value | Name | Implemented? | Vulkan blend state (color / alpha) | Evidence |
|---|---|---|---|---|
| 0 | Normal | yes | (ONE, ONE_MINUS_SRC_ALPHA) / (ONE, ONE_MINUS_SRC_ALPHA), ADD | verified — the corpus-dominant value, the editor default |
| 1 | Multiply | yes | (DST_COLOR, ZERO) / (ONE, ONE_MINUS_SRC_ALPHA), ADD | decoded |
| 6 | Add | yes | (ONE, ONE) / (ONE, ONE), ADD | decoded |
| 7 | Screen | yes | (ONE_MINUS_DST_COLOR, ONE) / (ONE, ONE_MINUS_SRC_ALPHA), ADD | decoded |
| 9 | Subtract | yes | (ONE, ONE) REVERSE_SUBTRACT / (ONE, ONE_MINUS_SRC_ALPHA), ADD | decoded |
| 11, 12, 24, 30 | — | **no** | clamped to Normal + one bounded diagnostic per scene (`event=renderer.scene.blend_mode_clamped layer=... mode=...`) | undecoded — outside the dropdown family, not fixed-function |
| any other | — | no | silently Normal (unknown values tolerated, like the original evidently tolerates 11/30/24/12) | — |

Semantics (the formulas the oracles hand-compute; `t` = texel,
`b` = background, per channel in 0..255):
- **Normal** = src-over: `t·a + b·(1−a)`.
- **Multiply** = `t·b / 255`.
- **Add** = `min(255, t + b)`.
- **Screen** = `255 − (255−t)(255−b) / 255` = `t·(1−b) + b`.
- **Subtract** = `max(0, b − t)` — the background minus the texel: WE's
  "Photoshop blend modes" family and the Vulkan REVERSE_SUBTRACT algebra
  (dst − src) agree; a reversed direction would fail the oracle.
- **Alpha policy** (deliberate, pinned by oracles): the mode acts on the
  COLOR; the alpha channel always composites src-over (ONE,
  ONE_MINUS_SRC_ALPHA) — the layer's own opacity still matters under every
  mode — except **Add**, whose semantic is additive on both channels
  (ONE, ONE). This fixes the review finding that Multiply's original
  (ZERO, ONE) discarded the layer's alpha entirely (a translucent multiply
  over a transparent backdrop vanished; over an opaque one the delivered
  alpha ignored the layer's opacity): the translucent-multiply oracle
  (11,20,20,192) pins the src-over alpha byte-exact.

The Screen factors were corrected during oracle validation: the first
draft used (ONE, ONE_MINUS_DST_COLOR), which computes `t + b·(1−b)` — not
the screen formula — and the device byte oracle caught the mismatch
([165,151,125] produced vs [154,141,140] hand-computed). The shipped
(ONE_MINUS_DST_COLOR, ONE) computes `t·(1−b) + b`, the screen formula.

The renderer prebuilds one pipeline variant per implemented mode (five;
N ≤ 16) sharing the layout, and binds the layer's variant per draw. The
`Scene.getLayer` proxy's read/write `blendMode` maps through this table:
writing 0/1/6/7/9 selects the mode; writing 11/12/24/30 clamps to Normal
with the same bounded diagnostic; any other value clamps silently.

### brightness and tint

| Field | File key | Default | Effect |
|---|---|---|---|
| `brightness` | `brightness` | `1.0` | multiplies the sampled RGB. The default 1.0 is the identity — a WE-verified fact (the OWE WPImageObject default); the clamp range `0.0..=10.0` is a design decision (dimming to black, up to a 10x boost), not a documented WE bound. Out-of-range values clamp, non-finite → 1.0 |
| `tint` | `tint` (alias `color` — the WE file key, vec3 or vec4; `tint` wins when both are present) | `[1,1,1,1]` | multiplies the sampled RGBA; per-component clamped to `0.0..=1.0`, non-finite → 1.0; a vec3 implies alpha 1.0 |

Both parse property-wrapped (the corpus editor form) and both clamp
out-of-range VALUES instead of rejecting — a too-bright effect darkens or
boosts a layer, never fails the scene. Wrong-TYPED values do reject like
alpha, with one corpus-honest exception: `brightness` accepts a JSON
number or a numeric string (the corpus editor serializes scalars as
strings); `tint` components must be numbers inside an array or
space-separated string. The tint alpha is folded into the pushed `m1.w`
host-side, so the shader's single multiply `a · layer_alpha · tint.a`
covers both.

Byte-exact oracle (llvmpipe, `scripts/smoke-scene.sh`): fullscreen texel
(64,103,142) over opaque clear (102,64,26) at the frame center (80,45) —
normal (142,103,64,255); multiply (14,26,26,255); add (168,167,166,255);
screen (154,141,140,255); subtract (0,0,38,255); effects (brightness 2.0,
tint (1, 0.4, 0.5)) (142,82,128,255); and add at layer alpha 0.5 over a
transparent clear pins the single premultiplication: the attachment stores
the straight composite (64,103,142,128) — alpha 0.5·255 = 127.5 rounds to
128 — and the readback premultiplies exactly once (71,52,32,128). The
translucent-multiply pin: multiply at layer alpha 0.5 over a 0.5-alpha
clear — the hard multiply stores (14,26,26) and the src-over alpha
0.5 + (128/255)·0.5 = 191.5 rounds to 192 — delivered (11,20,20,192): the
layer's own opacity survives, and the readback premultiplies exactly once.

## Text layers (M3e)

Text is a WE scene object family on par with images (the WE `Text`
objects; the OWE reference renders them through a dynamic glyph atlas of
quads — the architecture mirrored here). **Corpus reality**: the corpus
carries zero textures and zero known text layers — no real wallpaper
exercises text — so the implementation is validated with synthetic
fixtures (the `scripts/smoke-scene.sh` M3e lanes) and unit tests over
synthetic font directories via `--font-dir` / `KWE_FONT_DIRS`, never with
byte-pinned real-content renders.

### Researched reference facts

- `pointsize` is the WE file key for the font size, in points; **`fontsize`
  is not a WE key** (tolerated as an unknown key, never parsed). OWE's
  `TextPointSizeToPx` multiplies by `kPointsizeToPx = 4.0` and clamps to
  1..=1024 px. We multiply by the same 4.0 and clamp to **4..=512 px**
  (documented deviation: `MIN_FONT_PX`/`MAX_FONT_PX` — stricter bounds keep
  a single glyph comfortably inside the atlas).
- Alignment defaults to **center/center** in the original (OWE resolves
  `horizontalalign` → `alignment` → center); our parse mirrors the
  defaults.
- WE system fonts are addressed as `systemfont_<Family>` in scenes; the
  prefix is stripped before resolution.
- `text` and `font` are property-wrapped in the corpus editor form
  (`{user, value}`).
- WE text objects carry a `color` (vec3 or vec4), `alpha` and `brightness`
  like image objects; the color tints the glyphs.
- `font` accepts a family name, a `systemfont_` alias, or a path
  (absolute, or a basename matched against the scanned files).

### Font resolution order

One resolver per worker, cached per normalized family (alphanumerics
lowercased). Font sources: explicit `--font-dir` / `KWE_FONT_DIRS`
entries first, then `/usr/share/fonts`, `/usr/local/share/fonts`,
`~/.local/share/fonts`, `~/.fonts` — bounded (16 dirs, depth 4, 4096
files per dir, 16384 resolved files, 64 MiB per file) and sorted for
determinism. Resolution:

1. **Exact** — among basename candidates, the first whose name-table
   family matches. Verified via the vendored stb_truetype name records
   (decoded by the C shim: Windows/Unicode records are UTF-16BE, Mac
   records single-byte), bounded to 32 opens (`MAX_FAMILY_VERIFY`).

stb_truetype does no range checking of its own, so every candidate file
is **sfnt pre-flight validated by the shim before any stb call**: the
offset table (tag, table count) and each table record's `offset+length`
must lie inside the file (u64 math), ttc collection offsets must
resolve inside the buffer, and where cheap (`maxp`/`loca`/`glyf`) the
glyph ranges are checked before outline rasterization — hostile or
truncated fonts are rejected at open (`Font::open` → None), never
parsed. (Because the directory collection stops at a per-directory cap
before opening anything, a pathological font directory cannot force a
huge allocation either.)
2. **Basename** — the first unverified basename-prefix candidate in
   sorted order (WE-style basename matching; may be a CJK or condensed
   variant).
3. **Fallback chain** — ["Noto Sans", "DejaVu Sans", "Liberation Sans",
   "FreeSans"], each through steps 1–2; the resolution is reported as a
   fallback so the load-time diagnostic records the order.
4. **Any** — the first file that parses, in scan order.
5. **None** — the layer renders nothing; one bounded diagnostic per layer
   (`event=renderer.scene.text_font_none layer=... requested=...`).

A `font` written as a path resolves directly when it exists, else by
basename against the scanned files. Daemon-spawned workers carry fixed
args, so daemon lanes resolve real system fonts only; the synthetic-font
lanes (`--font-dir`, unit tests) drive the order above end-to-end.

### Implemented subset (M3e)

- **scene.json keys**: `text` (required to classify the object as a text
  layer; an object with `image` is an image layer — a `.json` image ref
  stays a model, M3h — and one with `text` *and* `image` is an image
  layer with the text counted), `font`, `pointsize`, `horizontalalign` /
  `verticalalign` (with `alignment` accepted for the horizontal, like the
  OWE chain), `color` (RGB/RGBA, property-wrapped or plain). Missing or
  blank `text` renders nothing. A scene-written `size` on a text layer is
  **ignored** (counted, one-time diagnostic): text renders at its
  automatic layout size with `size` pinned to (1,1), so layout pixels map
  1:1 to scene units; resizing happens through `scale` like every other
  layer.
- **Common properties** (origin, angles, scale, alpha, visible,
  blendMode, brightness, tint) inherit the M3c/M3d path unchanged,
  including the M3d alpha policy: the text color's alpha is folded into
  the layer alpha (`m1.w`) exactly like a tint.
- **Rendering**: one 2048×2048 RGBA8 glyph atlas per text layer
  (16 MiB), shelf-packed, white glyphs (RGB=255, coverage in alpha) with
  the text color riding the draw's tint slot — **zero shader changes**:
  the M3d fragment shader multiplies the sampled RGB by the tint and the
  alpha by the layer alpha, so glyph interiors land exactly in the text
  color and antialiased edges blend toward the background. Glyph quads
  span the unpadded metric box while their UVs are inset by the 1 px
  atlas pad, so glyph texels map 1:1 onto the quad. An overflow
  triggers a clear+repack, rate-limited to 2/s
  (`event=renderer.scene.text_atlas_rebuild_rate_limited`); font and
  pointsize changes run the same clear+repack **through the same
  budget** — a change that lands inside the 2/s window applies on the
  next sync with the previous atlas and geometry kept consistent
  meanwhile, so a 60 fps pointsize toggle cannot force a full repack
  per frame (only the initial load bypasses the budget); a glyph
  larger than 520 px is skipped
  (`event=renderer.scene.text_glyph_too_large`). Each text layer draws as
  one `DrawKind::Text` — `vertex_count` vertices (6 per glyph quad), one
  TRIANGLE_LIST draw — through a per-layer host-visible vertex buffer
  (created or grown, max 393 216 bytes). The atlas uploads through the
  existing image upload path and is **counted in the shared 256 MiB
  texture budget** at upload (16 MiB per layer; 16 text layers = the
  cap, first-come-first-served with image textures; one bounded
  `text_atlas_budget_skip` when the budget is already exhausted, with a
  byte refund if the upload fails); dirty state is synced before the
  initial render and each NewFrame, regenerating geometry only on text /
  alignment / font-size change.
- **JS surface** (`Scene.getLayer` on a text layer): read/write `text`
  (string; a write truncates to 4096 chars with
  `event=renderer.scene.text_truncated` and rebuilds the geometry),
  `pointsize` (clamped 4..=512 px; non-finite/≤0 → the default 12 pt →
  48 px), `horizontalAlign` / `verticalAlign` (0/1/2 = left|top /
  center / right|bottom, clamped), `color` (read/write `{r,g,b,a}`
  0..=1 per component — the scene.json color, writable through
  `layer.color.r = x` style sub-property writes, clamped per component
  like every other write path and tinting the glyphs; alpha folds into
  the layer alpha), plus all common layer properties. Text-only
  properties are only defined on text layers: on image layers they read
  `undefined` and writes reach no renderer state — never shared state.
- **Diagnostics** (one per layer, bounded): `text_font_fallback`,
  `text_font_none`, `text_truncated`, `text_atlas_rebuild_rate_limited`
  and `text_glyph_too_large` (each printed once per layer), and
  `text_atlas_budget_skip` (once per worker when the shared texture
  budget is exhausted at upload time).

### Bounds

| Bound | Value | Behavior |
|---|---|---|
| text layers per scene | 16 (`MAX_TEXT_LAYERS`) | further text objects are skipped (counted), never a rejection |
| text length | 4096 chars (`MAX_TEXT_CHARS`) | script writes truncate; scene.json longer strings truncate with the same diagnostic |
| font size | 4..=512 px (`MIN_FONT_PX`..`MAX_FONT_PX`), default 12 pt × 4 = 48 px | out-of-range and non-finite clamp to the default at the parse |
| point→px | × 4.0 (`POINT_TO_PX`, the researched WE multiplier) | rounded, then clamped |
| atlas | 2048² RGBA8 per layer, shelf-packed; each 16 MiB counts in the shared 256 MiB texture budget (first-come-first-served with image textures) | clear+repack rate-limited to 2/s — overflow and font/pointsize changes alike; one bounded `text_atlas_budget_skip` when the shared budget is exhausted |
| glyph bitmap | ≤ 520×520 px | larger glyphs are skipped per layer, never fatal |
| font file | 64 MiB | over-budget files are skipped by the resolver |
| font scan | 16 dirs, depth 4, 4096 files/dir, 16384 files | deterministic (sorted); directory entries are collected with an early stop at the per-dir cap, before any file is opened |
| family verify | 32 opens (`MAX_FAMILY_VERIFY`) | the Exact step never walks the whole corpus; past it, the Basename step returns the first candidate **unverified** — a family served by many files (CJK or condensed variants) can win over the regular face when the regular face sorts late |
| sfnt pre-flight | offset table, table records, ttc offsets, and (where cheap) maxp/loca/glyf ranges validated in the shim before any stb call | hostile or truncated fonts are rejected at open (`Font::open` → None), never parsed |
| atlas rebuild budget | 2/s (`ATLAS_REBUILDS_PER_SECOND`), 1 s window | overflow and font/pointsize changes all ride the budget; the initial load is the only unbounded path |

## Particle systems (M3f)

Particle systems are a WE scene object family on par with images and
text (the OWE reference renders emitters as point-quad instanced draws
with per-particle color/size interpolation over life; the architecture
mirrored here, with the instancing flattened into one batched vertex
buffer per system). M3f implements the **flat emitter model**: every
property is a scene.json key or a script-visible scalar, the simulation
advances on a fixed 1/60 s timestep, and each system draws through a
per-mode blend pipeline — the M3d semantics unchanged. **Corpus
reality**: the corpus carries zero particle systems (and zero textures —
an emitter needs one), so the implementation is validated with synthetic
fixtures (the `scripts/smoke-scene.sh` M3f lanes) and unit tests, never
with byte-pinned real-content renders.

### Researched reference facts

- WE emitters are configured in the file through a nested `particle`
  object; `maxcount` is the WE key for the live-particle cap, default
  **100** (we default to 1000 and clamp 1..=4096 — documented deviation:
  a 100 default starves every smoke fixture and hides over-spawn bugs).
- WE emitters carry **no `direction`/`spread` fields**: launch velocity
  comes from the velocity-random initializer (`Initializer.VELOCITY`),
  which is not file-configurable. The flat `direction` (radians from +x,
  y down) + `spread` (0..=2π cone) model is the M3f extension the
  deterministic smoke oracles need (documented deviation).
- The WE script surface is `IParticleSystem` (object accessors, plus
  `play()` = resume emission, `pause()` = emission off with live
  particles still simulating, `stop()` = clear immediately, `isPlaying()`
  = emitting or alive, `emitParticles(count)`, default count 1, works
  while stopped) and `IParticleSystemInstance` with the factors
  `count`, `speed`, `lifetime`, `size`, `alpha`, `rate`, `colorn` (the
  intentional WE spelling), each defaulting to 1.0; non-finite values
  clamp to 1.0 and the range clamps to [0, 1e6] (alpha/colorn to [0, 1]).
- WE emitter texture field is `material` (the brief's `texture` wins when
  both are present), resolved through the M3c image-source chain
  including the package table.
- WE blend modes map 0/1/6/7/9 = Normal/Multiply/Add/Screen/Subtract
  (the M3d table); `blendMode` and `colorBlendMode` are both accepted.
- WE randomness is per-frame and non-deterministic; M3f replaces it with
  one splitmix64 stream per system, seeded by the system index — a
  documented deviation. The stream itself is deterministic (the same
  spawn sequence repeats in the same order), BUT the fixed-step schedule
  derives from wall-clock dt, so the scene's live population depends on
  real time, not on the scene alone. Spread-0 systems never touch the
  stream at all (their trajectories are exact — what the range oracles
  rely on).

### scene.json keys (the `particle` object)

All keys in the table below live **inside** the `"particle"` dict
(`objects[i].particle.*`) except the shared-props row (`blendMode`,
`alpha`, `brightness`, `visible`), which sit on the object **beside**
`particle` like every WE object (`objects[i].blendMode` — the M3c/M3d
common path, corpus: `colorBlendMode` on image objects; `material` is
inside the dict per the WE texture field).

Missing keys take the defaults; scalar fields clamp (out-of-range →
clamp bound, non-finite → default); vector fields (`gravity`,
`colorStart`, `colorEnd`) reject non-vector shapes like every WE vector.
`speedMin`/`speedMax` win over a bare `speed`; a missing `speedMax`
falls back to the resolved minimum, and a reversed pair normalizes
(min ≤ max) — the runtime picks launch speeds uniformly in [min, max].

| Key | Range | Default | Notes |
|---|---|---|---|
| `spawnRate` | 0..=4096 /s | 10 | integer or numeric string |
| `life` | 0.1..=60 s | 1.0 | |
| `speed` / `speedMin` / `speedMax` | 0..=1e6 px/s | 0 | the pair supersedes `speed`; reversed pairs normalize |
| `direction` | ±1e6 | 0 | radians from +x, y down (M3f extension, see research notes); clamped in f64 BEFORE the f32 cast — a huge finite value like 1e300 must never overflow to f32::INFINITY (sin/cos of infinity is NaN, permanently poisoning the system) |
| `spread` | 0..=2π | 0 | all particles take the exact direction at 0 |
| `gravity` | ±1e6 px/s², 1..=3 components | [0, 0] | `[g]` → `[0, g]` (y down); extra components dropped |
| `sizeStart` / `sizeEnd` | 1..=512 px | 8 | interpolated over life |
| `colorStart` / `colorEnd` | RGBA 0..=1 each | white | vec3 implies alpha 1; interpolated over life |
| `alphaStart` / `alphaEnd` | 0..=1 | 1 → 0 | particles fade out by default |
| `maxCount` | 1..=4096 | 1000 | the WE key; integer or numeric string (floats reject like every integer key); excess spawns **drop**, never evict live particles (documented deviation from WE's 100) |
| `texture` / `material` | M3c image source | none | `texture` wins when both present; a non-string is None — the system registers and simulates but draws nothing |
| `blendMode` / `colorBlendMode` | M3d enum | Normal | pre-clamp; the runtime clamps to the implemented set like every layer |
| `alpha` / `brightness` / `visible` | M3c/M3d common | 1 / 1 / true | drawn effects; read-only through the script surface in M3f (the instance factors are the script knobs) |

Shared properties (`origin`, `angles`, `scale`, `alpha`, `visible`,
`blendMode`, `brightness`, `tint`) parse through the M3c/M3d path, but
`angles`/`scale` are **not applied** in M3f — particle systems render
world-space with `origin` only (documented deviation; the system
transform is planned).

### Simulation model

One `ParticleSystemState` per registered system, stepped every frame
with the frame's dt (`sync_particles`; also seeded with one pacing
interval at load so the first published frame already shows particles):

1. **accumulate** — `rate`-scaled dt (the WE simulation-rate factor),
   capped at 1.0 s (60 steps) per frame, `MAX_FRAME_DT` 1.0 s wall per
   frame; a stalled frame never unstretches.
2. **spawn** — only while emitting or a burst is pending (`play()` /
   `pause()` / `stop()` / `emitParticles(count)`, WE semantics); the
   per-step due count is `spawnRate × count × h`, floored, excess
   **dropped** (never evicted, one bounded
   `event=renderer.scene.particles_capped` per system), accumulator
   capped at 65536.
3. **integrate** — explicit Euler: `v += g·h; x += v·h; age += h`.
4. **retain** — particles with `age ≥ life` die (step 3's birth step
   order makes a particle born at step s sit at `(n−s+1) × speed × h`
   after n steps).

Size, color and alpha interpolate over normalized age
(`age / life`, clamped), so a `sizeStart 8, sizeEnd 4, alphaStart 1,
alphaEnd 0` particle shrinks and fades linearly. Determinism: every op
is f32 fixed-step; spread-0 trajectories are exact (no RNG), spread
systems use the per-system splitmix64 stream seeded by system index.

### Rendering

Each system owns one host-visible vertex buffer — **create-or-grow**:
uploaded on the first draw, grown when the vertex count exceeds the
current capacity, never shrunk. The buffer CONTENTS are rebuilt AND
re-uploaded every frame a fixed step ran (that is how often the live
particles can have changed — the sim's step order; a frame without a
step leaves the buffer untouched). The rebuild writes 6 vertices per
particle (an axis-aligned quad expanded around the center,
`tr/tl/bl/br` UVs covering the full texture), 40-byte stride with
per-particle color and size folded into the vertex attributes
(`shaders/particle.vert` / `particle.frag` — the M3f shader pair), via
one scratch Vec the worker reuses across systems and frames (no
per-frame allocation churn). Blend modes reuse the M3d per-mode pipeline
variants (≤ 16 pipelines, one per mode); the texture rides slot
`MAX_LAYERS + system_index` (272 slots total). A missing, over-budget or
undecodable material skips the system's draw at load (`particle_skip`,
never fatal — the system still simulates), and a vertex-upload failure
is contained the same way.

### Bounds

| Bound | Value | Behavior |
|---|---|---|
| particle systems per scene | 16 (`MAX_PARTICLE_SYSTEMS`) | further `particle` objects are skipped (counted, `event=renderer.scene.particle_system_skip count=...`), never a rejection |
| live particles per system | 4096 (`MAX_PARTICLES`) | `maxCount` clamps to it; excess spawns drop, never evict; one bounded `particles_capped` per system |
| timestep | 1/60 s (`FIXED_STEP`) | fixed-step accumulator for oracle determinism |
| sim time per frame | ≤ 1.0 s (`MAX_ACCUMULATED_SIM_SECONDS` = 60 steps) | hostile `rate` factors can never stall the frame |
| wall dt per frame | ≤ 1.0 s (`MAX_FRAME_DT`) | a stalled frame is dropped, not stretched |
| spawn accumulator | 65536 due particles/step (`MAX_SPAWN_ACCUMULATOR`) | excess dropped, bounded |
| vertex buffer | 4096 × 6 verts × 40 B ≈ 983 KiB per system | one host-visible buffer, create-or-grow; contents rebuilt + re-uploaded every frame a fixed step ran |
| texture slots | `MAX_LAYERS` (256) + 16 particle slots = 272 | the M3c texture budget still applies per upload |

## Image sources (M3c)

- **File scenes**: the reference is resolved against the canonicalized
  scene directory (symlink-escape-validated, exactly like scripts):
  relative with no `..`/absolute components, a regular file, at most
  `MAX_TEXTURE_SOURCE_BYTES` (64 MiB).
- **Pkg scenes**: the reference names a package entry (`kwe_core::image_entry`,
  case-insensitive — the literal path or the entry's tail after a `/`,
  exactly one match), read through the bounded reader; the host file
  system is never touched.

A missing, escaping, unreadable, or over-budget image **skips its layer**
with a bounded one-time diagnostic (`event=renderer.scene.layer_skip
layer=...`) — never the scene: the renderer stays healthy and the other
layers render. The same skip covers undecodable files and the total
texture budget.

Decoding (the `image` crate — see THIRD_PARTY.yml): PNG and JPEG always,
WebP when the crate builds with its webp feature. Bounds (textures.rs):
dimension ≤ 8192, pixels ≤ 16,777,216, decoded ≤ 64 MiB per texture,
source ≤ 64 MiB per texture, ≤ 256 MiB total across layers. Decoded
textures are RGBA8, uploaded as R8G8B8A8_UNORM (identity channel order —
the M3a readback lesson) with a shared linear clamp-to-edge sampler and a
per-layer descriptor set (pool capped at 256 sets).

## scene.pkg

**Implemented (M3b)** in `kwe-core` (`crates/kwe-core/src/pkg.rs`) and wired
into the worker's `--content` path: a `.pkg` content is opened by
`PkgReader`, its unique `scene.json` entry is parsed in memory, and — when
`general.script` names a package entry — that entry is extracted into a
private `kwe-scene-script-<pid>` directory under the worker's HOME (mode
0700) and loaded like a file scene's script. Textures, models, and other
assets are **M3c+** and are deliberately not extracted; the renderer logs
`event=renderer.scene.pkg entries=N script_entry=...`.

The **extension selects the reader**: `--content` ending in `.json` takes
the file-based parse, `.pkg` takes the archive reader — and mislabeled
content fails *as its labeled format* (a pkg renamed `.json` is parsed as
JSON and rejected, and vice versa).

The extracted script directory is the worker's own: it is removed on the
worker's graceful exit path, and a stale directory left by a hard kill is
replaced by the pid-recycle retry on the next start (never a brick).

### Verified layout

Triple-confirmed against byte-level inspection of ~60 real Workshop scene
packages (20 distinct `PKGV` versions, 3128 entries), the public QuickBMS
extractor script (0.1a), and the BSD-3-licensed RePKG implementation
(behavior references only per ADR 0001 — no code copied):

```text
u32 LE  magic-string length in bytes (8 on the corpus)
bytes   magic string: b"PKGV" + 4 ASCII digits, e.g. "PKGV0001"
u32 LE  entry count
  per entry:
    u32 LE  path length in bytes
    bytes   UTF-8 path, e.g. "scene.json"
    u32 LE  payload offset, relative to the start of the data section
    u32 LE  payload size in bytes
data section: raw concatenated payloads
```

Offsets are relative to the data section (right after the table), and
payloads are stored **raw**: the corpus contains no compressed entry
(JSON descriptors, TEXV0005 textures, and raw pixel data only — verified
with an independent pure-Python LZ4 block decoder), and none of the
reference implementations decompresses anything. The QuickBMS script notes
"PKGV0001, PKGV0006 and so on are all the same format"; the layout is
version-independent, so any `PKGV` + 4 digits is accepted.

### The LZ4 question (honest variant note)

The M3b brief described "LZ4-compressed payloads in the commonly documented
format". The evidence above **disproves that premise** for every package we
can see, so raw is the primary path. To cover the possibility that some
publisher-side tool produced frame-compressed packages, the reader
additionally recognizes the LZ4 frame magic (`04 22 4D 18`) at a payload's
start and decompresses it — with the output cap (64 MiB per entry)
enforced **during** decompression (the frame decoder is wrapped in
`take(cap + 1)`; a declared content size or a bomb can never allocate past
the cap). A payload whose first four bytes are not the frame magic is
returned verbatim. **Raw fallback policy**: a payload that *does* begin
with the frame magic but does not decode as a frame is treated as raw
instead of failing the read (raw is the corpus-proven primary) — one
bounded diagnostic line per fallback; an over-cap decompression is never
downgraded, so a bomb stays a `bounds` error. The `compressed` flag on
`PkgEntry` reports which path a given entry takes.

### Bounds and validation

| Bound | Value |
|---|---|
| package size | 512 MiB |
| entry count | 65 536 |
| entry path | 512 bytes |
| entry payload | 64 MiB (read-time cap, before and during decompression) |
| total payload | 512 MiB (checked while parsing the table) |

The whole table is validated **at open**, before any payload is touched:
magic/version shape (structured `unsupported version` error for a
PKGV-prefixed magic that is not 4 digits), entry count, per-path length and
UTF-8, ranges (`offset + size` inside the data section, checked overflow),
total payload sum, and path safety. The open is TOCTOU-safe like every
other read in kwe-core: lstat (reject symlinks), `O_NOFOLLOW` open, fstat
re-check on the fd, parse from the fd, size re-check after parsing. All
reads stay pinned to the fd.

### Path-traversal policy (documented decision)

M3b ships `read_entry` only — no extract-to-disk API — so a hostile entry
path cannot write outside the package on this slice. The table is still
validated at open: **empty paths, NUL bytes, backslashes, absolute paths,
and `..` components are rejected** (`PathTraversal`), so a future
extractor cannot inherit a hostile table. Callers that resolve entry paths
(the worker's script extraction) additionally confine resolution: the
script reference must be relative, `.js`, and match exactly one entry
(case-insensitive, literal or `/<name>`-suffixed); the extracted file is
always written as `script.js` under a pid-unique 0700 directory the worker
owns. Nothing from a package is ever resolved against the host file
system.

### Worker behavior

* `--content` ending in `.pkg` (case-insensitive) selects the archive path.
* Exactly one entry named exactly `scene.json` (case-insensitive) is
  required, with at most one leading directory component (`scene.json`,
  `dir/scene.json` — `myscene.json` and `a/b/scene.json` do not count):
  zero matches with a `scene.pkg` entry present (same name rule) means a
  **nested archive** (`event=renderer.scene.backend_reject kind=Pkg
  detail="nested scene.pkg inside the package is not supported (M3b)") —
  nested packages are refused, not recursed; zero matches otherwise, or
  several matches, are likewise exit 73. The rule lives in
  `kwe_core::scene_json_entry`, shared with preflight.
* The scene.json entry is read bounded to 16 MiB and parsed by the same
  core as file scenes (unknown keys tolerated, `general` rules identical).
* `general.script` resolves against the package table (same rule as the
  file lane: relative, `.js`, no `..`/backslash/NUL, exactly one match —
  `kwe_core::script_entry`); the entry is read bounded to 2 MiB, extracted,
  and loaded by the script engine. A script reference that is empty, `.pkg`,
  non-`.js`, absolute, traversing, missing from the table, or ambiguous is
  a backend rejection (exit 73).
* Archive failures (corrupt magic/table, truncated data, bounds, traversal
  entries) are backend rejections: `kind=Pkg`, exit 73 before the canary,
  so the supervisor records `exit_code_73` and rolls back. Preflight
  (kwe-core `preflight_scene`) runs the same structural validation for a
  `.pkg` content and rejects a corrupt archive before the worker spawns
  (this closes M1 finding G12, which previously let any `.pkg` through).
  Preflight also checks the renderer's per-entry caps **statically**, with
  the same resolution rules: the scene.json entry must be ≤ 16 MiB and the
  referenced script entry ≤ 2 MiB, so an oversized entry is refused as
  `invalid_params` instead of bouncing workers (exit 73). The script check
  reads the descriptor's stored bytes (never decompressed — preflight
  stays structural; a compressed descriptor skips the check and the
  renderer's bounded decode still enforces the cap).

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
| `Scene.getLayer(name \| index)` | **implemented (M3c)** | returns the `Layer` proxy for a registered layer (image or text), or `null` for an unknown name/index (never throws); layers are registered in `objects` order |
| `Scene.getLayerCount()` | **implemented (M3c)** | the number of registered layers (image and text) |
| `SceneLayer` (`Layer`) | **implemented (M3c, M3d)** | read+write proxy: `name` (read-only string, matching the reference behavior), `alpha` (0..1), `visible` (boolean), `angles` `{x, y, z}` (degrees), `origin` `{x, y}` (scene units, layer center), `scale` `{x, y}`, `size` `{x, y}` (scene units; an absent size is the decoded texture's dimensions, so init() sees the real size). M3d adds `blendMode` (0/1/6/7/9 select the researched modes; 11/12/24/30 clamp to Normal with a bounded diagnostic; anything else clamps silently), `brightness` (0..=10), and `tint` `{r, g, b, a}` (0..=1 each). Writes are clamped like `Engine.clearcolor`: non-finite → 0 (effects → their default 1.0), alpha to 0..=1, scalars to ±1e6, size to ≥ 0 (scale carries the mirror). Changing `image` at runtime is *planned* (M3d+) — an image-less layer registered via `Scene.getLayer` is fully readable/writable except for its texture |
| color effects (`brightness`, `tint`) | **implemented (M3d)** | the effects apply to the sampled texel before blending; clamps absorb any out-of-range write |
| text layers | **implemented (M3e)** | see the Text layers table below |
| particles | **implemented (M3f)** | see the Particles table below |
| `VideoLayer` objects | **implemented (M3g, partial)** | an object with a `video` reference and no `image` is decoded by a supervised libmpv software core; at most two cores are open, native decoded size fills an absent `size`, `loop` and bounded `rate` are accepted, and bad/capped sources skip only that layer. Local file/package containment, protocol whitelist, 160 MiB source cap, `audio=no`, and no scripts/network are intentional semantics. SceneScript per-layer video controls remain planned. |
| 3D models, user properties | *planned* (M3h–M3k) | the parse tolerates extra keys but renders none of them |

### Text layers (M3e)

| API | Status | Notes |
|---|---|---|
| `Layer.text` | **implemented (M3e)** | read/write string; a write truncates to 4096 chars (one bounded `text_truncated` diagnostic per layer) and rebuilds the geometry |
| `Layer.pointsize` | **implemented (M3e)** | points ×4 → px (the researched WE multiplier), clamped 4..=512; non-finite/≤0 → the default 12 pt (48 px) |
| `Layer.horizontalAlign` / `verticalAlign` | **implemented (M3e)** | 0/1/2 = left\|top / center / right\|bottom, clamped; the parse defaults to center/center (the OWE `horizontalalign` → `alignment` → center chain) |
| `Layer.color` | **implemented (M3e)** | read/write `{r, g, b, a}` 0..=1 per component (clamped at the parse and on every script write; RGB implies alpha 1); the alpha folds into the layer alpha (M3d policy), the RGB rides the draw's tint slot |
| font resolution | **implemented (M3e)** | the Exact → Basename → fallback chain → Any → None order documented in "Text layers (M3e)" above; fallback/none reported once per layer (`text_font_fallback`, `text_font_none`) |
| glyph atlas | **implemented (M3e)** | 2048² per layer, white glyphs with coverage in alpha — zero shader changes; overflow clear+repack rate-limited to 2/s |
| `Layer.size` on a text layer | ignored | text size is automatic (layout pixels map 1:1 to scene units); the write is counted (`text_size_ignored`), resizing goes through `scale` |
| text-bearing wallpapers | *planned* | the corpus carries **zero** text layers and zero textures — nothing real to validate against; M3e is exercised with synthetic fixtures only |
### Particles (M3f)

| API | Status | Notes |
|---|---|---|
| `Scene.getParticleSystem(name \| index)` | **implemented (M3f)** | returns the `ParticleSystem` proxy for a registered `particle` object, or `null` for an unknown name/index (never throws); the WE-compatible `Scene.getLayer` path reaches particle systems by name too |
| `Scene.getParticleSystemCount()` | **implemented (M3f)** | the number of registered particle systems |
| `ParticleSystem.spawnRate` / `life` / `speedMin` / `speedMax` / `direction` / `spread` / `sizeStart` / `sizeEnd` / `alphaStart` / `alphaEnd` / `maxCount` / `blendMode` | **implemented (M3f)** | read/write scalars, clamped on the Rust side like every proxy write (the scene.json ranges above; non-finite → default, out-of-range → the clamp bound) |
| `ParticleSystem.instance` | **implemented (M3f)** | the WE `IParticleSystemInstance`: `count`, `speed`, `lifetime`, `size`, `alpha`, `rate`, `colorn` — multiplicative factors, default 1.0, non-finite → 1.0, clamped [0, 1e6] (alpha/colorn [0, 1]); `count` scales the spawn rate (the smoke-d lane pins it), `lifetime` scales life, `rate` scales the sim time |
| `ParticleSystem.play()` / `pause()` / `stop()` / `isPlaying()` / `emitParticles(count)` | **implemented (M3f)** | WE semantics: play resumes emission, pause stops emission with live particles still simulating, stop clears immediately, `isPlaying()` = emitting or alive, `emitParticles` (default count 1) bursts even while stopped |
| `ParticleSystem.alpha` / `brightness` | **implemented (M3f)** | read/write like every layer (0..=1 / 0..=10); the drawn effects (read-only in M3f only in the sense that the instance factors are the documented knobs) |
| `particle`-object shared props | **implemented (M3f)** | `origin` positions the emitter; `angles`/`scale` parse but are **not applied** (documented deviation — systems render world-space; the transform is planned) |
| `texture` / `material` | **implemented (M3f)** | M3c image-source resolution (content root or pkg table); `texture` wins over `material`; missing/undecodable → the system simulates but draws nothing (`particle_skip`, never fatal) |
| emitter `direction`/`spread` | **implemented (M3f)** | the M3f extension (WE emitters have neither — launch velocity comes from the velocity-random initializer); radians from +x, spread 0..=2π cone |
| `maxCount` > 4096 / spawn excess | **implemented (M3f)** | excess spawns drop (never evict), one bounded `particles_capped` per system |
| particle-bearing wallpapers | *planned* | the corpus carries **zero** particle systems and zero textures — M3f is exercised with synthetic fixtures only |

| `.pkg` archives | **implemented (M3b)** | scene.json entry parsed in memory; script entry extracted to a private HOME dir; nested archives refused; **image entries resolve against the package table (M3c)** |
| image assets | **implemented (M3c)** | PNG/JPEG (+WebP) decoded from the content root (file scenes) or the package entry table (pkg scenes); a missing/undecodable/over-budget image skips its layer with a bounded diagnostic, never the scene |
| audio/pointer input in script | *planned* | the worker receives and acks the wire inputs but exposes none of them to the script until M3i |
| media input | **implemented (M3g transport only)** | latest-wins `playing`/`paused`/`stopped` fans out to open VideoLayers (stop pauses and seeks to zero); metadata is acknowledged but not exposed to SceneScript |

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

- docs/BETA_M3.md — the M3a..M3d slices: goal, acceptance evidence, exit
  codes, open risks (interrupt-budget deviation, llvmpipe determinism,
  reader staleness, loader lifetime).
- docs/adr/0001-original-vulkan-renderer.md — the architecture this slice
  implements (ADR 0001 is binding).
