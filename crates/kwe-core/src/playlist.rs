// SPDX-License-Identifier: Apache-2.0
use serde::{Deserialize, Serialize};
use std::{fs, path::PathBuf};

const MAX_ENTRIES: usize = 1024;
const MAX_PLAYLISTS: usize = 256;
const MAX_STORE_BYTES: u64 = 4 * 1024 * 1024;
const MIN_DURATION_SECONDS: u32 = 10;
const MAX_DURATION_SECONDS: u32 = 24 * 60 * 60;
const DEFAULT_DURATION_SECONDS: u32 = 5 * 60;
const MAX_TRANSITION_SECONDS: u8 = 10;

fn default_duration_seconds() -> u32 {
    DEFAULT_DURATION_SECONDS
}

#[derive(Debug, Clone, Copy, Default, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum PlaylistTransition {
    #[default]
    None,
    Crossfade,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct Playlist {
    pub id: String,
    pub title: String,
    pub entries: Vec<String>,
    pub shuffle: bool,
    pub repeat: bool,
    #[serde(default = "default_duration_seconds")]
    pub duration_seconds: u32,
    #[serde(default)]
    pub transition: PlaylistTransition,
    #[serde(default)]
    pub transition_seconds: u8,
}

#[derive(Debug, Clone)]
pub struct PlaylistStore {
    path: PathBuf,
}

impl PlaylistStore {
    pub fn new(path: PathBuf) -> Self {
        Self { path }
    }
    pub fn load(&self) -> Result<Vec<Playlist>, String> {
        let bytes = match fs::read(&self.path) {
            Ok(value) => value,
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(Vec::new()),
            Err(error) => return Err(error.to_string()),
        };
        if bytes.len() as u64 > MAX_STORE_BYTES {
            return Err("playlist store exceeds safety limit".into());
        }
        let playlists: Vec<Playlist> =
            serde_json::from_slice(&bytes).map_err(|error| error.to_string())?;
        if playlists.len() > MAX_PLAYLISTS {
            return Err("playlist count exceeds safety limit".into());
        }
        let mut ids = std::collections::HashSet::with_capacity(playlists.len());
        for playlist in &playlists {
            playlist.validate()?;
            if !ids.insert(&playlist.id) {
                return Err("playlist identities must be unique".into());
            }
        }
        Ok(playlists)
    }
    pub fn save(&self, playlists: &[Playlist]) -> Result<(), String> {
        if playlists.len() > MAX_PLAYLISTS {
            return Err("playlist count exceeds safety limit".into());
        }
        let mut ids = std::collections::HashSet::with_capacity(playlists.len());
        for playlist in playlists {
            playlist.validate()?;
            if !ids.insert(&playlist.id) {
                return Err("playlist identities must be unique".into());
            }
        }
        let bytes = serde_json::to_vec_pretty(playlists).map_err(|error| error.to_string())?;
        if bytes.len() as u64 > MAX_STORE_BYTES {
            return Err("playlist store exceeds safety limit".into());
        }
        if let Some(parent) = self.path.parent() {
            fs::create_dir_all(parent).map_err(|error| error.to_string())?;
        }
        let temporary = self
            .path
            .with_extension(format!("tmp-{}", std::process::id()));
        fs::write(&temporary, bytes).map_err(|error| error.to_string())?;
        fs::rename(temporary, &self.path).map_err(|error| error.to_string())
    }
}

impl Playlist {
    pub fn new(id: String, title: String) -> Result<Self, String> {
        if id.is_empty() || id.len() > 128 || title.trim().is_empty() {
            return Err("playlist identity is invalid".into());
        }
        Ok(Self {
            id,
            title: title.chars().take(256).collect(),
            entries: Vec::new(),
            shuffle: false,
            repeat: true,
            duration_seconds: DEFAULT_DURATION_SECONDS,
            transition: PlaylistTransition::None,
            transition_seconds: 0,
        })
    }

    pub fn add(&mut self, workshop_id: String) -> Result<(), String> {
        if workshop_id.is_empty() || workshop_id.len() > 128 {
            return Err("wallpaper ID is invalid".into());
        }
        if self.entries.contains(&workshop_id) {
            return Ok(());
        }
        if self.entries.len() >= MAX_ENTRIES {
            return Err("playlist entry limit reached".into());
        }
        self.entries.push(workshop_id);
        Ok(())
    }

    pub fn set_timing(
        &mut self,
        duration_seconds: u32,
        transition: PlaylistTransition,
        transition_seconds: u8,
    ) -> Result<(), String> {
        if !(MIN_DURATION_SECONDS..=MAX_DURATION_SECONDS).contains(&duration_seconds) {
            return Err("playlist duration is outside the safety bounds".into());
        }
        if transition_seconds > MAX_TRANSITION_SECONDS {
            return Err("playlist transition is outside the safety bounds".into());
        }
        if transition == PlaylistTransition::None && transition_seconds != 0 {
            return Err("disabled transitions must have zero duration".into());
        }
        self.duration_seconds = duration_seconds;
        self.transition = transition;
        self.transition_seconds = transition_seconds;
        Ok(())
    }

    pub fn validate(&self) -> Result<(), String> {
        if self.id.is_empty()
            || self.id.len() > 128
            || self.title.trim().is_empty()
            || self.title.chars().count() > 256
        {
            return Err("playlist identity is invalid".into());
        }
        if self.entries.len() > MAX_ENTRIES {
            return Err("playlist entry limit reached".into());
        }
        let mut seen = std::collections::HashSet::with_capacity(self.entries.len());
        for entry in &self.entries {
            if entry.is_empty() || entry.len() > 128 || !seen.insert(entry) {
                return Err("playlist entries are invalid".into());
            }
        }
        if !(MIN_DURATION_SECONDS..=MAX_DURATION_SECONDS).contains(&self.duration_seconds)
            || self.transition_seconds > MAX_TRANSITION_SECONDS
            || (self.transition == PlaylistTransition::None && self.transition_seconds != 0)
        {
            return Err("playlist timing is invalid".into());
        }
        Ok(())
    }

