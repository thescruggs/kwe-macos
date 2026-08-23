# S5/S6 adversarial review (stacked multi-effect compositing / same-frame
`_rt_FullFrameBuffer` / comment-aware preprocessor / scene-centre recenter /
assets discovery / premultiply wrapper)

Worktree `kwe-b4`, branch `beta-b4-apply-quarantine`, range `88bc2e6..168f508`
(S5: `694b690`, `885b579`, `b90e8fa`; S6: `8a35bec`, `bb67ad8`, `2d7b616`,
`cf4a42c`; pkgrel `a930620`, `168f508`). Read-only review, no edits made.
Merged without review. Prior bars: `docs/research/S1_REVIEW.md` … `S4A_REVIEW.md`.

Counts: **3 MUST-FIX, 2 RECOMMENDED, 1 NIT**.

---

## MUST-FIX

### 1. `move_point_outside_conditionals`'s `#if`/`#endif` scan is O(n²) on any shader with many `#endif`-shaped tokens and few/no real `#if`s — a single crafted shader file well under the existing 1 MiB cap hangs the renderer worker for a long time

`crates/kwe-scene-renderer/src/shaderpre.rs:556-587`:

```rust
fn move_point_outside_conditionals(body: &str, mut point: usize) -> usize {
    let mut stack: Vec<usize> = Vec::new();
    let mut search_from = 0usize;
    while search_from < body.len() {
        let next_if = body[search_from..].find("#if").map(|rel| search_from + rel);
        let next_endif = body[search_from..]
            .find("#endif")
            .map(|rel| search_from + rel);
        ...
        if is_endif {
            ...
            search_from = pos + "#endif".len();
        } else {
            stack.push(pos);
            search_from = pos + "#if".len();
        }
    }
    point
}
```

Every loop iteration recomputes **both** `next_if` and `next_endif` by
scanning `body[search_from..]` from scratch, unconditionally, regardless
of which one actually advanced last iteration. `search_from` only grows
by `len("#if")` (3) or `len("#endif")` (6) per iteration. If the file
contains one (or zero) `#if` and many `#endif`-shaped tokens after it,
every iteration's `.find("#if")` call has no match anywhere in the
remaining ~1 MiB buffer and must scan all the way to the end before
returning `None` — that scan reruns on nearly the same ~1 MiB slice on
**every** one of the ~150,000 iterations a 1 MiB file's worth of 7-byte
`"#endif\n"` lines produces. This is classic O(n²): roughly `n²/7` byte
comparisons for an n-byte file, i.e. tens of billions of byte
comparisons for a file at the existing `MAX_PREPROCESSED_BYTES` (1 MiB)
cap — multi-second-to-minutes of single-core CPU burn processing ONE
shader compile, on the very worker process that must stay responsive for
per-frame compositing.

