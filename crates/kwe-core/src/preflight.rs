// SPDX-License-Identifier: GPL-3.0-or-later
use std::{
    fs,
    path::{Path, PathBuf},
};

use serde::{Deserialize, Serialize};

const MAX_SCENE_BYTES: u64 = 512 * 1024 * 1024;
const MAX_VIDEO_BYTES: u64 = 2 * 1024 * 1024 * 1024;

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct ScenePreflight {
    pub path: PathBuf,
    pub safe: bool,
    pub format: String,
    pub size_bytes: u64,
    pub reasons: Vec<String>,
}

/// B2: the honesty gate. A scene whose every object is a feature this
/// build cannot render composites to bare `general.clearcolor` — a flat
/// rectangle the user reads as a crash, applied through a transaction that
/// reported success. Refusing here (invalid_params, before the renderer
/// starts) keeps the wallpaper that is already on screen and hands the
/// manager a reason to show instead of a blank desktop.
///
/// Only a scene that declares objects and can draw NONE of them is
/// refused: a scene with one drawable layer is applied (degraded, and the
/// renderer's own diagnostics name what it skipped), and a scene with no
/// objects at all is the author's empty scene, not a missing feature.
/// See `crate::sceneobjects` for the classification and
/// docs/bugs/SCENE_APPLY_BLANK_CLEAR_COLOR.md for the evidence.
pub(crate) fn no_drawable_content_reasons(
    root: &serde_json::Value,
    resolve: &mut crate::scenemodel::AssetLookup<'_>,
) -> Vec<String> {
    let summary = crate::summarize_scene_objects_resolved(root, resolve);
    if summary.drawable() > 0 {
        return Vec::new();
    }
    let unsupported = summary.unsupported_reasons();
    if unsupported.is_empty() {
        return Vec::new();
    }
    // CONTRACT: the manager matches the "draws nothing in this build"
    // phrase in the daemon's invalid_params detail to present this as a
    // feature gap ("your current wallpaper is unchanged") instead of a
    // rejected request (apps/kwe-manager/src/applyclient.cpp mapError).
    // Rewording the prefix silently downgrades that message; change both
    // sides together, or give the refusal its own error code first.
    vec![format!(
        "scene draws nothing in this build: {}",
        unsupported.join("; ")
    )]
}

/// S1: preflight's model-resolution lookup for the scene-json (file) lane
/// — scene directory, then the Wallpaper Engine assets root, in that
/// order. A `.pkg` lane's lookup (pkg entries first) lives in
/// `crate::pkg::preflight_pkg`. Canonicalizes both roots exactly once
/// (S1 review #3), not per model object resolved — `confined_read`
/// requires an already-canonical root.
fn file_lane_asset_lookup(
    scene_dir: &Path,
    assets_dir: Option<&Path>,
) -> impl FnMut(&str) -> Option<Vec<u8>> + use<> {
    let scene_dir_canonical = scene_dir.canonicalize().ok();
    let assets_dir_canonical = assets_dir.and_then(|dir| dir.canonicalize().ok());
    move |reference: &str| {
        if let Some(dir) = &scene_dir_canonical
            && let Some(bytes) = crate::scenemodel::confined_read(
                dir,
                reference,
                crate::scenemodel::MODEL_ASSET_READ_CAP,
            )
        {
            return Some(bytes);
        }
        assets_dir_canonical.as_deref().and_then(|assets| {
            crate::scenemodel::confined_read(
                assets,
                reference,
                crate::scenemodel::MODEL_ASSET_READ_CAP,
            )
        })
    }
}

