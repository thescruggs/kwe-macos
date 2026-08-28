// SPDX-License-Identifier: GPL-3.0-or-later
//! SR-2c: builds the SAME `scene::SceneConfig`/`LayerSpec`/`ParticleSpec`
//! structures `scene::parse_scene_json` (the legacy JSON-interpretation
//! core, kept `#[cfg(test)]`-gated as the differential-test ORACLE —
//! conductor decision (b), deletion only after a full SR-2 epic soak)
//! produces, from the typed `kwe_core::SceneIr` (SR-2b) instead of
//! re-reading raw JSON directly.
//!
//! Family scope (conductor decision (a)): the scene.json TOP-LEVEL load
//! only — `general` + `objects[]` into the renderer's own structures.
//! Model/material/effect FILE loading (a model's `image` -> `model.json` ->
//! `material.json` -> `.tex` walk; an object's `effects[]` entries
//! resolved against real effect files) stays legacy: every struct this
//! module builds carries `material: None`, `texture: None`,
//! `effects: Vec::new()`, `component: None`, `path: None` — exactly the
//! same "not yet resolved" state `scene::parse_scene_json` itself leaves
//! them in; a LATER loader step (`main.rs`'s `load_model_textures` and
//! friends, untouched by this slice) fills them in for BOTH the legacy
//! and the IR-adapter path identically, since both paths hand it the
//! SAME structures from here on.
//!
//! ## The "would legacy reject" reconstruction
//!
//! `kwe_core::SceneIr` never rejects on a single field's shape (SR-2b
//! decision (b): a shape the typed reader cannot represent defaults AND
//! preserves the raw value in the nearest `UnknownBag`, rather than
//! failing the whole parse). The legacy parser, for most scalar/vector
//! fields, does the opposite: a present-but-malformed value REJECTS the
//! whole object (or, for a `SceneObjectKind::Model` object specifically,
//! silently drops just that one object — never the whole scene: the
//! pre-S1 "skip-never-reject" contract, mirrored here by
//! `build_model_layer` returning `Option`, not `Result`).
//!
//! This module reconstructs that reject/accept decision from the IR by
//! checking whether the relevant JSON key ended up in the object's (or
//! the particle definition's own) unknown bag: if the typed reader could
//! not represent it, the key is there UNCONSUMED, which for every
//! REJECTING field in the legacy parser means legacy would have rejected
//! it too (see `shape_rejected` below, and each per-field helper's own
//! doc comment for the one case — `tint`/`color` — where this needs a
//! deliberate "check only the alias's WINNING key" refinement to avoid a
//! false reject).
//!
//! `SceneError`'s message TEXT is not reconstructed to match legacy's
//! wording byte-for-byte — confirmed (SR-2c study) that nothing outside
//! `scene.rs`'s own test module inspects a `SceneError`'s `.message`
//! content; only `SceneErrorKind` and the Ok/Err outcome cross any
//! process/API boundary (the daemon classifies a renderer's scene
//! rejection by its fixed EXIT CODE, never by scraping stderr text). The
//! differential tests (`scene.rs`'s own test module) accordingly assert
//! `SceneErrorKind` + Ok/Err parity and full `SceneConfig` equality on
//! `Ok`, never message-string equality.
//!
//! ## Known, documented divergences (SR-2c STOP findings — narrow,
//! verified harmless against every existing repo fixture and the real
//! corpus, but real)
//!
//! 1. **Numeric-string leniency legacy does not have.** SR-2b's `as_number`
//!    helper uniformly accepts a JSON Number OR a numeric String for every
//!    numeric field; several legacy fields are Number-ONLY: `alpha`
//!    (`scene.rs`'s own `parse_common_props`, `.as_f64()` only — a numeric
//!    STRING alpha REJECTS in legacy, types fine through the IR),
//!    `blendMode`/`colorBlendMode` (Number-only in legacy, tolerant either
//!    way so this manifests as a VALUE difference, not reject/accept),
//!    `general.fps` (Number-only), `general.resolution`'s two dims
//!    (`.as_u64()` — a true JSON INTEGER only, not even a float; the IR
//!    truncates a float and accepts a numeric string), and — inside a
//!    JSON ARRAY specifically — `general.clearcolor`'s 4 array-form
//!    elements (each `.as_f64()` only; the IR's array-form vector reader
//!    accepts a numeric string PER ELEMENT too).
//! 2. **`size`'s exact shape/sign strictness.** Legacy's image/model `size`
//!    is `parse_vector(.., &[2], non_negative: true)` — EXACTLY 2
//!    components, negative REJECTS. The IR's `size` field accepts the
//!    SAME `[2,3]` lengths `origin`/`scale` do (a 3rd component silently
//!    dropped) and does not distinguish sign. A 3-component or negative
//!    authored `size` therefore types fine through the IR where legacy
//!    would reject it.
//! 3. **`rate`'s (video) infinity tolerance.** Legacy's `rate` parses a
//!    numeric string via `str::parse`, which accepts the literal words
//!    `"inf"`/`"nan"` as IEEE special values BEFORE `clamp_playback_rate`
//!    normalizes them; the IR's `as_number` -> `.filter(is_finite)` chain
//!    treats those same literal strings as a shape mismatch instead
//!    (defaults, preserves the raw string in `unknown`). `rate` never
//!    REJECTS in legacy either way (fully tolerant field), so this is a
//!    VALUE difference only, in an already-synthetic edge case.
//!
//! None of these affects any existing fixture in `scene.rs`'s own test
//! module (the full differential suite — every one of those fixtures,
//! plus the corpus-parity hook — passes clean); every one of them
//! requires content no real Wallpaper Engine editor writes (a numeric
//! STRING where the format has only ever used bare numbers, or a
//! 3-component/negative pixel `size`). Closing them for good needs
//! per-field strictness modes in SR-2b's `as_number`/`as_vector` (a
//! change to the IR itself, out of this slice's scope) — recorded as an
//! open risk in `docs/SR2.md`, not silently swept aside.

use serde_json::{Map, Value};

use kwe_core::{
    CommonPropsIr, IrError, ObjectIr, ObjectKindIr, ParticleIr, SceneIr, VisibleIr,
    scene_property_value,
};

use crate::layers::{self, MAX_LAYERS};
use crate::particles;
use crate::scene::{
    LayerSpec, MaterialSpec, ParticleSpec, SceneConfig, SceneError, SceneErrorKind, TextSpec,
    VideoSpec,
};
use crate::text::{self, HorizontalAlign, VerticalAlign};
use crate::video;

