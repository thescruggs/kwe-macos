// SPDX-License-Identifier: GPL-3.0-or-later
use std::collections::VecDeque;

use serde::{Deserialize, Serialize};

use crate::Playlist;

const MAX_RUNTIME_ENTRIES: usize = 1024;
const SNAPSHOT_SCHEMA_VERSION: u32 = 1;
/// Longest legal remaining time: the maximum playlist duration in
/// milliseconds. Anything larger is a corrupt or hostile snapshot.
const MAX_SNAPSHOT_REMAINING_MS: u64 = 24 * 60 * 60 * 1000;

/// Persistent form of [`PlaylistRuntime`] state. Only durations are stored —
/// never absolute deadlines — so a snapshot restored after a daemon restart
/// (or any monotonic-clock re-anchor) resumes with the same remaining time.
/// The wall-clock position itself is never serialized.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct PlaylistRuntimeSnapshot {
    pub schema_version: u32,
    pub playlist_id: String,
    pub seed: u64,
    pub current_index: Option<usize>,
    pub current_wallpaper_id: Option<String>,
    /// Remaining milliseconds on the current entry while playing.
    pub remaining_ms: Option<u64>,
    /// Remaining milliseconds while paused; mutually exclusive with
    /// `remaining_ms`.
    pub paused_remaining_ms: Option<u64>,
    pub history: Vec<String>,
}

impl PlaylistRuntimeSnapshot {
    /// Fails closed on any malformed, oversized, or inconsistent state.
    pub fn validate(&self) -> Result<(), String> {
        if self.schema_version != SNAPSHOT_SCHEMA_VERSION {
            return Err("playlist runtime snapshot has an unsupported schema".into());
        }
        if self.playlist_id.is_empty() || self.playlist_id.len() > 128 {
            return Err("playlist runtime snapshot identity is invalid".into());
        }
        if self.history.len() > MAX_RUNTIME_ENTRIES
            || self
                .history
                .iter()
                .any(|entry| entry.is_empty() || entry.len() > 128)
        {
            return Err("playlist runtime snapshot history exceeds safety bounds".into());
        }
        match (self.current_index, self.current_wallpaper_id.as_ref()) {
            (None, None) => {}
            (Some(index), Some(id)) => {
                if index >= MAX_RUNTIME_ENTRIES || id.is_empty() || id.len() > 128 {
                    return Err("playlist runtime snapshot position is invalid".into());
                }
            }
            _ => return Err("playlist runtime snapshot position is inconsistent".into()),
        }
        let has_timing = self.remaining_ms.is_some() || self.paused_remaining_ms.is_some();
        if (self.current_index.is_some()
            && self.remaining_ms.is_some() == self.paused_remaining_ms.is_some())
            || (self.current_index.is_none() && has_timing)
        {
            return Err("playlist runtime snapshot timing is inconsistent".into());
        }
        if self
            .remaining_ms
            .into_iter()
            .chain(self.paused_remaining_ms)
            .any(|remaining| remaining > MAX_SNAPSHOT_REMAINING_MS)
        {
            return Err("playlist runtime snapshot timing exceeds safety bounds".into());
        }
        Ok(())
    }
}

#[derive(Debug, Clone, Serialize, PartialEq, Eq)]
#[serde(tag = "state", rename_all = "snake_case")]
pub enum PlaylistDecision {
    Started {
        wallpaper_id: String,
        index: usize,
        deadline_ms: u64,
    },
    Waiting {
        wallpaper_id: String,
        index: usize,
        remaining_ms: u64,
    },
    Advanced {
        wallpaper_id: String,
        index: usize,
        deadline_ms: u64,
    },
    Paused {
        wallpaper_id: String,
        index: usize,
        remaining_ms: u64,
    },
    Exhausted,
    NoEligible,
}

#[derive(Debug, Clone)]
pub struct PlaylistRuntime {
    current_index: Option<usize>,
    current_wallpaper_id: Option<String>,
    deadline_ms: Option<u64>,
    paused_remaining_ms: Option<u64>,
    last_now_ms: Option<u64>,
    seed: u64,
    history: VecDeque<String>,
}

