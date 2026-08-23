// SPDX-License-Identifier: GPL-3.0-or-later
// Layer runtime model for the M3c slice of the original SceneScript engine.
//
// One layer is one `objects` entry with an image reference, in scene.json
// order (the compositor's draw order). The script sees each layer through
// the Scene.getLayer proxy (js.rs); the worker reads the states every frame
// and builds the draw list. All property writes are clamped here, mirroring
// how Engine.clearcolor clamps — the script can never push a non-finite or
// unbounded value into the renderer.
//
// The transform math follows the researched wallpaper-engine semantics
// (docs/SCENE_FORMAT_V1.md, M3c section): a layer is a rectangle of
// `size` scene units anchored to its `origin` (position vector, (0,0) =
// scene center, +y down) with its center — WE alignment "center", the
// default. Rotation and scale happen about the origin, in that order:
// world = R(θ)·S(scale)·diag(size)·pos + origin, pos ∈ [-0.5, 0.5]². The
// renderer pushes the matrix; this module computes it.

use std::cell::RefCell;
use std::rc::Rc;

use crate::scene::LayerSpec;
use crate::text::{HorizontalAlign, VerticalAlign};

/// Cap on registered image layers (scene.rs rejects beyond this at parse;
/// the brief's bound — layers up to 256 are bound).
pub const MAX_LAYERS: usize = 256;

/// Bound on every scalar the script can write (mirror of the vector bounds
/// enforced at parse): non-finite values clamp to 0, magnitudes to ±1e6.
pub const MAX_LAYER_VALUE: f64 = 1e6;

/// The WE `colorBlendMode` values this engine renders, per the researched
/// mapping (docs/SCENE_FORMAT_V1.md, M3d section). Wallpaper Engine does not
/// publish an integer table: the value selects a shader combo ("imageblending"
/// type with `ui_editor_properties_blend_mode`) whose behavior WE implements
/// in its proprietary ApplyBlending; the editor dropdown exposes exactly the
/// five modes here (verified on the WE editor localization dump), and the
/// 2017 Steam patch note describes them as "the standard Photoshop blend
/// modes". 0 = Normal is verified from the editor default and the corpus
/// histogram (410 of 432 carriers use 0); the pairs 1=Multiply, 6=Add,
/// 7=Screen, 9=Subtract are decoded from that five-mode set against the
/// corpus histogram values (11/30/6/24/1/7/9/12) — the exact integers are
/// recorded as decoded, not independently verifiable from public sources.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum BlendMode {
    /// src-over (the M3c default; WE 0).
    Normal,
    /// Multiply: texel × background (WE 1).
    Multiply,
    /// Add: texel + background, saturating (WE 6).
    Add,
    /// Screen: 255-(255-texel)(255-background)/255 (WE 7).
    Screen,
    /// Subtract: max(0, background − texel), Photoshop's base−blend (WE 9).
    Subtract,
}

impl BlendMode {
    /// The implemented set, in variant-index order (the renderer's pipeline
    /// table is indexed by this order, so it must stay stable).
    pub const ALL: [BlendMode; 5] = [
        BlendMode::Normal,
        BlendMode::Multiply,
        BlendMode::Add,
        BlendMode::Screen,
        BlendMode::Subtract,
    ];

    /// The WE `colorBlendMode` integer (0/1/6/7/9).
    pub fn as_u32(self) -> u32 {
        match self {
            BlendMode::Normal => 0,
            BlendMode::Multiply => 1,
            BlendMode::Add => 6,
            BlendMode::Screen => 7,
            BlendMode::Subtract => 9,
        }
    }

    /// `Some` for the implemented WE values, `None` for everything else
    /// (including the known-unimplemented corpus values 11/12/24/30).
    pub fn from_u32(value: u32) -> Option<BlendMode> {
        match value {
            0 => Some(BlendMode::Normal),
            1 => Some(BlendMode::Multiply),
            6 => Some(BlendMode::Add),
            7 => Some(BlendMode::Screen),
            9 => Some(BlendMode::Subtract),
            _ => None,
        }
    }

    /// Clamp any WE value to the implemented set: unimplemented values
    /// (known or unknown) fall back to src-over. The caller decides whether
    /// the fallback is worth a bounded diagnostic.
    pub fn clamp(value: u32) -> BlendMode {
        BlendMode::from_u32(value).unwrap_or(BlendMode::Normal)
    }

