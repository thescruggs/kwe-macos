// SPDX-License-Identifier: GPL-3.0-or-later
//! Model → material → texture resolution (S1), shared by preflight and the
//! scene worker so both agree on whether a model layer can draw anything
//! before any pixel decode happens (B2 honesty contract,
//! `crate::sceneobjects`).
//!
//! Borrowed-From: Almamu/linux-wallpaperengine (GPL-3.0-or-later)
//! src/WallpaperEngine/Data/Parsers/ModelParser.cpp:1-33,
//! src/WallpaperEngine/Data/Parsers/MaterialParser.cpp:1-127,
//! src/WallpaperEngine/Data/Model/Material.h:1-59 @ b016d7d1 — adapted.
//! The `image` → model `.json` → `material` path → material `.json` →
//! `passes[0]` → first non-null `textures[]` slot walk mirrors
//! `ModelParser::parse`/`MaterialParser::parsePass` exactly; the texture
//! name → asset path rule (`materials/<name>.tex`) is
//! Borrowed-From: Almamu/linux-wallpaperengine (GPL-3.0-or-later)
//! src/WallpaperEngine/Assets/AssetLocator.cpp:72-80 @ b016d7d1 — adapted
//! (`AssetLocator::texture`). Verified against real Workshop model/material
//! pairs under `/media/crushinator/steamapps/workshop/content/431960/*/`.
//!
//! Scope (S1): resolve the first pass's first texture slot to bytes — a
//! model is drawn as a quad with its material's first texture. Mesh/puppet
//! geometry, the remaining texture slots, and the shader/combo/blend
//! pipeline are parsed and recorded (`ResolvedModel`) but not acted on;
//! later slices consume them.

use std::path::{Component, Path};

use serde_json::Value;

/// Cap on a model.json or material.json file: both are small hand-authored
/// descriptors in the corpus (hundreds of bytes); 1 MiB is generous
/// headroom without exposing the JSON parser to an unbounded buffer.
pub const MAX_MODEL_JSON_BYTES: u64 = 1024 * 1024;

/// Per-file read cap for the model-resolution lookup's scene-directory,
/// assets-root, and pkg-entry steps (S1 review NIT #10 — this used to be
/// two separate `const` definitions, one in `preflight.rs` and one in
/// `pkg.rs`, that happened to agree on the same value): generous headroom
/// over any real `.tex` (mirrors `kwe_core::pkg::MAX_PKG_ENTRY_BYTES`).
/// The model/material JSON steps apply their own tighter
/// `MAX_MODEL_JSON_BYTES` cap inside `resolve_model`.
pub const MODEL_ASSET_READ_CAP: u64 = 64 * 1024 * 1024;

/// The lookup a caller supplies to `resolve_model`: given a reference (a
/// package-relative path like `models/foo.json`, `materials/foo.json`, or
/// `materials/foo.tex`), return its bytes if found. Callers compose the
/// resolution order themselves — scene.pkg entries, then the scene
/// directory, then the Wallpaper Engine assets root — so this module stays
/// free of file-system and package-format specifics.
pub type AssetLookup<'a> = dyn FnMut(&str) -> Option<Vec<u8>> + 'a;

