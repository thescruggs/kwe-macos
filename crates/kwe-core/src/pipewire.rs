// SPDX-License-Identifier: GPL-3.0-or-later
use serde::{Deserialize, Serialize};
use std::process::{Command, Stdio};

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct PipewireStatus {
    pub available: bool,
    pub server: Option<String>,
    pub detail: String,
}

pub fn probe_pipewire() -> PipewireStatus {
    let result = Command::new("pw-cli")
        .args(["info", "0"])
        .stdin(Stdio::null())
        .stderr(Stdio::piped())
        .output();
    let Ok(output) = result else {
        return PipewireStatus {
            available: false,
            server: None,
            detail: "pw-cli is not installed".into(),
        };
    };
    if !output.status.success() {
        return PipewireStatus {
            available: false,
            server: None,
            detail: String::from_utf8_lossy(&output.stderr)
                .trim()
                .chars()
                .take(256)
                .collect(),
        };
    }
    let text = String::from_utf8_lossy(&output.stdout);
    let server = text.lines().find_map(|line| {
        line.trim()
            .strip_prefix("version:")
            .map(str::trim)
            .map(str::to_owned)
    });
    PipewireStatus {
        available: true,
        server,
        detail: "PipeWire control socket responded; capture remains opt-in".into(),
    }
}