/// `assets_dir`: the Wallpaper Engine assets root (S1), consulted after
/// the scene's own package/directory when resolving a model layer's
/// material texture (`crate::scenemodel::resolve_model`). `None` when not
/// configured — models then only resolve against assets the scene itself
/// carries.
pub fn preflight_scene(path: &Path, assets_dir: Option<&Path>) -> ScenePreflight {
    let mut report = ScenePreflight {
        path: path.to_path_buf(),
        safe: false,
        format: "unknown".into(),
        size_bytes: 0,
        reasons: Vec::new(),
    };
    let metadata = match fs::symlink_metadata(path) {
        Ok(value) => value,
        Err(error) => {
            report.reasons.push(format!("cannot stat scene: {error}"));
            return report;
        }
    };
    if metadata.file_type().is_symlink() {
        report
            .reasons
            .push("scene entry must not be a symlink".into());
        return report;
    }
    if !metadata.is_file() {
        report
            .reasons
            .push("scene entry must be a regular file".into());
        return report;
    }
    report.size_bytes = metadata.len();
    if report.size_bytes > MAX_SCENE_BYTES {
        report
            .reasons
            .push(format!("scene exceeds {MAX_SCENE_BYTES} byte limit"));
        return report;
    }
    report.format = match path
        .extension()
        .and_then(|value| value.to_str())
        .unwrap_or_default()
        .to_ascii_lowercase()
        .as_str()
    {
        "json" => "scene-json".into(),
        "pkg" => "scene-package".into(),
        extension => {
            report
                .reasons
                .push(format!("unsupported scene extension: .{extension}"));
            return report;
        }
    };
    if report.format == "scene-json" {
        if report.size_bytes > 16 * 1024 * 1024 {
            report
                .reasons
                .push("scene JSON exceeds 16 MiB parse limit".into());
            report.safe = false;
            return report;
        }
        match fs::read(path) {
            Ok(bytes) => match serde_json::from_slice::<serde_json::Value>(&bytes) {
                Ok(value) if value.is_object() => {
                    let scene_dir = path.parent().unwrap_or_else(|| Path::new("."));
                    let mut lookup = file_lane_asset_lookup(scene_dir, assets_dir);
                    report
                        .reasons
                        .extend(no_drawable_content_reasons(&value, &mut lookup));
                }
                Ok(_) => report
                    .reasons
                    .push("scene JSON root must be an object".into()),
                Err(error) => report
                    .reasons
                    .push(format!("scene JSON is invalid: {error}")),
            },
            Err(error) => report.reasons.push(format!("cannot read scene: {error}")),
        }
    } else {
        // M3b: the .pkg branch is structurally validated by the archive
        // reader (magic, version, entry table, bounds, paths). Before M3b
        // this branch passed unconditionally (M1 finding G12).
        return crate::pkg::preflight_pkg(path, assets_dir);
    }
    report.safe = report.reasons.is_empty();
    report
}

/// Static video-entry preflight: the path must be a regular non-symlink
/// file with an allowlisted container extension, bounded to 2 GiB. This
/// preflight never opens or probes media content; decode and duration
/// bounds are the worker's job — the video renderer rejects a backend
/// decode failure AND a known duration over 24 h with exit 73, while an
/// unreadable duration fails open. A corrupt file inside an allowlisted
/// extension therefore still passes here and is rejected by the worker.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct VideoPreflight {
    pub path: PathBuf,
    pub safe: bool,
    pub format: String,
    pub size_bytes: u64,
    pub reasons: Vec<String>,
}

pub fn preflight_video(path: &Path) -> VideoPreflight {
    let mut report = VideoPreflight {
        path: path.to_path_buf(),
        safe: false,
        format: "unknown".into(),
        size_bytes: 0,
        reasons: Vec::new(),
    };
    let metadata = match fs::symlink_metadata(path) {
        Ok(value) => value,
        Err(error) => {
            report.reasons.push(format!("cannot stat video: {error}"));
            return report;
        }
    };
    if metadata.file_type().is_symlink() {
        report
            .reasons
            .push("video entry must not be a symlink".into());
        return report;
    }
    if !metadata.is_file() {
        report
            .reasons
            .push("video entry must be a regular file".into());
        return report;
    }
    report.size_bytes = metadata.len();
    if report.size_bytes > MAX_VIDEO_BYTES {
        report
            .reasons
            .push(format!("video exceeds {MAX_VIDEO_BYTES} byte limit"));
        return report;
    }
    let extension = path
        .extension()
        .and_then(|value| value.to_str())
        .unwrap_or_default()
        .to_ascii_lowercase();
    report.format = match extension.as_str() {
        // Containers libmpv opens through its built-in demuxers.
        "mp4" | "webm" | "mkv" | "mov" | "avi" | "wmv" | "flv" | "m4v" | "ogv" => {
            format!("video-{extension}")
        }
        extension => {
            report
                .reasons
                .push(format!("unsupported video extension: .{extension}"));
            return report;
        }
    };
    report.safe = report.reasons.is_empty();
    report
}

