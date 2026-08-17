// SPDX-License-Identifier: Apache-2.0
//! Daemon-owned playlist session.
//!
//! The manager edits playlist definitions through the daemon API; this module
//! owns the bounded store, the monotonic pause-aware runtime, and transactional
//! runtime-state persistence. Only durations are persisted — never absolute
//! deadlines — so a restart (or any monotonic-clock re-anchor) resumes with
//! the same remaining time. The pure runtime lives in `kwe-core`; this module
//! supplies the clock, the unavailable set, and the persistence boundary.
//!
//! Safety posture: playlist state is not renderer-safety-critical. A corrupt
//! definitions store disables playlist methods but keeps the daemon serving;
//! a corrupt runtime-state file is quarantined (renamed `.invalid-*`) and the
//! session restarts fresh.

use std::{
    collections::{BTreeMap, BTreeSet},
    path::PathBuf,
    sync::{
        Arc,
        mpsc::{self, Receiver, SyncSender, TrySendError},
    },
    thread::{self, JoinHandle},
    time::{Duration, Instant},
};

use kwe_core::{
    Playlist, PlaylistDecision, PlaylistRuntime, PlaylistRuntimeSnapshot, PlaylistStore,
    PlaylistTransition,
};
use serde::{Deserialize, Serialize};

use crate::{
    persist::{atomic_write, quarantine_invalid_state},
    supervisor::SupervisorHandle,
};

const COMMAND_CAPACITY: usize = 16;
const COMMAND_TIMEOUT: Duration = Duration::from_secs(5);
const QUARANTINE_QUERY_TIMEOUT: Duration = Duration::from_millis(250);
/// How often a `Waiting` decision is re-persisted (transitions persist
/// immediately).
const WAITING_PERSIST_INTERVAL: Duration = Duration::from_secs(30);
const MAX_RUNTIME_STATE_BYTES: u64 = 4 * 1024 * 1024;
const MAX_SNAPSHOT_ENTRIES: usize = 256;
const MAX_CLOCK_SKIP_MS: u64 = 60 * 60 * 1000;
const RUNTIME_STATE_SCHEMA_VERSION: u32 = 1;
const RUNTIME_STATE_FILE: &str = "playlist-runtime-v1.json";
const DEFINITIONS_FILE: &str = "playlists-v1.json";

#[derive(Debug, Clone)]
pub struct PlaylistSessionConfig {
    pub state_dir: PathBuf,
    pub tick_ms: u64,
    pub supervisor: Option<SupervisorHandle>,
    /// Catalog-derived ids that are installed and usable. Pushed by `main`
    /// at startup and refreshed after every rescan.
    pub valid_ids: Arc<BTreeSet<String>>,
}

#[derive(Debug, Clone, Serialize)]
pub struct DefinitionsHealth {
    pub count: usize,
    pub store_health: String,
}

#[derive(Debug, Clone, Serialize)]
pub struct SessionStatus {
    pub active: bool,
    pub playlist_id: Option<String>,
    pub decision: Option<PlaylistDecision>,
    pub unavailable_ids: Vec<String>,
    pub definitions: DefinitionsHealth,
    pub clock_skipped_ms: u64,
}

#[derive(Debug, Clone, Copy, Serialize)]
pub struct ImportSummary {
    pub imported: usize,
    pub rejected: usize,
}

/// Errors that map onto stable protocol error names in `main.rs`.
#[derive(Debug)]
pub enum SessionError {
    NotFound(String),
    ImportBlocked,
    StoreUnavailable(String),
    Invalid(String),
    Busy(String),
}

impl std::fmt::Display for SessionError {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            SessionError::NotFound(detail) => write!(formatter, "not found: {detail}"),
            SessionError::ImportBlocked => write!(formatter, "playlist store is not empty"),
            SessionError::StoreUnavailable(detail) => write!(formatter, "store: {detail}"),
            SessionError::Invalid(detail) => write!(formatter, "invalid: {detail}"),
            SessionError::Busy(detail) => write!(formatter, "session busy: {detail}"),
        }
    }
}

/// Legacy manager playlist without an `id`; the session derives a bounded id
/// from the title during import.
#[derive(Debug, Clone, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ImportPlaylist {
    title: String,
    entries: Vec<String>,
    #[serde(default)]
    shuffle: bool,
    #[serde(default = "default_repeat")]
    repeat: bool,
    #[serde(default = "default_duration_seconds")]
    duration_seconds: u32,
    #[serde(default = "default_transition")]
    transition: String,
    #[serde(default)]
    transition_seconds: u8,
}

fn default_repeat() -> bool {
    true
}

fn default_duration_seconds() -> u32 {
    300
}

fn default_transition() -> String {
    "none".into()
}

