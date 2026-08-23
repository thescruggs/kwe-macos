// SPDX-License-Identifier: GPL-3.0-or-later
//! S2: GLSL -> SPIR-V compilation for the material-shader pipeline, and
//! the `MaterialUniforms` UBO layout `shaderpre::MATERIAL_UBO_BLOCK`'s
//! GLSL declares (the two must stay byte-for-byte in sync — see
//! `material_uniforms_size_matches_std140_layout` below).
//!
//! Uses the `shaderc` crate against the SYSTEM `libshaderc` (see
//! `Cargo.toml`'s comment and `packaging/PKGBUILD`) — no vendored
//! glslang/spirv-tools build.

use std::sync::OnceLock;

/// `g_Texture0..g_Texture7` plus the shader text itself: one preprocessed
/// stage over this size is refused before it ever reaches `shaderc` — no
/// real corpus shader (plus its includes) approaches it; this exists to
/// bound a crafted/hostile one.
pub const MAX_SHADER_TEXT_BYTES: usize = 256 * 1024;

/// Distinct (vertex, fragment, combos, blend) pipelines one scene may
/// compile. A scene with more distinct materials than this keeps using
/// the S1 quad path for the overflow (a one-time diagnostic, not a
/// per-material failure) rather than let a hostile/degenerate scene force
/// unbounded pipeline compilation.
pub const MAX_PIPELINES_PER_SCENE: usize = 64;

/// The Rust mirror of `shaderpre::MATERIAL_UBO_BLOCK`'s `std140` GLSL
/// struct — field order, count, and type must match exactly (every field
/// here is a 16-byte-aligned `vec4`/`mat4`, which is also naturally
/// std140-compatible with no implicit padding, so a field-for-field Rust
/// mirror is exact). Uploaded as one write to a per-layer host-visible
/// uniform buffer (vulkan.rs).
#[repr(C)]
#[derive(Debug, Clone, Copy)]
pub struct MaterialUniforms {
    pub mvp: [f32; 16],
    pub effect_texture_projection: [f32; 16],
    /// `g_Texture<N>Resolution`: xy = pixel resolution, zw = texel size
    /// (1/width, 1/height) — WE's own convention for this uniform.
    pub texture_resolution: [[f32; 4]; 8],
    pub points: [[f32; 4]; 8],
    pub color: [f32; 4],
    /// xy = `g_ParallaxPosition`, zw = `g_PointerPosition`. Not wired to
    /// live input this slice (no parallax/pointer state reaches the scene
    /// worker yet) — always zero, which is the same "unmodeled input
    /// defaults to inert" choice the task brief calls for.
    pub parallax_pointer: [f32; 4],
    /// x = `g_Time`, y = `g_UserAlpha`, z = `g_Brightness`, w = padding.
    pub time_alpha_brightness: [f32; 4],
    pub material_constants: [[f32; 4]; 16],
}

pub const MATERIAL_UNIFORMS_SIZE: usize = std::mem::size_of::<MaterialUniforms>();

impl Default for MaterialUniforms {
    fn default() -> Self {
        Self {
            mvp: IDENTITY_MAT4,
            // g_EffectTextureProjectionMatrix: unlike `mvp` (overwritten
            // by `bind_material_layer` and every `render()` draw) and
            // `parallax_pointer`/`points` (explicitly documented below as
            // "unmodeled input defaults to inert"), this field is never
            // written anywhere after `Default` -- effect passes are S3
            // scope, so there is no scene input to thread into it yet.
            // Stays a fixed identity for the same "unmodeled input
            // defaults to inert" reason, just not previously called out.
            effect_texture_projection: IDENTITY_MAT4,
            texture_resolution: [[0.0; 4]; 8],
            points: [[0.0; 4]; 8],
            color: [1.0, 1.0, 1.0, 1.0],
            parallax_pointer: [0.0; 4],
            time_alpha_brightness: [0.0, 1.0, 1.0, 0.0],
            material_constants: [[0.0; 4]; 16],
        }
    }
}

