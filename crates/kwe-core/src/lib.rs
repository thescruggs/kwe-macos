// SPDX-License-Identifier: Apache-2.0
//! Defensive local discovery for Wallpaper Engine Workshop projects.
//!
//! This is an original implementation informed by Steam's documented library
//! layout and the public behavior of compatible Linux projects. No source code
//! was copied. See `docs/PROVENANCE.md` and `THIRD_PARTY.yml`.

mod audio;
mod keyvalues;
mod mpris;
mod permissions;
mod pipewire;
mod playlist;
mod playlist_runtime;
mod policy;
mod preflight;
mod scan;
mod webpreflight;
mod websandbox;

pub use audio::analyze_stereo;
pub use keyvalues::{KvError, KvValue, parse_key_values};
pub use mpris::{MprisStatus, probe_mpris};
pub use permissions::PermissionPolicy;
pub use pipewire::{PipewireStatus, probe_pipewire};
pub use playlist::{Playlist, PlaylistStore, PlaylistTransition};
pub use playlist_runtime::{PlaylistDecision, PlaylistRuntime, PlaylistRuntimeSnapshot};
pub use policy::{
    PlaybackAction, PlaybackPolicy, PolicyDecision, PolicyRule, PolicySnapshot, PolicyTrigger,
};
pub use preflight::{ScenePreflight, VideoPreflight, preflight_scene, preflight_video};
pub use scan::{
    Catalog, CatalogItem, CatalogStats, Compatibility, Diagnostic, DiagnosticLevel, ProjectKind,
    ScanLimits, SteamLibrary, default_steam_roots, discover_libraries, scan_installed,
};
pub use webpreflight::{WebPreflight, preflight_web};
pub use websandbox::{WebSandboxCommand, chromium_command, sandbox_root};
