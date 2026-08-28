// SPDX-License-Identifier: GPL-3.0-or-later
//! SR-0c: bounded raw walk of a parsed `scene.json` root, filling
//! `required`/`detected`/`unknown` for the `scene-feature-inventory-v0`
//! record (docs/SCENE_CAPABILITIES.md). Object-family only — materials
//! require pkg/asset resolution of referenced files and are their own
//! follow-up slice (conductor scope decision, docs/SR0.md SR-0c).
//!
//! This module never resolves a reference (image path, particle file,
//! material) against the pkg/scene-dir/assets-root lookup chain the
//! renderer and preflight use; it only reads the shape of `scene.json`
//! itself, entirely inside this isolated process.
//!
//! ## Deviation from the task's literal entry-point signature
//!
//! The task text names the entry point as `fn inventory_scene_json(bytes:
//! &[u8], caps: &InventoryCaps) -> Inventory`, but also requires (a) the
//! caller to distinguish a JSON parse failure from a successfully parsed
//! but empty/malformed scene (both would otherwise produce an
//! indistinguishable all-zero `Inventory`), and (b) a wall-clock deadline
//! checked mid-walk. Neither is representable through that literal
//! signature, so this module returns `Result<Inventory, InventoryError>`
//! and takes an added `deadline: Instant` parameter; a deadline expiry is
//! folded into `Inventory::limits_hit` (`"timeout"`) exactly like the
//! `"objects-cap"` case, rather than a third error variant, since the walk
//! still produced a valid (truncated) inventory up to that point.

use std::time::Instant;

use serde_json::{Map, Value};

/// Bounded-walk limits for one `inventory_scene_json` call.
#[derive(Debug, Clone, Copy)]
pub struct InventoryCaps {
    /// Stop walking `objects[]` after this many entries (whether or not
    /// they classified). Exceeding it stops the walk, marks every
    /// truncatable list `truncated`, and adds `"objects-cap"` to
    /// `Inventory::limits_hit`. 4096 comfortably covers the local corpus
    /// (largest observed scene.json well under four figures of objects)
    /// while bounding a hostile file's walk cost.
    pub max_objects_walked: usize,
    /// Documentary only — nothing in this module recurses to this depth
    /// (or checks it): `serde_json::from_slice`'s own built-in recursion
    /// guard (measured on this workspace's serde_json 1.0.151: a
    /// 10_000-deep nested JSON array fails to parse at all, with a plain
    /// "recursion limit exceeded" error, no stack growth) already protects
    /// the parse step, and the walk itself only ever descends two levels —
    /// root, then `objects[i]` — never into a value nested inside an
    /// object's own fields (`general`'s contents, `effects[]` entries,
    /// etc. are never inspected here). Kept as a field so the bound is
    /// visible next to the others it is documented alongside, matching the
    /// task's contract.
    #[allow(dead_code)]
    pub max_depth: usize,
    /// First N sorted logical object ids kept per detected capability, and
    /// first N sorted sample paths kept for `unknown.samples`. Bounded
    /// independent of `max_objects_walked` via a running top-K-smallest
    /// insertion (`push_sample`), so memory never grows past this count
    /// even when far more than 16 objects/keys would otherwise qualify.
    pub max_samples: usize,
    /// Byte cap (char-boundary-safe truncation) on one sample path or
    /// logical object id before it is stored.
    pub max_sample_path_bytes: usize,
}

impl Default for InventoryCaps {
    fn default() -> Self {
        Self {
            max_objects_walked: 4096,
            max_depth: 32,
            max_samples: 16,
            max_sample_path_bytes: 128,
        }
    }
}

/// One detected capability: how many objects carried it, and the first N
/// (sorted, deduplicated by insertion order — one id per matching object)
/// logical object ids.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DetectedCapability {
    pub capability: &'static str,
    pub count: usize,
    pub objects: Vec<String>,
    pub truncated: bool,
}

