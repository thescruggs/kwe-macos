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
rotate, fade, and hide layers at runtime. M3d adds the per-layer blend
modes and color effects; M3e adds text layers (a glyph atlas rendered
through the M3c compositor with zero shader changes); M3f adds particle
systems (a bounded fixed-timestep CPU simulation, one batched draw per
system, and the `Scene.getParticleSystem` script surface). The rest of
the scene surface (3D, user properties — M3h–M3k) and any manager
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
v2=(0.5,0.5,1,1), v3=(-0.5,0.5,0,1)) drawn as one 6-vertex
`TRIANGLE_LIST` draw — a standard draw of two primitives. (The original
half-quad bug was not the draw shape: a vertex-buffer element/byte mix-up
sized the buffer at 16 bytes — one vertex — so the GPU's reads of
vertices 2..5 ran out of bounds and the second triangle rasterized
garbage; fixed and pinned by the isolated_draw probe.) The vertex shader
applies **no y-flip**; frame orientation is an *empirical contract*:
delivered frames are upright on both tested drivers (NVIDIA RTX 3070 and
llvmpipe), pinned by the quad_orientation device test and the smoke
suite's layer oracles on **both** lanes (daemon + standalone llvmpipe) —
how an OPTIMAL-tiling color attachment comes back in the readback is
driver-dependent, so a new driver must be re-verified against those
oracles before it is declared supported.

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

Blending is src-over by default: color ONE / ONE_MINUS_SRC_ALPHA, alpha
ONE / ONE_MINUS_SRC_ALPHA. The fragment shader outputs straight color
(`outColor = vec4(c.rgb, c.a * pc.m1.w)`), so the attachment stores the
straight composite and the readback's premultiplication happens exactly
once, at the protocol boundary. Both factors must be ONE: SRC_ALPHA on
the color factor would store an already-premultiplied composite that the
readback then premultiplies AGAIN (translucent pixels α/255 too dark —
the review-fixed double premultiplication); SRC_ALPHA on the alpha factor
would double-scale the layer alpha (191/255 over an opaque destination
produced 143/255 instead of 191/255). Oracle, byte-exact on both
drivers: opaque texel (64,103,142,255) at layer alpha 191/255 over a zero
clear is delivered as premultiplied BGRA **(106,77,48,191)**. A
non-default `colorBlendMode` renders per the researched M3d table below:
the five decoded modes (0 normal, 1 multiply, 6 add, 7 screen, 9
subtract) bind the matching fixed-function pipeline variant; the four
undecoded values (11, 12, 24, 30) clamp to normal with one bounded
diagnostic per scene (`event=renderer.scene.blend_mode_clamped`).

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

Corpus findings (60 real packages, 685 image-bearing objects, re-scanned):
432 carry a `colorBlendMode` — 410×0, 6×11, 6×30, 4×6, 2×24, 1×1, 1×7,
1×9, 1×12 (sums to 432) — the 17 non-zero occurrences (11, 30, 6, 24, 1,
7, 9, 12) now render per the M3d table: 6, 1, 7, 9 decode to add,
multiply, screen, subtract; 11, 30, 24, 12 clamp to normal with a bounded
diagnostic. **620 image
references point at model `.json` files** (WE's animated-model format,
M3h) and 65 carry a null image value; **none point at a real texture**, so
no corpus wallpaper yet exercises the decoded-texture path. 70% (315/447)
of image layers carrying `alpha` and 49% (276/568) of those carrying
`visible` are property-wrapped; the M3c parser unwraps the
`{"user": ..., "value": ...}` wrapper at load time for image-layer fields
(user properties themselves are M3j), while the property-wrapped
*clearcolor* form (1 of 60) remains rejected per the M3b finding.

### M3d — blend modes and color effects (this commit)

Per-layer `colorBlendMode` renders through fixed-function Vulkan blending:
the draw loop prebuilds one pipeline variant per implemented mode (five of
the N ≤ 16 budget, sharing the layout) and binds the layer's variant per
draw. `Scene.getLayer` gains read/write `blendMode`, `brightness` and
`tint`. The two effects apply to the sampled texel in the fragment shader
**before** the pipeline's blend mode combines the result with the frame;
push constants grow 48 → 64 bytes (an `effects` vector is appended; the
first 48 bytes are byte-identical, so the vertex shader is untouched).

The researched WE table (full evidence in SCENE_FORMAT_V1.md): 0 Normal,
1 Multiply, 6 Add, 7 Screen, 9 Subtract. The wpdoc editor dropdown pins
the five-mode FAMILY {Normal, Multiply, Add, Screen, Subtract} (Steam
patch note "standard Photoshop blend modes") but says nothing about the
integers; with no public WE shader source, the value→name mapping is a
corpus-histogram hypothesis — 0 dominates (410×0) and is the editor
default (verified); 1/6/7/9 are the best fit for the four remaining
dropdown names. 11, 12, 24, 30 (15 corpus occurrences) sit OUTSIDE the
dropdown family — the original evidently tolerates integers its editor
cannot produce — and are not expressible in fixed-function blending: they
clamp to Normal with a bounded one-time diagnostic
(`event=renderer.scene.blend_mode_clamped layer=... mode=...`).