    /// Index into [`BlendMode::ALL`] — the renderer's pipeline-variant
    /// table. Implemented values only: clamp before calling.
    pub fn variant_index(self) -> usize {
        match self {
            BlendMode::Normal => 0,
            BlendMode::Multiply => 1,
            BlendMode::Add => 2,
            BlendMode::Screen => 3,
            BlendMode::Subtract => 4,
        }
    }
}

/// Known-unimplemented corpus `colorBlendMode` values: non-fixed-function WE
/// modes (undecoded — no public mapping exists) that clamp to Normal with a
/// bounded one-time diagnostic. Corpus histogram: 6×11, 6×30, 2×24, 1×12.
/// Unknown values outside this set are tolerated silently (the M3c parse
/// behavior), still rendering src-over.
pub const BLEND_MODE_UNIMPLEMENTED: [u32; 4] = [11, 12, 24, 30];

/// Upper bound on `brightness`. WE's default is 1.0 — the identity, pinned
/// by the oracles; the clamp range 0..=10 is a design decision (dimming to
/// black and up to a 10x boost), not a documented WE bound.
pub const MAX_LAYER_BRIGHTNESS: f32 = 10.0;

/// The per-layer runtime state the script sees and the compositor draws.
/// Scripts mutate it through the Scene.getLayer proxies (js.rs); the worker
/// borrows it per frame to build the draw list. Everything here is a plain
/// value — all validation happens at the proxy boundary.
#[derive(Debug, Clone)]
pub struct LayerState {
    pub name: String,
    /// Position in the scene.json `objects` array — the global painter's
    /// order across kinds: merged_draws sorts the layer and particle draw
    /// lists by it, so an image that appears after a particle system in
    /// the file draws on top of it.
    pub scene_order: usize,
    /// Straight alpha in 0..=1 (WE default 1.0).
    pub alpha: f32,
    pub visible: bool,
    /// Euler angles in degrees (the WE script API unit). The file stores
    /// radians — the parse converts. Only z rotates 2D layers in M3c.
    pub angles: [f32; 3],
    /// Position in scene units (pixels); (0,0) is the scene center, +y
    /// down, per the researched WE origin semantics.
    pub origin: [f32; 2],
    /// Relative scale; 1.0 = original size (WE semantics). Negative values
    /// mirror; nothing else restricts them.
    pub scale: [f32; 2],
    /// Size in scene units (pixels) the texture is drawn at. [0, 0] (the
    /// parse default) is replaced by the decoded texture dimensions at
    /// load, so init() always sees the real size.
    pub size: [f32; 2],
    /// WE `colorBlendMode` clamped to the implemented set at every boundary
    /// (parse, script write) — the renderer's per-draw pipeline variant.
    pub blend_mode: BlendMode,
    /// Brightness multiplier on the sampled RGB (M3d): clamped 0..=10 (the
    /// design range — WE's default 1.0 is the identity), non-finite → 1.0.
    pub brightness: f32,
    /// Tint multiplier on the sampled RGBA (M3d): 0..=1 per component,
    /// default [1, 1, 1, 1]. For text layers the draw's tint comes from
    /// the text state's color instead (frame_draws).
    pub tint: [f32; 4],
    /// M3e text content, `Some` exactly for text layers. The worker's text
    /// renderer (text.rs) turns a dirty state into an atlas texture + quad
    /// vertex data; frame_draws skips text layers with no vertex data.
    pub text: Option<TextState>,
}

/// M3e text-layer runtime state. `dirty` is set by every script write
/// (js.rs) and at load; the worker rebuilds the layout, atlas, and vertex
/// data when it is set and clears it. `vertex_count` is written by the
/// worker after a rebuild (quads built) and read by frame_draws — 0 means
/// nothing to draw yet (empty text, missing font, or pre-sync).
#[derive(Debug, Clone)]
pub struct TextState {
    /// The string to render (capped at text::MAX_TEXT_CHARS chars by the
    /// worker, with a one-time diagnostic).
    pub text: String,
    /// Requested font family (`systemfont_` alias / path accepted; None =
    /// the resolver's default).
    pub font: Option<String>,
    /// Effective pixel em size, clamped to text::MIN_FONT_PX..=MAX_FONT_PX.
    pub pointsize_px: f32,
    pub horizontal_align: HorizontalAlign,
    pub vertical_align: VerticalAlign,
    /// RGBA multiplier, 0..=1 each; drives the draw's tint slot, the
    /// alpha folded into the pushed layer alpha (M3d alpha policy).
    pub color: [f32; 4],
    pub dirty: bool,
    pub vertex_count: u32,
}

