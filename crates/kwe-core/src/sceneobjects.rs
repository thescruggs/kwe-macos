// SPDX-License-Identifier: GPL-3.0-or-later
//! Shared classification of a scene.json `objects` array (B2).
//!
//! Both the scene renderer (which builds layers and particle systems from
//! the array) and preflight (which must answer "can this scene draw
//! anything at all?" before an apply transaction runs) need the same
//! answer for the same object. Keeping the rule in one place is what makes
//! the preflight refusal and the renderer's own accounting agree; when the
//! two disagreed, a scene made entirely of features this build lacks was
//! promoted as a healthy wallpaper and the desktop went flat
//! (docs/bugs/SCENE_APPLY_BLANK_CLEAR_COLOR.md).
//!
//! The classification is the researched Wallpaper Engine object model:
//! every visual is stored as a model instance, and the `image` field
//! carries either a texture reference or a `.json` model reference. An
//! object that carries `image` with a NON-string value (the editor's
//! `null`) is not an image at all — the corpus's 65 such objects are all
//! particle systems — so the non-string case falls through to the
//! video/particle/text keys instead of registering a textureless image
//! layer. That fall-through is the classification half of B2.

use serde_json::{Map, Value};

/// What one `objects[i]` entry is, under the rules above.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SceneObjectKind {
    /// `image` references a `.json` model instance (the WE solid-model
    /// architecture). S1: resolved model -> material -> texture
    /// (`kwe_core::scenemodel`) and, when that resolves, drawn as a
    /// textured quad — a malformed model object still registers nothing
    /// (skip-never-reject, `crates/kwe-scene-renderer/src/scene.rs`'s
    /// `parse_model_layer`), but a well-formed one now IS a real layer.
    Model,
    /// `image` references a texture this build can decode (png/jpeg/webp).
    Image,
    /// `image` references a TEXV0005 texture (`.tex`) — the Wallpaper
    /// Engine texture container, planned with BETA_M3h. The layer
    /// registers, the decode fails, and it draws nothing.
    TexvImage,
    /// A `video` object (BETA_M3g).
    Video,
    /// A particle system whose definition is inline and names a material
    /// or texture: it can draw today.
    Particle,
    /// A particle system whose `particle` value is a string — an external
    /// particle definition file. The system registers with defaults, but
    /// the definition (and therefore its material) is never read, so it
    /// draws nothing until the file-level merge lands.
    ParticleFile,
    /// A `text` object (BETA_M3e).
    Text,
    /// `image` present but not a string, and no video/particle/text key:
    /// registers as a textureless image layer so a script can still reach
    /// it by name. Draws nothing.
    TexturelessImage,
    /// Audio and anything else the build does not interpret.
    Other,
}

impl SceneObjectKind {
    /// Can an object of this kind put pixels on the screen in this build?
    /// This is the honest question preflight asks — not "did the parser
    /// register something".
    pub fn can_draw(self) -> bool {
        matches!(
            self,
            SceneObjectKind::Image
                | SceneObjectKind::Video
                | SceneObjectKind::Particle
                | SceneObjectKind::Text
        )
    }
}

/// The property wrapper (`{"user": ..., "value": ...}`) the editor writes
/// around user-bindable fields. Unwrapped before any field is read; the
/// renderer's own `scene_property_value` is the same rule.
pub fn scene_property_value(value: &Value) -> &Value {
    match value.as_object().and_then(|object| object.get("value")) {
        Some(inner) => inner,
        None => value,
    }
}

