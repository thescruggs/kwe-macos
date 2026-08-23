// SPDX-License-Identifier: GPL-3.0-or-later
//! S2 material-shader preprocessor: turns one Wallpaper Engine shader
//! source (`assets/shaders/<name>.vert` / `.frag`) into GLSL that
//! `shaderc` (materialshader.rs) can compile straight to SPIR-V for
//! Vulkan 1.2.
//!
//! Two things happen here, kept in one pass over the source text:
//!
//! 1. The upstream-faithful part — HLSL-compat header shim, `#include`
//!    resolution, `#require LightingV1` stub, `// [COMBO] {json}` combo
//!    scraping, `uniform TYPE name; // {json}` parameter-metadata
//!    scraping, `gl_FragColor` -> `out_FragColor`, combos as
//!    `#define NAME value` — is a direct port of
//!    `ShaderUnit::preprocess`/`compile` (see the `Borrowed-From` note on
//!    each function below).
//! 2. A Vulkan-target addition with no upstream equivalent — upstream
//!    compiles this same preprocessed text through glslang -> SPIR-V ->
//!    SPIRV-Cross back to *OpenGL 330*, where loose `uniform`s and
//!    unnumbered `varying`s are legal. Compiling straight to Vulkan
//!    SPIR-V is not: every non-opaque uniform must live in a block, every
//!    sampler needs a `layout(binding=)`, every stage-interface variable
//!    needs a `layout(location=)`. `fold_declarations` performs exactly
//!    that rewrite, in place, for the declarations this slice's material
//!    pipeline understands (the WE "standard" uniform set plus a
//!    material's own `constantshadervalues`); anything it does not
//!    recognize gets a zero-valued local so the shader still compiles
//!    (`Unknown uniforms get zero defaults` in the task brief) rather
//!    than failing preprocessing outright — a real compile failure from
//!    shaderc is a value the caller already has to handle (unsupported
//!    GLSL constructs, missing includes that were load-bearing, etc.), so
//!    this module leans on that instead of trying to detect every way a
//!    shader can fail to compile.
//!
//! Pure: every function takes text/closures in and returns values out — no
//! filesystem or Vulkan access happens in this module.

use std::collections::BTreeMap;

use serde_json::Value;

/// `ShaderUnit::preprocessIncludes` has no explicit recursion bound (it
/// relies on the corpus never nesting includes deeply); this module
/// enforces one so a crafted or cyclic `#include` chain cannot recurse
/// unboundedly.
pub const MAX_INCLUDE_DEPTH: usize = 8;

/// Total size of one stage's preprocessed source, after every include is
/// inlined and every substitution applied. Generous over any real WE
/// shader (the largest corpus fragment shader plus its includes is under
/// 32 KiB) while still bounding a crafted `#include` cycle or a chain of
/// large headers.
pub const MAX_PREPROCESSED_BYTES: usize = 1024 * 1024;

/// S2 review #5: `MAX_INCLUDE_DEPTH`/`MAX_PREPROCESSED_BYTES` bound
/// nesting and total bytes, but not the NUMBER of sibling `#include`
/// lines a single unit can carry — each one (found or not) round-trips
/// through the caller's `confined_read` (two `canonicalize()` calls plus
/// a stat), so a shader with many short, unresolvable includes could
/// otherwise force on the order of `MAX_PREPROCESSED_BYTES` / (shortest
/// include line) stat-heavy lookups before the byte budget ever kicks
/// in. Mirrors `MAX_INCLUDE_DEPTH`'s spirit: a low, generous-over-any-real-
/// shader cap on the total `#include` directives resolved per top-level
/// `preprocess` call (across every nesting level).
pub const MAX_INCLUDE_COUNT: usize = 64;

/// `g_Texture0..g_Texture7` — the descriptor set's sampled-image bindings
/// (vulkan.rs). A material referencing `g_Texture8` or higher cannot be
/// bound and preprocessing fails for that stage.
pub const MAX_MATERIAL_TEXTURES: usize = 8;

/// Slots in the `MaterialUniforms` UBO's `g_MaterialConstants` array
/// (materialshader.rs) available to a material's `constantshadervalues`
/// plus any other uniform name this module does not recognize as a WE
/// standard. A material with more distinct such uniforms than this falls
/// back past the cap (uses the zero-default path for the overflow, not a
/// hard preprocessing failure — see `fold_declarations`).
pub const MAX_MATERIAL_CONSTANTS: usize = 16;

/// The upstream HLSL-compat shim, verbatim (the `mul`/`lerp`/`frac`/...
/// macro block), plus two `#extension` pragmas with no upstream
/// equivalent: `#version 330` alone rejects `layout(binding=...)`
/// (`'binding' : not supported for this version or the enabled
/// extensions`) and `GL_ARB_separate_shader_objects` is what makes a
/// stage-local `layout(location=)` legal without a matching
/// `#version 420`+. Verified empirically against `glslc
/// --target-env=vulkan1.2` during development — dropping either
/// extension reproduces the exact compile error named above.
///
/// Borrowed-From: Almamu/linux-wallpaperengine (GPL-3.0-or-later)
/// src/WallpaperEngine/Render/Shaders/ShaderUnit.cpp:22-58 (the
/// `SHADER_HEADER`/`FRAGMENT_SHADER_DEFINES`/`VERTEX_SHADER_DEFINES`
/// macros) @ b016d7d1 — adapted (Rust string constants instead of C++
/// preprocessor macros; the two `#extension` lines above are this
/// module's own addition, not from upstream, which never needs them
/// since it targets OpenGL 330 by way of a SPIRV-Cross round trip rather
/// than compiling straight to Vulkan SPIR-V).
const SHADER_HEADER: &str = "#version 330\n\
#extension GL_ARB_shading_language_420pack : enable\n\
#extension GL_ARB_separate_shader_objects : enable\n\
precision highp float;\n\
#define mul(x, y) ((y) * (x))\n\
#define lerp mix\n\
#define frac fract\n\
#define CAST2(x) (vec2(x))\n\
#define CAST3(x) (vec3(x))\n\
#define CAST4(x) (vec4(x))\n\
#define CAST3X3(x) (mat3(x))\n\
#define float2 vec2\n\
#define float3 vec3\n\
#define float4 vec4\n\
#define int2 ivec2\n\
#define int3 ivec3\n\
#define int4 ivec4\n\
#define saturate(x) (clamp(x, 0.0, 1.0))\n\
#define texSample2D texture\n\
#define texSample2DLod textureLod\n\
#define log10(x) (log2(x) * 0.301029995663981)\n\
#define atan2 atan\n\
#define fmod(x, y) ((x)-(y)*trunc((x)/(y)))\n\
#define ddx dFdx\n\
#define ddy(x) dFdy(-(x))\n\
#define max(x, y) max(y, x)\n\
#define GLSL 1\n\n";

/// S4b fix: `#define max(x, y) max(y, x)` was previously dropped from the
/// verbatim header on the theory that GLSL already defines `max` the same
/// way HLSL does, so the swap was assumed to be a no-op kept only for
/// upstream fidelity. That theory was wrong for exactly the shape the WE
/// shader corpus actually writes: HLSL's `max(scalar, vecN)` implicitly
/// broadcasts the scalar regardless of argument position, but GLSL's
/// overload set only has `genType max(genType x, float y)` — the scalar
/// MUST be the second argument, and even then only as a `float` (an
/// integer literal like the bare `0` in `max(0, albedo.rgb)` needs GLSL's
/// int -> float implicit conversion on the SECOND-position argument to
/// resolve at all). A WE shader written as `max(0, someVec3)` (the real
/// corpus shape — `workshop/2423477561/effects/nitro.frag` and
/// `workshop/2988515046/effects/nitro.frag`, both pkg-bundled) has no
/// matching GLSL overload in EITHER position — `shaderc` reports `'max' :
/// no matching overloaded function found` for the un-swapped call. This
/// macro is what upstream itself ships (`ShaderUnit.cpp`'s own
/// `SHADER_HEADER`) to paper over exactly this: swapping unconditionally
/// is safe because a well-typed `max(a, b)` call is mathematically
/// commutative — a currently-compiling call written the OTHER way around
/// (vector first, scalar second) is unaffected in VALUE, and only breaks
/// if the corpus also authored `max()` in that order somewhere, which the
/// 60-scene local corpus byte-identity sweep (before/after this fix)
/// found no case of.
///
/// (The C preprocessor's blue-paint rule — a macro name is never
/// re-expanded inside its own substitution while that expansion is in
/// progress — is what makes `#define max(x,y) max(y,x)` terminate rather
/// than recurse infinitely; this is a standard, safe idiom, confirmed
/// against `shaderc`'s real preprocessor, not just a textual assumption.)
// `out_FragColor` needs an explicit `layout(location=0)` for the same
// Vulkan reason every other stage-interface variable does (see
// `SHADER_HEADER`'s doc comment) — there is exactly one color attachment
// (vulkan.rs's single-attachment render pass), so location 0 is always
// correct.
const FRAGMENT_SHADER_DEFINES: &str =
    "layout(location = 0) out vec4 out_FragColor;\n#define varying in\n";
const VERTEX_SHADER_DEFINES: &str = "#define attribute in\n#define varying out\n";

/// The `#require LightingV1` stub — lighting objects are not implemented,
/// so the generated function always returns zero contribution.
///
/// Borrowed-From: Almamu/linux-wallpaperengine (GPL-3.0-or-later)
/// src/WallpaperEngine/Render/Shaders/ShaderUnit.cpp:366-377
/// (`ShaderUnit::generateLightingV1`) @ b016d7d1 — adapted.
const LIGHTING_V1_STUB: &str = "// begin of generated module LightingV1\n\
vec3 PerformLighting_V1(vec3 worldPos, vec3 albedo, vec3 normal, vec3 viewDir,\n\
    vec3 specularTint, vec3 baseReflectance, float roughness, float metallic)\n\
{\n\
    return vec3(0.0);\n\
}\n\
// end of generated module LightingV1\n";

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Stage {
    Vertex,
    Fragment,
}

/// Given an include filename (already extension-normalized, e.g.
/// `"common_pbr.h"` or, for a workshop-shader redirect,
/// `"zcompat/scene/shaders/<id>/foo.h"`), return its bytes, or `None` if
/// it does not exist. Callers implement the `shaders/<name>` root and the
/// `workshop/<id>/<file>` -> `zcompat/scene/shaders/<id>/<file>` redirect
/// (`AssetLocator::shader`) before calling `preprocess` — this module only
/// resolves the bare `#include "file.h"` name it finds in shader text,
/// which the redirect does not apply to (upstream's redirect is keyed on
/// the *top-level* shader path, not each include).
pub type IncludeLookup<'a> = dyn FnMut(&str) -> Option<Vec<u8>> + 'a;

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum PreprocessError {
    /// `#include` chain nested past `MAX_INCLUDE_DEPTH`.
    IncludeDepthExceeded,
    /// More than `MAX_INCLUDE_COUNT` `#include` directives resolved
    /// (across every nesting level) in one `preprocess` call.
    TooManyIncludes,
    /// Preprocessed text exceeded `MAX_PREPROCESSED_BYTES`.
    SizeExceeded { bytes: usize, limit: usize },
    /// A `uniform sampler2D` names a texture index >= `MAX_MATERIAL_TEXTURES`.
    TooManyTextures { name: String, index: u32 },
    /// S2 review #1: a combo name (from either `material_combos` — i.e.
    /// `material.json`'s own `combos` map — or a shader's `// [COMBO]`
    /// scrape) is not a strict GLSL identifier. Both sources are
    /// untrusted metadata that reaches a `#define NAME VALUE` line
    /// verbatim; rejecting the whole material here (rather than
    /// dropping just the bad entry) keeps the failure honest and
    /// visible as one more fallback reason instead of silently
    /// compiling a material with a combo quietly missing.
    InvalidComboName(String),
    /// S4a review MUST-FIX #2: an `#if`/`#elif` condition
    /// `evaluate_if_expr` could not fully evaluate (an unrecognized
    /// expression shape, or the depth/token bound in MUST-FIX #1 was
    /// hit) WHILE its parent scope is live -- rather than guess "always
    /// live" (which can silently SUPPRESS a genuinely-live sibling
    /// `#else`/`#elif` branch and under-scrape its declarations, see the
    /// finding), the whole material is rejected the same way any other
    /// preprocess failure already is. Carries the raw (bounded, char-
    /// count-capped) condition text for a future diagnostic; not
    /// currently logged (`main.rs` discards the `Err` payload today).
    AmbiguousCondition(String),
}

impl std::fmt::Display for PreprocessError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::IncludeDepthExceeded => {
                write!(f, "include depth exceeded {MAX_INCLUDE_DEPTH}")
            }
            Self::TooManyIncludes => {
                write!(f, "more than {MAX_INCLUDE_COUNT} #include directives")
            }
            Self::SizeExceeded { bytes, limit } => {
                write!(f, "preprocessed size {bytes} exceeds the {limit} byte cap")
            }
            Self::TooManyTextures { name, index } => write!(
                f,
                "texture slot {index} (from uniform \"{name}\") exceeds the {MAX_MATERIAL_TEXTURES}-texture cap"
            ),
            Self::InvalidComboName(name) => {
                write!(f, "combo name {name:?} is not a valid GLSL identifier")
            }
            Self::AmbiguousCondition(expr) => {
                write!(f, "could not evaluate #if/#elif condition {expr:?}")
            }
        }
    }
}

/// S2 review #1: strict GLSL-identifier check for a combo name before it
/// can reach a `#define NAME VALUE` line — `^[A-Za-z_][A-Za-z0-9_]{0,63}$`.
/// Both combo sources this module accepts (`material_combos`, built by
/// the caller from `material.json`'s `combos` map, and the `// [COMBO]`
/// scrape below) are untrusted metadata: a JSON string value can encode
/// an embedded newline (`"FOO\nBAR"` decodes to an actual LF byte even
/// though the JSON sat on one physical line), which would otherwise let
/// a crafted name break out of the `#define` line and inject arbitrary
/// additional shader text. The 64-character length cap matches this
/// module's other small-identifier bounds (`MAX_MATERIAL_CONSTANTS`-style
/// sizing) and is generous over any real WE combo name (the longest
/// scraped from the real asset corpus is well under 32 characters).
fn is_valid_combo_name(name: &str) -> bool {
    let mut chars = name.chars();
    let Some(first) = chars.next() else {
        return false;
    };
    if !(first.is_ascii_alphabetic() || first == '_') {
        return false;
    }
    name.len() <= 64 && chars.all(|c| c.is_ascii_alphanumeric() || c == '_')
}

/// One `uniform TYPE name; // {json}` parameter declaration, exactly as
/// upstream's `preprocessVariables` would have handed to
/// `parseParameterConfiguration` — recorded for diagnostics/tests; the
/// material pipeline does not currently act on the JSON metadata beyond
/// what `fold_declarations` needs (type + name).
#[derive(Debug, Clone, PartialEq)]
pub struct UniformMeta {
    pub glsl_type: String,
    pub name: String,
    pub json: Option<Value>,
}