impl LayerState {
    pub fn from_spec(spec: &LayerSpec) -> Self {
        Self {
            name: spec.name.clone(),
            scene_order: spec.scene_order,
            alpha: spec.alpha,
            visible: spec.visible,
            angles: spec.angles,
            origin: spec.origin,
            scale: spec.scale,
            // Text layers render at their automatic layout size: pinned to
            // (1, 1) so the model maps layout pixels 1:1 to scene units
            // (text.rs layouts in pixel units, y down — same space). A
            // scene-written `size` on a text layer is ignored (counted for
            // a one-time diagnostic); scaling happens through `scale` like
            // every other layer.
            size: if spec.text.is_some() {
                [1.0, 1.0]
            } else {
                spec.size
            },
            // The spec's raw value may be unimplemented (11/12/24/30 or an
            // unknown); the worker noted it once per scene at load.
            blend_mode: BlendMode::clamp(spec.blend_mode),
            brightness: spec.brightness,
            tint: spec.tint,
            text: spec.text.as_ref().map(|spec| TextState {
                text: spec.text.clone(),
                font: spec.font.clone(),
                pointsize_px: spec.pointsize,
                horizontal_align: spec.horizontal_align,
                vertical_align: spec.vertical_align,
                color: spec.color,
                dirty: true,
                vertex_count: 0,
            }),
        }
    }
}

/// Clamp a scalar the script wrote, like Engine.clearcolor clamps: a
/// non-finite value becomes 0, a finite one is bounded to ±1e6.
pub fn clamp_layer_scalar(value: f64) -> f32 {
    if value.is_finite() {
        (value as f32).clamp(-MAX_LAYER_VALUE as f32, MAX_LAYER_VALUE as f32)
    } else {
        0.0
    }
}

/// Clamp an alpha the script wrote: non-finite → 0, otherwise 0..=1.
pub fn clamp_layer_alpha(value: f64) -> f32 {
    if value.is_finite() {
        (value as f32).clamp(0.0, 1.0)
    } else {
        0.0
    }
}

/// Clamp a size component the script wrote: sizes are never negative
/// (a mirrored layer mirrors through scale); non-finite → 0.
pub fn clamp_layer_size(value: f64) -> f32 {
    if value.is_finite() {
        (value as f32).clamp(0.0, MAX_LAYER_VALUE as f32)
    } else {
        0.0
    }
}

/// Clamp a brightness the script wrote (M3d): non-finite → 1.0 (the
/// identity — a NaN brightness must not silently blacken the layer),
/// otherwise 0..=10.
pub fn clamp_layer_brightness(value: f64) -> f32 {
    if value.is_finite() {
        (value as f32).clamp(0.0, MAX_LAYER_BRIGHTNESS)
    } else {
        1.0
    }
}

/// Clamp one tint component the script wrote (M3d): non-finite → 1.0 (the
/// identity), otherwise 0..=1.
pub fn clamp_layer_tint(value: f64) -> f32 {
    if value.is_finite() {
        (value as f32).clamp(0.0, 1.0)
    } else {
        1.0
    }
}

/// What a draw command renders (M3e, M3f). Image draws use the renderer's
/// shared unit quad; text draws use the layer's per-layer vertex buffer
/// with an explicit vertex count (regenerated on text / alignment /
/// font-size change, never per frame — text.rs). Particle draws (M3f)
/// use the system's own host-visible vertex buffer (6 verts per particle,
/// rebuilt by the worker every fixed step that moved particles) with the
/// same explicit count; the draw's `layer_index` is the system's texture
/// slot (MAX_LAYERS + system_index — particles.rs), which is also how the
/// renderer finds both the descriptor set and the vertex buffer.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum DrawKind {
    Image,
    Text { vertex_count: u32 },
    Particles { vertex_count: u32 },
}

