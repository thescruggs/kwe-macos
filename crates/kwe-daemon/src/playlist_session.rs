// SPDX-License-Identifier: GPL-3.0-or-later
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
use serde_json::Value;

use crate::{
    apply::ApplyError,
    persist::{atomic_write, quarantine_invalid_state},
    supervisor::{SupervisorHandle, WorkerPhase, WorkerStatus},
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
/// Exponential backoff between playlist apply attempts (BETA_M4c): a
/// failing entry-change must never become an apply storm.
const APPLY_BACKOFF_BASE: Duration = Duration::from_millis(1000);
const APPLY_BACKOFF_MAX: Duration = Duration::from_secs(30);
/// The apply worker's request/completion queue bound (Finding 2): at most
/// one apply is ever queued or in flight — a second dispatch busy-skips.
const APPLY_QUEUE_CAPACITY: usize = 1;
/// Bounded join for the apply worker at shutdown (Finding 7): a mid-flight
/// apply is bounded on its own, and shutdown never waits for it beyond this.
const APPLY_WORKER_JOIN_TIMEOUT: Duration = Duration::from_secs(5);

/// The lane a playlist session drives wallpaper assignment through
/// (BETA_M4c). The daemon's implementation is `apply::ApplyHandle` — the
/// full M4a apply transaction, sharing its single-transaction lock, its
/// rollback, and its bounded probes. Tests inject a recording stub. The
/// lane receives the session's configured output (None resolves at apply
/// time from the playlist's entries and the assignment store) and the
/// active playlist's entry ids.
pub trait PlaylistApplyLane: Send + Sync {
    /// `applied` is the wallpaper the session last applied (or satisfied
    /// itself was live). The lane uses it to tell the session's OWN stale
    /// renderer (which an entry change may displace) from a foreign one
    /// (which a user apply of a different wallpaper left live after the
    /// verdict; the lane yields to it).
    fn apply_playlist(
        &self,
        output: Option<String>,
        wallpaper_id: String,
        playlist_entries: &BTreeSet<String>,
        applied: Option<&str>,
    ) -> Result<Value, ApplyError>;
}

#[derive(Clone)]
pub struct PlaylistSessionConfig {
    pub state_dir: PathBuf,
    pub tick_ms: u64,
    pub supervisor: Option<SupervisorHandle>,
    /// Catalog-derived ids that are installed and usable. Pushed by `main`
    /// at startup and refreshed after every rescan.
    pub valid_ids: Arc<BTreeSet<String>>,
    /// The output playlists apply to (BETA_M4c). None resolves at apply
    /// time inside the lane: the last assigned output whose wallpaper is a
    /// member of the active playlist, else the first enabled and connected
    /// output (docs/BETA_M4.md M4c). Per-display playlist intent comes from
    /// docs/UX_DESIGN.md.
    pub output: Option<String>,
    /// The apply lane driving renderer assignment on entry changes; None
    /// keeps the session a pure timer (no renderer is ever started).
    pub apply: Option<Arc<dyn PlaylistApplyLane>>,
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

/// One apply request handed to the apply worker (Finding 2). `applied` is
/// the session's applied identity at dispatch time; the lane's post-lock
/// foreign-renderer check needs it to tell the session's own stale renderer
/// from a user apply that won the slot.
struct ApplyRequest {
    output: Option<String>,
    wallpaper_id: String,
    playlist_entries: BTreeSet<String>,
    applied: Option<String>,
}

/// The outcome of one worker apply, folded back into session state on a
/// later tick. `result` is the lane's `Result` verbatim, so the session can
/// tell a real failure (back off) from a transient yield (`Busy`/`Yielded`).
struct ApplyOutcome {
    wallpaper_id: String,
    result: Result<Value, ApplyError>,
}

/// The apply worker: consumes one request at a time off a bound-1 queue,
/// runs the (bounded) apply transaction on its OWN thread so the session
/// tick thread never blocks on it, and posts the outcome back on the
/// completion channel (drained by the session on a later tick). Shutdown is
/// signaled by the session dropping its request sender: the blocking `recv`
/// returns `Disconnected` and the worker exits after finishing any in-flight
/// transaction.
fn apply_worker_loop(
    apply: Arc<dyn PlaylistApplyLane>,
    requests: Receiver<ApplyRequest>,
    completions: SyncSender<ApplyOutcome>,
) {
    loop {
        let request = match requests.recv() {
            Ok(request) => request,
            Err(_) => return,
        };
        let result = apply.apply_playlist(
            request.output,
            request.wallpaper_id.clone(),
            &request.playlist_entries,
            request.applied.as_deref(),
        );
        if completions
            .send(ApplyOutcome {
                wallpaper_id: request.wallpaper_id,
                result,
            })
            .is_err()
        {
            // The session is gone: nothing to report back to.
            return;
        }
    }
}

struct SessionRuntime {
    store: PlaylistStore,
    state_path: PathBuf,
    tick_duration: Duration,
    supervisor: Option<SupervisorHandle>,
    output: Option<String>,
    apply: Option<Arc<dyn PlaylistApplyLane>>,
    playlists: Vec<Playlist>,
    store_error: Option<String>,
    runtimes: BTreeMap<String, PlaylistRuntime>,
    snapshots: BTreeMap<String, PlaylistRuntimeSnapshot>,
    active: Option<String>,
    last_decision: Option<PlaylistDecision>,
    last_waiting_persist: Instant,
    valid_ids: Arc<BTreeSet<String>>,
    quarantined_ids: BTreeSet<String>,
    /// SR-1c2: wallpaper ids the scene capability gate has refused for
    /// THIS session (`ApplyError::CapabilityGate`, both the blocking-
    /// missing and inspector-`incompatible` shapes). Fed into
    /// `unavailable_for` exactly like `quarantined_ids` — once an id lands
    /// here the decision engine routes around it the same way it already
    /// routes around a crash-quarantined one (see
    /// `quarantined_entry_is_never_applied`), so the playlist advances to
    /// the next eligible entry instead of retrying a refusal that cannot
    /// resolve itself. Unlike `quarantined_ids` there is no live oracle to
    /// refresh this from (the gate's answer does not change until the
    /// scene or the build does), so it is discovered reactively (the
    /// first apply attempt still happens once) and cleared only by
    /// `reset_apply` (a playlist switch/deactivation deserves a fresh
    /// evaluation).
    gate_refused_ids: BTreeSet<String>,
    /// The wallpaper the session last applied successfully (or satisfied
    /// itself was live). Distinguishes the session's OWN stale renderer
    /// from a foreign (user) one: the session displaces its own stale
    /// renderer on an entry change, but yields to a foreign one. Cleared
    /// when a foreign renderer takes the slot.
    applied_wallpaper: Option<String>,
    apply_failures: u32,
    next_apply_retry: Instant,
    /// The apply worker's request lane (None keeps the session a pure timer).
    apply_sender: Option<SyncSender<ApplyRequest>>,
    /// The apply worker's completion lane, drained every tick.
    apply_completions: Option<Receiver<ApplyOutcome>>,
    /// An apply request has been handed to the worker and its outcome has
    /// not yet been folded: the verdict logic must not dispatch another.
    apply_in_flight: bool,
    /// The last supervisor.status() error already logged (Finding 6): the
    /// apply_status_failed event is emitted once per error-state change, not
    /// once per 50-100 ms tick.
    last_status_error: Option<String>,
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
            output: config.output,
            apply: config.apply,
            playlists,
            store_error,
            runtimes: BTreeMap::new(),
            snapshots,
            active,
            last_decision: None,
            last_waiting_persist: Instant::now(),
            valid_ids: config.valid_ids,
            quarantined_ids: BTreeSet::new(),
            gate_refused_ids: BTreeSet::new(),
            applied_wallpaper: None,
            apply_failures: 0,
            next_apply_retry: Instant::now(),
            apply_sender: None,
            apply_completions: None,
            apply_in_flight: false,
            last_status_error: None,
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

    /// Spawns the apply worker (Finding 2): the apply lane runs on its own
    /// thread, so the session tick thread never blocks on a transaction
    /// (which could stall every `playlist.*` RPC for its whole 15-35 s).
    /// Returns the worker's join handle for the service to bound at drop;
    /// returns None (no worker) when the session has no apply lane.
    fn start_apply_worker(&mut self) -> Option<JoinHandle<()>> {
        let apply = self.apply.clone()?;
        let (request_sender, request_receiver) =
            mpsc::sync_channel::<ApplyRequest>(APPLY_QUEUE_CAPACITY);
        let (completion_sender, completion_receiver) =
            mpsc::sync_channel::<ApplyOutcome>(APPLY_QUEUE_CAPACITY);
        self.apply_sender = Some(request_sender);
        self.apply_completions = Some(completion_receiver);
        Some(
            thread::Builder::new()
                .name("kwe-playlist-apply".into())
                .spawn(move || apply_worker_loop(apply, request_receiver, completion_sender))
                .expect("spawn playlist apply worker"),
        )
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
            .filter(|id| {
                !self.valid_ids.contains(*id)
                    || self.quarantined_ids.contains(*id)
                    // SR-1c2: a gate-refused id is excluded exactly like a
                    // crash-quarantined one — see `gate_refused_ids`'s doc
                    // comment.
                    || self.gate_refused_ids.contains(*id)
            })
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

    /// Drives renderer assignment for the active entry (BETA_M4c): when
    /// the session's desired wallpaper is not already live, it applies
    /// through the lane — the full M4a apply transaction (hard cut; the
    /// keepalive re-publication covers the gap, docs/BETA_M4.md M4c). The
    /// policy is conservative and bounded:
    ///
    /// * the desired renderer wins: when the supervisor already runs (or
    ///   is transitioning to) the desired wallpaper, this tick does
    ///   nothing — the apply transaction or the supervisor's own lifecycle
    ///   owns the slot, and a user apply of the SAME wallpaper satisfies
    ///   the session too;
    /// * the session displaces its OWN stale renderer on an entry change
    ///   (the timer advanced: the old entry's renderer must go, hard cut);
    /// * a user/manager apply wins: when a DIFFERENT, foreign renderer is
    ///   live (Starting/Canary/Live/AwaitingAck/Restarting/RolledBack),
    ///   the session yields — it never fights the user's choice — and
    ///   re-asserts its desired entry once the foreign renderer is no
    ///   longer live (manual stop, crash, or the next entry change);
    /// * a manual stop of the session's own renderer is re-asserted on the
    ///   next tick (the supervisor is Stopped/Idle, nothing is live);
    /// * failures back off (1 s doubling to a 30 s cap) so a broken output
    ///   or shell cannot turn the session into an apply storm; the failure
    ///   is logged once per attempt with bounded detail;
    /// * a successful apply resets the backoff.
    ///
    /// Restart restore is covered by the same rules: after a daemon
    /// restart the supervisor is Idle, so the rehydrated entry is applied
    /// once even though the assignment store still records it — the store
    /// is the source for restore, the supervisor is the source of live.
    fn maybe_apply(&mut self, playlist: &Playlist, decision: &PlaylistDecision) {
        let Some(wallpaper_id) = decision_wallpaper_id(decision) else {
            return;
        };
        // Completed apply transactions are folded at the TOP of
        // `tick_session`, BEFORE `decision` (this function's own
        // parameter) is computed — not here. SR-1c2 found the ordering bug
        // this avoids: folding only here meant a just-learned gate refusal
        // could not affect the SAME tick's already-computed `decision`
        // (still naming the just-refused entry), causing exactly one
        // extra, otherwise-avoidable re-dispatch of an entry already known
        // to be excluded before the NEXT tick's fresh `unavailable` finally
        // caught up. Folding first means `decision` itself already
        // reflects the freshest state.
        let Some(supervisor) = &self.supervisor else {
            return;
        };
        let status = match supervisor.status() {
            Ok(status) => {
                // The status boundary recovered; a later error logs again.
                self.last_status_error = None;
                status
            }
            Err(error) => {
                // Log once per error-state change (Finding 6): while
                // supervisor.status() keeps erroring on every 50-100 ms tick
                // this stays silent instead of spamming apply_status_failed.
                let detail = error.to_string();
                if self.last_status_error.as_deref() != Some(detail.as_str()) {
                    self.last_status_error = Some(detail);
                    eprintln!("event=playlist.apply_status_failed detail={error}");
                }
                return;
            }
        };
        let retry_ready = Instant::now() >= self.next_apply_retry;
        let verdict = apply_verdict(
            self.applied_wallpaper.as_deref(),
            wallpaper_id,
            &status,
            retry_ready,
        );
        match verdict {
            ApplyVerdict::Satisfied => {
                self.applied_wallpaper = Some(wallpaper_id.to_string());
                self.reset_backoff();
                return;
            }
            ApplyVerdict::Yield => {
                // A foreign renderer owns the slot; the user's choice wins.
                // The backoff is cleared so re-assertion is prompt once the
                // foreign renderer stops (Finding 1).
                self.yield_to_foreign();
                return;
            }
            ApplyVerdict::Hold | ApplyVerdict::Wait => {
                // Hold: the failure backoff gate is still closed — keep the
                // applied identity (dropping it would reclassify our own
                // renderer as foreign and the session would never displace
                // it). Wait: the supervisor is recovering the requested
                // renderer (crash-restore); its own bounded recovery or
                // quarantine resolves it. Neither dispatches, neither
                // touches the backoff.
                return;
            }
            ApplyVerdict::Apply => {}
        }
        // While an apply is in flight (or queued), never dispatch another:
        // the worker consumes one request at a time and the outcome folds on
        // a later tick (Finding 2).
        if self.apply_in_flight {
            return;
        }
        let entries: BTreeSet<String> = playlist.entries.iter().cloned().collect();
        self.dispatch_apply(wallpaper_id.to_string(), entries);
    }

    /// Folds every completed apply outcome from the worker back into session
    /// state. Runs at the top of each tick so the verdict below sees the
    /// freshest applied/failure state. Non-failure outcomes (`Busy` from a
    /// user apply holding the transaction lock, `Yielded` from the lane's
    /// post-lock foreign-renderer check) are treated like a yield — no
    /// failure count, no backoff.
    fn fold_apply_completions(&mut self) {
        // Take the receiver out of `self` while folding so the mutations
        // below (which call `&mut self` helpers) never hold a borrow of it.
        // At most one completion can be pending (the in-flight invariant),
        // so the drain is trivially bounded.
        let Some(completions) = self.apply_completions.take() else {
            return;
        };
        while let Ok(outcome) = completions.try_recv() {
            let ApplyOutcome {
                wallpaper_id,
                result,
            } = outcome;
            self.apply_in_flight = false;
            match result {
                Ok(_) => {
                    self.applied_wallpaper = Some(wallpaper_id.clone());
                    self.reset_backoff();
                    eprintln!("event=playlist.assigned wallpaper_id={wallpaper_id}");
                }
                Err(ApplyError::Busy) => {
                    // A user's wallpaper.apply holds the transaction lock: a
                    // transient yield, never a failure (Finding 1).
                    self.yield_to_foreign();
                }
                Err(ApplyError::Yielded(detail)) => {
                    // A foreign renderer took the slot between the session's
                    // verdict and the lock (TOCTOU): yield, never a failure.
                    self.yield_to_foreign();
                    eprintln!("event=playlist.apply_yielded detail={detail}");
                }
                // SR-1c2: the scene capability gate refused this entry
                // (blocking-missing required capability, or the inspector
                // itself refused the content) — never a generic failure.
                // Recording the id in `gate_refused_ids` (read by
                // `unavailable_for`) is what makes the NEXT tick's decision
                // route around it, exactly the way a crash-quarantined id
                // already is — the playlist advances instead of retrying a
                // refusal that cannot resolve itself. No backoff/failure
                // count: this is not a transient condition apply_backoff's
                // doubling delay would help with, and never wedges/stops
                // the playlist (decision (a)).
                Err(ApplyError::CapabilityGate { missing, .. }) => {
                    self.gate_refused_ids.insert(wallpaper_id.clone());
                    eprintln!(
                        "event=playlist.entry_gate_refused wallpaper={wallpaper_id} missing={}",
                        missing.join(",")
                    );
                }
                Err(error) => {
                    self.apply_failures = self.apply_failures.saturating_add(1);
                    let delay = apply_backoff(self.apply_failures);
                    self.next_apply_retry = Instant::now() + delay;
                    eprintln!(
                        "event=playlist.apply_failed wallpaper_id={wallpaper_id} failures={} detail={} next_retry_ms={}",
                        self.apply_failures,
                        error.detail().unwrap_or_else(|| error.code()),
                        delay.as_millis(),
                    );
                }
            }
        }
        self.apply_completions = Some(completions);
    }

    /// Hands one apply request to the worker (bound queue of 1, busy-skip):
    /// the worker runs the (bounded) transaction off the tick thread. The
    /// in-flight flag prevents a second dispatch until the outcome folds.
    fn dispatch_apply(&mut self, wallpaper_id: String, entries: BTreeSet<String>) {
        let Some(sender) = &self.apply_sender else {
            return;
        };
        let request = ApplyRequest {
            output: self.output.clone(),
            wallpaper_id,
            playlist_entries: entries,
            applied: self.applied_wallpaper.clone(),
        };
        match sender.try_send(request) {
            Ok(()) => self.apply_in_flight = true,
            Err(TrySendError::Full(_)) => {
                // A request is already queued; the in-flight flag already
                // blocks the next dispatch, so nothing else to do.
            }
            Err(TrySendError::Disconnected(_)) => {
                // The worker exited (shutdown); nothing to dispatch to.
            }
        }
    }

    /// The user/manager choice wins the slot: forget the session's applied
    /// identity and clear the failure backoff so re-assertion is prompt once
    /// the foreign renderer is no longer live (Finding 1).
    fn yield_to_foreign(&mut self) {
        self.applied_wallpaper = None;
        self.reset_backoff();
    }

    /// Clears the failure backoff gate: the next tick applies immediately.
    fn reset_backoff(&mut self) {
        self.apply_failures = 0;
        self.next_apply_retry = Instant::now();
    }

    /// Resets the apply state (playlist switch, deactivation, or the active
    /// playlist disappearing): the new session's first decision applies
    /// immediately. Also clears `gate_refused_ids` (SR-1c2): a fresh
    /// playlist activation deserves a fresh gate evaluation rather than
    /// carrying forward a refusal recorded for a different playlist's
    /// (coincidentally same-id) entry.
    fn reset_apply(&mut self) {
        self.applied_wallpaper = None;
        self.reset_backoff();
        self.gate_refused_ids.clear();
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
        // Fold any completed apply transaction from the worker FIRST
        // (moved here in SR-1c2, see `maybe_apply`'s doc comment) so BOTH
        // the `unavailable` set below (gate-refused/quarantined ids) and
        // the decision computed from it already reflect the freshest
        // state this tick — not just the verdict maybe_apply computes
        // from them afterward.
        self.fold_apply_completions();
        let Some(playlist_id) = self.active.clone() else {
            return;
        };
        let Some(playlist) = self.playlists.iter().find(|p| p.id == playlist_id).cloned() else {
            self.active = None;
            self.last_decision = None;
            self.reset_apply();
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
            Ok(decision) => {
                self.maybe_persist(playlist_id, &decision, now_ms);
                // Renderer assignment is dispatched AFTER persistence: the
                // entry position is on disk before an apply is handed to the
                // worker, and a crash mid-apply cannot lose the position.
                // The apply worker keeps the transaction off this tick thread
                // (Finding 2).
                self.maybe_apply(&playlist, &decision);
            }
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
            self.reset_apply();
            self.persist_state();
            return Ok(self.status());
        };
        let Some(playlist) = self.playlists.iter().find(|p| p.id == id).cloned() else {
            return Err(SessionError::NotFound(id));
        };
        if self.active.as_deref() != Some(&id) {
            // A different (or fresh) active session starts its apply state
            // clean: the new entry's first decision applies immediately.
            self.reset_apply();
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

/// The wallpaper a decision wants on the output; Exhausted and NoEligible
/// want nothing (the session never applies for them).
fn decision_wallpaper_id(decision: &PlaylistDecision) -> Option<&str> {
    match decision {
        PlaylistDecision::Started { wallpaper_id, .. }
        | PlaylistDecision::Waiting { wallpaper_id, .. }
        | PlaylistDecision::Advanced { wallpaper_id, .. }
        | PlaylistDecision::Paused { wallpaper_id, .. } => Some(wallpaper_id),
        PlaylistDecision::Exhausted | PlaylistDecision::NoEligible => None,
    }
}

/// Phases where a renderer is live or on its way; a foreign one must
/// never be displaced by the playlist mid-flight.
fn phase_has_live_worker(phase: WorkerPhase) -> bool {
    matches!(
        phase,
        WorkerPhase::Starting
            | WorkerPhase::Canary
            | WorkerPhase::Live
            | WorkerPhase::AwaitingAck
            | WorkerPhase::Restarting
            | WorkerPhase::RolledBack
    )
}

/// What the session should do for the desired wallpaper this tick. Pure so
/// the user-apply precedence (the foreign-renderer yield), the entry-change
/// displacement of the session's own stale renderer, the crash-restore wait,
/// and the manual-stop re-assertion are unit-testable with fabricated worker
/// state (`WorkerStatus` is fully constructible).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum ApplyVerdict {
    /// The desired wallpaper is already live or on its way: remember it as
    /// applied (the apply transaction, the supervisor's own lifecycle, or
    /// a user apply of the same wallpaper all satisfy the session).
    Satisfied,
    /// A FOREIGN renderer owns the slot: the user's choice wins this tick.
    Yield,
    /// The failure backoff gate is closed, so this tick must not apply —
    /// whether the session's OWN stale renderer still holds the slot (the
    /// timer advanced) or nothing is live. Do nothing and keep the applied
    /// identity so the stale renderer stays recognizable as ours once the
    /// gate opens.
    Hold,
    /// The supervisor is recovering the requested renderer (crash-restore:
    /// Restarting/RolledBack with an active worker that renders a DIFFERENT
    /// wallpaper, or none). Do not claim Satisfied and do not dispatch a
    /// competing apply — the supervisor's own bounded recovery or quarantine
    /// resolves it, and a quarantine flows through the existing skip logic.
    Wait,
    /// Apply the desired wallpaper through the lane (unless the failure
    /// backoff gate is still closed).
    Apply,
}

fn apply_verdict(
    applied: Option<&str>,
    desired: &str,
    status: &WorkerStatus,
    retry_ready: bool,
) -> ApplyVerdict {
    // The desired renderer is already live or on its way.
    if status.requested_wallpaper_id.as_deref() == Some(desired)
        && phase_has_live_worker(status.phase)
    {
        // Crash-restore (Finding 4): during a supervisor recovery phase the
        // requested worker is dead and the ACTIVE worker renders a different
        // wallpaper (or nothing). Claiming Satisfied would leave the session
        // parked on an entry that is not actually displayed and would never
        // advance once the supervisor quarantines it — so wait instead.
        if matches!(
            status.phase,
            WorkerPhase::RolledBack | WorkerPhase::Restarting
        ) && status.wallpaper_id.as_deref() != Some(desired)
        {
            return ApplyVerdict::Wait;
        }
        return ApplyVerdict::Satisfied;
    }
    // A renderer is live but it is not the desired one.
    if phase_has_live_worker(status.phase) {
        // The session's OWN stale renderer (the timer advanced): the entry
        // change must displace it — hard cut through the apply transaction.
        // The displacement is gated like every other apply: without the
        // gate, a start blocked by a pending display handoff (the previous
        // renderer is retired for the handoff window) would be re-fired on
        // every tick — an apply storm against the same blocked transaction.
        if status.requested_wallpaper_id.as_deref() == applied {
            return if retry_ready {
                ApplyVerdict::Apply
            } else {
                ApplyVerdict::Hold
            };
        }
        // Anything else live is a user/manager apply: yield.
        return ApplyVerdict::Yield;
    }
    // Nothing is live (Idle, Stopped, Quarantined with a different
    // request): apply unless the failure backoff gate is still closed.
    if retry_ready {
        ApplyVerdict::Apply
    } else {
        ApplyVerdict::Hold
    }
}

/// Post-lock precedence check for the apply lane (Finding 1): once the
/// transaction lock is held, a live worker whose request is neither the
/// desired wallpaper nor the session's own previously-applied renderer is a
/// user/manager apply that won the slot between the session's verdict and the
/// lock (the TOCTOU window). The session must yield to it, not displace the
/// user's fresh renderer.
pub(crate) fn foreign_renderer_live(
    status: &WorkerStatus,
    desired: &str,
    applied: Option<&str>,
) -> bool {
    phase_has_live_worker(status.phase)
        && status.requested_wallpaper_id.as_deref() != Some(desired)
        && status.requested_wallpaper_id.as_deref() != applied
}

/// Exponential backoff between playlist apply attempts: 1 s doubling to a
/// 30 s cap. A failing entry-change must never become an apply storm.
fn apply_backoff(failures: u32) -> Duration {
    let shift = failures.saturating_sub(1).min(5);
    APPLY_BACKOFF_BASE
        .saturating_mul(1u32 << shift)
        .min(APPLY_BACKOFF_MAX)
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
    apply_worker: Option<JoinHandle<()>>,
}

impl PlaylistSessionService {
    pub fn start(config: PlaylistSessionConfig) -> Self {
        let (sender, receiver) = mpsc::sync_channel(COMMAND_CAPACITY);
        let mut runtime = SessionRuntime::new(config);
        // The apply worker (Finding 2) is spawned before the session thread
        // so the request/completion channels it owns are in place when the
        // session starts ticking.
        let apply_worker = runtime.start_apply_worker();
        let thread = thread::Builder::new()
            .name("kwe-playlist-session".into())
            .spawn(move || runtime.run(receiver))
            .expect("spawn playlist session thread");
        Self {
            handle: PlaylistSessionHandle { sender },
            thread: Some(thread),
            apply_worker,
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
        // Finding 7: the final position persist runs on the session (tick)
        // thread above and never waits for the worker. When the session
        // thread returns it drops its request sender, which signals the
        // worker to stop after finishing any in-flight apply. The join is
        // polled with the same bounded deadline as the session thread; a
        // worker still mid-transaction (bounded on its own) is detached and
        // exits as soon as its completion send fails against the dropped
        // receiver — shutdown never waits for a full in-flight apply.
        if let Some(worker) = self.apply_worker.take() {
            let deadline = Instant::now() + APPLY_WORKER_JOIN_TIMEOUT;
            while !worker.is_finished() && Instant::now() < deadline {
                std::thread::sleep(Duration::from_millis(10));
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use std::sync::Mutex;

    use serde_json::json;

    use crate::supervisor::SupervisorService;

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
            output: None,
            apply: None,
        }
    }

    fn config_with(
        state_dir: PathBuf,
        valid: &[&str],
        supervisor: SupervisorHandle,
        output: Option<String>,
        apply: Option<Arc<dyn PlaylistApplyLane>>,
    ) -> PlaylistSessionConfig {
        PlaylistSessionConfig {
            state_dir,
            tick_ms: 50,
            supervisor: Some(supervisor),
            valid_ids: valid_ids(valid),
            output,
            apply,
        }
    }

    /// One recorded apply request: (output, wallpaper id, entry set).
    type ApplyCall = (Option<String>, String, BTreeSet<String>);

    /// A recording test lane: records every apply request (output +
    /// wallpaper id + playlist entries). `remaining_failures` lets a test
    /// fail the first N applies with a transaction error, `remaining_busy`
    /// with `ApplyError::Busy` (a user apply holding the lock), and
    /// `remaining_yielded` with `ApplyError::Yielded` (the post-lock foreign
    /// check) — each before falling through to success. A stub lane cannot
    /// manufacture supervisor-live state, so the session re-applies its
    /// entry every tick after a success — the real lane makes the
    /// supervisor non-idle, which the steady-state no-reapply is tested
    /// against (main.rs integration tests).
    #[derive(Clone)]
    struct RecordingLane {
        calls: Arc<Mutex<Vec<ApplyCall>>>,
        remaining_failures: Arc<std::sync::atomic::AtomicU64>,
        remaining_busy: Arc<std::sync::atomic::AtomicU64>,
        remaining_yielded: Arc<std::sync::atomic::AtomicU64>,
        /// SR-1c2: wallpaper ids this lane always refuses with
        /// `ApplyError::CapabilityGate` (a fixed "missing" list, since the
        /// tests using this only care about the session's exclusion/skip
        /// behavior, not the gate's own classification — that is already
        /// covered by `apply.rs`'s `scene_capability_gate` tests).
        gate_refused: Arc<Mutex<BTreeSet<String>>>,
    }

    impl RecordingLane {
        fn new() -> Self {
            Self {
                calls: Arc::new(Mutex::new(Vec::new())),
                remaining_failures: Arc::new(std::sync::atomic::AtomicU64::new(0)),
                remaining_busy: Arc::new(std::sync::atomic::AtomicU64::new(0)),
                remaining_yielded: Arc::new(std::sync::atomic::AtomicU64::new(0)),
                gate_refused: Arc::new(Mutex::new(BTreeSet::new())),
            }
        }

        fn failing(self, failures: u64) -> Self {
            self.remaining_failures
                .store(failures, std::sync::atomic::Ordering::SeqCst);
            self
        }

        /// Always refuses `ids` with `ApplyError::CapabilityGate` (SR-1c2).
        fn gate_refusing(self, ids: &[&str]) -> Self {
            self.gate_refused
                .lock()
                .unwrap()
                .extend(ids.iter().map(|id| id.to_string()));
            self
        }

        fn busy(self, count: u64) -> Self {
            self.remaining_busy
                .store(count, std::sync::atomic::Ordering::SeqCst);
            self
        }

        fn yielded(self, count: u64) -> Self {
            self.remaining_yielded
                .store(count, std::sync::atomic::Ordering::SeqCst);
            self
        }

        fn calls(&self) -> Vec<ApplyCall> {
            self.calls.lock().unwrap().clone()
        }

        fn call_count(&self) -> usize {
            self.calls.lock().unwrap().len()
        }
    }

    impl PlaylistApplyLane for RecordingLane {
        fn apply_playlist(
            &self,
            output: Option<String>,
            wallpaper_id: String,
            playlist_entries: &BTreeSet<String>,
            _applied: Option<&str>,
        ) -> Result<Value, ApplyError> {
            self.calls.lock().unwrap().push((
                output,
                wallpaper_id.clone(),
                playlist_entries.clone(),
            ));
            if self.gate_refused.lock().unwrap().contains(&wallpaper_id) {
                return Err(ApplyError::CapabilityGate {
                    detail: "scene requires capabilities this build does not implement: \
                             fake.missing.capability"
                        .into(),
                    missing: vec!["fake.missing.capability".into()],
                    inspection_reason: None,
                });
            }
            if self
                .remaining_failures
                .fetch_update(
                    std::sync::atomic::Ordering::SeqCst,
                    std::sync::atomic::Ordering::SeqCst,
                    |remaining| remaining.checked_sub(1),
                )
                .is_ok()
            {
                return Err(ApplyError::Transaction("stub lane failed".into()));
            }
            if self
                .remaining_busy
                .fetch_update(
                    std::sync::atomic::Ordering::SeqCst,
                    std::sync::atomic::Ordering::SeqCst,
                    |remaining| remaining.checked_sub(1),
                )
                .is_ok()
            {
                return Err(ApplyError::Busy);
            }
            if self
                .remaining_yielded
                .fetch_update(
                    std::sync::atomic::Ordering::SeqCst,
                    std::sync::atomic::Ordering::SeqCst,
                    |remaining| remaining.checked_sub(1),
                )
                .is_ok()
            {
                return Err(ApplyError::Yielded("stub lane foreign renderer".into()));
            }
            Ok(json!({ "applied": wallpaper_id }))
        }
    }

    /// A fresh supervisor dir for one session test.
    fn supervisor_dir() -> PathBuf {
        std::env::temp_dir().join(format!(
            "kwe-playlist-sup-{}-{}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap_or_default()
                .as_nanos()
        ))
    }

    /// A real supervisor service on `state_dir`: the test binary is a valid
    /// renderer path and no worker is ever launched — the session never
    /// starts one through the stub lane, and the foreign-yield tests
    /// fabricate worker state through the real supervisor's own API (or
    /// the pure verdict tests).
    fn start_supervisor(state_dir: PathBuf) -> (SupervisorService, SupervisorHandle) {
        let service = SupervisorService::start(supervisor_config(state_dir)).unwrap();
        let handle = service.handle();
        (service, handle)
    }

    fn supervisor_config(state_dir: PathBuf) -> crate::supervisor::SupervisorConfig {
        let limits = crate::supervisor::RendererResourceLimits {
            address_space_mib: 4096,
            file_size_mib: 160,
            open_files: 256,
            processes: 1024,
            core_dump_bytes: 0,
        };
        crate::supervisor::SupervisorConfig {
            renderer_paths: BTreeMap::from([(
                crate::supervisor::RendererKind::Test,
                std::env::current_exe().unwrap(),
            )]),
            runtime_dir: supervisor_dir(),
            state_dir,
            startup_timeout_ms_by_kind: BTreeMap::from([
                (crate::supervisor::RendererKind::Test, 3000),
                (crate::supervisor::RendererKind::Video, 6000),
                (crate::supervisor::RendererKind::Web, 10_000),
                (crate::supervisor::RendererKind::Scene, 3000),
            ]),
            frame_timeout: Duration::from_secs(2),
            stop_grace: Duration::from_millis(500),
            restart_delay: Duration::from_millis(250),
            canary_duration: Duration::from_secs(1),
            handoff_timeout: Duration::from_secs(5),
            max_failures: 3,
            web_heartbeat_ms: 5000,
            web_heartbeat_max_failures: 3,
            resource_limits_by_kind: BTreeMap::from([
                (crate::supervisor::RendererKind::Test, limits),
                (crate::supervisor::RendererKind::Video, limits),
                (crate::supervisor::RendererKind::Web, limits),
                (crate::supervisor::RendererKind::Scene, limits),
            ]),
            scene_assets_dir: None,
            shader_helper_path: None,
        }
    }

    /// A fresh, empty supervisor service.
    fn idle_supervisor() -> (SupervisorService, SupervisorHandle) {
        start_supervisor(supervisor_dir().join("state"))
    }

    /// A fresh supervisor service whose persisted state already quarantines
    /// `wallpaper_id` (legacy identity without a kind suffix, matching what
    /// pre-M4b daemons wrote). Written before the supervisor starts so the
    /// session's quarantine refresh sees the record on its first tick.
    fn quarantined_supervisor(
        wallpaper_id: &str,
        content_hash: &str,
    ) -> (SupervisorService, SupervisorHandle) {
        let state_dir = supervisor_dir().join("state");
        std::fs::create_dir_all(&state_dir).unwrap();
        // B4: records are scoped to the build that earned them; a fixture
        // without the running build's id would be dropped at load.
        let build_id = crate::supervisor::build_identity(
            &supervisor_config(state_dir.clone()).validate().unwrap(),
        );
        let fixture = json!({
            "schema_version": 1,
            "build_id": build_id,
            "forced_kill_count": 0,
            "records": {
                format!("{wallpaper_id}:{content_hash}"): {
                    "wallpaper_id": wallpaper_id,
                    "content_hash": content_hash,
                    "failures": 3,
                    "quarantined": true,
                    "last_failure": "frame_timeout",
                    "last_detail": "session test fixture",
                    "updated_unix_seconds": 1,
                }
            },
            "last_good": null,
        });
        std::fs::write(
            state_dir.join("supervisor-v1.json"),
            serde_json::to_vec(&fixture).unwrap(),
        )
        .unwrap();
        start_supervisor(state_dir)
    }

    /// Waits until the lane recorded an apply for `wallpaper_id`; returns
    /// the first matching call.
    fn wait_for_apply(lane: &RecordingLane, wallpaper_id: &str) -> ApplyCall {
        let deadline = Instant::now() + Duration::from_secs(5);
        loop {
            if let Some(call) = lane
                .calls()
                .into_iter()
                .find(|(_, id, _)| id == wallpaper_id)
            {
                return call;
            }
            assert!(
                Instant::now() < deadline,
                "timed out waiting for the lane to apply {wallpaper_id}; calls={:?}",
                lane.calls()
            );
            std::thread::sleep(Duration::from_millis(20));
        }
    }

    /// Waits until the lane recorded at least `count` applies (the worker
    /// runs the lane off the tick thread, so tests poll for the recorded
    /// count instead of assuming a synchronous apply).
    fn wait_for_calls(lane: &RecordingLane, count: usize) {
        let deadline = Instant::now() + Duration::from_secs(10);
        while lane.call_count() < count && Instant::now() < deadline {
            std::thread::sleep(Duration::from_millis(20));
        }
        assert!(
            lane.call_count() >= count,
            "expected at least {count} lane calls, saw {}",
            lane.call_count()
        );
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
        // Wait (bounded) until the session thread has ticked past Started
        // into Waiting; a fixed sleep starved on the 3-core macOS CI runner.
        let deadline = std::time::Instant::now() + Duration::from_secs(5);
        let before_remaining = loop {
            let before = handle.status().unwrap();
            match before.decision {
                Some(PlaylistDecision::Waiting { remaining_ms, .. }) => break remaining_ms,
                other if std::time::Instant::now() >= deadline => {
                    panic!("expected Waiting, got {other:?}")
                }
                _ => std::thread::sleep(Duration::from_millis(25)),
            }
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

    // --- BETA_M4c: renderer assignment through the apply lane ------------

    /// A fabricated worker state with only the fields the apply verdict
    /// reads (phase + requested wallpaper); the rest are inert defaults.
    /// `WorkerStatus` is fully constructible, which is what makes the pure
    /// precedence matrix below possible.
    fn fabricated_status(phase: WorkerPhase, requested: Option<&str>) -> WorkerStatus {
        let limits = crate::supervisor::RendererResourceLimits {
            address_space_mib: 4096,
            file_size_mib: 160,
            open_files: 256,
            processes: 1024,
            core_dump_bytes: 0,
        };
        WorkerStatus {
            phase,
            kind: crate::supervisor::RendererKind::Test,
            wallpaper_id: requested.map(str::to_string),
            content_hash: None,
            pid: None,
            frame_file: None,
            last_good_file: None,
            sequence: 0,
            failures: 0,
            restart_count: 0,
            forced_kill_count: 0,
            last_failure: None,
            last_failure_detail: None,
            quarantined: false,
            scaling: crate::supervisor::ScalingMode::Aspect,
            requested_wallpaper_id: requested.map(str::to_string),
            requested_content_hash: None,
            candidate_pid: None,
            candidate_frame_file: None,
            candidate_sequence: 0,
            previous_pid: None,
            previous_frame_file: None,
            display_generation: 0,
            awaiting_display_ack: false,
            resource_limits: limits,
            input_sequence: 0,
            input_ack_sequence: 0,
            input_pending: false,
            input_coalesced: 0,
            input_protocol_errors: 0,
            pointer_inside: false,
            pointer_x: 0,
            pointer_y: 0,
            audio_pending: false,
            audio_coalesced: 0,
            audio_grant_dropped: 0,
            media_pending: false,
            media_coalesced: 0,
            stderr_tail: Vec::new(),
            stderr_dropped_bytes: 0,
            capability_limitations: Vec::new(),
        }
    }

    #[test]
    fn apply_verdict_satisfied_yield_apply_matrix() {
        // The desired wallpaper is live or on its way: satisfied without a
        // re-apply — whether the session itself, the apply transaction, or
        // a user apply of the same wallpaper produced the state.
        for phase in [
            WorkerPhase::Starting,
            WorkerPhase::Canary,
            WorkerPhase::Live,
            WorkerPhase::AwaitingAck,
            WorkerPhase::Restarting,
            WorkerPhase::RolledBack,
        ] {
            let status = fabricated_status(phase, Some("1"));
            assert_eq!(
                apply_verdict(Some("1"), "1", &status, true),
                ApplyVerdict::Satisfied,
                "live {phase:?} on the desired wallpaper must satisfy"
            );
            assert_eq!(
                apply_verdict(None, "1", &status, true),
                ApplyVerdict::Satisfied,
                "a user apply of the desired wallpaper must satisfy in {phase:?}"
            );
        }

        // The session's OWN stale renderer (the timer advanced): the entry
        // change must displace it — hard cut through the apply transaction.
        let stale = fabricated_status(WorkerPhase::Live, Some("1"));
        assert_eq!(
            apply_verdict(Some("1"), "2", &stale, true),
            ApplyVerdict::Apply,
            "own stale renderer must be displaced with the backoff gate open"
        );
        // While the gate is closed (a previous apply failed), the
        // displacement must not storm the blocked transaction: hold the
        // tick and keep the applied identity — dropping it would reclassify
        // the session's own renderer as foreign and it would never be
        // displaced.
        assert_eq!(
            apply_verdict(Some("1"), "2", &stale, false),
            ApplyVerdict::Hold,
            "own stale renderer with a closed backoff gate must hold"
        );

        // A foreign (user/manager) renderer wins at any live phase: the
        // session yields and never fights it.
        for phase in [
            WorkerPhase::Starting,
            WorkerPhase::Canary,
            WorkerPhase::Live,
            WorkerPhase::AwaitingAck,
            WorkerPhase::Restarting,
            WorkerPhase::RolledBack,
        ] {
            let foreign = fabricated_status(phase, Some("2"));
            assert_eq!(
                apply_verdict(Some("1"), "1", &foreign, true),
                ApplyVerdict::Yield,
                "a foreign renderer in {phase:?} must win"
            );
        }

        // Nothing live: apply when the backoff gate is open (manual-stop
        // re-assert, restart restore, post-rollback retry)...
        let idle = fabricated_status(WorkerPhase::Idle, None);
        assert_eq!(
            apply_verdict(Some("1"), "1", &idle, true),
            ApplyVerdict::Apply,
            "idle with an open gate must apply"
        );
        let stopped = fabricated_status(WorkerPhase::Stopped, Some("1"));
        assert_eq!(
            apply_verdict(Some("1"), "1", &stopped, true),
            ApplyVerdict::Apply,
            "a stopped own renderer must be re-asserted"
        );
        let quarantined = fabricated_status(WorkerPhase::Quarantined, Some("2"));
        assert_eq!(
            apply_verdict(Some("1"), "1", &quarantined, true),
            ApplyVerdict::Apply,
            "a quarantined candidate must be retried once the gate opens"
        );
        // ...and the previous assignment is kept while the gate is closed
        // (a Hold, not a Yield: a Yield clears the session's applied identity
        // AND its backoff, which would re-fire the failed apply on the next
        // tick — the exact storm the gate exists to prevent).
        assert_eq!(
            apply_verdict(Some("1"), "1", &idle, false),
            ApplyVerdict::Hold,
            "a closed backoff gate must hold the previous assignment"
        );
        assert_eq!(
            apply_verdict(None, "1", &idle, false),
            ApplyVerdict::Hold,
            "a closed backoff gate must not apply for a fresh entry either"
        );
    }

    #[test]
    fn crash_restore_recovery_is_not_satisfied() {
        // The session requested "1"; the worker crashed and the supervisor is
        // recovering. requested == desired, but the ACTIVE worker renders a
        // different wallpaper (or none): claiming Satisfied would park the
        // session on an entry that is not actually displayed and would never
        // advance once the supervisor quarantines it. The session waits —
        // the supervisor's own bounded recovery or quarantine resolves it.
        for phase in [WorkerPhase::RolledBack, WorkerPhase::Restarting] {
            // The active worker renders a DIFFERENT wallpaper (the retired
            // worker survives the rollback).
            let mut status = fabricated_status(phase, Some("1"));
            status.wallpaper_id = Some("0".into());
            assert_eq!(
                apply_verdict(Some("1"), "1", &status, true),
                ApplyVerdict::Wait,
                "{phase:?} with a different active wallpaper must wait"
            );
            assert_eq!(
                apply_verdict(None, "1", &status, true),
                ApplyVerdict::Wait,
                "{phase:?} with a different active wallpaper must wait (fresh session too)"
            );
            // No active worker at all (the crashed worker was taken down and
            // nothing was retired).
            let mut status = fabricated_status(phase, Some("1"));
            status.wallpaper_id = None;
            assert_eq!(
                apply_verdict(Some("1"), "1", &status, true),
                ApplyVerdict::Wait,
                "{phase:?} with no active worker must wait"
            );
        }
        // Once the ACTIVE worker really renders the desired wallpaper, a
        // recovery phase is still Satisfied.
        let status = fabricated_status(WorkerPhase::Restarting, Some("1"));
        assert_eq!(
            apply_verdict(Some("1"), "1", &status, true),
            ApplyVerdict::Satisfied
        );
        // A desired renderer on its way (Starting/Canary) is not recovery:
        // it is Satisfied — the worker being started IS the desired one.
        for phase in [WorkerPhase::Starting, WorkerPhase::Canary] {
            let status = fabricated_status(phase, Some("1"));
            assert_eq!(
                apply_verdict(Some("1"), "1", &status, true),
                ApplyVerdict::Satisfied
            );
        }
        // The entry-change displacement of the session's own stale renderer
        // is unchanged during recovery: the timer advanced, so the hard cut
        // applies (never a competing apply DURING recovery of the desired
        // wallpaper — that is the Wait case above).
        let mut status = fabricated_status(WorkerPhase::Restarting, Some("1"));
        status.wallpaper_id = Some("1".into());
        assert_eq!(
            apply_verdict(Some("1"), "2", &status, true),
            ApplyVerdict::Apply,
            "own stale renderer is still displaced on an entry change during recovery"
        );
    }

    #[test]
    fn foreign_renderer_live_matrix() {
        // A live worker whose request is neither the desired wallpaper nor
        // the session's own applied one is a user/manager apply that won the
        // slot between the verdict and the lock: the lane must yield.
        for phase in [
            WorkerPhase::Starting,
            WorkerPhase::Canary,
            WorkerPhase::Live,
            WorkerPhase::AwaitingAck,
            WorkerPhase::Restarting,
            WorkerPhase::RolledBack,
        ] {
            let status = fabricated_status(phase, Some("2"));
            assert!(
                foreign_renderer_live(&status, "1", None),
                "live foreign {phase:?} (no applied identity) must be foreign"
            );
            assert!(
                foreign_renderer_live(&status, "1", Some("3")),
                "live foreign {phase:?} (applied 3) must be foreign"
            );
            // The session's own stale renderer (requested == applied) is NOT
            // foreign: the entry-change displacement must proceed.
            assert!(
                !foreign_renderer_live(&status, "1", Some("2")),
                "own stale renderer in {phase:?} is not foreign"
            );
            // The desired renderer itself is not foreign.
            assert!(
                !foreign_renderer_live(&status, "2", None),
                "the desired renderer in {phase:?} is not foreign"
            );
        }
        // No live worker is never foreign.
        let idle = fabricated_status(WorkerPhase::Idle, None);
        assert!(!foreign_renderer_live(&idle, "1", None));
        let stopped = fabricated_status(WorkerPhase::Stopped, Some("2"));
        assert!(!foreign_renderer_live(&stopped, "1", None));
        let quarantined = fabricated_status(WorkerPhase::Quarantined, Some("2"));
        assert!(!foreign_renderer_live(&quarantined, "1", None));
    }

    #[test]
    fn apply_backoff_doubles_to_the_thirty_second_cap() {
        assert_eq!(apply_backoff(1), Duration::from_secs(1));
        assert_eq!(apply_backoff(2), Duration::from_secs(2));
        assert_eq!(apply_backoff(3), Duration::from_secs(4));
        assert_eq!(apply_backoff(4), Duration::from_secs(8));
        assert_eq!(apply_backoff(5), Duration::from_secs(16));
        assert_eq!(apply_backoff(6), Duration::from_secs(30));
        assert_eq!(apply_backoff(99), Duration::from_secs(30));
    }

    #[test]
    fn entry_change_applies_through_the_lane_with_the_right_params() {
        let (_service, supervisor) = idle_supervisor();
        let lane = RecordingLane::new();
        let session = PlaylistSessionService::start(config_with(
            temporary_state_dir("entry-change"),
            &["1", "2", "3"],
            supervisor,
            Some("DP-1".into()),
            Some(Arc::new(lane.clone())),
        ));
        let handle = session.handle();
        handle.put(daily_playlist()).unwrap();
        handle.activate(Some("daily".into())).unwrap();

        // The first eligible entry is applied with the configured output
        // and the full entry set (the lane resolves the store against it).
        let (output, wallpaper_id, entries) = wait_for_apply(&lane, "1");
        assert_eq!(output.as_deref(), Some("DP-1"));
        assert_eq!(wallpaper_id, "1");
        assert_eq!(
            entries,
            BTreeSet::from(["1".into(), "2".into(), "3".into()])
        );

        // Timer advance applies the next entry with the same parameters:
        // once the 10 s entry expires on the real clock (clock skip freezes
        // remaining time by design and cannot advance the entry), the
        // session displaces its own stale renderer through the lane.
        std::thread::sleep(Duration::from_millis(11_000));
        let (output, wallpaper_id, entries) = wait_for_apply(&lane, "2");
        assert_eq!(output.as_deref(), Some("DP-1"));
        assert_eq!(wallpaper_id, "2");
        assert_eq!(
            entries,
            BTreeSet::from(["1".into(), "2".into(), "3".into()])
        );
    }

    #[test]
    fn quarantined_entry_is_never_applied() {
        let (_service, supervisor) = quarantined_supervisor("1", "hash-1");
        let lane = RecordingLane::new();
        let session = PlaylistSessionService::start(config_with(
            temporary_state_dir("quarantine-skip"),
            &["1", "2", "3"],
            supervisor,
            None,
            Some(Arc::new(lane.clone())),
        ));
        let handle = session.handle();
        handle.put(daily_playlist()).unwrap();
        handle.activate(Some("daily".into())).unwrap();

        // The decision skips the quarantined entry: the first apply is for
        // entry 2, and entry 1 never reaches the lane.
        let (_, wallpaper_id, _) = wait_for_apply(&lane, "2");
        assert_eq!(wallpaper_id, "2", "quarantined entry 1 must be skipped");
        std::thread::sleep(Duration::from_millis(600));
        assert!(
            lane.calls().iter().all(|(_, id, _)| id == "2"),
            "quarantined entry 1 must never reach the lane: {:?}",
            lane.calls()
        );
    }

    /// SR-1c2 (i): a gate-refused entry is SKIPPED and the playlist
    /// advances to the next entry — unlike a pre-known quarantine, the
    /// gate's refusal is discovered REACTIVELY (the lane is asked for
    /// entry 1 once, refuses with `ApplyError::CapabilityGate`), and only
    /// THEN is entry 1 excluded from the next decision, exactly the way a
    /// crash-quarantined entry already is (`unavailable_for`). No renderer
    /// is ever started for entry 1: `RecordingLane` never touches the
    /// supervisor, so the real supervisor here stays Idle throughout —
    /// the only apply that reaches "success" state is entry 2's.
    #[test]
    fn entry_gate_refused_is_skipped_and_the_playlist_advances() {
        let (_service, supervisor) = idle_supervisor();
        let lane = RecordingLane::new().gate_refusing(&["1"]);
        let session = PlaylistSessionService::start(config_with(
            temporary_state_dir("gate-refused-skip"),
            &["1", "2", "3"],
            supervisor,
            None,
            Some(Arc::new(lane.clone())),
        ));
        let handle = session.handle();
        handle.put(daily_playlist()).unwrap();
        handle.activate(Some("daily".into())).unwrap();

        // Entry 1 is attempted once (reactive discovery), refused, then
        // excluded — the decision moves on to entry 2 without ever
        // retrying entry 1.
        let (_, wallpaper_id, _) = wait_for_apply(&lane, "2");
        assert_eq!(wallpaper_id, "2", "gate-refused entry 1 must be skipped");
        std::thread::sleep(Duration::from_millis(600));
        assert_eq!(
            lane.calls().iter().filter(|(_, id, _)| id == "1").count(),
            1,
            "gate-refused entry 1 must be attempted exactly once, never retried: {:?}",
            lane.calls()
        );
        // Diagnosable via the session status the same way a quarantined
        // entry already is (SR-1c2 decision (b)).
        let status = handle.status().unwrap();
        assert!(
            status.unavailable_ids.iter().any(|id| id == "1"),
            "a gate-refused entry must show up as unavailable: {status:?}"
        );
    }

    #[test]
    fn failing_apply_backs_off_without_storming() {
        let (_service, supervisor) = idle_supervisor();
        let lane = RecordingLane::new().failing(3);
        let session = PlaylistSessionService::start(config_with(
            temporary_state_dir("backoff"),
            &["1"],
            supervisor,
            None,
            Some(Arc::new(lane.clone())),
        ));
        let handle = session.handle();
        handle.put(daily_playlist()).unwrap();
        handle.activate(Some("daily".into())).unwrap();

        // The first attempt fails immediately; the retries back off 1 s,
        // 2 s, 4 s. Within the first ~3.6 s exactly the three failing
        // attempts happen (a storm at the 50 ms tick would be dozens).
        wait_for_apply(&lane, "1");
        std::thread::sleep(Duration::from_millis(3600));
        let count = lane.call_count();
        assert!(
            (3..=4).contains(&count),
            "backoff must bound applies to 3..=4 within 3.6 s, saw {count}"
        );

        // The 4 s gate then opens and the next attempt succeeds.
        let deadline = Instant::now() + Duration::from_secs(10);
        while lane.call_count() < 4 && Instant::now() < deadline {
            std::thread::sleep(Duration::from_millis(50));
        }
        assert!(lane.call_count() >= 4, "a retry must eventually succeed");
    }

    #[test]
    fn busy_apply_is_a_transient_yield_that_never_arms_backoff() {
        // A user's wallpaper.apply holds the transaction lock; the lane
        // reports `Busy`. That is a transient yield (Finding 1), never a
        // failure: it must NOT count toward apply_failures or arm the
        // exponential backoff. A failure would delay the next attempt by the
        // 1 s base; the Busy yield leaves the gate open, so the re-assert
        // lands well inside that window.
        let (_service, supervisor) = idle_supervisor();
        let lane = RecordingLane::new().busy(1);
        let session = PlaylistSessionService::start(config_with(
            temporary_state_dir("busy-yield"),
            &["1"],
            supervisor,
            None,
            Some(Arc::new(lane.clone())),
        ));
        let handle = session.handle();
        handle.put(daily_playlist()).unwrap();
        handle.activate(Some("daily".into())).unwrap();
        // Call 1 is the Busy; the re-assert (call 2) must follow promptly.
        wait_for_calls(&lane, 1);
        let start = Instant::now();
        wait_for_calls(&lane, 2);
        let elapsed = start.elapsed();
        assert!(
            elapsed < Duration::from_millis(800),
            "Busy must not arm the backoff gate; the re-assert took {elapsed:?}"
        );
    }

    #[test]
    fn yielded_outcome_clears_the_backoff_gate_for_prompt_reassertion() {
        // Two real failures arm the backoff (1 s then 2 s), then the lane
        // reports the post-lock foreign-yield outcome (`Yielded`). The yield
        // must CLEAR the armed backoff (Finding 1): the re-assert after it
        // lands promptly instead of waiting out the failure backoff that the
        // two failures armed (which would be ~2 s more).
        let (_service, supervisor) = idle_supervisor();
        let lane = RecordingLane::new().failing(2).yielded(1);
        let session = PlaylistSessionService::start(config_with(
            temporary_state_dir("yield-clears-backoff"),
            &["1"],
            supervisor,
            None,
            Some(Arc::new(lane.clone())),
        ));
        let handle = session.handle();
        handle.put(daily_playlist()).unwrap();
        handle.activate(Some("daily".into())).unwrap();
        // Call 1 (fail) then call 2 (fail after the 1 s gate): both arm the
        // backoff; call 3 (Yielded) only dispatches once the 2 s gate opens.
        wait_for_calls(&lane, 3);
        // After the Yielded fold clears the backoff, call 4 (success) is
        // prompt — well inside the ~2 s the armed failure backoff would have
        // imposed.
        let start = Instant::now();
        wait_for_calls(&lane, 4);
        let elapsed = start.elapsed();
        assert!(
            elapsed < Duration::from_secs(1),
            "the Yielded fold must clear the backoff gate for a prompt re-assert, took {elapsed:?}"
        );
    }

    #[test]
    fn yield_clears_the_apply_backoff_state() {
        // The yield path (whether reached through the verdict for a live
        // foreign renderer or through the fold of a Busy/Yielded outcome)
        // must clear BOTH the applied identity AND the failure backoff gate
        // (Finding 1): after a foreign renderer stops, re-assertion is
        // prompt instead of delayed by failures the session accumulated
        // before the user took the slot.
        let dir = temporary_state_dir("yield-backoff-state");
        std::fs::create_dir_all(&dir).unwrap();
        let mut runtime = SessionRuntime::new(config(dir, &["1"]));
        runtime.applied_wallpaper = Some("1".into());
        runtime.apply_failures = 3;
        runtime.next_apply_retry = Instant::now() + Duration::from_secs(30);
        runtime.yield_to_foreign();
        assert_eq!(
            runtime.applied_wallpaper, None,
            "a yield forgets the applied identity"
        );
        assert_eq!(
            runtime.apply_failures, 0,
            "a yield clears the failure count"
        );
        assert!(
            Instant::now() >= runtime.next_apply_retry,
            "a yield must reopen the backoff gate for a prompt re-assert"
        );
    }

    #[test]
    fn restart_restore_reapplies_the_entry_once() {
        let state_dir = temporary_state_dir("restart-restore");
        // First session: entry 1 applies; dropping the session persists the
        // runtime position (the drop order is reverse declaration, so the
        // session shuts down before the supervisor service).
        {
            let (_service, supervisor) = idle_supervisor();
            let lane = RecordingLane::new();
            let session = PlaylistSessionService::start(config_with(
                state_dir.clone(),
                &["1", "2", "3"],
                supervisor,
                None,
                Some(Arc::new(lane.clone())),
            ));
            let handle = session.handle();
            handle.put(daily_playlist()).unwrap();
            handle.activate(Some("daily".into())).unwrap();
            wait_for_apply(&lane, "1");
        }
        // Second session on the same state: the runtime restores entry 1,
        // the fresh supervisor is idle, and the session re-applies the
        // restored entry once through the lane.
        let (_service, supervisor) = idle_supervisor();
        let lane = RecordingLane::new();
        let _session = PlaylistSessionService::start(config_with(
            state_dir,
            &["1", "2", "3"],
            supervisor,
            None,
            Some(Arc::new(lane.clone())),
        ));
        let (_, wallpaper_id, _) = wait_for_apply(&lane, "1");
        assert_eq!(wallpaper_id, "1", "the restored entry must be re-applied");
        assert!(
            lane.calls().iter().all(|(_, id, _)| id == "1"),
            "the restored session must stay on entry 1: {:?}",
            lane.calls()
        );
    }
}