The alpha policy is deliberate: the mode acts on the COLOR; the alpha
channel always composites src-over (ONE, ONE_MINUS_SRC_ALPHA) — the
layer's own opacity still matters under every mode — except Add, which is
additive on both channels (ONE, ONE). This fixes the review finding that
Multiply's original (ZERO, ONE) discarded the layer's alpha (a translucent
multiply over a transparent backdrop vanished; over an opaque one the
delivered alpha ignored the layer's opacity) — pinned byte-exact by the
translucent-multiply oracle (11,20,20,192). Subtract is
`max(0, background − texel)` — REVERSE_SUBTRACT(ONE, ONE), the
background-minus-texel direction of Photoshop's Subtract (WE's stated mode
family) and of the Vulkan dst − src algebra; the reversed direction fails
the oracle. The Screen factors were corrected during oracle validation:
the first draft (ONE, ONE_MINUS_DST_COLOR) computes `t + b·(1−b)` instead
of the screen formula `t·(1−b) + b`; the device byte oracle caught it
([165,151,125] produced vs [154,141,140] hand-computed).

Effects: `brightness` (default 1, the identity — a WE-verified fact; the
clamp range 0..=10 is a design decision, not a documented WE bound)
multiplies RGB; `tint` (alias `color` — the WE file key, vec3 or vec4;
`tint` wins when both are present; default [1,1,1,1]; per-component
clamped 0..=1, non-finite → 1; vec3 implies alpha 1) multiplies RGBA. Both
parse property-wrapped and clamp out-of-range VALUES instead of rejecting;
wrong-TYPED values reject like alpha, except `brightness`, which accepts a
numeric string too (the corpus editor serializes scalars as strings). The
tint alpha is folded into the pushed layer alpha host-side
(`m1.w = alpha · tint.a`).

### M3d — acceptance evidence

| Case | Expected containment | Result |
|---|---|---|
| workspace gates | `cargo fmt --all -- --check`; `cargo clippy --workspace --all-targets -- -D warnings`; `cargo test --workspace --all-targets` | clean; 346 tests pass (92 in `kwe-scene-renderer`) |
| device tests (llvmpipe) | `KWE_TEST_DEVICE`-gated byte-exact renders: every implemented mode + the effects math | `blend_modes_composite_byte_exact`: texel (64,103,142) over clear (102,64,26) → normal (142,103,64,255), multiply (14,26,26,255), add (168,167,166,255), screen (154,141,140,255), subtract (0,0,38,255); `color_effects_composite_byte_exact`: brightness 2 + tint (1,0.25,0.5) → (142,51 or 52,128,255); `translucent_multiply_alpha_composites_src_over_byte_exact`: multiply at α 0.5 over a 0.5-alpha clear → (11,20,20,192) — all byte-exact |
| smoke 1–5 (daemon lane) | one scene per implemented mode, fullscreen texel over the opaque clear | (80,45) = (142,103,64,255), (14,26,26,255), (167,167,166,255), (153,141,140,255), (0,0,38,255) — tol 1; the RTX 3070 lands add/screen one byte under the exact llvmpipe values |
| smoke 6 (daemon lane) | brightness 2.0 + tint (1,0.4,0.5) | (80,45) = (142,82,128,255) |
| smoke 7 (daemon lane) | add at layer alpha 0.5 over a transparent clear — the single-premultiplication pin | (80,45) = (71,51,32,127) tol 1 — the RTX rounds the 0.5 alpha tie down to 127; the llvmpipe lane pins the RNE byte (71,52,32,128) exactly |
| smoke 10 (daemon lane) | translucent multiply — the alpha-policy pin | (80,45) = (11,20,20,192) tol 1 — the layer's own opacity survives the multiply |
| smoke 8 (daemon lane) | `colorBlendMode: 11` | renders normal (142,103,64,255); stderr ring carries `event=renderer.scene.blend_mode_clamped layer=layer mode=11` once |
| smoke 9 (daemon lane) | scripted switch: `update()` writes `blendMode = t < 3 ? 6 : 1` | the first sample POLLS until the add composite is observed (a slow lane must not sample before the first update() or after the switch); then, after t crosses 3 s, the multiply composite — two frames, one layer |
| standalone llvmpipe lanes | every implemented mode + effects + the alpha-128 and translucent-multiply cases + the scripted switch, EXACT bytes | normal (142,103,64,255), multiply (14,26,26,255), add (168,167,166,255), screen (154,141,140,255), subtract (0,0,38,255), add128 (71,52,32,128), multiply128 (11,20,20,192), effects (142,82,128,255), scripted add→multiply (polled) — all exact, tol 0 |
| clamp/parse/JS units | enum mapping (researched table), variant selection, JS write clamping, scene.json parse of blend mode + effects | `blend_mode_table_matches_the_researched_we_mapping`, `blend_mode_clamp_falls_back_to_normal_for_unknown_values` (asserts 11/12/24/30), `brightness_and_tint_clamps_are_bounded`, `brightness_and_tint_parsed_with_clamps` (incl. the numeric-string brightness form), `blend_mode_and_effects_writes_clamp_on_the_rust_side`, `blend_attachment_table_matches_the_researched_we_semantics`, `color_effects_math_matches_the_shader`, `blend_modes_recorded` |
| regressions | video + supervisor suites | `smoke-video.sh` exit 0 (deviation 2 ≤ 4) |
| plasmashell pid guard | no plasmashell touched | pid unchanged across the suite |