/// One layer's draw command for one frame. `m` and `t` are the model
/// transform in row-major form: world = m·pos + t for pos ∈ [-0.5, 0.5]²
/// (text layers map their layout-pixel quads 1:1 through size (1, 1)).
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct LayerDraw {
    /// Index into the renderer's texture/descriptor table: the layer's
    /// slot, or MAX_LAYERS + system_index for a particle draw.
    pub layer_index: usize,
    /// The object's position in the scene.json `objects` array — the
    /// painter's order ACROSS kinds. The draw lists are merged by it
    /// (merged_draws), so the compositor draws in the file's object order:
    /// an image after a particle system in the file draws on top of it.
    pub scene_order: usize,
    /// Linear part: [[m00, m01], [m10, m11]] (x = m00·x + m01·y + tx).
    pub m: [[f32; 2]; 2],
    /// Translation in scene units: the layer's origin.
    pub t: [f32; 2],
    /// Straight alpha in 0..=1, pushed into the fragment shader (folded
    /// with the tint alpha in m1.w).
    pub alpha: f32,
    /// The layer's blend-mode pipeline variant (clamped to the implemented
    /// set; the renderer indexes its pipeline table by variant_index).
    pub blend_mode: BlendMode,
    /// RGB brightness multiplier (M3d), pushed into the effects vec.
    pub brightness: f32,
    /// RGBA tint multiplier (M3d): rgb pushed into the effects vec, the
    /// alpha folded into the pushed layer alpha (m1.w = alpha · tint.a).
    /// For text layers this is the text color (frame_draws).
    pub tint: [f32; 4],
    /// What to draw: image (unit quad) or text (per-layer vertex buffer).
    pub kind: DrawKind,
    /// S2: draw through the layer's compiled material pipeline
    /// (vulkan.rs's `material_bindings[layer_index]`) instead of the S1
    /// base-texture quad. Set only for `DrawKind::Image` layers whose
    /// material shader preprocessed, compiled, and bound successfully;
    /// text and particle draws are always `false` (materials are S2 scope
    /// for model/image layers only).
    pub material: bool,
}

/// The model transform for one 2D layer: R(θ)·S(scale)·diag(size), about
/// the layer's origin, in scene units with +y down. Pure — the unit tests
/// assert exact positions for known inputs.
pub fn layer_model(
    angle_degrees: f32,
    scale: [f32; 2],
    size: [f32; 2],
    origin: [f32; 2],
) -> ([[f32; 2]; 2], [f32; 2]) {
    let (sin, cos) = angle_degrees.to_radians().sin_cos();
    let (sx, sy) = (scale[0] * size[0], scale[1] * size[1]);
    ([[cos * sx, -sin * sy], [sin * sx, cos * sy]], origin)
}

/// The per-frame draw list: scene.json object order, skipping invisible
/// layers, layers whose texture failed to load (texture_ok), and text
/// layers with no vertex data yet (empty text, missing font, or not yet
/// synced). Pure — unit-tested; the worker calls it once per render.
/// `material_ok` is index-aligned with `layers` like `texture_ok`: `true`
/// for a layer whose material shader preprocessed, compiled, and bound a
/// pipeline successfully (vulkan.rs `bind_material_layer`) — S2. Only
/// `DrawKind::Image` layers can be `true` here in practice (text/particle
/// layers never attempt material compilation); the check still applies
/// uniformly since a `false`/out-of-range entry is the common case (an
/// empty slice works for every caller that has no materials at all).
pub fn frame_draws(
    layers: &[Rc<RefCell<LayerState>>],
    texture_ok: &[bool],
    material_ok: &[bool],
) -> Vec<LayerDraw> {
    layers
        .iter()
        .enumerate()
        .filter_map(|(layer_index, state)| {
            let state = state.borrow();
            if !state.visible || texture_ok.get(layer_index) != Some(&true) {
                return None;
            }
            // M3e: a text layer draws exactly when its vertex data exists
            // (worker rebuilds it on change). Its tint is the text color.
            let (kind, tint) = match &state.text {
                Some(text) if text.vertex_count > 0 => (
                    DrawKind::Text {
                        vertex_count: text.vertex_count,
                    },
                    text.color,
                ),
                Some(_) => return None,
                None => (DrawKind::Image, state.tint),
            };
            let material = kind == DrawKind::Image && material_ok.get(layer_index) == Some(&true);
            let (m, t) = layer_model(state.angles[2], state.scale, state.size, state.origin);
            Some(LayerDraw {
                layer_index,
                scene_order: state.scene_order,
                m,
                t,
                alpha: state.alpha,
                blend_mode: state.blend_mode,
                brightness: state.brightness,
                tint,
                kind,
                material,
            })
        })
        .collect()
}