/// The result of walking one model layer's `image` reference through to a
/// texture. `texture_bytes` is the honesty gate's pass/fail signal
/// (deliverable 4: a model counts as drawable only when this resolves);
/// the remaining fields are recorded, unused this slice, for the next one
/// (shader/effect passes).
#[derive(Debug, Clone)]
pub struct ResolvedModel {
    pub model_ref: String,
    pub material_ref: String,
    pub texture_name: String,
    pub texture_ref: String,
    pub texture_bytes: Vec<u8>,
    pub shader: Option<String>,
    pub blending: Option<String>,
    pub cullmode: Option<String>,
    pub depthtest: Option<String>,
    /// `combos` (shader permutation flags) exactly as written; ignored
    /// this slice (no shader pipeline exists yet).
    pub combos: serde_json::Map<String, Value>,
    /// Every OTHER non-null texture slot name in `passes[0].textures`
    /// (index 0's name is `texture_name` above), unresolved and unread —
    /// recorded so the next slice does not have to re-parse the material.
    pub extra_textures: Vec<String>,
    /// S2: `passes[0].constantshadervalues` exactly as written (material
    /// constant overrides for the shader's own `uniform` parameters, e.g.
    /// `{"roughness": "0.2"}`) — the material pipeline maps these onto
    /// `MaterialUniforms.g_MaterialConstants` slots by name.
    pub constant_shader_values: serde_json::Map<String, Value>,
    /// S2: every `g_Texture<N>` slot, POSITIONALLY (index == N, `None` for
    /// an empty/null slot), resolved to bytes. Slot 0 duplicates
    /// `texture_bytes` when slot 0 is non-null (kept for the S1 honesty
    /// gate's "first non-null slot" contract, unaffected by this
    /// addition); this field is what the S2 material pipeline actually
    /// draws with. `Err` from `resolve_model` already means EVERY declared
    /// non-null slot up to `MAX_MATERIAL_TEXTURES` resolved — a material
    /// referencing a texture that does not exist fails resolution
    /// entirely, the same honesty contract slot 0 already had.
    pub texture_slots: Vec<Option<TextureSlot>>,
    /// S3: `model.json`'s own `"fullscreen"` boolean (default `false`) —
    /// a `copybackground` post-process layer (e.g. the corpus's
    /// `models/util/fullscreenlayer.json`) declares this so the renderer
    /// knows to size the layer to the scene's world extent when it has
    /// no static base texture to size from (its only texture slot is
    /// typically a `_rt_` runtime target).
    pub fullscreen: bool,
}

/// The texture-name-to-asset-path rule
/// (`AssetLocator::texture`, `AssetLocator.cpp:72-80`): a material's
/// texture slot names a bare name (optionally with subdirectories, e.g.
/// `masks/foo`), and the actual asset lives at `materials/<name>.tex`.
pub fn texture_asset_path(name: &str) -> String {
    format!("materials/{name}.tex")
}

fn bounded_json(bytes: Vec<u8>, what: &str) -> Result<Value, String> {
    if bytes.len() as u64 > MAX_MODEL_JSON_BYTES {
        return Err(format!(
            "{what} is {} bytes, over the {MAX_MODEL_JSON_BYTES} byte limit",
            bytes.len()
        ));
    }
    serde_json::from_slice(&bytes).map_err(|error| format!("{what} is invalid JSON: {error}"))
}

/// The first non-null texture name in a material pass's `textures` array
/// (`TextureParser::parseTextureMap`, `TextureParser.cpp:125-154`): each
/// entry is a string, an object with a `name` string, or null (an empty
/// slot). Returns `(first_name, remaining_names)`.
/// S2: every `g_Texture<N>` slot (`N` = 0..`MAX_MATERIAL_TEXTURES`) a
/// material pass declares, in POSITIONAL order (unlike
/// `first_texture_name`, a null entry at index 0 means slot 0 is empty —
/// it does NOT get skipped/renumbered — because the material shader's
/// `g_Texture0`/`g_Texture1`/... uniforms are bound by that same
/// positional index, and getting it wrong would sample the wrong image
/// into the wrong slot). Entries past `MAX_MATERIAL_TEXTURES` are
/// ignored (the material pipeline's descriptor set only has that many
/// bindings; a shader referencing a higher index already fails
/// preprocessing in `kwe-scene-renderer::shaderpre` independently).
pub const MAX_MATERIAL_TEXTURES: usize = 8;

fn positional_texture_names(textures: &Value) -> Vec<Option<String>> {
    let Some(array) = textures.as_array() else {
        return Vec::new();
    };
    array
        .iter()
        .take(MAX_MATERIAL_TEXTURES)
        .map(|entry| match entry {
            Value::String(s) if !s.is_empty() => Some(s.clone()),
            Value::Object(object) => object
                .get("name")
                .and_then(Value::as_str)
                .filter(|s| !s.is_empty())
                .map(str::to_owned),
            _ => None,
        })
        .collect()
}