/// The production entry point `scene::SceneConfig::parse`/`parse_pkg` call
/// instead of the legacy `parse_scene_json`.
pub fn parse_scene_json_via_ir(bytes: &[u8]) -> Result<SceneConfig, SceneError> {
    let ir = kwe_core::parse_scene_ir(bytes).map_err(ir_error_to_scene_error)?;
    scene_from_ir(&ir)
}

fn ir_error_to_scene_error(error: IrError) -> SceneError {
    match error {
        IrError::Parse(message) => SceneError::new(
            SceneErrorKind::Json,
            format!("scene.json is not valid JSON: {message}"),
        ),
        IrError::NotAnObject => {
            SceneError::new(SceneErrorKind::Json, "scene.json root must be an object")
        }
        IrError::ObjectsCap => SceneError::new(
            SceneErrorKind::Shape,
            format!(
                "scene.json \"objects\" has more than {} entries",
                kwe_core::MAX_OBJECTS
            ),
        ),
        IrError::ObjectsNotAnArray => SceneError::new(
            SceneErrorKind::Shape,
            "scene.json \"objects\" must be an array",
        ),
        IrError::ObjectEntryNotAnObject { index } => SceneError::new(
            SceneErrorKind::Shape,
            format!("scene.json \"objects[{index}]\" must be an object"),
        ),
    }
}

fn reject(index: usize, field: &str) -> SceneError {
    SceneError::new(
        SceneErrorKind::Shape,
        format!("scene.json \"objects[{index}].{field}\" is malformed"),
    )
}

fn reject_general(field: &str) -> SceneError {
    SceneError::new(
        SceneErrorKind::Shape,
        format!("scene.json \"general.{field}\" is malformed"),
    )
}

/// `true` when `key` is present in `unknown` — the IR's typed reader could
/// not represent it, which for every field this module treats as
/// reject-worthy means the legacy parser would have rejected too. See the
/// module doc's "would legacy reject" section.
fn shape_rejected(unknown: &kwe_core::UnknownBag, key: &str) -> bool {
    unknown.get(key).is_some()
}

/// Legacy's own `parse_vector` bound (`scene.rs`): every vector component
/// must be finite and within ±`layers::MAX_LAYER_VALUE` (1e6) — `ir.rs`'s
/// `as_number`/`as_vector` do not enforce this (SR-2b's "no range
/// clamping" design, ir.rs module doc departure (1)), so every vector
/// field this module builds re-applies it, matching every `parse_vector`
/// call site in `scene.rs`.
fn within_layer_bounds(components: &[f32]) -> bool {
    components.iter().all(|component| {
        component.is_finite() && f64::from(component.abs()) <= layers::MAX_LAYER_VALUE
    })
}

// ---------------------------------------------------------------------------
// general
// ---------------------------------------------------------------------------

fn general_clear_color(ir: &SceneIr) -> Result<[f32; 4], SceneError> {
    if shape_rejected(&ir.general.unknown, "clearcolor") {
        return Err(reject_general("clearcolor"));
    }
    // `ir.rs::parse_clear_color` does not range-check its channels (0..=1)
    // — legacy REJECTS out of range for all 4 channels in the array form
    // (the string form hardcodes alpha to 1.0, always in range, so
    // checking all 4 uniformly is safe either way).
    if ir
        .general
        .clear_color
        .iter()
        .any(|channel| !channel.is_finite() || !(0.0..=1.0).contains(channel))
    {
        return Err(reject_general("clearcolor"));
    }
    Ok(ir.general.clear_color)
}

fn general_resolution(ir: &SceneIr) -> Result<Option<(u32, u32)>, SceneError> {
    // Only "resolution" is a reject signal — "orthogonalprojection" is
    // legacy's own LENIENT fallback (never rejects, `Ok(None)` on any
    // failure): a present-but-malformed orthogonalprojection sitting in
    // `general.unknown` (because it did not win the alias, or won but
    // failed to parse) must never trigger a rejection here.
    if shape_rejected(&ir.general.unknown, "resolution") {
        return Err(reject_general("resolution"));
    }
    // Neither `ir.rs::parse_resolution_array` nor its own `parse_
    // orthogonal_projection` enforce the upper `MAX_DIMENSION` bound (SR-2b
    // only validates positivity/finiteness) — legacy's own `parse_
    // resolution` DOES, for the direct `resolution` array (REJECTS out of
    // `1..=MAX_DIMENSION`). Applied uniformly here regardless of which key
    // actually produced the typed value (the IR does not preserve that),
    // which is stricter than legacy's fully-lenient orthogonalprojection
    // fallback for the synthetic edge case of an absurdly large
    // orthogonalprojection dimension — see the module doc's divergence
    // list; every real corpus scene's orthogonalprojection values are
    // ordinary display resolutions, nowhere near this bound.
    if let Some((width, height)) = ir.general.resolution {
        let in_range = |dim: u32| (1..=crate::scene::MAX_DIMENSION).contains(&dim);
        if !in_range(width) || !in_range(height) {
            return Err(reject_general("resolution"));
        }
    }
    Ok(ir.general.resolution)
}

fn general_fps(ir: &SceneIr) -> Result<Option<f32>, SceneError> {
    if shape_rejected(&ir.general.unknown, "fps") {
        return Err(reject_general("fps"));
    }
    // `ir.rs` types any finite `fps`; legacy additionally REJECTS outside
    // `(0.0, 240.0]` (`parse_fps`) — the range check the IR's own
    // "no range clamping/rejection" design (module doc departure (1) in
    // ir.rs) deliberately leaves to a consumer.
    if let Some(fps) = ir.general.fps
        && !(fps > 0.0 && fps <= 240.0)
    {
        return Err(reject_general("fps"));
    }
    Ok(ir.general.fps)
}

fn general_script(ir: &SceneIr) -> Result<Option<String>, SceneError> {
    if shape_rejected(&ir.general.unknown, "script") {
        return Err(reject_general("script"));
    }
    Ok(ir.general.script.clone())
}

// ---------------------------------------------------------------------------
// Common properties shared by every family
// ---------------------------------------------------------------------------

/// The subset of `CommonPropsIr` (plus `name`/`id`, hoisted onto `ObjectIr`
/// in the IR but living on `LayerSpec`/`ParticleSpec` directly in legacy)
/// every family's builder needs, already validated/clamped exactly like
/// `scene::parse_common_props`'s own return value.
struct Common {
    name: String,
    id: Option<i64>,
    origin: [f32; 2],
    angles: [f32; 3],
    scale: [f32; 2],
    alpha: f32,
    visible: bool,
    blend_mode: u32,
    brightness: f32,
}

