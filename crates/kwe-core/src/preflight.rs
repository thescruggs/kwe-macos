// SPDX-License-Identifier: Apache-2.0
use std::{
    fs,
    path::{Path, PathBuf},
};

use serde::{Deserialize, Serialize};

const MAX_SCENE_BYTES: u64 = 512 * 1024 * 1024;

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
}
