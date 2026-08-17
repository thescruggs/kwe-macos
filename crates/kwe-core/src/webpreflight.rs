// SPDX-License-Identifier: Apache-2.0
use serde::{Deserialize, Serialize};
use std::{
    fs,
    path::{Path, PathBuf},
};

const MAX_HTML_BYTES: u64 = 16 * 1024 * 1024;

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct WebPreflight {
    pub path: PathBuf,
    pub safe: bool,
    pub network_allowed: bool,
    pub permissions: Vec<String>,
    pub reasons: Vec<String>,
}

pub fn preflight_web(root: &Path, permissions: &[String]) -> WebPreflight {
    let mut report = WebPreflight {
        path: root.to_path_buf(),
        safe: false,
        network_allowed: false,
        permissions: permissions
            .iter()
            .filter(|p| matches!(p.as_str(), "pointer" | "audio" | "network"))
            .cloned()
            .collect(),
        reasons: Vec::new(),
    };
    let entry = root.join("index.html");
    let metadata = match fs::symlink_metadata(&entry) {
        Ok(value) => value,
        Err(error) => {
            report
                .reasons
                .push(format!("cannot stat index.html: {error}"));
            return report;
        }
    };
    if metadata.file_type().is_symlink() {
        report
            .reasons
            .push("index.html must not be a symlink".into());
        return report;
    }
    if !metadata.is_file() {
        report
            .reasons
            .push("index.html must be a regular file".into());
        return report;
    }
    if metadata.len() > MAX_HTML_BYTES {
        report
            .reasons
            .push(format!("index.html exceeds {MAX_HTML_BYTES} byte limit"));
        return report;
    }
    let bytes = match fs::read(&entry) {
        Ok(value) => value,
        Err(error) => {
            report
                .reasons
                .push(format!("cannot read index.html: {error}"));
            return report;
        }
    };
    if !String::from_utf8_lossy(&bytes)
        .to_ascii_lowercase()
        .contains("<html")
    {
        report
            .reasons
            .push("index.html does not contain an HTML root".into());
    }
    report.network_allowed = false;
    report.safe = report.reasons.is_empty();
    report
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn requires_html_entry_and_keeps_network_disabled() {
        let root = std::env::temp_dir().join(format!("kwe-web-preflight-{}", std::process::id()));
        let _ = fs::remove_dir_all(&root);
        fs::create_dir_all(&root).unwrap();
        fs::write(root.join("index.html"), "<html><body>ok</body></html>").unwrap();
        let report = preflight_web(&root, &["network".into(), "pointer".into()]);
        assert!(report.safe);
        assert!(!report.network_allowed);
        assert_eq!(report.permissions, ["network", "pointer"]);
        let _ = fs::remove_dir_all(root);
    }
}