impl ImportPlaylist {
    fn into_playlist(self, id: String) -> Result<Playlist, String> {
        let transition = match self.transition.as_str() {
            "none" => PlaylistTransition::None,
            "crossfade" => PlaylistTransition::Crossfade,
            other => return Err(format!("unknown transition {other}")),
        };
        let mut playlist = Playlist::new(id, self.title)?;
        playlist.entries = self.entries;
        playlist.shuffle = self.shuffle;
        playlist.repeat = self.repeat;
        playlist.set_timing(self.duration_seconds, transition, self.transition_seconds)?;
        playlist.validate()?;
        Ok(playlist)
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
struct PersistedSessionState {
    schema_version: u32,
    active_playlist_id: Option<String>,
    snapshots: BTreeMap<String, PlaylistRuntimeSnapshot>,
}

enum SessionCommand {
    List(mpsc::Sender<Result<Vec<Playlist>, SessionError>>),
    Put(Playlist, mpsc::Sender<Result<Playlist, SessionError>>),
    Remove(String, mpsc::Sender<Result<String, SessionError>>),
    Activate(
        Option<String>,
        mpsc::Sender<Result<SessionStatus, SessionError>>,
    ),
    Status(mpsc::Sender<Result<SessionStatus, SessionError>>),
    Import(
        Vec<ImportPlaylist>,
        mpsc::Sender<Result<ImportSummary, SessionError>>,
    ),
    DebugClockSkip(u64, mpsc::Sender<Result<SessionStatus, SessionError>>),
    /// Replaces the catalog-derived availability inputs (startup and rescan).
    Availability {
        valid_ids: Arc<BTreeSet<String>>,
    },
    /// Persists final state and acknowledges so a caller can bound its join.
    Shutdown(mpsc::Sender<()>),
}

#[derive(Clone)]
pub struct PlaylistSessionHandle {
    sender: SyncSender<SessionCommand>,
}

impl PlaylistSessionHandle {
    fn request<T>(
        &self,
        make: impl FnOnce(mpsc::Sender<Result<T, SessionError>>) -> SessionCommand,
    ) -> Result<T, SessionError> {
        let (sender, receiver) = mpsc::channel();
        match self.sender.try_send(make(sender)) {
            Ok(()) => {}
            Err(TrySendError::Full(_)) => {
                return Err(SessionError::Busy("playlist command queue is full".into()));
            }
            Err(TrySendError::Disconnected(_)) => {
                return Err(SessionError::Busy("playlist session is unavailable".into()));
            }
        }
        receiver
            .recv_timeout(COMMAND_TIMEOUT)
            .map_err(|_| SessionError::Busy("playlist command timed out".into()))?
    }

    pub fn list(&self) -> Result<Vec<Playlist>, SessionError> {
        self.request(SessionCommand::List)
    }

    pub fn put(&self, playlist: Playlist) -> Result<Playlist, SessionError> {
        self.request(|reply| SessionCommand::Put(playlist, reply))
    }

    pub fn remove(&self, id: String) -> Result<String, SessionError> {
        self.request(|reply| SessionCommand::Remove(id, reply))
    }

    pub fn activate(&self, id: Option<String>) -> Result<SessionStatus, SessionError> {
        self.request(|reply| SessionCommand::Activate(id, reply))
    }

    pub fn status(&self) -> Result<SessionStatus, SessionError> {
        self.request(SessionCommand::Status)
    }

    pub fn import(&self, playlists: Vec<ImportPlaylist>) -> Result<ImportSummary, SessionError> {
        self.request(|reply| SessionCommand::Import(playlists, reply))
    }

    pub fn debug_clock_skip(&self, ms: u64) -> Result<SessionStatus, SessionError> {
        self.request(|reply| SessionCommand::DebugClockSkip(ms, reply))
    }

    pub fn update_availability(&self, valid_ids: Arc<BTreeSet<String>>) -> bool {
        matches!(
            self.sender
                .try_send(SessionCommand::Availability { valid_ids }),
            Ok(())
        )
    }
}

struct SessionRuntime {
    store: PlaylistStore,
    state_path: PathBuf,
    tick_duration: Duration,
    supervisor: Option<SupervisorHandle>,
    playlists: Vec<Playlist>,
    store_error: Option<String>,
    runtimes: BTreeMap<String, PlaylistRuntime>,
    snapshots: BTreeMap<String, PlaylistRuntimeSnapshot>,
    active: Option<String>,
    last_decision: Option<PlaylistDecision>,
    last_waiting_persist: Instant,
    valid_ids: Arc<BTreeSet<String>>,
    quarantined_ids: BTreeSet<String>,
    clock_offset_ms: u64,
    clock_skipped_ms: u64,
    start_instant: Instant,
}

impl SessionRuntime {
    fn new(config: PlaylistSessionConfig) -> Self {
        let state_dir = config.state_dir;
        let store = PlaylistStore::new(state_dir.join(DEFINITIONS_FILE));
        let (playlists, store_error) = match store.load() {
            Ok(playlists) => (playlists, None),
            Err(error) => {
                eprintln!("event=playlist.store_load_error detail={error}");
                (Vec::new(), Some(error))
            }
        };
        let (persisted_active, snapshots) = load_runtime_state(&state_dir.join(RUNTIME_STATE_FILE));
        let active = persisted_active.and_then(|id| {
            playlists
                .iter()
                .find(|playlist| playlist.id == id)
                .map(|playlist| playlist.id.clone())
        });
        let mut runtime = Self {
            store,
            state_path: state_dir.join(RUNTIME_STATE_FILE),
            tick_duration: Duration::from_millis(config.tick_ms),
            supervisor: config.supervisor,
            playlists,
            store_error,
            runtimes: BTreeMap::new(),
            snapshots,
            active,
            last_decision: None,
            last_waiting_persist: Instant::now(),
            valid_ids: config.valid_ids,
            quarantined_ids: BTreeSet::new(),
            clock_offset_ms: 0,
            clock_skipped_ms: 0,
            start_instant: Instant::now(),
        };
        // Rehydrate the active session so the first tick continues where the
        // previous daemon instance left off instead of advancing blindly.
        if let Some(active_id) = runtime.active.clone() {
            let now_ms = runtime.now_ms();
            let restored = {
                let Some(playlist) = runtime
                    .playlists
                    .iter()
                    .find(|playlist| playlist.id == active_id)
                    .cloned()
                else {
                    return runtime;
                };
                let entry = runtime
                    .runtimes
                    .entry(active_id)
                    .or_insert_with(|| PlaylistRuntime::new(0));
                runtime
                    .snapshots
                    .get(&playlist.id)
                    .cloned()
                    .and_then(|snapshot| entry.restore(&snapshot, &playlist, now_ms).ok())
            };
            if restored.is_none() {
                eprintln!("event=playlist.restore_skipped detail=no usable snapshot");
            }
        }
        runtime
    }

    fn now_ms(&self) -> u64 {
        self.start_instant
            .elapsed()
            .as_millis()
            .min(u64::MAX as u128) as u64
            + self.clock_offset_ms
    }

    fn definitions_health(&self) -> DefinitionsHealth {
        let store_health = if self.store_error.is_some() {
            "corrupt"
        } else {
            "ok"
        };
        DefinitionsHealth {
            count: self.playlists.len(),
            store_health: store_health.into(),
        }
    }

    fn unavailable_for(&self, playlist: &Playlist) -> Vec<String> {
        let mut unavailable: Vec<String> = playlist
            .entries
            .iter()
            .filter(|id| !self.valid_ids.contains(*id) || self.quarantined_ids.contains(*id))
            .cloned()
            .collect();
        unavailable.sort();
        unavailable.dedup();
        unavailable
    }

    fn status(&self) -> SessionStatus {
        let unavailable_ids = self
            .active
            .as_ref()
            .and_then(|id| self.playlists.iter().find(|playlist| &playlist.id == id))
            .map(|playlist| self.unavailable_for(playlist))
            .unwrap_or_default();
        SessionStatus {
            active: self.active.is_some(),
            playlist_id: self.active.clone(),
            decision: self.last_decision.clone(),
            unavailable_ids,
            definitions: self.definitions_health(),
            clock_skipped_ms: self.clock_skipped_ms,
        }
    }

    fn refresh_quarantine(&mut self) {
        let Some(supervisor) = &self.supervisor else {
            return;
        };
        match supervisor.try_quarantined_ids(QUARANTINE_QUERY_TIMEOUT) {
            Ok(ids) => self.quarantined_ids = ids,
            // The supervisor may be momentarily busy; keep the last known set.
            Err(error) => {
                eprintln!("event=playlist.quarantine_query_failed detail={error}");
            }
        }
    }

    /// Persists the active runtime's snapshot if the given decision changed
    /// state (or periodically while waiting). `remaining_ms` changes every
    /// tick, so change detection compares the stable signature only.
    fn maybe_persist(&mut self, playlist_id: String, decision: &PlaylistDecision, now_ms: u64) {
        let changed = self.last_decision.as_ref().map(decision_signature)
            != Some(decision_signature(decision));
        self.last_decision = Some(decision.clone());
        let waiting_refresh = matches!(decision, PlaylistDecision::Waiting { .. })
            && self.last_waiting_persist.elapsed() >= WAITING_PERSIST_INTERVAL;
        if !changed && !waiting_refresh {
            return;
        }
        let snapshot = self
            .playlists
            .iter()
            .find(|playlist| playlist.id == playlist_id)
            .and_then(|playlist| {
                self.runtimes
                    .get(&playlist_id)
                    .and_then(|runtime| runtime.snapshot(playlist, now_ms).ok())
            });
        if let Some(snapshot) = snapshot {
            self.snapshots.insert(playlist_id.clone(), snapshot);
        }
        if changed {
            eprintln!(
                "event=playlist.decision playlist_id={playlist_id} decision={}",
                serde_json::to_string(decision).unwrap_or_default()
            );
        }
        self.last_waiting_persist = Instant::now();
        self.persist_state();
    }

    fn tick_session(&mut self) {
        self.refresh_quarantine();
        let Some(playlist_id) = self.active.clone() else {
            return;
        };
        let Some(playlist) = self.playlists.iter().find(|p| p.id == playlist_id).cloned() else {
            self.active = None;
            self.last_decision = None;
            self.persist_state();
            eprintln!("event=playlist.active_removed playlist_id={playlist_id}");
            return;
        };
        let unavailable = self.unavailable_for(&playlist);
        let now_ms = self.now_ms();
        let decision = {
            let runtime = self
                .runtimes
                .entry(playlist_id.clone())
                .or_insert_with(|| PlaylistRuntime::new(0));
            runtime.tick(&playlist, now_ms, &unavailable)
        };
        match decision {
            Ok(decision) => self.maybe_persist(playlist_id, &decision, now_ms),
            Err(error) => {
                // Monotonic regression cannot happen through this clock path.
                eprintln!("event=playlist.tick_error playlist_id={playlist_id} detail={error}");
            }
        }
    }

    fn persist_state(&mut self) {
        let mut bytes = self.encode_state();
        // Evict non-active snapshots first so the active session always fits.
        while bytes.len() as u64 > MAX_RUNTIME_STATE_BYTES {
            let Some(evicted) = self
                .snapshots
                .keys()
                .find(|id| Some(*id) != self.active.as_ref())
                .cloned()
            else {
                break;
            };
            self.snapshots.remove(&evicted);
            eprintln!("event=playlist.state_evict playlist_id={evicted}");
            bytes = self.encode_state();
        }
        if bytes.len() as u64 > MAX_RUNTIME_STATE_BYTES {
            eprintln!("event=playlist.state_oversize bytes={}", bytes.len());
            return;
        }
        if let Err(error) = atomic_write(&self.state_path, &bytes) {
            eprintln!("event=playlist.state_save_error detail={error}");
        }
    }

    fn encode_state(&self) -> Vec<u8> {
        serde_json::to_vec_pretty(&PersistedSessionState {
            schema_version: RUNTIME_STATE_SCHEMA_VERSION,
            active_playlist_id: self.active.clone(),
            snapshots: self.snapshots.clone(),
        })
        .unwrap_or_default()
    }

    fn run(mut self, receiver: Receiver<SessionCommand>) {
        // Tick on a fixed cadence even while commands stream in: a bare
        // recv_timeout would let continuous polling starve the tick.
        let mut last_tick = Instant::now();
        loop {
            let wait = self.tick_duration.saturating_sub(last_tick.elapsed());
            match receiver.recv_timeout(wait) {
                Ok(SessionCommand::List(reply)) => {
                    let _ = reply.send(self.store_list());
                }
                Ok(SessionCommand::Put(playlist, reply)) => {
                    let _ = reply.send(self.put_playlist(playlist));
                }
                Ok(SessionCommand::Remove(id, reply)) => {
                    let _ = reply.send(self.remove_playlist(id));
                }
                Ok(SessionCommand::Activate(id, reply)) => {
                    let _ = reply.send(self.activate_playlist(id));
                }
                Ok(SessionCommand::Status(reply)) => {
                    let _ = reply.send(Ok(self.status()));
                }
                Ok(SessionCommand::Import(playlists, reply)) => {
                    let _ = reply.send(self.import_playlists(playlists));
                }
                Ok(SessionCommand::DebugClockSkip(ms, reply)) => {
                    let _ = reply.send(self.debug_clock_skip(ms));
                }
                Ok(SessionCommand::Availability { valid_ids }) => {
                    self.valid_ids = valid_ids;
                }
                Ok(SessionCommand::Shutdown(ack)) => {
                    // Capture the final position before the daemon exits.
                    if let Some(active) = self.active.clone() {
                        let now_ms = self.now_ms();
                        let snapshot = self
                            .playlists
                            .iter()
                            .find(|playlist| playlist.id == active)
                            .and_then(|playlist| {
                                self.runtimes
                                    .get(&active)
                                    .and_then(|runtime| runtime.snapshot(playlist, now_ms).ok())
                            });
                        if let Some(snapshot) = snapshot {
                            self.snapshots.insert(active, snapshot);
                        }
                    }
                    self.persist_state();
                    let _ = ack.send(());
                    return;
                }
                Err(mpsc::RecvTimeoutError::Disconnected) => return,
                Err(mpsc::RecvTimeoutError::Timeout) => {
                    self.tick_session();
                    last_tick = Instant::now();
                }
            }
        }
    }

    fn store_list(&self) -> Result<Vec<Playlist>, SessionError> {
        if let Some(error) = &self.store_error {
            return Err(SessionError::StoreUnavailable(error.clone()));
        }
        Ok(self.playlists.clone())
    }

    fn put_playlist(&mut self, playlist: Playlist) -> Result<Playlist, SessionError> {
        if let Some(error) = &self.store_error {
            return Err(SessionError::StoreUnavailable(error.clone()));
        }
        if let Err(error) = playlist.validate() {
            return Err(SessionError::Invalid(error));
        }
        let mut updated: Vec<Playlist> = self
            .playlists
            .iter()
            .filter(|existing| existing.id != playlist.id)
            .cloned()
            .collect();
        updated.push(playlist.clone());
        if let Err(error) = self.store.save(&updated) {
            return Err(SessionError::Invalid(error));
        }
        self.playlists = updated;
        Ok(playlist)
    }

    fn remove_playlist(&mut self, id: String) -> Result<String, SessionError> {
        if let Some(error) = &self.store_error {
            return Err(SessionError::StoreUnavailable(error.clone()));
        }
        if !self.playlists.iter().any(|playlist| playlist.id == id) {
            return Err(SessionError::NotFound(id));
        }
        let updated: Vec<Playlist> = self
            .playlists
            .iter()
            .filter(|playlist| playlist.id != id)
            .cloned()
            .collect();
        if let Err(error) = self.store.save(&updated) {
            return Err(SessionError::Invalid(error));
        }
        self.playlists = updated;
        self.runtimes.remove(&id);
        self.snapshots.remove(&id);
        if self.active.as_deref() == Some(&id) {
            self.active = None;
            self.last_decision = None;
        }
        self.persist_state();
        Ok(id)
    }

    fn activate_playlist(&mut self, id: Option<String>) -> Result<SessionStatus, SessionError> {
        if let Some(error) = &self.store_error {
            return Err(SessionError::StoreUnavailable(error.clone()));
        }
        let Some(id) = id else {
            self.active = None;
            self.last_decision = None;
            self.persist_state();
            return Ok(self.status());
        };
        let Some(playlist) = self.playlists.iter().find(|p| p.id == id).cloned() else {
            return Err(SessionError::NotFound(id));
        };
        if self.active.as_deref() != Some(&id) {
            let unavailable = self.unavailable_for(&playlist);
            let now_ms = self.now_ms();
            let (decision, snapshot) = {
                let runtime = self
                    .runtimes
                    .entry(id.clone())
                    .or_insert_with(|| PlaylistRuntime::new(0));
                let decision = if let Some(snapshot) = self.snapshots.get(&id).cloned() {
                    match runtime.restore(&snapshot, &playlist, now_ms) {
                        Ok(()) => runtime
                            .tick(&playlist, now_ms, &unavailable)
                            .unwrap_or(PlaylistDecision::NoEligible),
                        Err(error) => {
                            eprintln!(
                                "event=playlist.snapshot_restore_error playlist_id={id} detail={error}"
                            );
                            self.snapshots.remove(&id);
                            runtime
                                .start(&playlist, now_ms, &unavailable)
                                .unwrap_or(PlaylistDecision::NoEligible)
                        }
                    }
                } else {
                    runtime
                        .start(&playlist, now_ms, &unavailable)
                        .unwrap_or(PlaylistDecision::NoEligible)
                };
                (decision, runtime.snapshot(&playlist, now_ms).ok())
            };
            self.active = Some(id.clone());
            self.last_decision = Some(decision);
            if let Some(snapshot) = snapshot {
                self.snapshots.insert(id, snapshot);
            }
            self.persist_state();
        }
        Ok(self.status())
    }

    fn import_playlists(
        &mut self,
        legacy: Vec<ImportPlaylist>,
    ) -> Result<ImportSummary, SessionError> {
        if let Some(error) = &self.store_error {
            return Err(SessionError::StoreUnavailable(error.clone()));
        }
        if !self.playlists.is_empty() {
            return Err(SessionError::ImportBlocked);
        }
        let mut imported = Vec::new();
        let mut rejected = 0;
        let mut used_ids: BTreeSet<String> = BTreeSet::new();
        for entry in legacy {
            // 124 chars leaves room for a "-NNN" collision suffix within the
            // 128-byte playlist id bound.
            let base: String = entry.title.trim().chars().take(124).collect();
            let mut id = base.clone();
            let mut suffix = 2;
            while id.is_empty() || used_ids.contains(&id) {
                if suffix > 256 {
                    eprintln!(
                        "event=playlist.import_rejected detail=identity collision limit reached"
                    );
                    rejected += 1;
                    id.clear();
                    break;
                }
                id = format!("{base}-{suffix}");
                suffix += 1;
            }
            if id.is_empty() {
                continue;
            }
            match entry.into_playlist(id.clone()) {
                Ok(playlist) => {
                    used_ids.insert(id);
                    imported.push(playlist);
                }
                Err(error) => {
                    eprintln!("event=playlist.import_rejected detail={error}");
                    rejected += 1;
                }
            }
        }
        if imported.len() > 256 {
            return Err(SessionError::Invalid(
                "playlist count exceeds safety limit".into(),
            ));
        }
        if let Err(error) = self.store.save(&imported) {
            return Err(SessionError::Invalid(error));
        }
        self.playlists = imported;
        Ok(ImportSummary {
            imported: self.playlists.len(),
            rejected,
        })
    }

    /// Test-only suspend simulation. The clock offset advances and the active
    /// runtime is restored from its pre-jump snapshot within this single
    /// command, so no tick ever observes the jump — exactly how monotonic
    /// time behaves across suspend. Remaining time is preserved exactly.
    fn debug_clock_skip(&mut self, ms: u64) -> Result<SessionStatus, SessionError> {
        if !(1..=MAX_CLOCK_SKIP_MS).contains(&ms) {
            return Err(SessionError::Invalid(
                "clock skip is outside the safety bounds".into(),
            ));
        }
        let frozen = self.active.as_ref().and_then(|id| {
            let runtime = self.runtimes.get(id)?;
            let playlist = self.playlists.iter().find(|p| &p.id == id)?;
            runtime.snapshot(playlist, self.now_ms()).ok()
        });
        self.clock_offset_ms += ms;
        self.clock_skipped_ms += ms;
        if let (Some(id), Some(snapshot)) = (self.active.clone(), frozen)
            && let Some(playlist) = self.playlists.iter().find(|p| p.id == id).cloned()
        {
            let unavailable = self.unavailable_for(&playlist);
            let now_ms = self.now_ms();
            if let Some(runtime) = self.runtimes.get_mut(&id) {
                if let Err(error) = runtime.restore(&snapshot, &playlist, now_ms) {
                    eprintln!("event=playlist.clock_skip_restore_error detail={error}");
                } else if let Ok(decision) = runtime.tick(&playlist, now_ms, &unavailable) {
                    self.last_decision = Some(decision);
                }
            }
        }
        Ok(self.status())
    }
}

/// Stable identity of a decision: state, wallpaper, and index. `remaining_ms`
/// (and `deadline_ms`) change every tick and never trigger persistence.
fn decision_signature(decision: &PlaylistDecision) -> (u8, String, usize) {
    match decision {
        PlaylistDecision::Started {
            wallpaper_id,
            index,
            ..
        } => (0, wallpaper_id.clone(), *index),
        PlaylistDecision::Waiting {
            wallpaper_id,
            index,
            ..
        } => (1, wallpaper_id.clone(), *index),
        PlaylistDecision::Advanced {
            wallpaper_id,
            index,
            ..
        } => (2, wallpaper_id.clone(), *index),
        PlaylistDecision::Paused {
            wallpaper_id,
            index,
            ..
        } => (3, wallpaper_id.clone(), *index),
        PlaylistDecision::Exhausted => (4, String::new(), 0),
        PlaylistDecision::NoEligible => (5, String::new(), 0),
    }
}

fn load_runtime_state(
    path: &std::path::Path,
) -> (Option<String>, BTreeMap<String, PlaylistRuntimeSnapshot>) {
    // Bound the read before allocating: a huge accidental file must not
    // balloon the daemon at startup.
    let metadata = match std::fs::metadata(path) {
        Ok(metadata) => metadata,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
            return (None, BTreeMap::new());
        }
        Err(error) => {
            eprintln!("event=playlist.state_read_error detail={error}");
            return (None, BTreeMap::new());
        }
    };
    if metadata.len() > MAX_RUNTIME_STATE_BYTES {
        eprintln!(
            "event=playlist.state_invalid detail=runtime state exceeds {} bytes",
            MAX_RUNTIME_STATE_BYTES
        );
        quarantine_invalid_state(path);
        return (None, BTreeMap::new());
    }
    let bytes = match std::fs::read(path) {
        Ok(bytes) => bytes,
        Err(error) => {
            eprintln!("event=playlist.state_read_error detail={error}");
            quarantine_invalid_state(path);
            return (None, BTreeMap::new());
        }
    };
    match serde_json::from_slice::<PersistedSessionState>(&bytes) {
        Ok(state)
            if state.schema_version == RUNTIME_STATE_SCHEMA_VERSION
                && state.snapshots.len() <= MAX_SNAPSHOT_ENTRIES =>
        {
            let mut snapshots = BTreeMap::new();
            for (id, snapshot) in state.snapshots {
                match snapshot.validate() {
                    Ok(()) => {
                        snapshots.insert(id, snapshot);
                    }
                    Err(error) => {
                        eprintln!(
                            "event=playlist.snapshot_entry_invalid playlist_id={id} detail={error}"
                        );
                    }
                }
            }
            (state.active_playlist_id, snapshots)
        }
        Ok(_) => {
            eprintln!("event=playlist.state_invalid detail=unsupported or oversized runtime state");
            quarantine_invalid_state(path);
            (None, BTreeMap::new())
        }
        Err(error) => {
            eprintln!("event=playlist.state_invalid detail={error}");
            quarantine_invalid_state(path);
            (None, BTreeMap::new())
        }
    }
}

pub struct PlaylistSessionService {
    handle: PlaylistSessionHandle,
    thread: Option<JoinHandle<()>>,
}

impl PlaylistSessionService {
    pub fn start(config: PlaylistSessionConfig) -> Self {
        let (sender, receiver) = mpsc::sync_channel(COMMAND_CAPACITY);
        let runtime = SessionRuntime::new(config);
        let thread = thread::Builder::new()
            .name("kwe-playlist-session".into())
            .spawn(move || runtime.run(receiver))
            .expect("spawn playlist session thread");
        Self {
            handle: PlaylistSessionHandle { sender },
            thread: Some(thread),
        }
    }