/// Merge the per-kind draw lists into one painter-ordered list. Each input
/// is already ascending by `scene_order` (parse_objects pushes every kind
/// in the scene.json `objects` array's order, and both builders skip
/// non-drawable entries in that same pass), so a two-list merge restores
/// the FILE's object order across kinds: an image that appears after a
/// particle system draws on top of it, exactly as the file says. Ties
/// cannot happen (one draw per object); layers win the impossible tie for
/// stability. Pure — unit-tested; main.rs calls it once per render.
pub fn merged_draws(layers: Vec<LayerDraw>, particles: Vec<LayerDraw>) -> Vec<LayerDraw> {
    let mut merged = Vec::with_capacity(layers.len() + particles.len());
    let mut layers = layers.into_iter().peekable();
    let mut particles = particles.into_iter().peekable();
    while layers.peek().is_some() || particles.peek().is_some() {
        let take_layer = match (layers.peek(), particles.peek()) {
            (Some(layer), Some(particle)) => layer.scene_order <= particle.scene_order,
            (Some(_), None) => true,
            (None, _) => false,
        };
        if take_layer {
            merged.push(layers.next().expect("peeked Some"));
        } else {
            merged.push(particles.next().expect("peeked Some"));
        }
    }
    merged
}

#[cfg(test)]
mod tests {
    use super::*;

    fn state(name: &str, visible: bool) -> Rc<RefCell<LayerState>> {
        Rc::new(RefCell::new(LayerState {
            name: name.into(),
            scene_order: 0,
            alpha: 1.0,
            visible,
            angles: [0.0, 0.0, 0.0],
            origin: [0.0, 0.0],
            scale: [1.0, 1.0],
            size: [10.0, 20.0],
            blend_mode: BlendMode::Normal,
            brightness: 1.0,
            tint: [1.0, 1.0, 1.0, 1.0],
            text: None,
        }))
    }

    fn text_state(
        name: &str,
        text: &str,
        color: [f32; 4],
        vertex_count: u32,
    ) -> Rc<RefCell<LayerState>> {
        Rc::new(RefCell::new(LayerState {
            name: name.into(),
            scene_order: 0,
            alpha: 1.0,
            visible: true,
            angles: [0.0, 0.0, 0.0],
            origin: [0.0, 0.0],
            scale: [1.0, 1.0],
            size: [1.0, 1.0], // text layers: automatic size
            blend_mode: BlendMode::Normal,
            brightness: 1.0,
            tint: [1.0, 1.0, 1.0, 1.0],
            text: Some(TextState {
                text: text.into(),
                font: None,
                pointsize_px: 48.0,
                horizontal_align: HorizontalAlign::Center,
                vertical_align: VerticalAlign::Center,
                color,
                dirty: true,
                vertex_count,
            }),
        }))
    }

    #[test]
    fn blend_mode_table_matches_the_researched_we_mapping() {
        // The researched WE colorBlendMode table (docs/SCENE_FORMAT_V1.md,
        // M3d section): 0 = Normal (verified from the editor default and
        // the corpus histogram); 1/6/7/9 decoded from the editor's
        // five-mode set (Normal, Multiply, Add, Screen, Subtract). The
        // variant order is the renderer's pipeline table and must stay
        // stable.
        assert_eq!(
            BlendMode::ALL
                .iter()
                .map(|mode| mode.as_u32())
                .collect::<Vec<_>>(),
            [0, 1, 6, 7, 9]
        );
        for (value, expected) in [
            (0, BlendMode::Normal),
            (1, BlendMode::Multiply),
            (6, BlendMode::Add),
            (7, BlendMode::Screen),
            (9, BlendMode::Subtract),
        ] {
            assert_eq!(BlendMode::from_u32(value), Some(expected));
        }
        // Variant indices: the pipeline table order.
        assert_eq!(BlendMode::Normal.variant_index(), 0);
        assert_eq!(BlendMode::Multiply.variant_index(), 1);
        assert_eq!(BlendMode::Add.variant_index(), 2);
        assert_eq!(BlendMode::Screen.variant_index(), 3);
        assert_eq!(BlendMode::Subtract.variant_index(), 4);
    }

    #[test]
    fn blend_mode_clamp_falls_back_to_normal_for_unknown_values() {
        // The known-unimplemented corpus values (11/12/24/30 — recorded
        // undecoded non-fixed-function modes) and any unknown value clamp
        // to Normal; implemented values pass through.
        for value in [11, 12, 24, 30, 2, 5, 8, 100, u32::MAX] {
            assert_eq!(BlendMode::clamp(value), BlendMode::Normal, "value {value}");
            assert!(BlendMode::from_u32(value).is_none(), "value {value}");
        }
        for value in [0, 1, 6, 7, 9] {
            assert_eq!(BlendMode::clamp(value), BlendMode::from_u32(value).unwrap());
        }
        assert_eq!(BLEND_MODE_UNIMPLEMENTED, [11, 12, 24, 30]);
    }