/// Classify one `objects[i]` entry. Field order is the researched WE
/// classification order (image, video, particle, text), with the
/// non-string `image` fall-through described in the module docs.
pub fn classify_scene_object(object: &Map<String, Value>) -> SceneObjectKind {
    let image = object.get("image").map(scene_property_value);
    if let Some(reference) = image.and_then(Value::as_str) {
        let lowercase = reference.to_ascii_lowercase();
        return if lowercase.ends_with(".json") {
            SceneObjectKind::Model
        } else if lowercase.ends_with(".tex") {
            SceneObjectKind::TexvImage
        } else {
            SceneObjectKind::Image
        };
    }
    if object.contains_key("video") {
        return SceneObjectKind::Video;
    }
    if let Some(definition) = object.get("particle").map(scene_property_value) {
        return match definition.as_object() {
            Some(fields)
                if fields
                    .get("texture")
                    .or_else(|| fields.get("material"))
                    .map(scene_property_value)
                    .and_then(Value::as_str)
                    .is_some() =>
            {
                SceneObjectKind::Particle
            }
            Some(_) => SceneObjectKind::ParticleFile,
            None => SceneObjectKind::ParticleFile,
        };
    }
    if object.contains_key("text") {
        return SceneObjectKind::Text;
    }
    if image.is_some() {
        return SceneObjectKind::TexturelessImage;
    }
    SceneObjectKind::Other
}

/// Per-kind census of one scene's `objects` array.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct SceneObjectSummary {
    pub objects: usize,
    pub models: usize,
    /// Of `models`, how many resolved a material texture through the
    /// caller's lookup (S1, `summarize_scene_objects_resolved`; stays 0
    /// under the plain `summarize_scene_objects` — no lookup, no I/O,
    /// pure classification only). Never exceeds `models`.
    pub models_resolved: usize,
    pub images: usize,
    pub texv_images: usize,
    pub videos: usize,
    pub particles: usize,
    pub particle_files: usize,
    pub texts: usize,
    pub textureless_images: usize,
    pub other: usize,
}

impl SceneObjectSummary {
    /// Objects that can produce pixels in this build. A model counts only
    /// when it actually resolved a texture (S1 honesty: unlike a direct
    /// image reference, which counts statically even before its bytes are
    /// known to decode, a model layer has no pipeline at all without a
    /// resolvable texture — see the module doc's B2 reference and
    /// deliverable 4 of the S1 task).
    pub fn drawable(&self) -> usize {
        self.images + self.videos + self.particles + self.texts + self.models_resolved
    }

    /// Why a scene with no drawable object draws nothing, one clause per
    /// unsupported feature it actually uses. Empty when the scene has
    /// drawable content, or when the only objects it declares are ones the
    /// build ignores outright (audio, unknown kinds): an empty or
    /// audio-only scene is the author's choice — and its script may still
    /// animate the clear colour — not a missing feature.
    ///
    /// Every kind that REGISTERS something and still cannot draw must be
    /// named here, or the two gates disagree: the worker refuses any scene
    /// that registers objects and draws none of them, so a scene preflight
    /// passed on silence would bounce a worker and roll back instead of
    /// being refused cleanly.
    pub fn unsupported_reasons(&self) -> Vec<String> {
        let mut reasons = Vec::new();
        let unresolved_models = self.models.saturating_sub(self.models_resolved);
        if unresolved_models > 0 {
            // S1: models are no longer a blanket "scene3d, not rendered"
            // gap — a model whose material texture resolves (pkg/scene
            // dir/Wallpaper Engine assets root) draws as a textured quad.
            // What remains unsupported is specifically resolution failure
            // (most commonly: the assets root was never configured, so no
            // model's texture can resolve at all).
            reasons.push(format!(
                "{unresolved_models} model layer(s) whose material textures could not be resolved (missing Wallpaper Engine assets?)"
            ));
        }
        if self.texv_images > 0 {
            reasons.push(format!(
                "{} layer(s) use TEXV (.tex) textures, which this build cannot decode yet",
                self.texv_images
            ));
        }
        if self.textureless_images > 0 {
            reasons.push(format!(
                "{} layer(s) have no image reference and draw nothing",
                self.textureless_images
            ));
        }
        if self.particle_files > 0 {
            reasons.push(format!(
                "{} particle system(s) reference external particle files, which this build does not read yet",
                self.particle_files
            ));
        }
        reasons
    }
}