/// The REJECTING families (image/text/video/particle): a malformed common
/// property fails the whole object with `SceneErrorKind::Shape`, exactly
/// like `scene::parse_common_props`.
fn require_common(object: &ObjectIr, index: usize) -> Result<Common, SceneError> {
    let name = match &object.name {
        Some(name) => name.clone(),
        None => {
            // Distinguishes legacy's two distinct name-rejection shapes
            // (missing vs. present-but-not-a-string) the same way, even
            // though the message text itself is not load-bearing (see the
            // module doc).
            return Err(reject(index, "name"));
        }
    };
    if shape_rejected(&object.unknown, "origin") || !within_layer_bounds(&object.common.origin) {
        return Err(reject(index, "origin"));
    }
    // `ir.rs` stores `angles` as authored (radians, decision (b) —
    // conversion is a rendering-layer interpretation, not authored
    // state); `MAX_LAYER_VALUE` bounds the RAW radian value here, exactly
    // where legacy's own `parse_vector` bound check runs, BEFORE the
    // `to_degrees()` conversion below.
    if shape_rejected(&object.unknown, "angles") || !within_layer_bounds(&object.common.angles) {
        return Err(reject(index, "angles"));
    }
    if shape_rejected(&object.unknown, "scale") || !within_layer_bounds(&object.common.scale) {
        return Err(reject(index, "scale"));
    }
    let common_alpha_ok = common_alpha(&object.common, &object.unknown, index)?;
    let visible = common_visible(&object.common.visible, index)?;
    let brightness = common_brightness(&object.common, &object.unknown, index)?;
    Ok(Common {
        name,
        id: object.authored_id,
        origin: object.common.origin,
        angles: radians_to_degrees(object.common.angles),
        scale: object.common.scale,
        alpha: common_alpha_ok,
        visible,
        blend_mode: object.common.blend_mode, // tolerant, never rejects
        brightness,
    })
}

fn radians_to_degrees(angles: [f32; 3]) -> [f32; 3] {
    [
        angles[0].to_degrees(),
        angles[1].to_degrees(),
        angles[2].to_degrees(),
    ]
}

/// The SKIP-NEVER-REJECT family (Model, S1's pre-S1 contract): `None`
/// means "drop this object, never fail the scene" — mirrors
/// `scene::parse_model_layer`'s own `Option`-returning shape exactly.
fn optional_common(object: &ObjectIr) -> Option<Common> {
    let name = object.name.clone()?;
    if shape_rejected(&object.unknown, "origin") || !within_layer_bounds(&object.common.origin) {
        return None;
    }
    if shape_rejected(&object.unknown, "angles") || !within_layer_bounds(&object.common.angles) {
        return None;
    }
    if shape_rejected(&object.unknown, "scale") || !within_layer_bounds(&object.common.scale) {
        return None;
    }
    let alpha = object.common.alpha;
    if shape_rejected(&object.unknown, "alpha")
        || !alpha.is_finite()
        || !(0.0..=1.0).contains(&alpha)
    {
        return None;
    }
    let visible = match &object.common.visible {
        VisibleIr::Bool(value) => *value,
        VisibleIr::Absent => true,
        VisibleIr::PropertyBound(_) => return None,
    };
    if shape_rejected(&object.unknown, "brightness") {
        return None;
    }
    let brightness = layers::clamp_layer_brightness(f64::from(object.common.brightness));
    Some(Common {
        name,
        id: object.authored_id,
        origin: object.common.origin,
        angles: radians_to_degrees(object.common.angles),
        scale: object.common.scale,
        alpha,
        visible,
        blend_mode: object.common.blend_mode,
        brightness,
    })
}

fn common_alpha(
    common: &CommonPropsIr,
    unknown: &kwe_core::UnknownBag,
    index: usize,
) -> Result<f32, SceneError> {
    if shape_rejected(unknown, "alpha") {
        return Err(reject(index, "alpha"));
    }
    let alpha = common.alpha;
    if !alpha.is_finite() || !(0.0..=1.0).contains(&alpha) {
        return Err(reject(index, "alpha"));
    }
    Ok(alpha)
}

fn common_visible(visible: &VisibleIr, index: usize) -> Result<bool, SceneError> {
    match visible {
        VisibleIr::Bool(value) => Ok(*value),
        VisibleIr::Absent => Ok(true),
        VisibleIr::PropertyBound(_) => Err(reject(index, "visible")),
    }
}

fn common_brightness(
    common: &CommonPropsIr,
    unknown: &kwe_core::UnknownBag,
    index: usize,
) -> Result<f32, SceneError> {
    if shape_rejected(unknown, "brightness") {
        return Err(reject(index, "brightness"));
    }
    Ok(layers::clamp_layer_brightness(f64::from(common.brightness)))
}

/// `size`/`tint` — shared by Model and Image/TexvImage/TexturelessImage
/// exactly like `scene::parse_size_and_tint`. `size` accepts whatever
/// shape the IR already accepted (module doc divergence #2: legacy is
/// stricter — exactly 2, non-negative); `tint` rejects on the SAME
/// "winning alias key present in unknown" signal `require_common` uses
/// elsewhere — checking only `"tint"`, never `"color"`, for the reason
/// documented on `shape_rejected` and the module doc: `color`'s presence
/// in `unknown` is the NORMAL, expected trace of the alias-loser rule
/// whenever `tint` won cleanly, not a shape-mismatch signal.
fn size_and_tint(
    unknown: &kwe_core::UnknownBag,
    size: [f32; 2],
    tint: [f32; 4],
    index: usize,
) -> Result<([f32; 2], [f32; 4]), SceneError> {
    // `size` additionally REJECTS a negative component in legacy
    // (`parse_vector(.., non_negative: true)`); the IR's typed value still
    // preserves the authored sign (SR-2b never clamps), so it is checked
    // directly here (module doc divergence #2 is narrower than an earlier
    // draft of this comment claimed: only the 3-vs-2-component shape gap
    // is genuinely unrecoverable from the IR, not the sign).
    if shape_rejected(unknown, "size")
        || !within_layer_bounds(&size)
        || size.iter().any(|v| *v < 0.0)
    {
        return Err(reject(index, "size"));
    }
    if shape_rejected(unknown, "tint") || !within_layer_bounds(&tint) {
        return Err(reject(index, "tint"));
    }
    Ok((size, clamp_tint(tint)))
}