    #[test]
    fn brightness_and_tint_clamps_are_bounded() {
        // Brightness: non-finite → 1.0 (identity), otherwise 0..=10.
        assert_eq!(clamp_layer_brightness(50.0), 10.0);
        assert_eq!(clamp_layer_brightness(-1.0), 0.0);
        assert_eq!(clamp_layer_brightness(2.5), 2.5);
        assert_eq!(clamp_layer_brightness(f64::NAN), 1.0);
        assert_eq!(clamp_layer_brightness(f64::INFINITY), 1.0);
        assert_eq!(clamp_layer_brightness(f64::NEG_INFINITY), 1.0);
        // Tint: non-finite → 1.0, otherwise 0..=1.
        assert_eq!(clamp_layer_tint(2.0), 1.0);
        assert_eq!(clamp_layer_tint(-1.0), 0.0);
        assert_eq!(clamp_layer_tint(0.5), 0.5);
        assert_eq!(clamp_layer_tint(f64::NAN), 1.0);
        assert_eq!(clamp_layer_tint(f64::INFINITY), 1.0);
        assert_eq!(clamp_layer_tint(f64::NEG_INFINITY), 1.0);
    }

    #[test]
    fn model_identity_angle_is_exact() {
        // At angle 0 the model is exactly diag(scale·size): sin(0) is
        // exactly 0 in f32, so equality is exact, not within an epsilon.
        let (m, t) = layer_model(0.0, [1.0, 1.0], [10.0, 20.0], [100.0, -50.0]);
        assert_eq!(m, [[10.0, 0.0], [0.0, 20.0]]);
        assert_eq!(t, [100.0, -50.0]);
    }

    #[test]
    fn model_quarter_turn_maps_axes_exactly() {
        // 90° swaps the axes. sin(π/2 in f32) is exactly 1.0; cos is
        // -4.37e-8 — the on-axis entries are exact, the off-axis ones are
        // within the f32 cosine's error.
        let (m, t) = layer_model(90.0, [1.0, 1.0], [10.0, 20.0], [0.0, 0.0]);
        assert_eq!(m[0][1], -20.0); // x = -sin·sy·y
        assert_eq!(m[1][0], 10.0); // y = sin·sx·x
        assert!(m[0][0].abs() < 1e-6, "cos must vanish on x: {}", m[0][0]);
        assert!(m[1][1].abs() < 1e-6, "cos must vanish on y: {}", m[1][1]);
        assert_eq!(t, [0.0, 0.0]);
    }

    #[test]
    fn model_corner_positions_for_known_inputs() {
        // origin (10, 20), scale (2, 0.5), size (100, 50): a 200×25 world
        // rect. Its corners in world units:
        let (m, t) = layer_model(0.0, [2.0, 0.5], [100.0, 50.0], [10.0, 20.0]);
        let corner = |p: [f32; 2]| {
            [
                m[0][0] * p[0] + m[0][1] * p[1] + t[0],
                m[1][0] * p[0] + m[1][1] * p[1] + t[1],
            ]
        };
        // pos (-0.5, -0.5) → 10 - 100, 20 - 12.5
        assert_eq!(corner([-0.5, -0.5]), [-90.0, 7.5]);
        // pos (0.5, 0.5) → 10 + 100, 20 + 12.5
        assert_eq!(corner([0.5, 0.5]), [110.0, 32.5]);
        // The origin is the image center (WE alignment "center").
        assert_eq!(corner([0.0, 0.0]), [10.0, 20.0]);
    }

    #[test]
    fn alpha_and_scalar_clamps_are_bounded() {
        assert_eq!(clamp_layer_alpha(2.0), 1.0);
        assert_eq!(clamp_layer_alpha(-1.0), 0.0);
        assert_eq!(clamp_layer_alpha(0.5), 0.5);
        assert_eq!(clamp_layer_alpha(f64::NAN), 0.0);
        assert_eq!(clamp_layer_alpha(f64::INFINITY), 0.0);
        assert_eq!(clamp_layer_alpha(f64::NEG_INFINITY), 0.0);

        assert_eq!(clamp_layer_scalar(1e9), 1e6);
        assert_eq!(clamp_layer_scalar(-1e9), -1e6);
        assert_eq!(clamp_layer_scalar(3.25), 3.25);
        assert_eq!(clamp_layer_scalar(f64::NAN), 0.0);

        assert_eq!(clamp_layer_size(-100.0), 0.0);
        assert_eq!(clamp_layer_size(2.5), 2.5);
        assert_eq!(clamp_layer_size(f64::NAN), 0.0);
        assert_eq!(clamp_layer_size(1e9), 1e6);
    }

