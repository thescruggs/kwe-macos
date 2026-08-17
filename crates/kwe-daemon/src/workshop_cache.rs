// SPDX-License-Identifier: Apache-2.0
//! Bounded offline metadata cache for subscribed Workshop items.
//!
//! Steam's local manifests only carry ids and progress; titles, tags, and
//! preview availability come from the Workshop files themselves. When a
//! library is unmounted (or the daemon restarts while Steam is offline) that
//! metadata would otherwise vanish. This cache snapshots it per subscribed
//! item and, after each scan, fills placeholder `subscribed_missing` entries
//! and synthesizes entries for subscriptions whose library disappeared — so
//! the M6 exit gate's "library unmounted" leg degrades to an informative
//! offline state instead of silent disappearance.
//!
//! Entries are touched every scan they appear in (a live subscription never
//! ages out); ids absent from every scan for 30 days are dropped, so an
//! unsubscribed item fades deterministically.

use std::{
    collections::BTreeMap,
    fs,
    path::{Path, PathBuf},
};

use kwe_core::{Catalog, CatalogItem, Diagnostic, DiagnosticLevel, ProjectKind};
use serde::{Deserialize, Serialize};

use crate::persist::{atomic_write, quarantine_invalid_state};

const CACHE_SCHEMA_VERSION: u32 = 1;
const CACHE_FILE: &str = "workshop-metadata-v1.json";
const MAX_CACHE_ITEMS: usize = 25_000;
const MAX_CACHE_BYTES: u64 = 16 * 1024 * 1024;
/// Absent-from-scan entries older than this are dropped (30 days).
const STALE_ENTRY_MS: u128 = 30 * 24 * 60 * 60 * 1000;
/// `ScanLimits::default().max_projects` — synthesis must not exceed it.
const MAX_CATALOG_ITEMS: usize = 25_000;

#[derive(Debug, Clone, Serialize, Deserialize)]
struct CachedItem {
    title: String,
    kind: ProjectKind,
    tags: Vec<String>,
    preview_present: bool,
    metadata_hash: Option<String>,
    workshop_state: String,
    workshop_progress: Option<u8>,
    last_seen_unix_ms: u128,
}

#[derive(Debug, Default, Serialize, Deserialize)]
struct PersistedCache {
    schema_version: u32,
    items: BTreeMap<String, CachedItem>,
}

/// True when the workspace state means "the user subscribed in Steam".
fn is_subscribed_state(state: &str) -> bool {
    matches!(
        state,
        "subscribed_installed" | "downloading" | "subscribed_missing"
    )
}

/// The scanner's synthetic title for missing items has no user value; the
/// cache overlay may replace it (but never a title from the disk scan).
fn placeholder_title(item: &CatalogItem) -> bool {
    item.title == format!("Workshop item {}", item.workshop_id)
}

pub struct WorkshopCache {
    path: PathBuf,
    items: BTreeMap<String, CachedItem>,
}

impl WorkshopCache {
    /// Loads the cache, failing closed: a corrupt or oversized file is
    /// quarantined to an `.invalid-*` sibling and the cache starts fresh.
    pub fn open(state_dir: &Path) -> Self {
        let path = state_dir.join(CACHE_FILE);
        let items = match fs::metadata(&path) {
            Ok(metadata) if metadata.len() > MAX_CACHE_BYTES => {
                eprintln!("event=workshop.cache_oversize bytes={}", metadata.len());
                quarantine_invalid_state(&path);
                BTreeMap::new()
            }
            Ok(_) => match fs::read(&path) {
                Ok(bytes) => match serde_json::from_slice::<PersistedCache>(&bytes) {
                    Ok(cache)
                        if cache.schema_version == CACHE_SCHEMA_VERSION
                            && cache.items.len() <= MAX_CACHE_ITEMS =>
                    {
                        cache.items
                    }
                    Ok(_) => {
                        eprintln!(
                            "event=workshop.cache_invalid detail=unsupported or oversized cache"
                        );
                        quarantine_invalid_state(&path);
                        BTreeMap::new()
                    }
                    Err(error) => {
                        eprintln!("event=workshop.cache_invalid detail={error}");
                        quarantine_invalid_state(&path);
                        BTreeMap::new()
                    }
                },
                Err(error) => {
                    eprintln!("event=workshop.cache_read_error detail={error}");
                    quarantine_invalid_state(&path);
                    BTreeMap::new()
                }
            },
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => BTreeMap::new(),
            Err(error) => {
                eprintln!("event=workshop.cache_read_error detail={error}");
                BTreeMap::new()
            }
        };
        Self { path, items }
    }