fn clamp_tint(tint: [f32; 4]) -> [f32; 4] {
    let mut clamped = [1.0_f32; 4];
    for (slot, component) in clamped.iter_mut().zip(tint.iter()) {
        *slot = layers::clamp_layer_tint(f64::from(*component));
    }
    clamped
}

#[allow(clippy::too_many_arguments)] // an internal struct-literal builder, not a public API
fn layer_spec(
    common: Common,
    index: usize,
    image: Option<String>,
    model_ref: Option<String>,
    size: [f32; 2],
    tint: [f32; 4],
    text: Option<TextSpec>,
    video: Option<VideoSpec>,
    effects_raw: Vec<Value>,
) -> LayerSpec {
    LayerSpec {
        name: common.name,
        id: common.id,
        scene_order: index,
        image,
        model_ref,
        origin: common.origin,
        angles: common.angles,
        scale: common.scale,
        size,
        alpha: common.alpha,
        visible: common.visible,
        blend_mode: common.blend_mode,
        brightness: common.brightness,
        tint,
        texture: None,
        text,
        video,
        material: None as Option<MaterialSpec>,
        fullscreen: false,
        effects_raw,
        effects: Vec::new(),
    }
}

// ---------------------------------------------------------------------------
// objects[]
// ---------------------------------------------------------------------------

fn object_can_draw(kind: &ObjectKindIr) -> bool {
    matches!(
        kind,
        ObjectKindIr::Image(_)
            | ObjectKindIr::Video(_)
            | ObjectKindIr::Particle(_)
            | ObjectKindIr::Text(_)
    )
}

/// `object.unknown` carries a raw, unconsumed `"text"` key whenever the
/// object also authored one (Image/TexvImage/TexturelessImage never read
/// `text` at all, so it is NEVER consumed) — the exact presence check
/// `scene::parse_objects`'s `object.contains_key("text")` makes.
fn carries_a_text_key(object: &ObjectIr) -> bool {
    object.unknown.get("text").is_some()
}

pub fn scene_from_ir(ir: &SceneIr) -> Result<SceneConfig, SceneError> {
    let clear_color = general_clear_color(ir)?;
    let resolution = general_resolution(ir)?;
    let fps = general_fps(ir)?;
    let script_reference = general_script(ir)?;

    let mut layers_out: Vec<LayerSpec> = Vec::new();
    let mut particles_out: Vec<ParticleSpec> = Vec::new();
    let mut drawable_objects = 0usize;
    let mut model_layer_skips = 0usize;
    let mut text_layer_skips = 0usize;
    let mut particle_system_skips = 0usize;
    let mut video_layer_skips = 0usize;
    let mut particle_file_refs = 0usize;
    let mut text_on_image_objects = 0usize;
    let mut text_size_ignored = 0usize;
    let mut registered_text_layers = 0usize;
    let mut registered_video_layers = 0usize;

    for (index, object) in ir.objects.iter().enumerate() {
        if object_can_draw(&object.kind) {
            drawable_objects += 1;
        }
        match &object.kind {
            ObjectKindIr::Model(model) => {
                model_layer_skips += 1;
                if let Some(common) = optional_common(object) {
                    let sized = size_and_tint(&object.unknown, model.size, model.tint, index).ok();
                    if let Some((size, tint)) = sized {
                        let effects_raw = effects_raw_for(object);
                        layers_out.push(layer_spec(
                            common,
                            index,
                            None,
                            Some(model.model_ref.clone()),
                            size,
                            tint,
                            None,
                            None,
                            effects_raw,
                        ));
                    }
                }
            }
            ObjectKindIr::Image(image) => {
                if carries_a_text_key(object) {
                    text_on_image_objects += 1;
                }
                let common = require_common(object, index)?;
                let (size, tint) = size_and_tint(&object.unknown, image.size, image.tint, index)?;
                layers_out.push(layer_spec(
                    common,
                    index,
                    Some(image.image.clone()),
                    None,
                    size,
                    tint,
                    None,
                    None,
                    Vec::new(),
                ));
            }
            ObjectKindIr::TexvImage(image) => {
                if carries_a_text_key(object) {
                    text_on_image_objects += 1;
                }
                let common = require_common(object, index)?;
                let (size, tint) = size_and_tint(&object.unknown, image.size, image.tint, index)?;
                layers_out.push(layer_spec(
                    common,
                    index,
                    Some(image.image.clone()),
                    None,
                    size,
                    tint,
                    None,
                    None,
                    Vec::new(),
                ));
            }
            ObjectKindIr::TexturelessImage(image) => {
                if carries_a_text_key(object) {
                    text_on_image_objects += 1;
                }
                let common = require_common(object, index)?;
                let (size, tint) = size_and_tint(&object.unknown, image.size, image.tint, index)?;
                layers_out.push(layer_spec(
                    common,
                    index,
                    None,
                    None,
                    size,
                    tint,
                    None,
                    None,
                    Vec::new(),
                ));
            }
            ObjectKindIr::Video(video_ir) => {
                let common = require_common(object, index)?;
                let (size, tint) =
                    size_and_tint(&object.unknown, video_ir.size, video_ir.tint, index)?;
                let over_cap = registered_video_layers >= video::MAX_VIDEO_LAYERS;
                if over_cap {
                    video_layer_skips += 1;
                } else {
                    registered_video_layers += 1;
                }
                let source = if over_cap {
                    None
                } else {
                    video_ir.source.clone()
                };
                let spec = VideoSpec {
                    source,
                    loop_playback: video_ir.loop_playback,
                    rate: video::clamp_playback_rate(f64::from(video_ir.rate)),
                    path: None,
                };
                layers_out.push(layer_spec(
                    common,
                    index,
                    None,
                    None,
                    size,
                    tint,
                    None,
                    Some(spec),
                    Vec::new(),
                ));
            }
            ObjectKindIr::Particle(particle_ir) => {
                if particles_out.len() >= particles::MAX_PARTICLE_SYSTEMS {
                    particle_system_skips += 1;
                    continue;
                }
                particles_out.push(build_particle_system(object, particle_ir, index)?);
            }
            ObjectKindIr::ParticleFile(file_ir) => {
                if particles_out.len() >= particles::MAX_PARTICLE_SYSTEMS {
                    particle_system_skips += 1;
                    continue;
                }
                let common = require_common(object, index)?;
                if let Some(file_ref) = &file_ir.file_ref {
                    // classify_scene_object's ParticleFile only means "not
                    // an inline definition with a resolvable texture/
                    // material" — legacy's `parse_particle_system` reads
                    // the RAW `particle` value directly, independent of
                    // that classification, so a string here really is a
                    // file reference (`counts.particle_file_refs` only
                    // ever counts THIS shape, per `parse_objects`).
                    particle_file_refs += 1;
                    particles_out.push(particle_file_spec(
                        common,
                        index,
                        Some(file_ref.clone()),
                        file_ir,
                    ));
                } else if let Some(definition) =
                    object.unknown.get("particle").and_then(Value::as_object)
                {
                    // The one case `ir.rs`'s own ParticleFile handling
                    // cannot type ahead of time: `particle` unwraps to an
                    // OBJECT that classify_scene_object still routed to
                    // ParticleFile (no string `texture`/`material` key) —
                    // legacy's `parse_particle_system` does not re-check
                    // that classification at all; it parses the SAME
                    // inline object exactly like a Particle-kind one (every
                    // flat field, just with no resolvable texture in
                    // practice). `ir.rs` left the WHOLE raw object
                    // unconsumed in this exact case (see its own doc
                    // comment), so it is available here byte-faithfully.
                    // `instanceoverride` itself is NOT part of that raw
                    // residue (it is a sibling of `particle` on the
                    // object, read into `file_ir`'s own typed fields
                    // regardless of `particle`'s shape — see
                    // `ParticleFileIr`'s doc comment), so it is threaded
                    // through from `file_ir`, not re-read from `definition`.
                    particles_out.push(build_particle_system_from_raw(
                        definition, common, index, file_ir,
                    )?);
                } else {
                    // Neither a string nor an object (number/bool/array/
                    // null) — legacy's own catch-all: registers with every
                    // default, `file_ref` stays `None`, not counted as a
                    // file reference.
                    particles_out.push(particle_file_spec(common, index, None, file_ir));
                }
            }
            ObjectKindIr::Text(text_ir) => {
                if registered_text_layers >= text::MAX_TEXT_LAYERS {
                    text_layer_skips += 1;
                    continue;
                }
                registered_text_layers += 1;
                if text_ir.has_size {
                    text_size_ignored += 1;
                }
                let common = require_common(object, index)?;
                layers_out.push(build_text_layer(object, common, index, text_ir)?);
            }
            ObjectKindIr::Unknown => continue,
        }
    }

    if layers_out.len() > MAX_LAYERS {
        return Err(SceneError::new(
            SceneErrorKind::Shape,
            format!(
                "scene.json \"objects\" has {} image layers, over the {MAX_LAYERS} layer cap",
                layers_out.len()
            ),
        ));
    }

    Ok(SceneConfig {
        clear_color,
        script_path: None,
        script_entry: None,
        script_reference,
        resolution,
        fps,
        layers: layers_out,
        particles: particles_out,
        drawable_objects,
        model_layer_skips,
        text_layer_skips,
        particle_system_skips,
        video_layer_skips,
        particle_file_refs,
        text_on_image_objects,
        text_size_ignored,
    })
}