    #[test]
    fn draw_list_skips_invisible_and_textureless_layers() {
        let layers = vec![
            state("bg", true),
            state("hidden", false),
            state("broken", true),
        ];
        let texture_ok = [true, true, false];
        let draws = frame_draws(&layers, &texture_ok, &[]);
        assert_eq!(draws.len(), 1);
        assert_eq!(draws[0].layer_index, 0);
        // Order is scene.json order when several layers draw.
        let layers = vec![state("hidden", false), state("a", true), state("b", true)];
        let draws = frame_draws(&layers, &[true, true, true], &[]);
        assert_eq!(
            draws
                .iter()
                .map(|draw| draw.layer_index)
                .collect::<Vec<_>>(),
            [1, 2]
        );
    }

    #[test]
    fn draw_list_passes_state_through() {
        let layer = state("fg", true);
        {
            let mut state = layer.borrow_mut();
            state.alpha = 0.25;
            state.angles[2] = 90.0;
            state.origin = [40.0, 8.0];
            state.scale = [2.0, 1.0];
            state.size = [16.0, 16.0];
            // M3d: the effects and blend mode ride along into the draw.
            state.blend_mode = BlendMode::Screen;
            state.brightness = 2.0;
            state.tint = [0.5, 1.0, 0.25, 0.75];
        }
        let draws = frame_draws(&[layer], &[true], &[]);
        assert_eq!(draws.len(), 1);
        assert_eq!(draws[0].alpha, 0.25);
        assert_eq!(draws[0].t, [40.0, 8.0]);
        assert_eq!(draws[0].blend_mode, BlendMode::Screen);
        assert_eq!(draws[0].brightness, 2.0);
        assert_eq!(draws[0].tint, [0.5, 1.0, 0.25, 0.75]);
        assert_eq!(draws[0].kind, DrawKind::Image);
        // sx = 2·16 = 32, sy = 1·16 = 16, rotated 90°:
        // x = -1·16·y, y = 1·32·x (the off-diagonal cos terms are ~1e-6).
        let m = draws[0].m;
        assert_eq!(m[0][1], -16.0);
        assert_eq!(m[1][0], 32.0);
        assert!(m[0][0].abs() < 1e-4, "cos term on x: {}", m[0][0]);
        assert!(m[1][1].abs() < 1e-4, "cos term on y: {}", m[1][1]);
    }

    // ---- M3f: draw-order merge across kinds ----

    #[test]
    fn merged_draws_restore_the_file_object_order_across_kinds() {
        // parse_objects pushes each kind in the file's objects order, so
        // the two lists are individually ascending; merged_draws must
        // interleave them so an image that appears AFTER a particle system
        // in the file draws ON TOP of it — the adversarial-review
        // regression: the old `draws.extend(...)` put every particle draw
        // last, whatever the file said.
        let image = |scene_order| LayerDraw {
            layer_index: scene_order,
            scene_order,
            m: [[1.0, 0.0], [0.0, 1.0]],
            t: [0.0, 0.0],
            alpha: 1.0,
            blend_mode: BlendMode::Normal,
            brightness: 1.0,
            tint: [1.0, 1.0, 1.0, 1.0],
            kind: DrawKind::Image,
            material: false,
        };
        let particle = |scene_order| LayerDraw {
            layer_index: MAX_LAYERS + scene_order,
            scene_order,
            m: [[1.0, 0.0], [0.0, 1.0]],
            t: [0.0, 0.0],
            alpha: 1.0,
            blend_mode: BlendMode::Normal,
            brightness: 1.0,
            tint: [1.0, 1.0, 1.0, 1.0],
            kind: DrawKind::Particles { vertex_count: 6 },
            material: false,
        };
        // File: [particle @0, image @1, particle @2, image @3] — an
        // invisible layer @4 and an untextured system @5 are absent from
        // both lists, as the builders already skipped them.
        let merged = merged_draws(vec![image(1), image(3)], vec![particle(0), particle(2)]);
        let orders: Vec<usize> = merged.iter().map(|draw| draw.scene_order).collect();
        assert_eq!(orders, [0, 1, 2, 3]);
        assert_eq!(merged[0].kind, DrawKind::Particles { vertex_count: 6 });
        assert_eq!(merged[1].kind, DrawKind::Image);
        assert_eq!(merged[2].kind, DrawKind::Particles { vertex_count: 6 });
        assert_eq!(merged[3].kind, DrawKind::Image);
        // The smoke fixture's pair: [particle @0, image @1] — the image
        // (red square) must draw LAST, i.e. ON TOP of the particles.
        let merged = merged_draws(vec![image(1)], vec![particle(0)]);
        assert_eq!(merged[0].kind, DrawKind::Particles { vertex_count: 6 });
        assert_eq!(merged[1].kind, DrawKind::Image);
        assert_eq!(merged[1].layer_index, 1, "slots survive the merge");
        // Empty sides.
        assert_eq!(merged_draws(vec![], vec![]).len(), 0);
        assert_eq!(merged_draws(vec![image(0)], vec![]).len(), 1);
        assert_eq!(merged_draws(vec![], vec![particle(0)]).len(), 1);
    }