impl PlaylistRuntime {
    pub fn new(seed: u64) -> Self {
        Self {
            current_index: None,
            current_wallpaper_id: None,
            deadline_ms: None,
            paused_remaining_ms: None,
            last_now_ms: None,
            seed,
            history: VecDeque::new(),
        }
    }

    pub fn start(
        &mut self,
        playlist: &Playlist,
        now_ms: u64,
        unavailable: &[String],
    ) -> Result<PlaylistDecision, String> {
        playlist.validate()?;
        validate_unavailable(unavailable)?;
        self.current_index = None;
        self.current_wallpaper_id = None;
        self.deadline_ms = None;
        self.paused_remaining_ms = None;
        self.last_now_ms = Some(now_ms);
        self.history.clear();
        self.select(playlist, now_ms, unavailable, true)
    }

    pub fn tick(
        &mut self,
        playlist: &Playlist,
        now_ms: u64,
        unavailable: &[String],
    ) -> Result<PlaylistDecision, String> {
        playlist.validate()?;
        validate_unavailable(unavailable)?;
        self.observe_time(now_ms)?;
        self.reconcile_current(playlist);

        if let (Some(index), Some(remaining_ms)) = (self.current_index, self.paused_remaining_ms) {
            return Ok(PlaylistDecision::Paused {
                wallpaper_id: playlist.entries[index].clone(),
                index,
                remaining_ms,
            });
        }

        let current_unavailable = self.current_index.is_some_and(|index| {
            unavailable
                .iter()
                .any(|entry| entry == &playlist.entries[index])
        });
        if let (Some(index), Some(deadline_ms)) = (self.current_index, self.deadline_ms)
            && now_ms < deadline_ms
            && !current_unavailable
        {
            return Ok(PlaylistDecision::Waiting {
                wallpaper_id: playlist.entries[index].clone(),
                index,
                remaining_ms: deadline_ms - now_ms,
            });
        }

        self.select(playlist, now_ms, unavailable, false)
    }

    pub fn pause(&mut self, playlist: &Playlist, now_ms: u64) -> Result<PlaylistDecision, String> {
        playlist.validate()?;
        self.observe_time(now_ms)?;
        self.reconcile_current(playlist);
        let Some(index) = self.current_index else {
            return Ok(self.empty_decision(playlist));
        };
        let remaining_ms = self
            .paused_remaining_ms
            .unwrap_or_else(|| self.deadline_ms.unwrap_or(now_ms).saturating_sub(now_ms));
        self.paused_remaining_ms = Some(remaining_ms);
        Ok(PlaylistDecision::Paused {
            wallpaper_id: playlist.entries[index].clone(),
            index,
            remaining_ms,
        })
    }

    pub fn resume(&mut self, playlist: &Playlist, now_ms: u64) -> Result<PlaylistDecision, String> {
        playlist.validate()?;
        self.observe_time(now_ms)?;
        self.reconcile_current(playlist);
        let Some(index) = self.current_index else {
            return Ok(self.empty_decision(playlist));
        };
        if let Some(remaining_ms) = self.paused_remaining_ms.take() {
            self.deadline_ms = Some(now_ms.saturating_add(remaining_ms));
        }
        let deadline_ms = self.deadline_ms.unwrap_or(now_ms);
        Ok(PlaylistDecision::Waiting {
            wallpaper_id: playlist.entries[index].clone(),
            index,
            remaining_ms: deadline_ms.saturating_sub(now_ms),
        })
    }

    pub fn current_index(&self) -> Option<usize> {
        self.current_index
    }

    pub fn history(&self) -> Vec<String> {
        self.history.iter().cloned().collect()
    }

    /// Captures restorable state. `now_ms` must not regress against the last
    /// observed time; only durations are stored, never absolute deadlines.
    pub fn snapshot(
        &self,
        playlist: &Playlist,
        now_ms: u64,
    ) -> Result<PlaylistRuntimeSnapshot, String> {
        playlist.validate()?;
        if self.last_now_ms.is_some_and(|previous| now_ms < previous) {
            return Err("playlist monotonic time regressed".into());
        }
        let (remaining_ms, paused_remaining_ms) = if self.paused_remaining_ms.is_some() {
            (None, self.paused_remaining_ms)
        } else if self.current_index.is_some() {
            (
                self.deadline_ms
                    .map(|deadline| deadline.saturating_sub(now_ms)),
                None,
            )
        } else {
            (None, None)
        };
        let snapshot = PlaylistRuntimeSnapshot {
            schema_version: SNAPSHOT_SCHEMA_VERSION,
            playlist_id: playlist.id.clone(),
            seed: self.seed,
            current_index: self.current_index,
            current_wallpaper_id: self.current_wallpaper_id.clone(),
            remaining_ms,
            paused_remaining_ms,
            history: self.history.iter().cloned().collect(),
        };
        snapshot.validate()?;
        Ok(snapshot)
    }

