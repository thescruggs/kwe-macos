// SPDX-License-Identifier: Apache-2.0
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

/// Cap on registered image layers (scene.rs rejects beyond this at parse;
/// the brief's bound — layers up to 256 are bound).
pub const MAX_LAYERS: usize = 256;

/// Bound on every scalar the script can write (mirror of the vector bounds
/// enforced at parse): non-finite values clamp to 0, magnitudes to ±1e6.
pub const MAX_LAYER_VALUE: f64 = 1e6;

/// The per-layer runtime state the script sees and the compositor draws.
/// Scripts mutate it through the Scene.getLayer proxies (js.rs); the worker
/// borrows it per frame to build the draw list. Everything here is a plain
/// value — all validation happens at the proxy boundary.
#[derive(Debug, Clone)]
pub struct LayerState {
    pub name: String,
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
}

impl LayerState {
    pub fn from_spec(spec: &LayerSpec) -> Self {
        Self {
            name: spec.name.clone(),
            alpha: spec.alpha,
            visible: spec.visible,
            angles: spec.angles,
            origin: spec.origin,
            scale: spec.scale,
            size: spec.size,
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

/// One layer's draw command for one frame. `m` and `t` are the model
/// transform in row-major form: world = m·pos + t for pos ∈ [-0.5, 0.5]².
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct LayerDraw {
    /// Index into the renderer's texture/descriptor table — the layer's
    /// position in the scene.json `objects` array.
    pub layer_index: usize,
    /// Linear part: [[m00, m01], [m10, m11]] (x = m00·x + m01·y + tx).
    pub m: [[f32; 2]; 2],
    /// Translation in scene units: the layer's origin.
    pub t: [f32; 2],
    /// Straight alpha in 0..=1, pushed into the fragment shader.
    pub alpha: f32,
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
/// layers and layers whose texture failed to load (texture_ok). Pure —
/// unit-tested; the worker calls it once per render.
pub fn frame_draws(layers: &[Rc<RefCell<LayerState>>], texture_ok: &[bool]) -> Vec<LayerDraw> {
    layers
        .iter()
        .enumerate()
        .filter_map(|(layer_index, state)| {
            let state = state.borrow();
            if !state.visible || texture_ok.get(layer_index) != Some(&true) {
                return None;
            }
            let (m, t) = layer_model(state.angles[2], state.scale, state.size, state.origin);
            Some(LayerDraw {
                layer_index,
                m,
                t,
                alpha: state.alpha,
            })
        })
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn state(name: &str, visible: bool) -> Rc<RefCell<LayerState>> {
        Rc::new(RefCell::new(LayerState {
            name: name.into(),
            alpha: 1.0,
            visible,
            angles: [0.0, 0.0, 0.0],
            origin: [0.0, 0.0],
            scale: [1.0, 1.0],
            size: [10.0, 20.0],
        }))
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
        let draws = frame_draws(&layers, &texture_ok);
        assert_eq!(draws.len(), 1);
        assert_eq!(draws[0].layer_index, 0);
        // Order is scene.json order when several layers draw.
        let layers = vec![state("hidden", false), state("a", true), state("b", true)];
        let draws = frame_draws(&layers, &[true, true, true]);
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
        }
        let draws = frame_draws(&[layer], &[true]);
        assert_eq!(draws.len(), 1);
        assert_eq!(draws[0].alpha, 0.25);
        assert_eq!(draws[0].t, [40.0, 8.0]);
        // sx = 2·16 = 32, sy = 1·16 = 16, rotated 90°:
        // x = -1·16·y, y = 1·32·x (the off-diagonal cos terms are ~1e-6).
        let m = draws[0].m;
        assert_eq!(m[0][1], -16.0);
        assert_eq!(m[1][0], 32.0);
        assert!(m[0][0].abs() < 1e-4, "cos term on x: {}", m[0][0]);
        assert!(m[1][1].abs() < 1e-4, "cos term on y: {}", m[1][1]);
    }
}
