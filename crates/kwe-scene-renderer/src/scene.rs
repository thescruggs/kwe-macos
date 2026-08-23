// SPDX-License-Identifier: GPL-3.0-or-later
// Scene.json parsing for the M3a slice of the original SceneScript engine.
//
// The M3a worker understands exactly one input: a scene.json file laid out per
// docs/SCENE_FORMAT_V1.md. Everything the engine needs is either in the JSON
// (`general.clearcolor`) or referenced from it (`general.script`, resolved
// relative to the scene's content root). Unknown keys are tolerated so that
// real wallpaper packages never make the worker reject a scene; the M3c
// slice interprets the root `objects` array as image layers, M3e as text
// layers, M3f as particle systems (models, audio, and the
// `effects`/`properties` sections stay M3d+).
//
// Every read is bounded: the scene.json file is capped at 16 MiB (the daemon's
// preflight uses the same bound) and the referenced script at 2 MiB.

use std::error::Error as StdError;
use std::fmt;
use std::fs;
use std::path::{Component, Path, PathBuf};

use kwe_core::{SceneObjectKind, classify_scene_object, scene_property_value};
use serde_json::Value;

use crate::layers::{MAX_LAYER_VALUE, MAX_LAYERS};
use crate::particles;
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
    /// (models, audio — M3d+) is ignored. The loader resolves and decodes
    /// each layer's `image` and fills `texture`.
    pub layers: Vec<LayerSpec>,
    /// The `objects` array interpreted as particle systems (M3f), in
    /// scene.json order. The loader resolves and decodes each system's
    /// material and fills `texture`.
    pub particles: Vec<ParticleSpec>,
    /// Objects that can draw in this build (see ObjectCounts). The worker
    /// refuses a scene that declares objects and can draw none of them.
    pub drawable_objects: usize,
    /// Model-backed image objects skipped at parse (scene3d is BETA_M3h);
    /// counted for the worker's one-time diagnostic. A scene made only of
    /// these draws nothing at all — see the no-drawable-content guard in
    /// the worker and docs/bugs/SCENE_APPLY_BLANK_CLEAR_COLOR.md.
    pub model_layer_skips: usize,
    /// Text objects skipped because the scene declares more than
    /// text::MAX_TEXT_LAYERS of them. Never a rejection — the extra layers
    /// just do not register; counted for the worker's one-time diagnostic.
    pub text_layer_skips: usize,
    /// Particle systems skipped because the scene declares more than
    /// particles::MAX_PARTICLE_SYSTEMS of them (counted for the worker's
    /// one-time diagnostic; the particle pool is separate from the layer
    /// cap).
    pub particle_system_skips: usize,
    /// Video layers past video::MAX_VIDEO_LAYERS (M3g). The layers still
    /// register — a script can move and read them — but their source is
    /// cleared at parse so no decoder ever opens; counted for the worker's
    /// one-time diagnostic.
    pub video_layer_skips: usize,
    /// Particle objects whose `particle` value is a string — an external
    /// particle definition file (a researched WE feature). The M3f parse
    /// registers such systems with all defaults (the file-level definition
    /// merge is planned); counted for the worker's one-time diagnostic.
    pub particle_file_refs: usize,
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
    /// Position in the scene.json `objects` array — the file's object
    /// order. The per-kind draw lists merge by this key (layers.rs
    /// merged_draws), so an image that appears after a particle system in
    /// the file draws on top of it.
    pub scene_order: usize,
    /// Raw `image` reference exactly as written: a path relative to the
    /// content root (file scenes) or a package entry path (pkg scenes).
    /// `None` when the field is present but not a string (a
    /// property-wrapped `{"user": ..., "value": ...}` reference whose
    /// value is not a string): the layer is still registered so the script
    /// can reach it, but skipped at load with a bounded diagnostic.
    pub image: Option<String>,
    /// Raw model `image` reference (S1), `Some` exactly when the object
    /// classified as `SceneObjectKind::Model` — a `.json` model file whose
    /// `material` resolves to a texture through `kwe_core::scenemodel`.
    /// `image` stays `None` for a model layer; `load_model_textures`
    /// resolves this field instead of the usual `resolve`+`decode_texture`
    /// path, because resolution needs the model->material->texture walk
    /// and the pkg/dir/assets-root lookup order, not a single reference.
    pub model_ref: Option<String>,
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
    /// Video-layer content (M3g), `Some` exactly when the object was a
    /// video layer (has `video`, no `image`). The layer then draws through
    /// the video path: a libmpv software-render decoder writes RGBA frames
    /// into the layer's texture slot every frame (video.rs). `texture`
    /// stays `None` for video layers — their pixels never come from the
    /// image decoder.
    pub video: Option<VideoSpec>,
    /// S2: the resolved material data `load_model_textures` extracted
    /// alongside the base texture — `Some` only for a model layer whose
    /// `kwe_core::resolve_model` walk succeeded. `compile_material_layers`
    /// (main.rs) consumes this to attempt a material-pipeline draw; a
    /// layer with `material.is_none()` (not a model, or resolution
    /// failed) always uses the S1 base-texture quad.
    pub material: Option<MaterialSpec>,
    /// S3: `model.json`'s own `"fullscreen"` flag
    /// (`models/util/fullscreenlayer.json` in the real corpus — a
    /// `copybackground` post-process layer with no static texture of its
    /// own). When true and `size` is `[0, 0]` (no explicit scene.json
    /// size and no decoded base texture to size from — expected for a
    /// layer whose only texture slot is a `_rt_` runtime target),
    /// `load_model_textures` sizes the layer to the scene's world extent
    /// instead of leaving it a degenerate zero-size quad.
    pub fullscreen: bool,
    /// S3: this object's raw `effects` JSON array entries exactly as
    /// written, unresolved — parsing needs the same pkg/dir/assets-root
    /// lookup `load_model_textures` already has, so resolution happens
    /// there (`kwe_core::sceneeffect::resolve_object_effects`), not at
    /// scene.json parse time. Empty for every layer kind except a model
    /// layer whose object JSON carries an `effects` array.
    pub effects_raw: Vec<serde_json::Value>,
    /// S3: `effects_raw` resolved by `load_model_textures` (the same
    /// lookup closure that resolves `model_ref`) — `kwe_core::sceneeffect::
    /// resolve_object_effects`'s output. Empty when `effects_raw` was
    /// empty, or when every declared effect failed to resolve (the
    /// module's own honesty rule: this never blocks the layer's own base
    /// draw, it only means the renderer has no effect chain to run for
    /// this layer this frame).
    pub effects: Vec<kwe_core::ObjectEffect>,
}

/// S3: one resolved effect-pass texture slot, thinned from
/// `kwe_core::sceneeffect::EffectTextureSlot` the same way `MaterialSpec`
/// thins `ResolvedModel` — an owned copy so `scene.rs` stays free of
/// `kwe_core::sceneeffect`'s full surface.
#[derive(Debug, Clone)]
pub enum MaterialTextureSource {
    /// A real, resolved `.tex` asset's raw (undecoded) bytes.
    Bytes(Vec<u8>),
    /// A `_rt_`/`_alias_` runtime target name, resolved to a live FBO (or
    /// the shared dummy texture if nothing produces it this frame) by the
    /// renderer at draw time — never a filesystem lookup.
    RenderTarget(String),
}

/// S2: everything `compile_material_layers` needs from
/// `kwe_core::scenemodel::ResolvedModel` to attempt compiling and binding
/// a material pipeline for one layer. A thin, owned copy (rather than
/// keeping the whole `ResolvedModel` around) so `scene.rs` does not need
/// to depend on `kwe_core::scenemodel`'s full surface.
#[derive(Debug, Clone, Default)]
pub struct MaterialSpec {
    pub shader: Option<String>,
    pub blending: Option<String>,
    pub combos: std::collections::BTreeMap<String, i64>,
    /// Ordered `constantshadervalues` names -> their declared value, as
    /// written (string or number in the JSON — `compile_material_layers`
    /// parses to `f32`, skipping a value it cannot parse).
    pub constant_shader_values: Vec<(String, serde_json::Value)>,
    /// Positional `g_Texture<N>` slot source, `None` for an empty/
    /// unresolved slot — mirrors
    /// `kwe_core::scenemodel::ResolvedModel::texture_slots`. Raw bytes are
    /// decoded by `compile_material_layers` the same way
    /// `load_model_textures` already decodes slot 0; a `RenderTarget`
    /// name is resolved by the renderer at bind/draw time (S3).
    pub texture_slots: Vec<Option<MaterialTextureSource>>,
}

