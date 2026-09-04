// SPDX-License-Identifier: GPL-3.0-or-later
use std::{
    collections::BTreeSet,
    env, fs,
    io::Read,
    os::unix::fs::OpenOptionsExt,
    path::{Component, Path, PathBuf},
    time::{SystemTime, UNIX_EPOCH},
};

use serde::{Deserialize, Serialize};
use serde_json::Value;
use sha2::{Digest, Sha256};

use crate::{KvValue, parse_key_values};

pub const WALLPAPER_ENGINE_APP_ID: &str = "431960";

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ScanLimits {
    pub max_projects: usize,
    pub max_project_json_bytes: u64,
}

impl Default for ScanLimits {
    fn default() -> Self {
        Self {
            max_projects: 25_000,
            max_project_json_bytes: 4 * 1024 * 1024,
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Catalog {
    pub schema_version: u32,
    pub generated_unix_ms: u128,
    pub libraries: Vec<SteamLibrary>,
    pub items: Vec<CatalogItem>,
    pub diagnostics: Vec<Diagnostic>,
    pub stats: CatalogStats,
}

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct CatalogStats {
    pub total: usize,
    pub scene: usize,
    pub video: usize,
    pub web: usize,
    pub unknown: usize,
    pub invalid: usize,
    pub subscribed: usize,
    pub missing: usize,
    pub downloading: usize,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SteamLibrary {
    pub path: PathBuf,
    pub wallpaper_engine_installed: bool,
    pub workshop_path: PathBuf,
    pub workshop_available: bool,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct CatalogItem {
    pub workshop_id: String,
    pub title: String,
    pub kind: ProjectKind,
    pub compatibility: Compatibility,
    pub compatibility_detail: String,
    pub content_root: PathBuf,
    pub project_file: PathBuf,
    pub entry_file: Option<PathBuf>,
    pub preview_file: Option<PathBuf>,
    pub metadata_hash: Option<String>,
    pub tags: Vec<String>,
    pub requested_permissions: Vec<String>,
    pub workshop_state: String,
    pub workshop_progress: Option<u8>,
    pub diagnostics: Vec<Diagnostic>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ProjectKind {
    Scene,
    Video,
    Web,
    Unknown,
    Invalid,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Compatibility {
    RendererDependent,
    BackendMissing,
    Unsupported,
    Invalid,
}

#[derive(Debug, Clone, Copy, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum DiagnosticLevel {
    Info,
    Warning,
    Error,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Diagnostic {
    pub code: String,
    pub level: DiagnosticLevel,
    pub message: String,
}

pub fn default_steam_roots() -> Vec<PathBuf> {
    let mut roots = Vec::new();
    if let Some(value) = env::var_os("STEAM_ROOT") {
        roots.push(PathBuf::from(value));
    }
    if let Some(home) = env::var_os("HOME") {
        roots.extend(kwe_platform::default_steam_roots(&PathBuf::from(home)));
    }
    roots
}

/// The Wallpaper Engine assets root, if any Steam LIBRARY has one
/// installed: the first `<library>/steamapps/common/wallpaper_engine/assets`
/// that exists (S1; the default the daemon and `kwe preflight` use when
/// `--wallpaper-engine-assets`/`--assets-dir` is not given explicitly).
/// `None` when no library has it — model layers then resolve only against
/// the scene's own package/directory, and every effect/material shader
/// that is not fully self-contained inside a scene's own `scene.pkg`
/// fails to resolve (`shader_source_missing`/a spliced `#include` that
/// silently has no content — S6).
///
/// S6 root cause: this used to search `steam_roots` directly (the 3-4
/// hardcoded candidate paths from `default_steam_roots` — `$STEAM_ROOT`,
/// `~/.local/share/Steam`, `~/.steam/steam`, `~/.steam/root`), NOT the
/// full set of Steam LIBRARY FOLDERS those roots' `libraryfolders.vdf`
/// manifests register — exactly the expansion `discover_libraries`
/// already performs for `scan_installed` (which is how the catalog finds
/// Workshop items on an external library just fine while this function,
/// called separately, missed the assets root on that same library). A
/// common real-world Steam layout — Wallpaper Engine and its Workshop
/// items installed on a SEPARATE library folder (a second drive/mount)
/// from the primary Steam root — silently produced `None` here even
/// though `scan_installed` on the exact same roots correctly finds the
/// library and every scene on it. Fixed by routing through
/// `discover_libraries` the same way `scan_installed` does. Search order
/// is now the libraries' own (sorted, deduplicated) path order rather
/// than `steam_roots` order — immaterial for the overwhelmingly common
/// case of one library with Wallpaper Engine installed.
pub fn default_wallpaper_engine_assets_dir(steam_roots: &[PathBuf]) -> Option<PathBuf> {
    let (libraries, _diagnostics) = discover_libraries(steam_roots);
    libraries.into_iter().find_map(|library| {
        let candidate = library
            .path
            .join("steamapps/common/wallpaper_engine/assets");
        candidate.is_dir().then_some(candidate)
    })
}

pub fn discover_libraries(steam_roots: &[PathBuf]) -> (Vec<SteamLibrary>, Vec<Diagnostic>) {
    let mut candidates = BTreeSet::new();
    let mut diagnostics = Vec::new();
    for root in steam_roots {
        if !root.exists() {
            continue;
        }
        candidates.insert(normalize_existing(root));
        let manifest = root.join("steamapps/libraryfolders.vdf");
        match read_utf8_limited(&manifest, 8 * 1024 * 1024) {
            Ok(contents) => match parse_key_values(&contents) {
                Ok(tree) => collect_library_paths(&tree, &mut candidates),
                Err(error) => diagnostics.push(diag(
                    "steam.library_manifest_invalid",
                    DiagnosticLevel::Warning,
                    format!("Could not parse {}: {error}", manifest.display()),
                )),
            },
            Err(error) if error.kind() != std::io::ErrorKind::NotFound => diagnostics.push(diag(
                "steam.library_manifest_unreadable",
                DiagnosticLevel::Warning,
                format!("Could not read {}: {error}", manifest.display()),
            )),
            Err(_) => {}
        }
    }
    let libraries = candidates
        .into_iter()
        .map(|path| {
            let steamapps = path.join("steamapps");
            let workshop_path = steamapps
                .join("workshop/content")
                .join(WALLPAPER_ENGINE_APP_ID);
            SteamLibrary {
                wallpaper_engine_installed: steamapps
                    .join(format!("appmanifest_{WALLPAPER_ENGINE_APP_ID}.acf"))
                    .is_file(),
                workshop_available: workshop_path.is_dir(),
                workshop_path,
                path,
            }
        })
        .collect();
    (libraries, diagnostics)
}

pub fn scan_installed(steam_roots: &[PathBuf], limits: &ScanLimits) -> Catalog {
    let (libraries, mut diagnostics) = discover_libraries(steam_roots);
    let mut subscribed_ids = BTreeSet::new();
    let mut workshop_progress = std::collections::BTreeMap::new();
    for library in &libraries {
        match read_workshop_subscriptions(&library.path) {
            Ok(items) => {
                subscribed_ids.extend(items.keys().cloned());
                workshop_progress.extend(items);
            }
            Err(error) => diagnostics.push(diag(
                "steam.workshop_manifest_invalid",
                DiagnosticLevel::Warning,
                format!(
                    "Could not read Workshop state for {}: {error}",
                    library.path.display()
                ),
            )),
        }
    }
    let mut items = Vec::new();
    for library in &libraries {
        if !library.workshop_available {
            continue;
        }
        let entries = match fs::read_dir(&library.workshop_path) {
            Ok(value) => value,
            Err(error) => {
                diagnostics.push(diag(
                    "workshop.directory_unreadable",
                    DiagnosticLevel::Warning,
                    format!(
                        "Could not read {}: {error}",
                        library.workshop_path.display()
                    ),
                ));
                continue;
            }
        };
        let mut roots: Vec<PathBuf> = entries
            .filter_map(Result::ok)
            .filter(|entry| entry.file_type().is_ok_and(|kind| kind.is_dir()))
            .map(|entry| entry.path())
            .take(limits.max_projects.saturating_add(1))
            .collect();
        roots.sort();
        for root in roots {
            if items.len() >= limits.max_projects {
                diagnostics.push(diag(
                    "scan.project_limit",
                    DiagnosticLevel::Error,
                    format!(
                        "Stopped after {} projects to bound resource use",
                        limits.max_projects
                    ),
                ));
                break;
            }
            items.push(scan_project(&root, limits));
        }
    }
    let mut installed_ids = BTreeSet::new();
    for item in &mut items {
        installed_ids.insert(item.workshop_id.clone());
        if subscribed_ids.contains(&item.workshop_id) {
            item.workshop_progress = workshop_progress.get(&item.workshop_id).copied().flatten();
            item.workshop_state = if item.workshop_progress.is_some_and(|value| value < 100) {
                "downloading"
            } else {
                "subscribed_installed"
            }
            .into();
        }
    }
    for workshop_id in subscribed_ids
        .difference(&installed_ids)
        .take(limits.max_projects)
    {
        let mut item = CatalogItem {
            workshop_id: workshop_id.clone(),
            title: format!("Workshop item {workshop_id}"),
            kind: ProjectKind::Invalid,
            compatibility: Compatibility::Invalid,
            compatibility_detail:
                "Subscribed in Steam, but the local Workshop files are unavailable".into(),
            content_root: PathBuf::new(),
            project_file: PathBuf::new(),
            entry_file: None,
            preview_file: None,
            metadata_hash: None,
            tags: Vec::new(),
            requested_permissions: Vec::new(),
            workshop_state: "subscribed_missing".into(),
            workshop_progress: workshop_progress.get(workshop_id).copied().flatten(),
            diagnostics: Vec::new(),
        };
        item.diagnostics.push(diag(
            "workshop.item_missing",
            DiagnosticLevel::Warning,
            "Steam reports this item as subscribed, but no local project directory was found",
        ));
        items.push(item);
    }
    items.sort_by(|a, b| {
        a.title
            .to_lowercase()
            .cmp(&b.title.to_lowercase())
            .then(a.workshop_id.cmp(&b.workshop_id))
    });
    let stats = CatalogStats::from_items(&items);
    Catalog {
        schema_version: 1,
        generated_unix_ms: SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap_or_default()
            .as_millis(),
        libraries,
        items,
        diagnostics,
        stats,
    }
}

fn scan_project(root: &Path, limits: &ScanLimits) -> CatalogItem {
    let workshop_id = root
        .file_name()
        .and_then(|v| v.to_str())
        .unwrap_or("unknown")
        .to_string();
    let project_file = root.join("project.json");
    let fallback = || CatalogItem {
        workshop_id: workshop_id.clone(),
        title: format!("Workshop item {workshop_id}"),
        kind: ProjectKind::Invalid,
        compatibility: Compatibility::Invalid,
        compatibility_detail: "Metadata could not be loaded safely".into(),
        content_root: root.to_path_buf(),
        project_file: project_file.clone(),
        entry_file: None,
        preview_file: None,
        metadata_hash: None,
        tags: Vec::new(),
        requested_permissions: Vec::new(),
        workshop_state: "local".into(),
        workshop_progress: None,
        diagnostics: Vec::new(),
    };
    let metadata = match fs::symlink_metadata(&project_file) {
        Ok(value) => value,
        Err(error) => {
            let mut item = fallback();
            item.diagnostics.push(diag(
                "project.metadata_missing",
                DiagnosticLevel::Error,
                format!("project.json is unavailable: {error}"),
            ));
            return item;
        }
    };
    if metadata.file_type().is_symlink() || !metadata.is_file() {
        let mut item = fallback();
        item.diagnostics.push(diag(
            "project.metadata_unsafe_type",
            DiagnosticLevel::Error,
            "project.json must be a regular, non-symlink file",
        ));
        return item;
    }
    if metadata.len() > limits.max_project_json_bytes {
        let mut item = fallback();
        item.diagnostics.push(diag(
            "project.metadata_too_large",
            DiagnosticLevel::Error,
            format!(
                "project.json is {} bytes; limit is {}",
                metadata.len(),
                limits.max_project_json_bytes
            ),
        ));
        return item;
    }
    let bytes = match read_bytes_limited(&project_file, limits.max_project_json_bytes) {
        Ok(value) => value,
        Err(error) => {
            let mut item = fallback();
            item.diagnostics.push(diag(
                "project.metadata_unreadable",
                DiagnosticLevel::Error,
                error.to_string(),
            ));
            return item;
        }
    };
    let value: Value = match serde_json::from_slice(&bytes) {
        Ok(value) => value,
        Err(error) => {
            let mut item = fallback();
            item.metadata_hash = Some(hex::encode(Sha256::digest(&bytes)));
            item.diagnostics.push(diag(
                "project.metadata_invalid_json",
                DiagnosticLevel::Error,
                error.to_string(),
            ));
            return item;
        }
    };
    let raw_kind = string_field(&value, "type")
        .unwrap_or_default()
        .to_ascii_lowercase();
    let kind = match raw_kind.as_str() {
        "scene" => ProjectKind::Scene,
        "video" => ProjectKind::Video,
        "web" => ProjectKind::Web,
        _ => ProjectKind::Unknown,
    };
    let (compatibility, compatibility_detail) = match kind {
        ProjectKind::Scene => (
            Compatibility::RendererDependent,
            "Scene metadata is indexed; Alpha renderer parity is not yet claimed",
        ),
        ProjectKind::Video => (
            Compatibility::RendererDependent,
            "libmpv worker with software fallback; static video preflight",
        ),
        ProjectKind::Web => (
            Compatibility::RendererDependent,
            "sandboxed Chromium worker; network and audio off until granted",
        ),
        ProjectKind::Unknown => (
            Compatibility::Unsupported,
            "Missing or unrecognized Wallpaper Engine project type",
        ),
        ProjectKind::Invalid => unreachable!(),
    };
    let mut item_diagnostics = Vec::new();
    let mut entry_file = safe_child_field(root, &value, "file", &mut item_diagnostics);
    // Wallpaper Engine commonly stores scene.json inside scene.pkg while the
    // metadata still names scene.json. Treat the package as the runnable entry
    // without attempting to parse it during library indexing.
    if kind == ProjectKind::Scene && entry_file.as_ref().is_some_and(|path| !path.exists()) {
        let package = root.join("scene.pkg");
        if package.is_file() {
            entry_file = fs::canonicalize(&package).ok().or(Some(package));
        }
    }
    let preview_file = safe_child_field(root, &value, "preview", &mut item_diagnostics)
        .filter(|path| path.is_file());
    if entry_file.as_ref().is_some_and(|path| !path.exists()) {
        item_diagnostics.push(diag(
            "project.entry_missing",
            DiagnosticLevel::Warning,
            "The declared entry file does not exist",
        ));
    }
    let title = string_field(&value, "title")
        .filter(|v| !v.trim().is_empty())
        .map(|value| truncate_chars(&value, 256))
        .unwrap_or_else(|| format!("Workshop item {workshop_id}"));
    let tags = value
        .get("tags")
        .and_then(Value::as_array)
        .map(|values| {
            values
                .iter()
                .filter_map(|value| value.as_str().map(|text| truncate_chars(text, 64)))
                .take(32)
                .collect()
        })
        .unwrap_or_default();
    let requested_permissions = value
        .get("permissions")
        .and_then(Value::as_array)
        .map(|values| {
            values
                .iter()
                .filter_map(Value::as_str)
                .map(|text| truncate_chars(text, 64))
                .filter(|text| matches!(text.as_str(), "network" | "pointer" | "audio"))
                .take(16)
                .collect()
        })
        .unwrap_or_default();
    CatalogItem {
        workshop_id,
        title,
        kind,
        compatibility,
        compatibility_detail: compatibility_detail.into(),
        content_root: root.to_path_buf(),
        project_file,
        entry_file,
        preview_file,
        metadata_hash: Some(hex::encode(Sha256::digest(&bytes))),
        tags,
        requested_permissions,
        workshop_state: "local".into(),
        workshop_progress: None,
        diagnostics: item_diagnostics,
    }
}

fn safe_child_field(
    root: &Path,
    value: &Value,
    field: &str,
    diagnostics: &mut Vec<Diagnostic>,
) -> Option<PathBuf> {
    let relative = string_field(value, field)?;
    if relative.len() > 4096 {
        diagnostics.push(diag(
            "project.path_too_long",
            DiagnosticLevel::Error,
            format!("Rejected overlong '{field}' path"),
        ));
        return None;
    }
    let path = Path::new(&relative);
    if path.is_absolute()
        || path.components().any(|part| {
            matches!(
                part,
                Component::ParentDir | Component::RootDir | Component::Prefix(_)
            )
        })
    {
        diagnostics.push(diag(
            "project.unsafe_path",
            DiagnosticLevel::Error,
            format!("Rejected unsafe '{field}' path"),
        ));
        return None;
    }
    let candidate = root.join(path);
    if candidate.exists() {
        let canonical_root = fs::canonicalize(root).ok()?;
        let canonical_candidate = fs::canonicalize(&candidate).ok()?;
        if !canonical_candidate.starts_with(&canonical_root) {
            diagnostics.push(diag(
                "project.symlink_escape",
                DiagnosticLevel::Error,
                format!("Rejected '{field}' symlink outside the project"),
            ));
            return None;
        }
        Some(canonical_candidate)
    } else {
        Some(candidate)
    }
}

fn string_field(value: &Value, key: &str) -> Option<String> {
    value.get(key).and_then(Value::as_str).map(str::to_owned)
}

fn collect_library_paths(root: &KvValue, output: &mut BTreeSet<PathBuf>) {
    let Some(folders) = root
        .get_case_insensitive("libraryfolders")
        .and_then(KvValue::object)
    else {
        return;
    };
    for (key, value) in folders {
        if !key.chars().all(|character| character.is_ascii_digit()) {
            continue;
        }
        let path = value
            .string()
            .or_else(|| value.get_case_insensitive("path").and_then(KvValue::string));
        if let Some(path) = path {
            let candidate = PathBuf::from(path.replace("\\\\", "\\"));
            if candidate.is_absolute() && candidate.exists() {
                output.insert(normalize_existing(&candidate));
            }
        }
    }
}

fn read_workshop_subscriptions(
    root: &Path,
) -> Result<std::collections::BTreeMap<String, Option<u8>>, String> {
    let manifest = root
        .join("steamapps")
        .join(format!("appworkshop_{WALLPAPER_ENGINE_APP_ID}.acf"));
    let contents = match read_utf8_limited(&manifest, 8 * 1024 * 1024) {
        Ok(value) => value,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
            return Ok(std::collections::BTreeMap::new());
        }
        Err(error) => return Err(error.to_string()),
    };
    let tree = parse_key_values(&contents).map_err(|error| error.to_string())?;
    let workshop_items = tree
        .get_case_insensitive("appworkshop")
        .and_then(KvValue::object)
        .and_then(|value| {
            value
                .iter()
                .find(|(key, _)| key.eq_ignore_ascii_case("workshopitems"))
                .and_then(|(_, value)| value.object())
        });
    let mut ids = std::collections::BTreeMap::new();
    if let Some(items) = workshop_items {
        for (key, value) in items {
            if key.len() <= 20
                && key.chars().all(|character| character.is_ascii_digit())
                && key != "0"
            {
                ids.insert(key.clone(), workshop_progress(value));
            }
        }
    }
    Ok(ids)
}

fn workshop_progress(value: &KvValue) -> Option<u8> {
    let object = value.object()?;
    let field = |names: &[&str]| {
        names.iter().find_map(|name| {
            object
                .iter()
                .find(|(candidate, _)| candidate.eq_ignore_ascii_case(name))
                .and_then(|(_, value)| value.string())
        })
    };
    let downloaded = field(&["downloadedbytes", "downloaded", "bytesdownloaded"])
        .and_then(|value| value.parse::<u64>().ok());
    let total = field(&["size", "totalbytes", "total"]).and_then(|value| value.parse::<u64>().ok());
    match (downloaded, total) {
        (Some(done), Some(total)) if total > 0 && done <= total => {
            Some(done.saturating_mul(100).checked_div(total).unwrap_or(0) as u8)
        }
        _ => field(&["downloadprogress", "progress"])
            .and_then(|value| value.parse::<u8>().ok())
            .filter(|value| *value <= 100),
    }
}

fn normalize_existing(path: &Path) -> PathBuf {
    fs::canonicalize(path).unwrap_or_else(|_| path.to_path_buf())
}

fn read_bytes_limited(path: &Path, limit: u64) -> std::io::Result<Vec<u8>> {
    let file = fs::OpenOptions::new()
        .read(true)
        .custom_flags(libc::O_CLOEXEC | libc::O_NOFOLLOW)
        .open(path)?;
    let mut bytes = Vec::with_capacity(file.metadata()?.len().min(limit) as usize);
    file.take(limit.saturating_add(1)).read_to_end(&mut bytes)?;
    if bytes.len() as u64 > limit {
        return Err(std::io::Error::new(
            std::io::ErrorKind::InvalidData,
            format!("file exceeds {limit} byte safety limit"),
        ));
    }
    Ok(bytes)
}

fn truncate_chars(value: &str, limit: usize) -> String {
    value.chars().take(limit).collect()
}

fn read_utf8_limited(path: &Path, limit: u64) -> std::io::Result<String> {
    let bytes = read_bytes_limited(path, limit)?;
    String::from_utf8(bytes)
        .map_err(|error| std::io::Error::new(std::io::ErrorKind::InvalidData, error))
}

fn diag(code: &str, level: DiagnosticLevel, message: impl Into<String>) -> Diagnostic {
    Diagnostic {
        code: code.into(),
        level,
        message: message.into(),
    }
}

impl CatalogStats {
    fn from_items(items: &[CatalogItem]) -> Self {
        let mut stats = Self {
            total: items.len(),
            ..Self::default()
        };
        for item in items {
            match item.kind {
                ProjectKind::Scene => stats.scene += 1,
                ProjectKind::Video => stats.video += 1,
                ProjectKind::Web => stats.web += 1,
                ProjectKind::Unknown => stats.unknown += 1,
                ProjectKind::Invalid => stats.invalid += 1,
            }
            if item.workshop_state == "subscribed_installed" {
                stats.subscribed += 1;
            } else if item.workshop_state == "subscribed_missing" {
                stats.missing += 1;
            } else if item.workshop_state == "downloading" {
                stats.downloading += 1;
            }
        }
        stats
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn temp_fixture(name: &str) -> PathBuf {
        let path = env::temp_dir().join(format!("kwe-{name}-{}", std::process::id()));
        let _ = fs::remove_dir_all(&path);
        fs::create_dir_all(path.join("steamapps/workshop/content/431960/100")).unwrap();
        // The scanner canonicalises what it returns; macOS's $TMPDIR is a
        // symlink (/var -> /private/var), so compare against the real path.
        fs::canonicalize(&path).unwrap_or(path)
    }

    #[test]
    fn default_wallpaper_engine_assets_dir_finds_the_first_existing_root() {
        let a = temp_fixture("assets-a");
        let b = temp_fixture("assets-b");
        // Neither root has the assets dir yet: None.
        assert_eq!(
            default_wallpaper_engine_assets_dir(&[a.clone(), b.clone()]),
            None
        );
        fs::create_dir_all(b.join("steamapps/common/wallpaper_engine/assets")).unwrap();
        assert_eq!(
            default_wallpaper_engine_assets_dir(&[a.clone(), b.clone()]),
            Some(b.join("steamapps/common/wallpaper_engine/assets"))
        );
        let _ = fs::remove_dir_all(&a);
        let _ = fs::remove_dir_all(&b);
    }

    /// S6: the real-world shape that produced `shader_source_missing`/a
    /// silently-empty `#include "common.h"` for every scene run without
    /// an explicit `--assets-dir` — Wallpaper Engine installed on a
    /// SEPARATE Steam library folder (a second drive/mount registered in
    /// the primary root's `libraryfolders.vdf`), not directly under any
    /// of `default_steam_roots`'s hardcoded candidates. Before this fix,
    /// `default_wallpaper_engine_assets_dir` searched only the raw
    /// `steam_roots` list and returned `None` here even though
    /// `scan_installed`/`discover_libraries` on the exact same roots
    /// correctly find the library and every scene on it (proven by the
    /// installed daemon on this machine: `scan_installed` lists Workshop
    /// scenes fine, but `default_wallpaper_engine_assets_dir` came back
    /// `None`, so every renderer launched without `--assets-dir`).
    #[test]
    fn default_wallpaper_engine_assets_dir_finds_an_external_library_folder() {
        let root = temp_fixture("assets-external-library");
        let external = root.join("external-library");
        fs::create_dir_all(external.join("steamapps/common/wallpaper_engine/assets")).unwrap();
        fs::write(
            root.join("steamapps/libraryfolders.vdf"),
            format!(
                r#""LibraryFolders" {{ "0" {{ "path" "{}" }} }}"#,
                external.display().to_string().replace('\\', "\\\\")
            ),
        )
        .unwrap();
        // The primary root itself has no assets dir -- only the
        // externally-registered library does.
        assert_eq!(
            default_wallpaper_engine_assets_dir(std::slice::from_ref(&root)),
            Some(external.join("steamapps/common/wallpaper_engine/assets"))
        );
        let _ = fs::remove_dir_all(root);
    }

    #[test]
    fn discovers_root_and_libraryfolders_paths_once() {
        let root = temp_fixture("libraries");
        let external = root.join("external-library");
        fs::create_dir_all(external.join("steamapps/workshop/content/431960")).unwrap();
        fs::write(
            root.join("steamapps/libraryfolders.vdf"),
            format!(
                r#""LibraryFolders" {{ "0" {{ "path" "{}" }} }}"#,
                external.display().to_string().replace('\\', "\\\\")
            ),
        )
        .unwrap();
        fs::write(
            external.join("steamapps/appmanifest_431960.acf"),
            "\"AppState\" { \"appid\" \"431960\" }",
        )
        .unwrap();

        let (libraries, diagnostics) = discover_libraries(std::slice::from_ref(&root));
        assert!(diagnostics.is_empty());
        assert_eq!(libraries.len(), 2);
        assert!(libraries.iter().any(|library| library.path == root));
        let external_library = libraries
            .iter()
            .find(|library| library.path == external)
            .expect("libraryfolders path should be discovered");
        assert!(external_library.wallpaper_engine_installed);
        assert!(external_library.workshop_available);
        let _ = fs::remove_dir_all(root);
    }

    #[test]
    fn indexes_valid_and_invalid_projects_without_dropping_them() {
        let root = temp_fixture("scan");
        fs::write(
            root.join("steamapps/workshop/content/431960/100/project.json"),
            r#"{"title":"Synthetic scene","type":"scene","file":"scene.json","preview":"preview.jpg","tags":["nature","calm"],"permissions":["pointer","network","unknown"]}"#,
        ).unwrap();
        let catalog = scan_installed(std::slice::from_ref(&root), &ScanLimits::default());
        assert_eq!(catalog.stats.total, 1);
        assert_eq!(catalog.items[0].title, "Synthetic scene");
        assert_eq!(catalog.items[0].kind, ProjectKind::Scene);
        assert_eq!(catalog.items[0].tags, ["nature", "calm"]);
        assert_eq!(
            catalog.items[0].requested_permissions,
            ["pointer", "network"]
        );
        let _ = fs::remove_dir_all(root);
    }

    #[test]
    fn exposes_subscribed_and_missing_workshop_items() {
        let root = temp_fixture("workshop-state");
        fs::write(
            root.join("steamapps/workshop/content/431960/100/project.json"),
            r#"{"title":"Subscribed scene","type":"scene"}"#,
        )
        .unwrap();
        fs::write(
            root.join("steamapps/appworkshop_431960.acf"),
            r#""AppWorkshop" { "WorkshopItems" { "100" { "downloadedbytes" "50" "size" "100" } "200" "1" } }"#,
        )
        .unwrap();
        let catalog = scan_installed(std::slice::from_ref(&root), &ScanLimits::default());
        assert_eq!(catalog.stats.subscribed, 0);
        assert_eq!(catalog.stats.downloading, 1);
        assert_eq!(catalog.stats.missing, 1);
        assert!(
            catalog
                .items
                .iter()
                .any(|item| item.workshop_state == "downloading"
                    && item.workshop_progress == Some(50))
        );
        assert!(
            catalog
                .items
                .iter()
                .any(|item| item.workshop_state == "subscribed_missing")
        );
        let _ = fs::remove_dir_all(root);
    }

    #[test]
    fn marks_video_projects_renderer_dependent() {
        let root = temp_fixture("video-compat");
        fs::write(
            root.join("steamapps/workshop/content/431960/100/project.json"),
            r#"{"title":"Synthetic video","type":"video","file":"clip.mp4"}"#,
        )
        .unwrap();
        fs::write(
            root.join("steamapps/workshop/content/431960/100/clip.mp4"),
            b"not a real video",
        )
        .unwrap();
        let catalog = scan_installed(std::slice::from_ref(&root), &ScanLimits::default());
        assert_eq!(catalog.items[0].kind, ProjectKind::Video);
        assert_eq!(
            catalog.items[0].compatibility,
            Compatibility::RendererDependent
        );
        assert_eq!(
            catalog.items[0].compatibility_detail,
            "libmpv worker with software fallback; static video preflight"
        );
        let _ = fs::remove_dir_all(root);
    }

    #[test]
    fn marks_web_projects_renderer_dependent() {
        let root = temp_fixture("web-compat");
        fs::write(
            root.join("steamapps/workshop/content/431960/100/project.json"),
            r#"{"title":"Synthetic web","type":"web","file":"index.html"}"#,
        )
        .unwrap();
        fs::write(
            root.join("steamapps/workshop/content/431960/100/index.html"),
            b"<html>synthetic fixture</html>",
        )
        .unwrap();
        let catalog = scan_installed(std::slice::from_ref(&root), &ScanLimits::default());
        assert_eq!(catalog.items[0].kind, ProjectKind::Web);
        assert_eq!(
            catalog.items[0].compatibility,
            Compatibility::RendererDependent
        );
        assert_eq!(
            catalog.items[0].compatibility_detail,
            "sandboxed Chromium worker; network and audio off until granted"
        );
        let _ = fs::remove_dir_all(root);
    }

    #[test]
    fn rejects_parent_traversal() {
        let root = temp_fixture("traversal");
        fs::write(
            root.join("steamapps/workshop/content/431960/100/project.json"),
            r#"{"title":"Bad path","type":"video","file":"../../secret"}"#,
        )
        .unwrap();
        let catalog = scan_installed(std::slice::from_ref(&root), &ScanLimits::default());
        assert!(catalog.items[0].entry_file.is_none());
        assert!(
            catalog.items[0]
                .diagnostics
                .iter()
                .any(|d| d.code == "project.unsafe_path")
        );
        let _ = fs::remove_dir_all(root);
    }

    #[test]
    fn recognizes_packed_scene_entry() {
        let root = temp_fixture("packed-scene");
        let project = root.join("steamapps/workshop/content/431960/100");
        fs::write(
            project.join("project.json"),
            r#"{"title":"Packed scene","type":"scene","file":"scene.json"}"#,
        )
        .unwrap();
        fs::write(project.join("scene.pkg"), b"synthetic fixture").unwrap();
        let catalog = scan_installed(std::slice::from_ref(&root), &ScanLimits::default());
        assert_eq!(
            catalog.items[0]
                .entry_file
                .as_ref()
                .and_then(|path| path.file_name())
                .and_then(|name| name.to_str()),
            Some("scene.pkg")
        );
        assert!(catalog.items[0].diagnostics.is_empty());
        let _ = fs::remove_dir_all(root);
    }

    #[test]
    fn keeps_missing_and_malformed_metadata_visible() {
        let root = temp_fixture("bad-metadata");
        let workshop = root.join("steamapps/workshop/content/431960");
        fs::create_dir_all(workshop.join("200")).unwrap();
        fs::write(workshop.join("100/project.json"), b"{ definitely not json").unwrap();
        let catalog = scan_installed(std::slice::from_ref(&root), &ScanLimits::default());
        assert_eq!(catalog.stats.total, 2);
        assert_eq!(catalog.stats.invalid, 2);
        let codes: Vec<&str> = catalog
            .items
            .iter()
            .flat_map(|item| {
                item.diagnostics
                    .iter()
                    .map(|diagnostic| diagnostic.code.as_str())
            })
            .collect();
        assert!(codes.contains(&"project.metadata_invalid_json"));
        assert!(codes.contains(&"project.metadata_missing"));
        let _ = fs::remove_dir_all(root);
    }

    #[test]
    fn refuses_oversized_metadata() {
        let root = temp_fixture("oversized");
        fs::write(
            root.join("steamapps/workshop/content/431960/100/project.json"),
            b"123456789",
        )
        .unwrap();
        let limits = ScanLimits {
            max_project_json_bytes: 8,
            ..ScanLimits::default()
        };
        let catalog = scan_installed(std::slice::from_ref(&root), &limits);
        assert_eq!(catalog.items[0].kind, ProjectKind::Invalid);
        assert_eq!(
            catalog.items[0].diagnostics[0].code,
            "project.metadata_too_large"
        );
        let _ = fs::remove_dir_all(root);
    }

    #[test]
    fn refuses_symlinked_project_metadata() {
        use std::os::unix::fs::symlink;

        let root = temp_fixture("metadata-symlink");
        let outside = root.join("outside.json");
        fs::write(&outside, r#"{"title":"Outside","type":"video"}"#).unwrap();
        let project_file = root.join("steamapps/workshop/content/431960/100/project.json");
        symlink(&outside, &project_file).unwrap();
        let catalog = scan_installed(std::slice::from_ref(&root), &ScanLimits::default());
        assert_eq!(catalog.items[0].kind, ProjectKind::Invalid);
        assert_eq!(
            catalog.items[0].diagnostics[0].code,
            "project.metadata_unsafe_type"
        );
        let _ = fs::remove_dir_all(root);
    }
}
