# S4a adversarial review (material vertex attribute shapes / `#if`-aware
declaration folding / uniform identity defaults)

Worktree `kwe-s4`, branch `beta-s4-particles`, commits `2aa4f71`, `8f4c6fe`,
`ccd6a65` on `8ad9f40`. Read-only review, no edits made. Prior bars:
`docs/research/S1_REVIEW.md`, `S2_REVIEW.md`, `S3_REVIEW.md`.

Counts: **3 MUST-FIX, 2 RECOMMENDED, 1 NIT**.

---

## MUST-FIX

### 1. `evaluate_if_expr`'s recursive-descent parser has no depth bound — a single `#if` line with deeply nested parens or a chained `!` run, well within the existing 1 MiB shader budget, overflows the stack and aborts the whole renderer process

**Resolution: fixed.** Added `MAX_IF_EXPR_DEPTH` (32) and `MAX_IF_EXPR_TOKENS` (256) in `shaderpre.rs`. `tokenize` now rejects (returns `None`) once token count exceeds the bound, checked incrementally so a pathological line cannot even build a huge `Vec<Token>` first. `Parser` gained a monotonically-increasing `depth` field, checked in a new `enter()` helper called at the top of `or_expr` (covers nested-paren recursion via `primary_expr`'s `LParen` arm) and at the start of `unary_expr`'s `!`-recursion branch (covers chained `!`). New test `evaluate_if_expr_bounds_deeply_nested_parens_and_not_chains` exercises 10,000 nested parens and a 10,000-`!` chain; both return `None` immediately (µs, not a hang) with no stack overflow.

`crates/kwe-scene-renderer/src/shaderpre.rs:656-838` implements `#if`/`#elif`
expression evaluation as ordinary mutually-recursive descent:
`or_expr` (742) → `and_expr` (751) → `eq_expr` (760) → `unary_expr` (779) →
`primary_expr` (787), and `primary_expr`'s `Token::LParen` arm (816-822)
calls back into `or_expr` for every open paren; `unary_expr` (779-785) also
calls itself directly for every leading `!`, with no counter, no bound, and
no explicit stack anywhere in the call chain.

Nothing upstream of this bounds recursion depth — only overall byte size.
`resolve_includes` enforces `MAX_PREPROCESSED_BYTES` (1 MiB,
`shaderpre.rs:52`) on the post-include-expansion source *before*
`fold_declarations` ever runs (`shaderpre.rs:381-387`, checked at
`preprocess`'s call site `shaderpre.rs:1119`), but that check bounds total
bytes, not the shape of any single line. A `.vert`/`.frag` shader is
attacker-controlled content: a hostile or malformed Workshop package can
name any shader file its own `material.json` `shader` field points to, and
`main.rs::compile_one_material` (`main.rs:2113`) reads and preprocesses it
before anything else validates its contents. A single line such as
`#if ` + `"("` × ~400,000 + `"1"` + `")"` × ~400,000, or `#if ` + `"!"` ×
~900,000 + `"1"`, sits comfortably under the 1 MiB cap and drives
`primary_expr`/`unary_expr` several hundred thousand recursion levels deep
— stack overflow in Rust is `SIGSEGV`/`SIGABRT`, not a catchable panic, so
`catch_unwind` (if any exists at the compile boundary) cannot save the
process, and the entire `kwe-scene-renderer` worker dies, not just the one
scene/material being loaded. This is squarely the "bounded... frame
dimensions" / hostile-metadata rule in `AGENTS.md`'s engineering rules, and
it is cheaper to trigger than any GPU-resource exhaustion path reviewed in
S1-S3 (pure text, no shader compile, no textures).

The `#if`/`#ifdef`/.../`#endif` NESTING tracker itself (`cond_stack: Vec
<CondFrame>`, `shaderpre.rs:889-949`) does not have this problem — it is an
explicit heap-allocated `Vec`, not recursive function calls, so a shader
with many *nested `#if` directives* (one per line) only grows a `Vec`, not
the native stack. The bug is specific to `evaluate_if_expr`'s expression
grammar, invoked once per `#if`/`#elif` *line* (`shaderpre.rs:916`, `934`).

**Minimal fix**: thread a depth counter (or a token-count budget) through
`Parser` and fail (`return None`) once a small bound (e.g. 64, generous
over any real WE `#if` expression, which are all one-line boolean
combinations per the corpus survey) is exceeded, mirroring
`MAX_INCLUDE_DEPTH`'s existing pattern for the `#include` resolver in the
same file.

### 2. The "unsure → always live" fallback silently *suppresses* a genuinely-live `#else`/`#elif` branch instead of unioning it — an under-scraped attribute list produces a `VertexInputAttributeDescription` set that omits a location the compiled SPIR-V actually declares

**Resolution: fixed**, via the minimal fix suggested (propagate an error, not the union alternative — the review's own note that unioning reintroduces the MORPHING double-counting bug this slice already fixed is correct). New `PreprocessError::AmbiguousCondition(String)` variant (carries the raw, char-count-capped-at-200 condition text). `fold_declarations`'s `#if`/`#elif` handling now only calls `evaluate_if_expr` when the parent scope is actually live (`&&`-short-circuit already meant a dead parent never really needed the child's truth value); when it IS live and the expression cannot be judged, `fold_declarations` returns `Err(AmbiguousCondition)` instead of guessing `true` — this propagates through `preprocess`'s existing `?` up to `compile_one_material`'s already-tested `preprocess_failed` fallback path (S1 flat quad), not a new fallback reason. Three new tests: `unparseable_if_expression_falls_back_to_live` (renamed in place, now asserts `AmbiguousCondition`), `unparseable_if_expression_with_a_live_else_sibling_is_rejected_not_guessed` (the exact shape from the finding — an unparseable `#if` with a live `#else` declaring different attributes — asserts rejection, not a mismatched attribute/location list), and `unparseable_if_expression_behind_a_dead_parent_is_not_an_error` (confirms the short-circuit: an unparseable condition nested inside an already-dead `#if 0` does NOT reject the material).

`shaderpre.rs:916` (`#if`) and `shaderpre.rs:934` (`#elif`):
`evaluate_if_expr(...).unwrap_or(true)` — when the parser cannot fully
consume an expression (any construct outside `||`/`&&`/`==`/`!=`/`!`/
parens/`defined()`/bare identifiers/decimal integers — no `<`/`>`/`+`/
bitwise ops/hex literals/ternary, all of which real WE shaders use
elsewhere, e.g. `#if VERSION >= 2`), the branch is marked `taken = true`
*and* `frame.branch_taken = true` (`shaderpre.rs:917-920`,
`935`). Setting `branch_taken` is what makes the SIBLING `#elif`/`#else`
dead (`shaderpre.rs:930-931`: `if frame.branch_taken { frame.active_here =
false; }`, and the `#else` handler at `941-949` does the same via
`!frame.branch_taken`).

The doc comment on `evaluate_if_expr` (`shaderpre.rs:654-655`) says this
fallback exists "rather than risk silently dropping a real attribute
behind an expression it misjudged" — but it does exactly that in the
asymmetric case: if the true (real-`shaderc`) value of an unparseable `#if`
condition is actually **false**, and a live `#else` branch declares
*different* attributes, this fallback picks the `#if` branch's attributes
and never scrapes the `#else` branch's — the reverse of the stated intent.
Concretely: `#if SOME_VERSION_CHECK\n  attribute vec4 a_PositionVec4;\n
#else\n  attribute vec3 a_Position;\n  attribute vec2 a_TexCoord;\n#endif`
with an unparseable condition that is really false: Rust scrapes
`{a_PositionVec4}` (location 0), shaderc's real compile takes `#else` and
declares `{a_Position(0), a_TexCoord(1)}`. The resulting
`material_vertex_attributes` (`vulkan.rs`, `material_attribute_layout`)
builds a `VertexInputAttributeDescription` list for `a_PositionVec4` only —
missing `a_TexCoord`'s location entirely — while the real SPIR-V module
declares an input at that location the pipeline's vertex input state never
describes. Per the Vulkan spec this is invalid pipeline state (a
`VkPipelineVertexInputStateCreateInfo` missing an attribute the vertex
shader's input interface consumes); without validation layers active in a
release build, the practical failure mode is exactly what the review brief
asked about — the attribute reads whatever happens to be bound (garbage or
zeros), not a crash.

This exact shape is untested: `unparseable_if_expression_falls_back_to_live`
(`shaderpre.rs:1888-1907`) only exercises an `#if` with **no** `#else`
sibling, so the branch-suppression interaction is not pinned by any
existing test.

**Minimal fix**: on an unparseable `#if`/`#elif` condition, don't guess —
propagate an error out of `fold_declarations`/`preprocess` (a new
`PreprocessError` variant) so the caller's existing, tested fallback path
(`compile_one_material` → `fallback_reasons["compile_failed"]` → S1 flat
quad, `main.rs:2200-2214`) takes over for that one material, rather than
risk emitting an attribute/location list that doesn't match what `shaderc`
will actually compile. (Unioning both branches instead was considered and
rejected here: it reintroduces the exact `next_attribute_location`
double-counting bug this slice's own MORPHING fix was written to solve —
see `if_else_gated_attribute_scrapes_only_the_live_branch`,
`shaderpre.rs:1688-1716`.)

### 3. `g_ModelMatrix`/`g_ViewProjectionMatrix`/`g_NormalModelMatrix` (and their `Alt` siblings) fall through to the generic `mat4(0.0)`/`mat3(0.0)` zero-default — the same "zero-should-be-identity" bug class this slice fixed for `g_Texture<N>Rotation`/`g_Color4`, left unfixed for the matrices that feed vertex *position*, not just lighting

**Resolution: fixed.** `standard_uniform_expr` now maps `g_ModelMatrix`/`g_AltModelMatrix`/`g_ViewProjectionMatrix`/`g_AltViewProjectionMatrix` to `mat4(1.0)` and `g_NormalModelMatrix`/`g_AltNormalModelMatrix` to `mat3(1.0)` identity, matching the types verified by grep against the local WE asset shader corpus. New test `model_and_view_projection_matrices_fold_to_identity_not_zero` pins all six. Grepped every remaining unconditionally-declared `mat3`/`mat4` uniform in the local WE asset shader corpus for the commit message per the review's ask: two remain zero-defaulted — `g_EffectModelMatrix` (`volumetricsfront.frag`'s raymarch, single file) and `g_EffectModelViewProjectionMatrix` (`effectcomposebackground.vert`'s screen-coord compute, single file) — both effect-pass-only, single-shader-file names distinct from the widely-used image-object family this finding named; left unfixed as out of this finding's specific scope and recorded as a known residual gap in `docs/SCENE_FORMAT_V1.md` rather than silently dropped. `docs/SCENE_FORMAT_V1.md`'s framing updated: the "still draws, just without lighting" language is now scoped to genuinely lighting-only uniforms (`g_EyePosition` etc.), with the geometry-collapse mechanism for the three fixed matrices explained separately.

`shaderpre.rs:603-615` (`zero_literal`, pre-existing, unchanged by this
diff) maps any unrecognized `mat4`/`mat3` uniform to `mat4(0.0)`/
`mat3(0.0)`. `standard_uniform_expr` (`shaderpre.rs:513-586`) recognizes
exactly two matrix names — `g_ModelViewProjectionMatrix` and
`g_EffectTextureProjectionMatrix` (`shaderpre.rs:555-556`) — nothing else.
`g_ModelMatrix`, `g_ViewProjectionMatrix`, `g_NormalModelMatrix`,
`g_AltModelMatrix`, `g_AltNormalModelMatrix`, `g_AltViewProjectionMatrix`
are not mapped anywhere in `shaderpre.rs`/`vulkan.rs`/`materialshader.rs`
(verified by grep) and hit the zero fallback, logged only as one more name
in `unsupported_uniform_names` (`main.rs:2181-2182`, `2678-2683`) — never a
refusal, the material still compiles and draws.

This was previously moot: pre-S4, `material_vertex_format_supported`
accepted only exact `a_Position`+`a_TexCoord` shaders, and the
`LIGHTING`/`REFLECTION`/`VERTEXCOLOR`-gated code in
`genericimage2/3/4.vert` (the local corpus's real 4/5/6-attribute family)
never had a live path in past acceptance. S4's own stated deliverable is
widening acceptance to exactly this shader family (`a_Normal`/`a_Color`
materials). Traced directly in the local WE asset corpus
(`/media/crushinator/steamapps/common/wallpaper_engine/assets/shaders/genericimage3.vert`):

```
56: #define M_MDL g_ModelMatrix       // LIGHTING||REFLECTION off (default)
58: #define M_VP  g_ViewProjectionMatrix
...
163: vec4 worldPos = mul(vec4(localPos, 1.0), M_MDL);   // UNCONDITIONAL
...
206: #if LIGHTING
207:     gl_Position = mul(worldPos, M_VP);
208: #else
209:     gl_Position = mul(vec4(localPos, 1.0), M_MVP);  // g_ModelViewProjectionMatrix, fine
210: #endif
```

`worldPos` is computed on *every* draw of this shader family regardless of
combos (line 163 is not inside any `#if`), using `g_ModelMatrix` — zero
matrix in, `worldPos = vec4(0,0,0,0)` out. That's dead/wasted for the
default `LIGHTING=0` path (only consumed inside the `LIGHTING||REFLECTION`
block). But once a material sets `LIGHTING=1` (a legitimate combo override
— `VERTEXCOLOR`, a sibling gate on the very same attribute family this
slice targets, is live elsewhere in the broader WE asset corpus:
`materials/util/flatalphavertexcolor.json`, `materials/util/
gizmovertexcolor.json`), `gl_Position` itself is computed from
`worldPos * g_ViewProjectionMatrix` — two zero matrices multiplied through
— collapsing every vertex to the same degenerate clip-space point (not
"missing lighting," the geometry's *screen position* breaks).

`docs/SCENE_FORMAT_V1.md` (this diff, around line 151-156) already
discloses a version of this gap: "a material that genuinely uses
`g_EyePosition`/`g_ModelMatrix`/`g_ViewProjectionMatrix` for real (non-zero)
shading still draws with those zero-defaulted — a documented, pre-existing
gap" filed under "Neither fix is lighting/reflection... `#require
LightingV1` still resolves to a zero-contribution stub." That framing
undersells it: this is not confined to lighting math the way the
`LightingV1` stub is — `g_ModelMatrix`/`g_ViewProjectionMatrix` feed vertex
*position*, so "still draws" is not accurate once `LIGHTING=1` is set; the
object's geometry itself collapses. This is exactly the bug class the S4
commit log calls out finding via the corpus sweep for `g_Texture<N>Rotation`/
`g_Color4` ("a previously-refused material now compiling but drawing
WRONG, worse than the honest fallback it replaced") — same mechanism,
different uniform names, not caught by the 60-scene sweep only because
none of those 60 scenes happen to set `LIGHTING`/`REFLECTION`/`VERTEXCOLOR`
on a `genericimage2/3/4`-family material.

**Minimal fix**: same pattern already established in this slice for
`g_Texture<N>Rotation` — either map `g_ModelMatrix`/`g_ViewProjectionMatrix`/
`g_NormalModelMatrix`/`g_AltModelMatrix`/`g_AltNormalModelMatrix`/
`g_AltViewProjectionMatrix` to explicit identity expressions
(`mat4(1.0)`/`mat3(1.0)`) in `standard_uniform_expr`, or change
`zero_literal`'s `mat3`/`mat4` cases to identity outright (a zero matrix is
essentially never the intentionally-correct default for an *unimplemented*
transform — the same reasoning already written into this slice's
`g_Texture0Rotation` fix comment applies without modification here). Update
`docs/SCENE_FORMAT_V1.md`'s framing to say geometry position, not just
lighting, is affected until this is fixed.

---

## RECOMMENDED

### 4. `#ifdef`/`#ifndef` combo-name matching is case-insensitive (`shaderpre.rs:898`, `907`: `.to_uppercase()`), the real GLSL preprocessor is not

**Resolution: fixed**, applied as written. `#ifdef`/`#ifndef` now match `rest.trim()` exactly (case-sensitive) against `combos_upper`'s already-upper-cased keys, and `evaluate_if_expr`'s bare-identifier `lookup`/`defined(...)` handling was changed the same way for consistency (both directions of the finding's "fold case both sides consistently, or document the choice" — chose exact-case, matching `shaderc`'s real behavior against this module's always-upper-cased `#define` emission). New test `if_and_ifdef_are_case_sensitive` pins both `evaluate_if_expr` directly and a full `#ifdef` scrape.

`#define`s are always emitted uppercased (`shaderpre.rs:1144-1146`:
`format!("#define {} {}", name.to_uppercase(), value)`), but `shaderc`'s
real preprocessor matches macro names case-sensitively. A hostile or
merely oddly-cased shader (`#ifdef somethingMixedCase`) is currently
over-scraped as live by Rust's tracker whenever `SOMETHINGMIXEDCASE` (any
case) is a real combo, even though `shaderc` would see it as an undefined
macro (false) — this is the *safe* direction (extra unused attribute, per
the S3-review-established "over-inclusion is harmless" pattern), not
exploitable for under-scraping today, but it is one more place where
Rust's tracker and `shaderc`'s real preprocessor can silently disagree.
Lower cost than #2 to fix once #2's structural fix (propagate uncertainty
rather than guess) is in: normalize by requiring exact-case match against
the pre-uppercased `combos_upper` keys the same way `#if`'s bare-identifier
`lookup` already effectively does (case-fold both sides consistently and
document the deliberate choice either way).

### 5. `g_Texture0Rotation`'s identity default is an approximation, not literally upstream's default, and the doc comment doesn't say so

**Resolution: fixed**, applied as written. Added a doc-comment caveat on the `g_Texture<N>Rotation` mapping in `standard_uniform_expr`: notes upstream's real raw default is `{0,0,0,0}` (`CPass.h`), that every local corpus occurrence only READS this uniform behind `#if SPRITESHEET` (so the identity default is inert, not just harmless, whenever `SPRITESHEET` is off — the common case, including the Workshop 3100709479 scene this slice's own fix narrative cites), and that identity would still be visually wrong for a genuinely multi-frame spritesheet material (stretches the atlas instead of windowing one frame) — not validated against such a case.

Traced against `CPass.cpp`/`CPass.h`
(`/home/qcv123/gitClones/linux-wallpaperengine`): the real engine's default
`TextureAnimationState::rotation` is `{0,0,0,0}` (`CPass.h:122`), used
whenever the bound texture is not GIF-animated (`CPass.cpp:250`,
`isAnimated()` → `TextureFlags_IsGif` only, `Texture.h:174`) — i.e.
upstream's own *raw* default is zero, not identity, for the common static-
texture case, and the corpus shader only reads `g_Texture0Rotation` at all
behind `#if SPRITESHEET` (verified in `genericimage.vert:30`,
`genericimage2.vert:102`, `genericimage3.vert:153`,
`genericimage4.vert:177` — every occurrence is `SPRITESHEET`-gated). This
renderer's `vec4(1.0, 0.0, 0.0, 1.0)` choice is a reasonable engineering
call given the renderer doesn't decode per-frame spritesheet/GIF atlas data
at all (a documented, separate gap) — identity is a strictly better
approximation than the old zero-default for that unimplemented case,
matching this same document's own `g_ModelMatrix` reasoning in finding #3
— but the `shaderpre.rs:519-536` comment states it as *the* correct value
for the formula without noting it is only exercised when `SPRITESHEET` is
live, and would still be visually wrong (stretches the whole atlas instead
of windowing one frame) for a real multi-frame spritesheet material if one
is ever encountered. Worth a one-line comment caveat so a future reader
doesn't treat this as validated against a genuinely-animated corpus case.

---

## NIT

### 6. `AttributeDecl::location`'s doc comment (`shaderpre.rs:256-265`) promises callers "must use THIS field, not the attribute's position in the Vec" — true today, but `material_vertex_attributes` (`vulkan.rs`) has no test exercising a case where the two actually diverge (e.g. a live `#ifdef`-gated attribute *between* two others, shifting locations without shifting Vec order)

**Resolution: fixed**, applied as written. New test `location_field_is_correct_even_when_an_earlier_branch_is_dead` in `shaderpre.rs`: a live `#if VERTEXCOLOR`-gated `a_Color` sits between `a_Position` and `a_TexCoord` in source order (with a dead `#if MORPHING` branch contributing nothing in between, so the assigned locations are not simply "index equals location" by accident of the test's own structure), pinning `AttributeDecl.location` as the field callers must read.

Every current test (`shaderpre.rs`, `vulkan.rs`) happens to construct
inputs where `Vec` index and `location` coincide. Not a correctness bug —
the field is used correctly everywhere it's read (`vulkan.rs`
`material_vertex_attributes` reads `attribute.location`, never the loop
index) — just a coverage gap for the specific claim the doc comment makes.

---

## Verified while reviewing (no finding)

- `cond_stack`'s own nesting (`#if`/`#ifdef`/`#ifndef` pushes,
  `#endif` pops, `shaderpre.rs:896-950`) is a plain `Vec`, not recursion —
  many *nested directives* only grow heap memory, already implicitly
  bounded by `MAX_PREPROCESSED_BYTES`; the stack-overflow risk (finding
  #1) is specific to a single expression's own parse tree, not directive
  nesting depth.
- Huge integer literals in a `#if` expression degrade cleanly: `tokenize`'s
  digit-run `.parse::<i64>().ok()?` (`shaderpre.rs:` tokenizer) returns
  `None` on overflow, propagating to `evaluate_if_expr` returning `None`
  (falls back to "always live," not a panic) — no integer-overflow panic
  path found.
- `material_attribute_layout`/`MATERIAL_UNIT_QUAD`/`MATERIAL_UNIT_QUAD_STRIDE`
  (`vulkan.rs`): offsets don't overlap and fit the stride
  (`material_attribute_layout_offsets_do_not_overlap`), the constant array's
  declared length matches the declared stride
  (`material_unit_quad_matches_the_declared_stride`), and an unknown
  attribute name is a hard `Err`, never a panic or silent drop
  (`material_vertex_attributes_rejects_an_unknown_name`) — all three
  reviewer-brief "vertex buffer layout/stride math" concerns are
  positively pinned by tests, not just asserted in comments.
- `MaterialKey::compute` (`materialshader.rs:293-307`, unchanged by this
  diff) hashes `(shader_name, combos, blend_variant)`; since a material's
  scraped `AttributeDecl` list is a pure, deterministic function of
  exactly those same inputs (shader source is looked up by name, live
  branches by combos), the pipeline cache key indirectly but soundly
  covers the vertex format — no cache-key/attribute-shape drift found (and
  `register_material_pipeline`'s early-return-if-cached path never
  receives inconsistent attributes for the same key as a result).
- Over-inclusion (the *safe* direction of "unsure → always live", when it
  actually stays confined to widening the CURRENT branch rather than
  suppressing a sibling — contrast finding #2) cannot desync
  `AttributeDecl.location` from what `shaderc` assigns: extra unused
  `VertexInputAttributeDescription` entries are spec-legal and ignored by
  the pipeline, consistent with S1/S2's original narrower quad already
  relying on this same tolerance.
- `compile_one_material`'s new `event=renderer.scene.shader_compile_error`
  diagnostic (`main.rs:2200-2231`) is bounded: `text::truncate_chars(...,
  300)` is char-based (not byte-based — no UTF-8 boundary panic risk,
  pinned by the pre-existing `truncate_chars_is_char_based` test),
  one line per failed compile at scene-load time only, never per-frame.
- `docs/FEATURE_COMPATIBILITY.md`/`AI-Skills/BETA_PLAN.md`'s S4 entry
  honestly states multi-effect compositing, external particle files, and
  `_rt_FullFrameBuffer`'s one-frame-stale paint order were investigated
  and NOT implemented this slice, with specific upstream citations
  (`CImage::setupPasses`/`configurePassTarget`/`pinpongFramebuffer`) rather
  than a vague "future work" note.
- No real Workshop payload committed: the new `scripts/smoke-scene.sh` S4
  case builds its `.pkg`/TEXV/shader fixtures synthetically via an inline
  Python heredoc (same pattern as S1-S3); the pixel oracle
  (`a_Color × a_Normal` → deterministic pure blue) actually distinguishes
  a correct widened-pipeline draw from a silent S1-fallback regression,
  not just "did it not crash."
- The new device test
  (`material_pipeline_draws_using_a_color_and_a_normal_attributes`,
  `vulkan.rs`) skips cleanly with a printed `"...: skipped (set
  KWE_TEST_DEVICE to run)"` line when no GPU/`KWE_TEST_DEVICE` is set,
  matching every other device test in the file.
- Provenance: the new `#if`-aware conditional-compilation tracker
  (`CondFrame`, `evaluate_if_expr`) correctly carries no `Borrowed-From`
  tag — it has no upstream equivalent (`ShaderUnit::preprocess` does not
  evaluate `#if`; `shaderc`'s own preprocessor is the actual arbiter at
  compile time, as the code's own comments note) — an original addition
  layered on the already-tagged, pre-existing `fold_declarations` port.
  `THIRD_PARTY.yml` needed no changes and has none.