This is trivially reachable: `move_point_outside_conditionals` runs
inside `find_include_insertion_point` (`shaderpre.rs:618`), called from
`resolve_includes` (`shaderpre.rs:672`) whenever a shader has at least
one `#include` that resolves to non-empty content, called from
`preprocess` (`shaderpre.rs:1678`), called from `compile_one_material`
(`main.rs:2398`) for **every** layer's base material AND every effect
pass — i.e. directly on attacker-controlled shader source a Workshop
package can bundle inside its own `scene.pkg` (already established as
the trust boundary by S1-S4: "a hostile or malformed Workshop package
can name any shader file its own `material.json` `shader` field points
to"). No exotic nesting is needed — the attacker doesn't even need a
real `#if`; a shader with a trivial `void main(){}`, one resolvable
`#include`, and ~140,000 lines of `#endif\n` padding (comfortably under
1 MiB) is sufficient, and never needs to reach `shaderc` (which would
reject unbalanced `#endif`s) — the hang happens entirely in this Rust
scan, before compilation is ever attempted.

Contrast with the sibling function added in the *same* commit,
`find_main_token` (`shaderpre.rs:479-553`): its own doc comment
explicitly reasons about boundedness ("a single linear pass... cannot
loop unboundedly even over a hostile/malformed source") and its `i`
index strictly increases with no backtracking. `move_point_outside_
conditionals` does not carry the same property despite handling the
exact same class of input.

**Minimal fix**: track the next `#if` and next `#endif` positions
incrementally instead of researching from `search_from` every
iteration — e.g. only recompute `next_if` after actually consuming an
`#if` (and similarly for `next_endif`), or scan the body once
up-front collecting all `#if`/`#endif` byte offsets into a `Vec` in a
single linear pass, then walk that `Vec` for the stack logic. Either
makes the whole function O(n) again, matching `find_main_token`'s own
established bound in this same commit.

### 2. The S6 premultiply wrapper (`#define main kwe_material_main` + `out_FragColor.rgb *= out_FragColor.a`) is applied to **every** fragment shader `preprocess` compiles — including intermediate/targeted effect passes whose FBO output is later *sampled* by another pass — silently double- (or N-fold-) premultiplying alpha through any effect chain with more than one pass and non-1 alpha, undoing the very fix this commit shipped

`crates/kwe-scene-renderer/src/shaderpre.rs:1776` and `:1791`:

```rust
if stage == Stage::Fragment {
    source_out.push_str("#define main kwe_material_main\n");
}
...
if stage == Stage::Fragment {
    source_out.push_str(
        "\n#undef main\nvoid main() {\n    kwe_material_main();\n    out_FragColor.rgb *= out_FragColor.a;\n}\n",
    );
}
```

This wrapper is unconditional on `stage == Stage::Fragment` — there is
no parameter distinguishing "this is the layer's own base/final
material" from "this is an intermediate effect pass whose target FBO
will be *sampled as a texture* by a later pass." `shaderpre::preprocess`
has exactly one caller pair (vertex+fragment) in the whole crate,
`compile_one_material` (`main.rs:2398-2460`), and `compile_one_material`
is itself the single choke point used for **both** the layer's base/
final material (`main.rs:2787`) **and** every S5 intermediate effect
pass — targeted and the new S5 ping-pong untargeted-non-last passes
alike (`main.rs:2925`, the `plan.intermediate` loop). `compile_effect_pass`
(`vulkan.rs:2703-2790`) then builds that pass's pipeline with the
*same* `blend_attachment_for(blend_mode)` premultiplied-alpha blend
state (`vulkan.rs:2775`) the material pipeline uses, entry point named
`"main"` (the wrapped one). So:

- Pass 1 of a chain writes to its FBO through this premultiplied blend
  state, using the wrapped shader — its FBO ends up holding **correctly**
  premultiplied color (`rgb = trueRGB · trueAlpha`, `a = trueAlpha`),
  matching what its own blend write needed.
- Pass 2 samples that FBO as an ordinary texture (`texSample2D`, a bare
  alias for `texture()` — no un-premultiply step exists anywhere in this
  file or `materialshader.rs`, confirmed by grep). Pass 2's own fragment
  shader is *also* wrapped (same unconditional check), so its own output
  gets `rgb *= a` applied **again** — but its input (`trueRGB · trueAlpha`
  sampled from pass 1's FBO) was already premultiplied, so if pass 2's
  shader passes that color through (or blends/distorts it, common for
  ripple/blur/glow effects), the second wrapper multiplies by alpha a
  *second* time: `rgb = trueRGB · trueAlpha²`. A chain with N passes
  sampling forward compounds this to `trueAlpha^N`.

This is exactly the "double premultiply" scenario the review brief asked
about, and it directly undoes the fix `cf4a42c` shipped: that commit's
own root cause was a shader whose transparent regions are filled with a
non-black color, fixed by premultiplying *once* before the (already
premultiplied-alpha) blend state sees it. A multi-pass effect chain
(exactly what S5, in the same slice, added real ping-pong rendering
for — e.g. Workshop `1131061888`'s four-effect "trigun") with any
partial alpha anywhere in an intermediate pass (blur/glow edges, soft
masks, waterripple-style translucency — the same effects family this
fix's own repro scene, `1725674512`, uses) will silently darken/discolor
through every hop, worse the more passes it has. This is undisclosed:
neither `docs/SCENE_FORMAT_V1.md`'s "Stacked multi-effect compositing...
(S5)" section nor its blend/premultiplication paragraphs mention this
interaction, and it postdates that doc entirely (`cf4a42c` touched only
`shaderpre.rs`/`vulkan.rs`/`THIRD_PARTY.yml`, never
`SCENE_FORMAT_V1.md`).

Untested: the new device test, `material_fragment_shader_with_
transparent_nonblack_pixels_does_not_paint_over_the_clear_color`
(`vulkan.rs:5465-...`), only exercises a single `bind_material_layer`
draw with no effect chain and no FBO-to-FBO sampling — it cannot catch
this, and no other new S5/S6 test exercises a two-pass effect chain with
non-1 intermediate alpha.

**Minimal fix**: thread a flag (or a separate code path) through
`preprocess`/`compile_one_material` distinguishing "this fragment
shader's output goes straight to the compositor via the premultiplied-
alpha blend state" (needs the wrapper) from "this fragment shader's
output is written into an FBO that a LATER pass will sample as a plain
texture" (must NOT get the wrapper — or, if the FBO write itself must
stay premultiplied-for-storage for some other reason, the *consuming*
pass must un-premultiply the sample before using it, which is a bigger
change). The simplest correct fix given the existing plumbing: only wrap
`final_material` (`main.rs:2787`'s call, the layer's own bound material,
which really is the one whose output goes straight to the compositor's
blend state) — never wrap `plan.intermediate` passes' fragment shaders
(`main.rs:2925`'s call), since those write into an FBO another pass will
sample, not into the compositor.

### 3. `move_point_outside_conditionals`'s `#if`/`#endif` tracking is not comment-aware — unlike its sibling `find_main_token`, hardened for exactly this in the *same* commit — so a comment mentioning `#if`/`#endif`-shaped text inside a real conditional can desync the nesting stack and leave the splice point inside a live `#if` block, silently dropping included declarations behind a combo gate

`crates/kwe-scene-renderer/src/shaderpre.rs:556-587` (same function as
finding #1) scans raw byte offsets of the literal substrings `"#if"`/
`"#endif"` with no comment-tracking state, in sharp contrast to
`find_main_token` (`shaderpre.rs:479-553`) — added in this exact commit,
explicitly to fix "a `main` mention inside a `//` line comment or a `/*
*/` block comment... must never be mistaken for the real definition."
The `#if`/`#endif` walk needed the identical treatment and did not get
it.

Concrete failure: a shader with a documentation comment mentioning `#if`
*inside* a real `#if ... #endif` region —

```glsl
#if REAL
// #if inside comment
uniform vec3 g_Inside;
#endif
void main(){}
```

— pushes `REAL`'s position (A), then the comment's fake `"#if"` position
(B, B > A) onto the LIFO stack. The real `#endif` pops the stack's TOP
(LIFO), which is B (the fake, comment-internal one), not A (the real
`#if REAL`). The point-adjustment check (`point > start && point <=
pos`) then evaluates against B, not A — even when it fires, the "moved"
point sits at the comment's own line, which is still *inside* the real
`#if REAL ... #endif` region, not actually outside it. Worse, the real
`#if REAL` (A) is left permanently unpopped on the stack for the rest of
the file: if that's the file's only `#endif`, `A`'s region is never
checked again by any later logic, so a splice point that legitimately
lands inside `#if REAL...#endif` later in the walk is never corrected.

This is exactly the bug class `885b579`'s own commit message says it was
written to close ("splicing common_blur.h's blur13a/blur7a/blur3a inside
that block, leaving them undefined whenever MASK is off") — reintroduced
by a helper this same commit added but did not comment-harden, on any
real WE shader with a `//`-commented `#if`/`#endif` reference near a live
conditional (a plausible, not even adversarial, authoring pattern —
WE shaders already carry `// [COMBO]` and `// {json}` metadata comments
per this codebase's own `scrape` function).

**Minimal fix**: give `move_point_outside_conditionals` the same
comment-tracking state machine `find_main_token` already has (or better,
factor the comment-skipping logic out of `find_main_token` into a shared
helper both functions scan through), so a `#if`/`#endif` occurrence
inside a `//` or `/* */` comment is never pushed/popped onto the
conditional-nesting stack.

---

## RECOMMENDED

### 1. `find_include_insertion_point`'s `rfind("attribute"/"varying"/"uniform")` is neither comment-aware nor token-boundary-aware

`crates/kwe-scene-renderer/src/shaderpre.rs:621-624` searches for the
LAST literal occurrence of `"attribute"`, `"varying"`, `"uniform"`
anywhere in `body[..main_pos]`, including inside comments and as a
substring of a longer word (e.g. "nonuniform", "varying" inside prose).
The final `.min(main_line_start)` clamp (`:630`) prevents the result
from landing *past* `main`, so most such false matches are merely
"technically wrong reason, same practical outcome" — but combined with
MUST-FIX #3's stack-desync risk, a comment containing one of these
keywords positioned inside a live `#if` region can feed a bad `point`
into `move_point_outside_conditionals` that then fails to walk back out
correctly. Give this scan the same comment-tracking (and ideally
word-boundary) treatment as `find_main_token` for consistency and
defense in depth, now that the function it feeds is combo/`#if`-aware
rather than a blind "before `main`" splice.

### 2. `snapshot_full_frame_buffer_inline`'s same-frame semantics are scoped to a layer's own bound material only, but nothing prevents an effect chain's *own* pass from also being registered in `ffb_consumer_layers`-adjacent bookkeeping if a future change widens that set without re-checking the "effect-chain references stay stale" boundary

Not a bug today — `ffb_consumer_layers` (`main.rs:2638`) is only ever
pushed from the layer's own bound material check (`main.rs:2818-2823`),
and effect-pass texture slots that reference `_rt_FullFrameBuffer`
correctly go through the ordinary (stale, pre-S5) binding path with no
same-frame snapshot registration. This is a maintainability note, not a
correctness finding: the scope boundary between "layer's own material:
same-frame" and "effect pass: one-frame-stale" is enforced by *omission*
(nothing routes effect-pass FFB references into `ffb_consumer_layers`)
rather than by an explicit assertion/test that would fail loudly if a
future S7-style change accidentally widened it. Consider a regression
test asserting an effect pass sampling `_rt_FullFrameBuffer` is never
added to `ffb_consumer_layers`.

---

## NIT

### 1. `MAX_FULL_FRAME_BUFFER_SNAPSHOTS_PER_FRAME` is checked in two places (`vulkan.rs:310` defensively, `main.rs:3029` authoritatively via `truncate`) with no shared constant-derived test tying them together beyond `full_frame_buffer_snapshot_cap_matches_the_documented_bound`'s bare `assert_eq!(..., 8)`

Both checks are correct and independently bounded (confirmed by reading
both call sites), so this is not a functional problem — just a coupling
worth a one-line test asserting the two enforcement points agree in
behavior (e.g. a 9-consumer scene produces at most 8 same-frame
snapshots end-to-end), not merely that the constant equals a literal.

---

## Areas reviewed and found sound

- **Ping-pong allocation** (`main.rs::plan_effect_chain`): `write_slot`
  alternates strictly between two names (`_a`/`_b`), independent of how
  many intermediate passes exist — genuinely bounded to ≤ 2 targets per
  object regardless of chain length; per-scene/per-frame totals still
  gated by the pre-existing `MAX_EFFECT_TARGETS_PER_SCENE` (64),
  `MAX_EFFECT_PASS_BINDINGS` (256), `MAX_EFFECT_FRAME_ACTIONS` (512)
  caps, all of which return bounded errors (never panic) and are routed
  through the existing `fence timeout -> reject_render` / fallback-reason
  machinery at every new S5 call site.
- **`_rt_FullFrameBuffer` render-pass split** (`vulkan.rs::render`):
  `render_pass_resume` is render-pass-compatible with the main
  `render_pass` (same single color attachment, no depth buffer either
  side), so reusing `self.framebuffer` across the split is spec-legal;
  the `TRANSFER_SRC_OPTIMAL` hand-off between segments is proven correct
  by construction (both passes declare matching `final_layout`/
  `initial_layout`); the copy's own barriers on `_rt_FullFrameBuffer`
  match the pre-existing, already-reviewed `copy_effect_target`/
  `snapshot_full_frame_buffer` pattern exactly. The ≤ 8 snapshot cap is
  enforced on both sides (`vulkan.rs::render`'s own re-check,
  `main.rs`'s `truncate`). No new fence-touching call site was added —
  the frame's single existing fence wait is unaffected.
- **S6 recenter math**: `scene_center` is deliberately the *declared*
  scene resolution's center (`config.resolution / 2`), not the
  scaling-mode-adjusted `world_extent` — matches upstream
  (`CImage.cpp:259-262`'s `scene_width/2`) and is pinned by a dedicated
  regression test (`build_orthographic_mvp_centers_a_full_screen_layer_
  after_recentering`) constructed specifically around a scene where
  `world_extent() != declared resolution` (a "fill"-cropped case).
  Particles and text layers are recentred consistently: both share
  `LayerState.origin`/the same scene.json `origin` parsing as image
  layers, and the S1 push-constant path (used by particles, text, and
  non-material image draws alike) applies the identical `recenter()`
  call the S2 material path does, at the single unconditional call site
  in `render`'s draw loop — verified `particle.vert` uses the exact same
  `world = mat2(m)·pos + t` push-constant convention as `quad.vert`, and
  particle spawn positions are baked in "scene pixels" (the same
  absolute-origin convention), so this is a bonus fix for particles, not
  a new inconsistency.
- **Assets discovery** (`kwe-core/src/scan.rs::default_wallpaper_engine_
  assets_dir`): now routes through the pre-existing, already-bounded
  `discover_libraries` (8 MiB manifest read cap, bounded VDF parse) the
  same way `scan_installed` already did — no new unbounded scan
  introduced. An explicit `--assets-dir`/`--wallpaper-engine-assets`
  still wins unconditionally at both call sites that matter
  (`kwe-cli/src/main.rs:222-231`, `kwe-daemon/src/main.rs:289-298`) —
  the new default is only consulted on `None`.
- **`constantshadervalues` metadata-key matching fix** (`bb67ad8`):
  correctly falls back to the bare GLSL name when no `"material"` key
  exists (preserving every prior synthetic fixture's convention), and
  the new tests exercise both the mismatch and the fallback shapes.
- **Smoke/fixture repairs** (`2d7b616`): the `_recenter()` shift matches
  the shipped code's convention exactly (half the declared 160x90
  fixture resolution); the two "genuinely pre-existing S5 bugs" it
  fixed along the way (missing `s5bbelow.vert`, the unconditional
  `_rt_FullFrameBuffer` creation) are both justified by the documented
  "bare copybackground" corpus pattern this renderer already commits to
  supporting elsewhere (`main.rs`'s `ffb_only_passthrough` carve-out),
  not a silent regression cover-up.
- **Provenance**: `THIRD_PARTY.yml` gained a `Borrowed-From` entry for
  the `CImage.cpp:259-262` recenter citation; the assets-discovery and
  material-constant-key fixes are correctly noted as original (no
  upstream citation invented). No Workshop payloads or binary fixtures
  were committed anywhere in the diff (`git diff --stat` shows only
  source/docs/script/packaging text files).
