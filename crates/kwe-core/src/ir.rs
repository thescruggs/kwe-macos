// SPDX-License-Identifier: GPL-3.0-or-later
//! SR-2b: a typed scene.json intermediate representation with unknown-field
//! bags — the plan §4.1/§4.2 "typed IR" and "authored/runtime split" made
//! real for the first time.
//!
//! **This module adds the type + parser only — no existing caller migrates
//! to it in this slice** (mirrors SR-2a's own scope discipline). SR-2c
//! wires the first family through a differential adapter that runs both
//! the OLD `kwe-scene-renderer::scene::parse_scene_json` path and this new
//! IR over the same fixtures and asserts agreement.
//!
//! `docs/SR2.md`'s "scene.json field table" is the differential baseline
//! this module's typed field set was built from (conductor decision (c)):
//! every typed field here mirrors a field
//! `kwe-scene-renderer/src/scene.rs`/`kwe-core/src/sceneobjects.rs` ACTUALLY
//! reads today, with the SAME JSON key(s), the SAME alias precedence, and
//! the SAME default. Two deliberate departures from "mirror exactly",
//! both required by conductor decision (b) ("authored state only, never
//! runtime state") and documented in `docs/SR2.md`:
//!
//! 1. **No range clamping / rejection.** The renderer's `clamp_*`
//!    functions and hard rejections (an out-of-range `alpha`, a `visible`
//!    that isn't a bool) are RENDERING policy, not authored content. Every
//!    numeric IR field holds the coerced-but-UNclamped authored value; a
//!    shape the typed field genuinely cannot represent (wrong JSON type
//!    entirely, not just an out-of-range number) falls back to that
//!    field's default AND the raw value is preserved in the nearest
//!    [`UnknownBag`] under its original key — the IR never silently loses
//!    an authored value, but it also never rejects a whole object/scene
//!    over one field's shape the way the renderer does.
//! 2. **`speed`/`speedMin`/`speedMax` (particle) are left in the unknown
//!    bag entirely, not typed.** Per the study for this slice:
//!    `speedMin`'s default is `speed`'s own resolved value, and
//!    `speedMax`'s default is `speedMin`'s resolved value, with a final
//!    min/max swap — a field whose interpretation depends on a SIBLING
//!    field's resolved value, not a static default. Typing this without
//!    fabricating a derived number the author never wrote would require
//!    baking rendering-time derivation logic into the IR (exactly what
//!    decision (b) rules out). Per this slice's own STOP instruction, all
//!    three keys are left unread here — they land in the particle object's
//!    unknown bag byte-faithfully, and a later slice can decide how (or
//!    whether) to represent the derived pair.

use std::collections::{BTreeMap, BTreeSet};

use serde_json::{Map, Value, json};
use thiserror::Error;

use crate::sceneobjects::{SceneObjectKind, classify_scene_object, scene_property_value};

/// Hard cap on the raw `objects` array's length. **Unlike the SR-0c
/// inspector's identically-valued `max_objects_walked` (a SAMPLING budget
/// for a metadata-only scan, not a structural limit — see
/// `docs/SR2.md`)**, this is a REFUSAL bound: over-cap is
/// [`IrError::ObjectsCap`], never a silent truncation. A truncated IR
/// would draw a truncated scene if it were ever handed to a renderer, so
/// (unlike the inventory walk, which only ever needs a representative
/// sample) the IR must never drop an object silently. 4096 comfortably
/// covers the local corpus (the same rationale the inspector's own
/// comment gives), reused here as a NEW cap for a different purpose, not
/// as evidence the renderer itself enforces one — the real load path
/// (`kwe-scene-renderer/src/scene.rs`) has no raw-array-length cap at all
/// (see `docs/SR2.md`).
pub const MAX_OBJECTS: usize = 4096;

pub const SCHEMA_VERSION: u32 = 1;

/// One JSON object level's leftover keys: every key present that no typed
/// field consumed, raw value preserved byte-faithfully (well, structurally
/// — `serde_json::Value` equality, not literal byte spans). An alias pair
/// (e.g. `tint`/`color`) consumes only the WINNING spelling; the losing
/// spelling, when also present, lands here under its own key rather than
/// being silently dropped — see the module doc's departure (1).
///
/// `BTreeMap` iteration order is lexicographic by key, not authored
/// insertion order — JSON *objects* are unordered by spec, so this is
/// lossless; the authored ARRAY order of `objects[]` and `effects[]` is
/// preserved exactly, by `Vec` position, elsewhere in this module.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct UnknownBag(pub BTreeMap<String, Value>);

impl UnknownBag {
    pub fn is_empty(&self) -> bool {
        self.0.is_empty()
    }

    pub fn len(&self) -> usize {
        self.0.len()
    }

    pub fn get(&self, key: &str) -> Option<&Value> {
        self.0.get(key)
    }
}

/// Every key of `object` not named in `consumed` becomes an unknown entry,
/// value cloned as-is.
fn collect_unknown(object: &Map<String, Value>, consumed: &BTreeSet<String>) -> UnknownBag {
    let mut bag = BTreeMap::new();
    for (key, value) in object {
        if !consumed.contains(key) {
            bag.insert(key.clone(), value.clone());
        }
    }
    UnknownBag(bag)
}

/// Read `object.get(winner)`, falling back to `object.get(loser)` when
/// `winner` is absent — the exact `.or_else()` shadowing shape every alias
/// pair in `scene.rs` uses (`blendMode`/`colorBlendMode`,
/// `resolution`/`orthogonalprojection`, `tint`/`color`,
/// `texture`/`material`, `colorn`/`color`). Returns the value actually
/// used (if either key was present) and the SINGLE key name that provided
/// it — the caller marks only that one key consumed, so a losing spelling
/// present alongside the winner still lands in the unknown bag (module doc
/// departure (1)).
fn alias_winner<'a>(
    object: &'a Map<String, Value>,
    winner: &'static str,
    loser: &'static str,
) -> Option<(&'a Value, &'static str)> {
    if let Some(value) = object.get(winner) {
        Some((value, winner))
    } else {
        object.get(loser).map(|value| (value, loser))
    }
}

/// Coerce a (property-unwrapped) JSON value to `f64`: a plain number, or a
/// string parseable as one — the WE numeric-string convention every
/// numeric field in `scene.rs` accepts in at least some form. `None` means
/// the shape genuinely cannot be read as a number (module doc departure
/// (1) then applies: default + raw value preserved in the unknown bag).
fn as_number(value: &Value) -> Option<f64> {
    match value {
        Value::Number(n) => n.as_f64(),
        Value::String(s) => s.trim().parse::<f64>().ok(),
        _ => None,
    }
}

/// Parse a WE vector: a space-separated string (the authored form) or a
/// JSON array, each component coerced via [`as_number`]. `Some` only when
/// the resulting length is one of `allowed_lens` — mirrors `scene.rs`'s
/// own `parse_vector`'s shape acceptance (2-or-3 for origin/angles, 3-or-4
/// for tint/color, etc.), minus its range clamping (departure (1)).
fn as_vector(value: &Value, allowed_lens: &[usize]) -> Option<Vec<f32>> {
    let components: Vec<f64> = match value {
        Value::Array(items) => items.iter().map(as_number).collect::<Option<_>>()?,
        Value::String(s) => s
            .split_whitespace()
            .map(|token| token.parse::<f64>().ok())
            .collect::<Option<_>>()?,
        _ => return None,
    };
    if allowed_lens.contains(&components.len()) {
        Some(components.into_iter().map(|c| c as f32).collect())
    } else {
        None
    }
}

// ---------------------------------------------------------------------------
// Top level
// ---------------------------------------------------------------------------

#[derive(Debug, Error)]
pub enum IrError {
    #[error("invalid JSON: {0}")]
    Parse(String),
    #[error("scene.json root must be an object")]
    NotAnObject,
    #[error("scene.json \"objects\" has more than {MAX_OBJECTS} entries")]
    ObjectsCap,
    #[error("scene.json \"objects\" must be an array")]
    ObjectsNotAnArray,
    #[error("scene.json \"objects\"[{index}] must be an object")]
    ObjectEntryNotAnObject { index: usize },
}