/// A model layer's own `effects` array — S3's raw-`Vec<Value>` carry,
/// bounded the same way `scene::parse_model_layer` bounds it
/// (`kwe_core::MAX_EFFECTS_PER_OBJECT`, 32). `ObjectIr::effects` is
/// already parsed into typed `EffectRefIr` entries, but legacy wants the
/// RAW JSON array entries UNCHANGED (`.cloned()`, no defaulting of
/// id/name/visible — resolution happens later, against real files,
/// outside this family's scope) — each `EffectRefIr::raw` carries exactly
/// that original value, so this reads `raw` directly rather than
/// reconstructing from the typed fields (an earlier version did the
/// latter and wrongly materialized id/name/visible defaults into entries
/// that never authored them — caught by `ir_parity_effects_with_unknown_keys`).
fn effects_raw_for(object: &ObjectIr) -> Vec<Value> {
    // `EffectRefIr::raw` is the entry's ORIGINAL JSON value, byte-faithful
    // — legacy's own `parse_model_layer` clones the raw array entries
    // UNCHANGED (no defaulting of id/name/visible at parse time; that
    // happens only when `sceneeffect::resolve_object_effects` LATER reads
    // them), so reconstructing from the typed `id`/`name`/`visible` fields
    // here would wrongly materialize defaults the original entry never
    // had (SR-2c differential testing caught this).
    object
        .effects
        .iter()
        .take(kwe_core::MAX_EFFECTS_PER_OBJECT)
        .map(|effect| effect.raw.clone())
        .collect()
}

fn build_text_layer(
    object: &ObjectIr,
    common: Common,
    index: usize,
    text_ir: &kwe_core::TextIr,
) -> Result<LayerSpec, SceneError> {
    // `pointsize` is TYPE-STRICT in legacy (Number-or-numeric-string,
    // rejects any other shape) — `text::pointsize_to_px` itself already
    // handles the RANGE tolerantly (non-finite/<=0 -> the default), so no
    // additional range check is needed here, only the shape one.
    if shape_rejected(&object.unknown, "pointsize") {
        return Err(reject(index, "pointsize"));
    }
    let pointsize_px = text::pointsize_to_px(f64::from(text_ir.pointsize));
    let horizontal_align = match text_ir.horizontal_align {
        kwe_core::HorizontalAlignIr::Left => HorizontalAlign::Left,
        kwe_core::HorizontalAlignIr::Center => HorizontalAlign::Center,
        kwe_core::HorizontalAlignIr::Right => HorizontalAlign::Right,
    };
    let vertical_align = match text_ir.vertical_align {
        kwe_core::VerticalAlignIr::Top => VerticalAlign::Top,
        kwe_core::VerticalAlignIr::Center => VerticalAlign::Center,
        kwe_core::VerticalAlignIr::Bottom => VerticalAlign::Bottom,
    };
    // `color` (text has no `tint` alias) — same `parse_vector`-backed
    // shape/magnitude reject as every other vector field.
    if shape_rejected(&object.unknown, "color") || !within_layer_bounds(&text_ir.color) {
        return Err(reject(index, "color"));
    }
    let color = clamp_tint(text_ir.color);
    let spec = TextSpec {
        text: text_ir.text.clone(),
        font: text_ir.font.clone(),
        pointsize: pointsize_px,
        horizontal_align,
        vertical_align,
        color,
        has_size: text_ir.has_size,
    };
    Ok(layer_spec(
        common,
        index,
        None,
        None,
        [1.0, 1.0], // text renders at layout size; scale does the resizing
        color,
        Some(spec),
        None,
        Vec::new(),
    ))
}