const IDENTITY_MAT4: [f32; 16] = [
    1.0, 0.0, 0.0, 0.0, 0.0, 1.0, 0.0, 0.0, 0.0, 0.0, 1.0, 0.0, 0.0, 0.0, 0.0, 1.0,
];

/// Build `g_ModelViewProjectionMatrix` (column-major, matching GLSL's
/// `mat4 * vec4` = `mul(v, M)` under the header shim's
/// `#define mul(x, y) ((y) * (x))`) directly from the same 2D affine
/// transform `layers::layer_model` already computes and the existing S1
/// quad pipeline already applies via push constants
/// (`world = m·local + t`, `ndc = world * 2 / viewport`, no y-flip) — so
/// a material draw and the S1 quad draw of the same layer land on
/// exactly the same pixels. z stays 0 (no depth test) and w stays 1.
#[must_use]
pub fn build_orthographic_mvp(
    m: [[f32; 2]; 2],
    t: [f32; 2],
    world_width: f32,
    world_height: f32,
) -> [f32; 16] {
    let sx = 2.0 / world_width.max(1.0);
    let sy = 2.0 / world_height.max(1.0);
    // Column-major: columns are [col0, col1, col2, col3], each a vec4.
    [
        m[0][0] * sx,
        m[1][0] * sy,
        0.0,
        0.0, // column 0
        m[0][1] * sx,
        m[1][1] * sy,
        0.0,
        0.0, // column 1
        0.0,
        0.0,
        0.0,
        0.0, // column 2
        t[0] * sx,
        t[1] * sy,
        0.0,
        1.0, // column 3
    ]
}

#[derive(Debug)]
pub enum CompileError {
    Unavailable,
    Failed(String),
    TooLarge { bytes: usize, limit: usize },
}

impl std::fmt::Display for CompileError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Unavailable => write!(f, "shaderc compiler unavailable"),
            Self::Failed(detail) => write!(f, "shader compile failed: {detail}"),
            Self::TooLarge { bytes, limit } => {
                write!(f, "shader text {bytes} bytes exceeds the {limit} byte cap")
            }
        }
    }
}

fn compiler() -> Option<&'static shaderc::Compiler> {
    static COMPILER: OnceLock<Option<shaderc::Compiler>> = OnceLock::new();
    COMPILER
        .get_or_init(|| shaderc::Compiler::new().ok())
        .as_ref()
}

/// S2 review #3 (RECOMMENDED): the shader text handed to `compile_stage`/
/// `references_live_render_target` is fully attacker-controlled (a
/// Workshop `.vert`/`.frag`), and neither `shaderc::Compiler::
/// compile_into_spirv` nor `preprocess` carries an internal timeout.
/// Running the call on a helper thread and bounding how long THIS
/// function waits for it turns "the worker's scene-load path blocks
/// indefinitely on a pathological shader" into "one material falls back
/// after `MATERIAL_COMPILE_TIMEOUT`" — a hard, sound bound on the calling
/// code path regardless of what glslang/SPIRV-Tools do internally
/// (unlike `OptimizationLevel::Zero` alone — used below too, since this
/// pipeline's shaders are already small and simple so optimization buys
/// little — which only reduces EXPECTED compile time, not a guarantee).
/// The daemon's own `--renderer-scene-startup-timeout-ms`/frame-timeout
/// supervisor remains an external backstop, but is not relied on here: a
/// supervisor kill would count as a `ProcessExit`/timeout strike per S2
/// material compiled, not the clean one-material-fallback this timeout
/// gives instead. The spawned thread is not joined on timeout (`shaderc`
/// has no cancellation API) — it is left to finish or be reaped at
/// process exit; this leaks at most one thread per timed-out compile,
/// bounded by `MAX_PIPELINES_PER_SCENE` compile attempts per scene.
const MATERIAL_COMPILE_TIMEOUT: std::time::Duration = std::time::Duration::from_secs(5);

