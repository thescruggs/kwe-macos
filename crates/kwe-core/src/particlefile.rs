// SPDX-License-Identifier: GPL-3.0-or-later
//! External particle definition file resolution (S4b): a `particle` object
//! whose `particle` value is a STRING names a `.json` particle file (pkg
//! entry, scene directory, or `<assets>/particles/`) instead of carrying
//! its definition inline. Before S4b these registered with all-default
//! flat-model behavior and never drew (the file was never even opened —
//! see `crate::sceneobjects::SceneObjectKind::ParticleFile`'s prior doc).
//!
//! Borrowed-From: Almamu/linux-wallpaperengine (GPL-3.0-or-later)
//! src/WallpaperEngine/Render/Objects/CParticle.cpp (the `material` key
//! read from the particle file's root object) @ b016d7d1 — adapted. The
//! actual emitter/initializer/operator component-model parse (the bulk of
//! `CParticle.cpp`) lives in `kwe-scene-renderer::particlefile`, which only
//! the worker needs (preflight only needs to know the file resolves and
//! its material has a real texture — the same B2 honesty split S1 already
//! uses for models: `crate::scenemodel`).
//!
//! Resolution walk: `particle_ref` (a `.json` particle-file path) →
//! bounded-JSON root object → its `material` field (a `.json` material
//! path, exactly like a model's `material` field) → [`scenemodel::
//! resolve_material`] for the rest of the walk. Confinement and per-file
//! bounds are the caller's `lookup` closure's job (mirrors
//! `scenemodel::resolve_model`) — this module performs no file I/O itself.

use serde_json::Value;

use crate::scenemodel::{self, AssetLookup, ResolvedMaterial};

/// Cap on one particle definition file: these are small hand-authored
/// descriptors (the local WE asset corpus's examples are all under 2 KiB;
/// even a heavily authored one with dozens of operators stays well under
/// this), matching `scenemodel::MAX_MODEL_JSON_BYTES`'s reasoning — 1 MiB
/// is generous headroom without exposing the JSON parser to an unbounded
/// buffer.
pub const MAX_PARTICLE_FILE_BYTES: u64 = 1024 * 1024;

/// The result of walking one particle system's external file reference to
/// its material's texture. `particle_json` is the bounded, already-parsed
/// root object — the worker's own richer component-model parser
/// (`kwe-scene-renderer::particlefile::parse_component_model`) reads
/// straight from this instead of re-fetching/re-parsing the file; `material`
/// is the B2 honesty gate's pass/fail signal (a particle-file system counts
/// as drawable only when this resolved a real texture, mirroring
/// `scenemodel::ResolvedModel::texture_bytes`).
#[derive(Debug, Clone)]
pub struct ResolvedParticleFile {
    pub particle_ref: String,
    pub particle_json: serde_json::Map<String, Value>,
    pub material: ResolvedMaterial,
}

fn bounded_particle_json(bytes: Vec<u8>) -> Result<Value, String> {
    if bytes.len() as u64 > MAX_PARTICLE_FILE_BYTES {
        return Err(format!(
            "particle file is {} bytes, over the {MAX_PARTICLE_FILE_BYTES} byte limit",
            bytes.len()
        ));
    }
    serde_json::from_slice(&bytes)
        .map_err(|error| format!("particle file is invalid JSON: {error}"))
}