    /// Restores a previously captured snapshot and re-anchors any remaining
    /// duration against `now_ms`. The restored position is reconciled against
    /// the supplied playlist, so an entry that no longer exists is cleared
    /// exactly as it would be after an in-process playlist mutation.
    pub fn restore(
        &mut self,
        snapshot: &PlaylistRuntimeSnapshot,
        playlist: &Playlist,
        now_ms: u64,
    ) -> Result<(), String> {
        snapshot.validate()?;
        playlist.validate()?;
        if snapshot.playlist_id != playlist.id {
            return Err("playlist runtime snapshot belongs to another playlist".into());
        }
        self.seed = snapshot.seed;
        self.history = snapshot.history.iter().cloned().collect();
        self.current_index = snapshot.current_index;
        self.current_wallpaper_id = snapshot.current_wallpaper_id.clone();
        self.deadline_ms = snapshot
            .remaining_ms
            .map(|remaining| now_ms.saturating_add(remaining));
        self.paused_remaining_ms = snapshot.paused_remaining_ms;
        self.last_now_ms = Some(now_ms);
        self.reconcile_current(playlist);
        Ok(())
    }

    fn select(
        &mut self,
        playlist: &Playlist,
        now_ms: u64,
        unavailable: &[String],
        starting: bool,
    ) -> Result<PlaylistDecision, String> {
        let exclusions: Vec<String> = playlist
            .entries
            .iter()
            .filter(|entry| {
                unavailable.iter().any(|unavailable| unavailable == *entry)
                    || (!playlist.repeat && self.history.iter().any(|seen| seen == *entry))
            })
            .cloned()
            .collect();
        let selected = playlist.next_eligible_index(self.current_index, self.seed, &exclusions);
        let Some(index) = selected else {
            self.current_index = None;
            self.current_wallpaper_id = None;
            self.deadline_ms = None;
            self.paused_remaining_ms = None;
            return Ok(self.empty_decision(playlist));
        };

        self.seed = self.seed.wrapping_add(1);
        self.current_index = Some(index);
        self.current_wallpaper_id = Some(playlist.entries[index].clone());
        let deadline_ms = now_ms.saturating_add(u64::from(playlist.duration_seconds) * 1000);
        self.deadline_ms = Some(deadline_ms);
        self.paused_remaining_ms = None;
        self.history.push_back(playlist.entries[index].clone());
        while self.history.len() > MAX_RUNTIME_ENTRIES {
            self.history.pop_front();
        }

        if starting {
            Ok(PlaylistDecision::Started {
                wallpaper_id: playlist.entries[index].clone(),
                index,
                deadline_ms,
            })
        } else {
            Ok(PlaylistDecision::Advanced {
                wallpaper_id: playlist.entries[index].clone(),
                index,
                deadline_ms,
            })
        }
    }

    fn empty_decision(&self, playlist: &Playlist) -> PlaylistDecision {
        if !playlist.repeat && !self.history.is_empty() {
            PlaylistDecision::Exhausted
        } else {
            PlaylistDecision::NoEligible
        }
    }

    fn observe_time(&mut self, now_ms: u64) -> Result<(), String> {
        if self.last_now_ms.is_some_and(|previous| now_ms < previous) {
            return Err("playlist monotonic time regressed".into());
        }
        self.last_now_ms = Some(now_ms);
        Ok(())
    }

    fn reconcile_current(&mut self, playlist: &Playlist) {
        let Some(current_id) = self.current_wallpaper_id.as_ref() else {
            self.current_index = None;
            return;
        };
        if self
            .current_index
            .is_some_and(|index| playlist.entries.get(index) == Some(current_id))
        {
            return;
        }
        self.current_index = playlist
            .entries
            .iter()
            .position(|entry| entry == current_id);
        if self.current_index.is_none() {
            self.current_wallpaper_id = None;
            self.deadline_ms = None;
            self.paused_remaining_ms = None;
        }
    }
}

