# S4b adversarial review

Scope: `git diff 360b986...HEAD` on `beta-s4b-particles`
(`b51b0c2`, `460c80a`, `f5b01b0`, `52578bc`) — external particle
definition files (component model) and four `compile_failed`
shader-preprocessor root-cause fixes. Cross-checked against the vendored
upstream checkout at `/home/qcv123/gitClones/linux-wallpaperengine`
(`HEAD` = `b016d7d1fdcf4e5fd2f9c9fa420a8aaa07fee02d`, matching every cited
commit in this diff) and prior bars `S1_REVIEW.md`…`S4A_REVIEW.md`.

No MUST-FIX found. Two RECOMMENDED items on the `#include`-placement fix
(3a), two NIT items. Everything else — bounds, panics, determinism, the
global particle cap, B2 preflight/worker agreement, the `max()`/
`g_Point<N>`/array-uniform fixes, provenance citations, and the headline
metrics — verified correct on direct inspection and against upstream
source.

---

## RECOMMENDED

### 1. `find_main_insertion_point` is comment-blind — untested, undisclosed limitation shared with (not introduced beyond) upstream

`crates/kwe-scene-renderer/src/shaderpre.rs:466-484` (`find_main_insertion_point`)
does a raw word-boundary text search for `main(` over the *unmodified*
shader source — comments are never stripped anywhere in this module
before `resolve_includes` runs (`preprocess`, `shaderpre.rs:1531`, calls
`resolve_includes` on the raw `source` parameter). A shader with a
textual `main(` occurrence inside a `//` or `/* */` comment that
*precedes* the real function definition — e.g. `// see main() below` as
an early file comment — makes `find_main_insertion_point` return the
comment line's position (the first match wins), splicing the accumulated
`#include` text there instead of at the real `main(`. If that position
sits before the global declarations an included function depends on
(`shine_gaussian.frag`'s exact failure shape, per this fix's own doc),
this silently reproduces the `'g_Texture0' : undeclared identifier`
class of bug the fix exists to close — for a narrower, untested input
shape.

Mitigating context, verified directly against the vendored upstream
source: upstream's own `ShaderUnit::preprocessIncludes` has the identical
blind spot. Its insertion search (`ShaderUnit.cpp:208`,
`this->m_preprocessed.find(" main", end)`) is a literal substring search
with **no comment-awareness either** — a comment containing `" main("`
matches upstream's own logic just as readily. So this is not a new
deviation the port introduces; it's a faithful (if accidental) inheritance
of upstream's own naive placement search. It is, however, un-mentioned in
the port's own doc comment (which discloses the `#if`-stack cut but not
this one) and has no test coverage — `main_like_identifiers_do_not_confuse_the_insertion_point_search`
(`shaderpre.rs:1968`) only tests an in-code identifier (`mainColor`), not
a comment.

**Minimal fix**: add a one-line doc caveat noting the shared upstream
limitation, plus a defensive test pinning current (accepted) behavior for
`// ... main() ...` preceding the real function — so a future change to
the search can't silently make this worse without a test noticing. A
comment-skipping scan is a larger, optional follow-up, not required for
upstream fidelity.

### 2. Port's splice point (immediately before `main(`) does not match upstream's actual insertion point (after the last attribute/varying/uniform, `#if`-stack adjusted) — citation also undersells the divergence

Verified against the vendored upstream source in full: `ShaderUnit::preprocessIncludes`
(`ShaderUnit.cpp:136-312`, not `136-171` as cited by this diff's
Borrowed-From comment, `shaderpre.rs:495-497` and the mirrored citation in
`THIRD_PARTY.yml`) inserts the collected include text **after the last
`attribute`/`varying`/`uniform` declaration before `main`** (`ShaderUnit.cpp:224-249`),
walking an `#if`/`#endif` stack (`ShaderUnit.cpp:254-312`) to avoid
landing inside a dead branch. The port's `find_main_insertion_point`
instead always inserts at the line immediately before `main(` itself —
strictly *later* in the source than upstream's real insertion point
whenever the file has other code (e.g. a local, non-`main` helper
function) between the last global declaration and `main`.