    // ---- M3e: text layers ----

    #[test]
    fn text_draws_use_the_text_vertex_path() {
        // A synced text layer (vertex_count > 0) draws with the Text kind,
        // its color as the tint, and size (1,1) so layout pixels map 1:1.
        let layer = text_state("t", "Hi", [1.0, 0.0, 0.0, 1.0], 12);
        {
            let mut state = layer.borrow_mut();
            state.origin = [5.0, -3.0];
            state.scale = [2.0, 0.5];
            state.angles[2] = 90.0;
            state.alpha = 0.5;
        }
        let draws = frame_draws(&[layer], &[true], &[]);
        assert_eq!(draws.len(), 1);
        assert_eq!(draws[0].kind, DrawKind::Text { vertex_count: 12 });
        // The text color drives the tint slot (not the layer's tint).
        assert_eq!(draws[0].tint, [1.0, 0.0, 0.0, 1.0]);
        assert_eq!(draws[0].alpha, 0.5);
        // m = R(90°)·S(2, 0.5)·I: x = -0.5·y, y = 2·x (cos terms ~1e-8).
        let m = draws[0].m;
        assert_eq!(m[0][1], -0.5);
        assert_eq!(m[1][0], 2.0);
        assert!(m[0][0].abs() < 1e-6, "cos term on x: {}", m[0][0]);
        assert!(m[1][1].abs() < 1e-6, "cos term on y: {}", m[1][1]);
        assert_eq!(draws[0].t, [5.0, -3.0]);
    }

    #[test]
    fn text_layers_without_vertex_data_are_skipped() {
        // Empty text, a missing font, or a not-yet-synced layer all yield
        // vertex_count 0: no draw at all (never a 1x1 image quad).
        let layers = vec![
            text_state("t0", "", [1.0, 1.0, 1.0, 1.0], 0),
            state("img", true),
            text_state("t1", "later", [1.0, 1.0, 1.0, 1.0], 0),
        ];
        let draws = frame_draws(&layers, &[true, true, true], &[]);
        assert_eq!(draws.len(), 1);
        assert_eq!(draws[0].layer_index, 1);
        assert_eq!(draws[0].kind, DrawKind::Image);
    }

    #[test]
    fn text_layer_state_comes_from_spec() {
        // from_spec pins size to (1,1) for text layers, keeps image sizes,
        // starts the text state dirty with 0 vertices.
        let spec = crate::scene::TextSpec {
            text: "Hi".into(),
            font: Some("systemfont_DejaVu Sans".into()),
            pointsize: 52.0,
            horizontal_align: HorizontalAlign::Right,
            vertical_align: VerticalAlign::Top,
            color: [0.5, 0.25, 1.0, 0.75],
            has_size: false,
        };
        let layer = LayerState::from_spec(&LayerSpec {
            name: "t".into(),
            scene_order: 0,
            image: None,
            model_ref: None,
            origin: [1.0, 2.0],
            angles: [0.0, 0.0, 0.0],
            scale: [2.0, 2.0],
            size: [0.0, 0.0],
            alpha: 1.0,
            visible: true,
            blend_mode: 0,
            brightness: 1.0,
            tint: [0.5, 0.25, 1.0, 0.75],
            texture: None,
            text: Some(spec),
            video: None,
            material: None,
        });
        assert_eq!(layer.size, [1.0, 1.0]);
        let text = layer.text.as_ref().unwrap();
        assert_eq!(text.text, "Hi");
        assert_eq!(text.font.as_deref(), Some("systemfont_DejaVu Sans"));
        assert_eq!(text.pointsize_px, 52.0);
        assert_eq!(text.horizontal_align, HorizontalAlign::Right);
        assert_eq!(text.vertical_align, VerticalAlign::Top);
        assert_eq!(text.color, [0.5, 0.25, 1.0, 0.75]);
        assert!(text.dirty);
        assert_eq!(text.vertex_count, 0);
    }
}