/// The whole scene, as authored — no I/O, no effect-file resolution, no
/// range clamping. `schema_version` is always [`SCHEMA_VERSION`] on a
/// value this module produced; it exists so a FUTURE schema change has
/// somewhere to record itself without breaking every existing `SceneIr`
/// value silently.
#[derive(Debug, Clone, PartialEq)]
pub struct SceneIr {
    pub schema_version: u32,
    pub general: GeneralIr,
    pub objects: Vec<ObjectIr>,
    /// Every root-level key besides `"general"`/`"objects"` — today's
    /// `scene.rs` never reads a third top-level key at all (see
    /// `docs/SR2.md`), so in practice this is everything but those two.
    pub unknown: UnknownBag,
    /// Authored `id` values that collided: object N authored an `id`
    /// already claimed by an earlier object, so N's [`StableId`] fell back
    /// to `Index` instead. One entry per demoted object, format
    /// `"id {n} reused at index {index}"` — see [`StableId`]'s doc comment
    /// for the assignment rule this records.
    pub duplicate_ids: Vec<String>,
}

pub fn parse_scene_ir(bytes: &[u8]) -> Result<SceneIr, IrError> {
    let root: Value =
        serde_json::from_slice(bytes).map_err(|error| IrError::Parse(error.to_string()))?;
    let root_object = root.as_object().ok_or(IrError::NotAnObject)?;

    let mut top_consumed: BTreeSet<String> = BTreeSet::new();
    top_consumed.insert("general".to_string());
    top_consumed.insert("objects".to_string());

    let general = match root_object.get("general").and_then(Value::as_object) {
        Some(general_object) => parse_general_ir(general_object),
        None => GeneralIr::default(),
    };

    // Mirrors `scene.rs`'s own `parse_objects`: an ABSENT "objects" key
    // defaults to empty (a scene with no objects is valid), but a PRESENT
    // key that is not an array is a shape reject — legacy's own
    // `value.as_array().ok_or_else(...)` does not tolerate a non-array
    // "objects" the way it tolerates a missing one.
    let raw_objects: &[Value] = match root_object.get("objects") {
        None => &[],
        Some(Value::Array(items)) => items,
        Some(_) => return Err(IrError::ObjectsNotAnArray),
    };
    if raw_objects.len() > MAX_OBJECTS {
        return Err(IrError::ObjectsCap);
    }

    let mut objects = Vec::with_capacity(raw_objects.len());
    let mut seen_ids: BTreeSet<i64> = BTreeSet::new();
    let mut duplicate_ids: Vec<String> = Vec::new();
    for (index, entry) in raw_objects.iter().enumerate() {
        let object = entry
            .as_object()
            .ok_or(IrError::ObjectEntryNotAnObject { index })?;
        objects.push(parse_object_ir(
            object,
            index,
            &mut seen_ids,
            &mut duplicate_ids,
        ));
    }

    let unknown = collect_unknown(root_object, &top_consumed);

    Ok(SceneIr {
        schema_version: SCHEMA_VERSION,
        general,
        objects,
        unknown,
        duplicate_ids,
    })
}

// ---------------------------------------------------------------------------
// general
// ---------------------------------------------------------------------------

/// The `general` block, as authored. Fields mirror exactly the 5 keys
/// `scene.rs`'s `parse_scene_json` reads out of `general` today — see
/// `docs/SR2.md`'s field table; no other `general` key (e.g. a
/// hypothetical `camera`) has any parsing logic to mirror.
#[derive(Debug, Clone, PartialEq)]
pub struct GeneralIr {
    /// `clearcolor`: `[r,g,b,a]` array or `"r g b"` string form (alpha
    /// implied 1.0 for the string form, matching `scene.rs`). Default
    /// `[0,0,0,1]`.
    pub clear_color: [f32; 4],
    /// `resolution`, falling back to `orthogonalprojection` (`{"width":W,
    /// "height":H}`) when `resolution` is absent — the SAME alias
    /// precedence `scene.rs` uses (`resolution` wins outright; when it is
    /// present, `orthogonalprojection`'s bytes are never even read by the
    /// renderer, so a present-alongside-the-winner `orthogonalprojection`
    /// lands in `unknown` per departure (1)). No `MAX_DIMENSION` rejection
    /// here — the raw authored numbers are kept even if implausibly large.
    pub resolution: Option<(u32, u32)>,
    /// `fps`, a finite float. No `(0.0, 240.0]` range enforcement — that is
    /// the renderer's own rejection policy, not authored content.
    pub fps: Option<f32>,
    /// `script` (the `general.script` reference, unresolved — resolving it
    /// against pkg/scene-dir/assets-root is a later slice's job, not the
    /// IR's).
    pub script: Option<String>,
    pub unknown: UnknownBag,
}

impl Default for GeneralIr {
    fn default() -> Self {
        Self {
            clear_color: [0.0, 0.0, 0.0, 1.0],
            resolution: None,
            fps: None,
            script: None,
            unknown: UnknownBag::default(),
        }
    }
}

fn parse_general_ir(general: &Map<String, Value>) -> GeneralIr {
    let mut consumed: BTreeSet<String> = BTreeSet::new();

    // Every extraction below follows the module doc's departure (1): a key
    // is marked `consumed` ONLY when its raw value was actually read into
    // the typed field it faithfully represents. A key that is present but
    // whose shape does not fit (e.g. `clearcolor` as a number) is left
    // UNCONSUMED on purpose, so its raw value survives into `unknown`
    // instead of being silently replaced by the field's default with no
    // trace.
    let clear_color = match general.get("clearcolor").and_then(parse_clear_color) {
        Some(color) => {
            consumed.insert("clearcolor".to_string());
            color
        }
        None => [0.0, 0.0, 0.0, 1.0],
    };

    let resolution = match alias_winner(general, "resolution", "orthogonalprojection") {
        Some((value, winning_key @ "resolution")) => match parse_resolution_array(value) {
            Some(resolution) => {
                consumed.insert(winning_key.to_string());
                Some(resolution)
            }
            None => None,
        },
        Some((value, winning_key)) => match parse_orthogonal_projection(value) {
            Some(resolution) => {
                consumed.insert(winning_key.to_string());
                Some(resolution)
            }
            None => None,
        },
        None => None,
    };

    let fps = match general
        .get("fps")
        .and_then(as_number)
        .filter(|value| value.is_finite())
    {
        Some(value) => {
            consumed.insert("fps".to_string());
            Some(value as f32)
        }
        None => None,
    };

    let script = match general.get("script").and_then(Value::as_str) {
        Some(script) => {
            consumed.insert("script".to_string());
            Some(script.to_string())
        }
        None => None,
    };

    let unknown = collect_unknown(general, &consumed);
    GeneralIr {
        clear_color,
        resolution,
        fps,
        script,
        unknown,
    }
}

fn parse_clear_color(value: &Value) -> Option<[f32; 4]> {
    match value {
        Value::Array(_) => {
            let components = as_vector(value, &[4])?;
            Some([components[0], components[1], components[2], components[3]])
        }
        Value::String(s) => {
            let tokens: Vec<f64> = s
                .split_whitespace()
                .map(|token| token.parse::<f64>().ok())
                .collect::<Option<_>>()?;
            if tokens.len() != 3 {
                return None;
            }
            Some([tokens[0] as f32, tokens[1] as f32, tokens[2] as f32, 1.0])
        }
        _ => None,
    }
}

fn parse_resolution_array(value: &Value) -> Option<(u32, u32)> {
    let components = as_vector(value, &[2])?;
    let width = components[0];
    let height = components[1];
    if width.is_finite() && height.is_finite() && width > 0.0 && height > 0.0 {
        Some((width as u32, height as u32))
    } else {
        None
    }
}

fn parse_orthogonal_projection(value: &Value) -> Option<(u32, u32)> {
    let object = value.as_object()?;
    let dim = |key: &str| -> Option<u32> {
        let raw = object.get(key)?;
        let number = as_number(raw)?;
        if number.is_finite() && number > 0.0 {
            Some(number as u32)
        } else {
            None
        }
    };
    Some((dim("width")?, dim("height")?))
}

// ---------------------------------------------------------------------------
// objects[]
// ---------------------------------------------------------------------------