    /// Fills placeholder metadata on scanned items, snapshots fresh metadata
    /// from subscribed items, synthesizes vanished subscriptions, and drops
    /// aged-out entries. `now_unix_ms` is the caller's wall-clock epoch.
    pub fn merge_and_update(&mut self, catalog: &mut Catalog, now_unix_ms: u128) {
        let mut updates: Vec<(String, CachedItem)> = Vec::new();
        for item in catalog.items.iter_mut() {
            let cached = self.items.get(&item.workshop_id).cloned();
            if item.workshop_state == "subscribed_missing" {
                // No files to read: restore whatever the cache remembers and
                // keep the entry alive (the subscription itself is current).
                if let Some(cached) = &cached {
                    overlay(item, cached);
                }
                updates.push((
                    item.workshop_id.clone(),
                    CachedItem {
                        title: cached
                            .as_ref()
                            .map_or_else(|| item.title.clone(), |cached| cached.title.clone()),
                        kind: if item.kind == ProjectKind::Invalid {
                            cached.as_ref().map_or(item.kind, |cached| cached.kind)
                        } else {
                            item.kind
                        },
                        tags: cached
                            .as_ref()
                            .map_or_else(|| item.tags.clone(), |cached| cached.tags.clone()),
                        preview_present: cached
                            .as_ref()
                            .is_some_and(|cached| cached.preview_present),
                        metadata_hash: cached
                            .as_ref()
                            .and_then(|cached| cached.metadata_hash.clone()),
                        workshop_state: item.workshop_state.clone(),
                        workshop_progress: item.workshop_progress,
                        last_seen_unix_ms: now_unix_ms,
                    },
                ));
            } else if is_subscribed_state(&item.workshop_state) {
                // Fresh files on disk: snapshot the current metadata.
                updates.push((
                    item.workshop_id.clone(),
                    CachedItem {
                        title: item.title.clone(),
                        kind: item.kind,
                        tags: item.tags.clone(),
                        preview_present: item.preview_file.is_some(),
                        metadata_hash: item.metadata_hash.clone(),
                        workshop_state: item.workshop_state.clone(),
                        workshop_progress: item.workshop_progress,
                        last_seen_unix_ms: now_unix_ms,
                    },
                ));
            }
        }
        for (id, entry) in updates {
            self.items.insert(id, entry);
        }

        // Synthesize entries for subscriptions whose library vanished; drop
        // entries that have not appeared in any scan for too long.
        let present_ids: std::collections::BTreeSet<String> = catalog
            .items
            .iter()
            .map(|item| item.workshop_id.clone())
            .collect();
        let mut aged_out = Vec::new();
        for (id, entry) in &self.items {
            if present_ids.contains(id) {
                continue;
            }
            if entry.last_seen_unix_ms.saturating_add(STALE_ENTRY_MS) < now_unix_ms {
                aged_out.push(id.clone());
                continue;
            }
            if catalog.items.len() >= MAX_CATALOG_ITEMS {
                break;
            }
            catalog.items.push(synthesized_item(id, entry));
            catalog.stats.total = catalog.stats.total.saturating_add(1);
            catalog.stats.subscribed = catalog.stats.subscribed.saturating_add(1);
            catalog.stats.missing = catalog.stats.missing.saturating_add(1);
            catalog.stats.invalid = catalog.stats.invalid.saturating_add(1);
        }
        for id in aged_out {
            self.items.remove(&id);
        }
    }

    /// Persists the cache atomically; evicts the oldest entries when the
    /// bounded file size would be exceeded.
    pub fn save(&mut self) {
        let mut bytes = self.encode_state();
        while bytes.len() as u64 > MAX_CACHE_BYTES && !self.items.is_empty() {
            let oldest = self
                .items
                .iter()
                .min_by_key(|(_, entry)| entry.last_seen_unix_ms)
                .map(|(id, _)| id.clone());
            let Some(oldest) = oldest else { break };
            self.items.remove(&oldest);
            eprintln!("event=workshop.cache_evict workshop_id={oldest}");
            bytes = self.encode_state();
        }
        if bytes.len() as u64 > MAX_CACHE_BYTES {
            eprintln!("event=workshop.cache_oversize bytes={}", bytes.len());
            return;
        }
        if let Err(error) = atomic_write(&self.path, &bytes) {
            eprintln!("event=workshop.cache_save_error detail={error}");
        }
    }