### M3e — text layers (this commit)

Text layers (`text` objects) render through one bounded glyph atlas per
layer (2048² RGBA8, shelf-packed, 16 layers per scene): a script-visible
layer draws as a single `DrawKind::Text` TRIANGLE_LIST of 6 vertices per
glyph quad from a per-layer host-visible vertex buffer, with the atlas
uploaded through the M3c image path. The atlas stores **white** glyphs
(RGB=255, coverage in alpha) and the text color rides the draw's tint
slot, so the existing M3d fragment shader (sampled RGB × brightness ×
tint, alpha × layer alpha) renders glyph interiors exactly in the text
color with antialiased edges that blend toward the background — **zero
shader changes**, and the M3d alpha policy applies to the text color's
alpha unchanged. Geometry is regenerated only on text / alignment /
font-size change (dirty state synced before the initial render and each
NewFrame); the font is resolved once per layer and cached per normalized
family. Glyph quads span the unpadded metric box while their UVs are
inset by the atlas pad, so glyph texels map 1:1 onto the quad.

Font rasterization is the vendored stb_truetype.h (public domain or MIT,
dual licensed upstream; pinned revision 6e9f34d5; a THIRD_PARTY.yml entry
covers the header and the researched-but-unused redox-os crate) behind an
opaque C shim
(`vendor/stb/stb_shim.c`, built by a cc build.rs): the Rust side never
sees stb's struct layout. The shim decodes the name-table records for
family matching — Windows/Unicode records are UTF-16BE (the ASCII family
names arrive as "N\0o\0t\0o\0..."), Mac records single-byte — and
refuses outlines over 65 536 vertices. stb_truetype does no range
checking of its own, so **every candidate file is sfnt pre-flight
validated in the shim before any stb call**: the offset table (tag,
table count) and each table record's `offset+length` must lie inside
the file, ttc collection offsets must resolve inside the buffer, and
where cheap (`maxp`/`loca`/`glyf`) glyph ranges are checked before
outline rasterization — hostile or truncated fonts are rejected at
open (`Font::open` → None), never parsed. `font` accepts a family name, a
`systemfont_` alias (prefix stripped), or a path; the resolution order is
Exact (name-table-verified, bounded to 32 opens) → Basename (first
unverified prefix match) → the fallback chain ["Noto Sans", "DejaVu
Sans", "Liberation Sans", "FreeSans"] → Any usable font → None (renders
nothing, one bounded `text_font_none` diagnostic). Font sources are the
explicit `--font-dir` / `KWE_FONT_DIRS` entries first, then the standard
per-user and system directories — so the daemon lanes resolve real system
fonts, while the synthetic-font lanes (unit tests, `--font-dir`) drive
the order deterministically.

Researched WE facts (full evidence in SCENE_FORMAT_V1.md): `pointsize`
is the WE key (points; OWE multiplies by `kPointsizeToPx = 4.0` and
clamps 1..=1024 px — we clamp **4..=512 px**, a documented deviation);
`fontsize` is **not** a WE key; alignment defaults to center/center (the
OWE `horizontalalign` → `alignment` → center chain); `text`/`font` are
property-wrapped in the corpus editor form. Text layers inherit every
common property (origin, angles, scale, alpha, visible, blendMode,
brightness, tint) through the M3c/M3d path, with `size` pinned to (1,1)
so layout pixels map 1:1 to scene units (a scene-written `size` is
ignored with a one-time count; `scale` does the resizing). The JS proxy
adds read/write `text` (a write truncates to 4096 chars with a bounded
`text_truncated` diagnostic and rebuilds the geometry), `pointsize`
(clamped 4..=512, non-finite/≤0 → the default 12 pt → 48 px),
`horizontalAlign`/`verticalAlign` (0/1/2, clamped) and a read/write
`color` (`{r,g,b,a}` 0..=1 per component — `layer.color.r = x` style
sub-property writes are clamped per component and tint the glyphs;
alpha folds into the layer alpha). Font and pointsize changes run the
clear+repack **through the same 2/s rebuild budget** as overflow: a
change that lands inside the budget window applies on the next sync,
keeping the previous atlas and geometry consistent meanwhile, so a 60
fps pointsize toggle cannot force a full repack per frame (only the
initial load bypasses the budget). Atlas overflow clears and repacks,
rate-limited to 2/s; a glyph over 520 px is skipped per layer — neither
is ever fatal.

