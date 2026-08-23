// SPDX-License-Identifier: GPL-3.0-or-later
//! Effect / FBO chain parsing (S3), shared by preflight and the scene
//! worker the same way `scenemodel` is: this module performs no file I/O
//! of its own (a caller-supplied `AssetLookup` closure does every read),
//! and every unresolved reference degrades the *whole object-effect
//! entry* rather than the object itself — see [`resolve_object_effects`]'s
//! doc comment for the exact honesty rule this slice adds.
//!
//! Borrowed-From: Almamu/linux-wallpaperengine (GPL-3.0-or-later)
//! src/WallpaperEngine/Data/Parsers/EffectParser.cpp:19-111 (effect `.json`
//! schema: `fbos`/`passes`, material vs `command` pass shape),
//! src/WallpaperEngine/Data/Parsers/ObjectParser.cpp:206-248
//! (`parseEffect` — an object's `effects[]` entry: `id`/`name`/`visible`/
//! `file`/`passes[]` pass-override list),
//! src/WallpaperEngine/Render/Objects/Effects/CPass.cpp:697-819
//! (`setupTextureUniforms` — the texture-slot resolution priority order:
//! material's own `textures` < `usertextures` < override `textures` <
//! override `usertextures` < `binds`, highest priority last) @ b016d7d1 —
//! adapted (this module resolves references to bytes/sentinels; it does
//! not touch a GPU).
//!
//! ## Scope and the honesty rule (deliverable 1)
//!
//! Per the task brief: "an effect failure degrades to drawing the object
//! without that effect" — a model/image object with unresolvable or
//! malformed `effects[]` entries stays exactly as drawable as it would be
//! with no `effects` field at all (`scenemodel::resolve_model`'s honesty
//! gate is unaffected by anything in this module). Concretely,
//! [`resolve_object_effects`] never returns an `Err`: a malformed or
//! unresolvable effect entry is simply omitted from the returned list,
//! never counted as a reason an object can't draw
//! (`sceneobjects::SceneObjectSummary::unsupported_reasons` gets no new
//! entries from this module — see that module's own doc comment).
//!
//! ## `_rt_`/`_alias_` runtime target names
//!
//! A texture slot name starting with `_rt_` or `_alias_` never names a
//! `materials/<name>.tex` asset on disk — it names a live render target
//! (an effect's own declared FBO, or the scene-wide
//! `_rt_FullFrameBuffer`) that only exists once the renderer executes the
//! effect chain. [`is_runtime_target_name`] is the single predicate both
//! this module and `scenemodel::resolve_model` use to decide "this name
//! is resolved by the GPU renderer at draw time, not by a filesystem
//! lookup now" — the fix that lets a scene whose base material samples
//! `_rt_FullFrameBuffer` (e.g. the Workshop scene this slice targets)
//! pass the B2 honesty gate instead of failing slot-0 resolution against
//! a `materials/_rt_FullFrameBuffer.tex` path that can never exist.

use std::collections::BTreeMap;

use serde_json::Value;

use crate::scenemodel::{AssetLookup, MAX_MATERIAL_TEXTURES, texture_asset_path};
use crate::texvheader;

/// Cap on one effect `.json` file (deliverable 1: "JSON ≤ 1 MiB").
pub const MAX_EFFECT_JSON_BYTES: u64 = 1024 * 1024;

/// Cap on `effects[]` entries a single scene object may declare
/// (deliverable 1: "≤ 32 effects/object"). Entries beyond this are
/// silently ignored, not rejected — bounding preflight/worker cost is the
/// goal, not policing scene authorship.
pub const MAX_EFFECTS_PER_OBJECT: usize = 32;

/// Cap on `passes[]` entries inside one effect file (deliverable 1: "≤ 16
/// passes/effect").
pub const MAX_PASSES_PER_EFFECT: usize = 16;

/// Cap on `fbos[]` entries inside one effect file (deliverable 1: "≤ 16
/// fbos/effect").
pub const MAX_FBOS_PER_EFFECT: usize = 16;

/// The well-known scene-wide render target: a copy of the composited
/// scene taken before an object's post-process effect passes run (see
/// `kwe-scene-renderer::vulkan` for the exact frame-ordering contract this
/// slice implements — a documented, bounded simplification of upstream's
/// strictly paint-order-dependent visibility).
///
/// Borrowed-From: Almamu/linux-wallpaperengine (GPL-3.0-or-later)
/// src/WallpaperEngine/Render/CWallpaper.cpp:312-323 (`setupFramebuffers`
/// creates `_rt_FullFrameBuffer` as the wallpaper's own main composite
/// target) @ b016d7d1 — adapted (name only; our renderer takes one
/// whole-frame snapshot per frame rather than tracking incremental
/// per-object paint order).
pub const FULL_FRAME_BUFFER: &str = "_rt_FullFrameBuffer";