fn validate_unavailable(unavailable: &[String]) -> Result<(), String> {
    if unavailable.len() > MAX_RUNTIME_ENTRIES
        || unavailable
            .iter()
            .any(|entry| entry.is_empty() || entry.len() > 128)
    {
        return Err("unavailable playlist entries exceed safety bounds".into());
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn playlist(repeat: bool, shuffle: bool) -> Playlist {
        let mut playlist = Playlist::new("daily".into(), "Daily".into()).unwrap();
        for id in ["one", "two", "three"] {
            playlist.add(id.into()).unwrap();
        }
        playlist.repeat = repeat;
        playlist.shuffle = shuffle;
        playlist.duration_seconds = 10;
        playlist
    }

    #[test]
    fn advances_on_monotonic_deadlines_and_rejects_regression() {
        let playlist = playlist(true, false);
        let mut runtime = PlaylistRuntime::new(7);
        assert!(matches!(
            runtime.start(&playlist, 1_000, &[]).unwrap(),
            PlaylistDecision::Started { index: 0, .. }
        ));
        assert_eq!(
            runtime.tick(&playlist, 5_000, &[]).unwrap(),
            PlaylistDecision::Waiting {
                wallpaper_id: "one".into(),
                index: 0,
                remaining_ms: 6_000,
            }
        );
        assert!(matches!(
            runtime.tick(&playlist, 11_000, &[]).unwrap(),
            PlaylistDecision::Advanced { index: 1, .. }
        ));
        assert!(runtime.tick(&playlist, 10_999, &[]).is_err());
        assert_eq!(runtime.current_index(), Some(1));
    }

    #[test]
    fn pause_freezes_remaining_time() {
        let playlist = playlist(true, false);
        let mut runtime = PlaylistRuntime::new(1);
        runtime.start(&playlist, 0, &[]).unwrap();
        assert!(matches!(
            runtime.pause(&playlist, 4_000).unwrap(),
            PlaylistDecision::Paused {
                remaining_ms: 6_000,
                ..
            }
        ));
        assert!(matches!(
            runtime.tick(&playlist, 20_000, &[]).unwrap(),
            PlaylistDecision::Paused {
                remaining_ms: 6_000,
                ..
            }
        ));
        assert_eq!(
            runtime.resume(&playlist, 20_000).unwrap(),
            PlaylistDecision::Waiting {
                wallpaper_id: "one".into(),
                index: 0,
                remaining_ms: 6_000,
            }
        );
        assert!(matches!(
            runtime.tick(&playlist, 26_000, &[]).unwrap(),
            PlaylistDecision::Advanced { index: 1, .. }
        ));
    }

    #[test]
    fn non_repeat_shuffle_has_bounded_history_and_exhausts() {
        let playlist = playlist(false, true);
        let mut runtime = PlaylistRuntime::new(42);
        runtime.start(&playlist, 0, &[]).unwrap();
        runtime.tick(&playlist, 10_000, &[]).unwrap();
        runtime.tick(&playlist, 20_000, &[]).unwrap();
        assert_eq!(
            runtime.tick(&playlist, 30_000, &[]).unwrap(),
            PlaylistDecision::Exhausted
        );
        let history = runtime.history();
        assert_eq!(history.len(), 3);
        let mut unique = history.clone();
        unique.sort();
        unique.dedup();
        assert_eq!(unique.len(), 3);
    }

    #[test]
    fn skips_newly_unavailable_and_recovers_returning_unplayed_item() {
        let playlist = playlist(false, false);
        let mut runtime = PlaylistRuntime::new(9);
        runtime.start(&playlist, 0, &[]).unwrap();
        assert!(matches!(
            runtime
                .tick(&playlist, 1_000, &["one".into(), "two".into()])
                .unwrap(),
            PlaylistDecision::Advanced { index: 2, .. }
        ));
        assert_eq!(
            runtime.tick(&playlist, 11_000, &["two".into()]).unwrap(),
            PlaylistDecision::Exhausted
        );
        assert!(matches!(
            runtime.tick(&playlist, 12_000, &[]).unwrap(),
            PlaylistDecision::Advanced { index: 1, .. }
        ));
    }

    #[test]
    fn no_eligible_and_oversized_inputs_are_explicit() {
        let playlist = playlist(true, false);
        let mut runtime = PlaylistRuntime::new(0);
        assert_eq!(
            runtime
                .start(&playlist, 0, &["one".into(), "two".into(), "three".into()])
                .unwrap(),
            PlaylistDecision::NoEligible
        );
        let oversized = vec!["missing".to_string(); MAX_RUNTIME_ENTRIES + 1];
        assert!(runtime.tick(&playlist, 1, &oversized).is_err());
    }

    #[test]
    fn playlist_reorder_or_removal_cannot_make_runtime_index_out_of_bounds() {
        let mut playlist = playlist(true, false);
        let mut runtime = PlaylistRuntime::new(0);
        runtime.start(&playlist, 0, &[]).unwrap();
        playlist.entries.swap(0, 2);
        assert!(matches!(
            runtime.tick(&playlist, 1, &[]).unwrap(),
            PlaylistDecision::Waiting { index: 2, .. }
        ));
        playlist.entries.remove(2);
        assert!(matches!(
            runtime.tick(&playlist, 2, &[]).unwrap(),
            PlaylistDecision::Advanced { .. }
        ));
    }

    #[test]
    fn snapshot_round_trip_reanchors_remaining_time_after_restart() {
        let playlist = playlist(true, false);
        let mut original = PlaylistRuntime::new(7);
        original.start(&playlist, 1_000, &[]).unwrap();
        let snapshot = original.snapshot(&playlist, 4_000).unwrap();

        // "Restart" happens at a different monotonic origin (e.g. 1_000_000).
        let mut restored = PlaylistRuntime::new(0);
        restored.restore(&snapshot, &playlist, 1_000_000).unwrap();
        assert_eq!(restored.current_index(), Some(0));
        assert_eq!(
            restored.tick(&playlist, 1_003_000, &[]).unwrap(),
            PlaylistDecision::Waiting {
                wallpaper_id: "one".into(),
                index: 0,
                remaining_ms: 4_000,
            }
        );
        assert!(matches!(
            restored.tick(&playlist, 1_008_000, &[]).unwrap(),
            PlaylistDecision::Advanced { index: 1, .. }
        ));
    }

    #[test]
    fn paused_snapshot_round_trip_freezes_remaining_time() {
        let playlist = playlist(true, false);
        let mut original = PlaylistRuntime::new(3);
        original.start(&playlist, 0, &[]).unwrap();
        original.pause(&playlist, 2_000).unwrap();
        let snapshot = original.snapshot(&playlist, 9_000).unwrap();
        assert_eq!(snapshot.paused_remaining_ms, Some(8_000));
        assert_eq!(snapshot.remaining_ms, None);

        let mut restored = PlaylistRuntime::new(0);
        restored.restore(&snapshot, &playlist, 50_000).unwrap();
        assert_eq!(
            restored.tick(&playlist, 60_000, &[]).unwrap(),
            PlaylistDecision::Paused {
                wallpaper_id: "one".into(),
                index: 0,
                remaining_ms: 8_000,
            }
        );
    }

    #[test]
    fn snapshot_preserves_seed_history_and_exhaustion() {
        let playlist = playlist(false, true);
        let mut original = PlaylistRuntime::new(42);
        original.start(&playlist, 0, &[]).unwrap();
        original.tick(&playlist, 10_000, &[]).unwrap();
        original.tick(&playlist, 20_000, &[]).unwrap();
        assert_eq!(
            original.tick(&playlist, 30_000, &[]).unwrap(),
            PlaylistDecision::Exhausted
        );
        let snapshot = original.snapshot(&playlist, 30_000).unwrap();
        assert_eq!(snapshot.current_index, None);
        assert_eq!(snapshot.history.len(), 3);

        let mut restored = PlaylistRuntime::new(0);
        restored.restore(&snapshot, &playlist, 100_000).unwrap();
        // Exhausted (not NoEligible) must survive: history came back intact.
        assert_eq!(
            restored.tick(&playlist, 100_000, &[]).unwrap(),
            PlaylistDecision::Exhausted
        );

        // Shuffle determinism depends on the persisted seed: after a restore,
        // the next selection must match what the un-restarted runtime picks.
        let repeat_playlist = self::playlist(true, true);
        let mut original = PlaylistRuntime::new(42);
        original.start(&repeat_playlist, 0, &[]).unwrap();
        let snapshot = original.snapshot(&repeat_playlist, 1_000).unwrap();
        let mut restored = PlaylistRuntime::new(0);
        restored
            .restore(&snapshot, &repeat_playlist, 100_000)
            .unwrap();
        // Deadlines differ across a restart (different monotonic origins),
        // so compare the selection only.
        let restored_decision = restored.tick(&repeat_playlist, 110_000, &[]).unwrap();
        let original_decision = original.tick(&repeat_playlist, 10_000, &[]).unwrap();
        match (&restored_decision, &original_decision) {
            (
                PlaylistDecision::Advanced {
                    wallpaper_id: restored_id,
                    index: restored_index,
                    ..
                },
                PlaylistDecision::Advanced {
                    wallpaper_id: original_id,
                    index: original_index,
                    ..
                },
            ) => assert_eq!((restored_id, restored_index), (original_id, original_index)),
            other => panic!("expected Advanced decisions, got {other:?}"),
        }
    }

    #[test]
    fn restore_rejects_malformed_snapshots() {
        let playlist = playlist(true, false);
        let mut runtime = PlaylistRuntime::new(1);
        runtime.start(&playlist, 0, &[]).unwrap();
        let mut snapshot = runtime.snapshot(&playlist, 1_000).unwrap();

        snapshot.schema_version = 2;
        assert!(runtime.restore(&snapshot, &playlist, 0).is_err());
        snapshot.schema_version = 1;

        snapshot.playlist_id = "other".into();
        assert!(runtime.restore(&snapshot, &playlist, 0).is_err());
        snapshot.playlist_id = "daily".into();

        snapshot.remaining_ms = Some(1);
        snapshot.paused_remaining_ms = Some(1);
        assert!(runtime.restore(&snapshot, &playlist, 0).is_err());
        snapshot.remaining_ms = Some(10_000);
        snapshot.paused_remaining_ms = None;

        snapshot.current_index = None;
        assert!(runtime.restore(&snapshot, &playlist, 0).is_err());
        snapshot.current_index = Some(0);

        snapshot.remaining_ms = Some(24 * 60 * 60 * 1000 + 1);
        assert!(runtime.restore(&snapshot, &playlist, 0).is_err());
        snapshot.remaining_ms = Some(10_000);

        snapshot.history = vec!["oversized".repeat(129)];
        assert!(runtime.restore(&snapshot, &playlist, 0).is_err());
        snapshot.history.clear();

        runtime.restore(&snapshot, &playlist, 0).unwrap();
    }

    #[test]
    fn forward_clock_jump_advances_deterministically() {
        // Sleep/resume: the monotonic clock jumps far forward; the stale
        // deadline must simply expire and advance, never error or loop.
        let playlist = playlist(true, false);
        let mut runtime = PlaylistRuntime::new(0);
        runtime.start(&playlist, 0, &[]).unwrap();
        assert!(matches!(
            runtime.tick(&playlist, 1_000_000_000, &[]).unwrap(),
            PlaylistDecision::Advanced { index: 1, .. }
        ));
        assert!(matches!(
            runtime.tick(&playlist, 1_000_001_000, &[]).unwrap(),
            PlaylistDecision::Waiting { index: 1, .. }
        ));
    }

    #[test]
    fn restore_reconciles_against_mutated_playlist() {
        let mut playlist = playlist(true, false);
        let mut original = PlaylistRuntime::new(5);
        original.start(&playlist, 0, &[]).unwrap();
        let snapshot = original.snapshot(&playlist, 1_000).unwrap();

        // The snapshot's entry vanished while the daemon was down.
        playlist.entries.remove(0);
        let mut restored = PlaylistRuntime::new(0);
        restored.restore(&snapshot, &playlist, 10_000).unwrap();
        assert_eq!(restored.current_index(), None);
        assert!(matches!(
            restored.tick(&playlist, 10_000, &[]).unwrap(),
            PlaylistDecision::Advanced { .. }
        ));
    }
}