For the specific three corpus files this fix targets
(`shine_gaussian.frag`/`godrays_gaussian.frag`/`blur_precise_gaussian.frag`,
`#include` as literally the first line, nothing but the one uniform
declaration before `main`) the two insertion points coincide and the fix
is correct as verified by
`include_content_lands_before_main_not_at_its_own_line_position`
(`shaderpre.rs:1911`). But for a hypothetical corpus shader with a local
helper function between its declarations and `main` that itself calls an
included function, upstream's placement (before the helper) would compile
while the port's placement (after the helper, right before `main`) would
not — GLSL requires a function to be declared before use, like C. This
divergence is real and unverified against the actual corpus (not
committed here), and the doc's "matching upstream's real placement
strategy" claim (`shaderpre.rs:459`) overstates the match given the
citation only covers the collection half of upstream's algorithm, not the
placement-search half.

**Minimal fix**: widen the Borrowed-From citation to the full function
range (`ShaderUnit.cpp:136-312`) so a future auditor sees the placement
logic being adapted, and soften the "matching upstream's real placement
strategy" claim to name the specific divergence (splice-before-`main`
vs. upstream's splice-after-last-declaration) rather than implying full
parity. Functionally optional unless corpus evidence surfaces the helper-function
shape.

---

## NIT

### 3. `total_len` over-counts nested `#include` bytes (safe direction only)

`collect_includes` (`shaderpre.rs:378-455`) computes each include's
contribution to `*total_len` as `accumulated.len() - accumulated_before`
measured around the *entire* recursive call, including all of that
nested include's own already-counted contribution to `*total_len`. For
N levels of nesting the deepest bytes get counted once per enclosing
level, so `*total_len` can measurably exceed `accumulated`'s real length.
This only makes `MAX_PREPROCESSED_BYTES` (`PreprocessError::SizeExceeded`)
trigger *earlier* than the real byte count would justify — safe (rejects
sooner), not a bound bypass, but could reject a legitimate deeply-nested
corpus shader before it would otherwise hit the real cap. `MAX_INCLUDE_DEPTH`/
`MAX_INCLUDE_COUNT` (pre-existing, unchanged by S4b) already bound the
nesting this can occur over, so the practical impact is small.

**Minimal fix**: read `accumulated.len()` once at the end of
`resolve_includes` and compare that single value against
`MAX_PREPROCESSED_BYTES`, rather than accumulating deltas at every
recursion level.

### 4. Particle-file JSON reads reuse the 64 MiB texture read bound before the 1 MiB particle-file cap applies (inherited pattern, not new)

`kwe_core::particlefile::resolve_particle_file`'s `lookup` closure is
composed from the same `resolve_layer_image`/`reader.read_entry_bounded`
calls S1 already uses for `model.json`/`material.json`
(`crates/kwe-scene-renderer/src/main.rs:1141-1272`, `MAX_TEXTURE_SOURCE_BYTES`
= 64 MiB, `textures.rs:30`), so a particle file up to 64 MiB is fully read
into memory before `kwe_core::particlefile::bounded_particle_json`'s 1 MiB
check (`crates/kwe-core/src/particlefile.rs:52-61`) rejects it. Bounded
either way (never unbounded), just a wasted read for a hostile
oversized file — and this exact pattern already exists for S1's
`model.json`/`material.json` reads, so it is not new to S4b. Not worth
fixing in isolation; would need a shared smaller read-cap plumbed through
the same lookup closures S1 already built.

---

## Verified while reviewing (no finding)