/// Unknown-key/type/object counters plus a bounded sample of key paths.
/// Counters are exact totals; `samples`/`truncated` describe only the
/// sample list (docs/SCENE_CAPABILITIES.md: "unknown keys/types are
/// counted, never dropped").
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct UnknownCounts {
    pub keys: usize,
    pub types: usize,
    pub objects: usize,
    pub samples: Vec<String>,
    pub truncated: bool,
}

/// The object-family inventory of one parsed `scene.json`.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Inventory {
    /// Sorted, deduplicated capability ids required by at least one ACTIVE
    /// object (docs/SCENE_CAPABILITIES.md: "`required` derives only from
    /// *active* objects/passes/APIs").
    pub required: Vec<String>,
    /// Sorted by capability id.
    pub detected: Vec<DetectedCapability>,
    pub unknown: UnknownCounts,
    pub limits_hit: Vec<&'static str>,
}

/// The only failure this module reports itself: `bytes` is not valid JSON
/// at all. Every other malformed shape (root not an object, `objects` not
/// an array, a non-object entry, an unrecognized key) is counted in
/// `Inventory::unknown` instead of failing — see the module docs.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum InventoryError {
    Parse,
}

/// Known root-level `scene.json` keys. Derived from every root-level key
/// any parser in this workspace actually reads (grep `root_obj.get("` /
/// `value.get("general")` across crates/kwe-scene-renderer/src/scene.rs and
/// crates/kwe-core/src/pkg.rs): only `general` and `objects`. Neither
/// `scene.rs` nor `sceneobjects.rs` uses `#[derive(Deserialize)]` structs
/// for scene content at all — every field in this workspace is read via
/// raw `serde_json::Value::get("...")` navigation, not typed struct
/// fields, so the task's literal "enumerate the serde field names" method
/// does not apply mechanically here; this table instead enumerates the
/// actual string-literal keys that code reads (the closest faithful
/// analog: "every key the existing code already understands"). SR-2's
/// typed IR replaces this table with the parser itself as authority.
const ROOT_KEYS: &[&str] = &["general", "objects"];

/// Known `objects[i]` keys, derived the same way (grep `object.get("`/
/// `object.contains_key("` across `crates/kwe-core/src/sceneobjects.rs` and
/// `crates/kwe-scene-renderer/src/scene.rs`): `name`, `id`,
/// `origin`/`angles`/`scale`/`alpha`/`visible` (`parse_common_props`),
/// `blendMode`/`colorBlendMode` (aliases), `brightness`, `size`,
/// `tint`/`color` (aliases), `image`, `effects`, `text`, `font`,
/// `pointsize`, `video`, `loop`, `rate`, `instanceoverride`, `particle`.
/// `model`, `sound`, and `light` are not read by any parser in this
/// workspace yet (no model/sound/light object support has landed), but
/// this SR-0c task names them directly as classification discriminators
/// (§1) — they are included here as known so an object using one is never
/// double-counted as *both* a detected capability *and* an unknown key.
/// SR-2's typed IR replaces this table with the parser itself as
/// authority.
const OBJECT_KEYS: &[&str] = &[
    "name",
    "id",
    "origin",
    "angles",
    "scale",
    "alpha",
    "visible",
    "blendMode",
    "colorBlendMode",
    "brightness",
    "size",
    "tint",
    "color",
    "image",
    "model",
    "effects",
    "text",
    "font",
    "pointsize",
    "video",
    "loop",
    "rate",
    "instanceoverride",
    "particle",
    "sound",
    "light",
];

/// Every 256 objects the walk checks the wall-clock deadline (matching the
/// cadence the task specifies), so a slow environment cannot make the
/// per-object `Instant::now()` call itself dominate the walk's cost.
const DEADLINE_CHECK_STRIDE: usize = 256;

