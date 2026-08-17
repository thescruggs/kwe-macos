// SPDX-License-Identifier: Apache-2.0
use serde::{Deserialize, Serialize};
use std::process::{Command, Stdio};

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct MprisStatus {
    pub available: bool,
    pub players: Vec<String>,
    pub detail: String,
}

pub fn probe_mpris() -> MprisStatus {
    let result = Command::new("qdbus6")
        .args([
            "org.freedesktop.DBus",
            "/org/freedesktop/DBus",
            "org.freedesktop.DBus.ListNames",
        ])
        .stdin(Stdio::null())
        .stderr(Stdio::piped())
        .output();
    let Ok(output) = result else {
        return MprisStatus {
            available: false,
            players: Vec::new(),
            detail: "qdbus6 is not installed".into(),
        };
    };
    if !output.status.success() {
        return MprisStatus {
            available: false,
            players: Vec::new(),
            detail: String::from_utf8_lossy(&output.stderr)
                .trim()
                .chars()
                .take(256)
                .collect(),
        };
    }
    let players = String::from_utf8_lossy(&output.stdout)
        .lines()
        .filter(|line| line.starts_with("org.mpris.MediaPlayer2."))
        .take(32)
        .map(str::to_owned)
        .collect();
    MprisStatus {
        available: true,
        players,
        detail: "MPRIS names enumerated; no playback control performed".into(),
    }
}
