// SPDX-License-Identifier: GPL-3.0-or-later
//! Defensive local discovery for Wallpaper Engine Workshop projects.
//!
//! This is an original implementation informed by Steam's documented library
//! layout and the public behavior of compatible Linux projects. No source code
//! was copied. See `docs/PROVENANCE.md` and `THIRD_PARTY.yml`.

mod audio;
mod capabilities;
mod keyvalues;
mod mpris;
mod particlefile;
mod permissions;
mod pipewire;
mod pkg;
mod playlist;
mod playlist_runtime;
mod policy;
mod preflight;
mod scan;
mod sceneeffect;
mod scenemodel;
mod sceneobjects;
mod texvheader;
mod vfs;
mod webpreflight;
mod websandbox;

pub use audio::analyze_stereo;
pub use capabilities::{SCENE_CAPABILITIES_IMPLEMENTED, SCENE_CAPABILITIES_LIMITATION_TOLERATED};
pub use keyvalues::{KvError, KvValue, parse_key_values};
pub use mpris::{MprisStatus, probe_mpris};
pub use particlefile::{MAX_PARTICLE_FILE_BYTES, ResolvedParticleFile, resolve_particle_file};
pub use permissions::PermissionPolicy;
pub use pipewire::{PipewireStatus, probe_pipewire};
pub use pkg::{
    MAX_SCENE_JSON_BYTES, MAX_SCRIPT_BYTES, PkgEntry, PkgError, PkgErrorKind, PkgReader,
    asset_entry, image_entry, preflight_pkg, scene_json_entry, script_entry, video_entry,
};
pub use playlist::{Playlist, PlaylistStore, PlaylistTransition};
pub use playlist_runtime::{PlaylistDecision, PlaylistRuntime, PlaylistRuntimeSnapshot};
pub use policy::{
    PlaybackAction, PlaybackPolicy, PolicyDecision, PolicyRule, PolicySnapshot, PolicyTrigger,
};
pub use preflight::{
    ScenePreflight, VideoPreflight, preflight_scene, preflight_video, video_extension_allowed,
};
pub use scan::{
    Catalog, CatalogItem, CatalogStats, Compatibility, Diagnostic, DiagnosticLevel, ProjectKind,
    ScanLimits, SteamLibrary, default_steam_roots, default_wallpaper_engine_assets_dir,
    discover_libraries, scan_installed,
};
pub use sceneeffect::{
    EffectCommand, EffectCommandPass, EffectMaterialPass, EffectPass, EffectSpec,
    EffectTextureSlot, FULL_FRAME_BUFFER, FboSpec, MAX_EFFECTS_PER_OBJECT, MAX_FBOS_PER_EFFECT,
    MAX_PASSES_PER_EFFECT, ObjectEffect, PREVIOUS_INPUT, is_runtime_target_name,
    resolve_object_effects,
};
pub use scenemodel::{
    AssetLookup, MAX_MODEL_JSON_BYTES, ResolvedMaterial, ResolvedModel, TextureSlot, confined_read,
    resolve_material, resolve_model, texture_asset_path,
};
pub use sceneobjects::{
    MAX_MODEL_RESOLUTIONS, MAX_PARTICLE_FILE_RESOLUTIONS, SceneObjectKind, SceneObjectSummary,
    classify_scene_object, scene_property_value, summarize_scene_objects,
    summarize_scene_objects_resolved,
};
pub use vfs::{AssetCategory, ResolvedAsset, ResolvedPath, Vfs, VfsCaps, VfsError, VfsSource};
pub use webpreflight::{WebPreflight, preflight_web};
pub use websandbox::{
    WebSandboxCommand, chromium_command, sandbox_root, web_preview_command, web_renderer_command,
};