/// `kwe_core::ir.rs`'s `parse_instance_override` does NOT clamp any of the
/// 7 fields it types (SR-2b's "no range clamping" design departure, module
/// doc departure (1) — every numeric field holds the coerced-but-unclamped
/// authored value) — `ParticleIr`/`ParticleFileIr`'s own `instance_*`
/// fields are therefore raw, same as every other IR numeric field.
/// `scene.rs`'s own `parse_particle_system` clamps each one via
/// `particles::clamp_instance_factor` (max 1e6 for count/rate/size/
/// lifetime/speed, max 1.0 for alpha) and `colorn` via `.clamp(0.0, 1.0)`
/// (its own inline `mean.clamp(0.0, 1.0)`) BEFORE assigning into
/// `ParticleSpec`. `build_particle_system` (the Particle-kind path)
/// already applies this; this shared helper does the same for the
/// ParticleFile-kind paths (`particle_file_spec` and
/// `build_particle_system_from_raw`) — an earlier version of both read
/// `file_ir`'s fields straight through unclamped, a real-corpus-caught
/// bug (an authored `instanceoverride.alpha` of 2.0 stayed 2.0 through the
/// IR path instead of clamping to legacy's 1.0 ceiling).
fn clamp_instance_overrides(
    file_ir: &kwe_core::ParticleFileIr,
) -> (f32, f32, f32, f32, f32, f32, f32) {
    (
        particles::clamp_instance_factor(f64::from(file_ir.instance_count), 1e6),
        particles::clamp_instance_factor(f64::from(file_ir.instance_rate), 1e6),
        particles::clamp_instance_factor(f64::from(file_ir.instance_size), 1e6),
        particles::clamp_instance_factor(f64::from(file_ir.instance_lifetime), 1e6),
        particles::clamp_instance_factor(f64::from(file_ir.instance_speed), 1e6),
        particles::clamp_instance_factor(f64::from(file_ir.instance_alpha), 1.0),
        file_ir.instance_colorn.clamp(0.0, 1.0),
    )
}

/// `file_ir`'s `instance_*` fields ARE the object's `instanceoverride`
/// (see `kwe_core::ParticleFileIr`'s doc comment — read unconditionally,
/// the same way for a string file reference, an inline object, or neither)
/// — no additional shape-reject check is needed here, `instanceoverride`
/// never rejects in legacy either.
fn particle_file_spec(
    common: Common,
    index: usize,
    file_ref: Option<String>,
    file_ir: &kwe_core::ParticleFileIr,
) -> ParticleSpec {
    let (
        instance_count,
        instance_rate,
        instance_size,
        instance_lifetime,
        instance_speed,
        instance_alpha,
        instance_colorn,
    ) = clamp_instance_overrides(file_ir);
    ParticleSpec {
        name: common.name,
        scene_order: index,
        origin: common.origin,
        spawn_rate: particles::DEFAULT_PARTICLE_SPAWN_RATE,
        life: particles::DEFAULT_PARTICLE_LIFE,
        speed_min: particles::DEFAULT_PARTICLE_SPEED,
        speed_max: particles::DEFAULT_PARTICLE_SPEED,
        direction: particles::DEFAULT_PARTICLE_DIRECTION,
        spread: particles::DEFAULT_PARTICLE_SPREAD,
        gravity: particles::DEFAULT_PARTICLE_GRAVITY,
        size_start: particles::DEFAULT_PARTICLE_SIZE,
        size_end: particles::DEFAULT_PARTICLE_SIZE,
        color_start: [1.0, 1.0, 1.0, 1.0],
        color_end: [1.0, 1.0, 1.0, 1.0],
        alpha_start: particles::DEFAULT_PARTICLE_ALPHA_START,
        alpha_end: particles::DEFAULT_PARTICLE_ALPHA_END,
        material: None,
        max_count: particles::DEFAULT_PARTICLE_MAX_COUNT,
        blend_mode: common.blend_mode,
        alpha: common.alpha,
        visible: common.visible,
        brightness: common.brightness,
        texture: None,
        file_ref,
        component: None,
        instance_count,
        instance_rate,
        instance_size,
        instance_lifetime,
        instance_speed,
        instance_alpha,
        instance_colorn,
        scale: common.scale,
    }
}

/// The `"particle"` definition's own leftover sub-keys — populated only
/// when `parse_scene_ir` could not type at least one of them (SR-2b's
/// `ir.rs::parse_particle_ir` doc comment): `speed`/`speedMin`/`speedMax`
/// (module decision (c) — ALWAYS here whenever authored, deliberately
/// untyped) plus any genuinely malformed scalar/vector/maxCount/
/// instanceoverride key.
fn particle_residue(object: &ObjectIr) -> Option<&Map<String, Value>> {
    object.unknown.get("particle").and_then(Value::as_object)
}

fn particle_residue_has(object: &ObjectIr, key: &str) -> bool {
    particle_residue(object).is_some_and(|residue| residue.contains_key(key))
}