fn with_timeout<T, F>(f: F) -> Option<T>
where
    T: Send + 'static,
    F: FnOnce() -> T + Send + 'static,
{
    let (tx, rx) = std::sync::mpsc::channel();
    std::thread::spawn(move || {
        let _ = tx.send(f());
    });
    rx.recv_timeout(MATERIAL_COMPILE_TIMEOUT).ok()
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Stage {
    Vertex,
    Fragment,
}

/// Compile one preprocessed GLSL stage to SPIR-V for Vulkan 1.2. Bounded:
/// refuses text over `MAX_SHADER_TEXT_BYTES` before calling into
/// `shaderc`; runs the actual compile on a helper thread with a
/// `MATERIAL_COMPILE_TIMEOUT` bound (see `with_timeout`'s doc comment); a
/// `shaderc` failure (bad GLSL, unsupported construct) or a timeout comes
/// back as `CompileError::Failed`, a value the caller (main.rs) treats
/// exactly like any other material fallback trigger — never a panic.
pub fn compile_stage(source: &str, stage: Stage, label: &str) -> Result<Vec<u32>, CompileError> {
    if source.len() > MAX_SHADER_TEXT_BYTES {
        return Err(CompileError::TooLarge {
            bytes: source.len(),
            limit: MAX_SHADER_TEXT_BYTES,
        });
    }
    if compiler().is_none() {
        return Err(CompileError::Unavailable);
    }
    let source = source.to_string();
    let label = label.to_string();
    let outcome = with_timeout(move || -> Result<Vec<u32>, String> {
        let compiler = compiler().ok_or_else(|| "compiler unavailable".to_string())?;
        let mut options = shaderc::CompileOptions::new()
            .map_err(|_| "compiler options unavailable".to_string())?;
        options.set_target_env(
            shaderc::TargetEnv::Vulkan,
            shaderc::EnvVersion::Vulkan1_2 as u32,
        );
        options.set_optimization_level(shaderc::OptimizationLevel::Zero);
        let shader_kind = match stage {
            Stage::Vertex => shaderc::ShaderKind::Vertex,
            Stage::Fragment => shaderc::ShaderKind::Fragment,
        };
        compiler
            .compile_into_spirv(&source, shader_kind, &label, "main", Some(&options))
            .map(|artifact| artifact.as_binary().to_vec())
            .map_err(|error| error.to_string())
    });
    match outcome {
        Some(Ok(spirv)) => Ok(spirv),
        Some(Err(detail)) => Err(CompileError::Failed(detail)),
        None => Err(CompileError::Failed(format!(
            "compile exceeded the {MATERIAL_COMPILE_TIMEOUT:?} bound"
        ))),
    }
}

/// `shaderpre::PreprocessOutput::references_render_target` is a
/// conservative, purely textual check — it flags a `_rt_` mention
/// anywhere, including inside a shader's own dead-combo branches (e.g.
/// `genericimage2.frag`'s `#if BLENDMODE` block, which every material
/// using default combos never reaches). Refusing every such material
/// would defeat this slice's point: `genericimage2` is the corpus's most
/// common material shader and always carries a combo-gated `_rt_`
/// texture default that is normally unreachable. This function asks
/// `shaderc`'s own C-preprocessor (which actually evaluates `#if`/`#ifdef`
/// against the combo `#define`s already baked into `source`) whether
/// `_rt_` survives into the LIVE text, which is the precise answer: a
/// dead branch is stripped before this check ever sees it. Fails closed
/// (returns `true`, i.e. "treat as referencing a render target" ->
/// fallback) if the compiler is unavailable, the preprocess call itself
/// errors, or the `MATERIAL_COMPILE_TIMEOUT` bound (`with_timeout`,
/// shared with `compile_stage`) elapses — the same shader is about to be
/// handed to `compile_stage` anyway, so any failure here just means the
/// caller will see the same failure moments later via
/// `CompileError::Failed`.
pub fn references_live_render_target(source: &str, stage: Stage, label: &str) -> bool {
    if compiler().is_none() {
        return true;
    }
    // The stage argument only selects a default `#define` shaderc adds
    // internally for old-style `GL_es_profile` detection, irrelevant
    // here; kept for API symmetry with `compile_stage`.
    let _ = stage;
    let source = source.to_string();
    let label = label.to_string();
    let outcome = with_timeout(move || -> bool {
        let Some(compiler) = compiler() else {
            return true;
        };
        let Ok(mut options) = shaderc::CompileOptions::new() else {
            return true;
        };
        options.set_target_env(
            shaderc::TargetEnv::Vulkan,
            shaderc::EnvVersion::Vulkan1_2 as u32,
        );
        match compiler.preprocess(&source, &label, "main", Some(&options)) {
            Ok(artifact) => artifact.as_text().contains("_rt_"),
            Err(_) => true,
        }
    });
    outcome.unwrap_or(true)
}

/// Identifies one cached material pipeline: the shader base name plus its
/// resolved combo set plus the blend variant — everything that changes
/// what SPIR-V gets compiled or which fixed-function blend state a
/// pipeline needs. Two layers with the same material and the same
/// resolved combos share one compiled pipeline (vulkan.rs's cache is
/// keyed by this).
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub struct MaterialKey(pub u64);

impl MaterialKey {
    #[must_use]
    pub fn compute(
        shader_name: &str,
        combos: &std::collections::BTreeMap<String, i64>,
        blend_variant: usize,
    ) -> Self {
        use std::hash::{Hash, Hasher};
        let mut hasher = std::collections::hash_map::DefaultHasher::new();
        shader_name.hash(&mut hasher);
        for (name, value) in combos {
            name.hash(&mut hasher);
            value.hash(&mut hasher);
        }
        blend_variant.hash(&mut hasher);
        Self(hasher.finish())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn material_uniforms_size_matches_std140_layout() {
        // mat4 x2 (64B each) + vec4[8] x2 (128B each) + vec4 x3 (color,
        // parallax_pointer, time_alpha_brightness = 16B each) +
        // vec4[16] (256B) = 128 + 256 + 48 + 256 = 688... plus the two
        // mat4 = 128 -> 816? Compute explicitly instead of hand-deriving
        // twice; this test is the single source of truth against
        // shaderpre::MATERIAL_UBO_BLOCK's field list.
        let expected = 16 * 4 // mvp
            + 16 * 4 // effect_texture_projection
            + 8 * 4 * 4 // texture_resolution[8]
            + 8 * 4 * 4 // points[8]
            + 4 * 4 // color
            + 4 * 4 // parallax_pointer
            + 4 * 4 // time_alpha_brightness
            + 16 * 4 * 4; // material_constants[16]
        assert_eq!(MATERIAL_UNIFORMS_SIZE, expected);
        // Every field boundary must land on a 16-byte multiple (std140
        // vec4/mat4 alignment) given the struct is `#[repr(C)]` with
        // fields declared in the same order as the GLSL block.
        assert_eq!(MATERIAL_UNIFORMS_SIZE % 16, 0);
    }

    #[test]
    fn identity_mvp_at_full_scene_extent_is_identity_like() {
        // A layer with no rotation/scale/translation, at a world extent
        // matching a 2x2 unit box, should map local (-0.5..0.5) straight
        // to NDC (-1..1) with no shear — i.e. sx = sy = 1.
        let m = build_orthographic_mvp([[1.0, 0.0], [0.0, 1.0]], [0.0, 0.0], 2.0, 2.0);
        assert_eq!(m[0], 1.0);
        assert_eq!(m[5], 1.0);
        assert_eq!(m[12], 0.0);
        assert_eq!(m[13], 0.0);
        assert_eq!(m[15], 1.0);
    }

    #[test]
    fn translation_lands_in_the_last_column() {
        let world_w = 100.0;
        let world_h = 50.0;
        let m = build_orthographic_mvp([[1.0, 0.0], [0.0, 1.0]], [10.0, -5.0], world_w, world_h);
        assert!((m[12] - 10.0 * 2.0 / world_w).abs() < 1e-6);
        assert!((m[13] - (-5.0) * 2.0 / world_h).abs() < 1e-6);
    }

    #[test]
    fn material_key_is_stable_and_combo_sensitive() {
        let mut combos_a = std::collections::BTreeMap::new();
        combos_a.insert("LIGHTING".to_string(), 0);
        let mut combos_b = std::collections::BTreeMap::new();
        combos_b.insert("LIGHTING".to_string(), 1);
        let key_a1 = MaterialKey::compute("genericimage2", &combos_a, 0);
        let key_a2 = MaterialKey::compute("genericimage2", &combos_a, 0);
        let key_b = MaterialKey::compute("genericimage2", &combos_b, 0);
        assert_eq!(key_a1, key_a2);
        assert_ne!(key_a1, key_b);
    }

    #[test]
    fn compile_stage_over_size_cap_is_refused_without_calling_shaderc() {
        let huge = "a".repeat(MAX_SHADER_TEXT_BYTES + 1);
        let error = compile_stage(&huge, Stage::Fragment, "huge.frag").unwrap_err();
        assert!(matches!(error, CompileError::TooLarge { .. }));
    }

    /// Round-trip: preprocess a tiny synthetic fragment shader through
    /// `shaderpre` and compile it. Skips gracefully (does not fail the
    /// suite) when the system `libshaderc` is not present, matching the
    /// task's "skips gracefully when libshaderc is missing" requirement —
    /// checked at runtime (via `shaderc::Compiler::new()`) rather than a
    /// compile-time `cfg`, since whether the system library is installed
    /// is not something `cfg` can see.
    #[test]
    fn compile_round_trip_produces_spirv() {
        if compiler().is_none() {
            eprintln!("skipping compile_round_trip_produces_spirv: libshaderc unavailable");
            return;
        }
        let mut locs = std::collections::BTreeMap::new();
        let mut include: Box<crate::shaderpre::IncludeLookup<'static>> = Box::new(|_: &str| None);
        let source = "void main() { gl_FragColor = vec4(1.0, 0.0, 0.0, 1.0); }\n";
        let preprocessed = crate::shaderpre::preprocess(
            crate::shaderpre::Stage::Fragment,
            "round_trip.frag",
            source,
            &std::collections::BTreeMap::new(),
            &[],
            &mut locs,
            &mut include,
        )
        .unwrap();
        let spirv = compile_stage(&preprocessed.source, Stage::Fragment, "round_trip.frag")
            .expect("compiles");
        assert!(!spirv.is_empty());
    }

    /// A `_rt_`-default sampler behind a combo that is off by default
    /// (`#if BLENDMODE`, never `#define`d) must not survive shaderc's own
    /// preprocessing — the exact case that made the static
    /// `references_render_target` check too conservative for
    /// `genericimage2` (the corpus's most common material shader) during
    /// development.
    #[test]
    fn dead_branch_render_target_reference_does_not_survive_live_preprocess() {
        if compiler().is_none() {
            eprintln!(
                "skipping dead_branch_render_target_reference_does_not_survive_live_preprocess: libshaderc unavailable"
            );
            return;
        }
        let mut locs = std::collections::BTreeMap::new();
        let mut include: Box<crate::shaderpre::IncludeLookup<'static>> = Box::new(|_: &str| None);
        let source = "#if BLENDMODE\nuniform sampler2D g_Texture4; // {\"hidden\":true,\"default\":\"_rt_FullFrameBuffer\"}\n#endif\nvoid main() { gl_FragColor = vec4(1.0); }\n";
        let preprocessed = crate::shaderpre::preprocess(
            crate::shaderpre::Stage::Fragment,
            "dead_rt.frag",
            source,
            &std::collections::BTreeMap::new(),
            &[],
            &mut locs,
            &mut include,
        )
        .unwrap();
        assert!(
            preprocessed.references_render_target,
            "static check is conservative"
        );
        assert!(!references_live_render_target(
            &preprocessed.source,
            Stage::Fragment,
            "dead_rt.frag"
        ));
    }
}
