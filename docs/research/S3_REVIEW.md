# S3 adversarial review (effect passes / FBO chains / `_rt_*` targets)

Worktree `kwe-s3`, branch `beta-s3-effects`, commits `019b8ed`, `983f738`,
`37b562a` on `beta-b4-apply-quarantine` @ `9020b5e`. Read-only review, no
edits made. Prior bars: `docs/research/S1_REVIEW.md`, `S2_REVIEW.md`.

Counts: **3 MUST-FIX, 3 RECOMMENDED, 1 NIT**.

---

## MUST-FIX

### 1. `compile_effect_pass` allocates descriptor sets from the SAME fixed-size pool as `bind_material_layer`, with no combined accounting — the documented `MAX_EFFECT_PASS_BINDINGS` (256) bound is not actually honored

**Resolution: fixed.** Effect passes now allocate from their own `effect_descriptor_pool` (`vulkan.rs`), created lazily alongside `effect_render_pass` and sized by a new pure helper `LayerRenderer::descriptor_pool_capacity(max_sets)` shared with `material_descriptor_pool`'s own sizing -- the two pools are now fully independent budgets (`MAX_LAYERS` and `MAX_EFFECT_PASS_BINDINGS` each get their own pool, never shared). `compile_effect_pass`'s descriptor allocation now targets `self.effect_descriptor_pool`, not `self.material_descriptor_pool`. New unit test `descriptor_pool_sizing_matches_the_documented_bound` pins `descriptor_pool_capacity`'s output against both `MAX_LAYERS` and `MAX_EFFECT_PASS_BINDINGS` without a GPU. Teardown (`Drop`) destroys the new pool.

`crates/kwe-scene-renderer/src/vulkan.rs:1040-1044` creates
`material_descriptor_pool` once, at renderer init, sized for exactly
`MAX_LAYERS` (256) sets (`max_sets(MAX_LAYERS as u32)`,
`COMBINED_IMAGE_SAMPLER` count `MAX_LAYERS * MAX_MATERIAL_TEXTURES` =
2048, `UNIFORM_BUFFER` count `MAX_LAYERS` = 256) — this line is
untouched by the S3 diff. `bind_material_layer` (S2, unchanged) already
allocates up to `MAX_LAYERS` sets from this pool, one per model layer
with a bound material (vulkan.rs:1781-1783).

S3's `compile_effect_pass` (vulkan.rs:2535, descriptor allocation at
:2716-2718) allocates ADDITIONAL sets from the exact same
`self.material_descriptor_pool`, gated only by its own local counter
`self.effect_pass_bindings.len() >= MAX_EFFECT_PASS_BINDINGS` (256,
vulkan.rs:745-749) — a check that knows nothing about how many sets
`bind_material_layer` already took from the shared pool. Combined
worst-case demand is up to 256 (base materials) + 256 (effect passes) =
512 sets against a pool sized for 256 (and 4096 samplers / 512 UBOs
against capacity for 2048 / 256). Once the pool is exhausted,
`allocate_descriptor_sets` returns `VK_ERROR_OUT_OF_POOL_MEMORY`,
`compile_effect_pass` returns `Err`, and the caller
(`main.rs:1111-1126`) counts it as an ordinary `compile_failed` fallback
— not a crash, but a silent, scene-composition-dependent shrinkage of
effect-pass capacity: `docs/SCENE_FORMAT_V1.md`'s "Bounds" section
(`MAX_EFFECT_PASS_BINDINGS = 256 compiled effect-pass pipelines/scene`)
states this as if it were independently available, but the true
available headroom is `256 - (number of layers already holding a base
material binding)`, an emergent interaction nothing documents or tests.
A scene with, say, 200 ordinary material layers and one object with an
8-pass effect chain could see effect passes fail to bind even though
both individual caps (`MAX_LAYERS`, `MAX_EFFECT_PASS_BINDINGS`) look
satisfied in isolation.

