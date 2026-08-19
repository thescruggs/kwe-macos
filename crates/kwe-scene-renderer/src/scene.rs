// SPDX-License-Identifier: Apache-2.0
// Scene.json parsing for the M3a slice of the original SceneScript engine.
//
// The M3a worker understands exactly one input: a scene.json file laid out per
// docs/SCENE_FORMAT_V1.md. Everything the engine needs is either in the JSON
// (`general.clearcolor`) or referenced from it (`general.script`, resolved
// relative to the scene's content root). Unknown keys are tolerated so that
// real wallpaper packages never make the worker reject a scene; the M3c
// slice interprets the root `objects` array as image layers (models,
// particles, audio, text and the `effects`/`properties` sections stay
// M3d+).
//
// Every read is bounded: the scene.json file is capped at 16 MiB (the daemon's
// preflight uses the same bound) and the referenced script at 2 MiB.

use std::error::Error as StdError;
use std::fmt;
use std::fs;
use std::path::{Component, Path, PathBuf};

use serde_json::Value;

use crate::layers::{MAX_LAYER_VALUE, MAX_LAYERS};
use crate::text::{HorizontalAlign, VerticalAlign};
use crate::textures::DecodedTexture;

/// Cap on the raw scene.json bytes. Single source of truth in kwe-core
/// (crates/kwe-core/src/pkg.rs), where pkg preflight enforces the same cap
/// statically (preflight/worker parity, M3b review follow-up).
pub use kwe_core::{MAX_SCENE_JSON_BYTES, MAX_SCRIPT_BYTES};
/// The frame protocol's dimension cap (crates/kwe-frame-protocol).
pub const MAX_DIMENSION: u32 = 8192;

/// Which part of loading a scene failed; all kinds are backend rejects (exit
/// 73) in M3a, but the distinction shows up in diagnostics and unit tests.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SceneErrorKind {
    /// File system errors (missing file, permission, I/O).
    Read,
    /// JSON syntax errors or a root that is not an object.
    Json,
    /// Schema violations: wrong types, out-of-range values, `.pkg` scenes.
    Shape,
    /// Script reference problems: traversal outside the content root,
    /// missing script file, script too large, or a non-.js extension.
    Script,
}

#[derive(Debug)]
pub struct SceneError {
    pub kind: SceneErrorKind,
    pub message: String,
}

impl SceneError {
    pub fn new(kind: SceneErrorKind, message: impl Into<String>) -> Self {
        Self {
            kind,
            message: message.into(),
        }
    }
}

impl fmt::Display for SceneError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{}", self.message)
    }
}

impl StdError for SceneError {}

impl From<std::io::Error> for SceneError {
    fn from(e: std::io::Error) -> Self {
        Self::new(SceneErrorKind::Read, format!("scene file I/O error: {e}"))
    }
}

impl From<serde_json::Error> for SceneError {
    fn from(e: serde_json::Error) -> Self {
        Self::new(
            SceneErrorKind::Json,
            format!("scene.json is not valid JSON: {e}"),
        )
    }
}

/// The interpreted part of a scene.json the M3a worker needs.
#[derive(Debug, Clone)]
pub struct SceneConfig {
    /// RGBA straight-alpha clear color from `general.clearcolor` (0..=1).
    /// Defaults to opaque black when absent.
    pub clear_color: [f32; 4],
    /// Absolute path of the scene's script, if the scene has one that passed
    /// every check. The caller decides whether to run it.
    pub script_path: Option<PathBuf>,
    /// Index of the `general.script` entry inside the package table, when
    /// the scene was parsed from a scene.pkg (M3b). Mutually exclusive with
    /// `script_path`; the caller extracts the entry and sets `script_path`.
    pub script_entry: Option<usize>,
    /// Raw `general.script` string, before resolution against either the
    /// content root (file scenes) or the package table (pkg scenes).
    script_reference: Option<String>,
    /// Optional `general.resolution` (the worker still renders at the size
    /// the daemon asked for; the field is only validated and reported).
    pub resolution: Option<(u32, u32)>,
    /// Optional `general.fps` hint (unused by the worker in M3a; the daemon
    /// owns the pacing).
    pub fps: Option<f32>,
    /// The `objects` array interpreted as image and text layers, in
    /// scene.json order (M3c image, M3e text). Every other object kind
    /// (models, particles, audio — M3d+) is ignored. The loader resolves
    /// and decodes each layer's `image` and fills `texture`.
    pub layers: Vec<LayerSpec>,
    /// Text objects skipped because the scene declares more than
    /// text::MAX_TEXT_LAYERS of them. Never a rejection — the extra layers
    /// just do not register; counted for the worker's one-time diagnostic.
    pub text_layer_skips: usize,
    /// Objects carrying both `image` and `text` (image wins per the M3c
    /// rule; counted for the worker's one-time diagnostic).
    pub text_on_image_objects: usize,
    /// Text layers that also wrote a `size` field (ignored — text size is
    /// automatic; counted for the worker's one-time diagnostic).
    pub text_size_ignored: usize,
}

/// One `objects` entry interpreted as an image layer (M3c). Parsed fields
/// follow the researched wallpaper-engine schema (docs/SCENE_FORMAT_V1.md,
/// M3c section): vectors may be the space-separated string form the editor
/// writes or arrays; every numeric is finite and bounded to ±1e6.
#[derive(Debug, Clone)]
pub struct LayerSpec {
    /// The script's identity for this layer (`Scene.getLayer(name)`).
    pub name: String,
    /// Raw `image` reference exactly as written: a path relative to the
    /// content root (file scenes) or a package entry path (pkg scenes).
    /// `None` when the field is present but not a string (a
    /// property-wrapped `{"user": ..., "value": ...}` reference whose
    /// value is not a string): the layer is still registered so the script
    /// can reach it, but skipped at load with a bounded diagnostic.
    pub image: Option<String>,
    /// Position in scene units (pixels); (0,0) is the scene center, +y
    /// down (researched WE origin semantics).
    pub origin: [f32; 2],
    /// Euler angles in **degrees** (the WE script API unit). The file
    /// stores radians — the parse converts (corpus-verified: exact π
    /// values, none at 90/180). Only z rotates 2D layers in M3c.
    pub angles: [f32; 3],
    /// Relative scale; 1.0 = original size (WE semantics).
    pub scale: [f32; 2],
    /// Size in scene units (pixels); [0, 0] (absent or zero) means "the
    /// texture's decoded dimensions", substituted at load.
    pub size: [f32; 2],
    /// Straight alpha in 0..=1; default 1.0 (WE default).
    pub alpha: f32,
    /// Default true (WE default).
    pub visible: bool,
    /// `colorBlendMode` (the corpus key) or `blendMode` (the brief's key)
    /// exactly as written, pre-clamp. The runtime layer clamps it to the
    /// implemented set (layers.rs); the known-unimplemented corpus values
    /// (11/12/24/30) get a bounded one-time diagnostic from the worker at
    /// load; anything else is tolerated silently (rendering src-over, the
    /// M3c behavior).
    pub blend_mode: u32,
    /// Brightness multiplier on the sampled RGB (M3d). The WE default is
    /// 1.0 — the identity, verified on the OWE WPImageObject parse; the
    /// clamp range 0..=10 is a design decision, not a documented WE bound.
    pub brightness: f32,
    /// Tint multiplier on the sampled RGBA (M3d): 0..=1 per component,
    /// default [1, 1, 1, 1]. The WE file key is `color` (a vec3 — alpha
    /// defaults to 1); the brief's `tint` (3 or 4 components) takes
    /// precedence when both are present.
    pub tint: [f32; 4],
    /// Decoded RGBA8 texture, filled by the loader; `None` when the image
    /// is missing, unreadable, over budget, or not a decodable format. The
    /// layer then stays registered (script-visible) but draws nothing.
    pub texture: Option<DecodedTexture>,
    /// Text-layer content (M3e), `Some` exactly when the object was a text
    /// layer (has `text`, no `image`). The layer then draws through the
    /// text path: a per-layer glyph atlas + a quad whose vertex data the
    /// worker rebuilds on change (text.rs).
    pub text: Option<TextSpec>,
}

/// One `objects` entry interpreted as a text layer (M3e). Field names
/// follow the researched WE/OEW text-object schema (docs/SCENE_FORMAT_V1.md,
/// M3e section): the file key is `pointsize` (points; the engine multiplies
/// by 4), alignment is `horizontalalign`/`verticalalign` (both default
/// "center"), and the color is `color` (a vec3; alpha implied 1.0) —
/// `fontsize` is not a WE key. Text layers render at their automatic size
/// (a `size` field is ignored, see `text_size_ignored`); `scale` still
/// scales the layer, `origin` positions it.
#[derive(Debug, Clone)]
pub struct TextSpec {
    /// The string to render. Capped at text::MAX_TEXT_CHARS chars when the
    /// layout runs (text.rs); the parser keeps the raw value so the
    /// worker's one-time truncation diagnostic reports the truth. Defaults
    /// to "" when the field is absent or not a string (renders nothing).
    pub text: String,
    /// Requested font family (WE `font`, optionally `systemfont_`-prefixed
    /// or an absolute/basename path). None = the resolver's default.
    pub font: Option<String>,
    /// Effective pixel em size, clamped to text::MIN_FONT_PX..=MAX_FONT_PX.
    pub pointsize: f32,
    pub horizontal_align: HorizontalAlign,
    pub vertical_align: VerticalAlign,
    /// RGBA multiplier, 0..=1 each, default opaque white (WE `color` is a
    /// vec3; the 4th component is accepted and defaults to 1). The color
    /// drives the pipeline's tint slot; alpha composes with `alpha` per
    /// the M3d alpha policy.
    pub color: [f32; 4],
    /// Whether the scene wrote a `size` for this text layer (ignored).
    pub has_size: bool,
    // NOTE: the `objects` common props (origin/angles/scale/alpha/visible/
    // blend/brightness) live on the LayerSpec for every layer kind, text
    // included; TextSpec deliberately does not duplicate them (the worker
    // reads them off LayerState, which mirrors LayerSpec — see layers.rs).
}