**Corpus fact**: the corpus carries **zero textures and zero known text
layers** — no real wallpaper exercises text — so M3e is validated with
synthetic fixtures only: the smoke lanes below (generated at runtime,
never committed) and unit tests over synthetic font directories.

### M3e — acceptance evidence

| Case | Expected containment | Result |
|---|---|---|
| workspace gates | `cargo fmt --all -- --check`; `cargo clippy --workspace --all-targets -- -D warnings`; `cargo test --workspace --all-targets` | clean; 373 tests pass (119 in `kwe-scene-renderer`) |
| font resolution order | synthetic font dirs via `--font-dir` / `KWE_FONT_DIRS`: explicit dirs win, then basename, then the fallback chain | `resolution_order_prefers_explicit_dirs_then_family`, `resolver_bounds` (16 dirs / depth 4 / 4096 per dir / 16384 files / 64 MiB), `font_open_rejects_garbage` |
| hostile fonts | adversarial fixtures rejected in the shim's sfnt pre-flight, before any stb call | `hostile_font_stubs_are_rejected` (12-byte ttcf with numFonts=0xFFFFFFFF, ttcf offset past end, sfnt table count past the buffer, hmtx record offset = u32::MAX, loca past glyf, hmtx claimed length too short for numGlyphs — all open → None); `crafted_minimal_font_opens_and_renders_empty` (a minimal 7-table sfnt opens and renders an empty glyph) |
| glyph rasterization | stb path through the shim: 1-byte coverage, bounded box, refuse-oversize | `system_font_layout_and_rasterization` (rasterizes a real system font and counts covered pixels), `alignment_parsing` |
| quad layout math | 6 vertices per glyph, winding, bounds, UVs inset by the atlas pad | `vertex_bytes_match_unit_quad_winding` (UV expectations include the `ATLAS_PAD` inset) |
| atlas bounds | shelf packing, clear+repack on overflow, rebuilds rate-limited to 2/s — font/pointsize changes ride the same budget | `atlas_packs_and_evicts_with_rate_limit`, `atlas_rate_limits_rebuilds`, `atlas_overflow_repacks`, `atlas_rebuild_budget_covers_font_and_scale_changes` |
| atlas memory budget | each layer's 16 MiB atlas counts in the shared 256 MiB texture budget at upload; 16 text layers = the cap, first-come-first-served with image textures | one bounded `event=renderer.scene.text_atlas_budget_skip layer=...` when the budget is already exhausted; the byte count is refunded when an upload fails (atlas_bytes_used / budget_counted) |
| device pool reuse | re-uploading a layer destroys the replaced image/view/memory and frees its descriptor set — 320 re-uploads must not exhaust the 256-set pool | `KWE_TEST_DEVICE`-gated `texture_reuploads_replace_in_place_without_exhausting_the_pool`: `live_uploads` stays 1 after 320 re-uploads of index 0, then 2 when index 7 is uploaded |
| scene.json parse | defaults (12 pt → 48 px, center/center, opaque white), property-wrapped `text`/`font`, pointsize clamp 4..=512, alignment fallback like OWE, common props match image layers, 16-text-layer cap | `text_layers_parsed_with_defaults`, `text_layer_fields_parsed`, `text_pointsize_clamped_and_tolerant`, `text_alignment_falls_back_like_owe`, `text_common_props_match_image_layers`, `text_layer_caps_and_counts` |
| JS proxy | text/pointsize/align/color reads + clamped writes, truncation, dirty-rebuild | `text_layer_proxy_exposes_and_clamps_text_properties`, `text_proxy_writes_are_bounded`, `over_long_text_writes_are_truncated`, `text_writes_mark_dirty_and_rebuild_each_step` |
| smoke (a) llvmpipe lane | fixed string "SMOKE" + `font: "Noto Sans"` over the fullscreen blue clear — region oracle: text-colored pixels ≥ 300, pixels differing from the bg ≥ 400, mean-of-differing R-dominant | **foreground=742, differing=1983, mean=(R 245.4, G 0.0, B 120.2)** — the antialiased edges lean toward the blue background (documented in the script); glyph interiors are pure text color |
| smoke (b) daemon lane | script swaps `layer.text` at t ≥ 3.0 — two stable frames differ, foreground collapses to ~¼ after the swap (long "WWWW" → single "W") | polled c1=744, then changed=2000 ≥ 150, foreground 744 → 191 (≤ c1/2) |
| smoke (c) daemon lane | pointsize clamped at the parse and on script writes | stderr ring: `M3E-POINTSIZE-JSON 512` (JSON 9999), `M3E-POINTSIZE-SET 512` (script 9999), `M3E-POINTSIZE-NEG 4` (−5 → MIN_FONT_PX) |
| smoke (d) daemon lane | `font: "DefinitelyNotAFontFamily_M3E"` → fallback chain resolves, layer renders, the diagnostic names the request | **foreground=1820, differing=2016, mean=(R 245.9, G 245.9, B 255.0)** (white text over red clear); `event=renderer.scene.text_font_fallback layer=txt requested=DefinitelyNotAFontFamily_M3E` in the ring; SKIP-with-message when the host has no system fonts |
| corpus honesty | text layers in the corpus | **zero textures, zero known text layers** — synthetic fixtures only |
| regressions | video + supervisor suites | `smoke-video.sh` exit 0 (deviation 2 ≤ 4) |
| plasmashell pid guard | no plasmashell touched | pid unchanged across the suite |