/// Parse `bytes` and walk its object family, filling `required`/
/// `detected`/`unknown` under `caps`. Returns `Err(InventoryError::Parse)`
/// only when `bytes` is not valid JSON at all; every other malformed shape
/// is counted, never rejected (see the module docs). `deadline` is checked
/// every `DEADLINE_CHECK_STRIDE` objects; an expired deadline stops the
/// walk early exactly like `max_objects_walked` (see `limits_hit`).
pub fn inventory_scene_json(
    bytes: &[u8],
    caps: &InventoryCaps,
    deadline: Instant,
) -> Result<Inventory, InventoryError> {
    let root: Value = serde_json::from_slice(bytes).map_err(|_| InventoryError::Parse)?;

    let mut builder = Builder::new(caps);

    let Some(root_obj) = root.as_object() else {
        builder.unknown_type("$".to_string());
        return Ok(builder.finish());
    };
    for key in root_obj.keys() {
        if !ROOT_KEYS.contains(&key.as_str()) {
            builder.unknown_key(key.clone());
        }
    }

    let Some(objects_value) = root_obj.get("objects") else {
        return Ok(builder.finish());
    };
    let Some(objects_array) = objects_value.as_array() else {
        builder.unknown_type("objects".to_string());
        return Ok(builder.finish());
    };

    for (index, entry) in objects_array.iter().enumerate() {
        if index >= caps.max_objects_walked {
            builder.limits_hit.push("objects-cap");
            builder.truncate_all();
            break;
        }
        if index.is_multiple_of(DEADLINE_CHECK_STRIDE) && Instant::now() >= deadline {
            builder.limits_hit.push("timeout");
            builder.truncate_all();
            break;
        }
        let Some(object) = entry.as_object() else {
            builder.unknown_type(format!("objects[{index}]"));
            continue;
        };
        builder.walk_object(object, index);
    }

    Ok(builder.finish())
}

/// Accumulates one `inventory_scene_json` call. Kept separate from
/// `Inventory` itself so the public type stays a plain data record.
struct Builder<'a> {
    caps: &'a InventoryCaps,
    detected: std::collections::BTreeMap<&'static str, (usize, Vec<String>, bool)>,
    required: std::collections::BTreeSet<&'static str>,
    unknown_keys: usize,
    unknown_types: usize,
    unknown_objects: usize,
    unknown_samples: Vec<String>,
    unknown_truncated: bool,
    limits_hit: Vec<&'static str>,
}

impl<'a> Builder<'a> {
    fn new(caps: &'a InventoryCaps) -> Self {
        Self {
            caps,
            detected: std::collections::BTreeMap::new(),
            required: std::collections::BTreeSet::new(),
            unknown_keys: 0,
            unknown_types: 0,
            unknown_objects: 0,
            unknown_samples: Vec::new(),
            unknown_truncated: false,
            limits_hit: Vec::new(),
        }
    }

    fn unknown_key(&mut self, key: String) {
        self.unknown_keys += 1;
        self.sample(key);
    }

    fn unknown_type(&mut self, path: String) {
        self.unknown_types += 1;
        self.sample(path);
    }

    /// Bounded top-K-smallest insertion: keeps at most `max_samples`
    /// entries, always the lexicographically smallest seen so far, sorted.
    /// Memory never grows past `max_samples` regardless of how many
    /// candidates are offered.
    fn sample(&mut self, path: String) {
        let path = truncate_bytes(&path, self.caps.max_sample_path_bytes);
        if self.unknown_samples.len() < self.caps.max_samples {
            let position = self
                .unknown_samples
                .partition_point(|existing| existing.as_str() < path.as_str());
            self.unknown_samples.insert(position, path);
        } else if self
            .unknown_samples
            .last()
            .is_some_and(|last| path.as_str() < last.as_str())
        {
            let position = self
                .unknown_samples
                .partition_point(|existing| existing.as_str() < path.as_str());
            self.unknown_samples.insert(position, path);
            self.unknown_samples.pop();
            self.unknown_truncated = true;
        } else {
            self.unknown_truncated = true;
        }
    }