impl SceneConfig {
    /// Parse `path` and resolve the script reference, if any.
    ///
    /// Rejections (all `SceneError`):
    /// * file unreadable or larger than `MAX_SCENE_JSON_BYTES` → Read
    /// * invalid JSON or non-object root → Json
    /// * `general` not an object, `clearcolor`/`resolution`/`fps` wrong
    ///   types, values out of range, or `script` present but not a string
    ///   → Shape
    /// * script escaping the content root, missing, > 2 MiB, or not ending
    ///   in `.js` → Script
    pub fn parse(path: &Path) -> Result<SceneConfig, SceneError> {
        let root = canonical_root(path)?;
        let bytes = read_bounded(path, MAX_SCENE_JSON_BYTES)?;
        let parsed = parse_scene_json(&bytes)?;
        let script_path = match &parsed.script_reference {
            None => None,
            Some(reference) => Some(resolve_script(&root, reference)?),
        };
        Ok(SceneConfig {
            script_path,
            ..parsed
        })
    }

    /// Parse the scene.json bytes extracted from a scene.pkg (M3b) and
    /// resolve `general.script` against the package's entry table instead
    /// of the file system. `script_entry` names the entry to extract; the
    /// caller must extract it into a private directory and set
    /// `script_path` (nothing is ever resolved against the host file
    /// system from a package).
    pub fn parse_pkg(
        bytes: &[u8],
        entries: &[kwe_core::PkgEntry],
    ) -> Result<SceneConfig, SceneError> {
        let parsed = parse_scene_json(bytes)?;
        let script_entry = match &parsed.script_reference {
            None => None,
            Some(reference) => Some(resolve_pkg_script(reference, entries)?),
        };
        Ok(SceneConfig {
            script_entry,
            ..parsed
        })
    }
}

/// Shared JSON interpretation core for file and pkg scenes. The script
/// reference is left unresolved (`script_reference`); the two entry points
/// resolve it against their own root: the content directory (file scenes)
/// or the package entry table (pkg scenes).
fn parse_scene_json(bytes: &[u8]) -> Result<SceneConfig, SceneError> {
    let value: Value = serde_json::from_slice(bytes)?;
    let root_obj = value.as_object().ok_or_else(|| {
        SceneError::new(SceneErrorKind::Json, "scene.json root must be an object")
    })?;

    let empty_general = serde_json::Map::new();
    let general = match root_obj.get("general") {
        None | Some(Value::Null) => &empty_general,
        Some(Value::Object(g)) => g,
        Some(_) => {
            return Err(SceneError::new(
                SceneErrorKind::Shape,
                "scene.json \"general\" must be an object",
            ));
        }
    };

    let clear_color = parse_clear_color(general)?;
    let resolution = parse_resolution(general)?;
    let fps = parse_fps(general)?;
    let (layers, counts) = parse_objects(root_obj)?;

    let script_reference = match general.get("script") {
        None | Some(Value::Null) => None,
        Some(Value::String(s)) => Some(s.clone()),
        Some(_) => {
            return Err(SceneError::new(
                SceneErrorKind::Shape,
                "scene.json \"general.script\" must be a string path relative to the scene",
            ));
        }
    };

    Ok(SceneConfig {
        clear_color,
        script_path: None,
        script_entry: None,
        script_reference,
        resolution,
        fps,
        layers,
        text_layer_skips: counts.text_layer_skips,
        text_on_image_objects: counts.text_on_image_objects,
        text_size_ignored: counts.text_size_ignored,
    })
}

/// Bounded counts the parser collects while interpreting `objects` (M3e):
/// never rejections, only one-time worker diagnostics.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct ObjectCounts {
    pub text_layer_skips: usize,
    pub text_on_image_objects: usize,
    pub text_size_ignored: usize,
}

/// The `objects` array, interpreted as image (M3c) and text (M3e) layers
/// in scene.json order (the compositor's draw order — the layer on top is
/// drawn last, over the others). An object is an image layer exactly when
/// it carries an `image` field; a text layer exactly when it carries a
/// `text` field without `image` (an object with both counts as image —
/// `text_on_image_objects` — and an object with neither is ignored:
/// models, particles, audio — M3d+). A reference that ends in `.json` is a
/// model instance under the WE solid-model architecture (620 of the 685
/// corpus image references point at model files; the other 65 carry a null
/// image value) — skipped BEFORE any validation, so a malformed model
/// layer (no name, out-of-range alpha, ...) can never reject the scene,
/// and it is not counted toward the layer cap, until models arrive (M3h).
///
/// Text layers beyond text::MAX_TEXT_LAYERS are skipped (counted, never a
/// rejection); both caps (MAX_TEXT_LAYERS and MAX_LAYERS) apply to the
/// layers that register.
fn parse_objects(
    root_obj: &serde_json::Map<String, Value>,
) -> Result<(Vec<LayerSpec>, ObjectCounts), SceneError> {
    let Some(value) = root_obj.get("objects") else {
        return Ok((Vec::new(), ObjectCounts::default()));
    };
    let array = value.as_array().ok_or_else(|| {
        SceneError::new(
            SceneErrorKind::Shape,
            "scene.json \"objects\" must be an array",
        )
    })?;
    let mut layers = Vec::new();
    let mut text_layers = 0usize;
    let mut counts = ObjectCounts::default();
    for (index, entry) in array.iter().enumerate() {
        let object = entry.as_object().ok_or_else(|| {
            SceneError::new(
                SceneErrorKind::Shape,
                format!("scene.json \"objects[{index}]\" must be an object"),
            )
        })?;
        if object.contains_key("image") {
            if object.contains_key("text") {
                counts.text_on_image_objects += 1;
            }
            // A model instance: WE stores every visual (2D included) as a
            // model; model layers are M3h. The scene renders without it.
            // The check runs on the raw (property-unwrapped) reference
            // BEFORE parse_image_layer, so a malformed model layer skips
            // like any model layer instead of rejecting the whole scene.
            if property_value(object.get("image").expect("caller checked"))
                .as_str()
                .is_some_and(|image| image.to_ascii_lowercase().ends_with(".json"))
            {
                continue;
            }
            let layer = parse_image_layer(object, index)?;
            layers.push(layer);
        } else if object.contains_key("text") {
            if text_layers >= crate::text::MAX_TEXT_LAYERS {
                counts.text_layer_skips += 1;
                continue;
            }
            text_layers += 1;
            let layer = parse_text_layer(object, index)?;
            if layer.text.as_ref().is_some_and(|spec| spec.has_size) {
                counts.text_size_ignored += 1;
            }
            layers.push(layer);
        } else {
            continue; // particles, audio, ... — M3d+
        }
    }
    if layers.len() > MAX_LAYERS {
        return Err(SceneError::new(
            SceneErrorKind::Shape,
            format!(
                "scene.json \"objects\" has {} image layers, over the {MAX_LAYERS} layer cap",
                layers.len()
            ),
        ));
    }
    Ok((layers, counts))
}

/// The layer properties image (M3c) and text (M3e) layers share: name,
/// origin, angles (file radians → API degrees), scale, alpha, visible,
/// blend mode, brightness. Defaults, clamps, and error messages mirror the
/// original parse_image_layer code exactly; `kind` only labels the name
/// error ("image layers" / "text layers").
#[derive(Debug, Clone)]
struct CommonProps {
    name: String,
    origin: [f32; 2],
    angles: [f32; 3],
    scale: [f32; 2],
    alpha: f32,
    visible: bool,
    blend_mode: u32,
    brightness: f32,
}

fn parse_common_props(
    object: &serde_json::Map<String, Value>,
    index: usize,
    kind: &str,
) -> Result<CommonProps, SceneError> {
    let name = match object.get("name") {
        Some(Value::String(name)) => name.clone(),
        None | Some(Value::Null) => {
            return Err(SceneError::new(
                SceneErrorKind::Shape,
                format!("scene.json \"objects[{index}].name\" is required for {kind} layers"),
            ));
        }
        Some(_) => {
            return Err(SceneError::new(
                SceneErrorKind::Shape,
                format!("scene.json \"objects[{index}].name\" must be a string"),
            ));
        }
    };

    // Property-wrapped values (`{"user": ..., "value": ...}`) are how the
    // editor serializes user-bindable fields — corpus re-scan: 70%
    // (315/447) of image layers carrying alpha and 49% (276/568) of those
    // carrying visible are wrapped. The initial `value` is the behavior
    // until user properties arrive (M3j); the wrapper is unwrapped here,
    // and a wrapped scalar without a value rejects like any malformed
    // scalar.
    let origin = match object.get("origin") {
        None => [0.0, 0.0],
        Some(value) => {
            let vector = parse_vector(
                property_value(value),
                &field(index, "origin"),
                &[2, 3],
                false,
            )?;
            [vector[0], vector[1]] // z is unused by 2D rendering in M3c
        }
    };

    // The file stores radians (corpus: exact π, none at 90/180); the script
    // API speaks degrees, and so does the runtime model — convert here.
    let angles = match object.get("angles") {
        None => [0.0, 0.0, 0.0],
        Some(value) => {
            let mut vector = parse_vector(
                property_value(value),
                &field(index, "angles"),
                &[2, 3],
                false,
            )?;
            for angle in &mut vector {
                *angle = angle.to_degrees();
            }
            match vector.as_slice() {
                [x, y] => [*x, *y, 0.0],
                [x, y, z] => [*x, *y, *z],
                _ => unreachable!("parse_vector enforces the allowed lengths"),
            }
        }
    };

    let scale = match object.get("scale") {
        None => [1.0, 1.0],
        Some(value) => {
            let vector = parse_vector(
                property_value(value),
                &field(index, "scale"),
                &[2, 3],
                false,
            )?;
            [vector[0], vector[1]]
        }
    };

    let alpha = match object.get("alpha") {
        None => 1.0,
        Some(value) => {
            let alpha = property_value(value).as_f64().ok_or_else(|| {
                SceneError::new(
                    SceneErrorKind::Shape,
                    format!("scene.json \"{}\" must be a float", field(index, "alpha")),
                )
            })?;
            if !alpha.is_finite() || !(0.0..=1.0).contains(&alpha) {
                return Err(SceneError::new(
                    SceneErrorKind::Shape,
                    format!(
                        "scene.json \"{}\" must be between 0.0 and 1.0",
                        field(index, "alpha")
                    ),
                ));
            }
            alpha as f32
        }
    };

    let visible = match object.get("visible") {
        None => true,
        Some(value) => match property_value(value) {
            Value::Bool(visible) => *visible,
            _ => {
                return Err(SceneError::new(
                    SceneErrorKind::Shape,
                    format!(
                        "scene.json \"{}\" must be a boolean",
                        field(index, "visible")
                    ),
                ));
            }
        },
    };

    // `colorBlendMode` is the corpus key (all observed occurrences);
    // `blendMode` is accepted as an alias. The raw value is kept for the
    // worker's bounded diagnostic; the runtime clamps to the implemented
    // set (layers.rs BlendMode). Malformed values are tolerated as 0 — not
    // worth a rejection: the layer renders src-over.
    let blend_mode = match object
        .get("blendMode")
        .or_else(|| object.get("colorBlendMode"))
    {
        None => 0,
        Some(value) => match property_value(value).as_f64() {
            Some(mode) if mode.is_finite() && mode >= 0.0 && mode <= f64::from(u32::MAX) => {
                mode as u32
            }
            _ => 0,
        },
    };

    // M3d color effects, both property-wrapped like the M3c scalars. WE
    // writes `brightness` as a plain float (default 1.0) — parsed with the
    // same clamps as the script-side writes (0..=10, non-finite → 1.0). A
    // numeric string is accepted too (the corpus editor serializes
    // scalars as strings); any other type rejects like alpha. Brightness
    // beyond the range is clamped, not rejected (a >10 boost is not worth
    // failing the scene).
    let brightness = match object.get("brightness") {
        None => 1.0,
        Some(value) => {
            let value = property_value(value);
            let brightness = if let Some(number) = value.as_f64() {
                number
            } else if let Some(text) = value.as_str() {
                text.parse::<f64>().map_err(|_| {
                    SceneError::new(
                        SceneErrorKind::Shape,
                        format!(
                            "scene.json \"{}\" must be a float or a numeric string",
                            field(index, "brightness")
                        ),
                    )
                })?
            } else {
                return Err(SceneError::new(
                    SceneErrorKind::Shape,
                    format!(
                        "scene.json \"{}\" must be a float or a numeric string",
                        field(index, "brightness")
                    ),
                ));
            };
            crate::layers::clamp_layer_brightness(brightness)
        }
    };

    Ok(CommonProps {
        name,
        origin,
        angles,
        scale,
        alpha,
        visible,
        blend_mode,
        brightness,
    })
}