### M3f — particle systems (this commit)

Particle systems (`particle` objects) implement the **flat emitter
model** on the M3c/M3d render path: every property is a scene.json key or
a script-visible scalar, the simulation advances on a fixed 1/60 s
timestep, and each system draws as **one batched draw** of 6 vertices per
particle from a per-system host-visible vertex buffer with per-particle
color+size in the vertex attributes — a new shader pair
(`shaders/particle.vert`/`particle.frag`), with the blend modes reusing
the M3d per-mode pipeline variants (≤ 16 pipelines) and the texture
riding slot `MAX_LAYERS + system_index` (272 slots total).

Simulation (`src/particles.rs`): the frame's dt is accumulated
rate-scaled (`instance.rate`, the WE simulation-rate factor) and capped
at 1.0 s (60 steps) per frame; each step spawns (only while emitting or
a burst is pending — WE `play()`/`pause()`/`stop()`/`emitParticles()`
semantics), integrates explicit Euler (`v += g·h; x += v·h; age += h`)
and retains `age < life`. Size, color and alpha interpolate over
normalized age. Determinism: every op is f32 fixed-step; launch speeds
pick uniformly in `[speedMin, speedMax]` (the pair supersedes a bare
`speed` and normalizes), launch angles spread uniformly in `direction ±
spread/2` through one splitmix64 stream per system seeded by the system
index — a documented deviation from WE's per-frame randomness, it is
what makes the smoke range oracles reproducible (spread-0 systems never
touch the stream: their trajectories are exact).

Researched WE facts (full evidence in SCENE_FORMAT_V1.md): the emitter
config nests in a `particle` object; `maxcount` is the WE key for the
live-particle cap (WE default 100 — we default to 1000 and clamp
1..=4096, a documented deviation); **WE emitters have no
`direction`/`spread` fields** (velocity comes from the velocity-random
initializer) — the flat model is the M3f extension the deterministic
smoke oracles need; the script surface is `IParticleSystem`
(`play()`/`pause()`/`stop()`/`isPlaying()`/`emitParticles(count)` —
pause stops emission but keeps particles simulating, stop clears
immediately) plus `IParticleSystemInstance` with the factors `count`,
`speed`, `lifetime`, `size`, `alpha`, `rate`, `colorn` (the intentional
WE spelling), each defaulting to 1.0 and clamping non-finite to 1.0;
the texture field is `material` (the brief's `texture` wins when both
are present), resolved through the M3c image-source chain including the
package table. Blend modes reuse the M3d table (0/1/6/7/9 =
Normal/Multiply/Add/Screen/Subtract); `blendMode` and `colorBlendMode`
are both accepted. The JS surface: `Scene.getParticleSystem(name|index)`
(→ null for unknown, never throws), `Scene.getParticleSystemCount()`,
and the WE-compatible `Scene.getLayer` path — a `particle` object is
also reachable by name through `getLayer`. All scene.json keys are
clamped at the parse (missing → documented default, out-of-range → the
clamp bound, non-finite → default; vector fields reject non-vector
shapes like every WE vector).

**Corpus fact**: the corpus carries **zero particle systems and zero
textures** — an emitter needs one — so M3f is validated with synthetic
fixtures only: the smoke lanes below (generated at runtime, never
committed) and unit tests, never with byte-pinned real-content renders.

### M3f — acceptance evidence