/// Resolve one external particle-file reference: bounded JSON parse, its
/// `material` field, then [`scenemodel::resolve_material`] for the texture
/// chain. `Err` names the step that failed; the caller (preflight: not
/// drawable, worker: keep the M3f flat-model defaults) decides what that
/// means — this function never panics and performs no I/O beyond `lookup`.
pub fn resolve_particle_file(
    particle_ref: &str,
    lookup: &mut AssetLookup<'_>,
) -> Result<ResolvedParticleFile, String> {
    let bytes = lookup(particle_ref)
        .ok_or_else(|| format!("particle file \"{particle_ref}\" could not be resolved"))?;
    let json = bounded_particle_json(bytes)
        .map_err(|detail| format!("particle file \"{particle_ref}\": {detail}"))?;
    let object = json
        .as_object()
        .ok_or_else(|| format!("particle file \"{particle_ref}\" root must be an object"))?
        .clone();
    let material_ref = object
        .get("material")
        .and_then(Value::as_str)
        .ok_or_else(|| {
            format!("particle file \"{particle_ref}\" has no string \"material\" field")
        })?
        .to_string();
    let material = scenemodel::resolve_material(&material_ref, lookup)?;
    Ok(ResolvedParticleFile {
        particle_ref: particle_ref.to_string(),
        particle_json: object,
        material,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    fn lookup_ok<'a>() -> impl FnMut(&str) -> Option<Vec<u8>> + 'a {
        |reference: &str| match reference {
            "particles/halo.json" => Some(
                br#"{"material": "materials/particle/halo.json", "maxcount": 500,
                     "emitter": [{"name": "sphererandom", "rate": 20}]}"#
                    .to_vec(),
            ),
            "materials/particle/halo.json" => {
                Some(br#"{"passes": [{"textures": ["particle/halo"]}]}"#.to_vec())
            }
            "materials/particle/halo.tex" => Some(b"tex-bytes".to_vec()),
            _ => None,
        }
    }

    #[test]
    fn resolves_particle_file_through_its_material() {
        let mut lookup = lookup_ok();
        let resolved =
            resolve_particle_file("particles/halo.json", &mut lookup).expect("should resolve");
        assert_eq!(resolved.particle_ref, "particles/halo.json");
        assert_eq!(resolved.material.texture_bytes, b"tex-bytes");
        assert_eq!(
            resolved
                .particle_json
                .get("maxcount")
                .and_then(Value::as_u64),
            Some(500)
        );
    }

    #[test]
    fn missing_file_is_an_error_not_a_panic() {
        let mut lookup = |_: &str| None;
        assert!(resolve_particle_file("particles/missing.json", &mut lookup).is_err());
    }

    #[test]
    fn oversized_file_is_rejected_before_parsing() {
        let mut lookup = |reference: &str| {
            if reference == "particles/huge.json" {
                let mut bytes = br#"{"material": "m.json", "pad": ""#.to_vec();
                bytes.extend(std::iter::repeat_n(b'a', MAX_PARTICLE_FILE_BYTES as usize));
                bytes.extend_from_slice(b"\"}");
                Some(bytes)
            } else {
                None
            }
        };
        let error = resolve_particle_file("particles/huge.json", &mut lookup).unwrap_err();
        assert!(error.contains("byte limit"), "{error}");
    }

    #[test]
    fn non_object_root_is_rejected() {
        let mut lookup = |reference: &str| {
            if reference == "particles/array.json" {
                Some(b"[1,2,3]".to_vec())
            } else {
                None
            }
        };
        let error = resolve_particle_file("particles/array.json", &mut lookup).unwrap_err();
        assert!(error.contains("must be an object"), "{error}");
    }

    #[test]
    fn missing_material_field_is_rejected() {
        let mut lookup = |reference: &str| {
            if reference == "particles/nomaterial.json" {
                Some(br#"{"emitter": []}"#.to_vec())
            } else {
                None
            }
        };
        let error = resolve_particle_file("particles/nomaterial.json", &mut lookup).unwrap_err();
        assert!(error.contains("material"), "{error}");
    }

    #[test]
    fn unresolvable_material_is_rejected() {
        let mut lookup = |reference: &str| {
            if reference == "particles/badmat.json" {
                Some(br#"{"material": "materials/missing.json"}"#.to_vec())
            } else {
                None
            }
        };
        let error = resolve_particle_file("particles/badmat.json", &mut lookup).unwrap_err();
        assert!(error.contains("could not be resolved"), "{error}");
    }

    #[test]
    fn invalid_json_is_rejected() {
        let mut lookup = |reference: &str| {
            if reference == "particles/junk.json" {
                Some(b"not json".to_vec())
            } else {
                None
            }
        };
        let error = resolve_particle_file("particles/junk.json", &mut lookup).unwrap_err();
        assert!(error.contains("invalid JSON"), "{error}");
    }
}