- **Bounds, no panics**: `MAX_PARTICLE_FILE_BYTES` (1 MiB) checked before
  `serde_json::from_slice` runs
  (`kwe-core/src/particlefile.rs:52-61`, `oversized_file_is_rejected_before_parsing`
  test); every numeric field in
  `kwe-scene-renderer/src/particlefile.rs` goes through `scalar`/`clamped_scalar`/
  `vec2`/`vec2_clamped`/`color3`, all of which filter non-finite values
  and clamp to a documented range before any arithmetic — confirmed by
  `hostile_fields_stay_bounded_never_panic` (NaN/inf/huge/negative/wrong-type
  inputs). Emitter/initializer/operator arrays capped at
  `particles::MAX_COMPONENT_ITEMS` (16, `item_counts_are_bounded_by_max_component_items`
  test). No `unwrap()`/`expect()`/unchecked indexing on
  attacker-controlled data found in either `particlefile.rs`;
  `particles.rs`'s one `.expect(...)` (`step_fixed_component is only
  called when component is Some`, `particles.rs:740`) is guarded by its
  only call site's own `is_some()` check (`particles.rs:676`); the one
  attacker-adjacent index (`self.emitter_accumulators[index]`,
  `particles.rs:748`) is provably in-bounds — `emitter_accumulators` is
  sized to `component.emitters.len()` at construction
  (`particles.rs:571`) from the SAME `ComponentModel` value the loop
  later enumerates over (`particles.rs:744`), and nothing mutates
  `component`/`emitter_accumulators` independently after that.
- **Global particle cap cannot be raised by a file**: `clamp_max_count`
  (`particles.rs:1435-1436`) clamps a file's `maxcount` to `MAX_PARTICLES`
  (4096) regardless of the authored value (`clamp_max_count_bounds`
  test); the number of particle systems a scene can register is capped
  independently at `MAX_PARTICLE_SYSTEMS` (16, `scene.rs:741`,
  pre-existing M3f, applies uniformly to file-ref and inline particle
  objects alike). A file's `rate` is deliberately let through up to
  100,000/s (`particlefile.rs:152-166`, documented reasoning), but actual
  per-step spawn work is independently bounded by
  `MAX_SPAWN_ACCUMULATOR` (65536) and the `free = max_count -
  particles.len()` take-at-most-free-slots logic
  (`particles.rs:749-778`) — a hostile rate cannot inflate real work
  beyond the existing per-system/per-step caps.
- **Deterministic RNG unaffected**: splitmix64 seeded by system index
  (pre-existing); a component system's per-particle oscillator
  phase/speed for `Turbulence` operators is resolved once at
  `ParticleSystemState::from_spec` construction, before any per-step
  draw (`particles.rs:568-598`) — same determinism contract the flat
  model already relies on for its smoke oracles.
- **B2 preflight/worker agreement, extended correctly to particle
  files**: `summarize_scene_objects_resolved`
  (`kwe-core/src/sceneobjects.rs:280-355`) now walks every
  `SceneObjectKind::ParticleFile` object through
  `crate::particlefile::resolve_particle_file`, the exact function the
  worker's `load_particle_file_definitions`
  (`kwe-scene-renderer/src/main.rs:1476-1538`) calls too — both paths
  bottom out in `scenemodel::resolve_material`
  (`kwe-core/src/scenemodel.rs:223`), which already carries the S1-review
  fix (finding #2 of `S1_REVIEW.md`, the TEXV header pre-check via
  `texvheader::check_header`) — so the preflight/worker decode
  disagreement that review closed for models is inherited-closed for
  particle files too, not just accepted-on-faith. Attempt count capped
  at the new `MAX_PARTICLE_FILE_RESOLUTIONS` (256, mirrors
  `MAX_MODEL_RESOLUTIONS`, S1 review finding #3's fix), proven bounded by
  `particle_file_resolution_attempts_are_capped`
  (`sceneobjects.rs`, asserts both the resolved count and the
  lookup-closure call count stay at/under the cap for 306 declared
  objects). The B2 refusal reason text was updated consistently
  (`"...whose material could not be resolved"`, replacing the old
  blanket "never read yet" text) and only counts genuinely-unresolved
  files.
- **Confinement matches S1's lookup rules exactly**: `main.rs:1163-1272`
  wires `load_particle_file_definitions`'s lookup closure identically to
  `load_model_textures`'s — scene directory then assets root for loose
  files, pkg entries then pkg directory then assets root for packages —
  reusing `resolve_layer_image`'s existing path-confinement (relative-only,
  no `..`/absolute components, canonicalize + `starts_with(root)`,
  regular-file-only, symlink-safe per
  `resolve_layer_image_confines_to_the_content_root`, `main.rs:3969`) and
  the same shared texture-memory budget (`texture_budget_allows`) — no
  new confinement surface introduced.
- **`#define max(x, y) max(y, x)` restoration matches upstream verbatim**:
  confirmed byte-for-byte against the vendored checkout,
  `ShaderUnit.cpp:31` (`"#define max(x, y) max (y, x)\n"`), at the exact
  commit (`b016d7d1`) cited throughout this diff. Terminates via the
  standard C-preprocessor "blue paint" rule (a macro is never
  re-expanded inside its own in-progress substitution) — not new
  recursion, and this doc's own explanation of that rule is accurate.
