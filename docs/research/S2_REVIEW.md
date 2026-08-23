# S2 adversarial review (material shaders)

Worktree `kwe-s2`, branch `beta-s2-shaders`, commits `0f04d67`, `999877f`,
`68ce6a2` on `beta-b4-apply-quarantine`. Originally a read-only review;
findings below are each marked with a **Resolution** line recording what
was fixed and how, applied in a follow-up commit
(`fix(s2): adversarial review findings`) on the same branch.

Counts: **2 MUST-FIX, 3 RECOMMENDED, 2 NIT** — all 7 addressed (see each
finding's Resolution line).

Verified while reviewing: `cargo check -p kwe-scene-renderer --offline`
and `cargo test -p kwe-scene-renderer --offline` (shaderpre:: 22 tests,
materialshader:: 7 tests) both pass against the system `libshaderc`
(2026.3.1) with no network access — the "builds offline" claim in
`Cargo.toml`/`THIRD_PARTY.yml`/`PKGBUILD` holds. `MAX_PIPELINES_PER_SCENE`
(64), descriptor-pool sizing (bounded by `MAX_LAYERS`), `Drop` teardown
order, and the B2 honesty-contract wording in `docs/SCENE_FORMAT_V1.md`
("a layer never stops drawing because its material shader could not be
used") all check out — preflight/worker agreement is unchanged (S2 is
additive-only, gated after the S1 drawability decision).

---

## MUST-FIX

### 1. `material.json`'s `combos` keys are unsanitized and land raw in `#define NAME VALUE` — a hostile material.json (no shader-text control needed) can inject GLSL/preprocessor text into a trusted, vetted shader

**Resolution: fixed.** `shaderpre::is_valid_combo_name` enforces `^[A-Za-z_][A-Za-z0-9_]{0,63}$` on every combo name before it can reach a `#define` line. `preprocess` now validates every `material_combos` key (material.json's own `combos` map -- the external, untrusted vector the finding calls out) up front and returns `PreprocessError::InvalidComboName` if any fails, which main.rs's existing `Err(_) => "preprocess_failed"` catch-all already routes to the material's ordinary fallback path with a diagnostic -- no new match arm needed. `scrape`'s own `// [COMBO]` discovery drops (does not define) an invalid name instead of hard-failing, since that source is the shader's own asset text, not a separate untrusted file. New tests: `combo_name_with_embedded_newline_is_rejected_not_injected` (the literal `"FOO\nBAR"` proof-of-no-injection the finding asked for), `combo_name_with_invalid_characters_is_rejected`, `valid_combo_name_shapes`, `discovered_combo_with_invalid_name_is_dropped_not_defined`.

`crates/kwe-core/src/scenemodel.rs:68-70,319-322` records `combos` as
`pass0.get("combos").and_then(Value::as_object).cloned().unwrap_or_default()`
— a `serde_json::Map<String, Value>` copied "exactly as written," no key
validation. `crates/kwe-scene-renderer/src/main.rs:1450-1454` turns every
entry into `(name.clone(), value.as_i64()?)` for `MaterialSpec::combos`
with no filtering against the shader's own scraped combo names.
`crates/kwe-scene-renderer/src/shaderpre.rs:653-656,674-676`:

```rust
for (name, value) in material_combos {
    combos.insert(name.clone(), *value);
}
...
for (name, value) in &combos {
    source_out.push_str(&format!("#define {} {}\n", name.to_uppercase(), value));
}
```

`name` is inserted into `source_out` completely unchecked. A JSON string
value can carry an escaped `\n` (`"combo": "FOO\nBAR"` is valid JSON);
`serde_json` decodes that into an actual LF byte in the Rust `String`,
even though the whole JSON object sat on one physical line of
`material.json`. The emitted text becomes two lines:

```
#define FOO
BAR 0
```

— breaking out of the `#define` and injecting arbitrary additional GLSL
text (further `#define`s, `#extension` lines, code) into the *final
compiled shader*, using only a crafted `material.json` `combos` key. This
does **not** require the attacker to also control the `.frag`/`.vert`
shader source — a hostile Workshop package can point `shader` at any
built-in/trusted corpus shader and inject through `combos` alone, which
is a materially different (and worse) threat than "the shader text is
already fully attacker-owned so this adds nothing." The value side is
already safe (`as_i64()` — no string injection there); only the key is
unchecked. Compare `shaderpre::parse_decl`/`scrape`, which only ever
extract names from a single source line and so cannot smuggle an embedded
newline this way — the material.json path is the one gap.

Practical impact is bounded (worst case is a shaderc compile failure ->
existing fallback, or valid-but-different GLSL a hostile package
author already controls anyway through its own shader choice), but it
violates AGENTS.md's "parse untrusted metadata without exceptions
escaping a service boundary" in spirit — this is metadata, not code, and
should not be able to reach the compiler as raw text.

**Minimal fix**: validate every combo NAME (from both
`scrape`'s `[COMBO]` scrape and `material_combos`) against a GLSL
identifier pattern (`^[A-Za-z_][A-Za-z0-9_]*$`) before inserting into
`combos`; drop or reject entries that fail instead of emitting them.

### 2. `bind_material_layer`'s `FenceTimeout` is swallowed as an ordinary fallback instead of terminating the process, breaking the shared-fence safety invariant every other call site in this file honors

**Resolution: fixed.** `compile_material_layers`'s `bind_material_layer` match arm now mirrors every other fence-touching call site in main.rs: on `material_bind_error_is_fatal` (a small pure wrapper around `is_fence_timeout`, extracted so the decision is unit-testable without a real Vulkan device) it calls `reject_render` before falling through to the ordinary `bind_failed` fallback accounting. New test `material_bind_fence_timeout_is_fatal_other_errors_are_not` pins the pure decision function directly (the review's own "a fake/mock is hard here" note is why this stops short of exercising the real `reject_render`/process-exit path).

`crates/kwe-scene-renderer/src/main.rs:1835-1843`:

```rust
match renderer.bind_material_layer(index, key, &textures, uniforms) {
    Ok(()) => { material_ok[index] = true; compiled += 1; }
    Err(_) => { *fallback_reasons.entry("bind_failed").or_insert(0) += 1; }
}
```

Every other caller of a function that can return `RenderError::FenceTimeout`
from `self.fence`/`self.upload_buffer` — `upload_layer` (main.rs:1884-1885),
particle vertex/texture upload (694-695, 1924-1925), video refresh/texture
(730-731, 2441-2442), text upload (509, 575), `render()` itself (616-620)
— checks `is_fence_timeout(&error)` and calls `reject_render(...)` (a `!`
function, main.rs:1033) to exit the process immediately. `vulkan.rs`'s own
doc comment for the *same pattern* at the base-layer path
(`vulkan.rs:1234-1243`) explains why: "The queue submit may still be
reading the staging buffer and destination image... The caller must exit
immediately through reject_render; leaking the handles to process
teardown is safe, while freeing them here races Vulkan." `Sharing
self.fence and self.upload_buffer with render()` (vulkan.rs:1653-1656)
is documented as safe only because "`render()` waits its fence to
completion before returning" — i.e. every caller of a fence-touching
function is trusted to never let a `FenceTimeout` fall through to a
subsequent `reset_fences`/`queue_submit`/command-buffer re-record on that
same fence/buffer.

`bind_material_layer`'s own texture-upload loop
(`vulkan.rs:1466-1495`, via `upload_image_now`) can return exactly this
error and correctly skips cleanup on that path (`vulkan.rs:1483-1485`,
matching the documented leak-and-exit contract) — but `main.rs` does not
exit; it logs `bind_failed` and lets the loop, and then the render loop,
continue. The very next `render()` call does `reset_fences(&[self.fence])`
(vulkan.rs:1985) and reuses `self.upload_buffer`/`self.fence` while the
earlier submission may still be executing on the GPU — a Vulkan
validation-layer violation (resetting/reusing a fence and command buffer
associated with a not-yet-complete queue submission) and a real
correctness hazard, not just a leak. It also leaks whatever textures
`bind_material_layer` had already uploaded for that call (never freed,
since the fence-timeout branch intentionally skips their destruction).

**Minimal fix**: in `compile_material_layers`, match on the `bind_material_layer`
error the same way every other call site does:

```rust
Err(error) => {
    if is_fence_timeout(&error) {
        reject_render(&error, "fence timeout during material texture upload");
    }
    *fallback_reasons.entry("bind_failed").or_insert(0) += 1;
}
```

Add a regression test (unit-level: a fake/mock is hard here given real
Vulkan handles — at minimum, add a code comment/test asserting
`compile_material_layers`'s match arms are exhaustive over
`is_fence_timeout`, or extend the device-gated
`material_pipeline_draws_a_synthetic_solid_color` family with a
fence-timeout-path note so a future refactor cannot silently drop the
check again).

---

## RECOMMENDED

### 3. No bound on shaderc compile time

**Resolution: fixed.** `compile_stage` and `references_live_render_target` now run the actual `shaderc` call on a helper thread and bound how long the caller waits for it via `with_timeout`/`MATERIAL_COMPILE_TIMEOUT` (5 s) -- a hard, sound bound on the calling code path regardless of what glslang/SPIRV-Tools do internally, picked over `OptimizationLevel::Zero` alone because that only reduces expected compile time, not a guarantee (both are applied: `OptimizationLevel::Zero` is also set, documented as the secondary, non-load-bearing mitigation). The spawned thread is not joined on timeout (`shaderc` has no cancellation API) and is left to finish or be reaped at process exit -- documented as a bounded, at-most-one-thread-per-timed-out-compile leak. Existing `compile_round_trip_produces_spirv` and `dead_branch_render_target_reference_does_not_survive_live_preprocess` tests both still pass unchanged against the threaded implementation.

`materialshader::compile_stage` and `references_live_render_target`
(materialshader.rs:148-170, 189-208) call `shaderc::Compiler::compile_into_spirv`/
`preprocess` with no timeout, running `OptimizationLevel::Performance`
(materialshader.rs:161) — the full glslang/SPIRV-Tools optimizer pipeline,
which is more exposed to pathological compile times on adversarial input
than `Zero`/`Size`. The shader text is fully attacker-controlled (a
Workshop `.vert`/`.frag`), so a crafted construct that makes the optimizer
slow blocks the worker's scene-load path with no internal bound; the only
backstop is the daemon's external `--renderer-scene-startup-timeout-ms`/
frame-timeout supervisor killing and restarting the worker, which is not
documented here as the intended mitigation and would count as a
`ProcessExit`/timeout strike per material shader compiled, not a clean
`Refused`. Consider `OptimizationLevel::Zero` (this pipeline emits tiny,
already-simple corpus shaders — optimization buys little) and/or noting
explicitly that the daemon's startup timeout is the intended backstop.

### 4. Per-frame material UBO writes never flush non-coherent host-visible memory, unlike the sibling `refresh_layer` path

**Resolution: fixed.** Both UBO write sites now call `flush_mapped_memory_ranges` (`WHOLE_SIZE` at offset 0, mirroring `refresh_layer`'s identical pattern and rationale) immediately after `copy_nonoverlapping`: once in `bind_material_layer` (the initial write) and once in `render`'s per-draw material-update branch (every subsequent write -- a non-coherent memory type gives no implicit visibility guarantee across writes, so both sites needed their own flush, not just the first). Verified against the real end-to-end material path on both NVIDIA RTX 3070 and llvmpipe (`vulkan::tests::material_pipeline_draws_a_synthetic_solid_color`, `scripts/smoke-scene.sh`'s S2 case).

`allocate_host_visible` (vulkan.rs:2603-2622) prefers `HOST_COHERENT` but
falls back to plain `HOST_VISIBLE` if unavailable. `refresh_layer`
(vulkan.rs:1657-1708) explicitly calls `flush_mapped_memory_ranges` after
writing to its persistently-mapped staging buffer, with a comment
explaining exactly why ("the only thing that makes the write visible to
the device on a host-visible-but-not-coherent memory type"). The
material UBO uses the same `allocate_host_visible` fallback but is
written with no flush at bind time (vulkan.rs:1556-1562) and every frame
(vulkan.rs:2049-2050, `binding.uniforms.mvp`/`time_alpha_brightness`
updated then `copy_nonoverlapping`'d straight into `ubo_mapped`). On a
device where the host-visible memory type lacks `HOST_COHERENT`
(uncommon on desktop Mesa RADV/ANV but a real Vulkan portability case,
not never), `g_ModelViewProjectionMatrix`/`g_Time`/`g_UserAlpha`/
`g_Brightness` updates may not become visible to the GPU — a silent,
non-crashing mis-render (frozen material position/time), exactly the
"wrong defaults that visibly mis-render" class the review asked about.
Add a `flush_mapped_memory_ranges` call after the UBO write in both
`bind_material_layer` and `render()`'s per-draw update, mirroring
`refresh_layer`.

### 5. `#include` resolution has no cap on the NUMBER of includes, only depth (8) and total bytes (1 MiB)

**Resolution: fixed.** `resolve_includes` now threads an `include_count: &mut usize` alongside `total_len`, incremented for every `#include` directive encountered (found or not, at any nesting level) and checked against the new `MAX_INCLUDE_COUNT` (64, mirroring `MAX_INCLUDE_DEPTH`'s spirit) before the stat-heavy `confined_read` round trip runs. New test `include_count_bounded_independent_of_depth_or_size` builds 65 sibling includes, each individually well under the byte/depth caps, and asserts only the count cap trips.

`shaderpre::resolve_includes` (shaderpre.rs:266-314) bounds recursion
depth and total preprocessed size, but a shader with many *sibling*
`#include` lines (not nested) is only stopped once the 1 MiB total is hit
— each one (found or not) routes through
`resolve_shader_reference`/`kwe_core::confined_read`
(main.rs:1493-1514), which does two `canonicalize()` calls plus a
`symlink_metadata`/`metadata` stat per attempt. Worst case is on the
order of ~1 MiB / (shortest possible include line + its "not found"
comment) ≈ low thousands of stat-heavy lookups per stage, times up to 64
pipelines × 2 stages per scene. This runs in the already-sandboxed
worker process (not the daemon — S1 review finding #3's DoS class does
not apply here), so severity is low, but an explicit include-count cap
(mirroring `MAX_INCLUDE_DEPTH`'s spirit) would make the worst case
bounded independent of the byte budget's granularity.

---

## NIT

### 6. `g_Time` assumes a fixed 60 fps, not just "under sustained pressure"

**Resolution: fixed.** `material_frame_counter`'s doc comment now states plainly that the 60 fps assumption drifts systematically and permanently under any steady-state rate other than 60 (a configured fps cap, F2's limiter, a slower device), not only under transient frame-time pressure.

`vulkan.rs:353-360`'s doc comment frames `material_frame_counter as f32 /
60.0` as "exact for the steady-state case and only drifts under sustained
frame-time pressure," but this also drifts systematically and
permanently whenever the renderer's actual target/achieved frame rate is
not 60 (e.g. a user- or scene-configured fps cap, or F2's fps limiter
mentioned in project memory) — not just a transient-pressure case. Already
documented as an accepted known simplification (`AI-Skills/BETA_PLAN.md`);
worth tightening the comment's framing so a future reader does not assume
this only matters under load.

### 7. `MaterialUniforms::default()`'s `mvp`/`effect_texture_projection` identity default is dead in practice

**Resolution: fixed.** Added a one-line-plus comment on the `effect_texture_projection` field noting it is presently a fixed identity with no writer after `Default` (effect passes are S3 scope), the same "unmodeled input defaults to inert" rationale `parallax_pointer` already carries, so a future S3 reader does not have to rediscover that by grepping for writers.

`materialshader.rs:54-67`'s `Default` sets `mvp`/`effect_texture_projection`
to identity, but `bind_material_layer` immediately overwrites `mvp`
(vulkan.rs:1555) before upload and `render()` overwrites it again every
draw (vulkan.rs:2042-2043) — `effect_texture_projection` is never written
anywhere after `Default`, so it stays identity forever with no scene
input threading to it yet (consistent with `parallax_pointer`'s
documented "unmodeled input defaults to inert" choice, just not called
out the same way). Not a bug — worth a one-line comment noting
`effect_texture_projection` is presently a fixed identity, same rationale
as `parallax_pointer`, so a future S3 reader does not have to rediscover
that by grepping for writers.