**Minimal fix**: give effect passes their own descriptor pool sized to
`MAX_EFFECT_PASS_BINDINGS` (mirroring `material_descriptor_pool`'s own
sizing pattern), or raise `material_descriptor_pool`'s `max_sets`/pool
sizes to `MAX_LAYERS + MAX_EFFECT_PASS_BINDINGS` and document the
shared budget explicitly.

### 2. `effect_frame_actions`'s `Copy`/`Swap` entries have no cap — a scene built entirely from `command` passes (no shaders, no textures, no FBOs that need to exist) can queue over 100,000 full GPU submit+fence-wait round trips per frame

**Resolution: fixed.** New `MAX_EFFECT_FRAME_ACTIONS` (512) bound in `vulkan.rs`; `queue_effect_render` and `queue_effect_copy` are now `#[must_use] -> bool`, returning `false` (not queuing) once `effect_frame_actions.len() >= MAX_EFFECT_FRAME_ACTIONS`. `main.rs`'s call sites (`compile_material_layers`) check the return value and count a `false` under the `effect_frame_action_cap` fallback reason -- degrade, not crash, matching this module's existing contract. The device test `effect_chain_renders_through_an_intermediate_fbo` asserts its own (well-under-cap) `queue_effect_render` call returns `true`.

`queue_effect_copy` (`vulkan.rs:3129-3132`) pushes an
`EffectFrameAction::Copy` unconditionally — no bound check, unlike
`queue_effect_render`'s implicit cap (each `Render` action corresponds
1:1 with a successful `compile_effect_pass` call, which IS capped by
`MAX_EFFECT_PASS_BINDINGS`, vulkan.rs:745-749). The caller,
`main.rs:2464-2465` (`for (source, target) in &plan.commands {
renderer.queue_effect_copy(...) }`), queues one action per
`kwe_core::EffectCommand` in the chain plan with no additional gate.

A `command` pass needs neither a shader (`resolve_command_pass`,
`sceneeffect.rs`, only requires `command`/`source`/`target` strings) nor
a texture asset to resolve — it always parses successfully regardless
of whether `source`/`target` name a real FBO (existence is checked only
at RUNTIME inside `copy_effect_target`, vulkan.rs, which no-ops on a
miss). Bound only by the parse-time structural caps:
`MAX_PASSES_PER_EFFECT` (16) × `MAX_EFFECTS_PER_OBJECT` (32) = 512
command passes per object, referencing the SAME tiny `effects/x.json`
file repeatedly (`resolve_object_effects` re-resolves each `effects[]`
entry independently, `sceneeffect.rs:563-609` — nothing deduplicates a
`file` reference reused across entries) × `MAX_LAYERS` (256) objects =
up to 131,072 queued `Copy` actions from a handful of KB of hostile
`scene.json`/`effect.json`, no textures or shaders required at all.

`render_effect_chains` (`vulkan.rs:2775`) replays EVERY queued action
EVERY RENDERED FRAME (not once at load) via `copy_effect_target`, and
each one is its own full `begin_command_buffer` /
`queue_submit` / `wait_for_fences` (`FENCE_TIMEOUT_NS` = 1s) round trip
— not batched into the frame's other command-buffer work. Even at
negligible per-submit overhead this makes every frame take on the order
of 10²-10⁵ synchronous GPU round trips; at worst case (any single
submit legitimately taking close to the 1s timeout under load) a frame
could take on the order of a day. This is squarely the "bounded... frame
dimensions" rule in `AGENTS.md`'s engineering rules, and it is far
cheaper to trigger than S3's material/shader-compile paths (no shader
compilation, no texture decoding — pure JSON).