/// One `attribute TYPE name;` declaration, vertex stage only, in source
/// order — the caller (vulkan.rs material-pipeline registration) checks
/// this against the one vertex format the quad path supports (S2 scope:
/// mesh/puppet geometry stays out) and falls back if it does not match.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct AttributeDecl {
    pub glsl_type: String,
    pub name: String,
    /// The `layout(location = N)` this declaration was folded to. S4:
    /// assigned only to attributes `fold_declarations` scraped from a
    /// LIVE `#if`/`#ifdef` branch (see `CondFrame`) — an attribute behind
    /// a combo that defaults off (or that the material does not
    /// override) never consumes a location, matching what `shaderc`'s
    /// real preprocessor actually hands the compiler. Callers (the
    /// material-pipeline vertex-input build in `vulkan.rs`) must use
    /// THIS field, not the attribute's position in the `Vec`, since both
    /// happen to match today but nothing enforces they always will.
    pub location: u32,
}

#[derive(Debug, Clone, Default)]
pub struct PreprocessOutput {
    /// The final GLSL text, ready for `shaderc`.
    pub source: String,
    /// Combos in effect for this stage: discovered defaults overridden by
    /// the material's own combos map, keyed by the ORIGINAL (not
    /// upper-cased) combo name — `#define`s in `source` are upper-cased,
    /// matching upstream.
    pub combos: BTreeMap<String, i64>,
    /// Scraped `uniform TYPE name; // {json}` parameter metadata, in
    /// source order — diagnostic/test surface (asserted by the
    /// `uniform_metadata_scraped_with_json` unit test); the material
    /// pipeline does not currently need the JSON payload beyond what
    /// `fold_declarations` already used for slot assignment.
    #[allow(dead_code)]
    pub uniforms: Vec<UniformMeta>,
    pub attributes: Vec<AttributeDecl>,
    /// `g_Texture<N>` indices this stage's source actually declares —
    /// diagnostic/test surface (`sampler_gets_binding_from_texture_index`);
    /// `bind_material_layer` fills every slot regardless (a `None` texture
    /// samples the shared dummy), so production code does not currently
    /// need to know which indices a given shader actually references.
    #[allow(dead_code)]
    pub sampler_slots: Vec<u32>,
    /// True if any `_rt_`-prefixed name (a runtime FBO render target)
    /// appears anywhere in the preprocessed text, including in scraped
    /// uniform JSON metadata (a `"default":"_rt_FullFrameBuffer"` sampler
    /// default is exactly how upstream shaders reference one).
    /// Conservative by design: some `_rt_` mentions may be behind a combo
    /// this material never enables (see `materialshader::
    /// references_live_render_target` for the precise, live-preprocessed
    /// check). S2 used this as a fast pre-filter before gating
    /// compilation on a render-target reference; S3 gives every such
    /// reference a real resolution path (module doc comment,
    /// `kwe_core::sceneeffect`), so nothing currently gates on this flag
    /// — kept as diagnostic/test surface (`references_render_target`'s
    /// own scrape tests) for the same reason
    /// `references_live_render_target` is kept.
    #[allow(dead_code)]
    pub references_render_target: bool,
    /// Uniform names this module could not map to either a WE-standard
    /// slot or a material-constant slot — each got a local zero-valued
    /// declaration instead. Diagnostic only.
    pub unsupported_uniforms: Vec<String>,
}

/// Resolve every `#include "file"` in `source`, recursively (bounded by
/// `MAX_INCLUDE_DEPTH`), inlining the included text where the directive
/// appeared. A missing include is not an error — matches upstream, which
/// leaves a "tried including... but was not found" comment rather than
/// failing (some includes in the corpus are behind combos that are never
/// enabled, so their filename may not resolve without that being a real
/// problem).
///
/// S4b fix: every `#include`'s resolved text is collected into one shared
/// accumulator (in encounter order, flattened across nesting) instead of
/// being inlined at its exact source position, and [`resolve_includes`]
/// splices that accumulator in just before `main(` — matching upstream's
/// real placement strategy (see that function's doc). This function does
/// the recursive walk/collection; it never returns text with `#include`
/// lines still in it.
///
/// Borrowed-From: Almamu/linux-wallpaperengine (GPL-3.0-or-later)
/// src/WallpaperEngine/Render/Shaders/ShaderUnit.cpp:136-171 (the
/// `#include` extraction loop and its not-found fallback) @ b016d7d1 —
/// adapted (recursive bounded resolution replaces the two-pass,
/// one-level-of-nesting scheme).
fn collect_includes(
    source: &str,
    include: &mut IncludeLookup<'_>,
    depth: usize,
    total_len: &mut usize,
    include_count: &mut usize,
    accumulated: &mut String,
) -> Result<String, PreprocessError> {
    if depth > MAX_INCLUDE_DEPTH {
        return Err(PreprocessError::IncludeDepthExceeded);
    }
    let mut out = String::with_capacity(source.len());
    for line in source.split_inclusive('\n') {
        let trimmed = line.trim_start();
        if let Some(rest) = trimmed.strip_prefix("#include")
            && let Some(name) = extract_quoted(rest)
        {
            *include_count += 1;
            if *include_count > MAX_INCLUDE_COUNT {
                return Err(PreprocessError::TooManyIncludes);
            }
            // `total_len` tracks a DELTA against `accumulated`'s length
            // (not its absolute size) so a shader with several sibling
            // includes is not double- or triple-counted — every push
            // below adds real bytes to the one shared accumulator, and
            // only the growth since this include started is new.
            let accumulated_before = accumulated.len();
            accumulated.push_str("// begin of include from file ");
            accumulated.push_str(&name);
            accumulated.push('\n');
            match include(&name) {
                Some(bytes) => {
                    let text = String::from_utf8_lossy(&bytes);
                    // Nested includes flatten into the SAME accumulator,
                    // in the order encountered — matches upstream
                    // collecting every include's text into one list
                    // regardless of nesting depth. The nested call's own
                    // `out` (its non-#include lines) is pushed here; its
                    // OWN nested includes already pushed themselves into
                    // `accumulated` directly and bounded their own delta.
                    let resolved = collect_includes(
                        &text,
                        include,
                        depth + 1,
                        total_len,
                        include_count,
                        accumulated,
                    )?;
                    accumulated.push_str(&resolved);
                    if !resolved.ends_with('\n') {
                        accumulated.push('\n');
                    }
                }
                None => {
                    accumulated.push_str("// tried including file ");
                    accumulated.push_str(&name);
                    accumulated.push_str(" but was not found\n");
                }
            }
            accumulated.push_str("// end of included from file ");
            accumulated.push_str(&name);
            accumulated.push('\n');
            *total_len += accumulated.len() - accumulated_before;
            if *total_len > MAX_PREPROCESSED_BYTES {
                return Err(PreprocessError::SizeExceeded {
                    bytes: *total_len,
                    limit: MAX_PREPROCESSED_BYTES,
                });
            }
            continue; // the #include line itself is NOT copied into `out`
        }
        out.push_str(line);
    }
    *total_len += out.len();
    if *total_len > MAX_PREPROCESSED_BYTES {
        return Err(PreprocessError::SizeExceeded {
            bytes: *total_len,
            limit: MAX_PREPROCESSED_BYTES,
        });
    }
    Ok(out)
}

/// Byte index of the start of the line holding the file's `main(`
/// function definition — a bare `main` token (not part of a longer
/// identifier like `mainColor`) followed, after optional spaces/tabs, by
/// `(`. `None` if no such token exists (defensive; a well-formed shader
/// stage always has exactly one). The search is a single linear pass
/// (`search_from` strictly advances every iteration), so it cannot loop
/// unboundedly even over a hostile/malformed source within the existing
/// `MAX_PREPROCESSED_BYTES` cap.
fn find_main_insertion_point(body: &str) -> Option<usize> {
    let is_ident_byte = |b: u8| b.is_ascii_alphanumeric() || b == b'_';
    let mut search_from = 0usize;
    while let Some(relative) = body[search_from..].find("main") {
        let start = search_from + relative;
        let end = start + "main".len();
        let before_is_boundary = start == 0 || !is_ident_byte(body.as_bytes()[start - 1]);
        let after = &body[end..];
        let after_is_boundary = after.as_bytes().first().is_none_or(|&b| !is_ident_byte(b));
        if before_is_boundary && after_is_boundary {
            let rest = after.trim_start_matches([' ', '\t']);
            if rest.starts_with('(') {
                let line_start = body[..start].rfind('\n').map_or(0, |pos| pos + 1);
                return Some(line_start);
            }
        }
        search_from = end;
    }
    None
}

/// Resolve every `#include` in `source`, then splice ALL of their
/// resolved text in just before the file's `main(` definition — matching
/// upstream's real placement strategy, not a naive in-place text
/// substitution.
///
/// This matters: many WE shader headers (e.g. `common_blur.h`'s
/// `blur13`/`blur7`/`blur3`, which read a bare `g_Texture0`) are written
/// assuming the file that includes them has ALREADY declared the globals
/// those functions use, wherever in the file that declaration sits — a
/// pattern only sound if the included text lands after every top-level
/// declaration, which "insert at the `#include` line's own position"
/// cannot guarantee when (as in the local WE asset corpus's
/// `shine_gaussian.frag`/`godrays_gaussian.frag`/
/// `blur_precise_gaussian.frag`) the `#include` line is the FIRST line of
/// the file, before the `uniform sampler2D g_Texture0;` it depends on.
/// In-place inlining put the header's function bodies (which reference
/// `g_Texture0`) textually BEFORE that declaration, so `shaderc` reported
/// `'g_Texture0' : undeclared identifier` — this was a real, previously
/// undiagnosed bug in this module's OWN documented deviation from
/// upstream (the old doc comment's "every `#include` in the corpus sits
/// at file scope above any function definition" premise was false for
/// these three real corpus shaders).
///
/// Borrowed-From: Almamu/linux-wallpaperengine (GPL-3.0-or-later)
/// src/WallpaperEngine/Render/Shaders/ShaderUnit.cpp:136-171
/// (`preprocessIncludes` collects every include's text separately and
/// splices it in just before `main()`) @ b016d7d1 — adapted: upstream
/// additionally walks an `#if`/`#endif` stack while searching for the
/// insertion point so it cannot land inside a dead branch; every `main(`
/// in the local corpus is unconditional (not itself behind an `#if`), so
/// a plain first-occurrence search is equivalent for every shader this
/// renderer has been measured against — a shader with a combo-gated
/// `main(` would need that upstream `#if`-aware search, not implemented
/// here (falls back to appending at the very end if `main(` is not
/// found, rather than losing the included text).
fn resolve_includes(
    source: &str,
    include: &mut IncludeLookup<'_>,
    depth: usize,
    total_len: &mut usize,
    include_count: &mut usize,
) -> Result<String, PreprocessError> {
    let mut accumulated = String::new();
    let body = collect_includes(
        source,
        include,
        depth,
        total_len,
        include_count,
        &mut accumulated,
    )?;
    if accumulated.is_empty() {
        return Ok(body);
    }
    let mut spliced = String::with_capacity(body.len() + accumulated.len());
    match find_main_insertion_point(&body) {
        Some(index) => {
            spliced.push_str(&body[..index]);
            spliced.push_str(&accumulated);
            spliced.push_str(&body[index..]);
        }
        None => {
            spliced.push_str(&body);
            spliced.push_str(&accumulated);
        }
    }
    Ok(spliced)
}

fn extract_quoted(rest: &str) -> Option<String> {
    let start = rest.find('"')? + 1;
    let end = rest[start..].find('"')? + start;
    Some(rest[start..end].to_string())
}

/// Scrape `// [COMBO] {json}` (combo declaration + default) and
/// `uniform TYPE name; // {json}` (parameter metadata) lines. Returns
/// discovered combo defaults (only for combos not already in
/// `material_combos` — matches upstream: a material-specified combo value
/// always wins over the shader's own default) and the uniform metadata
/// list, in source order.
///
/// Borrowed-From: Almamu/linux-wallpaperengine (GPL-3.0-or-later)
/// src/WallpaperEngine/Render/Shaders/ShaderUnit.cpp:96-134
/// (`ShaderUnit::preprocessVariables`) @ b016d7d1 — adapted (only the
/// `[COMBO]`/commented-`uniform` detection and default-value extraction;
/// the sampler-combo-requirement resolution in
/// `parseParameterConfiguration` — `requireany`/texture-slot-driven combo
/// activation — is out of scope: GLSL treats an undefined macro in `#if`
/// as 0 (verified against `glslc`), so an un-scraped combo used only by a
/// `#if COMBO_NAME` guard still compiles, just always taking the
/// combo-off branch unless the material's own `combos` map sets it).
fn scrape(
    source: &str,
    material_combos: &BTreeMap<String, i64>,
) -> (BTreeMap<String, i64>, Vec<UniformMeta>) {
    let mut discovered = BTreeMap::new();
    let mut uniforms = Vec::new();
    for line in source.lines() {
        if let Some(json_text) = line.find("// [COMBO] ").map(|at| &line[at + 11..]) {
            if let Ok(Value::Object(object)) = serde_json::from_str::<Value>(json_text) {
                let Some(combo) = object.get("combo").and_then(Value::as_str) else {
                    continue;
                };
                // S2 review #1: a shader's own `// [COMBO]` scrape is
                // metadata too (parsed from the corpus asset file, not
                // hand-written GLSL) — reject a malformed name the same
                // way material.json's `combos` map is rejected below,
                // just by dropping the one entry rather than failing the
                // whole material (this data cannot smuggle text past a
                // `#define` line issued for a DIFFERENT combo the way an
                // external material.json entry could, since it is only
                // ever inserted here, never taken verbatim from the
                // caller).
                if !is_valid_combo_name(combo) {
                    continue;
                }
                if material_combos.contains_key(combo) || discovered.contains_key(combo) {
                    continue;
                }
                let default = match object.get("default") {
                    Some(Value::Number(n)) => n.as_i64().unwrap_or(0),
                    _ => 0,
                };
                discovered.insert(combo.to_string(), default);
            }
            continue;
        }
        let Some(semicolon) = line.find(';') else {
            continue;
        };
        let Some(comment) = line.find("// ") else {
            continue;
        };
        if !(line.contains("uniform ") && semicolon < comment) {
            continue;
        }
        let Some(last_space) = line[..semicolon].rfind(' ') else {
            continue;
        };
        let Some(previous_space) = line[..last_space].rfind(' ') else {
            continue;
        };
        let glsl_type = line[previous_space + 1..last_space].trim().to_string();
        let name = line[last_space + 1..semicolon].trim().to_string();
        if glsl_type.is_empty() || name.is_empty() || name.contains('[') {
            continue;
        }
        let json = serde_json::from_str::<Value>(&line[comment + 3..]).ok();
        uniforms.push(UniformMeta {
            glsl_type,
            name,
            json,
        });
    }
    (discovered, uniforms)
}