/// Sentinel bind name meaning "reuse whatever texture the previous stage
/// in this pass's own texture-slot resolution would have used" (upstream
/// `m_previousInput`, `CPass.cpp:209-244`). This module records the
/// sentinel; the renderer decides what "previous" means for slot N of a
/// given pass at draw time.
pub const PREVIOUS_INPUT: &str = "previous";

/// True if `name` refers to a live render target rather than a
/// `materials/<name>.tex` asset on disk — see the module doc comment.
pub fn is_runtime_target_name(name: &str) -> bool {
    name.starts_with("_rt_") || name.starts_with("_alias_")
}

/// One `fbos[]` entry: `EffectParser.cpp:97-111`.
#[derive(Debug, Clone)]
pub struct FboSpec {
    pub name: String,
    /// Defaults to `"rgba8888"`, matching upstream's default and this
    /// renderer's only implemented attachment format (RGBA8).
    pub format: String,
    /// FBO texture dimensions = the owning object's own size / `scale`
    /// (upstream `CImage.cpp:652-654`'s `FBOProvider::create(..,
    /// this->getSize())` — sized relative to the OBJECT, not the scene).
    pub scale: f32,
    /// Parsed and recorded; upstream's own `unique` flag has no observed
    /// distinct-instance behavior in the reviewed revision (every FBO is
    /// already one instance per object-effect application) — kept for
    /// schema fidelity and forward compatibility, not currently read by
    /// the renderer.
    pub unique: bool,
}

/// One resolved effect-pass texture slot.
#[derive(Debug, Clone)]
pub enum EffectTextureSlot {
    /// A real, decodable `.tex` asset, resolved the same way
    /// `scenemodel::TextureSlot` is (bytes present, TEXV header checked
    /// when applicable).
    Texture {
        name: String,
        texture_ref: String,
        bytes: Vec<u8>,
    },
    /// A `_rt_`/`_alias_` name: resolved to a live FBO/`_rt_FullFrameBuffer`
    /// view at render time, not to bytes now.
    RenderTarget(String),
    /// The `"previous"` bind sentinel.
    Previous,
}

