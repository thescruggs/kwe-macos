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
    /// `image` references a `.json` model instance: scene3d, BETA_M3h.
    /// Skipped by the renderer before any validation.
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
    /// Objects that can produce pixels in this build.
    pub fn drawable(&self) -> usize {
        self.images + self.videos + self.particles + self.texts
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
        if self.models > 0 {
            reasons.push(format!(
                "{} model layer(s) need scene3d, which this build does not render yet",
                self.models
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

#[cfg(test)]
mod tests {
    use super::*;

    fn object(json: &str) -> Map<String, Value> {
        serde_json::from_str(json).expect("test object")
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