/// Replace every `#require ModuleName` line: `LightingV1` gets the stub
/// function inlined in place (the corpus's only implemented module,
/// mirroring `ShaderUnit::resolveRequireModule`); anything else is
/// commented out (unimplemented, not a hard failure — matches upstream's
/// `sLog.error` + continue behavior).
fn resolve_requires(source: &str) -> String {
    let mut out = String::with_capacity(source.len());
    for line in source.split_inclusive('\n') {
        let trimmed = line.trim_start();
        if let Some(rest) = trimmed.strip_prefix("#require") {
            let module = rest.trim();
            if module == "LightingV1" {
                out.push_str(LIGHTING_V1_STUB);
            } else {
                out.push_str("// unresolved #require ");
                out.push_str(module);
                out.push('\n');
            }
            continue;
        }
        out.push_str(line);
    }
    out
}

/// The trailing swizzle that narrows a `vec4` UBO slot down to the
/// shader's OWN declared type for a folded standard uniform — S4b fix
/// (found via the local WE asset corpus's `common_perspective.h`
/// `squareToQuad`/`inverse` chain, see `standard_uniform_expr`'s
/// `g_Point<N>` case): several standard uniforms are backed by a `vec4`
/// UBO field regardless of what any ONE shader declares locally (e.g.
/// `effects/perspective.vert` declares `uniform vec2 g_Point0;` while the
/// shared `u_Std.g_Point_[8]` UBO field is `vec4`), and folding to the
/// bare vec4 expression silently changes the call-site TYPE the rest of
/// the shader sees — `squareToQuad(vec2, vec2, vec2, vec2)` called with
/// four `vec4` arguments is not the same overload, so `shaderc` reports
/// "no matching overloaded function found" for a function that outright
/// EXISTS, and any assignment expecting the narrower type then also
/// mismatches ("cannot convert from const float to 3x3 matrix" was the
/// SAME root cause's second symptom on the very same corpus shader,
/// cascading from `squareToQuad`'s own broken call). `glsl_type` is the
/// EXACT type token this shader's own declaration used, straight from
/// `parse_decl` — narrowing only ever DROPS trailing components (`.xy`/
/// `.xyz`), never invents data, so a `vec4`-declaring shader (the common
/// case) is untouched (empty swizzle).
fn narrowing_swizzle(glsl_type: &str) -> &'static str {
    match glsl_type {
        "vec2" => ".xy",
        "vec3" => ".xyz",
        _ => "",
    }
}

/// The WE "standard" uniform set this material pipeline provides via the
/// `MaterialUniforms` UBO (materialshader.rs) — name -> GLSL expression
/// reaching the matching UBO field. `g_Texture<N>Resolution` doubles as
/// texel size (`.zw`), matching WE's own convention (a resolution uniform
/// that is `vec4(width, height, 1/width, 1/height)`); `g_TexelSize` (no
/// number) aliases texture slot 0's, the common single-texture case.
/// `glsl_type` is the shader's OWN declared type for this uniform
/// (`parse_decl`'s scraped type token) — see `narrowing_swizzle`.
fn standard_uniform_expr(name: &str, glsl_type: &str) -> Option<String> {
    if let Some(rest) = name.strip_prefix("g_Texture")
        && let Some(index_str) = rest.strip_suffix("Resolution")
        && let Ok(index) = index_str.parse::<u32>()
        && index < MAX_MATERIAL_TEXTURES as u32
    {
        return Some(format!("u_Std.g_TextureResolution_[{index}]"));
    }
    // S4: `g_Texture<N>Rotation` (vec4) feeds every corpus `genericimage*`
    // vertex shader's own UV transform verbatim —
    // `v_TexCoord.xy = g_Texture<N>Translation + a_TexCoord.x *
    // g_Texture<N>Rotation.xy + a_TexCoord.y * g_Texture<N>Rotation.zw`
    // (`genericimage2.vert:102`/`genericimage3.vert:153`/
    // `genericimage4.vert:177`/`genericimage.vert:30` in the local WE
    // asset corpus, byte-identical formula in all four). This renderer
    // does not implement WE's scripted per-texture rotate/scale property
    // (out of scope — no corpus scene load path feeds it a live value),
    // but the identity transform for that formula is `vec4(1.0, 0.0,
    // 0.0, 1.0)`, NOT the zero this name previously fell through to via
    // `fold_declarations`'s generic zero-default path: a ZERO rotation
    // matrix collapses every sampled UV to a single point, so a
    // previously-refused material (S4 widened `material_vertex_format_supported`
    // to accept it) would compile and bind cleanly but draw a flat,
    // wrong colour instead of its real texture — found via the 60-scene
    // corpus sweep (Workshop 3100709479's `genericimage3` material).
    // `g_Texture<N>Translation` (vec2) is left on the ordinary
    // zero-default path: `vec2(0.0, 0.0)` IS the correct identity
    // translation for the same formula, so no special case is needed
    // there.
    //
    // S4a review RECOMMENDED #5: this identity default is a reasonable
    // ENGINEERING CHOICE, not literally upstream's own default — traced
    // against `CPass.cpp`/`CPass.h` in linux-wallpaperengine,
    // `TextureAnimationState::rotation`'s real raw default is `{0,0,0,0}`
    // (upstream only ever WRITES a non-zero rotation for a genuinely
    // GIF-animated texture). Every corpus occurrence of this formula
    // (`genericimage.vert:30`/`genericimage2.vert:102`/
    // `genericimage3.vert:153`/`genericimage4.vert:177`) only READS
    // `g_Texture<N>Rotation` inside `#if SPRITESHEET` (its `v_TexCoord`
    // assignment sits in the `#if SPRITESHEET ... #else v_TexCoord.xy =
    // a_TexCoord; #endif` branch) — with `SPRITESHEET` off (the common
    // case), this uniform's value is DECLARED (unconditionally) but
    // never actually READ, so the identity default below is inert there
    // either way. It only matters once a material sets `SPRITESHEET=1`,
    // which this renderer does not decode real per-frame spritesheet/GIF
    // atlas data for at all (a separate, documented gap) — identity is
    // still a strictly better approximation than the old zero-default
    // for that unimplemented case (matches this same file's
    // `g_ModelMatrix`/`g_ViewProjectionMatrix` reasoning), but it would
    // still be visually WRONG for a real multi-frame spritesheet material
    // (stretches the whole atlas across one quad instead of windowing a
    // single frame) if one is ever encountered — not validated against a
    // genuinely SPRITESHEET-live corpus case.
    if let Some(rest) = name.strip_prefix("g_Texture")
        && let Some(index_str) = rest.strip_suffix("Rotation")
        && let Ok(index) = index_str.parse::<u32>()
        && index < MAX_MATERIAL_TEXTURES as u32
    {
        return Some("vec4(1.0, 0.0, 0.0, 1.0)".to_string());
    }
    if let Some(rest) = name.strip_prefix("g_Point")
        && let Ok(index) = rest.parse::<u32>()
        && index < 8
    {
        return Some(format!(
            "u_Std.g_Point_[{index}]{}",
            narrowing_swizzle(glsl_type)
        ));
    }
    Some(
        match name {
            "g_ModelViewProjectionMatrix" => "u_Std.g_ModelViewProjectionMatrix_",
            "g_EffectTextureProjectionMatrix" => "u_Std.g_EffectTextureProjectionMatrix_",
            // S4a review MUST-FIX #3: `g_ModelMatrix`/`g_ViewProjectionMatrix`/
            // `g_NormalModelMatrix` (+ their `Alt` siblings) previously fell
            // through to the generic `mat4(0.0)`/`mat3(0.0)` zero-default —
            // the SAME "zero should be identity" bug class already fixed
            // above for `g_Texture<N>Rotation`/`g_Color4`, just for
            // matrices that feed vertex POSITION, not only lighting.
            // Traced in the local WE asset corpus
            // (`genericimage3.vert:56,58,163`): `worldPos = mul(vec4(localPos,
            // 1.0), M_MDL)` (`M_MDL` = `g_ModelMatrix` outside
            // `PRELIGHTING`) runs UNCONDITIONALLY on every draw of this
            // shader family, and once a material sets `LIGHTING=1`
            // (legitimate — a sibling gate on this exact `a_Normal`/
            // `a_Color` attribute family this slice widened acceptance
            // to, e.g. `materials/util/flatalphavertexcolor.json`),
            // `gl_Position` itself is computed from `worldPos *
            // g_ViewProjectionMatrix` — two zero matrices multiplied
            // through collapse the object's on-screen GEOMETRY to a
            // single degenerate point, not merely "missing lighting."
            // None of the 60 local corpus scenes happen to set
            // `LIGHTING`/`REFLECTION`/`VERTEXCOLOR` on a
            // `genericimage2/3/4`-family material, so the 60-scene sweep
            // could not have caught this the way it caught the
            // `g_Texture<N>Rotation`/`g_Color4` regressions — found by
            // adversarial review instead. Identity (`mat4(1.0)`/
            // `mat3(1.0)`) is the correct "no real transform implemented"
            // default, matching this slice's own `g_Texture0Rotation`
            // reasoning (a zero matrix is essentially never the
            // intentionally-correct default for an UNIMPLEMENTED
            // transform). This does not implement real per-object model
            // transforms, alt-camera matrices, or normal-matrix lighting
            // math (`#require LightingV1` still resolves to a
            // zero-contribution stub) — it only stops an identity-adjacent
            // computation from silently zeroing out the object's own
            // screen position.
            "g_ModelMatrix"
            | "g_AltModelMatrix"
            | "g_ViewProjectionMatrix"
            | "g_AltViewProjectionMatrix" => "mat4(1.0)",
            "g_NormalModelMatrix" | "g_AltNormalModelMatrix" => "mat3(1.0)",
            "g_Color" => "u_Std.g_Color_",
            "g_ParallaxPosition" => "u_Std.g_ParallaxPointer_.xy",
            "g_PointerPosition" => "u_Std.g_ParallaxPointer_.zw",
            "g_TexelSize" => "u_Std.g_TextureResolution_[0].zw",
            "g_Time" => "u_Std.g_TimeAlphaBrightness_.x",
            "g_UserAlpha" => "u_Std.g_TimeAlphaBrightness_.y",
            "g_Brightness" => "u_Std.g_TimeAlphaBrightness_.z",
            // S4: `g_Color4` (vec4) is the "newer material `VERSION`"
            // replacement for the `g_Brightness`/`g_UserAlpha` pair
            // above — `genericimage2.frag`/`genericimage3.frag`/
            // `genericimage4.frag` (local WE asset corpus) all gate on
            // `#ifndef VERSION { uniform g_Brightness; uniform
            // g_UserAlpha; ... color.rgb *= g_Brightness; color.a *=
            // g_UserAlpha; } #else { uniform vec4 g_Color4; ... color *=
            // g_Color4; }` — the SAME per-draw brightness/alpha values
            // this pipeline already threads through
            // `u_Std.g_TimeAlphaBrightness_`, just packed into one vec4
            // instead of two scalars. Before this fix, `g_Color4` fell
            // through to the generic zero-default (`vec4(0.0)`), which
            // multiplies every `VERSION`-tagged material's sampled
            // colour to fully transparent black regardless of the
            // object's real brightness/alpha — found via the 60-scene
            // corpus sweep (Workshop 3100709479's `genericimage3`
            // material, `combos={"VERSION": 2}`).
            "g_Color4" => "vec4(u_Std.g_TimeAlphaBrightness_.zzz, u_Std.g_TimeAlphaBrightness_.y)",
            _ => return None,
        }
        .to_string(),
    )
}

/// The fixed UBO block every material shader stage declares, always at
/// `set=0, binding=8`. `materialshader::MaterialUniforms` is the Rust
/// mirror — the two must stay byte-for-byte in sync (see that module's
/// doc comment and its `std140 layout` unit test).
const MATERIAL_UBO_BLOCK: &str = "layout(set = 0, binding = 8, std140) uniform MaterialUniforms {\n\
    mat4 g_ModelViewProjectionMatrix_;\n\
    mat4 g_EffectTextureProjectionMatrix_;\n\
    vec4 g_TextureResolution_[8];\n\
    vec4 g_Point_[8];\n\
    vec4 g_Color_;\n\
    vec4 g_ParallaxPointer_;\n\
    vec4 g_TimeAlphaBrightness_;\n\
    vec4 g_MaterialConstants_[16];\n\
} u_Std;\n";

/// Cap on how many elements a zero-filled array uniform's replacement
/// initializer will enumerate (S4b fix — see `fold_declarations`'s array
/// branch). Generous over any real corpus array (the audio-spectrum
/// uniforms this fixes top out at 64 elements: `g_AudioSpectrum64Left/
/// Right`); a hostile/malformed declaration past this bound is left on
/// the pre-existing "unsupported, line passes through unmodified" path
/// rather than building an unbounded initializer-list string.
const MAX_ZERO_ARRAY_LEN: u32 = 1024;

/// Split an array declarator's `name` field (`parse_decl` leaves the
/// brackets attached here, e.g. `"g_AudioSpectrum16Left[16]"`) into the
/// bare name and element count. `None` for anything not shaped exactly
/// `IDENT[DIGITS]` (a multi-dimensional array, a non-numeric size
/// expression, or a missing/malformed bracket) — the caller's existing
/// unsupported/pass-through fallback handles those the same as before
/// this fix.
fn parse_array_declarator(name: &str) -> Option<(&str, u32)> {
    let open = name.find('[')?;
    if !name.ends_with(']') {
        return None;
    }
    let base = &name[..open];
    let size_str = &name[open + 1..name.len() - 1];
    if base.is_empty() || size_str.is_empty() || !size_str.bytes().all(|b| b.is_ascii_digit()) {
        return None;
    }
    let size: u32 = size_str.parse().ok()?;
    if size == 0 || size > MAX_ZERO_ARRAY_LEN {
        return None;
    }
    Some((base, size))
}

fn zero_literal(glsl_type: &str) -> Option<&'static str> {
    Some(match glsl_type {
        "float" => "float(0.0)",
        "int" => "int(0)",
        "bool" => "bool(false)",
        "vec2" => "vec2(0.0)",
        "vec3" => "vec3(0.0)",
        "vec4" => "vec4(0.0)",
        "mat3" => "mat3(0.0)",
        "mat4" => "mat4(0.0)",
        _ => return None,
    })
}

fn material_constant_swizzle(glsl_type: &str) -> Option<&'static str> {
    match glsl_type {
        "float" => Some(".x"),
        "vec2" => Some(".xy"),
        "vec3" => Some(".xyz"),
        "vec4" => Some(""),
        _ => None,
    }
}

/// One `#if`/`#ifdef`/`#ifndef` nesting frame while `fold_declarations`
/// scans a shader (S4). `branch_taken` is true once SOME branch at this
/// level (the opening condition or a later `#elif`) has evaluated true —
/// it blocks any further `#elif` from re-taking and is what `#else`
/// inverts. `active_here` is true while the CURRENTLY open branch at
/// this level is the taken one, already folding in every enclosing
/// frame's liveness at the moment this frame was pushed or last updated
/// (well-formed nesting means an ancestor is always fully settled before
/// a child frame can exist — see `fold_declarations`'s `is_live`). A
/// line is only scraped/folded while EVERY frame on the stack has
/// `active_here == true`.
struct CondFrame {
    branch_taken: bool,
    active_here: bool,
}