/// One image layer entry. Numeric out-of-range values reject the whole
/// scene (like clearcolor); an unresolvable image never does (the task's
/// policy — a missing image skips the layer, not the scene).
fn parse_image_layer(
    object: &serde_json::Map<String, Value>,
    index: usize,
) -> Result<LayerSpec, SceneError> {
    let common = parse_common_props(object, index, "image")?;

    // Property-wrapped values (`{"user": ..., "value": ...}`) are how the
    // editor serializes user-bindable fields — corpus re-scan: 70%
    // (315/447) of image layers carrying alpha and 49% (276/568) of those
    // carrying visible are wrapped. The initial `value` is the behavior
    // until user properties arrive (M3j); the wrapper is unwrapped here,
    // and a wrapped scalar without a value rejects like any malformed
    // scalar.
    let image = match property_value(object.get("image").expect("caller checked")) {
        Value::String(reference) => Some(reference.clone()),
        _ => None,
    };

    let size = match object.get("size") {
        None => [0.0, 0.0],
        Some(value) => {
            let vector = parse_vector(property_value(value), &field(index, "size"), &[2], true)?;
            [vector[0], vector[1]]
        }
    };

    // `tint` (the brief's key, 3 or 4 components) takes precedence over
    // `color` (the WE file key, a vec3 — its alpha is implied 1.0, the
    // g_Color4 semantics of WE's shader library). Components clamp 0..=1.
    let tint = match object.get("tint").or_else(|| object.get("color")) {
        None => [1.0, 1.0, 1.0, 1.0],
        Some(value) => {
            let vector =
                parse_vector(property_value(value), &field(index, "tint"), &[3, 4], false)?;
            let mut tint = [1.0, 1.0, 1.0, 1.0];
            for (slot, component) in tint.iter_mut().zip(vector.iter()) {
                *slot = crate::layers::clamp_layer_tint(f64::from(*component));
            }
            tint
        }
    };

    Ok(LayerSpec {
        name: common.name,
        image,
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
        text: None,
    })
}

/// One text layer entry (M3e). Shared props come from parse_common_props;
/// text-only keys follow the OWE text-object schema: `text` (plain string
/// or property-wrapped), `font` (family / systemfont_ alias / path),
/// `pointsize` (points, ×4 to pixels — the WE key; `fontsize` is not a WE
/// key), `horizontalalign` / `verticalalign` (falling back to `alignment`
/// like OWE's align_or_default, default "center"), and `color` (vec3/vec4
/// multiplier). A `size` field is tolerated and ignored (counted via
/// `text_size_ignored`); a missing/blank `text` renders nothing.
fn parse_text_layer(
    object: &serde_json::Map<String, Value>,
    index: usize,
) -> Result<LayerSpec, SceneError> {
    let common = parse_common_props(object, index, "text")?;

    let text = match property_value(object.get("text").expect("caller checked")) {
        Value::String(s) => s.clone(),
        _ => String::new(),
    };
    let font = match object.get("font").map(property_value) {
        Some(Value::String(s)) if !s.is_empty() => Some(s.clone()),
        _ => None,
    };

    // OWE TextPointSizeToPx: px = round(pointsize * 4.0), clamped to our
    // bounded range (4..=512 px; OWE clamps 1..=1024 — documented
    // deviation). Non-finite or ≤ 0 sizes use the default (12 pt -> 48 px).
    let pointsize = match object.get("pointsize") {
        None => crate::text::DEFAULT_POINT_SIZE * crate::text::POINT_TO_PX,
        Some(value) => {
            let value = property_value(value);
            let pointsize = if let Some(number) = value.as_f64() {
                number
            } else if let Some(text) = value.as_str() {
                text.parse::<f64>().map_err(|_| {
                    SceneError::new(
                        SceneErrorKind::Shape,
                        format!(
                            "scene.json \"{}\" must be a float or a numeric string",
                            field(index, "pointsize")
                        ),
                    )
                })?
            } else {
                return Err(SceneError::new(
                    SceneErrorKind::Shape,
                    format!(
                        "scene.json \"{}\" must be a float or a numeric string",
                        field(index, "pointsize")
                    ),
                ));
            };
            crate::text::pointsize_to_px(pointsize)
        }
    };

    // OWE align_or_default: the horizontal/vertical keys win when
    // non-empty; otherwise `alignment` is consulted for the polarity word;
    // final default "center" on both axes (researched OWE parser).
    let horizontal_align = parse_text_align(object, "horizontalalign", "left", "right");
    let vertical_align = parse_text_align_v(object, "verticalalign");

    // WE `color` for text is a vec3 (alpha implied 1.0); a 4th component
    // is accepted. Components clamp 0..=1; default opaque white.
    let color = match object.get("color") {
        None => [1.0, 1.0, 1.0, 1.0],
        Some(value) => {
            let vector = parse_vector(
                property_value(value),
                &field(index, "color"),
                &[3, 4],
                false,
            )?;
            let mut color = [1.0, 1.0, 1.0, 1.0];
            for (slot, component) in color.iter_mut().zip(vector.iter()) {
                *slot = crate::layers::clamp_layer_tint(f64::from(*component));
            }
            color
        }
    };

    let has_size = object.contains_key("size");

    Ok(LayerSpec {
        name: common.name.clone(),
        image: None,
        origin: common.origin,
        angles: common.angles,
        scale: common.scale,
        size: [1.0, 1.0], // text renders at layout size; scale does the resizing
        alpha: common.alpha,
        visible: common.visible,
        blend_mode: common.blend_mode,
        brightness: common.brightness,
        tint: color,
        texture: None,
        text: Some(TextSpec {
            text,
            font,
            pointsize,
            horizontal_align,
            vertical_align,
            color,
            has_size,
        }),
    })
}

/// OWE align_or_default for one axis: the `key` value (property-wrapped
/// allowed) wins when non-empty; otherwise the `alignment` field is
/// consulted for the polarity words; default "center". The returned
/// HorizontalAlign doubles as the vertical polarity (Top/Bottom) through
/// `parse_text_align_v`.
fn parse_text_align(
    object: &serde_json::Map<String, Value>,
    key: &str,
    negative: &str,
    positive: &str,
) -> HorizontalAlign {
    // Exact words go through the enum parser; combined alignment strings
    // ("top-left") fall back to polarity containment.
    let polarity = |s: &str| -> HorizontalAlign {
        let s = s.to_ascii_lowercase();
        if s.contains(negative) {
            HorizontalAlign::Left
        } else if s.contains(positive) {
            HorizontalAlign::Right
        } else {
            HorizontalAlign::Center
        }
    };
    let resolve =
        |s: &str| -> HorizontalAlign { HorizontalAlign::parse(s).unwrap_or_else(|| polarity(s)) };
    if let Some(Some(value)) = object.get(key).map(property_value).map(Value::as_str)
        && !value.is_empty()
    {
        return resolve(value);
    }
    if let Some(Some(alignment)) = object
        .get("alignment")
        .map(property_value)
        .map(Value::as_str)
    {
        return resolve(alignment);
    }
    HorizontalAlign::Center
}

/// Vertical counterpart of parse_text_align (`top`/`bottom` polarity).
fn parse_text_align_v(object: &serde_json::Map<String, Value>, key: &str) -> VerticalAlign {
    // Exact vertical words first; combined strings ("top-left") fall
    // through to the shared polarity parser (top -> Left -> Top).
    if let Some(Some(value)) = object.get(key).map(property_value).map(Value::as_str)
        && !value.is_empty()
        && let Some(align) = VerticalAlign::parse(value)
    {
        return align;
    }
    match parse_text_align(object, key, "top", "bottom") {
        HorizontalAlign::Left => VerticalAlign::Top,
        HorizontalAlign::Right => VerticalAlign::Bottom,
        HorizontalAlign::Center => VerticalAlign::Center,
    }
}