**Minimal fix**: add `MAX_EFFECT_FRAME_ACTIONS` (e.g. matching or a
small multiple of `MAX_EFFECT_PASS_BINDINGS`) and check
`self.effect_frame_actions.len() >= MAX_EFFECT_FRAME_ACTIONS` in
`queue_effect_copy` before pushing, silently dropping the action past
the cap (matching this module's degrade-not-refuse contract) with a
`fallback_reasons` diagnostic entry from the `main.rs` call site.

### 3. No guard against an effect pass sampling the SAME render target it writes into — an unguarded Vulkan feedback loop (read via descriptor set + write via color attachment, same image, same render-pass instance)

**Resolution: fixed.** New pure predicate `main.rs::effect_pass_samples_its_own_target(texture_slots, target_name)`, checked in `compile_material_layers`'s intermediate-pass loop before `compile_effect_pass` is ever called -- a self-referencing pass is skipped with a new `effect_self_reference` fallback reason, mirroring `copy_effect_target`'s existing `source == target` guard for the command-pass case. Unit-tested directly (`effect_pass_samples_its_own_target_detects_the_feedback_loop`) and end-to-end through `plan_effect_chain`'s resolved output (`plan_effect_chain_resolves_a_self_referencing_pass_to_a_detectable_shape`), which also confirms the RECOMMENDED #5 per-object name scoping (below) does not break the detection -- the scoped target name and the scoped `RenderTarget` slot name it produces are compared literally, both scoped identically.

Nothing in `sceneeffect::resolve_material_pass`
(`crates/kwe-core/src/sceneeffect.rs:357-460`),
`main.rs::plan_effect_chain` (`main.rs:1853`), or
`vulkan.rs::compile_effect_pass` (`vulkan.rs:2535`) checks whether a
material pass's own resolved texture slots (from `bind`,
`usertextures`, `textures`, or the base material) name the SAME `_rt_*`
FBO the pass itself targets (`pass_object.get("target")`,
`sceneeffect.rs:446-449`). A pass such as
`{"material": "...", "target": "_rt_Foo", "bind": [{"index": 0, "name":
"_rt_Foo"}]}` parses and compiles cleanly: `resolve_texture_slots`
(vulkan.rs, shared by `bind_material_layer` and `compile_effect_pass`)
binds `effect_targets["_rt_Foo"]`'s current image view into the
descriptor set at compile time, while `render_effect_pass_binding`
(vulkan.rs) renders into that SAME `EffectFbo`'s image as the color
attachment (`load_op: CLEAR`) every frame. Sampling an image through a
descriptor while it is bound as a color attachment being written in the
same render-pass instance is a Vulkan feedback loop — undefined per
spec absent `VK_EXT_attachment_feedback_loop_layout` (not present
anywhere in this diff), and flagged by validation layers. Contrast with
`copy_effect_target` (vulkan.rs), which explicitly guards the analogous
`command`-pass case (`if source == target { return Ok(()); }`) — no
equivalent guard exists for the material-pass case, and the task brief
explicitly calls out "pass binding its own target" as an item to check.