/// Recursion-depth bound for `evaluate_if_expr`'s parser (nested parens
/// or a chained `!` run) — S4a review MUST-FIX #1: with no bound, a
/// single `#if` line comfortably under `MAX_PREPROCESSED_BYTES` (e.g.
/// `"#if "` + `"("` x 400,000 + `"1"` + `")"` x 400,000) drove the
/// recursive-descent parser several hundred thousand stack frames deep —
/// a native stack overflow (`SIGSEGV`/uncatchable), not a panic, killing
/// the whole worker process, not just the one material. 32 is generous
/// over any real WE `#if` expression (all one-line boolean combinations
/// per the local corpus survey) while nowhere near stack-exhausting.
/// Mirrors `MAX_INCLUDE_DEPTH`'s existing pattern in this same file.
const MAX_IF_EXPR_DEPTH: usize = 32;
/// Token-count bound for one `#if`/`#elif` expression, independent of
/// the depth bound above (a long CHAIN like `a && b && c && ...` has
/// depth 1 but unbounded token count) — checked during tokenization so a
/// pathological line cannot even build a huge `Token` vector first.
const MAX_IF_EXPR_TOKENS: usize = 256;

/// Evaluate a `#if`/`#elif` boolean/integer expression against known
/// combo values, for `fold_declarations`'s live-branch tracking. Combo
/// names are matched CASE-SENSITIVELY against `combos_upper`'s
/// already-upper-cased keys (S4a review RECOMMENDED #4): `#define`s are
/// always emitted upper-cased (`preprocess`'s `#define {}
/// {}",name.to_uppercase()`), and `shaderc`'s real preprocessor matches
/// macro names case-sensitively too, so a shader must write the combo
/// name in the SAME upper-cased form to ever really match — folding case
/// on the identifier side here (as an earlier version of this function
/// did) would accept e.g. `#if skinning` as if it were `#if SKINNING`,
/// which `shaderc` would not. Supports `||`, `&&`, `==`, `!=`, unary `!`,
/// parentheses, decimal integer literals, bare identifiers (truthy iff
/// nonzero — an unknown identifier is `0`, matching the real GLSL
/// preprocessor's "undefined macro in `#if` is 0" rule, already relied
/// on by `scrape`'s doc comment), and `defined(NAME)` / `defined NAME`.
/// Returns `None` for any expression shape this minimal recursive-descent
/// parser does not fully consume, OR once `MAX_IF_EXPR_DEPTH`/
/// `MAX_IF_EXPR_TOKENS` is exceeded — `fold_declarations` (S4a review
/// MUST-FIX #2) treats `None` as "cannot safely judge this branch" and
/// rejects the whole material via `PreprocessError::AmbiguousCondition`
/// rather than guessing a truth value that could suppress a genuinely-
/// live sibling `#else`/`#elif` branch.
fn evaluate_if_expr(expr: &str, combos_upper: &BTreeMap<String, i64>) -> Option<bool> {
    #[derive(Debug, Clone, PartialEq)]
    enum Token {
        Ident(String),
        Num(i64),
        And,
        Or,
        Eq,
        Ne,
        Not,
        LParen,
        RParen,
    }
    fn tokenize(expr: &str) -> Option<Vec<Token>> {
        let bytes = expr.as_bytes();
        let mut i = 0usize;
        let mut tokens = Vec::new();
        while i < bytes.len() {
            let c = bytes[i] as char;
            if c.is_whitespace() {
                i += 1;
                continue;
            }
            if tokens.len() >= MAX_IF_EXPR_TOKENS {
                return None;
            }
            match c {
                '(' => {
                    tokens.push(Token::LParen);
                    i += 1;
                }
                ')' => {
                    tokens.push(Token::RParen);
                    i += 1;
                }
                '&' if bytes.get(i + 1) == Some(&b'&') => {
                    tokens.push(Token::And);
                    i += 2;
                }
                '|' if bytes.get(i + 1) == Some(&b'|') => {
                    tokens.push(Token::Or);
                    i += 2;
                }
                '=' if bytes.get(i + 1) == Some(&b'=') => {
                    tokens.push(Token::Eq);
                    i += 2;
                }
                '!' if bytes.get(i + 1) == Some(&b'=') => {
                    tokens.push(Token::Ne);
                    i += 2;
                }
                '!' => {
                    tokens.push(Token::Not);
                    i += 1;
                }
                _ if c.is_ascii_digit() => {
                    let start = i;
                    while i < bytes.len() && (bytes[i] as char).is_ascii_digit() {
                        i += 1;
                    }
                    let n: i64 = expr[start..i].parse().ok()?;
                    tokens.push(Token::Num(n));
                }
                _ if c.is_ascii_alphabetic() || c == '_' => {
                    let start = i;
                    while i < bytes.len()
                        && ((bytes[i] as char).is_ascii_alphanumeric() || bytes[i] == b'_')
                    {
                        i += 1;
                    }
                    tokens.push(Token::Ident(expr[start..i].to_string()));
                }
                _ => return None,
            }
        }
        Some(tokens)
    }
    // S4a review RECOMMENDED #4: exact-case lookup — `combos_upper`'s
    // keys are already upper-cased (built once in `fold_declarations`
    // from the material's resolved combos), and matching a bare
    // identifier by ITS OWN case (not folded) mirrors `shaderc`'s real,
    // case-sensitive macro lookup against the always-upper-cased
    // `#define` this module emits.
    fn lookup(name: &str, combos_upper: &BTreeMap<String, i64>) -> i64 {
        combos_upper.get(name).copied().unwrap_or(0)
    }
    struct Parser<'a> {
        tokens: &'a [Token],
        pos: usize,
        combos_upper: &'a BTreeMap<String, i64>,
        /// S4a review MUST-FIX #1: monotonically increasing (never
        /// decremented) count of `or_expr`/`unary_expr` recursive
        /// entries during THIS parse — a fresh `Parser` (and so a fresh
        /// `depth = 0`) is built once per `evaluate_if_expr` call, i.e.
        /// once per `#if`/`#elif` LINE, so this bounds total recursive
        /// descent for one expression without needing careful
        /// increment/decrement bookkeeping on every return path (a
        /// stricter, always-safe superset of true nesting depth).
        depth: usize,
    }
    impl<'a> Parser<'a> {
        fn peek(&self) -> Option<&Token> {
            self.tokens.get(self.pos)
        }
        fn enter(&mut self) -> Option<()> {
            self.depth += 1;
            if self.depth > MAX_IF_EXPR_DEPTH {
                return None;
            }
            Some(())
        }
        fn or_expr(&mut self) -> Option<i64> {
            self.enter()?;
            let mut lhs = self.and_expr()?;
            while self.peek() == Some(&Token::Or) {
                self.pos += 1;
                let rhs = self.and_expr()?;
                lhs = i64::from(lhs != 0 || rhs != 0);
            }
            Some(lhs)
        }
        fn and_expr(&mut self) -> Option<i64> {
            let mut lhs = self.eq_expr()?;
            while self.peek() == Some(&Token::And) {
                self.pos += 1;
                let rhs = self.eq_expr()?;
                lhs = i64::from(lhs != 0 && rhs != 0);
            }
            Some(lhs)
        }
        fn eq_expr(&mut self) -> Option<i64> {
            let mut lhs = self.unary_expr()?;
            loop {
                match self.peek() {
                    Some(Token::Eq) => {
                        self.pos += 1;
                        let rhs = self.unary_expr()?;
                        lhs = i64::from(lhs == rhs);
                    }
                    Some(Token::Ne) => {
                        self.pos += 1;
                        let rhs = self.unary_expr()?;
                        lhs = i64::from(lhs != rhs);
                    }
                    _ => break,
                }
            }
            Some(lhs)
        }
        fn unary_expr(&mut self) -> Option<i64> {
            if self.peek() == Some(&Token::Not) {
                self.enter()?;
                self.pos += 1;
                let v = self.unary_expr()?;
                return Some(i64::from(v == 0));
            }
            self.primary_expr()
        }
        fn primary_expr(&mut self) -> Option<i64> {
            match self.tokens.get(self.pos)?.clone() {
                Token::Num(n) => {
                    self.pos += 1;
                    Some(n)
                }
                Token::Ident(name) => {
                    self.pos += 1;
                    if name == "defined" {
                        let has_parens = self.peek() == Some(&Token::LParen);
                        if has_parens {
                            self.pos += 1;
                        }
                        let Some(Token::Ident(target)) = self.tokens.get(self.pos).cloned() else {
                            return None;
                        };
                        self.pos += 1;
                        if has_parens {
                            if self.peek() != Some(&Token::RParen) {
                                return None;
                            }
                            self.pos += 1;
                        }
                        return Some(i64::from(self.combos_upper.contains_key(&target)));
                    }
                    Some(lookup(&name, self.combos_upper))
                }
                Token::LParen => {
                    self.pos += 1;
                    let v = self.or_expr()?;
                    if self.peek() != Some(&Token::RParen) {
                        return None;
                    }
                    self.pos += 1;
                    Some(v)
                }
                _ => None,
            }
        }
    }
    let tokens = tokenize(expr)?;
    let mut parser = Parser {
        tokens: &tokens,
        pos: 0,
        combos_upper,
        depth: 0,
    };
    let value = parser.or_expr()?;
    if parser.pos != tokens.len() {
        return None;
    }
    Some(value != 0)
}

/// Rewrite every `attribute`/`varying`/non-sampler-`uniform` declaration
/// this module recognizes so the result is legal Vulkan GLSL (see the
/// module doc's point 2). Declarations this function does not recognize
/// (arrays, unsupported types, anything not matching the simple
/// `QUALIFIER TYPE name;` shape) are left untouched — if the shader
/// actually needs them to compile, `shaderc` reports the failure and the
/// caller falls back; this module does not try to predict every way that
/// can happen.
///
/// `varying_locations` is shared across the vertex and fragment calls for
/// one material (the caller preprocesses vertex first) so a varying with
/// the same name gets the SAME `layout(location=)` in both stages — the
/// two stages are linked purely by matching locations in a Vulkan
/// pipeline, there is no by-name linking step the way there is in the
/// upstream/OpenGL target.
#[allow(clippy::too_many_lines)]
/// `(folded source, scraped attributes, referenced sampler slots, uniform
/// names that got a zero-default fallback)` — `fold_declarations`'s
/// return shape, named to satisfy `clippy::type_complexity`.
type FoldedDeclarations = (String, Vec<AttributeDecl>, Vec<u32>, Vec<String>);

