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
    let texture_ref = texture_asset_path(&texture_name);
    let texture_bytes = lookup(&texture_ref)
        .ok_or_else(|| format!("material texture \"{texture_ref}\" could not be resolved"))?;

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
    })
}

/// Confined read used by preflight's directory- and assets-root lookup
/// steps: relative reference, no `..`/absolute/backslash/NUL components,
/// canonicalized inside `root` (so a symlink cannot smuggle a read outside
/// it), a regular file, bounded to `cap` bytes. Mirrors the containment
/// rule `kwe-scene-renderer`'s `resolve_layer_image` enforces for the
/// worker's own file-lane resolution (kept as a separate, smaller
/// implementation here since kwe-core cannot depend on
/// kwe-scene-renderer).
pub fn confined_read(root: &Path, reference: &str, cap: u64) -> Option<Vec<u8>> {
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
    let root_canonical = root.canonicalize().ok()?;
    let candidate = root_canonical.join(joined);
    let canonical = candidate.canonicalize().ok()?;
    if !canonical.starts_with(&root_canonical) {
        return None;
    }
    let metadata = std::fs::symlink_metadata(&canonical).ok()?;
    if metadata.file_type().is_symlink() || !metadata.is_file() {
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
            ("materials/deco.tex", b"TEXV0005fake".to_vec()),
        ]);
        let resolved = resolve_model("models/deco.json", &mut lookup).expect("resolves");
        assert_eq!(resolved.material_ref, "materials/deco.json");
        assert_eq!(resolved.texture_name, "deco");
        assert_eq!(resolved.texture_ref, "materials/deco.tex");
        assert_eq!(resolved.texture_bytes, b"TEXV0005fake");
        assert_eq!(resolved.shader.as_deref(), Some("genericimage2"));
        assert_eq!(resolved.blending.as_deref(), Some("translucent"));
        assert!(resolved.extra_textures.is_empty());
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

        assert_eq!(
            confined_read(&root, "materials/a.tex", 1024),
            Some(b"hello".to_vec())
        );
        assert_eq!(confined_read(&root, "../outside/secret.tex", 1024), None);
        assert_eq!(confined_read(&root, "/etc/passwd", 1024), None);
        assert_eq!(confined_read(&root, "materials/link.tex", 1024), None);
        assert_eq!(confined_read(&root, "materials/missing.tex", 1024), None);
        assert_eq!(confined_read(&root, "", 1024), None);
        // Over the cap: refused even though the file exists and is small
        // enough to read — the cap gates by exact declared size.
        assert_eq!(confined_read(&root, "materials/a.tex", 2), None);

        let _ = fs::remove_dir_all(&root);
        let _ = fs::remove_dir_all(&outside);
    }
}