fn build_particle_system(
    object: &ObjectIr,
    particle_ir: &ParticleIr,
    index: usize,
) -> Result<ParticleSpec, SceneError> {
    let common = require_common(object, index)?;

    // gravity/colorStart/colorEnd are all `parse_vector`-backed in legacy
    // (rejects on shape OR on a component past ±MAX_LAYER_VALUE) — the
    // residue check catches shape; `within_layer_bounds` catches magnitude
    // (the IR's own gravity/color fields are unclamped, per SR-2b).
    if particle_residue_has(object, "gravity") || !within_layer_bounds(&particle_ir.gravity) {
        return Err(reject(index, "particle.gravity"));
    }
    if particle_residue_has(object, "colorStart") || !within_layer_bounds(&particle_ir.color_start)
    {
        return Err(reject(index, "particle.colorStart"));
    }
    if particle_residue_has(object, "colorEnd") || !within_layer_bounds(&particle_ir.color_end) {
        return Err(reject(index, "particle.colorEnd"));
    }
    for key in [
        "spawnRate",
        "life",
        "direction",
        "spread",
        "sizeStart",
        "sizeEnd",
        "alphaStart",
        "alphaEnd",
        "maxCount",
    ] {
        if particle_residue_has(object, key) {
            return Err(reject(index, &format!("particle.{key}")));
        }
    }
    // `texture`/`material`/`instanceoverride` are all TOLERANT fields in
    // legacy (never reject) — no check needed; the IR's typed values are
    // used as-is (module doc divergence: numeric-string leniency does not
    // apply here, since these accept numeric strings in legacy too).

    let (speed_min, speed_max) = resolve_speed_pair(object, index)?;

    Ok(ParticleSpec {
        name: common.name,
        scene_order: index,
        origin: common.origin,
        spawn_rate: particles::clamp_spawn_rate(f64::from(particle_ir.spawn_rate)),
        life: particles::clamp_life(f64::from(particle_ir.life)),
        speed_min,
        speed_max,
        direction: particles::clamp_direction(f64::from(particle_ir.direction)),
        spread: particles::clamp_spread(f64::from(particle_ir.spread)),
        gravity: [
            particles::clamp_gravity(f64::from(particle_ir.gravity[0])),
            particles::clamp_gravity(f64::from(particle_ir.gravity[1])),
        ],
        size_start: particles::clamp_size(f64::from(particle_ir.size_start)),
        size_end: particles::clamp_size(f64::from(particle_ir.size_end)),
        color_start: clamp_particle_color(particle_ir.color_start),
        color_end: clamp_particle_color(particle_ir.color_end),
        alpha_start: particles::clamp_alpha(f64::from(particle_ir.alpha_start)),
        alpha_end: particles::clamp_alpha(f64::from(particle_ir.alpha_end)),
        material: particle_ir.material.clone(),
        max_count: particles::clamp_max_count(u64::from(particle_ir.max_count)),
        blend_mode: common.blend_mode,
        alpha: common.alpha,
        visible: common.visible,
        brightness: common.brightness,
        texture: None,
        file_ref: None,
        component: None,
        instance_count: particles::clamp_instance_factor(
            f64::from(particle_ir.instance_count),
            1e6,
        ),
        instance_rate: particles::clamp_instance_factor(f64::from(particle_ir.instance_rate), 1e6),
        instance_size: particles::clamp_instance_factor(f64::from(particle_ir.instance_size), 1e6),
        instance_lifetime: particles::clamp_instance_factor(
            f64::from(particle_ir.instance_lifetime),
            1e6,
        ),
        instance_speed: particles::clamp_instance_factor(
            f64::from(particle_ir.instance_speed),
            1e6,
        ),
        instance_alpha: particles::clamp_instance_factor(
            f64::from(particle_ir.instance_alpha),
            1.0,
        ),
        instance_colorn: particle_ir.instance_colorn.clamp(0.0, 1.0),
        scale: common.scale,
    })
}

fn clamp_particle_color(color: [f32; 4]) -> [f32; 4] {
    let mut clamped = [1.0_f32; 4];
    for (slot, component) in clamped.iter_mut().zip(color.iter()) {
        *slot = particles::clamp_color_component(f64::from(*component));
    }
    clamped
}

/// `speed`/`speedMin`/`speedMax` — decision (c): deliberately untyped by
/// `ir.rs` (a cross-field default chain: `speedMin` defaults to `speed`'s
/// OWN resolved value, `speedMax` to `speedMin`'s), so this reads the
/// preserved RAW values straight out of the particle definition's residue
/// and reimplements `scene::parse_particle_system`'s exact `scalar`
/// closure + swap-if-reversed logic against them — same numbers, same
/// precedence, verbatim.
fn resolve_speed_pair(object: &ObjectIr, index: usize) -> Result<(f32, f32), SceneError> {
    let residue = particle_residue(object);
    let speed = residue_scalar(
        residue,
        "speed",
        f64::from(particles::DEFAULT_PARTICLE_SPEED),
        index,
        "particle.speed",
    )?;
    let speed_min = residue_scalar(residue, "speedMin", speed, index, "particle.speedMin")?;
    let speed_max = residue_scalar(residue, "speedMax", speed_min, index, "particle.speedMax")?;
    let speed_min_clamped = particles::clamp_speed(speed_min);
    let speed_max_clamped = particles::clamp_speed(speed_max);
    Ok(if speed_min_clamped <= speed_max_clamped {
        (speed_min_clamped, speed_max_clamped)
    } else {
        (speed_max_clamped, speed_min_clamped)
    })
}

/// Mirrors `scene::parse_particle_system`'s local `scalar` closure exactly:
/// a bounded float-or-numeric-string scalar read from the RAW (still
/// property-wrapped) residue value — missing takes `fallback` (itself
/// f64, since `speedMin`/`speedMax`'s own fallback is a SIBLING field's
/// already-resolved value, not a fixed constant), present-but-wrong-type
/// rejects.
fn residue_scalar(
    residue: Option<&Map<String, Value>>,
    key: &str,
    fallback: f64,
    index: usize,
    field: &str,
) -> Result<f64, SceneError> {
    let Some(value) = residue.and_then(|residue| residue.get(key)) else {
        return Ok(fallback);
    };
    let value = scene_property_value(value);
    if let Some(number) = value.as_f64() {
        return Ok(number);
    }
    if let Some(text) = value.as_str()
        && let Ok(number) = text.parse::<f64>()
    {
        return Ok(number);
    }
    Err(SceneError::new(
        SceneErrorKind::Shape,
        format!("scene.json \"objects[{index}].{field}\" must be a float or a numeric string"),
    ))
}