/// Census the `objects` array of a parsed scene.json root. A root without
/// a well-formed `objects` array summarizes to all zeroes: this function
/// reports, it never validates — the renderer's parse owns the errors.
pub fn summarize_scene_objects(root: &Value) -> SceneObjectSummary {
    let mut summary = SceneObjectSummary::default();
    let Some(objects) = root.get("objects").and_then(Value::as_array) else {
        return summary;
    };
    for entry in objects {
        let Some(object) = entry.as_object() else {
            continue;
        };
        summary.objects += 1;
        match classify_scene_object(object) {
            SceneObjectKind::Model => summary.models += 1,
            SceneObjectKind::Image => summary.images += 1,
            SceneObjectKind::TexvImage => summary.texv_images += 1,
            SceneObjectKind::Video => summary.videos += 1,
            SceneObjectKind::Particle => summary.particles += 1,
            SceneObjectKind::ParticleFile => summary.particle_files += 1,
            SceneObjectKind::Text => summary.texts += 1,
            SceneObjectKind::TexturelessImage => summary.textureless_images += 1,
            SceneObjectKind::Other => summary.other += 1,
        }
    }
    summary
}

/// Census the `objects` array, additionally resolving each model layer's
/// material texture through `resolve` (`crate::scenemodel::resolve_model`:
/// model.json -> material path -> material.json -> passes[0] -> first
/// texture slot -> `materials/<name>.tex`), so `models_resolved` and
/// therefore `drawable()`/`unsupported_reasons()` reflect reality instead
/// of the static "models never draw" assumption. `resolve` is the
/// caller's composed lookup (pkg entries -> scene directory -> Wallpaper
/// Engine assets root); this function performs no I/O of its own beyond
/// calling it. Shared by preflight (`preflight_scene`/`preflight_pkg`) and
/// the worker's own B2 gate (main.rs adds the resolved count the same
/// way after `load_model_textures` runs).
/// Cap on how many model objects `summarize_scene_objects_resolved` will
/// actually attempt to resolve in one preflight call (S1 review #3): each
/// attempt costs up to three `confined_read`/pkg-entry lookups, each of
/// which is a handful of blocking syscalls, run synchronously inside the
/// trusted `kwe-daemon` process as part of validating one request — with
/// no cap, a crafted scene.json under the existing whole-file size caps
/// (~16 MiB, room for ~10^5-10^6 minimal `{"image":"models/m.json"}`
/// objects) turns one apply/preflight request into hundreds of thousands
/// of inline syscalls, a control-plane DoS distinct from anything the
/// sandboxed renderer bounds. Mirrors `kwe-scene-renderer`'s
/// `layers::MAX_LAYERS` (256) — a scene with more model objects than that
/// gets rejected by the worker's own layer cap anyway once every
/// well-formed one of them registers as a layer (S1), so bounding
/// preflight's attempt count at the same number costs no honesty against
/// what the worker would actually do.
pub const MAX_MODEL_RESOLUTIONS: usize = 256;

pub fn summarize_scene_objects_resolved(
    root: &Value,
    resolve: &mut crate::scenemodel::AssetLookup<'_>,
) -> SceneObjectSummary {
    let mut summary = summarize_scene_objects(root);
    if summary.models == 0 {
        return summary;
    }
    let Some(objects) = root.get("objects").and_then(Value::as_array) else {
        return summary;
    };
    let mut attempted = 0usize;
    for entry in objects {
        if attempted >= MAX_MODEL_RESOLUTIONS {
            break;
        }
        let Some(object) = entry.as_object() else {
            continue;
        };
        if classify_scene_object(object) != SceneObjectKind::Model {
            continue;
        }
        let Some(reference) = object
            .get("image")
            .map(scene_property_value)
            .and_then(Value::as_str)
        else {
            continue; // malformed model reference: stays unresolved
        };
        attempted += 1;
        if crate::scenemodel::resolve_model(reference, resolve).is_ok() {
            summary.models_resolved += 1;
        }
    }
    summary
}