    fn encode_state(&self) -> Vec<u8> {
        serde_json::to_vec_pretty(&PersistedCache {
            schema_version: CACHE_SCHEMA_VERSION,
            items: self.items.clone(),
        })
        .unwrap_or_default()
    }
}

fn overlay(item: &mut CatalogItem, cached: &CachedItem) {
    let mut changed = false;
    if placeholder_title(item) && !cached.title.is_empty() {
        item.title = cached.title.clone();
        changed = true;
    }
    if item.kind == ProjectKind::Invalid && cached.kind != ProjectKind::Invalid {
        item.kind = cached.kind;
        changed = true;
    }
    if item.tags.is_empty() && !cached.tags.is_empty() {
        item.tags = cached.tags.clone();
        changed = true;
    }
    if item.metadata_hash.is_none() && cached.metadata_hash.is_some() {
        item.metadata_hash = cached.metadata_hash.clone();
        changed = true;
    }
    if changed {
        item.diagnostics.push(Diagnostic {
            code: "workshop.offline_metadata".into(),
            level: DiagnosticLevel::Info,
            message: "Metadata restored from the offline cache; the Steam library is unavailable."
                .into(),
        });
    }
}

fn synthesized_item(id: &str, entry: &CachedItem) -> CatalogItem {
    let mut item = CatalogItem {
        workshop_id: id.to_string(),
        title: if entry.title.is_empty() {
            format!("Workshop item {id}")
        } else {
            entry.title.clone()
        },
        kind: entry.kind,
        compatibility: kwe_core::Compatibility::RendererDependent,
        compatibility_detail: "Subscribed in Steam, but the local Workshop files are unavailable"
            .into(),
        content_root: PathBuf::new(),
        project_file: PathBuf::new(),
        entry_file: None,
        preview_file: None,
        metadata_hash: entry.metadata_hash.clone(),
        tags: entry.tags.clone(),
        requested_permissions: Vec::new(),
        workshop_state: "subscribed_missing".into(),
        workshop_progress: entry.workshop_progress,
        diagnostics: Vec::new(),
    };
    item.diagnostics.push(Diagnostic {
        code: "workshop.offline_metadata".into(),
        level: DiagnosticLevel::Info,
        message: "This subscription's Steam library is currently unavailable; metadata is from the offline cache."
            .into(),
    });
    item
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::time::{SystemTime, UNIX_EPOCH};

    fn now_ms() -> u128 {
        SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap_or_default()
            .as_millis()
    }

    fn state_dir(label: &str) -> PathBuf {
        std::env::temp_dir().join(format!(
            "kwe-workshop-cache-{label}-{}-{}",
            std::process::id(),
            now_ms()
        ))
    }

    fn entry(id: &str, title: &str, last_seen: u128) -> (String, CachedItem) {
        (
            id.to_string(),
            CachedItem {
                title: title.into(),
                kind: ProjectKind::Scene,
                tags: vec!["nature".into()],
                preview_present: true,
                metadata_hash: Some("abc".into()),
                workshop_state: "subscribed_installed".into(),
                workshop_progress: None,
                last_seen_unix_ms: last_seen,
            },
        )
    }

    fn empty_catalog() -> Catalog {
        Catalog {
            schema_version: 1,
            generated_unix_ms: 0,
            libraries: Vec::new(),
            items: Vec::new(),
            diagnostics: Vec::new(),
            stats: kwe_core::CatalogStats::default(),
        }
    }

    fn missing_item(id: &str) -> CatalogItem {
        CatalogItem {
            workshop_id: id.into(),
            title: format!("Workshop item {id}"),
            kind: ProjectKind::Invalid,
            compatibility: kwe_core::Compatibility::Invalid,
            compatibility_detail: String::new(),
            content_root: PathBuf::new(),
            project_file: PathBuf::new(),
            entry_file: None,
            preview_file: None,
            metadata_hash: None,
            tags: Vec::new(),
            requested_permissions: Vec::new(),
            workshop_state: "subscribed_missing".into(),
            workshop_progress: None,
            diagnostics: Vec::new(),
        }
    }

    #[test]
    fn fills_placeholder_metadata_on_missing_items() {
        let dir = state_dir("fill");
        fs::create_dir_all(&dir).unwrap();
        let mut cache = WorkshopCache::open(&dir);
        let (id, cached) = entry("100", "Real Title", now_ms());
        cache.items.insert(id.clone(), cached);
        let mut catalog = empty_catalog();
        catalog.items.push(missing_item(&id));
        cache.merge_and_update(&mut catalog, now_ms());
        let item = &catalog.items[0];
        assert_eq!(item.title, "Real Title");
        assert_eq!(item.kind, ProjectKind::Scene);
        assert_eq!(item.tags, vec!["nature"]);
        assert!(
            item.diagnostics
                .iter()
                .any(|diagnostic| diagnostic.code == "workshop.offline_metadata")
        );
        // Live subscription entries stay alive even while missing.
        let touched = cache.items[&id].last_seen_unix_ms;
        assert!(
            touched >= now_ms(),
            "entries seen in a scan must be touched"
        );
    }

    #[test]
    fn synthesizes_vanished_subscriptions_and_ages_out_unsubscribed() {
        let dir = state_dir("synthesize");
        fs::create_dir_all(&dir).unwrap();
        let mut cache = WorkshopCache::open(&dir);
        let now = now_ms();
        let (live_id, live) = entry("100", "Live", now);
        let (stale_id, stale) = entry("200", "Stale", now - STALE_ENTRY_MS - 1);
        cache.items.insert(live_id.clone(), live);
        cache.items.insert(stale_id.clone(), stale);
        let mut catalog = empty_catalog();
        cache.merge_and_update(&mut catalog, now);
        assert_eq!(
            catalog.items.len(),
            1,
            "only the fresh entry is synthesized"
        );
        let item = &catalog.items[0];
        assert_eq!(item.workshop_id, live_id);
        assert_eq!(item.title, "Live");
        assert_eq!(item.workshop_state, "subscribed_missing");
        assert!(cache.items.contains_key(&live_id));
        assert!(
            !cache.items.contains_key(&stale_id),
            "aged-out entries are dropped"
        );
    }

    #[test]
    fn snapshots_fresh_metadata_from_installed_items() {
        let dir = state_dir("snapshot");
        fs::create_dir_all(&dir).unwrap();
        let mut cache = WorkshopCache::open(&dir);
        let mut catalog = empty_catalog();
        catalog.items.push(CatalogItem {
            workshop_id: "7".into(),
            title: "Fresh".into(),
            kind: ProjectKind::Video,
            compatibility: kwe_core::Compatibility::RendererDependent,
            compatibility_detail: String::new(),
            content_root: PathBuf::new(),
            project_file: PathBuf::new(),
            entry_file: None,
            preview_file: Some(PathBuf::from("/preview.jpg")),
            metadata_hash: Some("hash".into()),
            tags: vec!["calm".into()],
            requested_permissions: Vec::new(),
            workshop_state: "subscribed_installed".into(),
            workshop_progress: None,
            diagnostics: Vec::new(),
        });
        cache.merge_and_update(&mut catalog, now_ms());
        let cached = &cache.items["7"];
        assert_eq!(cached.title, "Fresh");
        assert!(cached.preview_present);
        assert_eq!(cached.metadata_hash.as_deref(), Some("hash"));
    }

    #[test]
    fn local_only_items_are_never_cached() {
        let dir = state_dir("local-only");
        fs::create_dir_all(&dir).unwrap();
        let mut cache = WorkshopCache::open(&dir);
        let mut catalog = empty_catalog();
        catalog.items.push(CatalogItem {
            workshop_id: "9".into(),
            title: "Local".into(),
            kind: ProjectKind::Scene,
            compatibility: kwe_core::Compatibility::RendererDependent,
            compatibility_detail: String::new(),
            content_root: PathBuf::new(),
            project_file: PathBuf::new(),
            entry_file: None,
            preview_file: None,
            metadata_hash: None,
            tags: Vec::new(),
            requested_permissions: Vec::new(),
            workshop_state: "local".into(),
            workshop_progress: None,
            diagnostics: Vec::new(),
        });
        cache.merge_and_update(&mut catalog, now_ms());
        assert!(cache.items.is_empty());
    }

    #[test]
    fn corrupt_cache_is_quarantined_and_starts_fresh() {
        let dir = state_dir("corrupt");
        fs::create_dir_all(&dir).unwrap();
        fs::write(dir.join(CACHE_FILE), b"garbage").unwrap();
        let cache = WorkshopCache::open(&dir);
        assert!(cache.items.is_empty());
        let quarantined = fs::read_dir(&dir)
            .unwrap()
            .flatten()
            .any(|entry| entry.file_name().to_string_lossy().contains(".invalid-"));
        assert!(quarantined, "corrupt cache must be quarantined");
    }
}