/// Whether a scene VideoLayer reference names one of the same local
/// containers accepted by the supervised video worker. This is only an
/// early policy gate; libmpv still probes the bytes and the decoder's
/// protocol whitelist is the security boundary.
#[must_use]
pub fn video_extension_allowed(reference: &str) -> bool {
    if reference.contains("://") || reference.contains('\0') {
        return false;
    }
    matches!(
        Path::new(reference)
            .extension()
            .and_then(|value| value.to_str())
            .unwrap_or_default()
            .to_ascii_lowercase()
            .as_str(),
        "mp4" | "webm" | "mkv" | "mov" | "avi" | "wmv" | "flv" | "m4v" | "ogv"
    )
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::Write;

    #[test]
    fn accepts_object_scene_json_and_rejects_invalid_content() {
        let root = std::env::temp_dir().join(format!("kwe-preflight-{}", std::process::id()));
        let _ = fs::remove_dir_all(&root);
        fs::create_dir_all(&root).unwrap();
        let valid = root.join("scene.json");
        fs::write(&valid, br#"{"objects":[]}"#).unwrap();
        let report = preflight_scene(&valid, None);
        assert!(report.safe);
        assert_eq!(report.format, "scene-json");
        let invalid = root.join("bad.json");
        fs::File::create(&invalid)
            .unwrap()
            .write_all(b"not json")
            .unwrap();
        assert!(!preflight_scene(&invalid, None).safe);
        let _ = fs::remove_dir_all(root);
    }

    /// B2: a file-lane scene.json made only of features this build cannot
    /// render is refused with a reason naming each one, instead of applying
    /// and compositing bare clear colour.
    #[test]
    fn refuses_scene_json_with_no_drawable_content() {
        let root = std::env::temp_dir().join(format!("kwe-preflight-b2-{}", std::process::id()));
        let _ = fs::remove_dir_all(&root);
        fs::create_dir_all(&root).unwrap();
        let blank = root.join("scene.json");
        fs::write(
            &blank,
            br#"{"objects": [{"name": "a", "image": "models/a.json"}]}"#,
        )
        .unwrap();
        let report = preflight_scene(&blank, None);
        assert!(!report.safe);
        assert!(
            report
                .reasons
                .join("; ")
                .contains("material textures could not be resolved"),
            "{:?}",
            report.reasons
        );

        // One drawable object is enough: degraded applies are allowed.
        let mixed = root.join("mixed.json");
        fs::write(
            &mixed,
            br#"{"objects": [{"name": "a", "image": "models/a.json"},
                             {"name": "b", "image": "textures/b.png"}]}"#,
        )
        .unwrap();
        assert!(preflight_scene(&mixed, None).safe);

        // An objectless scene is empty by authorship, not by a missing
        // feature (the existing accepts_object_scene_json case), so it
        // stays safe.
        let empty = root.join("empty.json");
        fs::write(&empty, br#"{"objects": []}"#).unwrap();
        assert!(preflight_scene(&empty, None).safe);
        let _ = fs::remove_dir_all(root);
    }

    /// S1: with an assets root configured and a real model/material/tex
    /// chain resolvable inside it, the model layer now counts as
    /// drawable — the scene that used to refuse on "scene3d" passes.
    #[test]
    fn model_layer_resolves_and_applies_with_an_assets_root() {
        let root =
            std::env::temp_dir().join(format!("kwe-preflight-model-ok-{}", std::process::id()));
        let assets =
            std::env::temp_dir().join(format!("kwe-preflight-model-assets-{}", std::process::id()));
        let _ = fs::remove_dir_all(&root);
        let _ = fs::remove_dir_all(&assets);
        fs::create_dir_all(root.join("models")).unwrap();
        fs::create_dir_all(assets.join("materials")).unwrap();
        fs::write(
            root.join("models").join("a.json"),
            br#"{"material": "materials/a.json"}"#,
        )
        .unwrap();
        // The material lives in the scene directory too (a common corpus
        // layout); only the .tex asset itself needs the assets root.
        fs::create_dir_all(root.join("materials")).unwrap();
        fs::write(
            root.join("materials").join("a.json"),
            br#"{"passes": [{"shader": "genericimage2", "textures": ["a"]}]}"#,
        )
        .unwrap();
        fs::write(
            assets.join("materials").join("a.tex"),
            crate::texvheader::valid_minimal_texv(4, 4),
        )
        .unwrap();

        let scene = root.join("scene.json");
        fs::write(
            &scene,
            br#"{"objects": [{"name": "a", "image": "models/a.json"}]}"#,
        )
        .unwrap();

        let without_assets = preflight_scene(&scene, None);
        assert!(!without_assets.safe, "no assets root: still unresolved");

        let with_assets = preflight_scene(&scene, Some(&assets));
        assert!(
            with_assets.safe,
            "resolvable model must apply: {:?}",
            with_assets.reasons
        );
        let _ = fs::remove_dir_all(root);
        let _ = fs::remove_dir_all(assets);
    }

    #[test]
    fn accepts_allowlisted_video_extensions_case_insensitively() {
        let root =
            std::env::temp_dir().join(format!("kwe-preflight-video-allow-{}", std::process::id()));
        let _ = fs::remove_dir_all(&root);
        fs::create_dir_all(&root).unwrap();
        for extension in [
            "mp4", "webm", "mkv", "mov", "avi", "wmv", "flv", "m4v", "ogv",
        ] {
            let path = root.join(format!("clip.{extension}"));
            fs::write(&path, b"not a real video").unwrap();
            let report = preflight_video(&path);
            assert!(report.safe, "{extension}: {:?}", report.reasons);
            assert_eq!(report.format, format!("video-{extension}"));
        }
        let uppercase = root.join("clip.MP4");
        fs::write(&uppercase, b"not a real video").unwrap();
        let report = preflight_video(&uppercase);
        assert!(report.safe, "uppercase extension: {:?}", report.reasons);
        assert_eq!(report.format, "video-mp4");
        let _ = fs::remove_dir_all(root);
    }

    #[test]
    fn rejects_disallowed_video_extensions_with_reason() {
        let root =
            std::env::temp_dir().join(format!("kwe-preflight-video-ext-{}", std::process::id()));
        let _ = fs::remove_dir_all(&root);
        fs::create_dir_all(&root).unwrap();
        let path = root.join("clip.bin");
        fs::write(&path, b"not a real video").unwrap();
        let report = preflight_video(&path);
        assert!(!report.safe);
        assert!(
            report
                .reasons
                .iter()
                .any(|reason| reason.contains("unsupported video extension: .bin")),
            "unexpected reasons: {:?}",
            report.reasons
        );
        let _ = fs::remove_dir_all(root);
    }

    #[test]
    fn rejects_symlinked_video_entries() {
        let root = std::env::temp_dir().join(format!(
            "kwe-preflight-video-symlink-{}",
            std::process::id()
        ));
        let _ = fs::remove_dir_all(&root);
        fs::create_dir_all(&root).unwrap();
        fs::write(root.join("real.mp4"), b"not a real video").unwrap();
        std::os::unix::fs::symlink(root.join("real.mp4"), root.join("link.mp4")).unwrap();
        let report = preflight_video(&root.join("link.mp4"));
        assert!(!report.safe);
        assert!(
            report
                .reasons
                .iter()
                .any(|reason| reason.contains("must not be a symlink")),
            "unexpected reasons: {:?}",
            report.reasons
        );
        let _ = fs::remove_dir_all(root);
    }

    #[test]
    fn accepts_video_entries_at_the_exact_size_bound() {
        let root =
            std::env::temp_dir().join(format!("kwe-preflight-video-size-{}", std::process::id()));
        let _ = fs::remove_dir_all(&root);
        fs::create_dir_all(&root).unwrap();
        // A sparse file carries the size in its metadata without allocating.
        let exact = root.join("exact.mp4");
        fs::File::create(&exact)
            .unwrap()
            .set_len(MAX_VIDEO_BYTES)
            .unwrap();
        let report = preflight_video(&exact);
        assert!(
            report.safe,
            "exactly {MAX_VIDEO_BYTES} bytes must pass: {:?}",
            report.reasons
        );
        let oversized = root.join("big.mp4");
        fs::File::create(&oversized)
            .unwrap()
            .set_len(MAX_VIDEO_BYTES + 1)
            .unwrap();
        let report = preflight_video(&oversized);
        assert!(!report.safe);
        assert!(
            report
                .reasons
                .iter()
                .any(|reason| reason.contains("byte limit")),
            "unexpected reasons: {:?}",
            report.reasons
        );
        let _ = fs::remove_dir_all(root);
    }

    #[test]
    fn rejects_trailing_dot_extension_video_entries() {
        let root =
            std::env::temp_dir().join(format!("kwe-preflight-video-trdot-{}", std::process::id()));
        let _ = fs::remove_dir_all(&root);
        fs::create_dir_all(&root).unwrap();
        // A trailing dot leaves no extension; the path must not sneak past
        // the allowlist as if it were an mp4.
        let path = root.join("clip.mp4.");
        fs::write(&path, b"not a real video").unwrap();
        let report = preflight_video(&path);
        assert!(!report.safe);
        assert!(
            report
                .reasons
                .iter()
                .any(|reason| reason.contains("unsupported video extension")),
            "unexpected reasons: {:?}",
            report.reasons
        );
        let _ = fs::remove_dir_all(root);
    }

    #[test]
    fn scene_video_extension_policy_is_local_container_only() {
        assert!(video_extension_allowed("clip.MP4"));
        assert!(video_extension_allowed("nested/movie.webm"));
        assert!(!video_extension_allowed("playlist.m3u8"));
        assert!(!video_extension_allowed("https://example.test/movie.mp4"));
        assert!(!video_extension_allowed("movie.bin"));
    }

    #[test]
    fn accepts_structural_pkg_scenes() {
        let root =
            std::env::temp_dir().join(format!("kwe-preflight-pkg-ok-{}", std::process::id()));
        let _ = fs::remove_dir_all(&root);
        fs::create_dir_all(&root).unwrap();
        let mut writer = crate::pkg::testutil::PkgWriter::new();
        writer.add("scene.json", br#"{"general":{}}"#);
        writer.write(&root.join("scene.pkg"), "0001");
        let report = preflight_scene(&root.join("scene.pkg"), None);
        assert!(
            report.safe,
            "valid pkg must pass preflight: {:?}",
            report.reasons
        );
        assert_eq!(report.format, "scene-package");
        assert!(report.size_bytes > 0);
        let _ = fs::remove_dir_all(root);
    }

    #[test]
    fn rejects_garbage_pkg_scenes() {
        // M1 finding G12: before M3b the .pkg branch passed preflight
        // unconditionally. Now the archive table is validated structurally.
        let root =
            std::env::temp_dir().join(format!("kwe-preflight-pkg-bad-{}", std::process::id()));
        let _ = fs::remove_dir_all(&root);
        fs::create_dir_all(&root).unwrap();
        for (name, bytes) in [
            ("garbage.pkg", b"this is not a pkg".as_slice()),
            ("corrupt-magic.pkg", b"\x08\x00\x00\x00XXXX0001\x00\x00\x00\x00"),
            (
                "traversal.pkg",
                b"\x08\x00\x00\x00PKGV0001\x01\x00\x00\x00\x07\x00\x00\x00../evil\x00\x00\x00\x00\x01\x00\x00\x00x",
            ),
        ] {
            let path = root.join(name);
            fs::write(&path, bytes).unwrap();
            let report = preflight_scene(&path, None);
            assert!(!report.safe, "{name} must be rejected");
            assert_eq!(report.format, "scene-package");
            assert!(
                report
                    .reasons
                    .iter()
                    .any(|reason| reason.contains("scene package is invalid")),
                "{name}: unexpected reasons: {:?}",
                report.reasons
            );
        }
        let _ = fs::remove_dir_all(root);
    }

    #[test]
    fn rejects_symlinked_pkg_scenes() {
        let root =
            std::env::temp_dir().join(format!("kwe-preflight-pkg-link-{}", std::process::id()));
        let _ = fs::remove_dir_all(&root);
        fs::create_dir_all(&root).unwrap();
        let mut writer = crate::pkg::testutil::PkgWriter::new();
        writer.add("scene.json", br#"{"general":{}}"#);
        let real = root.join("real.pkg");
        writer.write(&real, "0001");
        let link = root.join("link.pkg");
        std::os::unix::fs::symlink(&real, &link).unwrap();
        let report = preflight_scene(&link, None);
        assert!(!report.safe);
        assert!(
            report
                .reasons
                .iter()
                .any(|reason| reason.contains("must not be a symlink")),
            "unexpected reasons: {:?}",
            report.reasons
        );
        let _ = fs::remove_dir_all(root);
    }

    #[test]
    fn rejects_oversized_pkg_entries_at_preflight() {
        // M3b review follow-up (preflight/worker cap parity): an oversized
        // scene.json or script entry is caught statically at preflight
        // (invalid_params) instead of bouncing the worker (exit 73).
        let root =
            std::env::temp_dir().join(format!("kwe-preflight-pkg-cap-{}", std::process::id()));
        let _ = fs::remove_dir_all(&root);
        fs::create_dir_all(&root).unwrap();

        let mut writer = crate::pkg::testutil::PkgWriter::new();
        writer.add("scene.json", &vec![0_u8; 16 * 1024 * 1024 + 1]);
        writer.write(&root.join("big-scene.pkg"), "0001");
        let report = preflight_scene(&root.join("big-scene.pkg"), None);
        assert!(!report.safe);
        assert!(
            report.reasons.iter().any(|reason| {
                reason.contains("scene.json entry") && reason.contains("over the 16777216 byte cap")
            }),
            "unexpected reasons: {:?}",
            report.reasons
        );

        let mut writer = crate::pkg::testutil::PkgWriter::new();
        writer.add("scene.json", br#"{"general":{"script":"script.js"}}"#);
        writer.add("script.js", &vec![0_u8; 2 * 1024 * 1024 + 1]);
        writer.write(&root.join("big-script.pkg"), "0001");
        let report = preflight_scene(&root.join("big-script.pkg"), None);
        assert!(!report.safe);
        assert!(
            report.reasons.iter().any(|reason| {
                reason.contains("script entry") && reason.contains("over the 2097152 byte cap")
            }),
            "unexpected reasons: {:?}",
            report.reasons
        );

        // A package whose entries fit the caps stays safe.
        let mut writer = crate::pkg::testutil::PkgWriter::new();
        writer.add("scene.json", br#"{"general":{"script":"script.js"}}"#);
        writer.add("script.js", b"function init() {}");
        writer.write(&root.join("small.pkg"), "0001");
        let report = preflight_scene(&root.join("small.pkg"), None);
        assert!(report.safe, "unexpected reasons: {:?}", report.reasons);
        let _ = fs::remove_dir_all(root);
    }

    #[test]
    fn rejects_directories_as_video_entries() {
        let root =
            std::env::temp_dir().join(format!("kwe-preflight-video-dir-{}", std::process::id()));
        let _ = fs::remove_dir_all(&root);
        fs::create_dir_all(root.join("clip.mp4")).unwrap();
        let report = preflight_video(&root.join("clip.mp4"));
        assert!(!report.safe);
        assert!(
            report
                .reasons
                .iter()
                .any(|reason| reason.contains("must be a regular file")),
            "unexpected reasons: {:?}",
            report.reasons
        );
        let _ = fs::remove_dir_all(root);
    }
}