/// One material pass inside an effect chain.
#[derive(Debug, Clone)]
pub struct EffectMaterialPass {
    /// The pass's own `material` reference (a `materials/*.json` path,
    /// resolved exactly like `scenemodel::resolve_model`'s material
    /// step).
    pub material_ref: String,
    pub shader: Option<String>,
    pub blending: Option<String>,
    pub combos: serde_json::Map<String, Value>,
    pub constant_shader_values: serde_json::Map<String, Value>,
    /// Positional texture slots (index == `g_Texture<N>`), `None` for an
    /// empty slot — same positional contract as
    /// `scenemodel::ResolvedModel::texture_slots`.
    pub texture_slots: Vec<Option<EffectTextureSlot>>,
    /// `None` = this pass draws directly onto the compositor (a
    /// post-process layer, upstream's "no target" case); `Some(name)` =
    /// this pass renders into the named FBO.
    pub target: Option<String>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum EffectCommand {
    /// A full-screen copy of `source` into `target` (upstream: the only
    /// command actually executed, `CImage.cpp:680-700`, synthesized as a
    /// passthrough material draw upstream — this renderer instead uses a
    /// direct image copy since both are Vulkan-native render targets of
    /// identical format/size; see `vulkan.rs`).
    Copy,
    /// A pointer swap of two FBOs' underlying render targets. Upstream
    /// parses `swap` into its command enum but never executes it
    /// (`CImage.cpp:680-683` only handles `Command_Copy`) — this is our
    /// own completion of the documented-but-unimplemented semantic, not
    /// a port of executed upstream behavior.
    Swap,
}

#[derive(Debug, Clone)]
pub struct EffectCommandPass {
    pub command: EffectCommand,
    pub source: String,
    pub target: String,
}

#[derive(Debug, Clone)]
pub enum EffectPass {
    Material(EffectMaterialPass),
    Command(EffectCommandPass),
}

#[derive(Debug, Clone)]
pub struct EffectSpec {
    pub name: String,
    pub fbos: Vec<FboSpec>,
    pub passes: Vec<EffectPass>,
}

/// One resolved `effects[]` entry on a scene object.
#[derive(Debug, Clone)]
pub struct ObjectEffect {
    pub id: i64,
    pub name: String,
    pub visible: bool,
    pub effect: EffectSpec,
}

fn bounded_json(bytes: Vec<u8>, what: &str) -> Option<Value> {
    if bytes.len() as u64 > MAX_EFFECT_JSON_BYTES {
        return None;
    }
    serde_json::from_slice(&bytes).ok().filter(|value: &Value| {
        let _ = what;
        value.is_object()
    })
}

/// Resolve one texture-slot name to an [`EffectTextureSlot`]. `None` means
/// the name could not be resolved at all (a real asset reference that
/// does not exist) — the caller treats this as a fatal parse failure for
/// the whole effect (see module doc comment).
fn resolve_texture_slot_name(
    name: &str,
    lookup: &mut AssetLookup<'_>,
) -> Option<EffectTextureSlot> {
    if name == PREVIOUS_INPUT {
        return Some(EffectTextureSlot::Previous);
    }
    if is_runtime_target_name(name) {
        return Some(EffectTextureSlot::RenderTarget(name.to_string()));
    }
    let texture_ref = texture_asset_path(name);
    let bytes = lookup(&texture_ref)?;
    if texvheader::is_texv(&bytes) {
        let real_bytes = texvheader::check_header(&bytes).ok()?;
        if real_bytes > texvheader::MAX_SINGLE_TEXTURE_BUDGET_BYTES {
            return None;
        }
    }
    Some(EffectTextureSlot::Texture {
        name: name.to_string(),
        texture_ref,
        bytes,
    })
}

/// Parse an index -> name map from any of THREE JSON shapes the real
/// corpus uses (verified against Workshop scene 1652229298's godrays
/// effect chain): an object keyed by the string index
/// (`material.json`-style `textures`/`usertextures` overrides), an array
/// of `{"index":N,"name":S}` objects (an effect file's own `bind` list,
/// `EffectParser.cpp`'s `bind` array — e.g.
/// `effects/godrays/effect.json`'s `"bind":[{"name":"previous","index":0}]`),
/// or a plain POSITIONAL array of string/null/`{"name":..}` entries — the
/// same shape `material.json`'s own `textures` field uses
/// (`scenemodel::positional_texture_names`) — confirmed as a real
/// object-level pass-override shape: scene 1652229298's godrays object
/// overrides pass 0 with `"textures":[null,"screenmask","util/clouds_256"]`,
/// a positional array, not an index-keyed object. The two array shapes
/// are told apart by whether any element is itself an object carrying an
/// `"index"` key (bind-shaped); otherwise the array is positional.
/// Bounded to `MAX_MATERIAL_TEXTURES` entries; a malformed individual
/// entry is skipped, not fatal (this is override/bind metadata layered on
/// top of an already-resolved base material).
fn parse_index_name_entries(value: Option<&Value>) -> BTreeMap<u32, String> {
    let mut out = BTreeMap::new();
    let Some(value) = value else { return out };
    let name_from_entry = |entry: &Value| -> Option<String> {
        match entry {
            Value::String(s) if !s.is_empty() => Some(s.clone()),
            Value::Object(object) => object
                .get("name")
                .and_then(Value::as_str)
                .filter(|s| !s.is_empty())
                .map(str::to_owned),
            _ => None,
        }
    };
    match value {
        Value::Object(map) => {
            for (key, entry) in map.iter().take(MAX_MATERIAL_TEXTURES * 4) {
                let Ok(index) = key.parse::<u32>() else {
                    continue;
                };
                if index as usize >= MAX_MATERIAL_TEXTURES {
                    continue;
                }
                if let Some(name) = name_from_entry(entry) {
                    out.insert(index, name);
                }
            }
        }
        Value::Array(array) => {
            let bind_shaped = array
                .iter()
                .any(|entry| entry.as_object().is_some_and(|o| o.contains_key("index")));
            if bind_shaped {
                for entry in array.iter().take(MAX_MATERIAL_TEXTURES * 4) {
                    let Some(object) = entry.as_object() else {
                        continue;
                    };
                    let Some(index) = object.get("index").and_then(Value::as_u64) else {
                        continue;
                    };
                    if index as usize >= MAX_MATERIAL_TEXTURES {
                        continue;
                    }
                    if let Some(name) = object
                        .get("name")
                        .and_then(Value::as_str)
                        .filter(|s| !s.is_empty())
                    {
                        out.insert(index as u32, name.to_string());
                    }
                }
            } else {
                for (index, entry) in array.iter().take(MAX_MATERIAL_TEXTURES).enumerate() {
                    if let Some(name) = name_from_entry(entry) {
                        out.insert(index as u32, name);
                    }
                }
            }
        }
        _ => {}
    }
    out
}

/// One object-level pass override (`ObjectParser::parseEffect`'s
/// `passes[]` entries).
struct PassOverride {
    combos: serde_json::Map<String, Value>,
    constants: serde_json::Map<String, Value>,
    textures: BTreeMap<u32, String>,
    usertextures: BTreeMap<u32, String>,
}

fn parse_pass_override(value: &Value) -> PassOverride {
    let object = value.as_object();
    PassOverride {
        combos: object
            .and_then(|o| o.get("combos"))
            .and_then(Value::as_object)
            .cloned()
            .unwrap_or_default(),
        constants: object
            .and_then(|o| o.get("constantshadervalues"))
            .and_then(Value::as_object)
            .cloned()
            .unwrap_or_default(),
        textures: parse_index_name_entries(object.and_then(|o| o.get("textures"))),
        usertextures: parse_index_name_entries(object.and_then(|o| o.get("usertextures"))),
    }
}

/// Resolve one effect-file material pass, merging in its matching
/// object-level override (if any) and the pass's own `bind` list, per
/// upstream's priority order (binds win last,
/// `CPass.cpp::setupTextureUniforms`). Returns `None` on any hard
/// failure (missing/oversized/malformed material.json, an unresolvable
/// declared texture slot) — the caller drops the whole effect.
fn resolve_material_pass(
    pass_object: &serde_json::Map<String, Value>,
    override_: Option<&PassOverride>,
    lookup: &mut AssetLookup<'_>,
) -> Option<EffectMaterialPass> {
    let material_ref = pass_object.get("material").and_then(Value::as_str)?;
    let material_bytes = lookup(material_ref)?;
    let material_json = bounded_json(material_bytes, "effect material.json")?;
    let material_object = material_json.as_object()?;
    let passes = material_object
        .get("passes")
        .and_then(Value::as_array)
        .filter(|p| !p.is_empty())?;
    let pass0 = passes[0].as_object()?;

    let shader = pass0
        .get("shader")
        .and_then(Value::as_str)
        .map(str::to_owned);
    let blending = pass0
        .get("blending")
        .and_then(Value::as_str)
        .map(str::to_owned);
    let mut combos = pass0
        .get("combos")
        .and_then(Value::as_object)
        .cloned()
        .unwrap_or_default();
    let mut constant_shader_values = pass0
        .get("constantshadervalues")
        .and_then(Value::as_object)
        .cloned()
        .unwrap_or_default();

    // Base texture names from the material itself, positional.
    let mut names: BTreeMap<u32, String> = BTreeMap::new();
    if let Some(array) = pass0.get("textures").and_then(Value::as_array) {
        for (index, entry) in array.iter().take(MAX_MATERIAL_TEXTURES).enumerate() {
            let name = match entry {
                Value::String(s) if !s.is_empty() => Some(s.clone()),
                Value::Object(object) => object
                    .get("name")
                    .and_then(Value::as_str)
                    .filter(|s| !s.is_empty())
                    .map(str::to_owned),
                _ => None,
            };
            if let Some(name) = name {
                names.insert(index as u32, name);
            }
        }
    }

    // Layer overrides in upstream priority: usertextures < textures for
    // the base material's own map is already merged above; the
    // object-level override's textures/usertextures come next, then the
    // effect file's own `bind` list for this pass (resolved by the
    // caller and passed as `override_`'s bind map is NOT here — binds
    // are per-PASS in the effect file, handled by the caller merging
    // them in after this returns). Apply override_ (usertextures first,
    // then textures — matches CPass.cpp's stated order: textures <
    // usertextures).
    if let Some(over) = override_ {
        combos.extend(over.combos.clone());
        constant_shader_values.extend(over.constants.clone());
        for (index, name) in &over.usertextures {
            names.insert(*index, name.clone());
        }
        for (index, name) in &over.textures {
            names.insert(*index, name.clone());
        }
    }

    let bind = parse_index_name_entries(pass_object.get("bind"));
    for (index, name) in bind {
        names.insert(index, name);
    }

    let mut texture_slots: Vec<Option<EffectTextureSlot>> =
        Vec::with_capacity(MAX_MATERIAL_TEXTURES);
    for index in 0..MAX_MATERIAL_TEXTURES as u32 {
        let Some(name) = names.get(&index) else {
            texture_slots.push(None);
            continue;
        };
        let slot = resolve_texture_slot_name(name, lookup)?;
        texture_slots.push(Some(slot));
    }

    let target = pass_object
        .get("target")
        .and_then(Value::as_str)
        .map(str::to_owned);

    Some(EffectMaterialPass {
        material_ref: material_ref.to_string(),
        shader,
        blending,
        combos,
        constant_shader_values,
        texture_slots,
        target,
    })
}

fn resolve_command_pass(pass_object: &serde_json::Map<String, Value>) -> Option<EffectCommandPass> {
    let command = match pass_object.get("command").and_then(Value::as_str)? {
        "copy" => EffectCommand::Copy,
        "swap" => EffectCommand::Swap,
        _ => return None,
    };
    let source = pass_object
        .get("source")
        .and_then(Value::as_str)?
        .to_string();
    let target = pass_object
        .get("target")
        .and_then(Value::as_str)?
        .to_string();
    Some(EffectCommandPass {
        command,
        source,
        target,
    })
}

/// Parse one effect `.json` file's `fbos`/`passes` plus the object-level
/// pass overrides that apply to it (index-aligned 1:1 with this file's
/// own MATERIAL passes only — command passes consume no override,
/// matching `CImage.cpp:658-717`). Returns `None` on any hard failure;
/// the caller drops the whole `effects[]` entry (module doc comment).
fn parse_effect_spec(
    effect_object: &serde_json::Map<String, Value>,
    pass_overrides: &[Value],
    lookup: &mut AssetLookup<'_>,
) -> Option<EffectSpec> {
    let name = effect_object
        .get("name")
        .and_then(Value::as_str)
        .unwrap_or("")
        .to_string();

    let mut fbos = Vec::new();
    if let Some(array) = effect_object.get("fbos").and_then(Value::as_array) {
        for entry in array.iter().take(MAX_FBOS_PER_EFFECT) {
            let Some(object) = entry.as_object() else {
                continue;
            };
            let Some(fbo_name) = object.get("name").and_then(Value::as_str) else {
                continue;
            };
            let format = object
                .get("format")
                .and_then(Value::as_str)
                .unwrap_or("rgba8888")
                .to_string();
            let scale = object
                .get("scale")
                .and_then(Value::as_f64)
                .map(|v| v as f32)
                .filter(|v| v.is_finite() && *v > 0.0)
                .unwrap_or(1.0);
            let unique = object
                .get("unique")
                .and_then(Value::as_bool)
                .unwrap_or(false);
            fbos.push(FboSpec {
                name: fbo_name.to_string(),
                format,
                scale,
                unique,
            });
        }
    }

    let raw_passes = effect_object.get("passes").and_then(Value::as_array)?;
    if raw_passes.is_empty() {
        return None;
    }
    let mut passes = Vec::new();
    let mut override_index = 0usize;
    for entry in raw_passes.iter().take(MAX_PASSES_PER_EFFECT) {
        let pass_object = entry.as_object()?;
        if pass_object.contains_key("command") {
            let command = resolve_command_pass(pass_object)?;
            passes.push(EffectPass::Command(command));
            continue;
        }
        let override_ = pass_overrides.get(override_index).map(parse_pass_override);
        override_index += 1;
        let material = resolve_material_pass(pass_object, override_.as_ref(), lookup)?;
        passes.push(EffectPass::Material(material));
    }

    Some(EffectSpec { name, fbos, passes })
}

/// Resolve every `effects[]` entry on one scene object, given the array
/// exactly as it appeared under the object's own `"effects"` key (the
/// caller — `kwe-scene-renderer::scene`'s parse step keeps this raw array
/// on `LayerSpec::effects_raw` since it needs the pkg/dir/assets-root
/// lookup this module's caller supplies, not available at scene.json
/// parse time). Never fails: a malformed or unresolvable entry is simply
/// omitted (module doc comment's honesty rule). Bounded to
/// `MAX_EFFECTS_PER_OBJECT` entries even before considering per-entry
/// validity.
pub fn resolve_object_effects(
    entries: &[Value],
    lookup: &mut AssetLookup<'_>,
) -> Vec<ObjectEffect> {
    let mut out = Vec::new();
    for entry in entries.iter().take(MAX_EFFECTS_PER_OBJECT) {
        let Some(effect_object) = entry.as_object() else {
            continue;
        };
        let visible = effect_object
            .get("visible")
            .and_then(Value::as_bool)
            .unwrap_or(true);
        let id = effect_object.get("id").and_then(Value::as_i64).unwrap_or(0);
        let name = effect_object
            .get("name")
            .and_then(Value::as_str)
            .unwrap_or("")
            .to_string();
        let Some(file_ref) = effect_object.get("file").and_then(Value::as_str) else {
            continue;
        };
        let Some(effect_bytes) = lookup(file_ref) else {
            continue;
        };
        let Some(effect_json) = bounded_json(effect_bytes, "effect .json") else {
            continue;
        };
        let Some(effect_root) = effect_json.as_object() else {
            continue;
        };
        let pass_overrides = effect_object
            .get("passes")
            .and_then(Value::as_array)
            .cloned()
            .unwrap_or_default();
        let Some(spec) = parse_effect_spec(effect_root, &pass_overrides, lookup) else {
            continue;
        };
        out.push(ObjectEffect {
            id,
            name,
            visible,
            effect: spec,
        });
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::HashMap;

    fn lookup_from(map: HashMap<&'static str, Vec<u8>>) -> impl FnMut(&str) -> Option<Vec<u8>> {
        move |reference: &str| map.get(reference).cloned()
    }

    fn material_json(shader: &str, textures: &[&str]) -> Vec<u8> {
        let texture_values: Vec<Value> = textures
            .iter()
            .map(|t| {
                if t.is_empty() {
                    Value::Null
                } else {
                    Value::String((*t).to_string())
                }
            })
            .collect();
        serde_json::to_vec(&serde_json::json!({
            "passes": [{ "shader": shader, "textures": texture_values }]
        }))
        .unwrap()
    }

    #[test]
    fn is_runtime_target_name_matches_rt_and_alias_prefixes() {
        assert!(is_runtime_target_name("_rt_FullFrameBuffer"));
        assert!(is_runtime_target_name("_alias_something"));
        assert!(!is_runtime_target_name("mytexture"));
        assert!(!is_runtime_target_name(""));
    }

    #[test]
    fn no_effects_field_resolves_to_empty_list() {
        let object = serde_json::json!({"name": "layer"});
        let mut lookup = lookup_from(HashMap::new());
        let entries = object
            .get("effects")
            .and_then(Value::as_array)
            .cloned()
            .unwrap_or_default();
        let out = resolve_object_effects(&entries, &mut lookup);
        assert!(out.is_empty());
    }

    #[test]
    fn simple_two_pass_effect_resolves_with_fbos_and_command() {
        let mut files: HashMap<&str, Vec<u8>> = HashMap::new();
        files.insert(
            "effects/blur.json",
            serde_json::to_vec(&serde_json::json!({
                "name": "blur",
                "fbos": [{"name": "_rt_Blur", "format": "rgba8888", "scale": 2.0}],
                "passes": [
                    {"material": "materials/blurpass.json", "target": "_rt_Blur"},
                    {"command": "copy", "source": "_rt_Blur", "target": "_rt_FullFrameBuffer"}
                ]
            }))
            .unwrap(),
        );
        files.insert(
            "materials/blurpass.json",
            material_json("blur", &["_rt_FullFrameBuffer"]),
        );
        let object = serde_json::json!({
            "effects": [{"id": 1, "name": "blur", "visible": true, "file": "effects/blur.json", "passes": [{}]}]
        });
        let mut lookup = lookup_from(files);
        let entries = object
            .get("effects")
            .and_then(Value::as_array)
            .cloned()
            .unwrap_or_default();
        let out = resolve_object_effects(&entries, &mut lookup);
        assert_eq!(out.len(), 1);
        let effect = &out[0].effect;
        assert_eq!(effect.fbos.len(), 1);
        assert_eq!(effect.fbos[0].name, "_rt_Blur");
        assert!((effect.fbos[0].scale - 2.0).abs() < f32::EPSILON);
        assert_eq!(effect.passes.len(), 2);
        match &effect.passes[0] {
            EffectPass::Material(m) => {
                assert_eq!(m.target.as_deref(), Some("_rt_Blur"));
                match &m.texture_slots[0] {
                    Some(EffectTextureSlot::RenderTarget(name)) => {
                        assert_eq!(name, "_rt_FullFrameBuffer")
                    }
                    other => panic!("unexpected slot: {other:?}"),
                }
            }
            other => panic!("expected material pass, got {other:?}"),
        }
        match &effect.passes[1] {
            EffectPass::Command(c) => {
                assert_eq!(c.command, EffectCommand::Copy);
                assert_eq!(c.source, "_rt_Blur");
                assert_eq!(c.target, "_rt_FullFrameBuffer");
            }
            other => panic!("expected command pass, got {other:?}"),
        }
    }

    #[test]
    fn unresolvable_effect_file_is_omitted_not_fatal() {
        let object = serde_json::json!({
            "effects": [{"id": 1, "file": "effects/missing.json"}]
        });
        let mut lookup = lookup_from(HashMap::new());
        let entries = object
            .get("effects")
            .and_then(Value::as_array)
            .cloned()
            .unwrap_or_default();
        let out = resolve_object_effects(&entries, &mut lookup);
        assert!(out.is_empty());
    }

    #[test]
    fn unresolvable_texture_slot_drops_the_whole_effect() {
        let mut files: HashMap<&str, Vec<u8>> = HashMap::new();
        files.insert(
            "effects/broken.json",
            serde_json::to_vec(&serde_json::json!({
                "name": "broken",
                "passes": [{"material": "materials/broken.json"}]
            }))
            .unwrap(),
        );
        files.insert(
            "materials/broken.json",
            material_json("x", &["missing_texture"]),
        );
        let object = serde_json::json!({
            "effects": [{"id": 1, "file": "effects/broken.json", "passes": [{}]}]
        });
        let mut lookup = lookup_from(files);
        let entries = object
            .get("effects")
            .and_then(Value::as_array)
            .cloned()
            .unwrap_or_default();
        let out = resolve_object_effects(&entries, &mut lookup);
        assert!(out.is_empty());
    }

    #[test]
    fn previous_sentinel_and_object_override_and_bind_priority() {
        let mut files: HashMap<&str, Vec<u8>> = HashMap::new();
        files.insert(
            "effects/e.json",
            serde_json::to_vec(&serde_json::json!({
                "name": "e",
                "passes": [{"material": "materials/m.json", "bind": [{"index": 0, "name": "previous"}]}]
            }))
            .unwrap(),
        );
        files.insert("materials/m.json", material_json("x", &["base_tex"]));
        files.insert("materials/base_tex.tex", vec![0u8; 4]);
        let object = serde_json::json!({
            "effects": [{"id": 1, "file": "effects/e.json", "passes": [{"textures": {"0": "override_tex"}}]}]
        });
        files.insert("materials/override_tex.tex", vec![1u8; 4]);
        let mut lookup = lookup_from(files);
        let entries = object
            .get("effects")
            .and_then(Value::as_array)
            .cloned()
            .unwrap_or_default();
        let out = resolve_object_effects(&entries, &mut lookup);
        assert_eq!(out.len(), 1);
        match &out[0].effect.passes[0] {
            EffectPass::Material(m) => {
                // bind wins over the object-level override, which wins
                // over the material's own base texture.
                match &m.texture_slots[0] {
                    Some(EffectTextureSlot::Previous) => {}
                    other => panic!("expected Previous (bind should win), got {other:?}"),
                }
            }
            other => panic!("expected material pass, got {other:?}"),
        }
    }

    #[test]
    fn effects_beyond_the_cap_are_ignored_not_fatal() {
        let mut files: HashMap<&str, Vec<u8>> = HashMap::new();
        files.insert(
            "effects/ok.json",
            serde_json::to_vec(&serde_json::json!({
                "name": "ok",
                "passes": [{"material": "materials/ok.json"}]
            }))
            .unwrap(),
        );
        files.insert("materials/ok.json", material_json("x", &[]));
        let entries: Vec<Value> = (0..(MAX_EFFECTS_PER_OBJECT + 10))
            .map(|i| serde_json::json!({"id": i, "file": "effects/ok.json", "passes": []}))
            .collect();
        let object = serde_json::json!({"effects": entries});
        let mut lookup = lookup_from(files);
        let entries = object
            .get("effects")
            .and_then(Value::as_array)
            .cloned()
            .unwrap_or_default();
        let out = resolve_object_effects(&entries, &mut lookup);
        assert_eq!(out.len(), MAX_EFFECTS_PER_OBJECT);
    }

    #[test]
    fn fbos_and_passes_beyond_the_cap_are_truncated() {
        let mut fbos = Vec::new();
        for i in 0..(MAX_FBOS_PER_EFFECT + 5) {
            fbos.push(serde_json::json!({"name": format!("_rt_x{i}")}));
        }
        let mut passes = Vec::new();
        for i in 0..(MAX_PASSES_PER_EFFECT + 5) {
            passes.push(serde_json::json!({"command": "swap", "source": format!("a{i}"), "target": format!("b{i}")}));
        }
        let effect_json = serde_json::json!({"name": "big", "fbos": fbos, "passes": passes});
        let effect_object = effect_json.as_object().unwrap();
        let mut lookup = lookup_from(HashMap::new());
        let spec = parse_effect_spec(effect_object, &[], &mut lookup).unwrap();
        assert_eq!(spec.fbos.len(), MAX_FBOS_PER_EFFECT);
        assert_eq!(spec.passes.len(), MAX_PASSES_PER_EFFECT);
    }

    #[test]
    fn command_pass_requires_source_and_target() {
        let effect_json = serde_json::json!({
            "name": "bad",
            "passes": [{"command": "copy"}]
        });
        let effect_object = effect_json.as_object().unwrap();
        let mut lookup = lookup_from(HashMap::new());
        assert!(parse_effect_spec(effect_object, &[], &mut lookup).is_none());
    }

    #[test]
    fn unknown_command_name_is_rejected() {
        let effect_json = serde_json::json!({
            "name": "bad",
            "passes": [{"command": "explode", "source": "a", "target": "b"}]
        });
        let effect_object = effect_json.as_object().unwrap();
        let mut lookup = lookup_from(HashMap::new());
        assert!(parse_effect_spec(effect_object, &[], &mut lookup).is_none());
    }

    #[test]
    fn oversized_effect_json_is_omitted() {
        let huge = vec![b' '; (MAX_EFFECT_JSON_BYTES + 1) as usize];
        let object = serde_json::json!({
            "effects": [{"id": 1, "file": "effects/huge.json"}]
        });
        let mut files: HashMap<&str, Vec<u8>> = HashMap::new();
        files.insert("effects/huge.json", huge);
        let mut lookup = lookup_from(files);
        let entries = object
            .get("effects")
            .and_then(Value::as_array)
            .cloned()
            .unwrap_or_default();
        let out = resolve_object_effects(&entries, &mut lookup);
        assert!(out.is_empty());
    }

    /// Regression pinned against the real Workshop scene 1652229298's
    /// godrays effect chain: an object-level pass override's `textures`
    /// is a POSITIONAL array (`[null,"screenmask","util/clouds_256"]`),
    /// not an index-keyed object, and the effect file's own `bind` is a
    /// list of `{"index":N,"name":S}` objects, including the `"previous"`
    /// sentinel at index 0. Both shapes must resolve on the same pass.
    #[test]
    fn real_corpus_positional_override_and_bind_list_shapes_both_resolve() {
        let mut files: HashMap<&str, Vec<u8>> = HashMap::new();
        files.insert(
            "effects/godrays/effect.json",
            serde_json::to_vec(&serde_json::json!({
                "name": "godrays",
                "fbos": [
                    {"name": "_rt_HalfCompoBuffer1", "scale": 2, "format": "rgba8888"}
                ],
                "passes": [
                    {
                        "material": "materials/effects/godrays_downsample2.json",
                        "target": "_rt_HalfCompoBuffer1",
                        "bind": [{"name": "previous", "index": 0}]
                    }
                ]
            }))
            .unwrap(),
        );
        files.insert(
            "materials/effects/godrays_downsample2.json",
            material_json("effects/godrays_downsample2", &[]),
        );
        files.insert("materials/screenmask.tex", vec![7u8; 4]);
        files.insert("materials/util/clouds_256.tex", vec![9u8; 4]);
        let object = serde_json::json!({
            "effects": [{
                "file": "effects/godrays/effect.json",
                "visible": true,
                "passes": [
                    {"textures": [null, "screenmask", "util/clouds_256"]}
                ]
            }]
        });
        let entries = object
            .get("effects")
            .and_then(Value::as_array)
            .cloned()
            .unwrap_or_default();
        let mut lookup = lookup_from(files);
        let out = resolve_object_effects(&entries, &mut lookup);
        assert_eq!(out.len(), 1);
        assert_eq!(out[0].effect.fbos[0].name, "_rt_HalfCompoBuffer1");
        match &out[0].effect.passes[0] {
            EffectPass::Material(m) => {
                // bind (index 0 = "previous") wins over the positional
                // override's slot 0 (which was null anyway); slots 1/2
                // come from the positional override.
                assert!(matches!(
                    m.texture_slots[0],
                    Some(EffectTextureSlot::Previous)
                ));
                match &m.texture_slots[1] {
                    Some(EffectTextureSlot::Texture { name, .. }) => assert_eq!(name, "screenmask"),
                    other => panic!("expected screenmask texture, got {other:?}"),
                }
                match &m.texture_slots[2] {
                    Some(EffectTextureSlot::Texture { name, .. }) => {
                        assert_eq!(name, "util/clouds_256")
                    }
                    other => panic!("expected clouds_256 texture, got {other:?}"),
                }
            }
            other => panic!("expected material pass, got {other:?}"),
        }
    }
}