/// One resolved `g_Texture<N>` slot (S2 material pipeline).
#[derive(Debug, Clone)]
pub struct TextureSlot {
    pub name: String,
    pub texture_ref: String,
    pub bytes: Vec<u8>,
    /// S3: `true` when `name` is a `_rt_`/`_alias_` runtime render-target
    /// reference (`crate::sceneeffect::is_runtime_target_name`) rather
    /// than a `materials/<name>.tex` asset — `bytes` is empty in that
    /// case; the slot is "resolved" for the B2 honesty gate, but its
    /// actual pixel content is decided by the renderer's effect chain at
    /// draw time (a live FBO view when an effect resolves it, or the
    /// shared dummy texture when it does not — never a refusal, matching
    /// this slice's degrade-not-refuse rule).
    pub is_render_target: bool,
}

fn first_texture_name(textures: &Value) -> Option<(String, Vec<String>)> {
    let array = textures.as_array()?;
    let mut names = Vec::new();
    for entry in array {
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
            names.push(name);
        }
    }
    if names.is_empty() {
        return None;
    }
    let first = names.remove(0);
    Some((first, names))
}

/// Resolve one model layer's `image` reference (a `.json` model path) all
/// the way to its first texture's bytes: model.json → `material` path →
/// material.json → `passes[0]` → first non-null texture name →
/// `materials/<name>.tex`. Every step goes through the caller's `lookup`
/// closure — this function performs no file I/O itself. `Err` names the
/// step that failed (missing reference, oversized/invalid JSON, no
/// passes, no texture slot, or the texture asset itself not found through
/// `lookup`) — the caller decides what an `Err` means (preflight: not
/// drawable; the worker: a degraded layer).
pub fn resolve_model(
    model_ref: &str,
    lookup: &mut AssetLookup<'_>,
) -> Result<ResolvedModel, String> {
    let model_bytes = lookup(model_ref)
        .ok_or_else(|| format!("model reference \"{model_ref}\" could not be resolved"))?;
    let model_json = bounded_json(model_bytes, "model.json")?;
    let model_object = model_json
        .as_object()
        .ok_or_else(|| "model.json root must be an object".to_string())?;
    let material_ref = model_object
        .get("material")
        .and_then(Value::as_str)
        .ok_or_else(|| "model.json has no string \"material\" field".to_string())?
        .to_string();
    let fullscreen = model_object
        .get("fullscreen")
        .and_then(Value::as_bool)
        .unwrap_or(false);

    let material_bytes = lookup(&material_ref)
        .ok_or_else(|| format!("material reference \"{material_ref}\" could not be resolved"))?;
    let material_json = bounded_json(material_bytes, "material.json")?;
    let material_object = material_json
        .as_object()
        .ok_or_else(|| "material.json root must be an object".to_string())?;
    let passes = material_object
        .get("passes")
        .and_then(Value::as_array)
        .filter(|passes| !passes.is_empty())
        .ok_or_else(|| "material.json has no non-empty \"passes\" array".to_string())?;
    let pass0 = passes[0]
        .as_object()
        .ok_or_else(|| "material.json passes[0] must be an object".to_string())?;

    let (texture_name, extra_textures) = pass0
        .get("textures")
        .and_then(first_texture_name)
        .ok_or_else(|| "material.json passes[0] has no texture slot".to_string())?;
    // S3: a `_rt_`/`_alias_` slot-0 name is never a `materials/<name>.tex`
    // asset on disk — it is a live render target the renderer's effect
    // chain resolves at draw time (`crate::sceneeffect::
    // is_runtime_target_name`). Treating it as a filesystem miss is
    // exactly the bug that kept the one remaining S1/S2-refused local
    // scene (a full-screen effect layer whose base material samples
    // `_rt_FullFrameBuffer`) from ever passing the B2 honesty gate: the
    // gate only needs to know a slot-0 reference is *meaningful*, not
    // that it decodes to bytes right now — a scene whose effect chain
    // then fails to resolve that target still draws (degraded: the
    // shared dummy texture fills that slot), it is never refused for it.
    let is_render_target = crate::sceneeffect::is_runtime_target_name(&texture_name);
    let (texture_ref, texture_bytes) = if is_render_target {
        (texture_name.clone(), Vec::new())
    } else {
        let texture_ref = texture_asset_path(&texture_name);
        let texture_bytes = lookup(&texture_ref)
            .ok_or_else(|| format!("material texture \"{texture_ref}\" could not be resolved"))?;
        // S1 review #2 (preflight/worker agreement): the pre-fix contract
        // only checked that texture BYTES exist, never whether the worker
        // could actually decode them — a resolvable-but-undecodable
        // `.tex` (corrupt header, an unimplemented format) passed
        // preflight and only failed once the worker parsed the identical
        // scene, rolling the apply back after `wallpaper.apply` had
        // already reported success. This cheap, decode-free header check
        // (`crate::texvheader`, mirrors
        // `kwe-scene-renderer::texv::parse_header`'s field layout without
        // duplicating the LZ4/mip-chain/BC-decode machinery kwe-core
        // cannot depend on) closes that gap for the common failure
        // modes: a corrupt/truncated header, an unimplemented format, or
        // a texture whose real dimensions alone would blow the shared
        // texture budget.
        if crate::texvheader::is_texv(&texture_bytes) {
            let real_bytes = crate::texvheader::check_header(&texture_bytes).map_err(|reason| {
                format!("material texture \"{texture_ref}\" is not decodable: {reason}")
            })?;
            if real_bytes > crate::texvheader::MAX_SINGLE_TEXTURE_BUDGET_BYTES {
                return Err(format!(
                    "material texture \"{texture_ref}\" exceeds the texture budget"
                ));
            }
        }
        (texture_ref, texture_bytes)
    };

    // S2: resolve every OTHER positional texture slot (1..MAX_MATERIAL_
    // TEXTURES), best-effort. Unlike slot 0 above, a slot that fails to
    // resolve or fails the same decode-ability header check does NOT fail
    // model resolution — it stays `None`. Slot 0 keeps its original S1
    // all-or-nothing contract (a model with an unresolvable primary
    // texture was never drawable and still is not); the extra slots are
    // additive material data the S1 quad path never needed, so requiring
    // them here would regress scenes S1 already accepts (a broken mask/
    // normal-map texture would newly refuse a model whose base texture is
    // perfectly fine).
    let positional_names = pass0
        .get("textures")
        .map(positional_texture_names)
        .unwrap_or_default();
    let mut texture_slots: Vec<Option<TextureSlot>> = Vec::with_capacity(MAX_MATERIAL_TEXTURES);
    for (index, name) in positional_names.iter().enumerate() {
        let Some(name) = name else {
            texture_slots.push(None);
            continue;
        };
        if index == 0 && *name == texture_name {
            // Slot 0 already resolved above (and is the honesty gate) —
            // reuse it rather than reading the same bytes twice.
            texture_slots.push(Some(TextureSlot {
                name: texture_name.clone(),
                texture_ref: texture_ref.clone(),
                bytes: texture_bytes.clone(),
                is_render_target,
            }));
            continue;
        }
        if crate::sceneeffect::is_runtime_target_name(name) {
            texture_slots.push(Some(TextureSlot {
                name: name.clone(),
                texture_ref: name.clone(),
                bytes: Vec::new(),
                is_render_target: true,
            }));
            continue;
        }
        let slot_ref = texture_asset_path(name);
        let slot = lookup(&slot_ref).and_then(|bytes| {
            if crate::texvheader::is_texv(&bytes) {
                let real_bytes = crate::texvheader::check_header(&bytes).ok()?;
                if real_bytes > crate::texvheader::MAX_SINGLE_TEXTURE_BUDGET_BYTES {
                    return None;
                }
            }
            Some(TextureSlot {
                name: name.clone(),
                texture_ref: slot_ref,
                bytes,
                is_render_target: false,
            })
        });
        texture_slots.push(slot);
    }
    while texture_slots.len() < MAX_MATERIAL_TEXTURES {
        texture_slots.push(None);
    }

    let constant_shader_values = pass0
        .get("constantshadervalues")
        .and_then(Value::as_object)
        .cloned()
        .unwrap_or_default();

    let shader = pass0
        .get("shader")
        .and_then(Value::as_str)
        .map(str::to_owned);
    let blending = pass0
        .get("blending")
        .and_then(Value::as_str)
        .map(str::to_owned);
    let cullmode = pass0
        .get("cullmode")
        .and_then(Value::as_str)
        .map(str::to_owned);
    let depthtest = pass0
        .get("depthtest")
        .and_then(Value::as_str)
        .map(str::to_owned);
    let combos = pass0
        .get("combos")
        .and_then(Value::as_object)
        .cloned()
        .unwrap_or_default();

    Ok(ResolvedModel {
        model_ref: model_ref.to_string(),
        material_ref,
        texture_name,
        texture_ref,
        texture_bytes,
        shader,
        blending,
        cullmode,
        depthtest,
        combos,
        extra_textures,
        constant_shader_values,
        texture_slots,
        fullscreen,
    })
}