Not necessarily hostile-only: a legitimate-looking effect (a
single-target accumulation/feedback pass, as opposed to the real
corpus's two-FBO ping-pong pattern) would hit this too, with
driver-dependent, undefined visual results — not a crash by design, but
outside the "honest fallback" contract this slice otherwise holds to
(nothing degrades or is diagnosed here; it silently compiles and runs).

**Minimal fix**: in `compile_effect_pass` (or its caller in
`compile_material_layers`), reject/skip a targeted pass whose own
resolved texture slots include a `RenderTarget` name equal to `target`
— fall back the same way an unresolvable reference already does
(`dummy_texture`), with a `fallback_reasons` entry
(e.g. `effect_self_reference`).

---

## RECOMMENDED

### 4. No aggregate byte budget or de-duplication for effect-triggered asset reads — S3 multiplies the per-object worst-case read volume roughly 400x over S1/S2 with no equivalent of the base-texture path's `texture_budget_allows`/`used_bytes`

**Resolution: fixed.** New `MAX_EFFECT_ASSET_READ_BYTES` (256 MiB, matching the base-texture path's `MAX_TOTAL_TEXTURE_BYTES` order of magnitude) and a pure `effect_asset_budget_allows(used_bytes)` predicate (mirrors `textures::texture_budget_allows`'s existing pattern), threaded through a per-scene-load `used_effect_bytes` accumulator in `load_model_textures`: the `AssetLookup` closure passed to `resolve_object_effects` now checks the budget before every read and stops (returning `None`, `resolve_object_effects`'s own honesty rule then degrades that reference, never crashes) once exceeded, with a one-time `event=renderer.scene.effect_asset_budget_exceeded` diagnostic. Boundary pinned by `effect_asset_budget_allows_pins_the_boundary` without allocating anywhere near 256 MiB in a test. No de-duplication (memoization of repeated `texture_ref`s) was added -- the aggregate cap alone bounds the worst case, which was the minimal fix suggested.

`resolve_object_effects` → `parse_effect_spec` → `resolve_material_pass`
(`crates/kwe-core/src/sceneeffect.rs`) calls the caller-supplied
`lookup` fresh, with no caching, once per `material` reference (up to
`MAX_PASSES_PER_EFFECT` × `MAX_EFFECTS_PER_OBJECT` = 512 times per
object) and once per texture slot per pass (up to
`MAX_MATERIAL_TEXTURES` = 8 more per pass). Each underlying read is
bounded per-file (`MAX_TEXTURE_SOURCE_BYTES` = 64 MiB via
`resolve_layer_image`/`confined_read`, `MAX_EFFECT_JSON_BYTES` = 1 MiB
via `bounded_json`), but nothing tracks or caps the SUM across a
scene's effect resolution the way `load_model_textures`'s `used_bytes`
+ `texture_budget_allows` (`crates/kwe-scene-renderer/src/textures.rs:66-72`)
caps the base-texture path to `MAX_TOTAL_TEXTURE_BYTES` (256 MiB). A
single small real texture file referenced repeatedly across many
`bind`/`usertextures` overrides (nothing deduplicates identical
`texture_ref` strings) is re-read and re-allocated on every reference —
up to 512 material-pass lookups × 8 texture slots × 256 objects, each
potentially the full 64 MiB cap. This is a materially larger surface
than the (already-unflagged-by-S1/S2-review) per-layer baseline: S1/S2
made at most ~10 lookups per layer; S3 raises that to up to ~4096 per
object via `effects[]`.

**Minimal fix**: thread a `used_bytes`/budget accumulator (or at least a
`seen: HashSet<String>` memo of already-read `texture_ref`s) through
`resolve_object_effects`'s `AssetLookup` closure the same way
`load_model_textures` already does for base textures, capping total
effect-related bytes read per scene load.

### 5. `effect_targets`'s global (not per-object) namespace means an object whose effect chain is computed but NOT applied (the `base_is_passthrough == false` safety branch, `main.rs:938-941`) still executes its targeted passes and writes into shared-name FBOs another object may sample

**Resolution: fixed** (beyond the recommended diagnostic-counter minimal fix -- the namespace itself is now scoped, which removes the interaction structurally). New `main.rs::scoped_target_name(layer_index, name)`: every effect-declared FBO name (a `fbos[]` entry, a pass's own `target`, a `bind`/texture-slot `RenderTarget` reference, a `command`'s literal `source`/`target`) is suffixed `#obj<layer_index>` before it ever reaches `effect_targets`/`compile_effect_pass`/`queue_effect_copy`/`prepare_effect_targets` -- EXCEPT the one deliberately scene-wide name, `_rt_FullFrameBuffer`, which stays global. Applied consistently in `plan_effect_chain` (the `"previous"` sentinel already propagates whatever scoped form was last assigned) and `effect_target_requests`. Two different objects declaring the same raw `fbos[]` name can no longer alias, by construction -- pinned by `plan_effect_chain_scopes_fbo_names_so_different_objects_never_alias` and `scoped_target_name_leaves_full_frame_buffer_global_and_scopes_everything_else`. `docs/SCENE_FORMAT_V1.md`'s "Effects and render targets" section and `vulkan.rs`'s `effect_targets` doc comment both updated to describe the scoped namespace instead of the prior "global, not observed in the corpus" wording.

`compile_material_layers` (`main.rs`) always runs the `for
(pass_material, target_name) in &plan.intermediate` loop
(`main.rs` ~2089-2126) whenever `plan` is `Some`, regardless of whether
`base_is_passthrough` gated the FINAL pass from replacing this layer's
own material. This is intentional (documented: earlier passes may feed
a later one) but combined with `effect_targets`'s documented global
namespace (`vulkan.rs`, `effect_targets`'s doc comment: "two different
objects declaring the SAME fbo name share one instance... not observed
in the local corpus") it means a "real photo, effect not applied"
object's intermediate passes are not actually inert — they still run
every frame and can silently feed a DIFFERENT object's chain that
happens to reference the same `_rt_*` name, an interaction the existing
`event=renderer.scene.effects` diagnostic line does not distinguish
(it counts `objects`/`passes`/`fallback` in aggregate, not "chain
computed but final-material override skipped"). Already flagged as an
accepted scope limit in the code; recommend a distinct diagnostic
counter (or at least a code comment cross-reference between the two
doc comments, which currently don't reference each other) so the
interaction is discoverable, not just individually documented.

### 6. The corpus-wide before/after byte-identity claim is not backed by a committed, reproducible script

**Resolution: fixed.** New `scripts/scene-corpus-byte-identity-sweep.sh`, committed (not wired into `check.sh`, matching the minimal-fix suggestion): parameterized by `KWE_CORPUS_DIR` (SKIPPED with exit 0 when unset/missing, matching `smoke-corpus-pkg.sh`'s convention -- no Workshop payload committed by or with it) and optional `KWE_REFERENCE_RENDERER`/`KWE_CANDIDATE_RENDERER`/`KWE_ASSETS_DIR`/sizing knobs. Re-run against the real local corpus comparing the pre-S3 baseline binary against the fully-fixed post-review candidate and reported below.

`AI-Skills/BETA_PLAN.md`'s S3 changelog entry states: "a full
before/after pixel comparison across the 54 locally-comparable scenes
found ZERO regressions (byte-identical mean/distinct-color sampling,
`/tmp` sweep scripts, not committed)" — the task brief explicitly asks
whether this evidence is reproducible; per the author's own words, it is
not. Unlike `scripts/smoke-scene.sh`'s pixel-oracle S3 case (committed,
deterministic, CI-runnable), the headline "60/60 scenes apply, zero
regressions" claim rests on scripts that exist only in `/tmp` on the
machine that ran them and cannot be re-run by another engineer, in CI,
or on a future regression to confirm the claim still holds.

**Minimal fix**: commit the sweep script (even as a `scripts/`
maintenance tool not wired into `check.sh`), or downgrade the claim's
phrasing to note it is not independently reproducible.

---

## NIT

### 7. `EffectCommand::Swap` executes identically to `Copy` (one-directional) — well-documented, but worth a runtime diagnostic distinguishing the two

**Resolution: fixed.** `EffectChainPlan.commands` now carries the original `kwe_core::EffectCommand` alongside each `(source, target)` pair; `compile_material_layers` counts how many resolved commands are `Swap` into a new `swap_used` counter, emitted as `swap_used=N` on the existing `event=renderer.scene.effects` line.

`vulkan.rs`'s `EffectFrameAction::Copy` doc comment and
`docs/SCENE_FORMAT_V1.md` ("Effects and render targets") both honestly
document that `command: swap` executes as a one-directional copy rather
than a true pointer swap, and why (re-resolving later passes'
already-baked descriptor views isn't supported by this renderer's
load-time-only binding design). This is adequately disclosed — not a
finding on its own — but nothing at runtime distinguishes how often a
scene actually uses `swap` (where the simplification changes observable
behavior across frames for a true ping-pong pattern) from `copy` (where
it doesn't). A `swap_used` count alongside the existing
`event=renderer.scene.effects` line would make the simplification's
real-world exposure visible without re-litigating the design decision.

---

## Verified while reviewing (no finding)

- `create_effect_fbo`/`try_create_effect_target` (`vulkan.rs`) properly
  clamp width/height to `MAX_EFFECT_TARGET_DIMENSION` (4096) and check
  the clamped values against `MAX_EFFECT_TARGET_BYTES` (256 MiB
  cumulative) BEFORE allocating — a hostile `fbo.scale` near zero (only
  bounds-checked for `> 0.0 && finite` in `sceneeffect.rs:513-518`)
  cannot produce an oversized allocation even though
  `effect_target_requests` (`main.rs`) computes raw width/height with no
  clamp of its own (float-to-int cast saturates to `u32::MAX` on
  overflow, but the downstream clamp catches it regardless).
- `copy_effect_target`'s `source == target` no-op guard
  (`vulkan.rs`) correctly handles the trivial command-pass
  self-reference case (contrast finding #3, the material-pass case).
- Teardown (`impl Drop for LayerRenderer`, `vulkan.rs:3562-3612`) frees
  every `effect_pass_bindings` entry's pipeline/UBO/textures, every
  `effect_targets` FBO's framebuffer/view/image/memory, and the shared
  `effect_render_pass` if created — no leak found on the normal teardown
  path. Fallback paths inside `create_effect_fbo`/`compile_effect_pass`
  correctly unwind partially-created resources on each intermediate
  error (checked step by step).
- Every new fence-touching call (`clear_effect_fbo`,
  `render_effect_pass_binding`, `copy_effect_target`,
  `snapshot_full_frame_buffer`) is wrapped by its `main.rs` caller with
  an `is_fence_timeout` check routing to `reject_render` — the S2
  review's fence-timeout invariant is honored at every new S3 site.
- Per-draw UBO writes in `compile_effect_pass` flush mapped memory
  before unmapping (`vulkan.rs`, mirroring the S2 review fix) — the
  write-once contract is correctly handled (unmap immediately rather
  than keep a stale pointer).
- Preflight/worker honesty: `resolve_object_effects` never fails an
  object (module doc comment, upheld by every code path read); the
  `render_target_only_without_effects` guard and the
  bare-passthrough-only override restriction are both documented in
  code comments AND `docs/SCENE_FORMAT_V1.md`, and both are counted via
  the existing bounded `fallback_reasons`/`eprintln` diagnostic
  convention (one line per distinct reason per scene load, not
  per-frame).
- `FEATURE_COMPATIBILITY.md`/`SCENE_FORMAT_V1.md` are honest about the
  mesh/puppet scope: 55/60 `unsupported_vertex_format` fallbacks remain
  (vs 53 pre-S3), explicitly stated as "essentially unchanged," with the
  precision-qualifier fix's real but not corpus-moving effect called out
  plainly rather than overstated; puppet MESH geometry is documented as
  researched-but-not-ported with "zero local corpus usage" as the stated
  reason, not silently dropped.
- Provenance: every adapted function/struct in `sceneeffect.rs`/
  `vulkan.rs` carries its own `Borrowed-From` comment;
  `THIRD_PARTY.yml` lists all five adapted upstream files plus the
  `swap`-is-an-original-completion and puppet-not-ported notes.
- No real Workshop payload committed: the new `scripts/smoke-scene.sh`
  S3 case builds its `.pkg`/TEXV/shader fixtures synthetically via an
  inline Python heredoc, matching the S1/S2 pattern.
- The two new device-gated tests
  (`effect_chain_renders_through_an_intermediate_fbo`,
  `unwritten_effect_target_samples_transparent_black_not_garbage`) skip
  cleanly with a printed `"...: skipped (set KWE_TEST_DEVICE to run)"`
  line, matching every other device test in the file — no GPU, no
  failure.