| Case | Expected containment | Result |
|---|---|---|
| workspace gates | `cargo fmt --all -- --check`; `cargo clippy --workspace --all-targets -- -D warnings`; `cargo test --workspace --all-targets` | clean; 403 tests pass (149 in `kwe-scene-renderer`) |
| simulation units | fixed-step accumulate/spawn/integrate/retain, f32 determinism, capped accumulators, drop-not-evict, alpha/size/color interpolation | `simulate_fixed_dt_sequence_*` / `exact_positions_for_fixed_dt_sequence` / cap + clamp tests in particles.rs |
| scene.json parse | defaults table, scalar clamps (numeric-string forms), speed pair normalization, gravity vector forms, maxCount clamp, material/texture precedence, common props | `particle_systems_parsed_with_defaults` / `particle_*` parse tests |
| JS proxy | getParticleSystem by name/index, instance factors with the WE defaults and clamps (non-finite → 1.0, [0,1e6] / alpha/colorn [0,1]), play/pause/stop/isPlaying/emitParticles, property writes clamp on the Rust side | `particle_system_proxy_*` / `particle_instance_factors_*` / playback tests in js.rs |
| smoke (a) daemon lane | deterministic trail — 100/s, life 1 s, speed 60, direction 0, spread 0: steady state 100 particles, one per scene px, the 8px quads tile the band [76,144]×[41,49] | **foreground=536** full-white px in the 69×8 band, pure-white mean (llvmpipe lane: 456 px — software-rasterizer coverage, same band) |
| smoke (b) daemon lane | gravity differential — blue (gravity [0,80]) falls y = 40t² (mean frame-y ~15 px below the stationary red square at y 45) | red=328 blue=64, red_mean_y=69.0 blue_mean_y=44.5 — the oracle's arg-order labels are swapped vs the visual colors (first arg = visual blue): the visual-blue trail holds 328 px at mean frame-y 69.0, the visual-red square 64 px at mean y 44.5 — gap 24.5 px (≥ 3 required) |
| smoke (c) daemon lane | spawn cap — spawnRate 4096/s with maxCount 4096 (the hard cap). The drop policy (spawn→integrate→retain, floored accumulator) suppresses ALL births while the cap is full and nothing has died: the population is ONE sliding 1-s cohort (age spread = maxCount/spawnRate), cycling with period = life = 5 s — exact-step sim: uniform-age disc (radius 0→30, ~3.9k px) at t≡1 mod 5 → SOLID annulus [30(t−1), 30t] sweeping outward at 30 px/s (max ~8.7-8.8k px at t≈2, ≥ 4k px for ~40% of every cycle) → off-frame by ~3.7 s → the cohort dies 1:1 into fresh births ([5,6], disc regrows). Excess spawns dropped (never evicted), one bounded diagnostic | `event=renderer.scene.particles_capped system=dust` in the ring; **4637** white px at the poll's first ≥ 4k crossing (~1.0 s; llvmpipe: 4409) — a ramp value, not the cycle max (sim max 8761); an uncapped population would fill all 14400 px |
| smoke (d) daemon lane | `instance.count = 8` from script multiplies pb's spawn rate — the pb/pa white ratio passes 3 | `M3F-COUNT-SET 8` in the ring; polled ratio **3.6** (pa 633 → pb 2281 white px; both lanes) |
| smoke (e) daemon lane | blend differential over an opaque (30,30,30) clear — Add (6) is min(255, texel+bg): 106 single / 182 double-overlapped; Normal (0) draws the opaque texel 76 flat | add disc max R **255**, normal disc max R **76** (gates ≥ 150 / ≤ 100; llvmpipe lane repeats the same oracles) |
| smoke (f) daemon lane | draw order across kinds — the file's objects array is [particle, image]: the 30×30 red image (objects[1]) draws ON TOP of the solid white particle disc (objects[0]); the old `draws.extend()` painted every particle draw last, whatever the file said (the regression this case pins) | frame center (80,45) reads **red** on both lanes, disc-only pixel (50,45) still white — image over particles |
| standalone llvmpipe lanes | every M3f case (a)-(f) repeated under the software rasterizer on the worker's own frame file | same oracles pass (a: 456 px, c: 4409 px, d: ratio 3.6, f: center red); (c) also greps its log for particles_capped, (d) for M3F-COUNT-SET 8 |
| regressions | video + supervisor suites | `smoke-video.sh` exit 0 (deviation 2 ≤ 4) |
| plasmashell pid guard | no plasmashell touched | pid unchanged across the suite |

## Run the suites