/// Confined read used by preflight's directory- and assets-root lookup
/// steps: relative reference, no `..`/absolute/backslash/NUL components,
/// confined inside `root_canonical`, a regular file, bounded to `cap`
/// bytes. Mirrors the containment rule `kwe-scene-renderer`'s
/// `resolve_layer_image` enforces for the worker's own file-lane
/// resolution (kept as a separate, smaller implementation here since
/// kwe-core cannot depend on kwe-scene-renderer).
///
/// **Precondition**: `root_canonical` must already be `Path::canonicalize`d
/// by the caller (S1 review #3) — every lookup for one scene reuses the
/// same one or two roots (scene directory, assets root), so canonicalizing
/// per call turned O(1) syscalls per root into O(models-attempted); the
/// callers below (`preflight::file_lane_asset_lookup`,
/// `pkg::pkg_lane_asset_lookup`) canonicalize each root exactly once,
/// outside the per-object lookup closure. A `root_canonical` that is not
/// actually canonical only makes `starts_with` stricter (fails closed:
/// `candidate.canonicalize()`'s result would need to start with a path
/// that itself may carry unresolved components), never a containment
/// weakening.
pub fn confined_read(root_canonical: &Path, reference: &str, cap: u64) -> Option<Vec<u8>> {
    if reference.is_empty() || reference.contains('\0') || reference.contains('\\') {
        return None;
    }
    let joined = Path::new(reference);
    if joined.is_absolute() {
        return None;
    }
    for component in joined.components() {
        if matches!(
            component,
            Component::ParentDir | Component::RootDir | Component::Prefix(_)
        ) {
            return None;
        }
    }
    let candidate = root_canonical.join(joined);
    // canonicalize() fully resolves symlinks, so `canonical` can never
    // itself be a symlink — the containment defense is entirely the
    // `starts_with` check below (S1 review NIT #7: a prior
    // `symlink_metadata(&canonical).is_symlink()` check here was always
    // false and did nothing).
    let canonical = candidate.canonicalize().ok()?;
    if !canonical.starts_with(root_canonical) {
        return None;
    }
    let metadata = std::fs::metadata(&canonical).ok()?;
    if !metadata.is_file() {
        return None;
    }
    if metadata.len() > cap {
        return None;
    }
    std::fs::read(&canonical).ok()
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::fs;

    fn tmpdir(tag: &str) -> std::path::PathBuf {
        let dir = std::env::temp_dir().join(format!("kwe-scenemodel-{tag}-{}", std::process::id()));
        let _ = fs::remove_dir_all(&dir);
        fs::create_dir_all(&dir).unwrap();
        dir
    }

    /// A trivial in-memory lookup over a fixed map, matching the real
    /// model/material/texture triple pulled from
    /// 1725674512/scene.pkg (see the module doc): confirms the walk
    /// matches the actual corpus shape byte-for-byte on the JSON side.
    fn map_lookup(entries: Vec<(&'static str, Vec<u8>)>) -> impl FnMut(&str) -> Option<Vec<u8>> {
        move |reference: &str| {
            entries
                .iter()
                .find(|(name, _)| *name == reference)
                .map(|(_, bytes)| bytes.clone())
        }
    }

    #[test]
    fn resolves_the_corpus_shaped_model_material_texture_chain() {
        let mut lookup = map_lookup(vec![
            (
                "models/deco.json",
                br#"{"autosize": true, "material": "materials/deco.json"}"#.to_vec(),
            ),
            (
                "materials/deco.json",
                br#"{"passes": [{"blending": "translucent", "cullmode": "nocull",
                    "depthtest": "disabled", "shader": "genericimage2",
                    "textures": ["deco"]}]}"#
                    .to_vec(),
            ),
            (
                "materials/deco.tex",
                crate::texvheader::valid_minimal_texv(4, 4),
            ),
        ]);
        let resolved = resolve_model("models/deco.json", &mut lookup).expect("resolves");
        assert_eq!(resolved.material_ref, "materials/deco.json");
        assert_eq!(resolved.texture_name, "deco");
        assert_eq!(resolved.texture_ref, "materials/deco.tex");
        assert_eq!(
            resolved.texture_bytes,
            crate::texvheader::valid_minimal_texv(4, 4)
        );
        assert_eq!(resolved.shader.as_deref(), Some("genericimage2"));
        assert_eq!(resolved.blending.as_deref(), Some("translucent"));
        assert!(resolved.extra_textures.is_empty());
    }

    /// Regression pinned against the real Workshop scene 1652229298's
    /// blocker: `materials/util/fullscreenlayer.json` (the shared
    /// `copybackground` post-process base material both "Fullscreen"
    /// objects in that scene use) declares its ONLY texture slot as the
    /// literal string `"_rt_FullFrameBuffer"`. Before this fix, slot 0
    /// resolution unconditionally tried `materials/_rt_FullFrameBuffer.tex`
    /// on disk, which can never exist, failing the whole model (the S1
    /// honesty gate) — this is what kept that scene refused after S1/S2.
    #[test]
    fn runtime_render_target_slot_zero_resolves_without_a_filesystem_lookup() {
        let mut lookup = map_lookup(vec![
            (
                "models/util/fullscreenlayer.json",
                br#"{"material": "materials/util/fullscreenlayer.json", "fullscreen": true}"#
                    .to_vec(),
            ),
            (
                "materials/util/fullscreenlayer.json",
                br#"{"passes": [{"shader": "passthrough", "blending": "translucent",
                    "textures": ["_rt_FullFrameBuffer"]}]}"#
                    .to_vec(),
            ),
            // Deliberately NOT providing "materials/_rt_FullFrameBuffer.tex"
            // — resolution must succeed without ever calling lookup() for
            // that path (a real filesystem could never satisfy it).
        ]);
        let resolved =
            resolve_model("models/util/fullscreenlayer.json", &mut lookup).expect("resolves");
        assert_eq!(resolved.texture_name, "_rt_FullFrameBuffer");
        assert_eq!(resolved.texture_ref, "_rt_FullFrameBuffer");
        assert!(resolved.texture_bytes.is_empty());
        assert!(resolved.fullscreen);
        assert!(resolved.texture_slots[0].as_ref().unwrap().is_render_target);
    }

    #[test]
    fn non_runtime_target_slot_zero_still_requires_the_asset_to_resolve() {
        let mut lookup = map_lookup(vec![(
            "models/m.json",
            br#"{"material": "materials/m.json"}"#.to_vec(),
        )]);
        // material.json is never provided — this must still error, not
        // silently pass, proving the render-target short-circuit only
        // applies to `_rt_`/`_alias_`-prefixed names.
        assert!(resolve_model("models/m.json", &mut lookup).is_err());
    }

    #[test]
    fn extra_slot_runtime_target_name_also_short_circuits() {
        let mut lookup = map_lookup(vec![
            (
                "models/m.json",
                br#"{"material": "materials/m.json"}"#.to_vec(),
            ),
            (
                "materials/m.json",
                br#"{"passes": [{"shader": "s", "textures": ["base", "_rt_HalfCompoBuffer1"]}]}"#
                    .to_vec(),
            ),
            ("materials/base.tex", b"bytes".to_vec()),
        ]);
        let resolved = resolve_model("models/m.json", &mut lookup).expect("resolves");
        let slot1 = resolved.texture_slots[1].as_ref().expect("slot 1 resolved");
        assert!(slot1.is_render_target);
        assert_eq!(slot1.name, "_rt_HalfCompoBuffer1");
        assert!(slot1.bytes.is_empty());
    }

    #[test]
    fn skips_null_texture_slots_and_records_the_rest() {
        let mut lookup = map_lookup(vec![
            (
                "models/m.json",
                br#"{"material": "materials/m.json"}"#.to_vec(),
            ),
            (
                "materials/m.json",
                br#"{"passes": [{"shader": "s", "textures": [null, "mask", "phase"]}]}"#.to_vec(),
            ),
            ("materials/mask.tex", b"bytes".to_vec()),
        ]);
        let resolved = resolve_model("models/m.json", &mut lookup).expect("resolves");
        assert_eq!(resolved.texture_name, "mask");
        assert_eq!(resolved.extra_textures, vec!["phase".to_string()]);
        // S2: texture_slots is positional, index 0 stays None (the null
        // entry there was never renumbered away) even though texture_name
        // (the S1 "first non-null" honesty signal) is "mask".
        assert!(resolved.texture_slots[0].is_none());
        assert_eq!(resolved.texture_slots[1].as_ref().unwrap().name, "mask");
        // Slot 2 ("phase") has no resolvable bytes in this lookup — a
        // missing extra slot degrades to None, it does not fail
        // resolution (unlike slot 0).
        assert!(resolved.texture_slots[2].is_none());
    }

    #[test]
    fn positional_slots_resolve_up_to_the_cap_and_constants_are_recorded() {
        let mut lookup = map_lookup(vec![
            (
                "models/m.json",
                br#"{"material": "materials/m.json"}"#.to_vec(),
            ),
            (
                "materials/m.json",
                br#"{"passes": [{"shader": "genericimage2",
                    "textures": ["albedo", "normal"],
                    "constantshadervalues": {"roughness": "0.4"}}]}"#
                    .to_vec(),
            ),
            ("materials/albedo.tex", b"albedo-bytes".to_vec()),
            ("materials/normal.tex", b"normal-bytes".to_vec()),
        ]);
        let resolved = resolve_model("models/m.json", &mut lookup).expect("resolves");
        assert_eq!(resolved.texture_slots.len(), MAX_MATERIAL_TEXTURES);
        assert_eq!(
            resolved.texture_slots[0].as_ref().unwrap().bytes,
            b"albedo-bytes"
        );
        assert_eq!(
            resolved.texture_slots[1].as_ref().unwrap().bytes,
            b"normal-bytes"
        );
        for slot in &resolved.texture_slots[2..] {
            assert!(slot.is_none());
        }
        assert_eq!(
            resolved
                .constant_shader_values
                .get("roughness")
                .and_then(Value::as_str),
            Some("0.4")
        );
    }

    #[test]
    fn missing_model_reference_is_an_error() {
        let mut lookup = map_lookup(vec![]);
        let error = resolve_model("models/missing.json", &mut lookup).unwrap_err();
        assert!(error.contains("could not be resolved"));
    }

    #[test]
    fn missing_material_field_is_an_error() {
        let mut lookup = map_lookup(vec![("models/m.json", br#"{}"#.to_vec())]);
        let error = resolve_model("models/m.json", &mut lookup).unwrap_err();
        assert!(error.contains("material"));
    }

    #[test]
    fn missing_material_reference_is_an_error() {
        let mut lookup = map_lookup(vec![(
            "models/m.json",
            br#"{"material": "materials/gone.json"}"#.to_vec(),
        )]);
        let error = resolve_model("models/m.json", &mut lookup).unwrap_err();
        assert!(error.contains("could not be resolved"));
    }

    #[test]
    fn empty_passes_is_an_error() {
        let mut lookup = map_lookup(vec![
            (
                "models/m.json",
                br#"{"material": "materials/m.json"}"#.to_vec(),
            ),
            ("materials/m.json", br#"{"passes": []}"#.to_vec()),
        ]);
        assert!(resolve_model("models/m.json", &mut lookup).is_err());
    }

    #[test]
    fn all_null_textures_is_an_error() {
        let mut lookup = map_lookup(vec![
            (
                "models/m.json",
                br#"{"material": "materials/m.json"}"#.to_vec(),
            ),
            (
                "materials/m.json",
                br#"{"passes": [{"textures": [null, null]}]}"#.to_vec(),
            ),
        ]);
        let error = resolve_model("models/m.json", &mut lookup).unwrap_err();
        assert!(error.contains("texture slot"));
    }

    #[test]
    fn unresolvable_texture_asset_is_an_error() {
        let mut lookup = map_lookup(vec![
            (
                "models/m.json",
                br#"{"material": "materials/m.json"}"#.to_vec(),
            ),
            (
                "materials/m.json",
                br#"{"passes": [{"textures": ["ghost"]}]}"#.to_vec(),
            ),
        ]);
        let error = resolve_model("models/m.json", &mut lookup).unwrap_err();
        assert!(error.contains("materials/ghost.tex"));
    }

    /// S1 review #2: preflight and the worker used to disagree on a
    /// resolvable-but-undecodable `.tex` — resolve_model counted the model
    /// as drawable once bytes existed, the worker's real TEXV decoder then
    /// failed on the exact same bytes, and the apply transaction rolled
    /// back after `wallpaper.apply` had already reported success. A
    /// header check now runs on any texture bytes carrying the TEXV0005
    /// magic, so a corrupt/unimplemented-format container is refused here
    /// too, before the apply transaction ever starts.
    #[test]
    fn resolvable_but_undecodable_texture_is_refused() {
        let mut lookup = map_lookup(vec![
            (
                "models/m.json",
                br#"{"material": "materials/m.json"}"#.to_vec(),
            ),
            (
                "materials/m.json",
                br#"{"passes": [{"textures": ["broken"]}]}"#.to_vec(),
            ),
            (
                "materials/broken.tex",
                b"TEXV0005-but-the-rest-of-this-is-garbage-not-a-real-header".to_vec(),
            ),
        ]);
        let error = resolve_model("models/m.json", &mut lookup).unwrap_err();
        assert!(error.contains("not decodable"), "unexpected: {error}");
    }

    #[test]
    fn oversized_json_is_refused_before_parsing() {
        let huge = vec![b' '; (MAX_MODEL_JSON_BYTES + 1) as usize];
        let mut lookup = map_lookup(vec![("models/m.json", huge)]);
        let error = resolve_model("models/m.json", &mut lookup).unwrap_err();
        assert!(error.contains("byte limit"));
    }

    #[test]
    fn garbage_json_is_an_error_not_a_panic() {
        let mut lookup = map_lookup(vec![("models/m.json", b"not json".to_vec())]);
        assert!(resolve_model("models/m.json", &mut lookup).is_err());
    }

    #[test]
    fn confined_read_stays_inside_root_and_rejects_traversal_and_symlinks() {
        let root = tmpdir("root");
        let outside = tmpdir("outside");
        fs::create_dir_all(root.join("materials")).unwrap();
        fs::write(root.join("materials").join("a.tex"), b"hello").unwrap();
        fs::write(outside.join("secret.tex"), b"secret").unwrap();
        std::os::unix::fs::symlink(
            outside.join("secret.tex"),
            root.join("materials").join("link.tex"),
        )
        .unwrap();

        // confined_read's precondition: the caller canonicalizes the root
        // once (S1 review #3), not per lookup.
        let root_canonical = root.canonicalize().unwrap();
        assert_eq!(
            confined_read(&root_canonical, "materials/a.tex", 1024),
            Some(b"hello".to_vec())
        );
        assert_eq!(
            confined_read(&root_canonical, "../outside/secret.tex", 1024),
            None
        );
        assert_eq!(confined_read(&root_canonical, "/etc/passwd", 1024), None);
        assert_eq!(
            confined_read(&root_canonical, "materials/link.tex", 1024),
            None
        );
        assert_eq!(
            confined_read(&root_canonical, "materials/missing.tex", 1024),
            None
        );
        assert_eq!(confined_read(&root_canonical, "", 1024), None);
        // Over the cap: refused even though the file exists and is small
        // enough to read — the cap gates by exact declared size.
        assert_eq!(confined_read(&root_canonical, "materials/a.tex", 2), None);

        let _ = fs::remove_dir_all(&root);
        let _ = fs::remove_dir_all(&outside);
    }
}