/// An object's stable identity across a reload, per plan §4.1: the
/// authored `id` when present and not already claimed by an earlier
/// object in the same `objects[]` array; otherwise its array position.
/// Deterministic and collision-free by construction — the FIRST object to
/// author a given `id` keeps it; every later object that authors the SAME
/// `id` falls back to `Index` instead (recorded in
/// [`SceneIr::duplicate_ids`]), never silently overwriting the earlier
/// claim.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
pub enum StableId {
    Authored(i64),
    Index(usize),
}

fn assign_stable_id(
    authored_id: Option<i64>,
    index: usize,
    seen_ids: &mut BTreeSet<i64>,
    duplicate_ids: &mut Vec<String>,
) -> StableId {
    match authored_id {
        Some(id) if seen_ids.insert(id) => StableId::Authored(id),
        Some(id) => {
            duplicate_ids.push(format!("id {id} reused at index {index}"));
            StableId::Index(index)
        }
        None => StableId::Index(index),
    }
}

/// `visible`, tri-state (SR-0c/SR-11 semantics) — deliberately NOT
/// collapsed to a plain `bool` the way `scene.rs`'s `CommonProps.visible`
/// is. Today's renderer, after unwrapping a `{"user":...,"value":...}`
/// property wrapper, REJECTS the whole object if what's left isn't a
/// bool (`scene.rs:952-966`); the IR instead preserves that non-bool
/// shape as `PropertyBound` rather than losing it to a rejection this
/// module has no equivalent error for (module doc departure (1)).
/// `Absent` is itself meaningful state, distinct from `Bool(true)`'s
/// coerced default — it records that the author wrote no `visible` key at
/// all.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum VisibleIr {
    Bool(bool),
    PropertyBound(Value),
    Absent,
}

/// The layer-common properties every object family shares, minus `name`/
/// `id` (hoisted onto [`ObjectIr`] itself). Mirrors `scene.rs`'s
/// `parse_common_props` field set and defaults exactly; see the module
/// doc for why values here are unclamped.
#[derive(Debug, Clone, PartialEq)]
pub struct CommonPropsIr {
    pub origin: [f32; 2],
    pub angles: [f32; 3],
    pub scale: [f32; 2],
    pub alpha: f32,
    pub visible: VisibleIr,
    /// `blendMode` (wins) / `colorBlendMode` (the corpus key — every
    /// observed real scene.json uses this spelling; `blendMode` wins only
    /// when BOTH happen to be present, which no real corpus scene does).
    pub blend_mode: u32,
    pub brightness: f32,
}

/// One `effects[]` entry, exactly as authored — no effect-FILE resolution
/// (that needs I/O this pure scene.json parse does not have; a later
/// migration's job, see `docs/SR2.md`). `id`/`name`/`visible` read WITHOUT
/// a property-wrapper unwrap (mirrors `sceneeffect.rs::resolve_object_effects`
/// exactly — that function does not unwrap these three, unlike almost
/// everything else in this module).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct EffectRefIr {
    pub id: i64,
    pub name: String,
    pub visible: bool,
    pub file: Option<String>,
    pub passes: Vec<Value>,
    pub unknown: UnknownBag,
    /// The entry's ORIGINAL raw JSON value, byte-faithful. `id`/`name`/
    /// `visible` above are typed WITH a default when absent (0/""/true) —
    /// indistinguishable from an authored `0`/`""`/`true` once typed — so
    /// a consumer that needs to reproduce exactly what was authored (a
    /// scene-family adapter reconstructing a legacy `Vec<Value>` for later
    /// effect-FILE resolution, not this module's own concern) reads this
    /// field instead of reconstructing from the typed ones.
    pub raw: Value,
}

fn parse_effect_ref_ir(entry: &Value) -> Option<EffectRefIr> {
    let object = entry.as_object()?;
    let mut consumed: BTreeSet<String> = BTreeSet::new();

    let id = match object.get("id").and_then(Value::as_i64) {
        Some(id) => {
            consumed.insert("id".to_string());
            id
        }
        None => 0,
    };
    let name = match object.get("name").and_then(Value::as_str) {
        Some(name) => {
            consumed.insert("name".to_string());
            name.to_string()
        }
        None => String::new(),
    };
    let visible = match object.get("visible").and_then(Value::as_bool) {
        Some(visible) => {
            consumed.insert("visible".to_string());
            visible
        }
        None => true,
    };
    let file = match object.get("file").and_then(Value::as_str) {
        Some(file) => {
            consumed.insert("file".to_string());
            Some(file.to_string())
        }
        None => None,
    };
    let passes = match object.get("passes").and_then(Value::as_array) {
        Some(passes) => {
            consumed.insert("passes".to_string());
            passes.clone()
        }
        None => Vec::new(),
    };

    let unknown = collect_unknown(object, &consumed);
    Some(EffectRefIr {
        id,
        name,
        visible,
        file,
        passes,
        unknown,
        raw: entry.clone(),
    })
}

/// `MAX_EFFECTS_PER_OBJECT` (32) — mirrors `scene.rs`'s own `.take(32)`
/// truncation at parse time for Model layers exactly (`sceneeffect.rs`'s
/// cap, re-applied at resolve time too, independently of this module).
/// Unlike the top-level `objects[]` cap, this one DOES truncate rather
/// than refuse: it is an existing, already-truncating renderer behavior
/// being mirrored, not a new refuse-on-overflow bound the IR is
/// introducing (`docs/SR2.md`).
const MAX_EFFECTS_PER_OBJECT: usize = 32;

fn parse_effects_ir(object: &Map<String, Value>) -> Vec<EffectRefIr> {
    match object.get("effects").and_then(Value::as_array) {
        Some(entries) => entries
            .iter()
            .take(MAX_EFFECTS_PER_OBJECT)
            .filter_map(parse_effect_ref_ir)
            .collect(),
        None => Vec::new(),
    }
}

/// Horizontal text alignment. Mirrors `kwe-scene-renderer::text::
/// HorizontalAlign` field-for-field — kwe-core cannot depend on
/// kwe-scene-renderer, so this is its own small mirror rather than a
/// shared type (same relationship `AssetCategory`/kwe-scene-renderer's own
/// per-family caps have in SR-2a).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum HorizontalAlignIr {
    Left,
    Center,
    Right,
}