- **`g_Point<N>` narrowing**: `narrowing_swizzle` (`shaderpre.rs:684-691`)
  only ever *drops* trailing components (`.xy`/`.xyz`) from the UBO's
  `vec4` slot, never invents data; a `vec4`-declaring shader (the common
  case) gets the empty swizzle, byte-identical to pre-fix output
  (`point_uniform_declared_as_vec4_is_unswizzled` test). A shader
  declaring a type with no narrowing entry (e.g. `float`, `mat4`) falls
  through to the pre-existing bare-vec4 default and fails at `shaderc`
  compile time exactly as it already did before this fix — no new
  failure class, only the vec2/vec3 cases are newly fixed.
- **Array-uniform zero-fill is bounded**: `MAX_ZERO_ARRAY_LEN` = 1024
  (`shaderpre.rs`), `parse_array_declarator` rejects size 0 or > 1024
  (falls back to the pre-existing unsupported/pass-through path, never
  builds an unbounded literal list) — both directions tested
  (`array_uniform_zero_fills_instead_of_leaving_a_loose_declaration`,
  `array_uniform_past_the_zero_fill_bound_is_left_unsupported_not_expanded`).
- **Provenance citations verified against upstream source**: every
  `create*Emitter`/`create*RandomInitializer`/`create*Operator` function
  name cited in the new `THIRD_PARTY.yml` entry
  (`createBoxEmitter`/`createSphereEmitter`/the `*RandomInitializer`
  family/`createMovementOperator`/`createAlphaFadeOperator`/
  `createSizeChangeOperator`/`createColorChangeOperator`/
  `createOscillateAlphaOperator`/`createOscillateSizeOperator`/
  `createControlPointAttractOperator`/`createTurbulenceOperator`) exists
  in `CParticle.cpp` at the cited commit. `sizerandom`'s documented
  deviation (dropping upstream's extra `/2.0`) is correctly reasoned and
  verified: upstream's `createSizeRandomInitializer`
  (`CParticle.cpp:724-739`) does divide by 2 because its own renderer
  consumes `p.size` as a half-extent directly, while this renderer's
  shared `build_vertex_bytes` (`particles.rs:1097`, `half = size *
  0.5`) already halves once — replicating upstream's extra `/2` would
  genuinely halve twice, exactly as the doc claims. `oscillatealpha`/
  `oscillatesize`'s `phase_max + TAU` phase-range widening
  (`particlefile.rs`/`particles.rs`) matches upstream's
  `createOscillateAlphaOperator`/`createOscillateSizeOperator`
  (`CParticle.cpp:1523`, `randomFloat(m_rng, phaseMin, phaseMax + 2.0f *
  glm::pi<float>())`) exactly.
- **Metrics honesty (item 4 of the review brief)**: the review brief's
  framing ("compiled 189→183, fallback 24→30, compile_failed 4→10") reads
  as a regression, but this ordering does **not** match what is actually
  committed. `docs/FEATURE_COMPATIBILITY.md`'s `content.scene2d` row and
  `AI-Skills/BETA_PLAN.md`'s S4b change-log entry both state the numbers
  unambiguously and consistently in the true direction: candidate
  (post-S4b) **189** compiled vs. **183** pre-S4b baseline (an increase),
  fallback **dropped** from 30 to 24, `compile_failed` **dropped** from
  10 to 4 — four named scenes each lost 1-2 `compile_failed` shaders
  (`2370927443`: 2→0, `2468489223`: 1→0, `2685383861`: 1→0,
  `3189982144`: 2→0) and the docs explicitly state "ZERO scenes gained
  one (no regression)". This is a real improvement, correctly and
  specifically documented (named scenes, per-scene deltas, an explicit
  regression-count assertion) — no MUST-FIX here; the reversed reading
  exists only in the review brief's framing, not the repository.
- **No Workshop payloads committed**: the only new fixture in this slice
  is `scripts/smoke-scene.sh`'s S4b case, built synthetically via the
  existing inline Python heredoc pattern (a boxrandom emitter at the
  origin, zero velocity, long lifetime — deterministic), verified through
  the real daemon lane with a pixel-region oracle AND an explicit
  assertion that `event=renderer.scene.particle_file_skip` did *not*
  fire (proving actual resolution, not just "didn't crash").
- **No new device tests in this slice** (S4b doesn't touch
  `vulkan.rs`/GPU device-test code) — nothing new to check for
  clean-skip behavior; the existing `KWE_TEST_DEVICE` gates are
  untouched.