fn field(index: usize, name: &str) -> String {
    format!("objects[{index}].{name}")
}

/// Unwrap a property-wrapped value (`{"user": ..., "value": ...}`) to its
/// `value`; anything else passes through unchanged.
fn property_value(value: &Value) -> &Value {
    match value.as_object().and_then(|object| object.get("value")) {
        Some(inner) => inner,
        None => value,
    }
}

/// Parse a WE vector field: the space-separated string form the editor
/// writes (`"1920.00000 1080.00000 0.00000"` — verified on the corpus) or
/// an array of numbers. `allowed` lists the accepted component counts (the
/// editor writes three; two is accepted, and the extra z is dropped by the
/// caller). Every component must be finite and within ±1e6; `non_negative`
/// additionally forbids negative values (sizes — a mirror goes through
/// scale, per WE semantics).
fn parse_vector(
    value: &Value,
    field: &str,
    allowed: &[usize],
    non_negative: bool,
) -> Result<Vec<f32>, SceneError> {
    let tokens: Vec<f64> = if let Some(text) = value.as_str() {
        let mut out = Vec::new();
        for token in text.split_whitespace() {
            let number = token.parse::<f64>().map_err(|_| {
                SceneError::new(
                    SceneErrorKind::Shape,
                    format!("scene.json \"{field}\" must contain only floats, got \"{token}\""),
                )
            })?;
            out.push(number);
        }
        out
    } else if let Some(array) = value.as_array() {
        array
            .iter()
            .map(|entry| {
                entry.as_f64().ok_or_else(|| {
                    SceneError::new(
                        SceneErrorKind::Shape,
                        format!("scene.json \"{field}\" entries must be floats"),
                    )
                })
            })
            .collect::<Result<Vec<f64>, SceneError>>()?
    } else {
        return Err(SceneError::new(
            SceneErrorKind::Shape,
            format!(
                "scene.json \"{field}\" must be an array of floats or a space-separated string"
            ),
        ));
    };
    if !allowed.contains(&tokens.len()) {
        let accepted = allowed
            .iter()
            .map(|n| n.to_string())
            .collect::<Vec<_>>()
            .join(" or ");
        return Err(SceneError::new(
            SceneErrorKind::Shape,
            format!(
                "scene.json \"{field}\" must have {accepted} components, found {}",
                tokens.len()
            ),
        ));
    }
    let mut out = Vec::with_capacity(tokens.len());
    for (i, token) in tokens.iter().enumerate() {
        if !token.is_finite() || token.abs() > MAX_LAYER_VALUE {
            return Err(SceneError::new(
                SceneErrorKind::Shape,
                format!("scene.json \"{field}[{i}]\" must be finite and within ±{MAX_LAYER_VALUE}"),
            ));
        }
        if non_negative && *token < 0.0 {
            return Err(SceneError::new(
                SceneErrorKind::Shape,
                format!("scene.json \"{field}[{i}]\" must not be negative"),
            ));
        }
        out.push(*token as f32);
    }
    Ok(out)
}

/// Canonicalized directory that contains `path`; the root every relative
/// script and image reference is confined to (the M3c image loader resolves
/// against the same root so the two can never disagree).
pub(crate) fn canonical_root(path: &Path) -> Result<PathBuf, SceneError> {
    let canonical = path.canonicalize().map_err(|e| {
        SceneError::new(
            SceneErrorKind::Read,
            format!("scene.json at {}: {e}", path.display()),
        )
    })?;
    if !canonical.is_file() {
        return Err(SceneError::new(
            SceneErrorKind::Read,
            format!("scene.json at {} is not a regular file", path.display()),
        ));
    }
    let root = canonical.parent().ok_or_else(|| {
        SceneError::new(SceneErrorKind::Read, "scene.json has no parent directory")
    })?;
    Ok(root.to_path_buf())
}

/// Read `path` into memory, refusing to buffer more than `cap` bytes.
/// Shared with the script loader so the 2 MiB script cap is enforced at
/// read time (metadata pre-checks alone race a swapped/grown file).
pub(crate) fn read_bounded(path: &Path, cap: u64) -> Result<Vec<u8>, SceneError> {
    let meta = fs::metadata(path).map_err(SceneError::from)?;
    if meta.len() > cap {
        return Err(SceneError::new(
            SceneErrorKind::Read,
            format!(
                "{} is {} bytes, over the {} byte cap",
                path.display(),
                meta.len(),
                cap
            ),
        ));
    }
    let bytes = fs::read(path)?;
    if bytes.len() as u64 > cap {
        return Err(SceneError::new(
            SceneErrorKind::Read,
            format!(
                "{} grew past the {} byte cap while reading",
                path.display(),
                cap
            ),
        ));
    }
    Ok(bytes)
}

fn parse_clear_color(general: &serde_json::Map<String, Value>) -> Result<[f32; 4], SceneError> {
    let Some(value) = general.get("clearcolor") else {
        return Ok([0.0, 0.0, 0.0, 1.0]);
    };
    // Two accepted serializations, both verified on the real corpus (M3b):
    // * the array form `[r, g, b, a]` (0.0..=1.0) used by the documented
    //   schema and the M3a fixtures;
    // * the space-separated string form `"r g b"` that Wallpaper Engine
    //   actually writes — 59 of 60 corpus scene.json entries use it, e.g.
    //   `"clearcolor": "0.7 0.7 0.7"` (RGB only; alpha defaults to 1.0).
    //   The property-wrapped object form `{"user": ..., "value": ...}`
    //   (1 of 60 corpus entries) stays a Shape rejection until user
    //   properties arrive in M3c+.
    if let Some(text) = value.as_str() {
        let tokens: Vec<&str> = text.split_whitespace().collect();
        if tokens.len() != 3 {
            return Err(SceneError::new(
                SceneErrorKind::Shape,
                format!(
                    "scene.json \"general.clearcolor\" string must be \"r g b\" \
                     (three space-separated floats), found {} tokens",
                    tokens.len()
                ),
            ));
        }
        let mut color = [0.0_f32; 4];
        for (i, token) in tokens.iter().enumerate() {
            let channel = token.parse::<f64>().map_err(|_| {
                SceneError::new(
                    SceneErrorKind::Shape,
                    format!(
                        "scene.json \"general.clearcolor[{i}]\" must be a float, got \"{token}\""
                    ),
                )
            })?;
            if !channel.is_finite() || !(0.0..=1.0).contains(&channel) {
                return Err(SceneError::new(
                    SceneErrorKind::Shape,
                    format!("scene.json \"general.clearcolor[{i}]\" must be between 0.0 and 1.0"),
                ));
            }
            color[i] = channel as f32;
        }
        color[3] = 1.0;
        return Ok(color);
    }
    let array = value.as_array().ok_or_else(|| {
        SceneError::new(
            SceneErrorKind::Shape,
            "scene.json \"general.clearcolor\" must be an array of four floats or a \"r g b\" string",
        )
    })?;
    if array.len() != 4 {
        return Err(SceneError::new(
            SceneErrorKind::Shape,
            format!(
                "scene.json \"general.clearcolor\" must have exactly four entries, found {}",
                array.len()
            ),
        ));
    }
    let mut color = [0.0_f32; 4];
    for (i, entry) in array.iter().enumerate() {
        let channel = entry.as_f64().ok_or_else(|| {
            SceneError::new(
                SceneErrorKind::Shape,
                format!("scene.json \"general.clearcolor[{i}]\" must be a float"),
            )
        })?;
        if !channel.is_finite() || !(0.0..=1.0).contains(&channel) {
            return Err(SceneError::new(
                SceneErrorKind::Shape,
                format!("scene.json \"general.clearcolor[{i}]\" must be between 0.0 and 1.0"),
            ));
        }
        color[i] = channel as f32;
    }
    Ok(color)
}

fn parse_resolution(
    general: &serde_json::Map<String, Value>,
) -> Result<Option<(u32, u32)>, SceneError> {
    let Some(value) = general.get("resolution") else {
        return Ok(None);
    };
    let array = value.as_array().ok_or_else(|| {
        SceneError::new(
            SceneErrorKind::Shape,
            "scene.json \"general.resolution\" must be an array of two positive integers",
        )
    })?;
    if array.len() != 2 {
        return Err(SceneError::new(
            SceneErrorKind::Shape,
            format!(
                "scene.json \"general.resolution\" must have exactly two entries, found {}",
                array.len()
            ),
        ));
    }
    let mut dims = [0_u32; 2];
    for (i, entry) in array.iter().enumerate() {
        let dim = entry.as_u64().ok_or_else(|| {
            SceneError::new(
                SceneErrorKind::Shape,
                format!("scene.json \"general.resolution[{i}]\" must be a positive integer"),
            )
        })?;
        if dim == 0 || dim > u64::from(MAX_DIMENSION) {
            return Err(SceneError::new(
                SceneErrorKind::Shape,
                format!(
                    "scene.json \"general.resolution[{i}]\" must be within 1..={MAX_DIMENSION}"
                ),
            ));
        }
        dims[i] = dim as u32;
    }
    Ok(Some((dims[0], dims[1])))
}

fn parse_fps(general: &serde_json::Map<String, Value>) -> Result<Option<f32>, SceneError> {
    let Some(value) = general.get("fps") else {
        return Ok(None);
    };
    let fps = value.as_f64().ok_or_else(|| {
        SceneError::new(
            SceneErrorKind::Shape,
            "scene.json \"general.fps\" must be a float",
        )
    })?;
    if !fps.is_finite() || fps <= 0.0 || fps > 240.0 {
        return Err(SceneError::new(
            SceneErrorKind::Shape,
            "scene.json \"general.fps\" must be within (0.0, 240.0]",
        ));
    }
    Ok(Some(fps as f32))
}