impl HorizontalAlignIr {
    fn parse_word(s: &str) -> Option<Self> {
        match s.to_ascii_lowercase().as_str() {
            "left" => Some(Self::Left),
            "center" => Some(Self::Center),
            "right" => Some(Self::Right),
            _ => None,
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum VerticalAlignIr {
    Top,
    Center,
    Bottom,
}

impl VerticalAlignIr {
    fn parse_word(s: &str) -> Option<Self> {
        match s.to_ascii_lowercase().as_str() {
            "top" => Some(Self::Top),
            "center" => Some(Self::Center),
            "bottom" => Some(Self::Bottom),
            _ => None,
        }
    }
}

enum Polarity {
    Negative,
    Positive,
    Center,
}

/// Mirrors `scene.rs`'s `parse_text_align` exactly: an exact word match on
/// `key` (`HorizontalAlign`-shaped words), else a `negative`/`positive`
/// substring-containment check on the SAME string, else — only if `key`
/// itself was absent or empty — the same two-step check against the
/// `"alignment"` key, else `Center`. Returns which of `key`/`"alignment"`
/// were actually read (present, regardless of whether they resolved to
/// anything), so the caller can mark exactly those consumed.
fn resolve_polarity(
    object: &Map<String, Value>,
    key: &'static str,
    negative: &str,
    positive: &str,
) -> (Polarity, Vec<&'static str>) {
    let mut consumed = Vec::new();
    let polarity_word = |s: &str| -> Polarity {
        let s = s.to_ascii_lowercase();
        if s.contains(negative) {
            Polarity::Negative
        } else if s.contains(positive) {
            Polarity::Positive
        } else {
            Polarity::Center
        }
    };
    let resolve = |s: &str| -> Polarity {
        match HorizontalAlignIr::parse_word(s) {
            Some(HorizontalAlignIr::Left) => Polarity::Negative,
            Some(HorizontalAlignIr::Right) => Polarity::Positive,
            Some(HorizontalAlignIr::Center) => Polarity::Center,
            None => polarity_word(s),
        }
    };
    if let Some(value) = object.get(key) {
        consumed.push(key);
        if let Some(text) = scene_property_value(value).as_str()
            && !text.is_empty()
        {
            return (resolve(text), consumed);
        }
    }
    if let Some(value) = object.get("alignment") {
        consumed.push("alignment");
        if let Some(text) = scene_property_value(value).as_str() {
            return (resolve(text), consumed);
        }
    }
    (Polarity::Center, consumed)
}

fn resolve_horizontal_align(object: &Map<String, Value>) -> (HorizontalAlignIr, Vec<&'static str>) {
    let (polarity, consumed) = resolve_polarity(object, "horizontalalign", "left", "right");
    let align = match polarity {
        Polarity::Negative => HorizontalAlignIr::Left,
        Polarity::Positive => HorizontalAlignIr::Right,
        Polarity::Center => HorizontalAlignIr::Center,
    };
    (align, consumed)
}

/// Mirrors `scene.rs`'s `parse_text_align_v`: an exact `VerticalAlign`
/// word on `"verticalalign"` first; failing that, the SAME top/bottom
/// polarity machinery re-reads `"verticalalign"` (a genuine double-read in
/// the real code, faithfully mirrored here, not an approximation) and
/// falls back to `"alignment"` exactly like the horizontal case.
fn resolve_vertical_align(object: &Map<String, Value>) -> (VerticalAlignIr, Vec<&'static str>) {
    if let Some(value) = object.get("verticalalign")
        && let Some(text) = scene_property_value(value).as_str()
        && !text.is_empty()
        && let Some(align) = VerticalAlignIr::parse_word(text)
    {
        return (align, vec!["verticalalign"]);
    }
    let (polarity, consumed) = resolve_polarity(object, "verticalalign", "top", "bottom");
    let align = match polarity {
        Polarity::Negative => VerticalAlignIr::Top,
        Polarity::Positive => VerticalAlignIr::Bottom,
        Polarity::Center => VerticalAlignIr::Center,
    };
    (align, consumed)
}

// ---------------------------------------------------------------------------
// Per-family payloads
// ---------------------------------------------------------------------------

#[derive(Debug, Clone, PartialEq)]
pub struct ModelIr {
    pub model_ref: String,
    pub size: [f32; 2],
    pub tint: [f32; 4],
}

#[derive(Debug, Clone, PartialEq)]
pub struct ImageIr {
    pub image: String,
    pub size: [f32; 2],
    pub tint: [f32; 4],
}

#[derive(Debug, Clone, PartialEq)]
pub struct TexturelessImageIr {
    pub size: [f32; 2],
    pub tint: [f32; 4],
}

#[derive(Debug, Clone, PartialEq)]
pub struct VideoIr {
    pub source: Option<String>,
    pub size: [f32; 2],
    pub tint: [f32; 4],
    pub loop_playback: bool,
    pub rate: f32,
}

#[derive(Debug, Clone, PartialEq)]
pub struct TextIr {
    pub text: String,
    pub font: Option<String>,
    /// Raw authored `pointsize` (points, NOT pixels) — the renderer's own
    /// `text::pointsize_to_px` does the ×4-and-clamp conversion; this field
    /// deliberately stays unconverted so it is consistent whether authored
    /// or defaulted (SR-2c fix: an earlier draft of this field defaulted to
    /// the ALREADY-CONVERTED 48px value while an authored value stayed in
    /// raw points, a real unit-mixing bug this comment now guards against).
    pub pointsize: f32,
    pub horizontal_align: HorizontalAlignIr,
    pub vertical_align: VerticalAlignIr,
    /// `"color"` only — text has no `tint` alias, unlike image/video.
    pub color: [f32; 4],
    pub has_size: bool,
}

/// An inline particle system (`"particle"` is a JSON object). `speed`/
/// `speedMin`/`speedMax` are DELIBERATELY absent — see the module doc's
/// departure (2); they are preserved in `ObjectIr::unknown` under their
/// own keys instead.
#[derive(Debug, Clone, PartialEq)]
pub struct ParticleIr {
    pub spawn_rate: f32,
    pub life: f32,
    pub direction: f32,
    pub spread: f32,
    pub size_start: f32,
    pub size_end: f32,
    pub alpha_start: f32,
    pub alpha_end: f32,
    pub gravity: [f32; 2],
    pub color_start: [f32; 4],
    pub color_end: [f32; 4],
    pub max_count: u32,
    /// `texture` (wins) / `material` (loses when both present).
    pub material: Option<String>,
    pub instance_count: f32,
    pub instance_rate: f32,
    pub instance_size: f32,
    pub instance_lifetime: f32,
    pub instance_speed: f32,
    pub instance_alpha: f32,
    /// `instanceoverride.colorn` (wins) / `.color` (loses when both
    /// present) — a WE 3-vector reduced to the mean of its components,
    /// mirroring `scene.rs` exactly (a scalar approximation the runtime
    /// state itself is scalar for, not an IR simplification).
    pub instance_colorn: f32,
}

/// A particle system whose `"particle"` value is a bare string — an
/// external particle-definition file reference (M3f/S4b).
///
/// `instanceoverride` (see [`ParticleIr::instance_colorn`]'s doc comment)
/// is a SIBLING key of `"particle"` on the object, read by `scene.rs`'s
/// `parse_particle_system` UNCONDITIONALLY before it branches on whether
/// `particle` is a string or an object — so a file-referenced particle
/// system gets its instance overrides applied exactly like an inline one,
/// and this struct carries the same 7 typed fields `ParticleIr` does.
#[derive(Debug, Clone, PartialEq)]
pub struct ParticleFileIr {
    pub file_ref: Option<String>,
    pub instance_count: f32,
    pub instance_rate: f32,
    pub instance_size: f32,
    pub instance_lifetime: f32,
    pub instance_speed: f32,
    pub instance_alpha: f32,
    pub instance_colorn: f32,
}

/// One object's family-specific payload. Mirrors `SceneObjectKind`'s own
/// 8 discriminators exactly (this module reuses `classify_scene_object`
/// itself, not a re-derived classification, so the two can never drift) —
/// with one deliberate departure from the task's original literal sketch,
/// documented in `docs/SR2.md`: no separate `Sound`/`Light` variants.
/// Neither exists as a distinct parse path anywhere in the codebase today
/// — a sound object, a light, and anything else the build does not
/// interpret all fall into `SceneObjectKind::Other` identically, with zero
/// family-specific fields read for any of them (conductor decision (c):
/// coverage is exactly today's parsed families, not the full WE
/// vocabulary — inventing a `Sound`/`Light` split here would type
/// structure the renderer does not have). `Unknown` covers that whole
/// bucket; whatever such an object authored is still fully captured in
/// `ObjectIr::common`/`ObjectIr::unknown` — nothing about the OBJECT is
/// lost, only its (nonexistent) family-specific typed fields.
#[derive(Debug, Clone, PartialEq)]
pub enum ObjectKindIr {
    Model(ModelIr),
    Image(ImageIr),
    TexvImage(ImageIr),
    TexturelessImage(TexturelessImageIr),
    Video(VideoIr),
    Particle(ParticleIr),
    ParticleFile(ParticleFileIr),
    Text(TextIr),
    Unknown,
}

#[derive(Debug, Clone, PartialEq)]
pub struct ObjectIr {
    pub stable_id: StableId,
    pub authored_id: Option<i64>,
    pub name: Option<String>,
    pub common: CommonPropsIr,
    pub kind: ObjectKindIr,
    pub effects: Vec<EffectRefIr>,
    pub unknown: UnknownBag,
}

fn parse_object_ir(
    object: &Map<String, Value>,
    index: usize,
    seen_ids: &mut BTreeSet<i64>,
    duplicate_ids: &mut Vec<String>,
) -> ObjectIr {
    let mut consumed: BTreeSet<String> = BTreeSet::new();

    let name = match object.get("name").and_then(Value::as_str) {
        Some(name) => {
            consumed.insert("name".to_string());
            Some(name.to_string())
        }
        None => None,
    };
    let authored_id = match object.get("id").and_then(Value::as_i64) {
        Some(id) => {
            consumed.insert("id".to_string());
            Some(id)
        }
        None => None,
    };
    let stable_id = assign_stable_id(authored_id, index, seen_ids, duplicate_ids);

    let common = parse_common_props_ir(object, &mut consumed);

    let object_kind = classify_scene_object(object);
    let KindResult {
        kind,
        extra_unknown,
    } = parse_kind_ir(object, object_kind, &mut consumed);

    let effects = parse_effects_ir(object);
    if object.get("effects").is_some_and(Value::is_array) {
        consumed.insert("effects".to_string());
    }

    let mut unknown = collect_unknown(object, &consumed);
    unknown.0.extend(extra_unknown);
    ObjectIr {
        stable_id,
        authored_id,
        name,
        common,
        kind,
        effects,
        unknown,
    }
}

/// Extracts a 2-or-3 component vector at `key`, marking `key` consumed
/// only when the shape actually parsed — module doc departure (1): a
/// present-but-wrong-shaped value is left for the unknown bag rather than
/// silently replaced with the default with no trace.
fn vector2(
    object: &Map<String, Value>,
    key: &str,
    consumed: &mut BTreeSet<String>,
    default: [f32; 2],
) -> [f32; 2] {
    match object
        .get(key)
        .and_then(|value| as_vector(scene_property_value(value), &[2, 3]))
    {
        Some(v) => {
            consumed.insert(key.to_string());
            [v[0], v[1]]
        }
        None => default,
    }
}

fn parse_common_props_ir(
    object: &Map<String, Value>,
    consumed: &mut BTreeSet<String>,
) -> CommonPropsIr {
    let origin = vector2(object, "origin", consumed, [0.0, 0.0]);
    let angles = match object
        .get("angles")
        .and_then(|value| as_vector(scene_property_value(value), &[2, 3]))
    {
        Some(v) => {
            consumed.insert("angles".to_string());
            if v.len() == 2 {
                [v[0], v[1], 0.0]
            } else {
                [v[0], v[1], v[2]]
            }
        }
        None => [0.0, 0.0, 0.0],
    };
    let scale = vector2(object, "scale", consumed, [1.0, 1.0]);
    let alpha = match object
        .get("alpha")
        .and_then(|value| as_number(scene_property_value(value)))
        .filter(|v| v.is_finite())
    {
        Some(v) => {
            consumed.insert("alpha".to_string());
            v as f32
        }
        None => 1.0,
    };
    let visible = match object.get("visible") {
        Some(value) => {
            consumed.insert("visible".to_string());
            match scene_property_value(value) {
                Value::Bool(b) => VisibleIr::Bool(*b),
                other => VisibleIr::PropertyBound(other.clone()),
            }
        }
        None => VisibleIr::Absent,
    };
    let blend_mode =
        match alias_winner(object, "blendMode", "colorBlendMode").and_then(|(value, key)| {
            as_number(value)
                .filter(|v| v.is_finite() && *v >= 0.0)
                .map(|v| (v, key))
        }) {
            Some((v, key)) => {
                consumed.insert(key.to_string());
                v as u32
            }
            None => 0,
        };
    let brightness = match object
        .get("brightness")
        .and_then(|value| as_number(scene_property_value(value)))
        .filter(|v| v.is_finite())
    {
        Some(v) => {
            consumed.insert("brightness".to_string());
            v as f32
        }
        None => 1.0,
    };

    CommonPropsIr {
        origin,
        angles,
        scale,
        alpha,
        visible,
        blend_mode,
        brightness,
    }
}

fn parse_size_and_tint(
    object: &Map<String, Value>,
    consumed: &mut BTreeSet<String>,
) -> ([f32; 2], [f32; 4]) {
    let size = vector2(object, "size", consumed, [0.0, 0.0]);
    let tint = match alias_winner(object, "tint", "color")
        .and_then(|(value, key)| as_vector(scene_property_value(value), &[3, 4]).map(|v| (v, key)))
    {
        Some((v, key)) => {
            consumed.insert(key.to_string());
            let mut tint = [1.0, 1.0, 1.0, 1.0];
            for (slot, component) in tint.iter_mut().zip(v.iter()) {
                *slot = *component;
            }
            tint
        }
        None => [1.0, 1.0, 1.0, 1.0],
    };
    (size, tint)
}

/// `parse_kind_ir`'s return, plus any SYNTHETIC unknown-bag entries the
/// family parser needs to inject directly rather than via the normal
/// "present key not in `consumed`" diff — today only the Particle family
/// uses this, for `"particle"`'s own un-typed sub-keys (see
/// `parse_particle_ir`'s doc comment).
struct KindResult {
    kind: ObjectKindIr,
    extra_unknown: BTreeMap<String, Value>,
}

fn parse_kind_ir(
    object: &Map<String, Value>,
    kind: SceneObjectKind,
    consumed: &mut BTreeSet<String>,
) -> KindResult {
    let mut extra_unknown = BTreeMap::new();
    let kind_ir = match kind {
        SceneObjectKind::Model => {
            consumed.insert("image".to_string());
            let model_ref = object
                .get("image")
                .map(scene_property_value)
                .and_then(Value::as_str)
                .unwrap_or_default()
                .to_string();
            let (size, tint) = parse_size_and_tint(object, consumed);
            ObjectKindIr::Model(ModelIr {
                model_ref,
                size,
                tint,
            })
        }
        SceneObjectKind::Image | SceneObjectKind::TexvImage => {
            consumed.insert("image".to_string());
            let image = object
                .get("image")
                .map(scene_property_value)
                .and_then(Value::as_str)
                .unwrap_or_default()
                .to_string();
            let (size, tint) = parse_size_and_tint(object, consumed);
            let payload = ImageIr { image, size, tint };
            if kind == SceneObjectKind::TexvImage {
                ObjectKindIr::TexvImage(payload)
            } else {
                ObjectKindIr::Image(payload)
            }
        }
        SceneObjectKind::TexturelessImage => {
            // `image` is present here but did NOT unwrap to a string
            // (classify_scene_object's own discriminator) — there is no
            // typed slot to hold whatever it actually is, so it is
            // deliberately left UNCONSUMED: its raw value survives into
            // the object's unknown bag instead of being silently dropped
            // (module doc departure (1)).
            let (size, tint) = parse_size_and_tint(object, consumed);
            ObjectKindIr::TexturelessImage(TexturelessImageIr { size, tint })
        }
        SceneObjectKind::Video => parse_video_ir(object, consumed),
        SceneObjectKind::Particle => {
            // classify_scene_object only reaches Particle when `particle`
            // unwraps to a JSON object, so consuming the top-level key is
            // always safe here; parse_particle_ir tracks which of ITS OWN
            // sub-keys it read and returns the rest as `residue`.
            consumed.insert("particle".to_string());
            let (particle, residue) = parse_particle_ir(object, consumed);
            if !residue.is_empty() {
                extra_unknown.insert("particle".to_string(), Value::Object(residue));
            }
            ObjectKindIr::Particle(particle)
        }
        SceneObjectKind::ParticleFile => {
            // Unlike Particle, classify_scene_object reaches ParticleFile
            // for ANY non-Particle shape of `particle` — including a
            // non-string one (e.g. a bare number). Only consume when the
            // unwrap actually produced a string; otherwise the raw shape
            // is left for the unknown bag. `instanceoverride` is read the
            // same way regardless (see `ParticleFileIr`'s doc comment) —
            // `scene.rs` applies it before branching on the shape of
            // `particle` at all.
            let (
                instance_count,
                instance_rate,
                instance_size,
                instance_lifetime,
                instance_speed,
                instance_alpha,
                instance_colorn,
            ) = parse_object_instance_override(object, consumed);
            let file_ref = match object
                .get("particle")
                .map(scene_property_value)
                .and_then(Value::as_str)
            {
                Some(file_ref) => {
                    consumed.insert("particle".to_string());
                    Some(file_ref.to_string())
                }
                None => None,
            };
            ObjectKindIr::ParticleFile(ParticleFileIr {
                file_ref,
                instance_count,
                instance_rate,
                instance_size,
                instance_lifetime,
                instance_speed,
                instance_alpha,
                instance_colorn,
            })
        }
        SceneObjectKind::Text => parse_text_ir(object, consumed),
        SceneObjectKind::Other => ObjectKindIr::Unknown,
    };
    KindResult {
        kind: kind_ir,
        extra_unknown,
    }
}

/// Bool coercion mirroring `scene.rs`'s `loop` field: a plain bool passes
/// through; a string `"false"`/`"0"`/`"no"` (case-insensitive) is false,
/// any other string true; a numeric `0` is false, any other number true;
/// any other shape defaults true — never a rejection, matching the
/// renderer's own tolerance for this one field.
fn as_bool_tolerant(value: &Value, default: bool) -> bool {
    match scene_property_value(value) {
        Value::Bool(b) => *b,
        // SR-2c fix: `scene.rs`'s own `loop` parser trims before
        // lowercasing (`value.trim().to_ascii_lowercase()`); an earlier
        // draft of this function omitted the trim, so `" false "` fell
        // through to the tolerant-default `true` instead of matching.
        Value::String(s) => !matches!(s.trim().to_ascii_lowercase().as_str(), "false" | "0" | "no"),
        Value::Number(n) => n.as_f64().is_none_or(|n| n != 0.0),
        _ => default,
    }
}

fn parse_video_ir(object: &Map<String, Value>, consumed: &mut BTreeSet<String>) -> ObjectKindIr {
    // classify_scene_object reaches Video on `contains_key("video")` alone
    // — no type check — so a non-string `video` value is possible here;
    // only consume the key when it actually yielded a string, mirroring
    // module doc departure (1).
    let source = match object
        .get("video")
        .map(scene_property_value)
        .and_then(Value::as_str)
    {
        Some(source) => {
            consumed.insert("video".to_string());
            Some(source.to_string())
        }
        None => None,
    };
    let (size, tint) = parse_size_and_tint(object, consumed);
    let loop_playback = match object.get("loop") {
        Some(value) => {
            consumed.insert("loop".to_string());
            as_bool_tolerant(value, true)
        }
        None => true,
    };
    let rate = match object
        .get("rate")
        .and_then(|value| as_number(scene_property_value(value)))
        .filter(|v| v.is_finite())
    {
        Some(v) => {
            consumed.insert("rate".to_string());
            v as f32
        }
        None => 1.0,
    };
    ObjectKindIr::Video(VideoIr {
        source,
        size,
        tint,
        loop_playback,
        rate,
    })
}

fn parse_text_ir(object: &Map<String, Value>, consumed: &mut BTreeSet<String>) -> ObjectKindIr {
    // classify_scene_object reaches Text on `"text"` key presence alone —
    // no type check — so a non-string `text` value is possible; only
    // consume when it actually yielded a string (module doc departure (1)).
    let text = match object
        .get("text")
        .map(scene_property_value)
        .and_then(Value::as_str)
    {
        Some(text) => {
            consumed.insert("text".to_string());
            text.to_string()
        }
        None => String::new(),
    };
    let font = match object
        .get("font")
        .map(scene_property_value)
        .and_then(Value::as_str)
    {
        Some(font) => {
            consumed.insert("font".to_string());
            if font.is_empty() {
                None
            } else {
                Some(font.to_string())
            }
        }
        None => None,
    };
    // Raw points (text::DEFAULT_POINT_SIZE), NOT pixels — see the
    // `TextIr::pointsize` field doc comment for why this must stay
    // consistent between the authored and defaulted cases.
    const DEFAULT_POINTSIZE_POINTS: f32 = 12.0;
    let pointsize = match object
        .get("pointsize")
        .and_then(|value| as_number(scene_property_value(value)))
        .filter(|v| v.is_finite())
    {
        Some(v) => {
            consumed.insert("pointsize".to_string());
            v as f32
        }
        None => DEFAULT_POINTSIZE_POINTS,
    };
    let (horizontal_align, horizontal_consumed) = resolve_horizontal_align(object);
    for key in horizontal_consumed {
        consumed.insert(key.to_string());
    }
    let (vertical_align, vertical_consumed) = resolve_vertical_align(object);
    for key in vertical_consumed {
        consumed.insert(key.to_string());
    }
    let color = match object
        .get("color")
        .and_then(|value| as_vector(scene_property_value(value), &[3, 4]))
    {
        Some(v) => {
            consumed.insert("color".to_string());
            let mut color = [1.0, 1.0, 1.0, 1.0];
            for (slot, component) in color.iter_mut().zip(v.iter()) {
                *slot = *component;
            }
            color
        }
        None => [1.0, 1.0, 1.0, 1.0],
    };
    // Text never reads `size` as a number (`scene.rs`'s `parse_text_layer`
    // only checks `contains_key` and otherwise ignores the value) — `size`
    // is deliberately left UNCONSUMED here so its actual authored value
    // (which the renderer discards but decision (b) still wants captured
    // as authored state) survives into the unknown bag; `has_size` records
    // only the presence, matching what the renderer itself acts on.
    let has_size = object.contains_key("size");
    ObjectKindIr::Text(TextIr {
        text,
        font,
        pointsize,
        horizontal_align,
        vertical_align,
        color,
        has_size,
    })
}

/// Parses the `"particle"` object's known keys into a [`ParticleIr`], and
/// returns SEPARATELY the sub-keys of that definition nothing here reads
/// (deliberately including `speed`/`speedMin`/`speedMax` — module doc
/// departure (2)) as their own small JSON object. The caller (`parse_kind_ir`)
/// attaches that residue to the OBJECT's unknown bag under the single key
/// `"particle"` when it is non-empty, so a reader finds exactly the
/// un-typed particle sub-keys there — never the whole (mostly-typed)
/// particle object again, and never silently dropped either.
fn parse_particle_ir(
    object: &Map<String, Value>,
    consumed: &mut BTreeSet<String>,
) -> (ParticleIr, Map<String, Value>) {
    let (
        instance_count,
        instance_rate,
        instance_size,
        instance_lifetime,
        instance_speed,
        instance_alpha,
        instance_colorn,
    ) = parse_object_instance_override(object, consumed);
    let definition = object
        .get("particle")
        .map(scene_property_value)
        .and_then(Value::as_object);
    let Some(definition) = definition else {
        // classify_scene_object only reaches Particle for an object shape;
        // defensive fallback to an empty definition (all defaults except
        // the instance overrides already read above, no residue).
        return (
            ParticleIr {
                instance_count,
                instance_rate,
                instance_size,
                instance_lifetime,
                instance_speed,
                instance_alpha,
                instance_colorn,
                ..default_particle_ir()
            },
            Map::new(),
        );
    };

    // `speed`/`speedMin`/`speedMax` are DELIBERATELY not read here — see
    // the module doc's departure (2). They are left for `definition`'s own
    // leftover keys to carry into the particle's contribution to the
    // object's unknown bag, via the general "definition minus what this
    // function names" pass below.
    let mut inner_consumed: BTreeSet<String> = BTreeSet::new();

    let scalar = |inner: &mut BTreeSet<String>, key: &str, default: f32| -> f32 {
        match definition
            .get(key)
            .and_then(|value| as_number(scene_property_value(value)))
            .filter(|v| v.is_finite())
        {
            Some(v) => {
                inner.insert(key.to_string());
                v as f32
            }
            None => default,
        }
    };

    let spawn_rate = scalar(&mut inner_consumed, "spawnRate", 10.0);
    let life = scalar(&mut inner_consumed, "life", 1.0);
    let direction = scalar(&mut inner_consumed, "direction", 0.0);
    let spread = scalar(&mut inner_consumed, "spread", 0.0);
    let size_start = scalar(&mut inner_consumed, "sizeStart", 8.0);
    let size_end = scalar(&mut inner_consumed, "sizeEnd", 8.0);
    let alpha_start = scalar(&mut inner_consumed, "alphaStart", 1.0);
    let alpha_end = scalar(&mut inner_consumed, "alphaEnd", 0.0);

    let gravity = match definition
        .get("gravity")
        .and_then(|value| as_vector(scene_property_value(value), &[1, 2, 3]))
    {
        Some(v) => {
            inner_consumed.insert("gravity".to_string());
            if v.len() == 1 {
                [0.0, v[0]]
            } else {
                [v[0], v[1]]
            }
        }
        None => [0.0, 0.0],
    };

    let color_start = match definition.get("colorStart").and_then(parse_particle_color) {
        Some(color) => {
            inner_consumed.insert("colorStart".to_string());
            color
        }
        None => [1.0, 1.0, 1.0, 1.0],
    };
    let color_end = match definition.get("colorEnd").and_then(parse_particle_color) {
        Some(color) => {
            inner_consumed.insert("colorEnd".to_string());
            color
        }
        None => [1.0, 1.0, 1.0, 1.0],
    };

    let max_count = match definition
        .get("maxCount")
        .and_then(|value| as_number(scene_property_value(value)))
        .filter(|v| v.is_finite() && *v >= 0.0)
    {
        Some(v) => {
            inner_consumed.insert("maxCount".to_string());
            v as u32
        }
        None => 1000,
    };

    let material = match alias_winner(definition, "texture", "material")
        .and_then(|(value, key)| scene_property_value(value).as_str().map(|s| (s, key)))
    {
        Some((material, key)) => {
            inner_consumed.insert(key.to_string());
            Some(material.to_string())
        }
        None => None,
    };

    // `instanceoverride` is NOT a key of `definition` — it is a SIBLING of
    // `"particle"` on the OUTER object (`scene.rs`'s `parse_particle_system`
    // reads `object.get("instanceoverride")`, not the particle definition's
    // own key), already read into `instance_count`..`instance_colorn` above
    // via `parse_object_instance_override(object, ..)` before `definition`
    // was even extracted. An earlier version of this function mistakenly
    // read it from `definition` instead, which meant it was NEVER found in
    // any real scene (SR-2c differential testing caught this: `object.rs`
    // always nests `instanceoverride` beside `particle`, never inside it).

    // Every definition key this function did not read — including
    // speed/speedMin/speedMax and any WE component-model keys
    // (emitter/initializer/operator/renderer/...) — is the residue.
    let residue: Map<String, Value> = definition
        .iter()
        .filter(|(key, _)| !inner_consumed.contains(*key))
        .map(|(key, value)| (key.clone(), value.clone()))
        .collect();

    (
        ParticleIr {
            spawn_rate,
            life,
            direction,
            spread,
            size_start,
            size_end,
            alpha_start,
            alpha_end,
            gravity,
            color_start,
            color_end,
            max_count,
            material,
            instance_count,
            instance_rate,
            instance_size,
            instance_lifetime,
            instance_speed,
            instance_alpha,
            instance_colorn,
        },
        residue,
    )
}

fn default_particle_ir() -> ParticleIr {
    ParticleIr {
        spawn_rate: 10.0,
        life: 1.0,
        direction: 0.0,
        spread: 0.0,
        size_start: 8.0,
        size_end: 8.0,
        alpha_start: 1.0,
        alpha_end: 0.0,
        gravity: [0.0, 0.0],
        color_start: [1.0, 1.0, 1.0, 1.0],
        color_end: [1.0, 1.0, 1.0, 1.0],
        max_count: 1000,
        material: None,
        instance_count: 1.0,
        instance_rate: 1.0,
        instance_size: 1.0,
        instance_lifetime: 1.0,
        instance_speed: 1.0,
        instance_alpha: 1.0,
        instance_colorn: 1.0,
    }
}

fn parse_particle_color(value: &Value) -> Option<[f32; 4]> {
    let components = as_vector(scene_property_value(value), &[3, 4])?;
    let mut color = [1.0, 1.0, 1.0, 1.0];
    for (slot, component) in color.iter_mut().zip(components.iter()) {
        *slot = *component;
    }
    Some(color)
}

#[allow(clippy::type_complexity)]
fn parse_instance_override(overrides: &Map<String, Value>) -> (f32, f32, f32, f32, f32, f32, f32) {
    let factor = |name: &str| -> f32 {
        overrides
            .get(name)
            .map(scene_property_value)
            .and_then(as_number)
            .filter(|v| v.is_finite())
            .map(|v| v as f32)
            .unwrap_or(1.0)
    };
    let count = factor("count");
    let rate = factor("rate");
    let size = factor("size");
    let lifetime = factor("lifetime");
    let speed = factor("speed");
    let alpha = factor("alpha");
    let colorn = match overrides.get("colorn").or_else(|| overrides.get("color")) {
        Some(value) => as_vector(scene_property_value(value), &[3])
            .map(|v| (v[0] + v[1] + v[2]) / 3.0)
            .unwrap_or(1.0),
        None => 1.0,
    };
    (count, rate, size, lifetime, speed, alpha, colorn)
}

/// `instanceoverride` as authored on the OBJECT itself (a sibling of
/// `"particle"`, read the same way whether `particle` is a string, an
/// object, or anything else — see `ParticleFileIr`'s doc comment).
/// Consumes the top-level `"instanceoverride"` key on success, matching
/// every other object-typed field this module reads fully.
#[allow(clippy::type_complexity)]
fn parse_object_instance_override(
    object: &Map<String, Value>,
    consumed: &mut BTreeSet<String>,
) -> (f32, f32, f32, f32, f32, f32, f32) {
    match object
        .get("instanceoverride")
        .and_then(|value| scene_property_value(value).as_object())
    {
        Some(overrides) => {
            consumed.insert("instanceoverride".to_string());
            parse_instance_override(overrides)
        }
        None => (1.0, 1.0, 1.0, 1.0, 1.0, 1.0, 1.0),
    }
}

// ---------------------------------------------------------------------------
// Round-trip serialization
// ---------------------------------------------------------------------------
//
// `to_raw_value` reproduces a semantically-equal `Value` — re-parsing it
// with `parse_scene_ir` must yield an EQUAL `SceneIr`, per the SR-2
// acceptance "unknown fields survive IR load/report round-trip". It is
// NOT a byte-identical reproduction of the original scene.json: a typed
// field with its DEFAULT value is always re-emitted explicitly (even when
// the original omitted the key entirely) rather than tracking "was this
// authored or defaulted" separately — re-parsing an explicit default
// yields the same typed default, so this is lossless for `SceneIr`
// equality even though the intermediate JSON differs from the original
// bytes. A shadowed alias's LOSING spelling and any other unknown-bag
// entry are emitted verbatim under their original key.

impl SceneIr {
    pub fn to_raw_value(&self) -> Value {
        let mut root = Map::new();
        root.insert("general".to_string(), self.general.to_raw_value());
        root.insert(
            "objects".to_string(),
            Value::Array(self.objects.iter().map(ObjectIr::to_raw_value).collect()),
        );
        for (key, value) in &self.unknown.0 {
            root.insert(key.clone(), value.clone());
        }
        Value::Object(root)
    }
}

impl GeneralIr {
    fn to_raw_value(&self) -> Value {
        let mut map = Map::new();
        map.insert("clearcolor".to_string(), json!(self.clear_color));
        if let Some((width, height)) = self.resolution {
            map.insert("resolution".to_string(), json!([width, height]));
        }
        if let Some(fps) = self.fps {
            map.insert("fps".to_string(), json!(fps));
        }
        if let Some(script) = &self.script {
            map.insert("script".to_string(), Value::String(script.clone()));
        }
        for (key, value) in &self.unknown.0 {
            map.insert(key.clone(), value.clone());
        }
        Value::Object(map)
    }
}

fn merge_size_tint(map: &mut Map<String, Value>, size: [f32; 2], tint: [f32; 4]) {
    map.insert("size".to_string(), json!(size));
    map.insert("tint".to_string(), json!(tint));
}

fn horizontal_align_word(align: HorizontalAlignIr) -> &'static str {
    match align {
        HorizontalAlignIr::Left => "left",
        HorizontalAlignIr::Center => "center",
        HorizontalAlignIr::Right => "right",
    }
}

fn vertical_align_word(align: VerticalAlignIr) -> &'static str {
    match align {
        VerticalAlignIr::Top => "top",
        VerticalAlignIr::Center => "center",
        VerticalAlignIr::Bottom => "bottom",
    }
}

impl ObjectKindIr {
    /// Inserts this kind's own typed keys into `map` (an object's
    /// in-progress raw form). The `Particle` arm writes a fresh
    /// `"particle"` object from ITS typed fields only — `ObjectIr::
    /// to_raw_value` merges that variant's residue (this object's own
    /// unknown-bag `"particle"` entry, if any) into the SAME object
    /// afterward, rather than this method reading `ObjectIr::unknown`
    /// itself (kept a one-way dependency: kind -> map, not kind ->
    /// object).
    fn merge_into(&self, map: &mut Map<String, Value>) {
        match self {
            ObjectKindIr::Model(inner) => {
                map.insert("image".to_string(), Value::String(inner.model_ref.clone()));
                merge_size_tint(map, inner.size, inner.tint);
            }
            ObjectKindIr::Image(inner) | ObjectKindIr::TexvImage(inner) => {
                map.insert("image".to_string(), Value::String(inner.image.clone()));
                merge_size_tint(map, inner.size, inner.tint);
            }
            ObjectKindIr::TexturelessImage(inner) => {
                // No typed "image" here by construction (module doc
                // departure (1)) — the object's own unknown bag already
                // carries the original non-string `image` value; that
                // merge happens in `ObjectIr::to_raw_value`, not here.
                merge_size_tint(map, inner.size, inner.tint);
            }
            ObjectKindIr::Video(inner) => {
                if let Some(source) = &inner.source {
                    map.insert("video".to_string(), Value::String(source.clone()));
                }
                merge_size_tint(map, inner.size, inner.tint);
                map.insert("loop".to_string(), Value::Bool(inner.loop_playback));
                map.insert("rate".to_string(), json!(inner.rate));
            }
            ObjectKindIr::Text(inner) => {
                map.insert("text".to_string(), Value::String(inner.text.clone()));
                if let Some(font) = &inner.font {
                    map.insert("font".to_string(), Value::String(font.clone()));
                }
                map.insert("pointsize".to_string(), json!(inner.pointsize));
                map.insert(
                    "horizontalalign".to_string(),
                    Value::String(horizontal_align_word(inner.horizontal_align).to_string()),
                );
                map.insert(
                    "verticalalign".to_string(),
                    Value::String(vertical_align_word(inner.vertical_align).to_string()),
                );
                map.insert("color".to_string(), json!(inner.color));
                // `has_size`'s real `size` value (when true) lives in the
                // object's unknown bag (parse_text_ir never consumes
                // `size`) and is merged there, not here.
            }
            ObjectKindIr::Particle(inner) => {
                let mut definition = Map::new();
                definition.insert("spawnRate".to_string(), json!(inner.spawn_rate));
                definition.insert("life".to_string(), json!(inner.life));
                definition.insert("direction".to_string(), json!(inner.direction));
                definition.insert("spread".to_string(), json!(inner.spread));
                definition.insert("sizeStart".to_string(), json!(inner.size_start));
                definition.insert("sizeEnd".to_string(), json!(inner.size_end));
                definition.insert("alphaStart".to_string(), json!(inner.alpha_start));
                definition.insert("alphaEnd".to_string(), json!(inner.alpha_end));
                definition.insert("gravity".to_string(), json!(inner.gravity));
                definition.insert("colorStart".to_string(), json!(inner.color_start));
                definition.insert("colorEnd".to_string(), json!(inner.color_end));
                definition.insert("maxCount".to_string(), json!(inner.max_count));
                if let Some(material) = &inner.material {
                    definition.insert("texture".to_string(), Value::String(material.clone()));
                }
                map.insert("particle".to_string(), Value::Object(definition));
                // `instanceoverride` is a SIBLING of `"particle"` on the
                // OBJECT (see `ParticleFileIr`'s doc comment) — inserted
                // into `map`, not `definition`, matching where it was read.
                map.insert(
                    "instanceoverride".to_string(),
                    instance_override_to_raw_value(
                        inner.instance_count,
                        inner.instance_rate,
                        inner.instance_size,
                        inner.instance_lifetime,
                        inner.instance_speed,
                        inner.instance_alpha,
                        inner.instance_colorn,
                    ),
                );
            }
            ObjectKindIr::ParticleFile(inner) => {
                if let Some(file_ref) = &inner.file_ref {
                    map.insert("particle".to_string(), Value::String(file_ref.clone()));
                }
                map.insert(
                    "instanceoverride".to_string(),
                    instance_override_to_raw_value(
                        inner.instance_count,
                        inner.instance_rate,
                        inner.instance_size,
                        inner.instance_lifetime,
                        inner.instance_speed,
                        inner.instance_alpha,
                        inner.instance_colorn,
                    ),
                );
            }
            ObjectKindIr::Unknown => {}
        }
    }
}

/// The inverse of `parse_instance_override`'s reduction — re-emits the
/// 7 typed `instance_*` fields as the `instanceoverride` object WE's own
/// schema expects. `colorn` is re-emitted as a 3-vector (mirroring the
/// authored shape `parse_instance_override`'s `as_vector(.., &[3])` call
/// requires), not the reduced scalar `instance_colorn` itself holds — a
/// bare number would fail that shape check on re-parse and silently reset
/// to the 1.0 default.
#[allow(clippy::too_many_arguments)]
fn instance_override_to_raw_value(
    count: f32,
    rate: f32,
    size: f32,
    lifetime: f32,
    speed: f32,
    alpha: f32,
    colorn: f32,
) -> Value {
    let mut overrides = Map::new();
    overrides.insert("count".to_string(), json!(count));
    overrides.insert("rate".to_string(), json!(rate));
    overrides.insert("size".to_string(), json!(size));
    overrides.insert("lifetime".to_string(), json!(lifetime));
    overrides.insert("speed".to_string(), json!(speed));
    overrides.insert("alpha".to_string(), json!(alpha));
    overrides.insert("colorn".to_string(), json!([colorn, colorn, colorn]));
    Value::Object(overrides)
}

impl EffectRefIr {
    /// Exactly `self.raw` — the typed fields (`id`/`name`/`visible`) each
    /// carry a default indistinguishable from an authored value once
    /// typed (see `raw`'s own doc comment), so reconstructing from them
    /// would silently materialize keys the original entry never had.
    /// Returning `raw` directly is both simpler and exact.
    fn to_raw_value(&self) -> Value {
        self.raw.clone()
    }
}

impl ObjectIr {
    fn to_raw_value(&self) -> Value {
        let mut map = Map::new();
        if let Some(name) = &self.name {
            map.insert("name".to_string(), Value::String(name.clone()));
        }
        if let Some(id) = self.authored_id {
            map.insert("id".to_string(), json!(id));
        }
        map.insert("origin".to_string(), json!(self.common.origin));
        map.insert("angles".to_string(), json!(self.common.angles));
        map.insert("scale".to_string(), json!(self.common.scale));
        map.insert("alpha".to_string(), json!(self.common.alpha));
        match &self.common.visible {
            VisibleIr::Bool(value) => {
                map.insert("visible".to_string(), Value::Bool(*value));
            }
            VisibleIr::PropertyBound(value) => {
                map.insert("visible".to_string(), value.clone());
            }
            VisibleIr::Absent => {}
        }
        map.insert("blendMode".to_string(), json!(self.common.blend_mode));
        map.insert("brightness".to_string(), json!(self.common.brightness));

        self.kind.merge_into(&mut map);

        if !self.effects.is_empty() {
            map.insert(
                "effects".to_string(),
                Value::Array(self.effects.iter().map(EffectRefIr::to_raw_value).collect()),
            );
        }

        // The Particle kind already wrote a typed "particle" object above;
        // when this object's own unknown bag ALSO carries a "particle"
        // entry (the residue `parse_particle_ir` returned — see its doc
        // comment), merge that residue's sub-keys into the SAME object
        // rather than overwriting it wholesale.
        for (key, value) in &self.unknown.0 {
            if key == "particle"
                && let Some(Value::Object(existing)) = map.get_mut("particle")
                && let Value::Object(residue) = value
            {
                existing.extend(residue.clone());
                continue;
            }
            map.insert(key.clone(), value.clone());
        }

        Value::Object(map)
    }
}

#[cfg(test)]
mod tests;