    pub fn next_index(&self, current: usize, seed: u64) -> Option<usize> {
        self.next_eligible_index(Some(current), seed, &[])
    }

    pub fn next_eligible_index(
        &self,
        current: Option<usize>,
        seed: u64,
        excluded_wallpaper_ids: &[String],
    ) -> Option<usize> {
        if self.entries.is_empty() {
            return None;
        }
        let current = current.filter(|index| *index < self.entries.len());
        let is_eligible = |index: usize| {
            !excluded_wallpaper_ids
                .iter()
                .take(MAX_ENTRIES)
                .any(|excluded| excluded == &self.entries[index])
        };
        if self.shuffle {
            let alternatives: Vec<usize> = (0..self.entries.len())
                .filter(|index| Some(*index) != current && is_eligible(*index))
                .collect();
            if !alternatives.is_empty() {
                let mixed = (seed ^ seed.rotate_left(17)).wrapping_mul(0x9E37_79B9);
                return Some(alternatives[mixed as usize % alternatives.len()]);
            }
            return current.filter(|index| self.repeat && is_eligible(*index));
        }

        let start = current.map_or(0, |index| index.saturating_add(1));
        for offset in 0..self.entries.len() {
            let unwrapped = start.saturating_add(offset);
            if !self.repeat && unwrapped >= self.entries.len() {
                break;
            }
            let index = unwrapped % self.entries.len();
            if is_eligible(index) {
                return Some(index);
            }
        }
        None
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn playlist_selection_is_bounded_and_deterministic() {
        let mut playlist = Playlist::new("main".into(), "Main".into()).unwrap();
        playlist.add("431960-1".into()).unwrap();
        playlist.add("431960-2".into()).unwrap();
        assert_eq!(playlist.next_index(0, 1), Some(1));
        playlist.shuffle = true;
        assert_eq!(playlist.next_index(0, 99), playlist.next_index(0, 99));
        assert_ne!(playlist.next_index(0, 99), Some(0));
    }

    #[test]
    fn playlist_store_round_trips_and_rejects_corruption() {
        let path = std::env::temp_dir().join(format!("kwe-playlists-{}.json", std::process::id()));
        let _ = fs::remove_file(&path);
        let mut playlist = Playlist::new("main".into(), "Main".into()).unwrap();
        playlist.add("431960-1".into()).unwrap();
        let store = PlaylistStore::new(path.clone());
        store.save(&[playlist.clone()]).unwrap();
        assert_eq!(store.load().unwrap(), vec![playlist]);
        fs::write(&path, b"not-json").unwrap();
        assert!(store.load().is_err());
        let _ = fs::remove_file(path);
    }

    #[test]
    fn older_playlist_data_gets_safe_timing_defaults() {
        let playlist: Playlist = serde_json::from_str(
            r#"{"id":"main","title":"Main","entries":[],"shuffle":false,"repeat":true}"#,
        )
        .unwrap();
        assert_eq!(playlist.duration_seconds, DEFAULT_DURATION_SECONDS);
        assert_eq!(playlist.transition, PlaylistTransition::None);
        assert_eq!(playlist.transition_seconds, 0);
        playlist.validate().unwrap();
    }

    #[test]
    fn timing_and_loaded_data_are_bounded() {
        let mut playlist = Playlist::new("main".into(), "Main".into()).unwrap();
        assert!(playlist.set_timing(9, PlaylistTransition::None, 0).is_err());
        assert!(
            playlist
                .set_timing(300, PlaylistTransition::Crossfade, 11)
                .is_err()
        );
        playlist
            .set_timing(600, PlaylistTransition::Crossfade, 3)
            .unwrap();
        assert_eq!(playlist.duration_seconds, 600);
        assert_eq!(playlist.transition_seconds, 3);

        playlist.entries = vec!["duplicate".into(), "duplicate".into()];
        assert!(playlist.validate().is_err());

        let original = Playlist::new("main".into(), "Main".into()).unwrap();
        let duplicate = Playlist::new("main".into(), "Other".into()).unwrap();
        let path = std::env::temp_dir().join(format!(
            "kwe-playlist-duplicates-{}.json",
            std::process::id()
        ));
        let store = PlaylistStore::new(path.clone());
        assert!(store.save(&[original, duplicate]).is_err());
        let _ = fs::remove_file(path);
    }

    #[test]
    fn selection_skips_unavailable_items_without_unbounded_retries() {
        let mut playlist = Playlist::new("main".into(), "Main".into()).unwrap();
        for id in ["one", "two", "three"] {
            playlist.add(id.into()).unwrap();
        }
        let unavailable = vec!["two".to_string()];
        assert_eq!(
            playlist.next_eligible_index(Some(0), 7, &unavailable),
            Some(2)
        );
        assert_eq!(
            playlist.next_eligible_index(Some(0), 7, &["one".into(), "two".into(), "three".into()]),
            None
        );

        playlist.shuffle = true;
        let selected = playlist.next_eligible_index(Some(0), 91, &unavailable);
        assert_eq!(selected, Some(2));
        assert_eq!(
            selected,
            playlist.next_eligible_index(Some(0), 91, &unavailable)
        );
    }
}