    fn walk_object(&mut self, object: &Map<String, Value>, index: usize) {
        for key in object.keys() {
            if !OBJECT_KEYS.contains(&key.as_str()) {
                self.unknown_key(format!("objects[{index}].{key}"));
            }
        }

        // WE allows property-bound visibility objects
        // (`{"user": ..., "value": ...}`); resolving the bound value is
        // SR-11 scope. Here the object only needs to count as active — a
        // malformed/bound `visible` must never silently drop a capability
        // out of `required`.
        let active = match object.get("visible") {
            None => true,
            Some(Value::Bool(visible)) => *visible,
            Some(Value::Object(_)) => true,
            Some(_) => {
                self.unknown_type(format!("objects[{index}].visible"));
                true
            }
        };

        let id = logical_id(object, index, self.caps.max_sample_path_bytes);

        // Discriminating-field classification (mirrors
        // `kwe_core::sceneobjects::classify_scene_object`'s priority
        // order: image first, then particle, then text; this SR-0c slice
        // additionally names sound/light, not yet classified anywhere else
        // in this workspace, after those. `video` is deliberately not a
        // discriminator here — the task's §1 discriminator list omits it,
        // so `scene.layer.video` detection is out of this slice's scope.
        let primary = if object.contains_key("image") || object.contains_key("model") {
            Some("scene.layer.image")
        } else if object.contains_key("particle") {
            Some("scene.particle")
        } else if object.contains_key("text") {
            Some("scene.layer.text")
        } else if object.contains_key("sound") {
            Some("scene.layer.sound")
        } else if object.contains_key("light") {
            Some("scene.lighting")
        } else {
            None
        };
        let has_effects = object
            .get("effects")
            .and_then(Value::as_array)
            .is_some_and(|effects| !effects.is_empty());

        match primary {
            Some(capability) => self.record(capability, &id, active),
            None => self.unknown_objects += 1,
        }
        if has_effects {
            self.record("scene.effects", &id, active);
        }
    }

    fn record(&mut self, capability: &'static str, id: &str, active: bool) {
        let entry = self
            .detected
            .entry(capability)
            .or_insert_with(|| (0, Vec::new(), false));
        entry.0 += 1;
        if entry.1.len() < self.caps.max_samples {
            let position = entry
                .1
                .partition_point(|existing: &String| existing.as_str() < id);
            entry.1.insert(position, id.to_string());
        } else if entry.1.last().is_some_and(|last| id < last.as_str()) {
            let position = entry
                .1
                .partition_point(|existing: &String| existing.as_str() < id);
            entry.1.insert(position, id.to_string());
            entry.1.pop();
            entry.2 = true;
        } else {
            entry.2 = true;
        }
        if active {
            self.required.insert(capability);
        }
    }

    /// The walk stopped early (objects-cap or timeout): every list that
    /// could have had more entries is marked truncated. Counts themselves
    /// are already exact for everything walked so far — only the sample
    /// lists' completeness claim changes.
    fn truncate_all(&mut self) {
        self.unknown_truncated = true;
        for (_, _, truncated) in self.detected.values_mut() {
            *truncated = true;
        }
    }

    fn finish(self) -> Inventory {
        Inventory {
            required: self.required.into_iter().map(str::to_string).collect(),
            detected: self
                .detected
                .into_iter()
                .map(
                    |(capability, (count, objects, truncated))| DetectedCapability {
                        capability,
                        count,
                        objects,
                        truncated,
                    },
                )
                .collect(),
            unknown: UnknownCounts {
                keys: self.unknown_keys,
                types: self.unknown_types,
                objects: self.unknown_objects,
                samples: self.unknown_samples,
                truncated: self.unknown_truncated,
            },
            limits_hit: self.limits_hit,
        }
    }
}

/// The object's `id:<value>` when `id` is a number or string, else
/// `index:<n>` — bounded the same way a sample path is.
fn logical_id(object: &Map<String, Value>, index: usize, max_bytes: usize) -> String {
    let rendered = match object.get("id") {
        Some(Value::Number(number)) => Some(format!("id:{number}")),
        Some(Value::String(text)) => Some(format!("id:{text}")),
        _ => None,
    };
    match rendered {
        Some(rendered) => truncate_bytes(&rendered, max_bytes),
        None => format!("index:{index}"),
    }
}