#[cfg(test)]
mod tests {
    use super::*;

    fn object(json: &str) -> Map<String, Value> {
        serde_json::from_str(json).expect("test object")
    }

    /// S1 review #3: a crafted scene declaring far more model objects than
    /// `MAX_MODEL_RESOLUTIONS` must not turn preflight into an unbounded
    /// number of lookup-closure calls (each a handful of syscalls in the
    /// real pkg/dir/assets lookup) — the cap must stop ATTEMPTING
    /// resolution, not merely stop counting successes, so every model here
    /// is deliberately resolvable if attempted.
    #[test]
    fn model_resolution_attempts_are_capped() {
        let total = MAX_MODEL_RESOLUTIONS + 50;
        let mut objects = String::from(r#"{"objects": ["#);
        for i in 0..total {
            objects.push_str(&format!(
                r#"{{"name": "m{i}", "image": "models/m{i}.json"}},"#
            ));
        }
        objects.push_str(r#"{"name": "tail", "image": null}]}"#);
        let root: Value = serde_json::from_str(&objects).unwrap();

        let calls = std::cell::Cell::new(0usize);
        let mut resolve = |reference: &str| {
            calls.set(calls.get() + 1);
            if let Some(name) = reference
                .strip_prefix("models/")
                .and_then(|s| s.strip_suffix(".json"))
            {
                return Some(format!(r#"{{"material": "materials/{name}.json"}}"#).into_bytes());
            }
            if let Some(name) = reference
                .strip_prefix("materials/")
                .and_then(|s| s.strip_suffix(".json"))
            {
                return Some(format!(r#"{{"passes": [{{"textures": ["{name}"]}}]}}"#).into_bytes());
            }
            if reference.starts_with("materials/") && reference.ends_with(".tex") {
                return Some(b"bytes".to_vec());
            }
            None
        };

        let summary = summarize_scene_objects_resolved(&root, &mut resolve);
        assert_eq!(summary.models, total);
        assert_eq!(
            summary.models_resolved, MAX_MODEL_RESOLUTIONS,
            "every model is resolvable, so a correct cap stops attempts at exactly the cap"
        );
        assert!(
            calls.get() <= MAX_MODEL_RESOLUTIONS * 3,
            "resolution attempts must stay bounded: {} lookup calls for {total} declared models",
            calls.get()
        );
    }

    #[test]
    fn image_references_classify_by_extension() {
        assert_eq!(
            classify_scene_object(&object(r#"{"image": "models/a.JSON"}"#)),
            SceneObjectKind::Model
        );
        assert_eq!(
            classify_scene_object(&object(r#"{"image": "materials/a.tex"}"#)),
            SceneObjectKind::TexvImage
        );
        assert_eq!(
            classify_scene_object(&object(r#"{"image": "textures/a.png"}"#)),
            SceneObjectKind::Image
        );
    }

    /// The B2 classification fix: the editor writes `"image": null` on
    /// particle objects, and every one of the corpus's 65 such objects is
    /// a particle system. Before the fix they took the image branch and
    /// registered as textureless image layers, so the particle systems
    /// vanished.
    #[test]
    fn null_image_with_particle_is_a_particle_system() {
        assert_eq!(
            classify_scene_object(&object(
                r#"{"image": null, "particle": "particles/presets/fireflies.json"}"#
            )),
            SceneObjectKind::ParticleFile
        );
        assert_eq!(
            classify_scene_object(&object(
                r#"{"image": null, "particle": {"material": "materials/dot.png"}}"#
            )),
            SceneObjectKind::Particle
        );
    }

    /// A property-wrapped reference is unwrapped before classification,
    /// on both the image and the particle side.
    #[test]
    fn property_wrapped_values_unwrap() {
        assert_eq!(
            classify_scene_object(&object(
                r#"{"image": {"user": "u", "value": "models/a.json"}}"#
            )),
            SceneObjectKind::Model
        );
        assert_eq!(
            classify_scene_object(&object(
                r#"{"particle": {"user": "u", "value": {"texture": "t.png"}}}"#
            )),
            SceneObjectKind::Particle
        );
    }

    /// An object with a non-string image and no other visual key still
    /// registers as a layer (scripts reach it by name), and still draws
    /// nothing.
    #[test]
    fn textureless_image_registers_but_cannot_draw() {
        let kind = classify_scene_object(&object(r#"{"image": null, "name": "a"}"#));
        assert_eq!(kind, SceneObjectKind::TexturelessImage);
        assert!(!kind.can_draw());
    }

    #[test]
    fn text_and_video_are_drawable() {
        assert!(classify_scene_object(&object(r#"{"text": "hi"}"#)).can_draw());
        assert!(classify_scene_object(&object(r#"{"video": "a.mp4"}"#)).can_draw());
    }

    /// An object carrying both image and text is an image layer (the M3c
    /// rule), so a model-backed one is still a model skip.
    #[test]
    fn image_wins_over_text() {
        assert_eq!(
            classify_scene_object(&object(r#"{"image": "models/a.json", "text": "hi"}"#)),
            SceneObjectKind::Model
        );
    }

    #[test]
    fn summary_counts_every_kind_and_reports_reasons() {
        let root: Value = serde_json::from_str(
            r#"{"objects": [
                {"image": "models/a.json"},
                {"image": "materials/b.tex"},
                {"image": null, "particle": "particles/c.json"},
                {"text": "hi"},
                {"sound": "s.mp3"},
                7
            ]}"#,
        )
        .expect("test scene");
        let summary = summarize_scene_objects(&root);
        assert_eq!(summary.objects, 5); // the bare number is not an object
        assert_eq!(summary.models, 1);
        assert_eq!(summary.texv_images, 1);
        assert_eq!(summary.particle_files, 1);
        assert_eq!(summary.texts, 1);
        assert_eq!(summary.other, 1);
        assert_eq!(summary.drawable(), 1);
        assert!(summary.unsupported_reasons().len() == 3);
    }

    /// A scene with no objects at all is empty by authorship, not by
    /// missing features: no reasons to report.
    /// The two gates must agree: the worker refuses a scene that registers
    /// layers and draws none of them, so preflight must refuse the
    /// textureless-only scene too rather than passing it into a rollback.
    #[test]
    fn textureless_only_scene_is_named_as_a_reason() {
        let root: Value =
            serde_json::from_str(r#"{"objects": [{"name": "ghost", "image": null}]}"#)
                .expect("test scene");
        let summary = summarize_scene_objects(&root);
        assert_eq!(summary.drawable(), 0);
        assert_eq!(summary.textureless_images, 1);
        assert!(
            summary.unsupported_reasons()[0].contains("no image reference"),
            "{:?}",
            summary.unsupported_reasons()
        );
    }

    /// An audio-only scene registers nothing, so neither gate fires: the
    /// scene is empty by authorship and its script may animate the clear
    /// colour.
    #[test]
    fn audio_only_scene_reports_no_reasons() {
        let root: Value =
            serde_json::from_str(r#"{"objects": [{"sound": "s.mp3"}]}"#).expect("test scene");
        let summary = summarize_scene_objects(&root);
        assert_eq!(summary.other, 1);
        assert!(summary.unsupported_reasons().is_empty());
    }

    #[test]
    fn empty_scene_reports_no_reasons() {
        let root: Value = serde_json::from_str(r#"{"objects": []}"#).expect("test scene");
        let summary = summarize_scene_objects(&root);
        assert_eq!(summary.drawable(), 0);
        assert!(summary.unsupported_reasons().is_empty());
    }

    #[test]
    fn missing_objects_array_summarizes_to_zero() {
        let root: Value = serde_json::from_str(r#"{"general": {}}"#).expect("test scene");
        assert_eq!(
            summarize_scene_objects(&root),
            SceneObjectSummary::default()
        );
    }
}