/// A `particle` object that classified as `ParticleFile` only because it
/// named no `texture`/`material` — legacy still parses it exactly like an
/// inline Particle-kind definition (`scene::parse_particle_system`'s
/// object branch), reimplemented here directly against the raw JSON
/// `ir.rs` left unconsumed for this exact case (see the caller's doc
/// comment). Mirrors `build_particle_system`'s clamp/reject choices
/// field-for-field, just reading straight from `Value`s instead of an
/// already-typed `ParticleIr`.
fn build_particle_system_from_raw(
    definition: &Map<String, Value>,
    common: Common,
    index: usize,
    file_ir: &kwe_core::ParticleFileIr,
) -> Result<ParticleSpec, SceneError> {
    let scalar = |key: &str, fallback: f32, field: &str| -> Result<f32, SceneError> {
        residue_scalar(Some(definition), key, f64::from(fallback), index, field).map(|v| v as f32)
    };
    let spawn_rate = particles::clamp_spawn_rate(f64::from(scalar(
        "spawnRate",
        particles::DEFAULT_PARTICLE_SPAWN_RATE,
        "particle.spawnRate",
    )?));
    let life = particles::clamp_life(f64::from(scalar(
        "life",
        particles::DEFAULT_PARTICLE_LIFE,
        "particle.life",
    )?));
    let speed = residue_scalar(
        Some(definition),
        "speed",
        f64::from(particles::DEFAULT_PARTICLE_SPEED),
        index,
        "particle.speed",
    )?;
    let speed_min = residue_scalar(
        Some(definition),
        "speedMin",
        speed,
        index,
        "particle.speedMin",
    )?;
    let speed_max = residue_scalar(
        Some(definition),
        "speedMax",
        speed_min,
        index,
        "particle.speedMax",
    )?;
    let speed_min = particles::clamp_speed(speed_min);
    let speed_max = particles::clamp_speed(speed_max);
    let (speed_min, speed_max) = if speed_min <= speed_max {
        (speed_min, speed_max)
    } else {
        (speed_max, speed_min)
    };
    let direction = particles::clamp_direction(f64::from(scalar(
        "direction",
        particles::DEFAULT_PARTICLE_DIRECTION,
        "particle.direction",
    )?));
    let spread = particles::clamp_spread(f64::from(scalar(
        "spread",
        particles::DEFAULT_PARTICLE_SPREAD,
        "particle.spread",
    )?));
    let size_start = particles::clamp_size(f64::from(scalar(
        "sizeStart",
        particles::DEFAULT_PARTICLE_SIZE,
        "particle.sizeStart",
    )?));
    let size_end = particles::clamp_size(f64::from(scalar(
        "sizeEnd",
        particles::DEFAULT_PARTICLE_SIZE,
        "particle.sizeEnd",
    )?));
    let alpha_start = particles::clamp_alpha(f64::from(scalar(
        "alphaStart",
        particles::DEFAULT_PARTICLE_ALPHA_START,
        "particle.alphaStart",
    )?));
    let alpha_end = particles::clamp_alpha(f64::from(scalar(
        "alphaEnd",
        particles::DEFAULT_PARTICLE_ALPHA_END,
        "particle.alphaEnd",
    )?));

    let gravity = match definition.get("gravity") {
        None => particles::DEFAULT_PARTICLE_GRAVITY,
        Some(value) => {
            let tokens = raw_vector(scene_property_value(value), &[1, 2, 3])
                .ok_or_else(|| reject(index, "particle.gravity"))?;
            match tokens.as_slice() {
                [g] => [0.0, *g as f32],
                [x, y] | [x, y, _] => [*x as f32, *y as f32],
                _ => unreachable!("raw_vector enforces the allowed lengths"),
            }
        }
    };
    let color_start = raw_particle_color(definition, "colorStart", index)?;
    let color_end = raw_particle_color(definition, "colorEnd", index)?;

    let max_count = match definition.get("maxCount") {
        None => particles::DEFAULT_PARTICLE_MAX_COUNT,
        Some(value) => {
            let value = scene_property_value(value);
            let count = if let Some(number) = value.as_u64() {
                number
            } else if let Some(text) = value.as_str() {
                text.parse::<u64>()
                    .map_err(|_| reject(index, "particle.maxCount"))?
            } else {
                return Err(reject(index, "particle.maxCount"));
            };
            particles::clamp_max_count(count)
        }
    };

    let material = definition
        .get("texture")
        .or_else(|| definition.get("material"))
        .map(scene_property_value)
        .and_then(Value::as_str)
        .map(str::to_string);

    // `instanceoverride` is a SIBLING of `particle` on the object, not a
    // key of `definition` — `file_ir` already carries it, typed, read the
    // same way for every shape of `particle` (see `ParticleFileIr`'s doc
    // comment). Clamped here, not read raw — see `clamp_instance_overrides`.
    let (
        instance_count,
        instance_rate,
        instance_size,
        instance_lifetime,
        instance_speed,
        instance_alpha,
        instance_colorn,
    ) = clamp_instance_overrides(file_ir);

    Ok(ParticleSpec {
        name: common.name,
        scene_order: index,
        origin: common.origin,
        spawn_rate,
        life,
        speed_min,
        speed_max,
        direction,
        spread,
        gravity: [
            particles::clamp_gravity(f64::from(gravity[0])),
            particles::clamp_gravity(f64::from(gravity[1])),
        ],
        size_start,
        size_end,
        color_start,
        color_end,
        alpha_start,
        alpha_end,
        material,
        max_count,
        blend_mode: common.blend_mode,
        alpha: common.alpha,
        visible: common.visible,
        brightness: common.brightness,
        texture: None,
        file_ref: None,
        component: None,
        instance_count,
        instance_rate,
        instance_size,
        instance_lifetime,
        instance_speed,
        instance_alpha,
        instance_colorn,
        scale: common.scale,
    })
}

fn raw_particle_color(
    definition: &Map<String, Value>,
    key: &str,
    index: usize,
) -> Result<[f32; 4], SceneError> {
    match definition.get(key) {
        None => Ok([1.0, 1.0, 1.0, 1.0]),
        Some(value) => {
            let tokens = raw_vector(scene_property_value(value), &[3, 4])
                .ok_or_else(|| reject(index, &format!("particle.{key}")))?;
            let mut color = [1.0_f32; 4];
            for (slot, component) in color.iter_mut().zip(tokens.iter()) {
                *slot = particles::clamp_color_component(*component);
            }
            Ok(color)
        }
    }
}

/// Mirrors `scene.rs`'s own `parse_vector` exactly: a space-separated
/// string or a JSON array of numbers (NEVER a numeric-string per element
/// — module doc divergence #1 does not apply inside an array here either,
/// matching legacy precisely since this reads raw `Value`s directly, not
/// through `ir.rs`'s more lenient `as_vector`), arity in `allowed`, every
/// component finite and within ±`layers::MAX_LAYER_VALUE`.
fn raw_vector(value: &Value, allowed: &[usize]) -> Option<Vec<f64>> {
    let tokens: Vec<f64> = if let Some(text) = value.as_str() {
        let mut out = Vec::new();
        for token in text.split_whitespace() {
            out.push(token.parse::<f64>().ok()?);
        }
        out
    } else {
        let array = value.as_array()?;
        array
            .iter()
            .map(Value::as_f64)
            .collect::<Option<Vec<f64>>>()?
    };
    if !allowed.contains(&tokens.len()) {
        return None;
    }
    if tokens
        .iter()
        .any(|token| !token.is_finite() || token.abs() > layers::MAX_LAYER_VALUE)
    {
        return None;
    }
    Some(tokens)
}
