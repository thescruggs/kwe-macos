// SPDX-License-Identifier: Apache-2.0
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

pub fn preflight_scene(path: &Path) -> ScenePreflight {
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
                Ok(value) if value.is_object() => {}
                Ok(_) => report
                    .reasons
                    .push("scene JSON root must be an object".into()),
                Err(error) => report
                    .reasons
                    .push(format!("scene JSON is invalid: {error}")),
            },
            Err(error) => report.reasons.push(format!("cannot read scene: {error}")),
        }
    }
    report.safe = report.reasons.is_empty();
    report
}

/// Static video-entry preflight: the path must be a regular non-symlink
/// file with an allowlisted container extension, bounded to 2 GiB. Decode
/// and duration bounds are the worker's job (mpv open + duration ≤ 24 h
/// fails closed with exit 73); this preflight never opens or probes media
/// content, so a corrupt file inside an allowlisted extension still passes
/// here and is rejected by the worker instead.
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
        let report = preflight_scene(&valid);
        assert!(report.safe);
        assert_eq!(report.format, "scene-json");
        let invalid = root.join("bad.json");
        fs::File::create(&invalid)
            .unwrap()
            .write_all(b"not json")
            .unwrap();
        assert!(!preflight_scene(&invalid).safe);
        let _ = fs::remove_dir_all(root);
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
    fn rejects_oversized_video_entries_via_sparse_file() {
        let root =
            std::env::temp_dir().join(format!("kwe-preflight-video-size-{}", std::process::id()));
        let _ = fs::remove_dir_all(&root);
        fs::create_dir_all(&root).unwrap();
        // A sparse file carries the size in its metadata without allocating.
        let path = root.join("big.mp4");
        fs::File::create(&path)
            .unwrap()
            .set_len(MAX_VIDEO_BYTES + 1)
            .unwrap();
        let report = preflight_video(&path);
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