    pub fn handle(&self) -> PlaylistSessionHandle {
        self.handle.clone()
    }
}

impl Drop for PlaylistSessionService {
    fn drop(&mut self) {
        // Bound the shutdown wait: a wedged filesystem must not hang daemon
        // exit. The ack bounds the final persist; the join polls with a
        // deadline in case the thread is wedged before it can ack.
        let (ack_sender, ack_receiver) = mpsc::channel();
        let _ = self
            .handle
            .sender
            .send(SessionCommand::Shutdown(ack_sender));
        let _ = ack_receiver.recv_timeout(Duration::from_secs(5));
        if let Some(thread) = self.thread.take() {
            let deadline = Instant::now() + Duration::from_secs(5);
            while !thread.is_finished() && Instant::now() < deadline {
                std::thread::sleep(Duration::from_millis(10));
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn temporary_state_dir(label: &str) -> PathBuf {
        std::env::temp_dir().join(format!(
            "kwe-playlist-session-{label}-{}-{}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap_or_default()
                .as_nanos()
        ))
    }

    fn daily_playlist() -> Playlist {
        let mut playlist = Playlist::new("daily".into(), "Daily".into()).unwrap();
        for id in ["1", "2", "3"] {
            playlist.add(id.into()).unwrap();
        }
        playlist.duration_seconds = 10;
        playlist
    }

    fn valid_ids(ids: &[&str]) -> Arc<BTreeSet<String>> {
        Arc::new(ids.iter().map(|id| id.to_string()).collect())
    }

    fn config(state_dir: PathBuf, valid: &[&str]) -> PlaylistSessionConfig {
        PlaylistSessionConfig {
            state_dir,
            tick_ms: 50,
            supervisor: None,
            valid_ids: valid_ids(valid),
        }
    }

    fn decision_wallpaper(decision: Option<&PlaylistDecision>) -> Option<(String, usize)> {
        decision.and_then(|decision| match decision {
            PlaylistDecision::Started {
                wallpaper_id,
                index,
                ..
            }
            | PlaylistDecision::Waiting {
                wallpaper_id,
                index,
                ..
            }
            | PlaylistDecision::Advanced {
                wallpaper_id,
                index,
                ..
            }
            | PlaylistDecision::Paused {
                wallpaper_id,
                index,
                ..
            } => Some((wallpaper_id.clone(), *index)),
            PlaylistDecision::Exhausted | PlaylistDecision::NoEligible => None,
        })
    }

    #[test]
    fn corrupt_definitions_store_keeps_daemon_up() {
        let dir = temporary_state_dir("corrupt-definitions");
        std::fs::create_dir_all(&dir).unwrap();
        std::fs::write(dir.join(DEFINITIONS_FILE), b"garbage").unwrap();
        let service = PlaylistSessionService::start(config(dir, &[]));
        let handle = service.handle();
        let status = handle.status().unwrap();
        assert_eq!(status.definitions.store_health, "corrupt");
        assert_eq!(status.definitions.count, 0);
        assert!(matches!(
            handle.list(),
            Err(SessionError::StoreUnavailable(_))
        ));
        assert!(matches!(
            handle.put(daily_playlist()),
            Err(SessionError::StoreUnavailable(_))
        ));
    }

    #[test]
    fn corrupt_runtime_state_is_quarantined_and_session_starts_fresh() {
        let dir = temporary_state_dir("corrupt-runtime");
        std::fs::create_dir_all(&dir).unwrap();
        std::fs::write(dir.join(RUNTIME_STATE_FILE), b"garbage").unwrap();
        let service = PlaylistSessionService::start(config(dir.clone(), &[]));
        let status = service.handle().status().unwrap();
        assert!(!status.active);
        let quarantined = std::fs::read_dir(&dir)
            .unwrap()
            .flatten()
            .any(|entry| entry.file_name().to_string_lossy().contains(".invalid-"));
        assert!(quarantined, "invalid runtime state must be quarantined");
    }

    #[test]
    fn activate_persists_and_restart_restores_session() {
        let dir = temporary_state_dir("restart");
        let first = config(dir.clone(), &["1", "2", "3"]);
        {
            let service = PlaylistSessionService::start(first);
            let handle = service.handle();
            handle.put(daily_playlist()).unwrap();
            handle.activate(Some("daily".into())).unwrap();
            let status = handle.status().unwrap();
            assert!(status.active);
            assert_eq!(status.playlist_id.as_deref(), Some("daily"));
            assert_eq!(
                decision_wallpaper(status.decision.as_ref()),
                Some(("1".into(), 0))
            );
            // Drop persists the final snapshot through the Shutdown command.
        }
        let service = PlaylistSessionService::start(config(dir, &["1", "2", "3"]));
        let handle = service.handle();
        // The first tick rehydrates and reports the restored position.
        std::thread::sleep(Duration::from_millis(200));
        let status = handle.status().unwrap();
        assert!(status.active, "active session must survive a restart");
        assert_eq!(status.playlist_id.as_deref(), Some("daily"));
        match status.decision.unwrap() {
            PlaylistDecision::Waiting {
                wallpaper_id,
                remaining_ms,
                ..
            } => {
                assert_eq!(wallpaper_id, "1");
                // Downtime is not charged: the remaining time re-anchors.
                assert!(remaining_ms <= 10_000);
            }
            other => panic!("expected Waiting, got {other:?}"),
        }
    }

    #[test]
    fn import_maps_legacy_titles_to_bounded_ids() {
        let dir = temporary_state_dir("import");
        let service = PlaylistSessionService::start(config(dir, &[]));
        let handle = service.handle();
        let legacy = vec![
            ImportPlaylist {
                title: "  First  ".into(),
                entries: vec!["one".into()],
                shuffle: false,
                repeat: true,
                duration_seconds: 300,
                transition: "none".into(),
                transition_seconds: 0,
            },
            ImportPlaylist {
                title: "First".into(),
                entries: vec!["two".into()],
                shuffle: false,
                repeat: true,
                duration_seconds: 300,
                transition: "none".into(),
                transition_seconds: 0,
            },
        ];
        let summary = handle.import(legacy).unwrap();
        assert_eq!(summary.imported, 2);
        assert_eq!(summary.rejected, 0);
        let playlists = handle.list().unwrap();
        let ids: Vec<&str> = playlists.iter().map(|p| p.id.as_str()).collect();
        assert_eq!(ids, vec!["First", "First-2"]);

        // Import into a non-empty store is blocked.
        assert!(matches!(
            handle.import(Vec::new()),
            Err(SessionError::ImportBlocked)
        ));
    }

    #[test]
    fn import_ids_never_exceed_the_identity_bound() {
        // Titles longer than the 128-byte id bound must still import: the
        // derived base leaves room for a collision suffix, and every id must
        // round-trip the daemon's own validate().
        let dir = temporary_state_dir("import-bound");
        let service = PlaylistSessionService::start(config(dir, &[]));
        let handle = service.handle();
        let long = "L".repeat(200);
        let legacy = vec![
            ImportPlaylist {
                title: long.clone(),
                entries: vec!["one".into()],
                shuffle: false,
                repeat: true,
                duration_seconds: 300,
                transition: "none".into(),
                transition_seconds: 0,
            },
            ImportPlaylist {
                title: long,
                entries: vec!["two".into()],
                shuffle: false,
                repeat: true,
                duration_seconds: 300,
                transition: "none".into(),
                transition_seconds: 0,
            },
        ];
        let summary = handle.import(legacy).unwrap();
        assert_eq!(
            summary.imported, 2,
            "long-titled duplicates must not be dropped"
        );
        assert_eq!(summary.rejected, 0);
        let ids: Vec<String> = handle.list().unwrap().into_iter().map(|p| p.id).collect();
        assert_eq!(ids.len(), 2);
        assert!(ids.iter().all(|id| id.len() <= 128 && !id.is_empty()));
        assert_ne!(ids[0], ids[1], "collision suffix must keep ids distinct");
    }

    #[test]
    fn unavailable_set_marks_missing_and_quarantined_ids() {
        let dir = temporary_state_dir("availability");
        let service = PlaylistSessionService::start(config(dir, &["1"]));
        let handle = service.handle();
        handle.put(daily_playlist()).unwrap();
        let status = handle.activate(Some("daily".into())).unwrap();
        // 2 and 3 are missing from the catalog; 1 is installed.
        assert_eq!(status.unavailable_ids, vec!["2", "3"]);
        assert_eq!(
            decision_wallpaper(status.decision.as_ref()),
            Some(("1".into(), 0))
        );
    }

    #[test]
    fn debug_clock_skip_preserves_remaining_time() {
        let dir = temporary_state_dir("clock-skip");
        let service = PlaylistSessionService::start(config(dir, &["1", "2", "3"]));
        let handle = service.handle();
        handle.put(daily_playlist()).unwrap();
        handle.activate(Some("daily".into())).unwrap();
        // Let a few ticks run so the reported decision is Waiting.
        std::thread::sleep(Duration::from_millis(150));
        let before = handle.status().unwrap();
        let before_remaining = match before.decision.unwrap() {
            PlaylistDecision::Waiting { remaining_ms, .. } => remaining_ms,
            other => panic!("expected Waiting, got {other:?}"),
        };
        let after = handle.debug_clock_skip(60_000).unwrap();
        assert_eq!(after.clock_skipped_ms, 60_000);
        match after.decision.unwrap() {
            PlaylistDecision::Waiting { remaining_ms, .. } => {
                // The jump is invisible to the runtime: remaining time is
                // preserved up to the tiny elapsed time between the two
                // status snapshots (well under one tick).
                assert!(remaining_ms <= before_remaining);
                assert!(before_remaining - remaining_ms < 500);
            }
            other => panic!("expected Waiting, got {other:?}"),
        }
    }
}