fn fold_declarations(
    source: &str,
    material_constants: &[String],
    varying_locations: &mut BTreeMap<String, u32>,
    combos: &BTreeMap<String, i64>,
) -> Result<FoldedDeclarations, PreprocessError> {
    let combos_upper: BTreeMap<String, i64> = combos
        .iter()
        .map(|(name, value)| (name.to_uppercase(), *value))
        .collect();
    let mut out = String::with_capacity(source.len());
    let mut attributes = Vec::new();
    let mut sampler_slots = Vec::new();
    let mut unsupported = Vec::new();
    let mut next_attribute_location = 0u32;
    let mut next_sampler_slot = 0u32;
    let mut used_sampler_slots: [bool; MAX_MATERIAL_TEXTURES] = [false; MAX_MATERIAL_TEXTURES];
    // S4: `#if`/`#ifdef`/`#ifndef`/`#elif`/`#else`/`#endif` nesting stack —
    // see `CondFrame`. A declaration is only scraped/folded (and only
    // consumes an attribute location / sampler slot / varying location)
    // while every frame on the stack is `live`; an unbalanced `#endif`
    // (more pops than pushes) is tolerated by simply not popping past
    // empty — this function does not attempt to validate shader syntax,
    // only to track it well enough to avoid mis-scraping dead branches
    // (`shaderc`, later, is the actual arbiter of validity).
    let mut cond_stack: Vec<CondFrame> = Vec::new();
    let is_live = |stack: &[CondFrame]| stack.iter().all(|frame| frame.active_here);

    for line in source.split_inclusive('\n') {
        let trimmed = line.trim();
        let indent = &line[..line.len() - line.trim_start().len()];
        let newline = if line.ends_with('\n') { "\n" } else { "" };

        if let Some(rest) = trimmed.strip_prefix("#ifdef ") {
            // S4a review RECOMMENDED #4: exact-case match — see
            // `evaluate_if_expr::lookup`'s doc comment.
            let parent_live = is_live(&cond_stack);
            let taken = parent_live && combos_upper.contains_key(rest.trim());
            cond_stack.push(CondFrame {
                branch_taken: taken,
                active_here: taken,
            });
            out.push_str(line);
            continue;
        } else if let Some(rest) = trimmed.strip_prefix("#ifndef ") {
            let parent_live = is_live(&cond_stack);
            let taken = parent_live && !combos_upper.contains_key(rest.trim());
            cond_stack.push(CondFrame {
                branch_taken: taken,
                active_here: taken,
            });
            out.push_str(line);
            continue;
        } else if let Some(rest) = trimmed.strip_prefix("#if ") {
            let parent_live = is_live(&cond_stack);
            // S4a review MUST-FIX #2: only ASK `evaluate_if_expr` when
            // the parent scope is actually live (`&&` short-circuits, so
            // a dead parent never even evaluates the child condition --
            // its truth value cannot matter, matching the existing
            // "don't scrape inside a dead branch" contract). When the
            // parent IS live and the condition cannot be judged, reject
            // the whole material (`AmbiguousCondition`) instead of
            // guessing `true` -- guessing can silently suppress a
            // genuinely-live sibling `#else`/`#elif` branch's
            // declarations (see the finding).
            let taken = if parent_live {
                match evaluate_if_expr(rest.trim(), &combos_upper) {
                    Some(value) => value,
                    None => {
                        return Err(PreprocessError::AmbiguousCondition(
                            rest.trim().chars().take(200).collect(),
                        ));
                    }
                }
            } else {
                false
            };
            cond_stack.push(CondFrame {
                branch_taken: taken,
                active_here: taken,
            });
            out.push_str(line);
            continue;
        } else if let Some(rest) = trimmed.strip_prefix("#elif ") {
            // Parent liveness is everything BELOW the top frame.
            let parent_live = cond_stack.len() < 2
                || cond_stack[..cond_stack.len() - 1]
                    .iter()
                    .all(|frame| frame.active_here);
            if let Some(frame) = cond_stack.last_mut() {
                if frame.branch_taken {
                    frame.active_here = false;
                } else if parent_live {
                    let taken = match evaluate_if_expr(rest.trim(), &combos_upper) {
                        Some(value) => value,
                        None => {
                            return Err(PreprocessError::AmbiguousCondition(
                                rest.trim().chars().take(200).collect(),
                            ));
                        }
                    };
                    frame.branch_taken = taken;
                    frame.active_here = taken;
                } else {
                    frame.active_here = false;
                }
            }
            out.push_str(line);
            continue;
        } else if trimmed == "#else" {
            let parent_live = cond_stack.len() < 2
                || cond_stack[..cond_stack.len() - 1]
                    .iter()
                    .all(|frame| frame.active_here);
            if let Some(frame) = cond_stack.last_mut() {
                frame.active_here = parent_live && !frame.branch_taken;
                frame.branch_taken = true;
            }
            out.push_str(line);
            continue;
        } else if trimmed == "#endif" {
            cond_stack.pop();
            out.push_str(line);
            continue;
        }

        if !is_live(&cond_stack) {
            out.push_str(line);
            continue;
        }

        if let Some(rest) = trimmed.strip_prefix("attribute ") {
            if let Some((glsl_type, name)) = parse_decl(rest) {
                let location = next_attribute_location;
                next_attribute_location += 1;
                attributes.push(AttributeDecl {
                    glsl_type: glsl_type.clone(),
                    name: name.clone(),
                    location,
                });
                out.push_str(indent);
                out.push_str(&format!(
                    "layout(location = {location}) attribute {glsl_type} {name};{newline}"
                ));
                continue;
            }
        } else if let Some(rest) = trimmed.strip_prefix("varying ") {
            if let Some((glsl_type, name)) = parse_decl(rest) {
                let next = varying_locations.len() as u32;
                let location = *varying_locations.entry(name.clone()).or_insert(next);
                out.push_str(indent);
                out.push_str(&format!(
                    "layout(location = {location}) varying {glsl_type} {name};{newline}"
                ));
                continue;
            }
        } else if let Some(rest) = trimmed.strip_prefix("uniform ")
            && let Some((glsl_type, name)) = parse_decl(rest)
        {
            if glsl_type == "sampler2D" {
                let index = if let Some(n) = name
                    .strip_prefix("g_Texture")
                    .and_then(|s| s.parse::<u32>().ok())
                {
                    n
                } else {
                    while (next_sampler_slot as usize) < MAX_MATERIAL_TEXTURES
                        && used_sampler_slots[next_sampler_slot as usize]
                    {
                        next_sampler_slot += 1;
                    }
                    next_sampler_slot
                };
                if index as usize >= MAX_MATERIAL_TEXTURES {
                    return Err(PreprocessError::TooManyTextures { name, index });
                }
                used_sampler_slots[index as usize] = true;
                sampler_slots.push(index);
                out.push_str(indent);
                out.push_str(&format!(
                    "layout(set = 0, binding = {index}) uniform sampler2D {name};{newline}"
                ));
                continue;
            }
            if !glsl_type.contains('[') && !name.contains('[') {
                if let Some(expr) = standard_uniform_expr(&name, &glsl_type) {
                    out.push_str(indent);
                    out.push_str(&format!("#define {name} {expr}{newline}"));
                    continue;
                }
                if let Some(slot) = material_constants.iter().position(|n| n == &name)
                    && slot < MAX_MATERIAL_CONSTANTS
                    && let Some(swizzle) = material_constant_swizzle(&glsl_type)
                {
                    out.push_str(indent);
                    out.push_str(&format!(
                        "#define {name} u_Std.g_MaterialConstants_[{slot}]{swizzle}{newline}"
                    ));
                    continue;
                }
                if let Some(zero) = zero_literal(&glsl_type) {
                    unsupported.push(name.clone());
                    out.push_str(indent);
                    out.push_str(&format!("const {glsl_type} {name} = {zero};{newline}"));
                    continue;
                }
            } else if !glsl_type.contains('[')
                && let Some((base, size)) = parse_array_declarator(&name)
                && let Some(zero) = zero_literal(&glsl_type)
            {
                // S4b fix: an ARRAY uniform (e.g. `uniform float
                // g_AudioSpectrum16Left[16];`, the real corpus shape that
                // surfaced this — an audio-visualizer material declaring
                // spectrum-band arrays this renderer does not feed real
                // audio data into) previously fell all the way through to
                // the generic per-line pass-through below, leaving the
                // ORIGINAL loose `uniform ... [N];` line in the compiled
                // output — Vulkan's GLSL profile requires every
                // non-opaque uniform to live inside a named block, so
                // `shaderc` reported `'non-opaque uniforms outside a
                // block' : not allowed when using GLSL for Vulkan`. Same
                // "still compiles, just inert" contract as the scalar/
                // vector zero-default just above: a NON-const array
                // (GLSL's `const` array initializer rules are stricter
                // than a plain global's) zero-initialized with an
                // explicit `TYPE[N](...)` constructor — GLSL has no
                // `TYPE[N] = TYPE(0.0)` broadcast shorthand, every element
                // must be listed.
                unsupported.push(name.clone());
                out.push_str(indent);
                out.push_str(&format!(
                    "{glsl_type} {base}[{size}] = {glsl_type}[{size}](",
                ));
                for i in 0..size {
                    if i > 0 {
                        out.push_str(", ");
                    }
                    out.push_str(zero);
                }
                out.push_str(&format!(");{newline}"));
                continue;
            }
            unsupported.push(name.clone());
        }
        out.push_str(line);
    }
    Ok((out, attributes, sampler_slots, unsupported))
}

/// GLSL precision qualifiers that may precede a type in a declaration
/// (`attribute mediump vec2 a_TexCoord;` — one real corpus shader,
/// `puppettexturechannels.vert`, uses this). Stripped by `parse_decl` so
/// `glsl_type` is always the bare type (`"vec2"`, never `"mediump
/// vec2"`) — every caller (attribute-shape matching in
/// `main.rs::material_vertex_format_supported`, uniform-slot mapping in
/// `standard_uniform_expr`, the folded `layout(...)` line this module
/// emits) compares/formats `glsl_type` as a bare type name.
const PRECISION_QUALIFIERS: [&str; 3] = ["lowp", "mediump", "highp"];

/// Parse `TYPE NAME` (or `TYPE NAME[N]`) up to the first `;`, tolerating a
/// trailing `// comment` and an optional leading precision qualifier.
/// Returns `None` for anything that does not match (arrays are returned
/// WITH the brackets still in `name`/`glsl_type`, so callers can detect
/// and skip them explicitly rather than silently mis-parsing).
fn parse_decl(rest: &str) -> Option<(String, String)> {
    let semicolon = rest.find(';')?;
    let body = rest[..semicolon].trim();
    let last_space = body.rfind(' ')?;
    let mut glsl_type = body[..last_space].trim();
    for qualifier in PRECISION_QUALIFIERS {
        if let Some(rest) = glsl_type.strip_prefix(qualifier)
            && rest.starts_with(char::is_whitespace)
        {
            glsl_type = rest.trim_start();
            break;
        }
    }
    let glsl_type = glsl_type.to_string();
    let name = body[last_space + 1..].trim().to_string();
    if glsl_type.is_empty() || name.is_empty() {
        return None;
    }
    Some((glsl_type, name))
}