/// One `objects` entry interpreted as a video layer (M3g). The
/// classification key is `video`, checked after `image` and before
/// `particle`.
///
/// **Corpus honesty (the load-bearing caveat for this whole slice):** the
/// 60-package corpus contains **zero video layers and zero video files** —
/// no `objects` entry carries a `video`/`movie` key, and no package entry
/// has a video extension (verified by a full re-scan of every scene.json
/// and every package entry table at M3g). Unlike M3c's image layers, no
/// field below can be corroborated against real content. The schema is
/// therefore derived from the researched WE object model — video layers
/// reuse the shared `objects` props exactly like image and text layers do
/// — plus the playback keys the renderer needs, and it is exercised by
/// synthetic fixtures only. Every field is property-wrapped like the M3c
/// fields; playback scalars CLAMP to the documented ranges (the M3f
/// convention) rather than rejecting the scene, because a video that
/// cannot play must never cost the user the rest of the wallpaper.
#[derive(Debug, Clone)]
pub struct VideoSpec {
    /// Raw `video` reference exactly as written: a path relative to the
    /// content root (file scenes) or a package entry path (pkg scenes).
    /// `None` when the field is present but not a string — the layer is
    /// still registered so the script can reach it, but no decoder opens
    /// and it draws nothing (the M3c missing-image policy).
    pub source: Option<String>,
    /// Whether playback restarts at EOF. WE video wallpapers loop by
    /// default and there is no corpus key to contradict it, so the default
    /// is `true`; `false` leaves the last decoded frame on screen (libmpv
    /// `keep-open`), which is the only non-looping behavior that keeps the
    /// layer visible.
    pub loop_playback: bool,
    /// Playback speed multiplier, clamped to
    /// `video::MIN_PLAYBACK_RATE..=video::MAX_PLAYBACK_RATE`, default 1.0.
    /// A design field, not a documented WE key (recorded as a deviation):
    /// the deterministic smoke oracles need a way to pin playback speed.
    pub rate: f32,
    /// The resolved on-disk path, filled by main.rs after parse (`None`
    /// until then, and for a layer whose source did not resolve). libmpv
    /// opens a path, not a byte slice, so a package-embedded video is
    /// extracted into the worker's private HOME first — unlike an image,
    /// which is decoded straight out of the package in memory.
    pub path: Option<PathBuf>,
}

/// One `objects` entry interpreted as a particle system (M3f). The
/// classification key is `particle`, checked after `image` and before
/// `text` — the researched WE order (image, sound, particle, text): an
/// object value is an inline definition, a string is a reference to an
/// external particle definition file (counted and registered with
/// defaults — the merge is planned).
///
/// WE describes particle systems with a component model (emitter /
/// initializer / operator / renderer arrays — researched from
/// docs.wallpaperengine.io, recorded in the M3f doc section); M3f
/// implements a flat subset of the emitter surface with documented
/// defaults. A real WE particle object therefore parses with all defaults
/// (the component-model parse is planned); every scalar below is
/// property-wrapped like the M3c fields, and out-of-range values CLAMP to
/// the documented ranges (the M3f task contract — deviating from layer
/// property strictness, recorded in the matrix).
#[derive(Debug, Clone)]
pub struct ParticleSpec {
    /// The script's identity (`Scene.getParticleSystem(name)`; also
    /// reachable through Scene.getLayer, WE-style).
    pub name: String,
    /// Position in the scene.json `objects` array — the file's object
    /// order (the draw-order merge key, layers.rs merged_draws; NOT the
    /// PRNG seed — the runtime seeds by system index).
    pub scene_order: usize,
    /// Spawn position in scene units; (0,0) = scene center, +y down. The
    /// object's `angles`/`scale` are parsed (shared props) but NOT applied
    /// in M3f — particle systems render world-space (documented deviation;
    /// the system transform is planned).
    pub origin: [f32; 2],
    /// Particles per second, 0..=4096, default 10.
    pub spawn_rate: f32,
    /// Seconds, 0.1..=60, default 1.0.
    pub life: f32,
    /// Launch speed range in px/s, 0..=1e6, default 0. `speedMin`/
    /// `speedMax` win over a bare `speed`; the pair normalizes (min ≤ max).
    pub speed_min: f32,
    pub speed_max: f32,
    /// Launch direction in radians from +x (y down), default 0. WE
    /// emitters have no direction/spread fields (velocity comes from the
    /// velocity-random initializer) — the flat model is the M3f extension
    /// the deterministic smoke oracles need (documented deviation).
    pub direction: f32,
    /// Launch angle spread in radians, 0..=2π, default 0 (all particles
    /// take the exact direction).
    pub spread: f32,
    /// Acceleration in px/s², ±1e6, default [0, 0]. A scalar applies to y
    /// (down); arrays take 1..=3 components.
    pub gravity: [f32; 2],
    /// Quad endpoints in px, 1..=512, default 8; interpolated over life.
    pub size_start: f32,
    pub size_end: f32,
    /// RGBA straight-alpha, 0..=1 each (3 components imply alpha 1),
    /// default white; interpolated over life.
    pub color_start: [f32; 4],
    pub color_end: [f32; 4],
    /// Alpha endpoints 0..=1, default 1 → 0 (particles fade out).
    pub alpha_start: f32,
    pub alpha_end: f32,
    /// `material` is the WE texture key; the brief's `texture` wins when
    /// both are present. Raw reference exactly as written; `None` when
    /// present but not a string (the system registers and simulates, but
    /// draws nothing — skipped at load with a bounded diagnostic).
    pub material: Option<String>,
    /// Live-particle cap, 1..=particles::MAX_PARTICLES, default 1000
    /// (documented deviation from WE's 100). Excess spawns drop, never
    /// evict live particles (particles.rs).
    pub max_count: u32,
    /// `blendMode`/`colorBlendMode` exactly as written, pre-clamp (the
    /// runtime clamps to the implemented set like every layer).
    pub blend_mode: u32,
    /// Object alpha 0..=1 (default 1) — the drawn alpha; read-only in M3f
    /// (the script surface covers the instance factors only).
    pub alpha: f32,
    pub visible: bool,
    /// Brightness 0..=10 (default 1) — drawn effects; read-only in M3f.
    pub brightness: f32,
    /// Decoded texture, filled by the loader (main.rs); `None` when the
    /// material is missing, unreadable, over budget, or not decodable —
    /// the system stays registered but draws nothing.
    pub texture: Option<DecodedTexture>,
    /// S4b: the raw `particle` value when it was a STRING — an external
    /// particle definition file reference — set by `parse_particle_system`
    /// instead of parsing any flat-model fields (a string value carries no
    /// inline definition). `main.rs`'s `load_particle_file_definitions`
    /// resolves this the same way a model layer's `image` resolves
    /// (`kwe_core::particlefile::resolve_particle_file`) and fills
    /// `component`/`max_count`/`texture` on success; `None` for every
    /// inline (object-valued) particle system.
    pub file_ref: Option<String>,
    /// S4b: the parsed component model (emitter/initializer/operator
    /// arrays), filled by `load_particle_file_definitions` only when
    /// `file_ref` resolved AND parsed. `None` keeps this system on the
    /// flat M3f model (every inline system, and any file reference that
    /// failed to resolve/parse — the existing honest fallback: the system
    /// stays registered with its M3f defaults rather than vanishing).
    pub component: Option<particles::ComponentModel>,
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
    let (layers, particles, counts) = parse_objects(root_obj)?;

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
        particles,
        drawable_objects: counts.drawable_objects,
        model_layer_skips: counts.model_layer_skips,
        text_layer_skips: counts.text_layer_skips,
        particle_system_skips: counts.particle_system_skips,
        video_layer_skips: counts.video_layer_skips,
        particle_file_refs: counts.particle_file_refs,
        text_on_image_objects: counts.text_on_image_objects,
        text_size_ignored: counts.text_size_ignored,
    })
}

/// Bounded counts the parser collects while interpreting `objects` (M3e,
/// M3f): never rejections, only one-time worker diagnostics.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct ObjectCounts {
    /// Objects that can put pixels on the screen in this build, by the
    /// shared classification (`kwe_core::SceneObjectKind::can_draw`): a
    /// decodable image reference, a video layer, a text layer, or a
    /// particle system with an inline material. Zero means the scene
    /// composites to bare clear colour no matter what its script does.
    pub drawable_objects: usize,
    /// Objects whose `image` is a `.json` model reference (scene3d,
    /// BETA_M3h). Skipped before any validation; counted here so the
    /// worker can say so out loud instead of compositing bare clear
    /// colour in silence (B2).
    pub model_layer_skips: usize,
    pub text_layer_skips: usize,
    pub text_on_image_objects: usize,
    pub text_size_ignored: usize,
    /// Particle objects past particles::MAX_PARTICLE_SYSTEMS (skipped).
    pub particle_system_skips: usize,
    /// Particle objects whose `particle` value is a string — external
    /// particle definition file references (registered with defaults; the
    /// file-level merge is planned).
    pub particle_file_refs: usize,
    /// Video objects past `video::MAX_VIDEO_LAYERS` (M3g). They still
    /// register as layers — the script can read and write their props —
    /// but no decoder opens for them, so they draw nothing.
    pub video_layer_skips: usize,
}