```sh
scripts/smoke-scene.sh       # M3a..M3c: scene renderer through the daemon,
                             #   scripted-color oracle, containment, the
                             #   scene.pkg lanes, the M3c image-layer cases
                             #   (a)-(f), plus a standalone llvmpipe lane
                             #   (scripted color AND the M3c layer oracles)
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
| workspace gates | `cargo fmt --all -- --check`; `cargo clippy --workspace --all-targets -- -D warnings`; `cargo test --workspace --all-targets` | clean; 336 tests pass (82 in `kwe-scene-renderer`) |
| device tests (both drivers) | `KWE_TEST_DEVICE`-gated renders: fullscreen red 1×1 (`isolated_draw`), 2×2 four-color orientation quad (`quad_orientation`), straight-alpha blend (`blend_partial_alpha`) | byte-exact on llvmpipe and NVIDIA RTX 3070: every pixel [0,0,255,255]; at(8,8)/(56,8)/(8,40)/(56,40) = blue/green/red/white; every pixel (106,77,48,191) — the once-premultiplied oracle |
| smoke (a): two layers, scene order | red fullscreen under a blue 40×22 mark at (60,34); draw order = scene.json order | daemon lane: (90,55)=(0,0,255,255) red, (140,79)=(255,0,0,255) blue, (150,85)=(255,0,0,255) blue — byte-exact, tol 0 |
| smoke (b): src-over blend | fullscreen (64,103,142) texel at alpha 191/255 over a zero clear | (80,45)=(106,77,48,191) tol 1 — the straight-composite + one-readback-premultiply oracle |
| smoke (c): reversed order | same scene with the objects array reversed | (140,79)=(0,0,255,255) — blue now on top, array order wins |
| smoke (d): missing image | `image: "broken.png"` that does not exist | renderer stays `live`, sequence advances, stderr ring carries `event=renderer.scene.layer_skip layer=broken`; remaining layers render |
| smoke (e): 257 layers | 257 image objects | `rolled_back`, `last_failure_detail` carries `exit_code_73` and `over the 256 layer cap`; pid unchanged from the previous worker — no bounce |
| smoke (f): scripted move | script sets `mark.origin.x=60, origin.y=34, size 40×22` on the blue layer every frame | (140,79)=(255,0,0,255) blue (moved onto the red region), (158,85)=(255,0,0,255), (90,55)=(0,0,255,255) red — the proxy read/write + clamp round-trip |
| standalone llvmpipe lane: M3c oracles | the M3c (a) composite and (b) blend scenes under the software rasterizer | same pixel oracles pass on llvmpipe: (90,55)/(140,79)/(150,85) = red/blue/blue and (80,45)=(106,77,48,191) — a mirrored readback or broken quad fails these samples |
| layer cap / clamp units | 256 accepted, 257 rejected; clamps: non-finite→0, alpha 0..=1, \|v\| ≤ 1e6, size ≥ 0, `visible` boolean coercion; malformed model layers skip | `exactly_256_image_layers_accepted`, `rejects_257th`, `malformed_model_layers_skip_never_reject`; clamp tests in layers.rs + js.rs |
| corpus stats (re-scan) | image-bearing objects, blendMode histogram, reference classes, property-wrapping rates | 685 objects; 432 with `colorBlendMode` (410×0, 6×11, 6×30, 4×6, 2×24, 1×1, 1×7, 1×9, 1×12 — sums to 432); **620 model `.json` refs + 65 null image values, 0 textures (M3h)**; 70% (315/447) alpha / 49% (276/568) visible property-wrapped |
| regressions | video + supervisor suites | `smoke-video.sh` exit 0 (deviation 2 ≤ 4), `smoke-supervisor.sh` exit 0 |
| plasmashell pid guard | no plasmashell touched | pid unchanged across the suite |

### M3f — particle systems (this commit)

| Case | Expected containment | Result |
|---|---|---|
| workspace gates | `cargo fmt --all -- --check`; `cargo clippy --workspace --all-targets -- -D warnings`; `cargo test --workspace --all-targets` | clean; 403 tests pass (149 in `kwe-scene-renderer`) |
| smoke (a): deterministic trail | 100/s, life 1 s, speed 60, direction 0, spread 0: steady state 100 particles, one per scene px, the 8px quads tile the band [76,144]×[41,49] | daemon lane **foreground=536** full-white px in the 69×8 band, mean (255,255,255); llvmpipe lane 456 px — the deterministic spread-0 trajectories (splitmix64 never consulted) |
| smoke (b): gravity differential | blue (gravity [0,80]) falls y = 40t² from the origin; stationary red (gravity 0) stays at y 45 | red=328 blue=64, red_mean_y=69.0 blue_mean_y=44.5 — the oracle's arg-order labels are swapped vs the visual colors (first arg = visual blue): the visual-blue trail holds 328 px at mean frame-y 69.0, the visual-red square 64 px at mean y 44.5 — gap 24.5 px (≥ 3 required); both lanes |
| smoke (c): spawn cap | spawnRate 4096/s × life 5 s = 20480 demanded but maxCount 4096 (the hard cap) bounds the population. The drop policy suppresses ALL births while the cap is full and nothing has died (spawn→integrate→retain, floored accumulator): ONE sliding cohort, age spread exactly 1 s = maxCount/spawnRate, cycling with period = life = 5 s. Exact-step sim: uniform-age disc (radius 0→30, ~3.9k px) at t≡1 mod 5 → SOLID annulus [30(t−1), 30t] at 30 px/s (max ~8.7-8.8k px at t≈2 — the annulus area π(60²−30²) = 8482 + quad overhang; ≥ 4k px for ~40% of every cycle) → off-frame by ~3.7 s (0 px) → the cohort dies 1:1 into fresh births ([5,6], disc regrows). One bounded diagnostic at the first drop (~1 s) | `event=renderer.scene.particles_capped system=dust` in the ring (re-queried after the poll); **4637** white px at the poll's first ≥ 4k crossing (llvmpipe: 4409) — a ramp value, not the cycle max (sim max 8761; the reviewer's "~5-6k px uniform-age disc" matches the center-box [40,5,80,80] max 5938 during the annulus pass, not a whole-frame count); exit-73 float `maxCount` rejection covered by the unit `maxCount` parse tests (smoke case used the integer form) |
| smoke (d): instance.count factor | script sets `pb.instance.count = 8` at t=2 s; count multiplies the spawn accumulator — pb's sparse disc saturates while pa's stays sparse | `M3F-COUNT-SET 8` in the ring via console.log; polled pb/pa white ratio **3.6** (pa 633 → pb 2281 px, both lanes) — bash integer division would truncate 3.6 to 3, so the suite compares ×10 |
| smoke (e): blend differential | Add (6) = min(255, texel+bg): 106 single / 182 double-overlapped / up to 255; Normal (0) draws the opaque 76 texel flat; discs r=45 at frame x 28/132 keep 7 px margins from the [80,160] seam | add box max R **255**, normal box max R **76** (gates ≥ 150 / ≤ 100; both lanes); `blendMode` is an object-level prop (the M3c/M3d shared path) — the suite fixture hoists it out of the `particle` dict, matching the WE serialization |
| smoke (f): draw order across kinds | the file's objects array is [particle, image]: the 30×30 red image (objects[1]) draws ON TOP of the solid white particle disc (objects[0] — a capped 4096/s × life 1 steady disc of radius 30, always solid at the center); the old `draws.extend()` painted every particle draw last, whatever the file said (the regression this case pins) | frame center (80,45) reads **red** (probe ≥ 1 px) on both lanes, disc-only pixel (50,45) still white — the image is over the particles, not missing; unit coverage: `merged_draws_restore_the_file_object_order_across_kinds` (layers.rs), `scene_order_records_the_objects_array_position_across_kinds` (scene.rs) |
| exit code parity | a malformed particle scene rejects like every bad scene | `"objects[0].maxCount" must be an integer or a numeric string` → exit 73 → `rolled_back` (the suite's run-3 fixture passed `maxCount: 1000.0` — the float rejection observed live; the fixture uses the integer form) |
| regressions | video + supervisor suites | `smoke-video.sh` exit 0 (deviation 2 ≤ 4) |
| plasmashell pid guard | no plasmashell touched | pid unchanged across the suite |

### M3g — VideoLayer textures via libmpv (recovered implementation)

This slice keeps video decoding in the supervised `kwe-scene-renderer`
process. A `video` object is registered in scene order and gets at most two
software libmpv cores; additional layers remain registered but draw nothing.
The decoder uses `rgb0` into one reusable RGBA buffer, then Vulkan refreshes a
persistent image through one grow-only host-visible staging buffer. Matching
dimensions never allocate or replace descriptors per frame. A missing,
unreadable, unsupported-extension, corrupt, oversized, or failed source
degrades only that layer; the last good texture remains visible after a
refresh failure.

The source boundary is intentionally local-only: file references are opened
with no-follow and copied from a validated fd into a worker-owned snapshot;
package entries are extracted into the same 0700 pid-qualified directory. The 160 MiB source cap matches the scene
worker's RLIMIT_FSIZE. mpv is configured with `hwdec=no`, `audio=no`,
`cache=no`, bounded lavf demux buffers, `access-references=no`,
`autoload-files=no`, `load-scripts=no`, and FFmpeg's
`protocol_whitelist=file`; no network grant is consulted or needed. Package
files are removed only after worker teardown. Media state is latest-wins and
fans out play/pause/stop (stop also seeks to zero) to open layers; per-layer
SceneScript controls remain deferred.

| Case | Expected containment | Result |
|---|---|---|
| playback oracle | synthetic two-colour 64×64 mp4 changes at the frame center | `scripts/smoke-scene.sh` M3g-a: both colors observed; compositor frame refreshes |
| native-size | omit `size` | M3g-b: decoder dimensions fill the layer and the surrounding clear remains unchanged |
| decoder cap | three valid layers | M3g-c: exactly two cores open; one layer is skipped with a bounded diagnostic |
| bad source | missing local source beside a healthy image | M3g-d: only the bad layer skips and the scene remains live |
| package video | runtime-generated package embeds the synthetic clip | M3g-e: package extraction decodes, then runtime-directory absence proves teardown cleanup |
| corrupt package video | valid package containing an invalid video payload | M3g-f: only the corrupt layer skips; the scene reaches live and stops cleanly |
| media state | paused, playing, and stopped latest-wins commands | daemon ack sequence advances and keepalive continues while paused/stopped |
| standalone teardown | direct llvmpipe worker opens the synthetic clip | both colors advance and SIGTERM exits cleanly with no staged-media residue |
| package/path policy | package extraction is bounded and cleaned after decoder drop; traversal, symlink, remote protocol, and non-container extension are rejected | unit and package resolver coverage; no media code runs in plasmashell |
| compositor failure | ordinary refresh failure disables that decoder once; fence timeout is process-fatal before fence/resource reuse | classifier unit plus worker rejection path |
| regressions | video/supervisor suites and plasmashell guard | `smoke-video.sh`, `smoke-supervisor.sh`, and scene smoke are required; environments without ffmpeg must report the M3g lane as skipped, not pass silently |

No UI changed in M3g. `content.scene2d` remains partial/backend-dependent:
the scene capability manifest and manager presentation are deferred, and this
slice does not claim full `content.scene2d` or `runtime.scenescript` parity.

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