/// Preprocess one shader stage. `material_combos` is the material's own
/// combo overrides (`ResolvedModel::combos`, already parsed by
/// `kwe_core::scenemodel`); `material_constants` is the ordered list of
/// non-standard uniform names this material provides a constant for
/// (built by the caller from `ResolvedModel::constant_shader_values`,
/// deterministically ordered, capped at `MAX_MATERIAL_CONSTANTS`).
/// `varying_locations` must be the SAME map across the vertex and
/// fragment calls for one material (see `fold_declarations`).
pub fn preprocess(
    stage: Stage,
    file_label: &str,
    source: &str,
    material_combos: &BTreeMap<String, i64>,
    material_constants: &[String],
    varying_locations: &mut BTreeMap<String, u32>,
    include: &mut IncludeLookup<'_>,
) -> Result<PreprocessOutput, PreprocessError> {
    // S2 review #1: `material_combos` is built by the caller straight
    // from `material.json`'s own `combos` map — external, untrusted
    // metadata that (unlike the shader text itself) a hostile Workshop
    // package can control WITHOUT also owning the `.vert`/`.frag`
    // source, by pointing `shader` at any trusted corpus shader. Reject
    // the whole material (the caller's existing fallback path) rather
    // than silently dropping or truncating a bad entry — a JSON string
    // value can carry an escaped `\n` that `serde_json` decodes into a
    // real LF byte even though the JSON sat on one physical line, which
    // would otherwise let a crafted name break out of its own `#define`
    // line and inject arbitrary further GLSL/preprocessor text into a
    // vetted shader.
    for name in material_combos.keys() {
        if !is_valid_combo_name(name) {
            return Err(PreprocessError::InvalidComboName(name.clone()));
        }
    }

    let mut total_len = 0usize;
    let mut include_count = 0usize;
    let included = resolve_includes(source, include, 0, &mut total_len, &mut include_count)?;
    let required = resolve_requires(&included);
    let (discovered_combos, uniforms) = scrape(&required, material_combos);

    let mut combos: BTreeMap<String, i64> = BTreeMap::new();
    for (name, value) in material_combos {
        combos.insert(name.clone(), *value);
    }
    for (name, value) in &discovered_combos {
        combos.entry(name.clone()).or_insert(*value);
    }

    let (folded, attributes, sampler_slots, unsupported_uniforms) =
        fold_declarations(&required, material_constants, varying_locations, &combos)?;

    let mut source_out = String::new();
    source_out.push_str(&format!(
        "// ======================================================\n// Processed shader {file_label}\n// ======================================================\n"
    ));
    source_out.push_str(SHADER_HEADER);
    source_out.push_str(match stage {
        Stage::Fragment => FRAGMENT_SHADER_DEFINES,
        Stage::Vertex => VERTEX_SHADER_DEFINES,
    });
    source_out.push_str(MATERIAL_UBO_BLOCK);
    for (name, value) in &combos {
        source_out.push_str(&format!("#define {} {}\n", name.to_uppercase(), value));
    }
    source_out.push('\n');
    source_out.push_str(&folded.replace("gl_FragColor", "out_FragColor"));

    if source_out.len() > MAX_PREPROCESSED_BYTES {
        return Err(PreprocessError::SizeExceeded {
            bytes: source_out.len(),
            limit: MAX_PREPROCESSED_BYTES,
        });
    }

    // Checked against `required` (post-include, pre-fold) rather than the
    // final `source_out`: `fold_declarations` rewrites a matched sampler
    // or uniform line and drops its trailing `// {json}` comment — which
    // is exactly where a `"default":"_rt_FullFrameBuffer"` reference
    // lives — so checking the folded text would miss it.
    let references_render_target = required.contains("_rt_");

    Ok(PreprocessOutput {
        source: source_out,
        combos,
        uniforms,
        attributes,
        sampler_slots,
        references_render_target,
        unsupported_uniforms,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    fn no_includes() -> Box<IncludeLookup<'static>> {
        Box::new(|_: &str| None)
    }

    // S2 review #1 (MUST-FIX): a material.json `combos` key can carry a
    // JSON-escaped `\n` that decodes to a real LF byte even though the
    // JSON sat on one physical line -- proving this cannot break out of
    // the emitted `#define` line and inject further shader text is the
    // whole point of the strict-identifier check.
    #[test]
    fn combo_name_with_embedded_newline_is_rejected_not_injected() {
        let mut include = no_includes();
        let mut locs = BTreeMap::new();
        let mut material_combos = BTreeMap::new();
        // Exactly the JSON-decoded shape the finding describes:
        // `"combo": "FOO\nBAR"` decodes to this literal Rust string.
        material_combos.insert("FOO\nBAR".to_string(), 0);
        let error = preprocess(
            Stage::Fragment,
            "t.frag",
            "void main(){}\n",
            &material_combos,
            &[],
            &mut locs,
            &mut include,
        )
        .unwrap_err();
        assert_eq!(
            error,
            PreprocessError::InvalidComboName("FOO\nBAR".to_string())
        );
        // Never reaches a point where this string is formatted into the
        // output at all -- there is no `source` to inspect on the `Err`
        // path, which is itself the proof: an injected `#define`/
        // `#extension`/arbitrary-GLSL line can only reach `source_out`
        // by way of the `combos` loop this check runs before.
    }

    #[test]
    fn combo_name_with_invalid_characters_is_rejected() {
        let mut include = no_includes();
        let mut locs = BTreeMap::new();
        let mut material_combos = BTreeMap::new();
        material_combos.insert("FOO BAR".to_string(), 1);
        let error = preprocess(
            Stage::Fragment,
            "t.frag",
            "void main(){}\n",
            &material_combos,
            &[],
            &mut locs,
            &mut include,
        )
        .unwrap_err();
        assert_eq!(
            error,
            PreprocessError::InvalidComboName("FOO BAR".to_string())
        );
    }

    #[test]
    fn valid_combo_name_shapes() {
        for name in ["LIGHTING", "_underscore", "combo1", "A", "a_b_C_9"] {
            assert!(is_valid_combo_name(name), "expected valid: {name}");
        }
        for name in [
            "",
            "1leading",
            "has space",
            "has\ttab",
            "has\nnewline",
            "has-dash",
        ] {
            assert!(!is_valid_combo_name(name), "expected invalid: {name}");
        }
        assert!(is_valid_combo_name(&"a".repeat(64)));
        assert!(!is_valid_combo_name(&"a".repeat(65)));
    }

    /// A shader's own `// [COMBO]` scrape is metadata too -- a malformed
    /// name there is dropped (not a hard preprocess failure, since it
    /// cannot smuggle text past a DIFFERENT combo's `#define` line the
    /// way an external material.json entry could).
    #[test]
    fn discovered_combo_with_invalid_name_is_dropped_not_defined() {
        let mut include = no_includes();
        let mut locs = BTreeMap::new();
        let source = "// [COMBO] {\"combo\":\"BAD NAME\",\"default\":1}\nvoid main(){}\n";
        let out = preprocess(
            Stage::Fragment,
            "t.frag",
            source,
            &BTreeMap::new(),
            &[],
            &mut locs,
            &mut include,
        )
        .unwrap();
        assert!(!out.combos.contains_key("BAD NAME"));
        // The offending line stays in the output as an inert comment
        // (scrape only decides whether to DEFINE a combo, it never edits
        // the source text) -- what must never appear is a #define line for
        // it.
        assert!(!out.source.contains("#define BAD NAME"));
    }

    #[test]
    fn include_count_bounded_independent_of_depth_or_size() {
        // MAX_INCLUDE_COUNT (64) sibling includes, each a tiny found file
        // well under the byte/depth caps individually -- only the count
        // cap should trip.
        let mut include: Box<IncludeLookup<'static>> =
            Box::new(|_: &str| Some(b"const float X = 1.0;\n".to_vec()));
        let mut locs = BTreeMap::new();
        let mut source = String::new();
        for i in 0..(MAX_INCLUDE_COUNT + 1) {
            source.push_str(&format!("#include \"h{i}.h\"\n"));
        }
        source.push_str("void main(){}\n");
        let error = preprocess(
            Stage::Fragment,
            "t.frag",
            &source,
            &BTreeMap::new(),
            &[],
            &mut locs,
            &mut include,
        )
        .unwrap_err();
        assert_eq!(error, PreprocessError::TooManyIncludes);
    }

    #[test]
    fn header_shim_and_frag_color_rewrite_present() {
        let mut include = no_includes();
        let mut locs = BTreeMap::new();
        let out = preprocess(
            Stage::Fragment,
            "t.frag",
            "void main() { gl_FragColor = vec4(1.0); }\n",
            &BTreeMap::new(),
            &[],
            &mut locs,
            &mut include,
        )
        .unwrap();
        assert!(out.source.contains("#version 330"));
        assert!(out.source.contains("#define lerp mix"));
        assert!(out.source.contains("out vec4 out_FragColor;"));
        assert!(out.source.contains("out_FragColor = vec4(1.0);"));
        assert!(!out.source.contains("gl_FragColor"));
    }

    #[test]
    fn combo_scraped_with_default_and_emitted_as_define() {
        let mut include = no_includes();
        let mut locs = BTreeMap::new();
        let source = "// [COMBO] {\"combo\":\"LIGHTING\",\"default\":0}\nvoid main(){}\n";
        let out = preprocess(
            Stage::Fragment,
            "t.frag",
            source,
            &BTreeMap::new(),
            &[],
            &mut locs,
            &mut include,
        )
        .unwrap();
        assert_eq!(out.combos.get("LIGHTING"), Some(&0));
        assert!(out.source.contains("#define LIGHTING 0"));
    }

    #[test]
    fn material_combo_override_wins_over_shader_default() {
        let mut include = no_includes();
        let mut locs = BTreeMap::new();
        let mut material_combos = BTreeMap::new();
        material_combos.insert("LIGHTING".to_string(), 1);
        let source = "// [COMBO] {\"combo\":\"LIGHTING\",\"default\":0}\nvoid main(){}\n";
        let out = preprocess(
            Stage::Fragment,
            "t.frag",
            source,
            &material_combos,
            &[],
            &mut locs,
            &mut include,
        )
        .unwrap();
        assert_eq!(out.combos.get("LIGHTING"), Some(&1));
        assert!(out.source.contains("#define LIGHTING 1"));
    }

    #[test]
    fn combo_disabled_marker_is_not_scraped() {
        // Real corpus files carry a `[COMBO_DISABLED]` variant that must
        // NOT match the `[COMBO] ` scrape (upstream's exact-substring
        // `"// [COMBO] "` check does not match it either).
        let mut include = no_includes();
        let mut locs = BTreeMap::new();
        let source = "// [COMBO_DISABLED] {\"combo\":\"DOUBLESIDEDLIGHTING\",\"default\":0}\nvoid main(){}\n";
        let out = preprocess(
            Stage::Fragment,
            "t.frag",
            source,
            &BTreeMap::new(),
            &[],
            &mut locs,
            &mut include,
        )
        .unwrap();
        assert!(!out.combos.contains_key("DOUBLESIDEDLIGHTING"));
    }

    #[test]
    fn uniform_metadata_scraped_with_json() {
        let mut include = no_includes();
        let mut locs = BTreeMap::new();
        let source =
            "uniform float g_UserAlpha; // {\"material\":\"Alpha\",\"default\":1}\nvoid main(){}\n";
        let out = preprocess(
            Stage::Fragment,
            "t.frag",
            source,
            &BTreeMap::new(),
            &[],
            &mut locs,
            &mut include,
        )
        .unwrap();
        assert_eq!(out.uniforms.len(), 1);
        assert_eq!(out.uniforms[0].name, "g_UserAlpha");
        assert_eq!(out.uniforms[0].glsl_type, "float");
        assert!(out.uniforms[0].json.is_some());
    }

    #[test]
    fn uncommented_plain_uniform_is_not_scraped_as_metadata() {
        // Matches upstream: only `uniform TYPE name; // json` (semicolon
        // before the comment) is a scraped "parameter" — a plain
        // `uniform mat4 g_ModelViewProjectionMatrix;` with no trailing
        // comment is not (it still gets folded into the UBO by
        // `fold_declarations`, just not recorded as metadata).
        let mut include = no_includes();
        let mut locs = BTreeMap::new();
        let source = "uniform mat4 g_ModelViewProjectionMatrix;\nvoid main(){}\n";
        let out = preprocess(
            Stage::Vertex,
            "t.vert",
            source,
            &BTreeMap::new(),
            &[],
            &mut locs,
            &mut include,
        )
        .unwrap();
        assert!(out.uniforms.is_empty());
        assert!(
            out.source
                .contains("#define g_ModelViewProjectionMatrix u_Std.g_ModelViewProjectionMatrix_")
        );
    }

    #[test]
    fn include_resolves_and_inlines() {
        let mut include: Box<IncludeLookup<'static>> = Box::new(|name: &str| {
            if name == "common.h" {
                Some(b"vec3 helper() { return vec3(1.0); }\n".to_vec())
            } else {
                None
            }
        });
        let mut locs = BTreeMap::new();
        let source = "#include \"common.h\"\nvoid main(){}\n";
        let out = preprocess(
            Stage::Fragment,
            "t.frag",
            source,
            &BTreeMap::new(),
            &[],
            &mut locs,
            &mut include,
        )
        .unwrap();
        assert!(out.source.contains("vec3 helper()"));
    }

    #[test]
    fn nested_include_resolves_two_levels() {
        let mut include: Box<IncludeLookup<'static>> = Box::new(|name: &str| match name {
            "a.h" => Some(b"#include \"b.h\"\n".to_vec()),
            "b.h" => Some(b"const float X = 1.0;\n".to_vec()),
            _ => None,
        });
        let mut locs = BTreeMap::new();
        let out = preprocess(
            Stage::Fragment,
            "t.frag",
            "#include \"a.h\"\nvoid main(){}\n",
            &BTreeMap::new(),
            &[],
            &mut locs,
            &mut include,
        )
        .unwrap();
        assert!(out.source.contains("const float X = 1.0;"));
    }

    /// The S4b fix: `common_blur.h`'s real shape (`blur13`/`blur7`/`blur3`
    /// reading a bare `g_Texture0`) assumes the INCLUDING file's own
    /// declarations are visible, wherever in the file they sit — real
    /// corpus shaders (`shine_gaussian.frag`, `godrays_gaussian.frag`,
    /// `blur_precise_gaussian.frag`) `#include` it as the FIRST line,
    /// before their own `uniform sampler2D g_Texture0;`. In-place inlining
    /// (the pre-fix behavior) would put the header's function ahead of
    /// that declaration — `shaderc` reports `'g_Texture0' : undeclared
    /// identifier`. Tests `resolve_includes` directly (not the full
    /// `preprocess` pipeline, which would fold the sampler declaration
    /// away) so the plain text ordering is pinned precisely.
    #[test]
    fn include_content_lands_before_main_not_at_its_own_line_position() {
        let mut total_len = 0usize;
        let mut include_count = 0usize;
        let mut include: Box<IncludeLookup<'static>> = Box::new(|name: &str| {
            if name == "common_blur.h" {
                Some(b"vec3 helper() { return texture(g_Texture0, vec2(0.0)).rgb; }\n".to_vec())
            } else {
                None
            }
        });
        let source =
            "#include \"common_blur.h\"\n\nuniform sampler2D g_Texture0;\n\nvoid main(){}\n";
        let out =
            resolve_includes(source, &mut include, 0, &mut total_len, &mut include_count).unwrap();
        let decl_pos = out
            .find("uniform sampler2D g_Texture0;")
            .expect("declaration present");
        let use_pos = out.find("helper()").expect("included function present");
        let main_pos = out.find("void main(").expect("main present");
        assert!(
            decl_pos < use_pos,
            "the declaration must precede the included function that reads it: {out}"
        );
        assert!(
            use_pos < main_pos,
            "included text must land before main(), not after: {out}"
        );
        // The #include line itself is gone from its original position —
        // it is not left behind as dead text once its content moved.
        assert!(!out.contains("#include \"common_blur.h\""));
    }

    /// A source with no `main(` at all (malformed input, defensive-only —
    /// every real shader stage has exactly one) must not silently drop
    /// the included text; it appends at the end instead.
    #[test]
    fn include_with_no_main_appends_at_the_end_instead_of_dropping() {
        let mut total_len = 0usize;
        let mut include_count = 0usize;
        let mut include: Box<IncludeLookup<'static>> =
            Box::new(|_: &str| Some(b"const float X = 1.0;\n".to_vec()));
        let out = resolve_includes(
            "#include \"a.h\"\nsome_other_content();\n",
            &mut include,
            0,
            &mut total_len,
            &mut include_count,
        )
        .unwrap();
        assert!(out.contains("const float X = 1.0;"));
        assert!(out.contains("some_other_content();"));
    }

    /// `main` used as part of a longer identifier (`mainColor`) must not
    /// be mistaken for the function definition — the splice point search
    /// is word-boundary-aware in both directions.
    #[test]
    fn main_like_identifiers_do_not_confuse_the_insertion_point_search() {
        let mut total_len = 0usize;
        let mut include_count = 0usize;
        let mut include: Box<IncludeLookup<'static>> =
            Box::new(|_: &str| Some(b"const float X = 1.0;\n".to_vec()));
        let source = "vec3 mainColor() { return vec3(0.0); }\n#include \"a.h\"\nvoid main(){}\n";
        let out =
            resolve_includes(source, &mut include, 0, &mut total_len, &mut include_count).unwrap();
        let color_pos = out.find("mainColor").unwrap();
        let x_pos = out.find("const float X = 1.0;").unwrap();
        let main_pos = out.find("void main(").unwrap();
        assert!(color_pos < x_pos, "did not stop at mainColor: {out}");
        assert!(x_pos < main_pos);
    }

    #[test]
    fn missing_include_is_not_an_error() {
        let mut include = no_includes();
        let mut locs = BTreeMap::new();
        let out = preprocess(
            Stage::Fragment,
            "t.frag",
            "#include \"missing.h\"\nvoid main(){}\n",
            &BTreeMap::new(),
            &[],
            &mut locs,
            &mut include,
        )
        .unwrap();
        assert!(out.source.contains("but was not found"));
    }

    #[test]
    fn include_depth_bounded() {
        let mut include: Box<IncludeLookup<'static>> =
            Box::new(|_: &str| Some(b"#include \"self.h\"\n".to_vec()));
        let mut locs = BTreeMap::new();
        let error = preprocess(
            Stage::Fragment,
            "t.frag",
            "#include \"self.h\"\nvoid main(){}\n",
            &BTreeMap::new(),
            &[],
            &mut locs,
            &mut include,
        )
        .unwrap_err();
        assert_eq!(error, PreprocessError::IncludeDepthExceeded);
    }

    #[test]
    fn oversized_preprocessed_text_is_refused() {
        let big = "a".repeat(MAX_PREPROCESSED_BYTES + 1);
        let mut include: Box<IncludeLookup<'static>> = {
            let big = big.clone();
            Box::new(move |_: &str| Some(big.clone().into_bytes()))
        };
        let mut locs = BTreeMap::new();
        let error = preprocess(
            Stage::Fragment,
            "t.frag",
            "#include \"huge.h\"\nvoid main(){}\n",
            &BTreeMap::new(),
            &[],
            &mut locs,
            &mut include,
        )
        .unwrap_err();
        assert!(matches!(error, PreprocessError::SizeExceeded { .. }));
    }

    #[test]
    fn require_lighting_v1_stub_inserted() {
        let mut include = no_includes();
        let mut locs = BTreeMap::new();
        let out = preprocess(
            Stage::Fragment,
            "t.frag",
            "#require LightingV1\nvoid main(){}\n",
            &BTreeMap::new(),
            &[],
            &mut locs,
            &mut include,
        )
        .unwrap();
        assert!(out.source.contains("PerformLighting_V1"));
    }

    #[test]
    fn unknown_require_is_commented_not_fatal() {
        let mut include = no_includes();
        let mut locs = BTreeMap::new();
        let out = preprocess(
            Stage::Fragment,
            "t.frag",
            "#require SomethingElse\nvoid main(){}\n",
            &BTreeMap::new(),
            &[],
            &mut locs,
            &mut include,
        )
        .unwrap();
        assert!(out.source.contains("unresolved #require SomethingElse"));
    }

    #[test]
    fn sampler_gets_binding_from_texture_index() {
        let mut include = no_includes();
        let mut locs = BTreeMap::new();
        let out = preprocess(
            Stage::Fragment,
            "t.frag",
            "uniform sampler2D g_Texture0; // {}\nvoid main(){}\n",
            &BTreeMap::new(),
            &[],
            &mut locs,
            &mut include,
        )
        .unwrap();
        assert!(
            out.source
                .contains("layout(set = 0, binding = 0) uniform sampler2D g_Texture0;")
        );
        assert_eq!(out.sampler_slots, vec![0]);
    }

    #[test]
    fn texture_index_past_cap_is_an_error() {
        let mut include = no_includes();
        let mut locs = BTreeMap::new();
        let error = preprocess(
            Stage::Fragment,
            "t.frag",
            "uniform sampler2D g_Texture9; // {}\nvoid main(){}\n",
            &BTreeMap::new(),
            &[],
            &mut locs,
            &mut include,
        )
        .unwrap_err();
        assert!(matches!(error, PreprocessError::TooManyTextures { .. }));
    }

    #[test]
    fn varying_shares_location_between_vertex_and_fragment() {
        let mut include = no_includes();
        let mut locs = BTreeMap::new();
        let vert = preprocess(
            Stage::Vertex,
            "t.vert",
            "varying vec2 v_TexCoord;\nvoid main(){}\n",
            &BTreeMap::new(),
            &[],
            &mut locs,
            &mut include,
        )
        .unwrap();
        let frag = preprocess(
            Stage::Fragment,
            "t.frag",
            "varying vec2 v_TexCoord;\nvoid main(){}\n",
            &BTreeMap::new(),
            &[],
            &mut locs,
            &mut include,
        )
        .unwrap();
        assert!(
            vert.source
                .contains("layout(location = 0) varying vec2 v_TexCoord;")
        );
        assert!(
            frag.source
                .contains("layout(location = 0) varying vec2 v_TexCoord;")
        );
    }

    #[test]
    fn attribute_locations_assigned_in_order() {
        let mut include = no_includes();
        let mut locs = BTreeMap::new();
        let out = preprocess(
            Stage::Vertex,
            "t.vert",
            "attribute vec3 a_Position;\nattribute vec2 a_TexCoord;\nvoid main(){}\n",
            &BTreeMap::new(),
            &[],
            &mut locs,
            &mut include,
        )
        .unwrap();
        assert_eq!(
            out.attributes,
            vec![
                AttributeDecl {
                    glsl_type: "vec3".into(),
                    name: "a_Position".into(),
                    location: 0,
                },
                AttributeDecl {
                    glsl_type: "vec2".into(),
                    name: "a_TexCoord".into(),
                    location: 1,
                },
            ]
        );
        assert!(
            out.source
                .contains("layout(location = 0) attribute vec3 a_Position;")
        );
        assert!(
            out.source
                .contains("layout(location = 1) attribute vec2 a_TexCoord;")
        );
    }

    /// S4: the real corpus's `genericimage3.vert`/`genericimage4.vert`
    /// pattern — `#if MORPHING` gates `a_PositionVec4` vs. the `#else`
    /// branch's `a_Position`. Before S4, `fold_declarations` scraped
    /// EVERY `attribute` line textually regardless of `#if` nesting, so
    /// this shader always scraped THREE attributes
    /// (`a_PositionVec4`, `a_Position`, `a_TexCoord`) with `a_TexCoord`
    /// landing at location 2 — `main.rs::material_vertex_format_supported`
    /// then always refused it (attributes[1] was `a_Position`, not
    /// `a_TexCoord`) no matter what the material's actual `combos` said.
    /// With `MORPHING` left at its default (unset — no material combo
    /// override, no `// [COMBO]` default other than 0), only the `#else`
    /// branch is live: exactly `a_Position` (location 0) then
    /// `a_TexCoord` (location 1), matching what `shaderc` actually
    /// compiles.
    #[test]
    fn if_else_gated_attribute_scrapes_only_the_live_branch() {
        let mut include = no_includes();
        let mut locs = BTreeMap::new();
        let out = preprocess(
            Stage::Vertex,
            "t.vert",
            "#if MORPHING\nattribute vec4 a_PositionVec4;\n#else\nattribute vec3 a_Position;\n#endif\nattribute vec2 a_TexCoord;\nvoid main(){}\n",
            &BTreeMap::new(),
            &[],
            &mut locs,
            &mut include,
        )
        .unwrap();
        assert_eq!(
            out.attributes,
            vec![
                AttributeDecl {
                    glsl_type: "vec3".into(),
                    name: "a_Position".into(),
                    location: 0,
                },
                AttributeDecl {
                    glsl_type: "vec2".into(),
                    name: "a_TexCoord".into(),
                    location: 1,
                },
            ]
        );
    }

    /// Same shader shape, but with the material overriding `MORPHING=1`
    /// (`combos` reaching `preprocess` the way `material.json`'s own
    /// `combos` map would) — now the `#if` branch is live instead: only
    /// `a_PositionVec4` (location 0) then `a_TexCoord` (location 1).
    #[test]
    fn if_else_gated_attribute_respects_a_material_combo_override() {
        let mut include = no_includes();
        let mut locs = BTreeMap::new();
        let mut combos = BTreeMap::new();
        combos.insert("MORPHING".to_string(), 1);
        let out = preprocess(
            Stage::Vertex,
            "t.vert",
            "#if MORPHING\nattribute vec4 a_PositionVec4;\n#else\nattribute vec3 a_Position;\n#endif\nattribute vec2 a_TexCoord;\nvoid main(){}\n",
            &combos,
            &[],
            &mut locs,
            &mut include,
        )
        .unwrap();
        assert_eq!(
            out.attributes,
            vec![
                AttributeDecl {
                    glsl_type: "vec4".into(),
                    name: "a_PositionVec4".into(),
                    location: 0,
                },
                AttributeDecl {
                    glsl_type: "vec2".into(),
                    name: "a_TexCoord".into(),
                    location: 1,
                },
            ]
        );
    }

    /// `#ifdef`/`#ifndef` and a compound `||`/`&&`/`==` expression, the
    /// shapes actually used by the local shader corpus
    /// (`LIGHTING || REFLECTION`, `(LIGHTING || REFLECTION) && NORMALMAP
    /// == 0`) — all default off, so every gated attribute here is dead.
    #[test]
    fn ifdef_and_compound_expressions_default_dead() {
        let mut include = no_includes();
        let mut locs = BTreeMap::new();
        let out = preprocess(
            Stage::Vertex,
            "t.vert",
            "attribute vec3 a_Position;\nattribute vec2 a_TexCoord;\n\
             #ifdef SKINNING\nattribute uvec4 a_BlendIndices;\n#endif\n\
             #if LIGHTING || REFLECTION\nattribute vec3 a_Normal;\n#endif\n\
             #if (LIGHTING || REFLECTION) && NORMALMAP == 0\nattribute vec4 a_Color;\n#endif\n\
             void main(){}\n",
            &BTreeMap::new(),
            &[],
            &mut locs,
            &mut include,
        )
        .unwrap();
        assert_eq!(
            out.attributes,
            vec![
                AttributeDecl {
                    glsl_type: "vec3".into(),
                    name: "a_Position".into(),
                    location: 0,
                },
                AttributeDecl {
                    glsl_type: "vec2".into(),
                    name: "a_TexCoord".into(),
                    location: 1,
                },
            ]
        );
    }

    /// The same compound expression, but `LIGHTING=1` this time — the
    /// `a_Normal` branch becomes live (`LIGHTING || REFLECTION` is now
    /// true), while the `NORMALMAP == 0` branch stays dead
    /// (`NORMALMAP` defaults to 0, so `== 0` is actually TRUE — pick a
    /// combo shape that isolates just the `||` behavior instead:
    /// `SKINNING` via `#ifdef`).
    #[test]
    fn ifdef_becomes_live_once_the_material_defines_the_combo() {
        let mut include = no_includes();
        let mut locs = BTreeMap::new();
        let mut combos = BTreeMap::new();
        combos.insert("SKINNING".to_string(), 1);
        let out = preprocess(
            Stage::Vertex,
            "t.vert",
            "attribute vec3 a_Position;\nattribute vec2 a_TexCoord;\n\
             #ifdef SKINNING\nattribute vec3 a_Normal;\n#endif\n\
             void main(){}\n",
            &combos,
            &[],
            &mut locs,
            &mut include,
        )
        .unwrap();
        assert_eq!(
            out.attributes,
            vec![
                AttributeDecl {
                    glsl_type: "vec3".into(),
                    name: "a_Position".into(),
                    location: 0,
                },
                AttributeDecl {
                    glsl_type: "vec2".into(),
                    name: "a_TexCoord".into(),
                    location: 1,
                },
                AttributeDecl {
                    glsl_type: "vec3".into(),
                    name: "a_Normal".into(),
                    location: 2,
                },
            ]
        );
    }

    /// S4a review MUST-FIX #1: a single `#if` line with deeply nested
    /// parens (10k) must return quickly (bounded recursion depth, no
    /// stack overflow) rather than crash the process. Also covers the
    /// same shape for a long chained `!` run.
    #[test]
    fn evaluate_if_expr_bounds_deeply_nested_parens_and_not_chains() {
        let nested = format!("{}1{}", "(".repeat(10_000), ")".repeat(10_000));
        assert_eq!(evaluate_if_expr(&nested, &BTreeMap::new()), None);
        let not_chain = format!("{}1", "!".repeat(10_000));
        assert_eq!(evaluate_if_expr(&not_chain, &BTreeMap::new()), None);
    }

    /// S4a review RECOMMENDED #4: `#if`/`#ifdef` combo-name matching is
    /// case-SENSITIVE, matching `shaderc`'s real preprocessor against the
    /// always-upper-cased `#define` this module emits — a shader that
    /// spells a live combo in any case OTHER than upper-case must NOT
    /// match, even though the combo genuinely exists.
    #[test]
    fn if_and_ifdef_are_case_sensitive() {
        let mut combos = BTreeMap::new();
        combos.insert("SKINNING".to_string(), 1);
        let combos_upper: BTreeMap<String, i64> =
            combos.iter().map(|(k, v)| (k.to_uppercase(), *v)).collect();
        assert_eq!(evaluate_if_expr("SKINNING", &combos_upper), Some(true));
        assert_eq!(evaluate_if_expr("skinning", &combos_upper), Some(false));

        let mut include = no_includes();
        let mut locs = BTreeMap::new();
        let out = preprocess(
            Stage::Vertex,
            "t.vert",
            "attribute vec3 a_Position;\nattribute vec2 a_TexCoord;\n\
             #ifdef skinning\nattribute vec3 a_Normal;\n#endif\nvoid main(){}\n",
            &combos,
            &[],
            &mut locs,
            &mut include,
        )
        .unwrap();
        // Lower-case `#ifdef skinning` does NOT match the upper-cased
        // `#define SKINNING 1` this module emits, so a_Normal stays dead
        // -- exactly what `shaderc`'s real preprocessor would do too.
        assert_eq!(out.attributes.len(), 2);
    }

    /// NIT #6: `AttributeDecl::location`'s doc comment promises callers
    /// must use the field itself, not `Vec` position -- exercise a case
    /// where a LIVE `#ifdef`-gated attribute sits BETWEEN two others,
    /// shifting nothing (all three are live and in source order here,
    /// which is the common case), but pinned together with a dead
    /// preceding branch so the assigned locations are NOT simply
    /// `0, 1, 2` by accident of this test's own structure lining up with
    /// Vec order.
    #[test]
    fn location_field_is_correct_even_when_an_earlier_branch_is_dead() {
        let mut combos = BTreeMap::new();
        combos.insert("VERTEXCOLOR".to_string(), 1);
        let mut include = no_includes();
        let mut locs = BTreeMap::new();
        let out = preprocess(
            Stage::Vertex,
            "t.vert",
            "attribute vec3 a_Position;\n\
             #if MORPHING\nattribute vec4 a_PositionVec4;\n#endif\n\
             #if VERTEXCOLOR\nattribute vec4 a_Color;\n#endif\n\
             attribute vec2 a_TexCoord;\nvoid main(){}\n",
            &combos,
            &[],
            &mut locs,
            &mut include,
        )
        .unwrap();
        // MORPHING (dead) contributes nothing; VERTEXCOLOR (live) sits
        // between a_Position and a_TexCoord in SOURCE order, so
        // a_TexCoord's `location` (2) does NOT equal its Vec index if it
        // were computed from position alone in a differently-ordered
        // scrape -- this test exists to keep that field, not Vec index,
        // as the thing every caller reads.
        assert_eq!(out.attributes.len(), 3);
        assert_eq!(out.attributes[0].name, "a_Position");
        assert_eq!(out.attributes[0].location, 0);
        assert_eq!(out.attributes[1].name, "a_Color");
        assert_eq!(out.attributes[1].location, 1);
        assert_eq!(out.attributes[2].name, "a_TexCoord");
        assert_eq!(out.attributes[2].location, 2);
    }

    /// An unparseable `#if` expression, when its parent scope is live,
    /// must reject the whole material (`PreprocessError::AmbiguousCondition`)
    /// rather than guess a truth value — S4a review MUST-FIX #2
    /// corrected this slice's original "fall back to always live"
    /// behavior, which could silently suppress a genuinely-live sibling
    /// `#else` branch (see
    /// `unparseable_if_expression_with_a_live_else_sibling_is_rejected_not_guessed`).
    #[test]
    fn unparseable_if_expression_falls_back_to_live() {
        assert_eq!(evaluate_if_expr("SOME_FUNC(X)", &BTreeMap::new()), None);
        // S4a review MUST-FIX #2: an unparseable `#if` whose parent scope
        // IS live must reject the whole material (`AmbiguousCondition`),
        // not guess "always live" -- guessing can silently suppress a
        // genuinely-live sibling `#else` branch's declarations (a
        // DIFFERENT, worse failure than refusing this one material; see
        // `unparseable_if_expression_with_a_live_else_sibling_is_rejected_not_guessed`
        // below for the exact shape that motivated this).
        let mut include = no_includes();
        let mut locs = BTreeMap::new();
        let error = preprocess(
            Stage::Vertex,
            "t.vert",
            "attribute vec3 a_Position;\nattribute vec2 a_TexCoord;\n\
             #if SOME_FUNC(X)\nattribute vec3 a_Normal;\n#endif\nvoid main(){}\n",
            &BTreeMap::new(),
            &[],
            &mut locs,
            &mut include,
        )
        .unwrap_err();
        assert!(matches!(error, PreprocessError::AmbiguousCondition(_)));
    }

    /// The exact regression MUST-FIX #2 describes: an unparseable `#if`
    /// condition that is really FALSE (upstream/`shaderc` would take the
    /// `#else` branch) must not let the OLD "guess true" fallback scrape
    /// the `#if` branch's attributes while silently never touching the
    /// `#else` branch's real, different attributes. Before the fix this
    /// scraped `{a_PositionVec4}` only; after the fix it rejects the
    /// whole material instead.
    #[test]
    fn unparseable_if_expression_with_a_live_else_sibling_is_rejected_not_guessed() {
        let mut include = no_includes();
        let mut locs = BTreeMap::new();
        let error = preprocess(
            Stage::Vertex,
            "t.vert",
            "#if SOME_FUNC(X)\nattribute vec4 a_PositionVec4;\n#else\n             attribute vec3 a_Position;\nattribute vec2 a_TexCoord;\n#endif\nvoid main(){}\n",
            &BTreeMap::new(),
            &[],
            &mut locs,
            &mut include,
        )
        .unwrap_err();
        assert!(matches!(error, PreprocessError::AmbiguousCondition(_)));
    }

    /// An unparseable condition BEHIND an already-dead parent branch must
    /// NOT be rejected -- its truth value cannot matter (nothing inside
    /// would be scraped either way), matching the existing "don't even
    /// look inside a dead branch" contract and avoiding refusing a
    /// material over an expression that is genuinely irrelevant to it.
    #[test]
    fn unparseable_if_expression_behind_a_dead_parent_is_not_an_error() {
        let mut include = no_includes();
        let mut locs = BTreeMap::new();
        let out = preprocess(
            Stage::Vertex,
            "t.vert",
            "attribute vec3 a_Position;\nattribute vec2 a_TexCoord;\n\
             #if 0\n#if SOME_FUNC(X)\nattribute vec3 a_Normal;\n#endif\n#endif\nvoid main(){}\n",
            &BTreeMap::new(),
            &[],
            &mut locs,
            &mut include,
        )
        .unwrap();
        assert_eq!(out.attributes.len(), 2);
    }

    /// Regression: `attribute mediump vec2 a_TexCoord;` (the real corpus
    /// shader `puppettexturechannels.vert` — see
    /// `/media/crushinator/steamapps/common/wallpaper_engine/assets/
    /// shaders/puppettexturechannels.vert` — declares its `a_Position`
    /// with no precision qualifier but its own vertex format differs in
    /// a way this renderer does not implement; other corpus shaders DO
    /// carry a bare precision-qualified `a_TexCoord`) must scrape as
    /// `glsl_type: "vec2"`, not `"mediump vec2"` — before this fix, the
    /// qualifier stayed glued to the type, so
    /// `main.rs::material_vertex_format_supported`'s `attributes[1].
    /// glsl_type == "vec2"` string-equality check always failed for a
    /// shader whose second attribute happened to carry ANY precision
    /// qualifier, needlessly falling back to the flat quad for an
    /// otherwise fully-supported vertex shape.
    #[test]
    fn precision_qualifier_is_stripped_from_the_scraped_type() {
        let mut include = no_includes();
        let mut locs = BTreeMap::new();
        let out = preprocess(
            Stage::Vertex,
            "t.vert",
            "attribute vec3 a_Position;\nattribute mediump vec2 a_TexCoord;\nvoid main(){}\n",
            &BTreeMap::new(),
            &[],
            &mut locs,
            &mut include,
        )
        .unwrap();
        assert_eq!(
            out.attributes,
            vec![
                AttributeDecl {
                    glsl_type: "vec3".into(),
                    name: "a_Position".into(),
                    location: 0,
                },
                AttributeDecl {
                    glsl_type: "vec2".into(),
                    name: "a_TexCoord".into(),
                    location: 1,
                },
            ]
        );
        // The folded declaration also drops the qualifier (harmless on
        // desktop GLSL/Vulkan, where precision qualifiers are advisory) —
        // `fold_declarations` builds its `layout(...)` line from the same
        // stripped `glsl_type` `parse_decl` returns.
        assert!(
            out.source
                .contains("layout(location = 1) attribute vec2 a_TexCoord;")
        );
    }

    #[test]
    fn material_constant_folds_into_ubo_slot() {
        let mut include = no_includes();
        let mut locs = BTreeMap::new();
        let constants = vec!["g_Roughness".to_string()];
        let out = preprocess(
            Stage::Fragment,
            "t.frag",
            "uniform float g_Roughness; // {\"material\":\"roughness\",\"default\":0.5}\nvoid main(){}\n",
            &BTreeMap::new(),
            &constants,
            &mut locs,
            &mut include,
        )
        .unwrap();
        assert!(
            out.source
                .contains("#define g_Roughness u_Std.g_MaterialConstants_[0].x")
        );
        assert!(out.unsupported_uniforms.is_empty());
    }

    #[test]
    fn unknown_uniform_gets_zero_default_and_diagnostic() {
        let mut include = no_includes();
        let mut locs = BTreeMap::new();
        let out = preprocess(
            Stage::Fragment,
            "t.frag",
            "uniform vec3 g_SomeUnknownThing;\nvoid main(){}\n",
            &BTreeMap::new(),
            &[],
            &mut locs,
            &mut include,
        )
        .unwrap();
        assert!(
            out.source
                .contains("const vec3 g_SomeUnknownThing = vec3(0.0);")
        );
        assert_eq!(
            out.unsupported_uniforms,
            vec!["g_SomeUnknownThing".to_string()]
        );
    }

    #[test]
    fn render_target_reference_is_flagged() {
        let mut include = no_includes();
        let mut locs = BTreeMap::new();
        let out = preprocess(
            Stage::Fragment,
            "t.frag",
            "uniform sampler2D g_Texture4; // {\"default\":\"_rt_FullFrameBuffer\"}\nvoid main(){}\n",
            &BTreeMap::new(),
            &[],
            &mut locs,
            &mut include,
        )
        .unwrap();
        assert!(out.references_render_target);
    }

    #[test]
    fn no_render_target_reference_when_absent() {
        let mut include = no_includes();
        let mut locs = BTreeMap::new();
        let out = preprocess(
            Stage::Fragment,
            "t.frag",
            "uniform sampler2D g_Texture0; // {\"label\":\"albedo\"}\nvoid main(){}\n",
            &BTreeMap::new(),
            &[],
            &mut locs,
            &mut include,
        )
        .unwrap();
        assert!(!out.references_render_target);
    }

    #[test]
    fn standard_matrix_uniform_folds_and_disappears_as_a_declaration() {
        let mut include = no_includes();
        let mut locs = BTreeMap::new();
        let out = preprocess(
            Stage::Vertex,
            "t.vert",
            "uniform mat4 g_ModelViewProjectionMatrix;\nvoid main(){ gl_Position = g_ModelViewProjectionMatrix * vec4(1.0); }\n",
            &BTreeMap::new(),
            &[],
            &mut locs,
            &mut include,
        )
        .unwrap();
        // No loose `uniform mat4 g_ModelViewProjectionMatrix;` line
        // remains (Vulkan would reject it outside a block) — the name now
        // resolves via the #define.
        assert!(
            !out.source
                .contains("uniform mat4 g_ModelViewProjectionMatrix;")
        );
    }

    /// S4 regression (found via the 60-scene corpus sweep, Workshop
    /// 3100709479's `genericimage3` material): `g_Texture0Rotation`
    /// must fold to the IDENTITY transform `vec4(1.0, 0.0, 0.0, 1.0)`,
    /// not the generic zero-default every other unrecognized uniform
    /// gets — a zero rotation matrix collapses every corpus
    /// `genericimage*.vert`'s `v_TexCoord` computation to a single
    /// point, turning a real, varied texture into one flat colour.
    #[test]
    fn texture_rotation_uniform_folds_to_identity_not_zero() {
        let mut include = no_includes();
        let mut locs = BTreeMap::new();
        let mut unsupported = std::collections::BTreeSet::new();
        for index in 0..MAX_MATERIAL_TEXTURES {
            let out = preprocess(
                Stage::Vertex,
                "t.vert",
                &format!(
                    "uniform vec4 g_Texture{index}Rotation;\nvoid main(){{ gl_Position = g_Texture{index}Rotation; }}\n"
                ),
                &BTreeMap::new(),
                &[],
                &mut locs,
                &mut include,
            )
            .unwrap();
            assert!(
                out.source.contains(&format!(
                    "#define g_Texture{index}Rotation vec4(1.0, 0.0, 0.0, 1.0)"
                )),
                "slot {index}: {}",
                out.source
            );
            // Not counted as an "unsupported uniform" diagnostic — this
            // name IS understood, just always-identity (no scripted
            // per-texture rotation support), unlike a genuinely unknown
            // name.
            unsupported.extend(out.unsupported_uniforms.iter().cloned());
        }
        assert!(unsupported.is_empty(), "{unsupported:?}");
    }

    /// S4b fix: `g_Point<N>` folds to the shared `vec4` UBO slot, but a
    /// shader that declares it as `vec2` (the real corpus shape —
    /// `effects/perspective.vert`) must get a `.xy`-narrowed expression,
    /// not the bare vec4 — passing a vec4 where a function expects vec2
    /// arguments (`squareToQuad(vec2, vec2, vec2, vec2)`) is a genuine
    /// type mismatch, not a cosmetic one (`shaderc` reports "no matching
    /// overloaded function found" for a function that DOES exist).
    #[test]
    fn point_uniform_narrows_to_the_shaders_own_declared_type() {
        let mut include = no_includes();
        let mut locs = BTreeMap::new();
        let out = preprocess(
            Stage::Vertex,
            "t.vert",
            "uniform vec2 g_Point0;\nvoid main(){ gl_Position = vec4(g_Point0, 0.0, 1.0); }\n",
            &BTreeMap::new(),
            &[],
            &mut locs,
            &mut include,
        )
        .unwrap();
        assert!(
            out.source.contains("#define g_Point0 u_Std.g_Point_[0].xy"),
            "{}",
            out.source
        );
        assert!(out.unsupported_uniforms.is_empty());
    }

    /// A `vec4`-declaring shader (the common case, no local corpus example
    /// needing narrowing) gets the bare slot expression — no swizzle
    /// appended, byte-identical to the pre-fix output.
    #[test]
    fn point_uniform_declared_as_vec4_is_unswizzled() {
        let mut include = no_includes();
        let mut locs = BTreeMap::new();
        let out = preprocess(
            Stage::Vertex,
            "t.vert",
            "uniform vec4 g_Point3;\nvoid main(){ gl_Position = g_Point3; }\n",
            &BTreeMap::new(),
            &[],
            &mut locs,
            &mut include,
        )
        .unwrap();
        assert!(
            out.source.contains("#define g_Point3 u_Std.g_Point_[3]\n"),
            "{}",
            out.source
        );
    }

    /// S4b fix: an ARRAY uniform (`uniform float g_AudioSpectrum16Left
    /// [16];`, the real corpus shape — an audio-visualizer material this
    /// renderer does not feed live spectrum data into) previously fell
    /// through to the generic per-line pass-through, leaving a loose
    /// (non-block) `uniform` declaration in the compiled text — Vulkan's
    /// GLSL profile rejects any non-opaque uniform outside a block. It
    /// must now become a zero-initialized, non-uniform array the rest of
    /// the shader can still read by the same name.
    #[test]
    fn array_uniform_zero_fills_instead_of_leaving_a_loose_declaration() {
        let mut include = no_includes();
        let mut locs = BTreeMap::new();
        let out = preprocess(
            Stage::Fragment,
            "t.frag",
            "uniform float g_AudioSpectrum16Left[16];\nvoid main(){ gl_FragColor = vec4(g_AudioSpectrum16Left[0]); }\n",
            &BTreeMap::new(),
            &[],
            &mut locs,
            &mut include,
        )
        .unwrap();
        assert!(
            !out.source.contains("uniform float g_AudioSpectrum16Left"),
            "the loose uniform declaration must not survive: {}",
            out.source
        );
        assert!(
            out.source
                .contains("float g_AudioSpectrum16Left[16] = float[16](float(0.0), float(0.0)"),
            "{}",
            out.source
        );
        assert!(
            out.unsupported_uniforms
                .contains(&"g_AudioSpectrum16Left[16]".to_string())
        );
    }

    /// A hostile/absurd array size (past `MAX_ZERO_ARRAY_LEN`) is left on
    /// the pre-existing pass-through path (still "unsupported", never a
    /// panic or an unbounded initializer-list string) rather than
    /// generating a huge constructor.
    #[test]
    fn array_uniform_past_the_zero_fill_bound_is_left_unsupported_not_expanded() {
        let mut include = no_includes();
        let mut locs = BTreeMap::new();
        let huge = MAX_ZERO_ARRAY_LEN + 1;
        let source = format!(
            "uniform float g_Huge[{huge}];\nvoid main(){{ gl_FragColor = vec4(g_Huge[0]); }}\n"
        );
        let out = preprocess(
            Stage::Fragment,
            "t.frag",
            &source,
            &BTreeMap::new(),
            &[],
            &mut locs,
            &mut include,
        )
        .unwrap();
        // Left exactly as written -- not expanded into a huge literal list.
        assert!(
            out.source
                .contains(&format!("uniform float g_Huge[{huge}];"))
        );
    }

    /// S4b fix: `#define max(x, y) max(y, x)` (upstream's own header,
    /// previously dropped) is what makes `max(0, someVec3)` — the real
    /// corpus shape (`workshop/2423477561/effects/nitro.frag`) — resolve:
    /// the swap puts the scalar in GLSL's required SECOND position, where
    /// its int -> float implicit conversion against `max(genType, float)`
    /// applies. Pinned as a textual assertion on the header (the actual
    /// overload-resolution behavior is exercised end-to-end by
    /// `materialshader`'s device-gated compile tests and the corpus
    /// verification recorded in the S4b change-log).
    #[test]
    fn max_macro_swaps_arguments_matching_upstreams_header() {
        let mut include = no_includes();
        let mut locs = BTreeMap::new();
        let out = preprocess(
            Stage::Fragment,
            "t.frag",
            "void main(){ gl_FragColor = vec4(1.0); }\n",
            &BTreeMap::new(),
            &[],
            &mut locs,
            &mut include,
        )
        .unwrap();
        assert!(out.source.contains("#define max(x, y) max(y, x)"));
    }

    /// `g_Texture<N>Translation` needs no special case: the generic
    /// zero-default (`vec2(0.0, 0.0)`) IS the correct identity for the
    /// same UV formula — pinned so a future change to the zero-default
    /// path can't silently break this pairing without a test noticing.
    #[test]
    fn texture_translation_uniform_keeps_the_generic_zero_default() {
        let mut include = no_includes();
        let mut locs = BTreeMap::new();
        let out = preprocess(
            Stage::Vertex,
            "t.vert",
            "uniform vec2 g_Texture0Translation;\nvoid main(){ gl_Position = vec4(g_Texture0Translation, 0.0, 1.0); }\n",
            &BTreeMap::new(),
            &[],
            &mut locs,
            &mut include,
        )
        .unwrap();
        assert!(
            out.source
                .contains("const vec2 g_Texture0Translation = vec2(0.0);")
        );
        assert_eq!(out.unsupported_uniforms, vec!["g_Texture0Translation"]);
    }

    /// S4 regression (found via the 60-scene corpus sweep, Workshop
    /// 3100709479's `genericimage3` material, `VERSION` combo present):
    /// `g_Color4` must fold to the live per-draw brightness/alpha
    /// values, not the generic zero-default — a zero `g_Color4`
    /// multiplies EVERY `VERSION`-tagged genericimage-family material's
    /// sampled colour to fully transparent black.
    #[test]
    fn color4_uniform_folds_to_brightness_alpha_not_zero() {
        let mut include = no_includes();
        let mut locs = BTreeMap::new();
        let out = preprocess(
            Stage::Fragment,
            "t.frag",
            "uniform vec4 g_Color4;\nvoid main(){ gl_FragColor = g_Color4; }\n",
            &BTreeMap::new(),
            &[],
            &mut locs,
            &mut include,
        )
        .unwrap();
        assert!(out.source.contains(
            "#define g_Color4 vec4(u_Std.g_TimeAlphaBrightness_.zzz, u_Std.g_TimeAlphaBrightness_.y)"
        ));
        assert!(!out.unsupported_uniforms.contains(&"g_Color4".to_string()));
    }

    /// S4a review MUST-FIX #3: `g_ModelMatrix`/`g_ViewProjectionMatrix`/
    /// `g_NormalModelMatrix` (+ `Alt` siblings) must fold to identity, not
    /// the generic zero-default -- a zero `g_ModelMatrix`/
    /// `g_ViewProjectionMatrix` collapses a `genericimage*`-family
    /// object's on-screen geometry to a single point once a material
    /// sets `LIGHTING=1` (`worldPos = mul(localPos, g_ModelMatrix)`,
    /// unconditional; `gl_Position = mul(worldPos, g_ViewProjectionMatrix)`
    /// under `LIGHTING`).
    #[test]
    fn model_and_view_projection_matrices_fold_to_identity_not_zero() {
        for name in [
            "g_ModelMatrix",
            "g_AltModelMatrix",
            "g_ViewProjectionMatrix",
            "g_AltViewProjectionMatrix",
        ] {
            let mut include = no_includes();
            let mut locs = BTreeMap::new();
            let out = preprocess(
                Stage::Vertex,
                "t.vert",
                &format!(
                    "uniform mat4 {name};\nvoid main(){{ gl_Position = {name} * vec4(1.0); }}\n"
                ),
                &BTreeMap::new(),
                &[],
                &mut locs,
                &mut include,
            )
            .unwrap();
            assert!(
                out.source.contains(&format!("#define {name} mat4(1.0)")),
                "{name}: {}",
                out.source
            );
            assert!(!out.unsupported_uniforms.contains(&name.to_string()));
        }
        for name in ["g_NormalModelMatrix", "g_AltNormalModelMatrix"] {
            let mut include = no_includes();
            let mut locs = BTreeMap::new();
            let out = preprocess(
                Stage::Vertex,
                "t.vert",
                &format!(
                    "uniform mat3 {name};\nvoid main(){{ gl_Position = vec4({name} * vec3(1.0), 1.0); }}\n"
                ),
                &BTreeMap::new(),
                &[],
                &mut locs,
                &mut include,
            )
            .unwrap();
            assert!(
                out.source.contains(&format!("#define {name} mat3(1.0)")),
                "{name}: {}",
                out.source
            );
            assert!(!out.unsupported_uniforms.contains(&name.to_string()));
        }
    }
}