/// The `objects` array, interpreted as image (M3c), particle (M3f), and
/// text (M3e) layers in scene.json order (the compositor's draw order —
/// the layer on top is drawn last, over the others). An object is an image
/// layer exactly when it carries an `image` field; a particle system
/// exactly when it carries a `particle` field without `image` (classified
/// after image, before text — the researched WE order: image, sound,
/// particle, text); a text layer exactly when it carries a `text` field
/// without `image` (an object with both counts as image —
/// `text_on_image_objects` — and an object with neither is ignored:
/// models, audio — M3d+). A reference that ends in `.json` is a model
/// instance under the WE solid-model architecture (620 of the 685 corpus
/// image references point at model files; the other 65 carry a null image
/// value); `parse_model_layer` runs BEFORE any validation error can
/// propagate (S1 — it returns `None` instead of `Err`), so a malformed
/// model layer (no name, out-of-range alpha, ...) can never reject the
/// scene, exactly like the pre-S1 skip. A model layer that DOES parse now
/// registers like an image layer and counts toward the layer cap (M3d+
/// note updated for S1).
///
/// Text layers beyond text::MAX_TEXT_LAYERS are skipped (counted, never a
/// rejection); particle systems beyond particles::MAX_PARTICLE_SYSTEMS are
/// skipped the same way. The layer caps (MAX_TEXT_LAYERS and MAX_LAYERS)
/// apply to the layers that register; the particle pool is separate.
fn parse_objects(
    root_obj: &serde_json::Map<String, Value>,
) -> Result<(Vec<LayerSpec>, Vec<ParticleSpec>, ObjectCounts), SceneError> {
    let Some(value) = root_obj.get("objects") else {
        return Ok((Vec::new(), Vec::new(), ObjectCounts::default()));
    };
    let array = value.as_array().ok_or_else(|| {
        SceneError::new(
            SceneErrorKind::Shape,
            "scene.json \"objects\" must be an array",
        )
    })?;
    let mut layers = Vec::new();
    let mut particles = Vec::new();
    let mut text_layers = 0usize;
    let mut video_layers = 0usize;
    let mut counts = ObjectCounts::default();
    for (index, entry) in array.iter().enumerate() {
        let object = entry.as_object().ok_or_else(|| {
            SceneError::new(
                SceneErrorKind::Shape,
                format!("scene.json \"objects[{index}]\" must be an object"),
            )
        })?;
        // The kind decision is `kwe_core::classify_scene_object` — the
        // same rule preflight uses to answer "can this scene draw
        // anything?", so a scene the daemon accepted and a scene this
        // parser builds layers for can never disagree (B2). The caps and
        // the per-kind parsing stay here.
        let kind = classify_scene_object(object);
        if kind.can_draw() {
            counts.drawable_objects += 1;
        }
        match kind {
            // A model instance (S1): WE stores every visual (2D
            // included) as a model. The classification runs on the raw
            // (property-unwrapped) reference BEFORE parse_model_layer, so
            // a malformed model layer skips (parse_model_layer returns
            // `None`) exactly like the pre-S1 contract instead of
            // rejecting the whole scene. `model_layer_skips` now counts
            // every model object seen (registered or not) — the field
            // name predates S1's texture resolution; see the worker's own
            // diagnostic (main.rs) for what actually failed to resolve.
            SceneObjectKind::Model => {
                counts.model_layer_skips += 1;
                if let Some(layer) = parse_model_layer(object, index) {
                    layers.push(layer);
                }
            }
            // An image layer (M3c). TEXV (.tex) references and non-string
            // references register the same way: the layer exists for the
            // script, and the load step skips the texture with its own
            // diagnostic.
            SceneObjectKind::Image
            | SceneObjectKind::TexvImage
            | SceneObjectKind::TexturelessImage => {
                if object.contains_key("text") {
                    counts.text_on_image_objects += 1;
                }
                let layer = parse_image_layer(object, index)?;
                layers.push(layer);
            }
            SceneObjectKind::Video => {
                // A video layer (M3g). Placed after `image` and before
                // `particle`: video is not in the researched WE classification
                // order (image, sound, particle, text) because the corpus has
                // no video objects to place it with, so the position is a
                // documented design decision — adjacent to `image` because a
                // video layer IS an image layer whose texture is a movie, and
                // ahead of `particle`/`text` so an object carrying both keys
                // resolves to the kind that owns the texture slot.
                //
                // Layers past the concurrency cap still register (the script
                // reaches them through Scene.getLayer) but never open a
                // decoder — counted, never a rejection, exactly like the
                // particle-system cap.
                let over_cap = video_layers >= crate::video::MAX_VIDEO_LAYERS;
                if over_cap {
                    counts.video_layer_skips += 1;
                } else {
                    video_layers += 1;
                }
                let mut layer = parse_video_layer(object, index)?;
                if over_cap {
                    layer.video = layer.video.map(|spec| VideoSpec {
                        source: None, // over the cap: registered, never decoded
                        ..spec
                    });
                }
                layers.push(layer);
            }
            // A particle system (M3f). An object whose `image` is present
            // but NOT a string reaches this arm through the shared
            // classifier: the editor writes `"image": null` on every
            // particle object, and before B2 those took the image branch
            // and registered as textureless image layers, so the particle
            // systems silently vanished (65 of 65 in the local corpus).
            SceneObjectKind::Particle | SceneObjectKind::ParticleFile => {
                // A string `particle` value is a reference to an external
                // particle definition file (a WE feature): counted for the
                // worker's one-time diagnostic; the system registers with all
                // defaults (the file-level definition merge is planned).
                // Systems past the cap are skipped (counted, never a
                // rejection).
                if particles.len() >= particles::MAX_PARTICLE_SYSTEMS {
                    counts.particle_system_skips += 1;
                    continue;
                }
                if scene_property_value(object.get("particle").expect("caller checked")).is_string()
                {
                    counts.particle_file_refs += 1;
                }
                particles.push(parse_particle_system(object, index)?);
            }
            SceneObjectKind::Text => {
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
            }
            SceneObjectKind::Other => continue, // audio, ... — M3d+
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
    Ok((layers, particles, counts))
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
                scene_property_value(value),
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
                scene_property_value(value),
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
                scene_property_value(value),
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
            let alpha = scene_property_value(value).as_f64().ok_or_else(|| {
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
        Some(value) => match scene_property_value(value) {
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
        Some(value) => match scene_property_value(value).as_f64() {
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
            let value = scene_property_value(value);
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

/// `size` and `tint`/`color`: shared by image and model layers (S1) — a
/// model instance is drawn as a quad through the same geometry pipeline an
/// image layer uses, so it carries the same two fields.
fn parse_size_and_tint(
    object: &serde_json::Map<String, Value>,
    index: usize,
) -> Result<([f32; 2], [f32; 4]), SceneError> {
    let size = match object.get("size") {
        None => [0.0, 0.0],
        Some(value) => {
            let vector = parse_vector(
                scene_property_value(value),
                &field(index, "size"),
                &[2],
                true,
            )?;
            [vector[0], vector[1]]
        }
    };

    // `tint` (the brief's key, 3 or 4 components) takes precedence over
    // `color` (the WE file key, a vec3 — its alpha is implied 1.0, the
    // g_Color4 semantics of WE's shader library). Components clamp 0..=1.
    let tint = match object.get("tint").or_else(|| object.get("color")) {
        None => [1.0, 1.0, 1.0, 1.0],
        Some(value) => {
            let vector = parse_vector(
                scene_property_value(value),
                &field(index, "tint"),
                &[3, 4],
                false,
            )?;
            let mut tint = [1.0, 1.0, 1.0, 1.0];
            for (slot, component) in tint.iter_mut().zip(vector.iter()) {
                *slot = crate::layers::clamp_layer_tint(f64::from(*component));
            }
            tint
        }
    };

    Ok((size, tint))
}

/// One model layer entry (S1, `SceneObjectKind::Model`). A model instance
/// draws as a quad through the same geometry fields an image layer uses
/// (origin/angles/scale/size/alpha/visible/blend/brightness/tint); its
/// pixel source is `model_ref` — resolved later (`load_model_textures`,
/// `kwe_core::scenemodel::resolve_model`) against the pkg/scene-dir/assets
/// lookup chain, not a direct image reference.
///
/// Unlike `parse_image_layer`, a parse failure here is never a scene
/// rejection: this is the pre-S1 "skip-never-reject" contract for model
/// layers (`malformed_model_layers_skip_never_reject` — a model layer with
/// no name, an out-of-range alpha, or any other malformed field must not
/// take the whole scene down). `None` means "this object registers
/// nothing" — the caller skips it exactly as if it never resolved.
fn parse_model_layer(object: &serde_json::Map<String, Value>, index: usize) -> Option<LayerSpec> {
    let common = parse_common_props(object, index, "model").ok()?;
    let model_ref = match scene_property_value(object.get("image").expect("caller checked")) {
        Value::String(reference) => reference.clone(),
        // classify_scene_object requires a string to reach Model at all;
        // defensive only.
        _ => return None,
    };
    let (size, tint) = parse_size_and_tint(object, index).ok()?;
    // S3: keep the raw `effects` array for `load_model_textures` to
    // resolve (it needs the pkg/dir/assets-root lookup this pure-JSON
    // parse step does not have). Bounded here too, before the clone, so
    // a scene declaring an absurd number of effect entries never costs
    // more than one bounded `Vec::clone` at parse time —
    // `kwe_core::sceneeffect::resolve_object_effects` applies the same
    // cap again later, independently.
    let effects_raw = object
        .get("effects")
        .and_then(Value::as_array)
        .map(|entries| {
            entries
                .iter()
                .take(kwe_core::MAX_EFFECTS_PER_OBJECT)
                .cloned()
                .collect()
        })
        .unwrap_or_default();

    Some(LayerSpec {
        name: common.name,
        scene_order: index,
        image: None,
        model_ref: Some(model_ref),
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
        video: None,
        material: None,
        fullscreen: false,
        effects_raw,
        effects: Vec::new(),
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
    let image = match scene_property_value(object.get("image").expect("caller checked")) {
        Value::String(reference) => Some(reference.clone()),
        _ => None,
    };

    let (size, tint) = parse_size_and_tint(object, index)?;

    Ok(LayerSpec {
        name: common.name,
        scene_order: index,
        image,
        model_ref: None,
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
        video: None,
        material: None,
        fullscreen: false,
        effects_raw: Vec::new(),
        effects: Vec::new(),
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

    let text = match scene_property_value(object.get("text").expect("caller checked")) {
        Value::String(s) => s.clone(),
        _ => String::new(),
    };
    let font = match object.get("font").map(scene_property_value) {
        Some(Value::String(s)) if !s.is_empty() => Some(s.clone()),
        _ => None,
    };

    // OWE TextPointSizeToPx: px = round(pointsize * 4.0), clamped to our
    // bounded range (4..=512 px; OWE clamps 1..=1024 — documented
    // deviation). Non-finite or ≤ 0 sizes use the default (12 pt -> 48 px).
    let pointsize = match object.get("pointsize") {
        None => crate::text::DEFAULT_POINT_SIZE * crate::text::POINT_TO_PX,
        Some(value) => {
            let value = scene_property_value(value);
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
                scene_property_value(value),
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
        scene_order: index,
        image: None,
        model_ref: None,
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
        video: None,
        material: None,
        fullscreen: false,
        effects_raw: Vec::new(),
        effects: Vec::new(),
    })
}

/// One video layer entry (M3g). Shared props come from parse_common_props,
/// so a video layer honors origin/angles/scale/alpha/visible/blend/
/// brightness exactly like an image layer. The video-only keys are
/// `video` (the source reference, plain or property-wrapped), `loop`
/// (default true), and `rate` (clamped playback speed).
///
/// Like an image layer, `size` [0, 0] (absent or zero) means "the decoded
/// video's own dimensions", substituted once the decoder reports them.
/// Unlike an image layer, an unreadable source is ALWAYS survivable: the
/// layer registers, the decoder never opens, and the scene renders
/// without it.
fn parse_video_layer(
    object: &serde_json::Map<String, Value>,
    index: usize,
) -> Result<LayerSpec, SceneError> {
    let common = parse_common_props(object, index, "video")?;

    let source = match scene_property_value(object.get("video").expect("caller checked")) {
        Value::String(reference) => Some(reference.clone()),
        _ => None,
    };

    let size = match object.get("size") {
        None => [0.0, 0.0],
        Some(value) => {
            let vector = parse_vector(
                scene_property_value(value),
                &field(index, "size"),
                &[2],
                true,
            )?;
            [vector[0], vector[1]]
        }
    };

    // Same precedence as image layers: the brief's `tint` wins over WE's
    // `color`, components clamp 0..=1.
    let tint = match object.get("tint").or_else(|| object.get("color")) {
        None => [1.0, 1.0, 1.0, 1.0],
        Some(value) => {
            let vector = parse_vector(
                scene_property_value(value),
                &field(index, "tint"),
                &[3, 4],
                false,
            )?;
            let mut tint = [1.0, 1.0, 1.0, 1.0];
            for (slot, component) in tint.iter_mut().zip(vector.iter()) {
                *slot = crate::layers::clamp_layer_tint(f64::from(*component));
            }
            tint
        }
    };

    // Playback keys clamp instead of rejecting (the M3f convention): a
    // hostile or sloppy `rate` costs the user a speed, never the scene. A
    // non-boolean `loop` falls back to the default rather than rejecting,
    // for the same reason.
    let loop_playback = match object.get("loop").map(scene_property_value) {
        None | Some(Value::Null) => true,
        Some(Value::Bool(value)) => *value,
        // The editor serializes some scalars as strings; accept the two
        // spellings it could produce and default anything else.
        Some(Value::String(value)) => !matches!(
            value.trim().to_ascii_lowercase().as_str(),
            "false" | "0" | "no"
        ),
        Some(Value::Number(number)) => number.as_f64().is_none_or(|value| value != 0.0),
        Some(_) => true,
    };
    let rate = match object.get("rate").map(scene_property_value) {
        None | Some(Value::Null) => 1.0,
        Some(value) => {
            let raw = value
                .as_f64()
                .or_else(|| value.as_str().and_then(|text| text.trim().parse().ok()))
                .unwrap_or(1.0);
            crate::video::clamp_playback_rate(raw)
        }
    };

    Ok(LayerSpec {
        name: common.name,
        scene_order: index,
        image: None,
        model_ref: None,
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
        video: Some(VideoSpec {
            source,
            loop_playback,
            rate,
            path: None,
        }),
        material: None,
        fullscreen: false,
        effects_raw: Vec::new(),
        effects: Vec::new(),
    })
}

/// A particle spec with every flat emitter-model field at its documented
/// default (common props land from the shared parse path). `index` is the
/// object's position in the `objects` array (the draw-order merge key).
fn particle_spec_defaults(common: CommonProps, index: usize) -> ParticleSpec {
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
        file_ref: None,
        component: None,
    }
}

/// One particle system entry (M3f). The `particle` value is the
/// definition: an object of flat emitter-model fields, or — the WE
/// external particle file form (a string) / any malformed value — the
/// system registers with all documented defaults (skip never reject; the
/// file-level merge is planned). Scalar fields are property-wrapped like
/// every M3c scalar and accept numeric strings (the corpus editor
/// serialization); missing or non-finite values take the documented
/// default and out-of-range values CLAMP to the documented ranges — the
/// M3f task contract, deviating from the layer properties' strict
/// rejection (recorded in the coverage matrix). The WE component model
/// keys (emitter/initializer/operator/renderer/controlpoint/children/
/// flags/...) are tolerated extra keys that parse to nothing in M3f.
fn parse_particle_system(
    object: &serde_json::Map<String, Value>,
    index: usize,
) -> Result<ParticleSpec, SceneError> {
    let common = parse_common_props(object, index, "particle")?;
    let mut spec = particle_spec_defaults(common, index);
    let raw = scene_property_value(object.get("particle").expect("caller checked"));
    if let Value::String(reference) = raw {
        // S4b: an external particle definition file reference. The actual
        // component-model parse happens at load time (main.rs's
        // `load_particle_file_definitions`, which has the lane-specific
        // lookup closure this pure-parse stage does not) — record the
        // reference so that step can find it; the spec keeps its M3f flat
        // defaults until (and unless) the file resolves.
        spec.file_ref = Some(reference.clone());
        return Ok(spec);
    }
    let Value::Object(definition) = raw else {
        return Ok(spec); // malformed value (not object, not string): defaults
    };

    // A bounded float-or-numeric-string scalar (the brightness style):
    // missing/non-finite -> the default, out-of-range -> the clamp.
    let scalar = |name: &str, fallback: f64, clamp: fn(f64) -> f32| -> Result<f32, SceneError> {
        let Some(value) = definition.get(name) else {
            return Ok(clamp(fallback));
        };
        let value = scene_property_value(value);
        let number = if let Some(number) = value.as_f64() {
            number
        } else if let Some(text) = value.as_str() {
            text.parse::<f64>().map_err(|_| {
                SceneError::new(
                    SceneErrorKind::Shape,
                    format!(
                        "scene.json \"{}\" must be a float or a numeric string",
                        field(index, name)
                    ),
                )
            })?
        } else {
            return Err(SceneError::new(
                SceneErrorKind::Shape,
                format!(
                    "scene.json \"{}\" must be a float or a numeric string",
                    field(index, name)
                ),
            ));
        };
        Ok(clamp(number))
    };

    spec.spawn_rate = scalar(
        "spawnRate",
        f64::from(particles::DEFAULT_PARTICLE_SPAWN_RATE),
        particles::clamp_spawn_rate,
    )?;
    spec.life = scalar(
        "life",
        f64::from(particles::DEFAULT_PARTICLE_LIFE),
        particles::clamp_life,
    )?;
    // `speedMin`/`speedMax` win over a bare `speed`; a missing `speedMax`
    // falls back to the resolved minimum (the pair supersedes `speed` —
    // [90, 100] from `speed: 100, speedMin: 90` would silently stretch the
    // range), and a reversed pair normalizes so min <= max (the runtime
    // picks in [min, max]).
    let speed = scalar(
        "speed",
        f64::from(particles::DEFAULT_PARTICLE_SPEED),
        particles::clamp_speed,
    )?;
    let speed_min = scalar("speedMin", f64::from(speed), particles::clamp_speed)?;
    let speed_max = scalar("speedMax", f64::from(speed_min), particles::clamp_speed)?;
    let (speed_min, speed_max) = if speed_min <= speed_max {
        (speed_min, speed_max)
    } else {
        (speed_max, speed_min)
    };
    spec.speed_min = speed_min;
    spec.speed_max = speed_max;
    spec.direction = scalar(
        "direction",
        f64::from(particles::DEFAULT_PARTICLE_DIRECTION),
        particles::clamp_direction,
    )?;
    spec.spread = scalar(
        "spread",
        f64::from(particles::DEFAULT_PARTICLE_SPREAD),
        particles::clamp_spread,
    )?;
    spec.size_start = scalar(
        "sizeStart",
        f64::from(particles::DEFAULT_PARTICLE_SIZE),
        particles::clamp_size,
    )?;
    spec.size_end = scalar(
        "sizeEnd",
        f64::from(particles::DEFAULT_PARTICLE_SIZE),
        particles::clamp_size,
    )?;
    spec.alpha_start = scalar(
        "alphaStart",
        f64::from(particles::DEFAULT_PARTICLE_ALPHA_START),
        particles::clamp_alpha,
    )?;
    spec.alpha_end = scalar(
        "alphaEnd",
        f64::from(particles::DEFAULT_PARTICLE_ALPHA_END),
        particles::clamp_alpha,
    )?;

    // gravity: arrays take 1..=3 components ([g] -> [0, g], the extra z is
    // dropped); scalar shapes reject like every WE vector (parse_vector) —
    // the scalar fields clamp, the vector fields keep the M3c behavior,
    // documented in the matrix.
    spec.gravity = match definition.get("gravity") {
        None => particles::DEFAULT_PARTICLE_GRAVITY,
        Some(value) => {
            let vector = parse_vector(
                scene_property_value(value),
                &field(index, "gravity"),
                &[1, 2, 3],
                false,
            )?;
            match vector.as_slice() {
                [g] => [0.0, *g],
                [x, y] => [*x, *y],
                [x, y, _] => [*x, *y],
                _ => unreachable!("parse_vector enforces the allowed lengths"),
            }
        }
    };

    spec.color_start = parse_particle_color(definition, "colorStart", index)?;
    spec.color_end = parse_particle_color(definition, "colorEnd", index)?;

    // maxCount (the WE key): the live-particle cap, 1..=MAX_PARTICLES.
    if let Some(value) = definition.get("maxCount") {
        let value = scene_property_value(value);
        let count = if let Some(number) = value.as_u64() {
            number
        } else if let Some(text) = value.as_str() {
            text.parse::<u64>().map_err(|_| {
                SceneError::new(
                    SceneErrorKind::Shape,
                    format!(
                        "scene.json \"{}\" must be an integer or a numeric string",
                        field(index, "maxCount")
                    ),
                )
            })?
        } else {
            return Err(SceneError::new(
                SceneErrorKind::Shape,
                format!(
                    "scene.json \"{}\" must be an integer or a numeric string",
                    field(index, "maxCount")
                ),
            ));
        };
        spec.max_count = particles::clamp_max_count(count);
    }

    // `material` is the WE texture key; the brief's `texture` wins when
    // both are present. A non-string value is None — the system registers
    // and simulates, but draws nothing (skipped at load like a non-string
    // layer image).
    let material = definition
        .get("texture")
        .or_else(|| definition.get("material"));
    spec.material = match material.map(scene_property_value) {
        Some(Value::String(reference)) => Some(reference.clone()),
        _ => None,
    };

    Ok(spec)
}

/// Parse one particle color field (M3f): a vec3/vec4 in the WE vector
/// forms (array or space-separated string, property-wrapped allowed), each
/// component clamped 0..=1 like the layer tint; 3 components imply alpha
/// 1.0. Missing -> opaque white.
fn parse_particle_color(
    definition: &serde_json::Map<String, Value>,
    name: &str,
    index: usize,
) -> Result<[f32; 4], SceneError> {
    let Some(value) = definition.get(name) else {
        return Ok([1.0, 1.0, 1.0, 1.0]);
    };
    let vector = parse_vector(
        scene_property_value(value),
        &field(index, name),
        &[3, 4],
        false,
    )?;
    let mut color = [1.0, 1.0, 1.0, 1.0];
    for (slot, component) in color.iter_mut().zip(vector.iter()) {
        *slot = particles::clamp_color_component(f64::from(*component));
    }
    Ok(color)
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
    if let Some(Some(value)) = object.get(key).map(scene_property_value).map(Value::as_str)
        && !value.is_empty()
    {
        return resolve(value);
    }
    if let Some(Some(alignment)) = object
        .get("alignment")
        .map(scene_property_value)
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
    if let Some(Some(value)) = object.get(key).map(scene_property_value).map(Value::as_str)
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

/// Parse a WE vector field: the space-separated string form the editor
/// writes (`"1920.00000 1080.00000 0.00000"` — verified on the corpus) or
/// an array of numbers. `allowed` lists the accepted component counts (the
/// editor writes three; two is accepted, and the extra z is dropped by the
/// caller). Every component must be finite and within ±1e6; `non_negative`
/// additionally forbids negative values (sizes — a mirror goes through
/// scale, per WE semantics).
pub(crate) fn parse_vector(
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
        parse_objects(root).map(|(layers, _, _)| layers)
    }

    /// Like parse_objects_of, but returning the particle systems (M3f).
    fn parse_particles_of(json: &str) -> Result<Vec<ParticleSpec>, SceneError> {
        let value: Value = serde_json::from_str(json).unwrap();
        let root = value.as_object().unwrap();
        parse_objects(root).map(|(_, particles, _)| particles)
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
    fn model_json_references_now_register_as_model_layers() {
        // S1: 620 of the corpus's 685 image references point at model
        // .json files. Before S1 they were all skipped ("scene3d,
        // BETA_M3h"); now a model reference with a valid name registers a
        // model layer (image stays None, model_ref carries the
        // reference) — the same as an image layer, minus the texture
        // reference — and counts toward the layer cap like one.
        let mut objects = r#"{"objects": ["#.to_string();
        for i in 0..200 {
            objects.push_str(&format!(
                r#"{{"name": "m{i}", "image": "models/util/m{i}.json"}},"#
            ));
        }
        objects.push_str(r#"{"name": "real", "image": "tex.png"}]}"#);
        let layers = parse_objects_of(&objects).unwrap();
        assert_eq!(layers.len(), 201);
        assert_eq!(layers[0].name, "m0");
        assert_eq!(layers[0].image, None);
        assert_eq!(layers[0].model_ref.as_deref(), Some("models/util/m0.json"));
        assert_eq!(layers[200].name, "real");
        assert_eq!(layers[200].model_ref, None);
    }

    /// S1: 200 well-formed model layers plus the pre-S1 malformed-model
    /// skip test (below) still push the model object count over
    /// MAX_LAYERS if uncapped; this keeps the count in bounds while still
    /// exercising 200 real model layers.
    #[test]
    fn too_many_model_and_image_layers_combined_rejected() {
        let mut objects = r#"{"objects": ["#.to_string();
        for i in 0..(MAX_LAYERS + 1) {
            objects.push_str(&format!(
                r#"{{"name": "m{i}", "image": "models/m{i}.json"}},"#
            ));
        }
        objects.push_str(r#"{"name": "real", "image": "tex.png"}]}"#);
        let error = parse_objects_of(&objects).unwrap_err();
        assert_eq!(error.kind, SceneErrorKind::Shape);
    }

    /// B2 (updated for S1): `model_layer_skips` now counts every model
    /// object seen, whether or not it went on to register a layer (the
    /// field name predates texture resolution). A scene made only of
    /// unresolvable model layers still degrades honestly — the worker
    /// adds the resolved count separately (main.rs) after
    /// `load_model_textures` runs, which this parse-only test does not
    /// exercise.
    #[test]
    fn model_layer_skips_counts_every_model_object_seen() {
        let value: Value = serde_json::from_str(
            r#"{"objects": [
                {"name": "a", "image": "models/a.json"},
                {"name": "b", "image": "models/b.JSON"},
                {"name": "real", "image": "tex.png"}
            ]}"#,
        )
        .unwrap();
        let (layers, particles, counts) = parse_objects(value.as_object().unwrap()).unwrap();
        assert_eq!(counts.model_layer_skips, 2);
        // Both well-formed model layers register now (S1) — plus the
        // image layer, that is 3.
        assert_eq!(layers.len(), 3);
        assert_eq!(layers[0].model_ref.as_deref(), Some("models/a.json"));
        assert_eq!(layers[1].model_ref.as_deref(), Some("models/b.JSON"));
        assert!(particles.is_empty());
    }

    /// B2 (the classification half): the editor writes `"image": null` on
    /// particle objects — all 65 of the local corpus's null-image objects
    /// are particle systems. They used to take the image branch and
    /// register as textureless image layers, which silently deleted the
    /// scene's only drawable content.
    #[test]
    fn null_image_particle_objects_register_as_particle_systems() {
        let value: Value = serde_json::from_str(
            r#"{"objects": [
                {"name": "sparkle", "image": null,
                 "particle": "particles/presets/magic_sparkle.json"},
                {"name": "dust", "image": null, "particle": {"material": "m.png"}}
            ]}"#,
        )
        .unwrap();
        let (layers, particles, counts) = parse_objects(value.as_object().unwrap()).unwrap();
        assert!(layers.is_empty(), "no textureless image layers register");
        assert_eq!(particles.len(), 2);
        assert_eq!(particles[0].name, "sparkle");
        assert_eq!(particles[1].material.as_deref(), Some("m.png"));
        assert_eq!(counts.particle_file_refs, 1);
    }

    /// An object with a non-string image and no other visual key keeps the
    /// pre-B2 behavior: it registers as a textureless image layer, so a
    /// script can still reach it by name.
    #[test]
    fn null_image_without_another_kind_still_registers_a_layer() {
        let layers =
            parse_objects_of(r#"{"objects": [{"name": "ghost", "image": null}]}"#).unwrap();
        assert_eq!(layers.len(), 1);
        assert_eq!(layers[0].name, "ghost");
        assert_eq!(layers[0].image, None);
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
        let (layers, _, counts) = parse_objects(root).unwrap();
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

    // ---- M3f: particle systems ----

    #[test]
    fn particle_systems_parsed_with_documented_defaults() {
        // A bare particle object: every flat emitter-model field takes its
        // documented default (spawnRate 10, life 1.0, speed 0, direction 0,
        // spread 0, gravity [0,0], size 8/8, white, alpha 1->0, maxCount
        // 1000) and the shared common props (origin, alpha, visible,
        // blendMode, brightness) land like every other layer kind.
        let particles =
            parse_particles_of(r#"{"objects": [{"name": "dust", "particle": {}}]}"#).unwrap();
        assert_eq!(particles.len(), 1);
        let spec = &particles[0];
        assert_eq!(spec.name, "dust");
        assert_eq!(spec.origin, [0.0, 0.0]);
        assert_eq!(spec.spawn_rate, particles::DEFAULT_PARTICLE_SPAWN_RATE);
        assert_eq!(spec.life, particles::DEFAULT_PARTICLE_LIFE);
        assert_eq!(spec.speed_min, 0.0);
        assert_eq!(spec.speed_max, 0.0);
        assert_eq!(spec.direction, 0.0);
        assert_eq!(spec.spread, 0.0);
        assert_eq!(spec.gravity, [0.0, 0.0]);
        assert_eq!(spec.size_start, 8.0);
        assert_eq!(spec.size_end, 8.0);
        assert_eq!(spec.color_start, [1.0, 1.0, 1.0, 1.0]);
        assert_eq!(spec.color_end, [1.0, 1.0, 1.0, 1.0]);
        assert_eq!(spec.alpha_start, 1.0);
        assert_eq!(spec.alpha_end, 0.0);
        assert_eq!(spec.max_count, 1000);
        assert_eq!(spec.material, None);
        assert_eq!(spec.blend_mode, 0);
        assert_eq!(spec.alpha, 1.0);
        assert!(spec.visible);
        assert_eq!(spec.brightness, 1.0);
        assert!(spec.texture.is_none());

        // Common props parse like every layer kind.
        let particles = parse_particles_of(
            r#"{"objects": [{"name": "dust", "particle": {},
                              "origin": "40 20 0", "alpha": 0.5,
                              "visible": false, "colorBlendMode": 6,
                              "brightness": "2.5"}]}"#,
        )
        .unwrap();
        assert_eq!(particles[0].origin, [40.0, 20.0]);
        assert_eq!(particles[0].alpha, 0.5);
        assert!(!particles[0].visible);
        assert_eq!(particles[0].blend_mode, 6);
        assert_eq!(particles[0].brightness, 2.5);
    }

    #[test]
    fn particle_flat_emitter_fields_parsed_with_clamps() {
        let particles = parse_particles_of(
            r#"{"objects": [{"name": "dust", "particle": {
                "spawnRate": 100,
                "life": 3.0,
                "speed": 60,
                "speedMin": 10,
                "speedMax": 90,
                "direction": 1.5,
                "spread": 2.0,
                "gravity": [0, 80],
                "sizeStart": 4,
                "sizeEnd": 16,
                "colorStart": "1 0 0",
                "colorEnd": [0, 1, 0, 0.5],
                "alphaStart": 1,
                "alphaEnd": 0.25,
                "maxCount": 500,
                "material": "textures/dot.png"
            }}]}"#,
        )
        .unwrap();
        let spec = &particles[0];
        assert_eq!(spec.spawn_rate, 100.0);
        assert_eq!(spec.life, 3.0);
        assert_eq!(spec.speed_min, 10.0);
        assert_eq!(spec.speed_max, 90.0);
        assert_eq!(spec.direction, 1.5);
        assert_eq!(spec.spread, 2.0);
        assert_eq!(spec.gravity, [0.0, 80.0]);
        assert_eq!(spec.size_start, 4.0);
        assert_eq!(spec.size_end, 16.0);
        assert_eq!(spec.color_start, [1.0, 0.0, 0.0, 1.0]);
        assert_eq!(spec.color_end, [0.0, 1.0, 0.0, 0.5]);
        assert_eq!(spec.alpha_start, 1.0);
        assert_eq!(spec.alpha_end, 0.25);
        assert_eq!(spec.max_count, 500);
        assert_eq!(spec.material.as_deref(), Some("textures/dot.png"));

        // Clamps: spawnRate 9999 -> 4096, life 0 -> 0.1, size 0.5 -> 1,
        // colors out of range -> 0..=1, maxCount 1e9 -> 4096.
        let particles = parse_particles_of(
            r#"{"objects": [{"name": "dust", "particle": {
                "spawnRate": 9999, "life": 0, "sizeStart": 0.5,
                "sizeEnd": 9999, "colorStart": [2, -1, 0.5],
                "maxCount": 1000000000
            }}]}"#,
        )
        .unwrap();
        let spec = &particles[0];
        assert_eq!(spec.spawn_rate, particles::MAX_PARTICLE_SPAWN_RATE);
        assert_eq!(spec.life, particles::MIN_PARTICLE_LIFE);
        assert_eq!(spec.size_start, particles::MIN_PARTICLE_SIZE);
        assert_eq!(spec.size_end, particles::MAX_PARTICLE_SIZE);
        assert_eq!(spec.color_start, [1.0, 0.0, 0.5, 1.0]);
        assert_eq!(spec.max_count, particles::MAX_PARTICLES as u32);

        // Direction 1e300: finite but huge — the bare f64 -> f32 cast
        // would overflow to f32::INFINITY and sin/cos(INFINITY) is NaN,
        // permanently poisoning the system. The parse must clamp to ±1e6
        // (the adversarial-review hole).
        let particles = parse_particles_of(
            r#"{"objects": [{"name": "dust", "particle": {"direction": 1e300}}]}"#,
        )
        .unwrap();
        assert_eq!(particles[0].direction, particles::MAX_PARTICLE_DIRECTION);
        assert!(particles[0].direction.is_finite());
        let particles = parse_particles_of(
            r#"{"objects": [{"name": "dust", "particle": {"direction": -1e300}}]}"#,
        )
        .unwrap();
        assert_eq!(particles[0].direction, -particles::MAX_PARTICLE_DIRECTION);
    }

    #[test]
    fn scene_order_records_the_objects_array_position_across_kinds() {
        // M3f draw order: every parsed kind records its position in the
        // scene.json `objects` array, and each kind list stays ascending —
        // the merge precondition for main.rs's merged_draws (a particle
        // system listed BEFORE an image draws UNDER it, whatever the mix).
        let value: Value = serde_json::from_str(
            r#"{"objects": [
                {"name": "imgA", "image": "a.png"},
                {"name": "dust", "particle": {}},
                {"name": "imgB", "image": "b.png"},
                {"name": "snow", "particle": {}},
                {"name": "label", "text": "hi"},
                {"name": "song.mp3", "audio": "song.mp3"}
            ]}"#,
        )
        .unwrap();
        let (layers, particles, _) = parse_objects(value.as_object().unwrap()).unwrap();
        assert_eq!(layers.len(), 3);
        assert_eq!(particles.len(), 2);
        assert_eq!(layers[0].scene_order, 0);
        assert_eq!(particles[0].scene_order, 1);
        assert_eq!(layers[1].scene_order, 2);
        assert_eq!(particles[1].scene_order, 3);
        assert_eq!(layers[2].scene_order, 4);
        let layer_orders: Vec<usize> = layers.iter().map(|l| l.scene_order).collect();
        assert_eq!(layer_orders, [0, 2, 4]);
        let particle_orders: Vec<usize> = particles.iter().map(|p| p.scene_order).collect();
        assert_eq!(particle_orders, [1, 3]);
    }

    #[test]
    fn particle_speed_precedence_and_normalization() {
        // speedMin/speedMax win over a bare `speed`; a reversed pair
        // normalizes.
        let particles = parse_particles_of(
            r#"{"objects": [
                {"name": "a", "particle": {"speed": 60}},
                {"name": "b", "particle": {"speed": 1, "speedMin": 10, "speedMax": 20}},
                {"name": "c", "particle": {"speed": 100, "speedMin": 90}},
                {"name": "d", "particle": {"speedMin": 80, "speedMax": 20}}
            ]}"#,
        )
        .unwrap();
        assert_eq!(particles[0].speed_min, 60.0);
        assert_eq!(particles[0].speed_max, 60.0);
        assert_eq!(particles[1].speed_min, 10.0);
        assert_eq!(particles[1].speed_max, 20.0);
        assert_eq!(particles[2].speed_min, 90.0);
        assert_eq!(
            particles[2].speed_max, 90.0,
            "missing max falls back to speed"
        );
        assert_eq!(particles[3].speed_min, 20.0, "reversed pair normalizes");
        assert_eq!(particles[3].speed_max, 80.0);
    }

    #[test]
    fn particle_scalar_forms_and_gravity_shapes() {
        // Numeric strings (the corpus editor serialization) and wrapped
        // values are accepted; gravity takes 1..=3 components (z dropped) —
        // scalar shapes reject like every WE vector.
        let particles = parse_particles_of(
            r#"{"objects": [{"name": "dust", "particle": {
                "spawnRate": {"user": "rate", "value": "250"},
                "life": "1.5"
            }}]}"#,
        )
        .unwrap();
        assert_eq!(particles[0].spawn_rate, 250.0);
        assert_eq!(particles[0].life, 1.5);
        assert_eq!(particles[0].gravity, [0.0, 0.0]);
        let scalar_error =
            parse_particles_of(r#"{"objects": [{"name": "dust", "particle": {"gravity": 100}}]}"#)
                .unwrap_err();
        assert_eq!(scalar_error.kind, SceneErrorKind::Shape);
        let particles = parse_particles_of(
            r#"{"objects": [
                {"name": "a", "particle": {"gravity": "0 100 0"}},
                {"name": "b", "particle": {"gravity": [5, -5]}}
            ]}"#,
        )
        .unwrap();
        assert_eq!(particles[0].gravity, [0.0, 100.0]);
        assert_eq!(particles[1].gravity, [5.0, -5.0]);
        // Out-of-range vector components reject like every WE vector.
        let error = parse_particles_of(
            r#"{"objects": [{"name": "dust", "particle": {"gravity": [1e12, 0]}}]}"#,
        )
        .unwrap_err();
        assert_eq!(error.kind, SceneErrorKind::Shape);
        // A non-numeric scalar rejects like brightness.
        let error = parse_particles_of(
            r#"{"objects": [{"name": "dust", "particle": {"spawnRate": "fast"}}]}"#,
        )
        .unwrap_err();
        assert_eq!(error.kind, SceneErrorKind::Shape);
    }

    #[test]
    fn particle_texture_precedence_and_non_string() {
        // `texture` (the brief's key) wins over `material` (the WE key);
        // a non-string value is None (registered, draws nothing).
        let particles = parse_particles_of(
            r#"{"objects": [
                {"name": "a", "particle": {"texture": "t.png", "material": "m.png"}},
                {"name": "b", "particle": {"material": "m.png"}},
                {"name": "c", "particle": {"material": 42}}
            ]}"#,
        )
        .unwrap();
        assert_eq!(particles[0].material.as_deref(), Some("t.png"));
        assert_eq!(particles[1].material.as_deref(), Some("m.png"));
        assert_eq!(particles[2].material, None);
    }

    #[test]
    fn particle_file_references_register_with_defaults() {
        // A string `particle` value is an external particle definition
        // file (researched WE feature): counted for the worker's one-time
        // diagnostic, the system registers with all defaults — the merge
        // is planned. Never a rejection.
        let json = r#"{"objects": [{"name": "dust", "particle": "particles/dust.json"}]}"#;
        let value: Value = serde_json::from_str(json).unwrap();
        let root = value.as_object().unwrap();
        let (_, particles, counts) = parse_objects(root).unwrap();
        assert_eq!(particles.len(), 1);
        assert_eq!(particles[0].name, "dust");
        assert_eq!(particles[0].spawn_rate, 10.0);
        assert_eq!(counts.particle_file_refs, 1);
        assert_eq!(counts.particle_system_skips, 0);
    }

    #[test]
    fn particle_system_cap_counts_skips() {
        // Systems past particles::MAX_PARTICLE_SYSTEMS are skipped and
        // counted (never a rejection); the pool is separate from the layer
        // cap — 16 particle systems plus 256 image layers still parses.
        let mut objects = String::from(r#"{"objects": ["#);
        for i in 0..particles::MAX_PARTICLE_SYSTEMS {
            objects.push_str(&format!(r#"{{"name": "p{i}", "particle": {{}}}},"#));
        }
        objects.push_str(r#"{"name": "extra", "particle": {}}"#);
        objects.push_str(r#",{"name": "l0", "image": "a.png"}"#);
        objects.push_str("]}");
        let value: Value = serde_json::from_str(&objects).unwrap();
        let root = value.as_object().unwrap();
        let (layers, particles, counts) = parse_objects(root).unwrap();
        assert_eq!(particles.len(), particles::MAX_PARTICLE_SYSTEMS);
        assert_eq!(counts.particle_system_skips, 1);
        assert_eq!(layers.len(), 1);
        // The layer cap is untouched by the particle pool: 256 layers plus
        // 16 systems together.
        let mut objects = String::from(r#"{"objects": ["#);
        for i in 0..MAX_LAYERS {
            objects.push_str(&format!(r#"{{"name": "l{i}", "image": "t.png"}},"#));
        }
        for i in 0..particles::MAX_PARTICLE_SYSTEMS {
            objects.push_str(&format!(r#"{{"name": "p{i}", "particle": {{}}}},"#));
        }
        objects.pop();
        objects.push_str("]}");
        let value: Value = serde_json::from_str(&objects).unwrap();
        let root = value.as_object().unwrap();
        let (layers, particles, _) = parse_objects(root).unwrap();
        assert_eq!(layers.len(), MAX_LAYERS);
        assert_eq!(particles.len(), particles::MAX_PARTICLE_SYSTEMS);
    }

    #[test]
    fn particle_classification_order_image_then_particle_then_text() {
        // The researched WE order: image, sound, particle, text. An object
        // with image + particle counts as an image layer; an object with
        // particle + text counts as a particle system; text-only as text.
        let json = r#"{"objects": [
            {"name": "a", "image": "a.png", "particle": {}},
            {"name": "b", "particle": {}, "text": "hi"},
            {"name": "c", "text": "hi"},
            {"name": "d", "audio": "s.mp3"}
        ]}"#;
        let value: Value = serde_json::from_str(json).unwrap();
        let root = value.as_object().unwrap();
        let (layers, particles, counts) = parse_objects(root).unwrap();
        assert_eq!(layers.len(), 2, "a (image wins) and c (text)");
        assert_eq!(layers[0].name, "a");
        assert!(layers[0].text.is_none());
        assert_eq!(layers[1].name, "c");
        assert_eq!(particles.len(), 1);
        assert_eq!(particles[0].name, "b");
        assert_eq!(counts.text_on_image_objects, 0);
    }

    #[test]
    fn particle_malformed_common_props_reject_like_layers() {
        // Shared props follow the layer strictness (name required, alpha
        // 0..=1, visible bool, ...); the emitter fields clamp instead.
        let error = parse_particles_of(r#"{"objects": [{"particle": {}}]}"#).unwrap_err();
        assert_eq!(error.kind, SceneErrorKind::Shape);
        assert!(error.message.contains("name"), "{}", error.message);
        let error =
            parse_particles_of(r#"{"objects": [{"name": "p", "particle": {}, "alpha": 2.0}]}"#)
                .unwrap_err();
        assert_eq!(error.kind, SceneErrorKind::Shape);
    }

    #[test]
    fn pkg_scene_carries_particle_systems() {
        // The pkg lane parses the same `objects` array; material references
        // resolve against the package table at load, not here.
        let dir = tmpdir();
        let scene_json = br#"{"objects": [{"name": "dust", "particle": {
            "spawnRate": 50, "material": "textures/dot.png"}}]}"#;
        let entries = pkg_entries(
            &dir,
            &build_pkg(&[
                ("scene.json", scene_json),
                ("textures/dot.png", b"TEXV0005"),
            ]),
        );
        let config = SceneConfig::parse_pkg(scene_json, &entries).unwrap();
        assert_eq!(config.particles.len(), 1);
        assert_eq!(config.particles[0].spawn_rate, 50.0);
        assert_eq!(
            config.particles[0].material.as_deref(),
            Some("textures/dot.png")
        );
        assert!(config.particles[0].texture.is_none());
    }

    // ---- M3g: video layers ----

    /// Parse one `objects` array and return only its layers.
    fn parse_layers(json: &str) -> Vec<LayerSpec> {
        let value: Value = serde_json::from_str(json).unwrap();
        let root = value.as_object().unwrap();
        parse_objects(root).unwrap().0
    }

    #[test]
    fn video_layer_classifies_after_image_and_before_particle() {
        // The classification order is a documented design decision (the
        // corpus has no video objects to corroborate one): an object
        // carrying both `image` and `video` is an image layer, and one
        // carrying both `video` and `particle`/`text` is a video layer —
        // the kind that owns the texture slot wins.
        let layers = parse_layers(
            r#"{"objects": [
                {"name": "img", "image": "a.png", "video": "v.mp4"},
                {"name": "vid", "video": "v.mp4", "particle": {}, "text": "hi"}
            ]}"#,
        );
        assert_eq!(layers.len(), 2);
        assert_eq!(layers[0].image.as_deref(), Some("a.png"));
        assert!(layers[0].video.is_none(), "`image` wins over `video`");
        assert!(layers[1].image.is_none());
        assert!(layers[1].text.is_none(), "`video` wins over `text`");
        assert_eq!(
            layers[1].video.as_ref().unwrap().source.as_deref(),
            Some("v.mp4")
        );
    }

    #[test]
    fn video_layer_defaults_and_property_wrapping() {
        // Defaults: loop on, rate 1.0, size [0, 0] ("the video's own
        // dimensions"), path unresolved. The source accepts the editor's
        // property-wrapped form exactly like an image reference does, and
        // a non-string `video` registers the layer with no source — never
        // a rejection.
        let layers = parse_layers(
            r#"{"objects": [
                {"name": "plain", "video": "clip.mp4"},
                {"name": "wrapped", "video": {"value": "wrapped.mp4"}},
                {"name": "nonstring", "video": 7}
            ]}"#,
        );
        let plain = layers[0].video.as_ref().unwrap();
        assert_eq!(plain.source.as_deref(), Some("clip.mp4"));
        assert!(plain.loop_playback, "WE video wallpapers loop by default");
        assert_eq!(plain.rate, 1.0);
        assert!(plain.path.is_none(), "the parse never resolves a path");
        assert_eq!(layers[0].size, [0.0, 0.0]);
        assert_eq!(layers[0].alpha, 1.0);
        assert!(layers[0].visible);
        assert_eq!(
            layers[1].video.as_ref().unwrap().source.as_deref(),
            Some("wrapped.mp4")
        );
        assert!(
            layers[2].video.as_ref().unwrap().source.is_none(),
            "a non-string video registers the layer without a source"
        );
    }

    #[test]
    fn video_loop_accepts_the_editor_spellings() {
        // `loop` clamps to a default rather than rejecting (the M3f
        // convention): booleans are exact, the editor's string and number
        // spellings of false are honored, and anything else stays true.
        let layers = parse_layers(
            r#"{"objects": [
                {"name": "a", "video": "v.mp4", "loop": false},
                {"name": "b", "video": "v.mp4", "loop": true},
                {"name": "c", "video": "v.mp4", "loop": "false"},
                {"name": "d", "video": "v.mp4", "loop": " NO "},
                {"name": "e", "video": "v.mp4", "loop": "0"},
                {"name": "f", "video": "v.mp4", "loop": 0},
                {"name": "g", "video": "v.mp4", "loop": 1},
                {"name": "h", "video": "v.mp4", "loop": null},
                {"name": "i", "video": "v.mp4", "loop": "yes"},
                {"name": "j", "video": "v.mp4", "loop": {"value": false}}
            ]}"#,
        );
        let looping: Vec<bool> = layers
            .iter()
            .map(|layer| layer.video.as_ref().unwrap().loop_playback)
            .collect();
        assert_eq!(
            looping,
            vec![
                false, true, false, false, false, false, true, true, true, false
            ]
        );
    }

    #[test]
    fn video_rate_clamps_never_rejects() {
        // A hostile or sloppy `rate` costs the user a speed, never the
        // scene: every value lands inside the documented range.
        let layers = parse_layers(
            r#"{"objects": [
                {"name": "a", "video": "v.mp4", "rate": 2},
                {"name": "b", "video": "v.mp4", "rate": 1000},
                {"name": "c", "video": "v.mp4", "rate": -5},
                {"name": "d", "video": "v.mp4", "rate": 0},
                {"name": "e", "video": "v.mp4", "rate": "1.5"},
                {"name": "f", "video": "v.mp4", "rate": "garbage"},
                {"name": "g", "video": "v.mp4", "rate": {"value": 0.25}}
            ]}"#,
        );
        let rates: Vec<f32> = layers
            .iter()
            .map(|layer| layer.video.as_ref().unwrap().rate)
            .collect();
        assert_eq!(
            rates,
            vec![
                2.0,
                crate::video::MAX_PLAYBACK_RATE,
                crate::video::MIN_PLAYBACK_RATE,
                crate::video::MIN_PLAYBACK_RATE,
                1.5,
                1.0,
                crate::video::MIN_PLAYBACK_RATE.max(0.25),
            ]
        );
    }

    #[test]
    fn video_concurrency_cap_clears_sources_without_rejecting() {
        // Past video::MAX_VIDEO_LAYERS the layer still registers — a
        // script can move it and read its props through Scene.getLayer —
        // but its source is cleared at parse so no decoder ever opens.
        // Counted for the worker's one-time diagnostic, exactly like the
        // particle-system cap.
        let mut objects = String::from(r#"{"objects": ["#);
        for i in 0..crate::video::MAX_VIDEO_LAYERS + 3 {
            objects.push_str(&format!(r#"{{"name": "v{i}", "video": "v.mp4"}},"#));
        }
        objects.push_str(r#"{"name": "img", "image": "a.png"}"#);
        objects.push_str("]}");
        let value: Value = serde_json::from_str(&objects).unwrap();
        let root = value.as_object().unwrap();
        let (layers, _, counts) = parse_objects(root).unwrap();
        assert_eq!(
            layers.len(),
            crate::video::MAX_VIDEO_LAYERS + 4,
            "over-cap layers register, never skipped from the scene"
        );
        assert_eq!(counts.video_layer_skips, 3);
        let with_source = layers
            .iter()
            .filter(|layer| {
                layer
                    .video
                    .as_ref()
                    .is_some_and(|spec| spec.source.is_some())
            })
            .count();
        assert_eq!(with_source, crate::video::MAX_VIDEO_LAYERS);
        // The image layer is untouched by the video cap.
        assert_eq!(layers.last().unwrap().image.as_deref(), Some("a.png"));
    }

    #[test]
    fn video_layer_carries_the_full_config_through_parse() {
        // End to end through the shared JSON core so the skip counter
        // and the shared props reach the worker; `color` still feeds
        // `tint` and the draw order is the objects index.
        let config = parse_scene_json(
            br#"{"objects": [
                {"name": "back", "image": "a.png"},
                {"name": "clip", "video": "movies/clip.mp4", "loop": false,
                 "rate": 0.5, "origin": [1, 2, 3], "size": [640, 480],
                 "color": [1, 0, 0], "alpha": 0.5, "visible": false}
            ]}"#,
        )
        .unwrap();
        assert_eq!(config.video_layer_skips, 0);
        let layer = &config.layers[1];
        assert_eq!(layer.scene_order, 1);
        assert_eq!(layer.origin, [1.0, 2.0]);
        assert_eq!(layer.size, [640.0, 480.0], "an explicit size is kept");
        assert_eq!(layer.tint, [1.0, 0.0, 0.0, 1.0]);
        assert_eq!(layer.alpha, 0.5);
        assert!(!layer.visible);
        let spec = layer.video.as_ref().unwrap();
        assert_eq!(spec.source.as_deref(), Some("movies/clip.mp4"));
        assert!(!spec.loop_playback);
        assert_eq!(spec.rate, 0.5);
    }
}