/// Resolve a `general.script` reference. Must stay inside the scene's content
/// root, exist, be a regular `.js` file, and be at most `MAX_SCRIPT_BYTES`.
fn resolve_script(root: &Path, reference: &str) -> Result<PathBuf, SceneError> {
    if reference.is_empty() {
        return Err(SceneError::new(
            SceneErrorKind::Script,
            "scene.json \"general.script\" must not be empty",
        ));
    }
    if reference.ends_with(".pkg") || reference.to_ascii_lowercase().ends_with(".pkg") {
        return Err(SceneError::new(
            SceneErrorKind::Script,
            "a .pkg script reference is only valid inside a packaged scene; \
             file-based scenes ship scene.json and scene.pkg side by side",
        ));
    }
    if !reference.to_ascii_lowercase().ends_with(".js") {
        return Err(SceneError::new(
            SceneErrorKind::Script,
            format!("scene script must be a .js file, got \"{reference}\""),
        ));
    }

    let joined = Path::new(reference);
    if joined.is_absolute() {
        return Err(SceneError::new(
            SceneErrorKind::Script,
            format!("scene script \"{reference}\" must be relative to the scene"),
        ));
    }
    for component in joined.components() {
        if matches!(
            component,
            Component::ParentDir | Component::RootDir | Component::Prefix(_)
        ) {
            return Err(SceneError::new(
                SceneErrorKind::Script,
                format!("scene script \"{reference}\" must stay inside the scene directory"),
            ));
        }
    }

    // Verify against the canonicalized target so symlinks cannot smuggle the
    // script out of the content root.
    let candidate = root.join(joined);
    let canonical = candidate.canonicalize().map_err(|e| {
        SceneError::new(
            SceneErrorKind::Script,
            format!("scene script \"{reference}\" is missing or unreadable: {e}"),
        )
    })?;
    if !canonical.starts_with(root) {
        return Err(SceneError::new(
            SceneErrorKind::Script,
            format!("scene script \"{reference}\" resolves outside the scene directory"),
        ));
    }
    if !canonical.is_file() {
        return Err(SceneError::new(
            SceneErrorKind::Script,
            format!("scene script \"{reference}\" is not a regular file"),
        ));
    }
    let meta = fs::metadata(&canonical).map_err(SceneError::from)?;
    if meta.len() > MAX_SCRIPT_BYTES {
        return Err(SceneError::new(
            SceneErrorKind::Script,
            format!(
                "scene script \"{reference}\" is {} bytes, over the {MAX_SCRIPT_BYTES} byte cap",
                meta.len()
            ),
        ));
    }
    Ok(canonical)
}