/// Truncate `value` to at most `max_bytes` bytes, never splitting a UTF-8
/// character.
fn truncate_bytes(value: &str, max_bytes: usize) -> String {
    if value.len() <= max_bytes {
        return value.to_string();
    }
    let mut end = max_bytes;
    while end > 0 && !value.is_char_boundary(end) {
        end -= 1;
    }
    value[..end].to_string()
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::time::Duration;

    fn far_deadline() -> Instant {
        Instant::now() + Duration::from_secs(30)
    }

    fn caps() -> InventoryCaps {
        InventoryCaps::default()
    }

    fn inventory(json: &str) -> Inventory {
        inventory_scene_json(json.as_bytes(), &caps(), far_deadline()).expect("valid json fixture")
    }

    fn detected_ids<'a>(inv: &'a Inventory, capability: &str) -> Option<&'a DetectedCapability> {
        inv.detected.iter().find(|d| d.capability == capability)
    }

    /// Golden: one visible image object (with id), one visible:false text
    /// object, one particle object with a non-empty effects array, one
    /// unclassifiable object, and one unknown root key.
    #[test]
    fn golden_object_family_inventory() {
        let json = r#"{
            "general": {},
            "bogusRootKey": 1,
            "objects": [
                {"id": 7, "image": "textures/a.png", "visible": true},
                {"name": "caption", "text": "hi", "visible": false},
                {"name": "sparks", "particle": {"texture": "t.png"}, "effects": [{"name": "glow"}]},
                {"name": "mystery"}
            ]
        }"#;
        let inv = inventory(json);

        assert_eq!(inv.unknown.keys, 1);
        assert_eq!(inv.unknown.samples, vec!["bogusRootKey".to_string()]);
        assert_eq!(inv.unknown.types, 0);
        assert_eq!(inv.unknown.objects, 1);

        let image = detected_ids(&inv, "scene.layer.image").expect("image detected");
        assert_eq!(image.count, 1);
        assert_eq!(image.objects, vec!["id:7".to_string()]);
        assert!(!image.truncated);

        let text = detected_ids(&inv, "scene.layer.text").expect("text detected");
        assert_eq!(text.count, 1);
        assert_eq!(text.objects, vec!["index:1".to_string()]);

        let particle = detected_ids(&inv, "scene.particle").expect("particle detected");
        assert_eq!(particle.count, 1);
        assert_eq!(particle.objects, vec!["index:2".to_string()]);

        let effects = detected_ids(&inv, "scene.effects").expect("effects detected");
        assert_eq!(effects.count, 1);
        assert_eq!(effects.objects, vec!["index:2".to_string()]);

        assert_eq!(
            inv.required,
            vec![
                "scene.effects".to_string(),
                "scene.layer.image".to_string(),
                "scene.particle".to_string(),
            ],
            "the visible:false text object must not appear in required"
        );
        assert!(!inv.required.contains(&"scene.layer.text".to_string()));
    }

    /// Same input twice → byte-identical Inventory (the record built from
    /// it, including its digest, is therefore identical too — main.rs's
    /// own determinism test covers the digest end to end).
    #[test]
    fn same_input_is_deterministic() {
        let json = r#"{"objects": [
            {"id": "a", "image": "x.png"},
            {"id": "b", "particle": {"texture": "t.png"}}
        ]}"#;
        assert_eq!(inventory(json), inventory(json));
    }

    /// (a) `objects` is a string, not an array: a type mismatch, never a
    /// parse failure.
    #[test]
    fn non_array_objects_is_an_unknown_type_not_a_parse_failure() {
        let inv = inventory(r#"{"objects": "nope"}"#);
        assert_eq!(inv.unknown.types, 1);
        assert_eq!(inv.unknown.samples, vec!["objects".to_string()]);
        assert!(inv.detected.is_empty());
    }

    /// (b) A scene declaring far more objects than `max_objects_walked`
    /// stops the walk at the cap and marks every affected list truncated.
    #[test]
    fn objects_beyond_the_cap_stop_the_walk_truncated() {
        let mut json = String::from(r#"{"objects": ["#);
        let total = InventoryCaps::default().max_objects_walked + 50;
        for i in 0..total {
            if i > 0 {
                json.push(',');
            }
            json.push_str(r#"{"image": "x.png"}"#);
        }
        json.push_str("]}");
        let inv = inventory_scene_json(json.as_bytes(), &caps(), far_deadline()).unwrap();
        assert_eq!(inv.limits_hit, vec!["objects-cap"]);
        let image = detected_ids(&inv, "scene.layer.image").unwrap();
        assert_eq!(image.count, InventoryCaps::default().max_objects_walked);
        assert!(image.truncated);
    }

    /// (c) Invalid JSON syntax is the one case this module reports as an
    /// error, not a counted unknown.
    #[test]
    fn invalid_json_is_a_parse_error() {
        let result = inventory_scene_json(b"{not json", &caps(), far_deadline());
        assert_eq!(result, Err(InventoryError::Parse));
    }

    /// (d) A pathologically deep nested value under a known key never
    /// recurses in this module's own walk (the walk only ever looks at
    /// `objects[i]`'s immediate fields) — and, measured directly on this
    /// workspace's serde_json (1.0.151), a 10_000-deep nested array
    /// already fails to parse at all ("recursion limit exceeded"), so it
    /// never reaches this module's code in the first place. Either way,
    /// there is no stack growth and no hang: this asserts the actual
    /// observed outcome (a clean `Err`, not a crash), which is the safety
    /// property that matters — see the module docs for why this differs
    /// from the task's literal expectation that it "parses via serde_json".
    #[test]
    fn deeply_nested_value_under_a_known_key_never_recurses_and_never_crashes() {
        let mut nested = String::new();
        for _ in 0..10_000 {
            nested.push('[');
        }
        nested.push('1');
        for _ in 0..10_000 {
            nested.push(']');
        }
        let json = format!(r#"{{"general": {nested}, "objects": []}}"#);
        let result = inventory_scene_json(json.as_bytes(), &caps(), far_deadline());
        assert_eq!(result, Err(InventoryError::Parse));
    }

    /// An expired deadline stops the walk exactly like the objects cap.
    #[test]
    fn expired_deadline_stops_the_walk() {
        let json = r#"{"objects": [{"image": "x.png"}]}"#;
        let expired = Instant::now() - Duration::from_secs(1);
        let inv = inventory_scene_json(json.as_bytes(), &caps(), expired).unwrap();
        assert_eq!(inv.limits_hit, vec!["timeout"]);
        assert!(inv.detected.is_empty());
    }

    /// `sound` and `light` are recognized discriminators even though no
    /// parser elsewhere in this workspace consumes them yet, and are never
    /// double-counted as unknown keys.
    #[test]
    fn sound_and_light_objects_classify_and_are_not_unknown_keys() {
        let inv = inventory(r#"{"objects": [{"sound": "s.mp3"}, {"light": {}}]}"#);
        assert_eq!(inv.unknown.keys, 0);
        assert_eq!(detected_ids(&inv, "scene.layer.sound").unwrap().count, 1);
        assert_eq!(detected_ids(&inv, "scene.lighting").unwrap().count, 1);
    }

    /// A non-object `objects[]` entry is skipped, never a parse failure,
    /// and is counted as an unknown type with its index in the path.
    #[test]
    fn non_object_array_entries_are_skipped_and_counted() {
        let inv = inventory(r#"{"objects": [7, {"image": "x.png"}]}"#);
        assert_eq!(inv.unknown.types, 1);
        assert_eq!(inv.unknown.samples, vec!["objects[0]".to_string()]);
        assert_eq!(detected_ids(&inv, "scene.layer.image").unwrap().count, 1);
    }
}