/// Resolve a `general.script` reference against the package entry table
/// (M3b). The rules live in `kwe_core::pkg::script_entry`, shared with
/// preflight (which checks the script entry's size against the same
/// resolution): relative, `.js`, no `..`/backslash/NUL, and exactly one
/// match — case-insensitively, either the literal path or the entry's tail
/// after a `/` (so `scripts/main.js` finds an entry stored as
/// `wallpaper/scripts/main.js`). Entry paths were already validated at
/// package open (no `..`, no absolute paths), so resolution can never leave
/// the table; the rejection messages exist for diagnostics, not safety.
fn resolve_pkg_script(
    reference: &str,
    entries: &[kwe_core::PkgEntry],
) -> Result<usize, SceneError> {
    kwe_core::script_entry(reference, entries)
        .map_err(|message| SceneError::new(SceneErrorKind::Script, message))
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::fs;
    use std::sync::atomic::{AtomicU32, Ordering};

    fn tmpdir() -> PathBuf {
        static COUNTER: AtomicU32 = AtomicU32::new(0);
        let n = COUNTER.fetch_add(1, Ordering::Relaxed);
        let dir = std::env::temp_dir().join(format!("kwe-scene-test-{}-{n}", std::process::id()));
        fs::create_dir_all(&dir).unwrap();
        dir
    }

    fn write(dir: &Path, name: &str, contents: &str) -> PathBuf {
        let path = dir.join(name);
        fs::write(&path, contents).unwrap();
        path
    }

    #[test]
    fn clear_color_exact_parse() {
        let dir = tmpdir();
        let scene = write(
            &dir,
            "scene.json",
            r#"{"general": {"clearcolor": [0.25, 0.5, 0.75, 1.0], "resolution": [1920, 1080], "fps": 30}}"#,
        );
        let config = SceneConfig::parse(&scene).unwrap();
        assert_eq!(config.clear_color, [0.25, 0.5, 0.75, 1.0]);
        assert_eq!(config.resolution, Some((1920, 1080)));
        assert_eq!(config.fps, Some(30.0));
        assert!(config.script_path.is_none());
    }

    #[test]
    fn clear_color_string_form_accepted() {
        // The corpus serialization: 59 of 60 real scene.json entries carry
        // clearcolor as a space-separated "r g b" string (M3b finding).
        let dir = tmpdir();
        let scene = write(
            &dir,
            "scene.json",
            r#"{"general": {"clearcolor": "0.7 0.7 0.7"}}"#,
        );
        let config = SceneConfig::parse(&scene).unwrap();
        assert_eq!(config.clear_color, [0.7, 0.7, 0.7, 1.0]);
        // Whitespace-tolerant and precision-rich, like the corpus variant
        // "0.713725 0.713725 0.713725" seen on one real wallpaper.
        let wide = write(
            &dir,
            "wide.json",
            r#"{"general": {"clearcolor": "  0.25\t0.5 0.75  "}}"#,
        );
        let config = SceneConfig::parse(&wide).unwrap();
        assert_eq!(config.clear_color, [0.25, 0.5, 0.75, 1.0]);
    }

    #[test]
    fn clear_color_string_form_rejects_wrong_tokens_and_range() {
        let dir = tmpdir();
        let two = write(
            &dir,
            "two.json",
            r#"{"general": {"clearcolor": "0.7 0.7"}}"#,
        );
        assert_eq!(
            SceneConfig::parse(&two).unwrap_err().kind,
            SceneErrorKind::Shape
        );
        let four = write(
            &dir,
            "four.json",
            r#"{"general": {"clearcolor": "0.7 0.7 0.7 1.0"}}"#,
        );
        assert_eq!(
            SceneConfig::parse(&four).unwrap_err().kind,
            SceneErrorKind::Shape
        );
        let not_a_float = write(
            &dir,
            "nope.json",
            r#"{"general": {"clearcolor": "red green blue"}}"#,
        );
        assert_eq!(
            SceneConfig::parse(&not_a_float).unwrap_err().kind,
            SceneErrorKind::Shape
        );
        let over = write(
            &dir,
            "over.json",
            r#"{"general": {"clearcolor": "1.5 0 0"}}"#,
        );
        assert_eq!(
            SceneConfig::parse(&over).unwrap_err().kind,
            SceneErrorKind::Shape
        );
    }

    #[test]
    fn clear_color_defaults_to_opaque_black() {
        let dir = tmpdir();
        let scene = write(&dir, "scene.json", r#"{"general": {}}"#);
        let config = SceneConfig::parse(&scene).unwrap();
        assert_eq!(config.clear_color, [0.0, 0.0, 0.0, 1.0]);
    }

    #[test]
    fn missing_general_defaults_cleanly() {
        let dir = tmpdir();
        let scene = write(&dir, "scene.json", r#"{}"#);
        let config = SceneConfig::parse(&scene).unwrap();
        assert_eq!(config.clear_color, [0.0, 0.0, 0.0, 1.0]);
        assert!(config.script_path.is_none());
    }

    #[test]
    fn clear_color_rejects_wrong_arity_and_range() {
        let dir = tmpdir();
        let short = write(
            &dir,
            "short.json",
            r#"{"general": {"clearcolor": [1, 2, 3]}}"#,
        );
        assert_eq!(
            SceneConfig::parse(&short).unwrap_err().kind,
            SceneErrorKind::Shape
        );
        let over = write(
            &dir,
            "over.json",
            r#"{"general": {"clearcolor": [1.5, 0, 0, 1]}}"#,
        );
        assert_eq!(
            SceneConfig::parse(&over).unwrap_err().kind,
            SceneErrorKind::Shape
        );
        let nan = write(
            &dir,
            "nan.json",
            r#"{"general": {"clearcolor": [1, "x", 0, 1]}}"#,
        );
        assert_eq!(
            SceneConfig::parse(&nan).unwrap_err().kind,
            SceneErrorKind::Shape
        );
    }

    #[test]
    fn root_must_be_object_and_json_valid() {
        let dir = tmpdir();
        let not_object = write(&dir, "arr.json", r#"[1, 2, 3]"#);
        assert_eq!(
            SceneConfig::parse(&not_object).unwrap_err().kind,
            SceneErrorKind::Json
        );
        let not_json = write(&dir, "bad.json", r#"{"general": "#);
        assert_eq!(
            SceneConfig::parse(&not_json).unwrap_err().kind,
            SceneErrorKind::Json
        );
    }

    #[test]
    fn script_resolved_relative_to_scene() {
        let dir = tmpdir();
        fs::create_dir_all(dir.join("scripts")).unwrap();
        write(&dir.join("scripts"), "main.js", "// fixture\n");
        let scene = write(
            &dir,
            "scene.json",
            r#"{"general": {"script": "scripts/main.js"}}"#,
        );
        let config = SceneConfig::parse(&scene).unwrap();
        let expected = dir.join("scripts").join("main.js").canonicalize().unwrap();
        assert_eq!(config.script_path, Some(expected));
    }

    #[test]
    fn script_non_string_rejected_as_shape() {
        let dir = tmpdir();
        let scene = write(&dir, "scene.json", r#"{"general": {"script": 42}}"#);
        assert_eq!(
            SceneConfig::parse(&scene).unwrap_err().kind,
            SceneErrorKind::Shape
        );
    }

    #[test]
    fn script_traversal_rejected() {
        let dir = tmpdir();
        // Passes the .js check so the traversal guard is what rejects it.
        let scene = write(
            &dir,
            "scene.json",
            r#"{"general": {"script": "../../etc/passwd.js"}}"#,
        );
        let err = SceneConfig::parse(&scene).unwrap_err();
        assert_eq!(err.kind, SceneErrorKind::Script);
        assert!(
            err.message.contains("inside the scene directory"),
            "{}",
            err.message
        );
    }

    #[test]
    fn script_absolute_path_rejected() {
        let dir = tmpdir();
        let scene = write(
            &dir,
            "scene.json",
            r#"{"general": {"script": "/etc/passwd"}}"#,
        );
        assert_eq!(
            SceneConfig::parse(&scene).unwrap_err().kind,
            SceneErrorKind::Script
        );
    }

    #[test]
    fn script_missing_file_rejected() {
        let dir = tmpdir();
        let scene = write(
            &dir,
            "scene.json",
            r#"{"general": {"script": "does-not-exist.js"}}"#,
        );
        assert_eq!(
            SceneConfig::parse(&scene).unwrap_err().kind,
            SceneErrorKind::Script
        );
    }

    #[test]
    fn pkg_script_reference_still_rejected_for_file_scenes() {
        // A file-based scene.json cannot point its script at scene.pkg: only
        // packaged scenes (parse_pkg) may reference package entries.
        let dir = tmpdir();
        let scene = write(
            &dir,
            "scene.json",
            r#"{"general": {"script": "scene.pkg"}}"#,
        );
        assert_eq!(
            SceneConfig::parse(&scene).unwrap_err().kind,
            SceneErrorKind::Script
        );
    }

    /// Minimal scene.pkg fixture (mirrors the verified corpus layout:
    /// PKGV0001, length-prefixed paths, raw payloads).
    fn build_pkg(entries: &[(&str, &[u8])]) -> Vec<u8> {
        let mut out = Vec::new();
        out.extend_from_slice(&8_u32.to_le_bytes());
        out.extend_from_slice(b"PKGV0001");
        out.extend_from_slice(&(entries.len() as u32).to_le_bytes());
        let mut offset: u32 = 0;
        for (path, payload) in entries {
            out.extend_from_slice(&(path.len() as u32).to_le_bytes());
            out.extend_from_slice(path.as_bytes());
            out.extend_from_slice(&offset.to_le_bytes());
            out.extend_from_slice(&(payload.len() as u32).to_le_bytes());
            offset += payload.len() as u32;
        }
        for (_, payload) in entries {
            out.extend_from_slice(payload);
        }
        out
    }

    fn pkg_entries(dir: &Path, bytes: &[u8]) -> Vec<kwe_core::PkgEntry> {
        let pkg = dir.join("fixture.pkg");
        fs::write(&pkg, bytes).unwrap();
        kwe_core::PkgReader::open(&pkg).unwrap().entries().to_vec()
    }

    #[test]
    fn pkg_scene_parsed_with_script_entry() {
        let dir = tmpdir();
        let scene_json =
            br#"{"general":{"clearcolor":[0.5,0.25,0.125,1.0],"script":"scripts/main.js"}}"#;
        let entries = pkg_entries(
            &dir,
            &build_pkg(&[
                ("scene.json", scene_json),
                ("scripts/main.js", b"function init(){}"),
            ]),
        );
        let config = SceneConfig::parse_pkg(scene_json, &entries).unwrap();
        assert_eq!(config.clear_color, [0.5, 0.25, 0.125, 1.0]);
        assert_eq!(config.script_entry, Some(1));
        assert!(config.script_path.is_none());
    }

    #[test]
    fn pkg_script_suffix_match_within_directories() {
        let dir = tmpdir();
        let scene_json = br#"{"general":{"script":"scripts/main.js"}}"#;
        let entries = pkg_entries(
            &dir,
            &build_pkg(&[
                ("scene.json", scene_json),
                ("wallpaper/scripts/main.js", b"function init(){}"),
                ("textures/tex.png", b"TEXV0005"),
            ]),
        );
        let config = SceneConfig::parse_pkg(scene_json, &entries).unwrap();
        assert_eq!(config.script_entry, Some(1));
    }

    #[test]
    fn pkg_scene_without_script_has_no_entry() {
        let dir = tmpdir();
        let scene_json = br#"{"general":{}}"#;
        let entries = pkg_entries(&dir, &build_pkg(&[("scene.json", scene_json)]));
        let config = SceneConfig::parse_pkg(scene_json, &entries).unwrap();
        assert!(config.script_entry.is_none());
    }

    #[test]
    fn pkg_script_missing_entry_rejected() {
        let dir = tmpdir();
        let scene_json = br#"{"general":{"script":"nope.js"}}"#;
        let entries = pkg_entries(&dir, &build_pkg(&[("scene.json", scene_json)]));
        let err = SceneConfig::parse_pkg(scene_json, &entries).unwrap_err();
        assert_eq!(err.kind, SceneErrorKind::Script);
        assert!(
            err.message.contains("is not an entry of the package"),
            "{}",
            err.message
        );
    }

    #[test]
    fn pkg_script_ambiguous_match_rejected() {
        let dir = tmpdir();
        let scene_json = br#"{"general":{"script":"main.js"}}"#;
        let entries = pkg_entries(
            &dir,
            &build_pkg(&[
                ("scene.json", scene_json),
                ("a/main.js", b""),
                ("b/main.js", b""),
            ]),
        );
        let err = SceneConfig::parse_pkg(scene_json, &entries).unwrap_err();
        assert_eq!(err.kind, SceneErrorKind::Script);
        assert!(
            err.message.contains("matches 2 package entries"),
            "{}",
            err.message
        );
    }

    #[test]
    fn pkg_script_hostile_references_rejected() {
        let dir = tmpdir();
        let entries = pkg_entries(&dir, &build_pkg(&[("scene.json", b"{}")]));
        for reference in [
            "../evil.js",
            "/etc/passwd.js",
            "back\\slash.js",
            "scene.pkg",
            "main.txt",
            "",
        ] {
            let scene_json = serde_json::json!({ "general": { "script": reference } }).to_string();
            let err = SceneConfig::parse_pkg(scene_json.as_bytes(), &entries).unwrap_err();
            assert_eq!(err.kind, SceneErrorKind::Script, "{reference:?}");
        }
    }

    #[test]
    fn pkg_scene_invalid_json_rejected() {
        let dir = tmpdir();
        let entries = pkg_entries(&dir, &build_pkg(&[("scene.json", b"not json")]));
        assert_eq!(
            SceneConfig::parse_pkg(b"not json", &entries)
                .unwrap_err()
                .kind,
            SceneErrorKind::Json
        );
    }

    #[test]
    fn non_js_script_rejected() {
        let dir = tmpdir();
        let scene = write(&dir, "scene.json", r#"{"general": {"script": "main.txt"}}"#);
        assert_eq!(
            SceneConfig::parse(&scene).unwrap_err().kind,
            SceneErrorKind::Script
        );
    }

    #[test]
    fn oversized_scene_json_rejected() {
        let dir = tmpdir();
        let path = dir.join("big.json");
        // 17 MiB of padding inside a string.
        let payload = format!(
            r#"{{"general": {{"clearcolor": [1, 0, 0, 1]}}, "pad": "{}"}}"#,
            "x".repeat(17 * 1024 * 1024)
        );
        fs::write(&path, &payload).unwrap();
        assert_eq!(
            SceneConfig::parse(&path).unwrap_err().kind,
            SceneErrorKind::Read
        );
    }

    #[test]
    fn oversized_script_rejected() {
        let dir = tmpdir();
        let big = dir.join("big.js");
        fs::write(&big, vec![b'x'; (MAX_SCRIPT_BYTES + 1) as usize]).unwrap();
        let scene = write(&dir, "scene.json", r#"{"general": {"script": "big.js"}}"#);
        assert_eq!(
            SceneConfig::parse(&scene).unwrap_err().kind,
            SceneErrorKind::Script
        );
    }

    // ---- M3c: objects / image layers ----

    fn parse_objects_of(json: &str) -> Result<Vec<LayerSpec>, SceneError> {
        let value: Value = serde_json::from_str(json).unwrap();
        let root = value.as_object().unwrap();
        parse_objects(root).map(|(layers, _)| layers)
    }

    #[test]
    fn image_layers_parsed_in_order_with_defaults() {
        // Corpus-style serialization: string vectors, three components.
        let layers = parse_objects_of(
            r#"{"objects": [
                {"name": "bg", "image": "textures/bg.png",
                 "origin": "1920.00000 1080.00000 0.00000",
                 "angles": "0.00000 -0.00000 0.00000",
                 "scale": "1.26081 1.26081 1.26081",
                 "size": "3840.00000 2194.00000"},
                {"name": "fg", "image": "textures/fg.png"}
            ]}"#,
        )
        .unwrap();
        assert_eq!(layers.len(), 2);
        assert_eq!(layers[0].name, "bg");
        assert_eq!(layers[0].image.as_deref(), Some("textures/bg.png"));
        assert_eq!(layers[0].origin, [1920.0, 1080.0]);
        assert_eq!(layers[0].angles, [0.0, 0.0, 0.0]);
        assert_eq!(layers[0].scale, [1.26081, 1.26081]);
        assert_eq!(layers[0].size, [3840.0, 2194.0]);
        assert_eq!(layers[0].alpha, 1.0);
        assert!(layers[0].visible);
        assert_eq!(layers[0].blend_mode, 0);
        // Defaults for a bare layer.
        assert_eq!(layers[1].origin, [0.0, 0.0]);
        assert_eq!(layers[1].angles, [0.0, 0.0, 0.0]);
        assert_eq!(layers[1].scale, [1.0, 1.0]);
        assert_eq!(layers[1].size, [0.0, 0.0]);
        assert_eq!(layers[1].alpha, 1.0);
    }

    #[test]
    fn array_vector_form_accepted() {
        let layers = parse_objects_of(
            r#"{"objects": [{"name": "l", "image": "a.png",
                             "origin": [10, 20], "size": [100, 50]}]}"#,
        )
        .unwrap();
        assert_eq!(layers[0].origin, [10.0, 20.0]);
        assert_eq!(layers[0].size, [100.0, 50.0]);
    }

    #[test]
    fn angles_converted_from_radians_to_degrees() {
        // Corpus-verified: scene.json stores radians (exact π values seen,
        // none at 90/180); the script API and runtime model use degrees.
        let layers = parse_objects_of(
            r#"{"objects": [{"name": "spin", "image": "a.png",
                             "angles": "3.14159 0.00000 1.57080"}]}"#,
        )
        .unwrap();
        assert!((layers[0].angles[0] - 180.0).abs() < 0.001);
        assert_eq!(layers[0].angles[1], 0.0);
        assert!((layers[0].angles[2] - 90.0).abs() < 0.001);
    }

    #[test]
    fn property_wrapped_values_unwrapped() {
        // The corpus's dominant serialization for user-bindable fields
        // (70% of alpha, 49% of visible — re-scanned); the initial value
        // is the behavior until user properties (M3j).
        let layers = parse_objects_of(
            r#"{"objects": [
                {"name": "a", "image": "a.png",
                 "alpha": {"user": "volume", "value": 0.5},
                 "visible": {"user": "on", "value": false},
                 "origin": {"user": "pos", "value": "5.00000 6.00000 0.00000"}},
                {"name": "b", "image": {"user": "tex", "value": "b.png"}}
            ]}"#,
        )
        .unwrap();
        assert_eq!(layers[0].alpha, 0.5);
        assert!(!layers[0].visible);
        assert_eq!(layers[0].origin, [5.0, 6.0]);
        assert_eq!(layers[1].image.as_deref(), Some("b.png"));
    }

    #[test]
    fn non_string_image_registers_layer_without_reference() {
        // A property-wrapped image whose value is not a string, or any
        // other non-string image field: the layer stays registered (the
        // script can still reach it) but is skipped at load.
        let layers = parse_objects_of(r#"{"objects": [{"name": "x", "image": 42}]}"#).unwrap();
        assert_eq!(layers.len(), 1);
        assert!(layers[0].image.is_none());
    }

    #[test]
    fn objects_without_image_field_ignored() {
        // Audio entries, particles, effects objects — nothing renders.
        let layers = parse_objects_of(
            r#"{"objects": [
                {"name": "song.mp3", "audio": "song.mp3"},
                {"name": "dust", "particle": "particles/dust.json"},
                {"name": "bg", "image": "bg.png"}
            ]}"#,
        )
        .unwrap();
        assert_eq!(layers.len(), 1);
        assert_eq!(layers[0].name, "bg");
    }

    #[test]
    fn model_json_references_skipped_as_m3h() {
        // All 685 corpus image references point at model .json files (WE's
        // solid-model architecture): they are model layers, not textures —
        // skipped, and not counted toward the layer cap.
        let mut objects = r#"{"objects": ["#.to_string();
        for i in 0..300 {
            objects.push_str(&format!(
                r#"{{"name": "m{i}", "image": "models/util/m{i}.json"}},"#
            ));
        }
        objects.push_str(r#"{"name": "real", "image": "tex.png"}]}"#);
        let layers = parse_objects_of(&objects).unwrap();
        assert_eq!(layers.len(), 1);
        assert_eq!(layers[0].name, "real");
    }

    #[test]
    fn malformed_model_layers_skip_never_reject() {
        // A model layer is skipped BEFORE validation: no name, an
        // out-of-range alpha, or a non-string name must never reject the
        // scene — the skip-never-reject policy applies to the whole layer.
        let objects = r#"{"objects": [
            {"image": "models/missing-name.json"},
            {"name": 7, "image": "models/bad-name.json", "alpha": 2.0},
            {"name": "real", "image": "tex.png"}
        ]}"#;
        let layers = parse_objects_of(objects).unwrap();
        assert_eq!(layers.len(), 1);
        assert_eq!(layers[0].name, "real");
    }

    #[test]
    fn too_many_image_layers_rejected() {
        let layers = (0..MAX_LAYERS + 1)
            .map(|i| format!(r#"{{"name": "l{i}", "image": "t{i}.png"}}"#))
            .collect::<Vec<_>>()
            .join(",");
        let objects = format!(r#"{{"objects": [{layers}]}}"#);
        let error = parse_objects_of(&objects).unwrap_err();
        assert_eq!(error.kind, SceneErrorKind::Shape);
        assert!(
            error
                .message
                .contains(&format!("over the {MAX_LAYERS} layer cap")),
            "{}",
            error.message
        );
    }

    #[test]
    fn exactly_256_image_layers_accepted() {
        let layers = (0..MAX_LAYERS)
            .map(|i| format!(r#"{{"name": "l{i}", "image": "t{i}.png"}}"#))
            .collect::<Vec<_>>()
            .join(",");
        let objects = format!(r#"{{"objects": [{layers}]}}"#);
        assert_eq!(parse_objects_of(&objects).unwrap().len(), MAX_LAYERS);
    }

    #[test]
    fn layer_out_of_range_values_rejected() {
        let cases = [
            (r#"{"name": "l", "image": "a.png", "alpha": 1.5}"#, "alpha"),
            (
                r#"{"name": "l", "image": "a.png", "origin": "0 0 x"}"#,
                "origin",
            ),
            (
                r#"{"name": "l", "image": "a.png", "scale": "1000000000 1 1"}"#,
                "scale",
            ),
            (
                r#"{"name": "l", "image": "a.png", "size": "-5 10"}"#,
                "size",
            ),
            (r#"{"name": "l", "image": "a.png", "size": "10"}"#, "size"),
            (
                r#"{"name": "l", "image": "a.png", "angles": "90"}"#,
                "angles",
            ),
            (
                r#"{"name": "l", "image": "a.png", "origin": [1, "x"]}"#,
                "origin",
            ),
            (
                r#"{"name": "l", "image": "a.png", "visible": 1}"#,
                "visible",
            ),
            (
                r#"{"name": "l", "image": "a.png", "origin": "1 2 3 4"}"#,
                "origin",
            ),
            (
                r#"{"name": "l", "image": "a.png", "alpha": {"user": "a"}}"#,
                "alpha",
            ),
        ];
        for (json, expected_field) in cases {
            let error = parse_objects_of(&format!(r#"{{"objects": [{json}]}}"#)).unwrap_err();
            assert_eq!(error.kind, SceneErrorKind::Shape, "{json}");
            assert!(
                error.message.contains(expected_field),
                "expected a mention of {expected_field:?} in: {}",
                error.message
            );
        }
    }

    #[test]
    fn image_layer_without_name_rejected() {
        let error = parse_objects_of(r#"{"objects": [{"image": "a.png"}]}"#).unwrap_err();
        assert_eq!(error.kind, SceneErrorKind::Shape);
        assert!(error.message.contains("name"), "{}", error.message);
        let error =
            parse_objects_of(r#"{"objects": [{"name": 42, "image": "a.png"}]}"#).unwrap_err();
        assert_eq!(error.kind, SceneErrorKind::Shape);
    }

    #[test]
    fn objects_wrong_shape_rejected() {
        for json in [r#"{"objects": 42}"#, r#"{"objects": [42]}"#] {
            let error = parse_objects_of(json).unwrap_err();
            assert_eq!(error.kind, SceneErrorKind::Shape, "{json}");
        }
    }

    #[test]
    fn blend_modes_recorded() {
        // `colorBlendMode` is the corpus key; `blendMode` is the brief's
        // alias. The raw value is recorded pre-clamp for the worker's
        // bounded diagnostic; the runtime clamps to the implemented set
        // (M3d: 0/1/6/7/9 render; 11/12/24/30 note once; anything else is
        // tolerated silently). A property-wrapped value is unwrapped.
        let layers = parse_objects_of(
            r#"{"objects": [
                {"name": "a", "image": "a.png", "colorBlendMode": 24},
                {"name": "b", "image": "b.png", "blendMode": 6},
                {"name": "c", "image": "c.png", "colorBlendMode": 0},
                {"name": "d", "image": "d.png", "blendMode": "weird"},
                {"name": "e", "image": "e.png",
                 "colorBlendMode": {"user": "blend", "value": 7}}
            ]}"#,
        )
        .unwrap();
        assert_eq!(layers[0].blend_mode, 24);
        assert_eq!(layers[1].blend_mode, 6);
        assert_eq!(layers[2].blend_mode, 0);
        // A non-numeric mode is tolerated (src-over), not a reject.
        assert_eq!(layers[3].blend_mode, 0);
        // Property-wrapped modes are unwrapped like every other scalar.
        assert_eq!(layers[4].blend_mode, 7);
    }

    #[test]
    fn brightness_and_tint_parsed_with_clamps() {
        // Defaults: brightness 1.0, tint all-ones (WE `color` default
        // {1,1,1}, alpha implied 1 — the g_Color4 semantics).
        let layers = parse_objects_of(r#"{"objects": [{"name": "l", "image": "a.png"}]}"#).unwrap();
        assert_eq!(layers[0].brightness, 1.0);
        assert_eq!(layers[0].tint, [1.0, 1.0, 1.0, 1.0]);

        // The WE file key is `color` (vec3); the brief's `tint` takes
        // precedence, accepts 3 or 4 components, and both are
        // property-wrapped like the M3c scalars. Out-of-range values clamp
        // (0..=10 brightness, 0..=1 tint); non-finite becomes the identity.
        let layers = parse_objects_of(
            r#"{"objects": [
                {"name": "a", "image": "a.png", "color": "0.5 1.0 0.25"},
                {"name": "b", "image": "b.png",
                 "tint": {"user": "tint", "value": "0 1 2 0.5"}},
                {"name": "c", "image": "c.png", "brightness": 50},
                {"name": "d", "image": "d.png",
                 "color": [9, 9, 9], "tint": [0.25, 0.5, 0.75, 1]},
                {"name": "e", "image": "e.png", "brightness": -3}
            ]}"#,
        )
        .unwrap();
        assert_eq!(layers[0].tint, [0.5, 1.0, 0.25, 1.0]);
        assert_eq!(layers[0].brightness, 1.0);
        // tint clamps to 0..=1; brightness clamps to 0..=10.
        assert_eq!(layers[1].tint, [0.0, 1.0, 1.0, 0.5]);
        assert_eq!(layers[2].brightness, 10.0);
        // `tint` wins over `color`; 4 components are honored as RGBA.
        assert_eq!(layers[3].tint, [0.25, 0.5, 0.75, 1.0]);
        assert_eq!(layers[4].brightness, 0.0);

        // A numeric string is accepted (the corpus editor serializes
        // scalars as strings); a non-numeric one is a shape rejection,
        // like alpha.
        let layers = parse_objects_of(
            r#"{"objects": [{"name": "f", "image": "f.png", "brightness": "2.5"}]}"#,
        )
        .unwrap();
        assert_eq!(layers[0].brightness, 2.5);
        let error = parse_objects_of(
            r#"{"objects": [{"name": "l", "image": "a.png", "brightness": "bright"}]}"#,
        )
        .unwrap_err();
        assert_eq!(error.kind, SceneErrorKind::Shape);
    }

    // ---- M3e: text layers ----

    #[test]
    fn text_layers_parsed_with_defaults() {
        // A bare text object: defaults are the researched OWE ones —
        // pointsize 12 pt -> 48 px, alignment center/center, opaque white,
        // brightness 1.0, alpha 1.0, visible true.
        let layers = parse_objects_of(r#"{"objects": [{"name": "t", "text": "Hello"}]}"#).unwrap();
        assert_eq!(layers.len(), 1);
        let text = layers[0].text.as_ref().unwrap();
        assert_eq!(text.text, "Hello");
        assert_eq!(text.font, None);
        assert_eq!(text.pointsize, 48.0);
        assert_eq!(text.horizontal_align, HorizontalAlign::Center);
        assert_eq!(text.vertical_align, VerticalAlign::Center);
        assert_eq!(text.color, [1.0, 1.0, 1.0, 1.0]);
        assert!(!text.has_size);
        // The shared fields (common props) live on the layer; defaults are
        // the researched OWE ones — brightness 1.0, alpha 1.0, visible
        // true, no image.
        assert_eq!(layers[0].name, "t");
        assert_eq!(layers[0].alpha, 1.0);
        assert!(layers[0].visible);
        assert_eq!(layers[0].brightness, 1.0);
        assert_eq!(layers[0].blend_mode, 0);
        assert_eq!(layers[0].image, None);
        assert!(layers[0].texture.is_none());
        assert_eq!(layers[0].size, [1.0, 1.0]); // text size is automatic
    }

    #[test]
    fn text_layer_fields_parsed() {
        let layers = parse_objects_of(
            r#"{"objects": [{
                "name": "t",
                "text": {"user": "txt", "value": "Hi"},
                "font": "systemfont_Noto Sans",
                "pointsize": 13,
                "horizontalalign": "right",
                "verticalalign": "top",
                "color": [1.0, 0.0, 0.0],
                "alpha": 0.5,
                "brightness": 2.0,
                "size": [100, 100]
            }]}"#,
        )
        .unwrap();
        let text = layers[0].text.as_ref().unwrap();
        // Property-wrapped text is unwrapped like every other field.
        assert_eq!(text.text, "Hi");
        assert_eq!(text.font.as_deref(), Some("systemfont_Noto Sans"));
        // pointsize 13 pt -> round(13 * 4) = 52 px.
        assert_eq!(text.pointsize, 52.0);
        assert_eq!(text.horizontal_align, HorizontalAlign::Right);
        assert_eq!(text.vertical_align, VerticalAlign::Top);
        assert_eq!(text.color, [1.0, 0.0, 0.0, 1.0]);
        assert!(text.has_size);
        // The common props (alpha, brightness) land on the layer.
        assert_eq!(layers[0].alpha, 0.5);
        assert_eq!(layers[0].brightness, 2.0);
    }

    #[test]
    fn text_pointsize_clamped_and_tolerant() {
        // OWE: px = round(pointsize * 4), clamp 1..=1024; ours 4..=512
        // (documented deviation). Non-finite or ≤ 0 falls back to the
        // default 48 px; numeric strings are accepted like brightness.
        for (input, want) in [
            ("100", 400.0),
            ("200", 512.0), // clamped
            ("0.5", 4.0),   // clamped up
            ("-3", 48.0),   // invalid -> default
            ("\"nan\"", 48.0),
            ("\"inf\"", 48.0),
        ] {
            let layers = parse_objects_of(&format!(
                r#"{{"objects": [{{"name": "t", "text": "x", "pointsize": {input}}}]}}"#
            ))
            .unwrap();
            assert_eq!(
                layers[0].text.as_ref().unwrap().pointsize,
                want,
                "pointsize {input}"
            );
        }
        // A non-numeric pointsize rejects like brightness.
        let error =
            parse_objects_of(r#"{"objects": [{"name": "t", "text": "x", "pointsize": "big"}]}"#)
                .unwrap_err();
        assert_eq!(error.kind, SceneErrorKind::Shape);
    }

    #[test]
    fn text_alignment_falls_back_like_owe() {
        // horizontalalign/verticalalign win; `alignment` is consulted when
        // they are empty; default "center" (the researched OWE
        // align_or_default chain).
        let layers = parse_objects_of(
            r#"{"objects": [
                {"name": "a", "text": "x", "horizontalalign": "left"},
                {"name": "b", "text": "x", "alignment": "right"},
                {"name": "c", "text": "x", "alignment": "top"},
                {"name": "d", "text": "x", "horizontalalign": "", "verticalalign": ""},
                {"name": "e", "text": "x", "alignment": {"user": "u", "value": "left"}}
            ]}"#,
        )
        .unwrap();
        assert_eq!(
            layers[0].text.as_ref().unwrap().horizontal_align,
            HorizontalAlign::Left
        );
        assert_eq!(
            layers[0].text.as_ref().unwrap().vertical_align,
            VerticalAlign::Center
        );
        assert_eq!(
            layers[1].text.as_ref().unwrap().horizontal_align,
            HorizontalAlign::Right
        );
        assert_eq!(
            layers[2].text.as_ref().unwrap().vertical_align,
            VerticalAlign::Top
        );
        // Empty explicit values fall back to the alignment field too, and
        // empty everywhere means center (the OWE default).
        assert_eq!(
            layers[3].text.as_ref().unwrap().horizontal_align,
            HorizontalAlign::Center
        );
        assert_eq!(
            layers[3].text.as_ref().unwrap().vertical_align,
            VerticalAlign::Center
        );
        // Property-wrapped alignment is unwrapped.
        assert_eq!(
            layers[4].text.as_ref().unwrap().horizontal_align,
            HorizontalAlign::Left
        );
    }

    #[test]
    fn text_common_props_match_image_layers() {
        // The shared parse path must behave identically for text layers:
        // radians->degrees, wrappers, clamps, rejections.
        let layers = parse_objects_of(
            r#"{"objects": [{
                "name": "t", "text": "x",
                "origin": "100 200 0", "angles": [3.14159265, 0, 0],
                "scale": [2, 3], "alpha": 0.25, "visible": false,
                "colorBlendMode": 7, "brightness": "4"
            }]}"#,
        )
        .unwrap();
        // The common props land on the LayerSpec exactly like image layers
        // (TextSpec does not duplicate them — the worker reads LayerState,
        // which mirrors LayerSpec).
        assert_eq!(layers[0].origin, [100.0, 200.0]);
        assert!((layers[0].angles[0] - 180.0).abs() < 0.001);
        assert_eq!(layers[0].scale, [2.0, 3.0]);
        assert_eq!(layers[0].alpha, 0.25);
        assert!(!layers[0].visible);
        assert_eq!(layers[0].blend_mode, 7);
        assert_eq!(layers[0].brightness, 4.0);
        assert_eq!(layers[0].size, [1.0, 1.0], "text layers pin size");
        // Text layers reject bad shared scalars like image layers.
        let error = parse_objects_of(r#"{"objects": [{"name": "t", "text": "x", "alpha": 2.0}]}"#)
            .unwrap_err();
        assert_eq!(error.kind, SceneErrorKind::Shape);
        let error = parse_objects_of(r#"{"objects": [{"text": "x"}]}"#).unwrap_err();
        assert_eq!(error.kind, SceneErrorKind::Shape);
        assert!(error.message.contains("name"));
    }

    #[test]
    fn text_layer_caps_and_counts() {
        // Objects with both image and text count as image layers (and are
        // counted for the diag); text layers past MAX_TEXT_LAYERS are
        // skipped, not a rejection.
        use crate::text::MAX_TEXT_LAYERS;
        let mut objects = String::from(r#"{"objects": ["#);
        for i in 0..MAX_TEXT_LAYERS {
            objects.push_str(&format!(r#"{{"name": "t{i}", "text": "x"}},"#));
        }
        objects.push_str(r#"{"name": "extra", "text": "y"}"#);
        objects.push_str(r#",{"name": "both", "image": "a.png", "text": "z"}"#);
        objects.push_str("]}");
        let value: Value = serde_json::from_str(&objects).unwrap();
        let root = value.as_object().unwrap();
        let (layers, counts) = parse_objects(root).unwrap();
        assert_eq!(layers.len(), MAX_TEXT_LAYERS + 1); // 16 text + 1 image
        assert!(layers[MAX_TEXT_LAYERS].text.is_none()); // "both" is an image layer
        assert_eq!(layers[MAX_TEXT_LAYERS].image.as_deref(), Some("a.png"));
        assert_eq!(counts.text_layer_skips, 1);
        assert_eq!(counts.text_on_image_objects, 1);
        assert_eq!(counts.text_size_ignored, 0);
        // Text layers count toward the MAX_LAYERS cap like image layers
        // (the text cap only limits text layers — the bulk here is image
        // layers so the 257th registered layer triggers the rejection:
        // 241 images + 16 text = 257 registrations; the 17th text object
        // is skipped by the text cap and never registers).
        let mut objects = String::from(r#"{"objects": ["#);
        for i in 0..MAX_LAYERS - MAX_TEXT_LAYERS + 1 {
            objects.push_str(&format!(r#"{{"name": "i{i}", "image": "i.png"}},"#));
        }
        for i in 0..MAX_TEXT_LAYERS + 1 {
            objects.push_str(&format!(r#"{{"name": "t{i}", "text": "x"}},"#));
        }
        objects.pop();
        objects.push_str("]}");
        let error = parse_objects_of(&objects).unwrap_err();
        assert_eq!(error.kind, SceneErrorKind::Shape);
        assert!(error.message.contains("layer cap"));
    }

    #[test]
    fn pkg_scene_carries_image_layers() {
        // The pkg lane parses the same `objects` array; image references
        // resolve against the package table at load, not here.
        let dir = tmpdir();
        let scene_json = br#"{"objects": [{"name": "bg", "image": "textures/bg.png"}]}"#;
        let entries = pkg_entries(
            &dir,
            &build_pkg(&[("scene.json", scene_json), ("textures/bg.png", b"TEXV0005")]),
        );
        let config = SceneConfig::parse_pkg(scene_json, &entries).unwrap();
        assert_eq!(config.layers.len(), 1);
        assert_eq!(config.layers[0].image.as_deref(), Some("textures/bg.png"));
        assert!(config.layers[0].texture.is_none());
    }
}
