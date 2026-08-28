// SPDX-License-Identifier: GPL-3.0-or-later
//! Small Alpha control service. The newline-delimited protocol is deliberately
//! bounded and versioned so the UI never parses Workshop content itself.

mod apply;
mod audio;
mod grants;
mod inspect;
mod persist;
mod playlist_session;
mod selfcheck;
mod supervisor;
mod workshop_cache;

use std::{
    collections::{BTreeMap, BTreeSet},
    fs,
    io::{BufReader, Read, Write},
    os::fd::AsRawFd,
    os::unix::fs::{FileTypeExt, PermissionsExt},
    os::unix::net::{UnixListener, UnixStream},
    path::{Path, PathBuf},
    sync::{
        Arc, RwLock,
        atomic::{AtomicBool, AtomicU32, AtomicU64, Ordering},
    },
    time::{Duration, Instant},
};

use anyhow::{Context, Result, anyhow, bail};
use apply::{ApplyConfig, ApplyHandle, ApplyService, ApplyWallpaperParams, RestoreWallpaperParams};
use audio::{AudioCaptureConfig, AudioCaptureHandle, AudioCaptureService};
use clap::Parser;
use grants::GrantPatch;
use inspect::InspectConfig;
use kwe_core::{Catalog, ProjectKind, ScanLimits, default_steam_roots, scan_installed};
use kwe_input_protocol::{AudioFrame, MediaState, PointerButton, PointerPhase};
use playlist_session::{
    ImportPlaylist, PlaylistSessionConfig, PlaylistSessionHandle, PlaylistSessionService,
    SessionError,
};
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};
use supervisor::{
    ContentSpec, RendererKind, RendererResourceLimits, ScalingMode, StartSpec, SupervisorConfig,
    SupervisorHandle, SupervisorService, TestFault, WorkerStatus, validate_identity_part,
};
use workshop_cache::WorkshopCache;

const API_VERSION: u32 = 1;
const MAX_REQUEST_BYTES: usize = 64 * 1024;
/// `playlist.import` carries a whole legacy playlist blob (up to 4 MiB, the
/// manager's historical store bound) and is capped separately.
const MAX_IMPORT_REQUEST_BYTES: usize = 4 * 1024 * 1024 + 1024;
/// Byte marker used to tell import requests from other methods before the
/// full JSON parse. The post-parse method check stays authoritative.
const IMPORT_MARKER: &[u8] = b"playlist.import";

/// Review R1 (SR-0b): `scene.inspect` is the only RPC whose backing work can
/// take up to `--inspector-wall-timeout-ms` (30 s max) — every other method
/// answers off in-memory state inline on the single-threaded accept loop.
/// `dispatch_scene_inspect` therefore runs the actual inspection on a
/// dedicated thread instead of inline, so the accept loop keeps answering
/// every other connection while an inspection is outstanding. This flag
/// bounds that off-loop work to at most ONE thread at a time (single
/// in-flight inspection, no thread-per-request growth): a second
/// `scene.inspect` arriving while the flag is held answers
/// `inspector-busy` immediately instead of spawning a second thread/process
/// or queuing.
static INSPECT_IN_FLIGHT: AtomicBool = AtomicBool::new(false);

#[derive(Debug, Parser)]
#[command(version, about = "Crash-contained KDE Wallpaper Engine user service")]
struct Arguments {
    /// Unix socket path. Defaults beneath XDG_RUNTIME_DIR.
    #[arg(long)]
    socket: Option<PathBuf>,
    /// Override Steam root discovery. May be specified more than once.
    #[arg(long = "steam-root")]
    steam_roots: Vec<PathBuf>,
    /// Exit after handling one connection (integration-test helper).
    #[arg(long)]
    once: bool,
    /// Test renderer executable supervised by this daemon.
    #[arg(long)]
    renderer: Option<PathBuf>,
    /// Video renderer executable (default: kwe-video-renderer beside the daemon).
    #[arg(long)]
    renderer_video: Option<PathBuf>,
    /// Web renderer executable (default: kwe-web-renderer beside the daemon).
    #[arg(long)]
    renderer_web: Option<PathBuf>,
    /// Scene renderer executable (default: kwe-scene-renderer beside the daemon).
    #[arg(long)]
    renderer_scene: Option<PathBuf>,
    /// Scene inspector executable for `scene.inspect` (SR-0b; default:
    /// kwe-scene-inspector beside the daemon). Missing/unconfigured fails
    /// the RPC closed with `inspector-unavailable` rather than falling back
    /// to any other binary.
    #[arg(long)]
    inspector: Option<PathBuf>,
    /// Wall-clock deadline for one `scene.inspect` call; on expiry the
    /// inspector's process group is SIGKILLed and the RPC answers
    /// `{"outcome":"unknown","reason":"timeout"}`.
    #[arg(long, default_value_t = 10_000, value_parser = clap::value_parser!(u64).range(100..=30_000))]
    inspector_wall_timeout_ms: u64,
    /// Wallpaper Engine assets root (S1), passed to the scene worker and
    /// to scene preflight so model layers can resolve their material
    /// textures. Default: the first existing
    /// `<steam root>/steamapps/common/wallpaper_engine/assets` over the
    /// resolved Steam roots.
    #[arg(long = "wallpaper-engine-assets")]
    wallpaper_engine_assets: Option<PathBuf>,
    /// Private directory for ephemeral renderer frame files.
    #[arg(long)]
    renderer_runtime_dir: Option<PathBuf>,
    /// Private directory for quarantine state and the last-good still image.
    #[arg(long)]
    state_dir: Option<PathBuf>,
    #[arg(long, default_value_t = 3000, value_parser = clap::value_parser!(u64).range(100..=30000))]
    renderer_startup_timeout_ms: u64,
    /// Video renderers get more time than the test pattern needs.
    #[arg(long, default_value_t = 6000, value_parser = clap::value_parser!(u64).range(100..=30000))]
    renderer_video_startup_timeout_ms: u64,
    /// Scene startup includes one bounded libmpv load wait per VideoLayer;
    /// allow two sequential decoders to initialize without weakening the
    /// normal test-renderer deadline.
    #[arg(long, default_value_t = 6000, value_parser = clap::value_parser!(u64).range(100..=30000))]
    renderer_scene_startup_timeout_ms: u64,
    /// Chromium needs the most: cold start plus first screenshot.
    #[arg(long, default_value_t = 10000, value_parser = clap::value_parser!(u64).range(100..=30000))]
    renderer_web_startup_timeout_ms: u64,
    #[arg(long, default_value_t = 2000, value_parser = clap::value_parser!(u64).range(100..=30000))]
    renderer_frame_timeout_ms: u64,
    #[arg(long, default_value_t = 500, value_parser = clap::value_parser!(u64).range(20..=5000))]
    renderer_stop_grace_ms: u64,
    #[arg(long, default_value_t = 250, value_parser = clap::value_parser!(u64).range(0..=10000))]
    renderer_restart_delay_ms: u64,
    #[arg(long, default_value_t = 1000, value_parser = clap::value_parser!(u64).range(100..=30000))]
    renderer_canary_ms: u64,
    #[arg(long, default_value_t = 5000, value_parser = clap::value_parser!(u64).range(100..=30000))]
    renderer_handoff_timeout_ms: u64,
    #[arg(long, default_value_t = 3, value_parser = clap::value_parser!(u32).range(1..=10))]
    renderer_max_failures: u32,
    /// Maximum virtual address space for each renderer. Vulkan drivers reserve
    /// substantial virtual ranges, so the production default is intentionally
    /// higher than the service's aggregate resident-memory limit.
    #[arg(long, default_value_t = 4096, value_parser = clap::value_parser!(u64).range(256..=65536))]
    renderer_address_space_mib: u64,
    /// Maximum file descriptors available to each renderer.
    #[arg(long, default_value_t = 256, value_parser = clap::value_parser!(u64).range(32..=4096))]
    renderer_open_files: u64,
    /// Web renderers reserve a ~53 GiB virtual sandbox per Chromium process
    /// (V8 sandbox on chromium 151, measured), and the DevTools pipe bootstrap
    /// needs RLIMIT_AS above a ~98 GiB floor (below it the browser starts and
    /// renders but the CDP pipe never answers, failing silently with no
    /// stderr); the old 16 GiB budget SIGTRAPs the browser at exec. The 128
    /// GiB default clears the floor with margin. All of this is virtual —
    /// RSS stays ~250 MB per browser process (measured) and the resident
    /// protection comes from the supervisor timeouts.
    #[arg(long, default_value_t = 131072, value_parser = clap::value_parser!(u64).range(256..=262144))]
    renderer_web_address_space_mib: u64,
    /// Chromium needs more descriptors than the test renderer.
    #[arg(long, default_value_t = 1024, value_parser = clap::value_parser!(u64).range(32..=4096))]
    renderer_web_open_files: u64,
    /// UID-scoped process ceiling inherited by each renderer. The kernel's
    /// RLIMIT_NPROC check counts every thread of the uid (user->processes),
    /// so this guards the whole desktop, not the worker (docs/BETA_M1.md
    /// open risk 1): the video kind overrides it with
    /// --renderer-video-processes, and every kind's per-renderer protection
    /// comes from RLIMIT_AS plus the supervisor timeouts, not NPROC.
    #[arg(long, default_value_t = 1024, value_parser = clap::value_parser!(u64).range(64..=32768))]
    renderer_processes: u64,
    /// UID-scoped process ceiling for the video renderer kind only. A
    /// desktop session commonly runs more threads than the global 1024
    /// default (this machine measures ~1265), and libmpv fails to create
    /// threads once RLIMIT_NPROC binds; 32768 is the top of the validated
    /// range. Other kinds keep the global default.
    #[arg(long, default_value_t = 32768, value_parser = clap::value_parser!(u64).range(64..=32768))]
    renderer_video_processes: u64,
    /// UID-scoped process ceiling for the web renderer kind only. The same
    /// kernel check that broke libmpv fork/thread creation (the RLIMIT_NPROC
    /// limit counts every thread of the uid) hits the sandbox at spawn:
    /// bwrap forks the whole Chromium process tree, and the daemon cannot
    /// even exec the worker once the uid exceeds the 1024 global default
    /// (verified: the M2b worker fails "spawning bwrap" under 1024 on a
    /// ~1265-thread session).
    #[arg(long, default_value_t = 32768, value_parser = clap::value_parser!(u64).range(64..=32768))]
    renderer_web_processes: u64,
    /// UID-scoped process ceiling for the scene renderer, which owns up to
    /// two libmpv cores in VideoLayers.
    #[arg(long, default_value_t = 32768, value_parser = clap::value_parser!(u64).range(64..=32768))]
    renderer_scene_processes: u64,
    /// Session-scoped liveness probe interval for web renderers: the
    /// worker probes the page's renderer main thread every interval and
    /// exits 73 after consecutive failures (a page that wedges after first
    /// paint otherwise looks alive forever behind the keepalive
    /// re-publication).
    #[arg(long, default_value_t = 5000, value_parser = clap::value_parser!(u64).range(250..=60000))]
    renderer_web_heartbeat_ms: u64,
    /// Consecutive heartbeat failures before a web renderer exits 73.
    #[arg(long, default_value_t = 3, value_parser = clap::value_parser!(u32).range(1..=10))]
    renderer_web_heartbeat_max_failures: u32,
    /// Enable synthetic hang/corruption/exit requests for development tests.
    #[arg(long)]
    allow_test_faults: bool,
    /// Playlist session tick interval.
    #[arg(long, default_value_t = 500, value_parser = clap::value_parser!(u64).range(50..=5000))]
    playlist_tick_ms: u64,
    /// Spawn the bounded PipeWire audio capture worker (default: off).
    #[arg(long)]
    audio_capture: bool,
    /// Audio capture worker executable (default: kwe-audio-worker beside the
    /// daemon).
    #[arg(long)]
    audio_worker: Option<PathBuf>,
    /// PipeWire capture node passed through to the worker as --capture-node;
    /// lets tests direct the worker at a null sink instead of the user's
    /// real default sink.
    #[arg(long)]
    audio_capture_node: Option<String>,
    /// Plasma shell D-Bus service name for the wallpaper apply scripts.
    /// Plasma 6 registers org.kde.plasmashell (the Plasma 5-era
    /// org.kde.PlasmaShell alias no longer exists).
    #[arg(long, default_value = "org.kde.plasmashell")]
    plasma_shell_service: String,
    /// qdbus binary for the apply/restore scripts. Defaults to qdbus, then
    /// qdbus6, resolved from PATH at call time so the daemon starts fine on
    /// systems without either.
    #[arg(long)]
    qdbus_binary: Option<PathBuf>,
    /// Replace the whole Plasma shell evaluation boundary (enumeration and
    /// switch scripts alike) with this executable, run as `<path> <script>`
    /// (default: qdbus). Integration smokes stub the Plasma boundary with
    /// it so no live session is touched; live enablement (BETA_M4d) leaves
    /// it unset and runs the real qdbus.
    #[arg(long)]
    plasma_switch_command: Option<PathBuf>,
    /// kscreen-doctor binary used for the read-only output enumeration.
    #[arg(long, default_value = "kscreen-doctor")]
    kscreen_doctor_binary: PathBuf,
    /// systemctl binary used to recover a display environment when the
    /// daemon's own has none — a unit started before the desktop session
    /// (BETA B1). None resolves `systemctl` on PATH, and the daemon starts
    /// fine on systems without it (the recovery reports lazily).
    #[arg(long)]
    systemctl_binary: Option<PathBuf>,
    /// Deadline for every live Plasma probe (enumeration, switch, restore).
    #[arg(long, default_value_t = 5000, value_parser = clap::value_parser!(u64).range(500..=30000))]
    apply_probe_timeout_ms: u64,
    /// Deadline for the renderer to reach a live phase after wallpaper.apply.
    #[arg(long, default_value_t = 15000, value_parser = clap::value_parser!(u64).range(1000..=60000))]
    apply_promotion_timeout_ms: u64,
    /// Output that playlist-driven assignments target (BETA_M4c). None
    /// resolves at apply time to the last assigned output of the active
    /// playlist, else the first enabled and connected output.
    #[arg(long)]
    playlist_output: Option<String>,
}

#[derive(Debug, Deserialize)]
struct Request {
    version: u32,
    #[serde(default)]
    id: Value,
    method: String,
    #[serde(default)]
    params: Value,
}

#[derive(Debug, Serialize)]
struct Response<'a> {
    version: u32,
    id: &'a Value,
    ok: bool,
    result: Value,
}

fn main() -> Result<()> {
    let arguments = Arguments::parse();
    // B4: remember which executable this is, so a package upgrade that
    // swaps the file underneath a still-running daemon is detected and the
    // user is told to restart instead of the version skew turning into
    // quarantine records (docs/bugs/APPLY_REJECTED_QUARANTINED.md).
    match selfcheck::init() {
        Some(binary) => eprintln!("event=daemon.binary path={}", binary.path().display()),
        None => eprintln!("event=daemon.binary_unknown detail=stale-binary check disabled"),
    }
    let socket = arguments.socket.unwrap_or(default_socket_path()?);
    validate_socket_parent(&socket)?;
    if socket.exists() {
        let metadata = fs::symlink_metadata(&socket)?;
        if !metadata.file_type().is_socket() {
            bail!("refusing to replace non-socket path {}", socket.display());
        }
        match UnixStream::connect(&socket) {
            Ok(_) => bail!(
                "another daemon is already listening at {}",
                socket.display()
            ),
            Err(_) => fs::remove_file(&socket)
                .with_context(|| format!("remove stale socket {}", socket.display()))?,
        }
    }
    let roots = if arguments.steam_roots.is_empty() {
        default_steam_roots()
    } else {
        arguments.steam_roots
    };
    // S1 review #6: an explicit --wallpaper-engine-assets that does not
    // exist/is not a directory previously no-oped silently (every model
    // lookup against it simply fails `canonicalize()`, with nothing
    // naming why). Validate and log it explicitly instead of falling
    // through to auto-detection unannounced.
    let scene_assets_dir = match arguments.wallpaper_engine_assets {
        Some(explicit) if explicit.is_dir() => Some(explicit),
        Some(explicit) => {
            eprintln!(
                "event=daemon.config.invalid_assets_dir path={} detail=not-a-directory",
                explicit.display()
            );
            None
        }
        None => kwe_core::default_wallpaper_engine_assets_dir(&roots),
    };
    let default_paths = default_renderer_paths()?;
    let renderer_paths = BTreeMap::from([
        (
            RendererKind::Test,
            arguments
                .renderer
                .unwrap_or_else(|| default_paths[&RendererKind::Test].clone()),
        ),
        (
            RendererKind::Video,
            arguments
                .renderer_video
                .unwrap_or_else(|| default_paths[&RendererKind::Video].clone()),
        ),
        (
            RendererKind::Web,
            arguments
                .renderer_web
                .unwrap_or_else(|| default_paths[&RendererKind::Web].clone()),
        ),
        (
            RendererKind::Scene,
            arguments
                .renderer_scene
                .unwrap_or_else(|| default_paths[&RendererKind::Scene].clone()),
        ),
    ]);
    let renderer_runtime_dir = arguments
        .renderer_runtime_dir
        .unwrap_or_else(|| socket.parent().unwrap_or(Path::new(".")).join("renderers"));
    let state_dir = match arguments.state_dir {
        Some(path) => path,
        None => default_state_dir()?,
    };
    let playlist_state_dir = state_dir.clone();
    let apply_state_dir = state_dir.clone();
    let startup_timeout_ms_by_kind = BTreeMap::from([
        (RendererKind::Test, arguments.renderer_startup_timeout_ms),
        (
            RendererKind::Video,
            arguments.renderer_video_startup_timeout_ms,
        ),
        (RendererKind::Web, arguments.renderer_web_startup_timeout_ms),
        (
            RendererKind::Scene,
            arguments.renderer_scene_startup_timeout_ms,
        ),
    ]);
    let global_limits = RendererResourceLimits {
        address_space_mib: arguments.renderer_address_space_mib,
        file_size_mib: 160,
        open_files: arguments.renderer_open_files,
        processes: arguments.renderer_processes,
        core_dump_bytes: 0,
    };
    let video_limits = RendererResourceLimits {
        processes: arguments.renderer_video_processes,
        ..global_limits
    };
    let web_limits = RendererResourceLimits {
        address_space_mib: arguments.renderer_web_address_space_mib,
        open_files: arguments.renderer_web_open_files,
        processes: arguments.renderer_web_processes,
        ..global_limits
    };
    let scene_limits = RendererResourceLimits {
        processes: arguments.renderer_scene_processes,
        ..global_limits
    };
    let resource_limits_by_kind =
        resource_limits_for_kinds(global_limits, video_limits, web_limits, scene_limits);
    // SR-0b: scene.inspect's own containment config, built alongside the
    // supervisor's. The inspector reuses the scene renderer kind's resource
    // ceilings (never less contained than the renderer it stands in for)
    // and the same runtime dir the supervisor creates per-worker HOME dirs
    // under (distinct `inspect-home-*` naming avoids any collision).
    let inspect_config = InspectConfig {
        inspector_path: arguments.inspector.or_else(default_inspector_path),
        runtime_dir: renderer_runtime_dir.clone(),
        wall_timeout: Duration::from_millis(arguments.inspector_wall_timeout_ms),
        resource_limits: scene_limits,
    };
    let supervisor_service = SupervisorService::start(SupervisorConfig {
        renderer_paths,
        runtime_dir: renderer_runtime_dir,
        state_dir,
        startup_timeout_ms_by_kind,
        frame_timeout: Duration::from_millis(arguments.renderer_frame_timeout_ms),
        stop_grace: Duration::from_millis(arguments.renderer_stop_grace_ms),
        restart_delay: Duration::from_millis(arguments.renderer_restart_delay_ms),
        canary_duration: Duration::from_millis(arguments.renderer_canary_ms),
        handoff_timeout: Duration::from_millis(arguments.renderer_handoff_timeout_ms),
        max_failures: arguments.renderer_max_failures,
        web_heartbeat_ms: arguments.renderer_web_heartbeat_ms,
        web_heartbeat_max_failures: arguments.renderer_web_heartbeat_max_failures,
        resource_limits_by_kind,
        scene_assets_dir: scene_assets_dir.clone(),
    })?;
    let supervisor = supervisor_service.handle();
    let workshop_cache = Arc::new(std::sync::Mutex::new(WorkshopCache::open(
        &playlist_state_dir,
    )));
    let catalog = {
        let mut cache = workshop_cache
            .lock()
            .map_err(|_| anyhow!("workshop cache lock poisoned"))?;
        let mut initial_catalog = scan_installed(&roots, &ScanLimits::default());
        if cache.merge_and_update(&mut initial_catalog, unix_ms()) {
            cache.save();
        }
        Arc::new(RwLock::new(initial_catalog))
    };
    // The apply service must exist before the playlist session: the session
    // drives the same apply transaction the wallpaper.* API uses (BETA_M4c),
    // sharing its transaction lock so a playlist entry change and a user
    // apply never run concurrently.
    let apply_service = ApplyService::new(
        ApplyConfig {
            state_dir: apply_state_dir,
            shell_service: arguments.plasma_shell_service,
            qdbus_binary: arguments.qdbus_binary,
            switch_command: arguments.plasma_switch_command,
            kscreen_binary: arguments.kscreen_doctor_binary,
            systemctl_binary: arguments.systemctl_binary,
            probe_timeout: Duration::from_millis(arguments.apply_probe_timeout_ms),
            promotion_timeout: Duration::from_millis(arguments.apply_promotion_timeout_ms),
            scene_assets_dir: scene_assets_dir.clone(),
            // SR-1c: the identical `scene.inspect` containment config used
            // below for direct `scene.inspect` calls — the apply gate's
            // inspection is the same inspection, not a second copy of it.
            inspect_config: inspect_config.clone(),
        },
        catalog.clone(),
        supervisor.clone(),
    )?;
    let apply = apply_service.handle();
    if let Some(output) = &arguments.playlist_output {
        validate_identity_part("output", output)
            .with_context(|| format!("invalid --playlist-output {output:?}"))?;
    }
    let playlist_service = PlaylistSessionService::start(PlaylistSessionConfig {
        state_dir: playlist_state_dir,
        tick_ms: arguments.playlist_tick_ms,
        supervisor: Some(supervisor.clone()),
        valid_ids: compute_valid_ids(&catalog),
        output: arguments.playlist_output,
        apply: Some(Arc::new(apply.clone())),
    });
    let playlist = playlist_service.handle();
    // The audio capture service always runs so `audio.status` stays
    // answerable; without --audio-capture it stays idle and reports
    // enabled: false.
    let audio_worker_path = match arguments.audio_worker {
        Some(path) => path,
        None => default_audio_worker_path()?,
    };
    let audio_service = AudioCaptureService::start(AudioCaptureConfig {
        enabled: arguments.audio_capture,
        worker_path: audio_worker_path,
        socket: socket.clone(),
        capture_node: arguments.audio_capture_node,
    })?;
    let audio = audio_service.handle();
    let worker_pid = audio_service.worker_pid();
    let listener =
        UnixListener::bind(&socket).with_context(|| format!("bind {}", socket.display()))?;
    fs::set_permissions(&socket, fs::Permissions::from_mode(0o600))?;
    let initial_count = catalog
        .read()
        .map_err(|_| anyhow!("catalog lock poisoned"))?
        .stats
        .total;
    eprintln!(
        "kwe-daemon ready: {} projects; socket {}",
        initial_count,
        socket.display()
    );
    for connection in listener.incoming() {
        match connection {
            Ok(stream) => {
                stream.set_read_timeout(Some(Duration::from_secs(5))).ok();
                stream.set_write_timeout(Some(Duration::from_secs(5))).ok();
                if let Err(error) = handle_client(
                    stream,
                    &catalog,
                    &roots,
                    &supervisor,
                    &playlist,
                    &workshop_cache,
                    &audio,
                    &worker_pid,
                    &apply,
                    &inspect_config,
                    arguments.allow_test_faults,
                ) {
                    eprintln!("event=api.client_error detail={error}");
                }
            }
            Err(error) => eprintln!("event=api.accept_error detail={error}"),
        }
        if arguments.once {
            break;
        }
    }
    drop(listener);
    let _ = fs::remove_file(&socket);
    Ok(())
}

/// Collects one newline-terminated request line from `reader`.
///
/// Requests are capped at `MAX_IMPORT_REQUEST_BYTES` overall; anything that
/// does not carry the `playlist.import` marker is rejected once it exceeds
/// `MAX_REQUEST_BYTES`, before the rest is buffered or parsed. The marker
/// scan is incremental (newest chunk plus overlap) so large imports stay
/// linear in request size. Slow reads sleep briefly instead of busy-spinning.
fn read_request_line<R: Read>(reader: &mut R, deadline: Instant) -> Result<Vec<u8>> {
    let mut line = Vec::new();
    // Whether IMPORT_MARKER has been seen anywhere in the buffered bytes so
    // far; memoized so each chunk is scanned at most once.
    let mut saw_import_marker = false;
    loop {
        if line.contains(&b'\n') {
            break;
        }
        if Instant::now() >= deadline {
            bail!("request deadline exceeded");
        }
        let mut chunk = [0u8; 8192];
        match reader.read(&mut chunk) {
            Ok(0) => break,
            Ok(count) => {
                // Overlap the previous tail so a marker spanning a chunk
                // boundary is still found; re-scanning the whole line per
                // chunk would be quadratic for large import payloads.
                let scan_start = line.len().saturating_sub(IMPORT_MARKER.len() - 1);
                line.extend_from_slice(&chunk[..count]);
                if line.len() > MAX_IMPORT_REQUEST_BYTES {
                    bail!("request exceeded {MAX_IMPORT_REQUEST_BYTES} bytes");
                }
                if !saw_import_marker {
                    saw_import_marker = line[scan_start..]
                        .windows(IMPORT_MARKER.len())
                        .any(|w| w == IMPORT_MARKER);
                }
                // Reject oversized non-import requests early to avoid buffering
                // and parsing up to 4 MiB for methods that only accept 64 KiB.
                // The post-parse method check stays authoritative.
                if line.len() > MAX_REQUEST_BYTES && !saw_import_marker {
                    bail!("request exceeded {MAX_REQUEST_BYTES} bytes");
                }
            }
            Err(error)
                if matches!(
                    error.kind(),
                    std::io::ErrorKind::WouldBlock | std::io::ErrorKind::TimedOut
                ) =>
            {
                std::thread::sleep(Duration::from_millis(1));
            }
            Err(error) => return Err(error.into()),
        }
    }
    if !line.contains(&b'\n') {
        bail!("request ended without a newline");
    }
    Ok(line)
}

// The API layer intentionally takes its context explicitly so unit tests can
// drive process_request directly; a context struct would only re-bundle what
// the boundary already names.
#[allow(clippy::too_many_arguments)]
fn handle_client(
    mut stream: UnixStream,
    catalog: &Arc<RwLock<Catalog>>,
    roots: &[PathBuf],
    supervisor: &SupervisorHandle,
    playlist: &PlaylistSessionHandle,
    workshop_cache: &Arc<std::sync::Mutex<WorkshopCache>>,
    audio: &AudioCaptureHandle,
    worker_pid: &Arc<AtomicU32>,
    apply: &ApplyHandle,
    inspect: &InspectConfig,
    allow_test_faults: bool,
) -> Result<()> {
    let cloned = stream.try_clone()?;
    let mut reader = BufReader::new(cloned).take((MAX_IMPORT_REQUEST_BYTES + 1) as u64);
    // One request per connection, but the per-read 5s timeout alone lets a
    // trickling peer hold the single-threaded accept loop open indefinitely.
    // Enforce an overall deadline for collecting the request line.
    let request_deadline = Instant::now() + Duration::from_secs(10);
    let line = read_request_line(&mut reader, request_deadline)?;
    let request: Request = serde_json::from_slice(&line).context("invalid request JSON")?;
    if line.len() > MAX_REQUEST_BYTES && request.method != "playlist.import" {
        bail!("request exceeded {MAX_REQUEST_BYTES} bytes");
    }
    // R1 (SR-0b review): scene.inspect is the one RPC that can legitimately
    // take up to 30 s, so it never reaches the generic process_request call
    // below — that call is inline on the single-threaded accept loop, and
    // every other connection (renderer.status, wallpaper.apply, the
    // pointer/audio relays) would stall behind it otherwise. Param
    // validation stays inline and fast; only a validated, non-busy request
    // hands off to a dedicated thread. See `dispatch_scene_inspect`.
    if request.method == "scene.inspect" {
        return dispatch_scene_inspect(stream, request, inspect);
    }
    // Peer credential identification: the daemon's own audio worker is
    // recognized by pid and uid so its no-active-renderer rejections can be
    // dropped silently instead of erroring the caller's connection.
    // SO_PEERCRED is read directly because std's peer_cred() is still
    // feature-gated.
    let peer = peer_cred(&stream);
    let (ok, result) = process_request(
        &request,
        catalog,
        roots,
        Some(supervisor),
        Some(playlist),
        workshop_cache,
        Some(audio),
        Some(apply),
        Some(inspect),
        peer,
        worker_pid,
        allow_test_faults,
    )?;
    let response = Response {
        version: API_VERSION,
        id: &request.id,
        ok,
        result,
    };
    write_response(&mut stream, &response)
}

/// Writes one response envelope exactly as `handle_client` always has:
/// newline-delimited JSON, flushed. Shared by the normal inline path and
/// `dispatch_scene_inspect`'s off-thread path so both produce an identical
/// wire response.
fn write_response(stream: &mut UnixStream, response: &Response) -> Result<()> {
    serde_json::to_writer(&mut *stream, response)?;
    stream.write_all(b"\n")?;
    stream.flush()?;
    Ok(())
}

/// Clears `INSPECT_IN_FLIGHT` when dropped — including when the spawned
/// thread's closure unwinds from a panic — so a bug in `run_inspection`
/// can never leave the single-inspection gate stuck held forever.
struct InspectInFlightGuard;

impl Drop for InspectInFlightGuard {
    fn drop(&mut self) {
        INSPECT_IN_FLIGHT.store(false, Ordering::Release);
    }
}

/// `scene.inspect` params validation, shared by `dispatch_scene_inspect`'s
/// fast inline path and `process_request`'s normal dispatch (used directly
/// by the RPC unit tests): parse, then reject an empty or relative `path`
/// exactly like `permissions.get`/`permissions.set` reject a malformed
/// `wallpaper_id`. Returns the validated path, or the typed error value to
/// answer with.
fn validate_scene_inspect_params(params: &Value) -> Result<String, Value> {
    match serde_json::from_value::<SceneInspectParams>(params.clone()) {
        Ok(params) if params.path.is_empty() || !Path::new(&params.path).is_absolute() => {
            Err(json!({
                "error": "invalid_params",
                "detail": "path must be a non-empty absolute path"
            }))
        }
        Ok(params) => Ok(params.path),
        Err(error) => Err(json!({"error": "invalid_params", "detail": error.to_string()})),
    }
}

/// Off-loop handling for one `scene.inspect` request (R1, SR-0b review).
/// Validation is inline and fast. A valid request either answers
/// `inspector-busy` immediately (the single-in-flight gate is already
/// held) or is handed to exactly one spawned thread that runs
/// `inspect::run_inspection` and writes the response itself — bounding the
/// daemon to at most one such thread at a time (see `INSPECT_IN_FLIGHT`).
/// Every other error path (bad params, busy) answers synchronously here,
/// on the accept-loop thread, since those are immediate and bounded.
fn dispatch_scene_inspect(
    mut stream: UnixStream,
    request: Request,
    inspect: &InspectConfig,
) -> Result<()> {
    let validated = validate_scene_inspect_params(&request.params);
    let result = match validated {
        Err(error) => error,
        Ok(path) => {
            if INSPECT_IN_FLIGHT.swap(true, Ordering::AcqRel) {
                json!({"outcome": "unknown", "reason": "inspector-busy"})
            } else {
                let inspect = inspect.clone();
                let id = request.id;
                // Created before the spawn and moved into the closure:
                // if thread creation itself panics (pthread_create EAGAIN
                // under NPROC/TasksMax pressure), the unwound closure drops
                // the guard and the gate still clears.
                let guard = InspectInFlightGuard;
                std::thread::spawn(move || {
                    // Cleared on every exit path, panic included — including
                    // an explicit early drop right after the inspection
                    // finishes, below, so the gate is provably clear before
                    // the caller can observe the response.
                    let guard = guard;
                    let result = inspect::run_inspection(&inspect, Path::new(&path));
                    drop(guard);
                    let ok = result.get("error").is_none();
                    let response = Response {
                        version: API_VERSION,
                        id: &id,
                        ok,
                        result,
                    };
                    if let Err(error) = write_response(&mut stream, &response) {
                        eprintln!("event=inspect.response_error detail={error}");
                    }
                });
                return Ok(());
            }
        }
    };
    let ok = result.get("error").is_none();
    let response = Response {
        version: API_VERSION,
        id: &request.id,
        ok,
        result,
    };
    write_response(&mut stream, &response)
}

// See the note on handle_client: explicit context keeps the API layer
// directly testable.
#[allow(clippy::too_many_arguments)]
fn process_request(
    request: &Request,
    catalog: &Arc<RwLock<Catalog>>,
    roots: &[PathBuf],
    supervisor: Option<&SupervisorHandle>,
    playlist: Option<&PlaylistSessionHandle>,
    workshop_cache: &Arc<std::sync::Mutex<WorkshopCache>>,
    audio: Option<&AudioCaptureHandle>,
    apply: Option<&ApplyHandle>,
    inspect: Option<&InspectConfig>,
    peer: PeerCred,
    worker_pid: &Arc<AtomicU32>,
    allow_test_faults: bool,
) -> Result<(bool, Value)> {
    let result = if request.version != API_VERSION {
        json!({"error": "unsupported_api_version", "supported": API_VERSION})
    } else {
        match request.method.as_str() {
            "health" => {
                let count = catalog
                    .read()
                    .map_err(|_| anyhow!("catalog lock poisoned"))?
                    .stats
                    .total;
                json!({
                    "status": "ready",
                    "catalog_items": count,
                    // B4: true once the installed kwe-daemon file is no
                    // longer the running one (package upgraded, unit not
                    // restarted). Applies are refused while this is set.
                    "service_stale": selfcheck::is_stale(),
                })
            }
            "catalog" => {
                let guard = catalog
                    .read()
                    .map_err(|_| anyhow!("catalog lock poisoned"))?;
                serde_json::to_value(&*guard)?
            }
            "rescan" => {
                let mut cache = workshop_cache
                    .lock()
                    .map_err(|_| anyhow!("workshop cache lock poisoned"))?;
                let mut updated = scan_installed(roots, &ScanLimits::default());
                if cache.merge_and_update(&mut updated, unix_ms()) {
                    cache.save();
                }
                let count = updated.stats.total;
                *catalog
                    .write()
                    .map_err(|_| anyhow!("catalog lock poisoned"))? = updated;
                if let Some(playlist) = playlist
                    && !playlist.update_availability(compute_valid_ids(catalog))
                {
                    eprintln!("event=playlist.availability_dropped detail=command queue full");
                }
                json!({"catalog_items": count})
            }
            "playlist.list" => playlist_call(playlist, |handle| {
                handle
                    .list()
                    .map(|playlists| json!({"playlists": playlists}))
            }),
            "playlist.put" => {
                match serde_json::from_value::<PlaylistPutParams>(request.params.clone()) {
                    Ok(params) => playlist_call(playlist, |handle| handle.put(params.playlist)),
                    Err(error) => {
                        json!({"error": "invalid_params", "detail": error.to_string()})
                    }
                }
            }
            "playlist.remove" => {
                match serde_json::from_value::<PlaylistRemoveParams>(request.params.clone()) {
                    Ok(params) => playlist_call(playlist, |handle| {
                        handle
                            .remove(params.id)
                            .map(|removed| json!({"removed": removed}))
                    }),
                    Err(error) => {
                        json!({"error": "invalid_params", "detail": error.to_string()})
                    }
                }
            }
            "playlist.activate" => {
                match serde_json::from_value::<PlaylistActivateParams>(request.params.clone()) {
                    Ok(params) => playlist_call(playlist, |handle| handle.activate(params.id)),
                    Err(error) => {
                        json!({"error": "invalid_params", "detail": error.to_string()})
                    }
                }
            }
            "playlist.status" => playlist_call(playlist, |handle| handle.status()),
            "playlist.import" => {
                match serde_json::from_value::<PlaylistImportParams>(request.params.clone()) {
                    Ok(params) => playlist_call(playlist, |handle| handle.import(params.playlists)),
                    Err(error) => {
                        json!({"error": "invalid_params", "detail": error.to_string()})
                    }
                }
            }
            "playlist.debug-clock-skip" => {
                if !allow_test_faults {
                    json!({"error": "test_faults_disabled"})
                } else {
                    match serde_json::from_value::<PlaylistDebugClockSkipParams>(
                        request.params.clone(),
                    ) {
                        Ok(params) => {
                            playlist_call(playlist, |handle| handle.debug_clock_skip(params.ms))
                        }
                        Err(error) => {
                            json!({"error": "invalid_params", "detail": error.to_string()})
                        }
                    }
                }
            }
            "renderer.status" => supervisor_call(supervisor, |handle| handle.status()),
            "renderer.stop" => supervisor_call(supervisor, |handle| handle.stop()),
            "renderer.ack" => {
                match serde_json::from_value::<RendererAckParams>(request.params.clone()) {
                    Ok(params) => {
                        supervisor_call(supervisor, |handle| handle.acknowledge(params.generation))
                    }
                    Err(error) => {
                        json!({"error": "invalid_params", "detail": error.to_string()})
                    }
                }
            }
            "renderer.input" => {
                match serde_json::from_value::<RendererInputParams>(request.params.clone()) {
                    Ok(params) => supervisor_call(supervisor, |handle| {
                        handle.pointer_input(
                            params.generation,
                            params.phase,
                            params.x,
                            params.y,
                            params.button,
                        )
                    }),
                    Err(error) => {
                        json!({"error": "invalid_params", "detail": error.to_string()})
                    }
                }
            }
            "audio.forward" => {
                match serde_json::from_value::<AudioForwardParams>(request.params.clone()) {
                    Ok(params) => {
                        // The protocol constructor enforces 16/32/64 matching
                        // bands with finite values in 0..=1.
                        match AudioFrame::new(
                            params.generation,
                            params.frame.left,
                            params.frame.right,
                        ) {
                            Ok(frame) => {
                                let result = supervisor_call(supervisor, |handle| {
                                    handle.audio_frame(params.generation, frame)
                                });
                                // The daemon's own worker drops its frames
                                // latest-wins while no renderer is active;
                                // external callers still see the error.
                                match classify_audio_error(peer, worker_pid, &result) {
                                    Some(dropped) => dropped,
                                    None => result,
                                }
                            }
                            Err(error) => {
                                json!({"error": "invalid_params", "detail": error.to_string()})
                            }
                        }
                    }
                    Err(error) => {
                        json!({"error": "invalid_params", "detail": error.to_string()})
                    }
                }
            }
            "audio.status" => audio_call(audio, |handle| handle.status()),
            "permissions.get" => {
                match serde_json::from_value::<PermissionsGetParams>(request.params.clone()) {
                    Ok(params)
                        if validate_identity_part("wallpaper_id", &params.wallpaper_id)
                            .is_err() =>
                    {
                        json!({
                            "error": "invalid_params",
                            "detail": "wallpaper_id must be 1..=128 ASCII letters, digits, '.', '_', or '-'"
                        })
                    }
                    Ok(params) => permissions_call(supervisor, |handle| {
                        handle
                            .permissions_get(params.wallpaper_id)
                            .map(|grant| json!({"granted": grant}))
                    }),
                    Err(error) => {
                        json!({"error": "invalid_params", "detail": error.to_string()})
                    }
                }
            }
            "permissions.set" => {
                match serde_json::from_value::<PermissionsSetParams>(request.params.clone()) {
                    Ok(params)
                        if validate_identity_part("wallpaper_id", &params.wallpaper_id)
                            .is_err() =>
                    {
                        json!({
                            "error": "invalid_params",
                            "detail": "wallpaper_id must be 1..=128 ASCII letters, digits, '.', '_', or '-'"
                        })
                    }
                    Ok(params) => permissions_call(supervisor, |handle| {
                        handle
                            .permissions_set(
                                params.wallpaper_id,
                                GrantPatch {
                                    network: params.network,
                                    audio: params.audio,
                                    pointer: params.pointer,
                                },
                            )
                            .map(|grant| json!({"granted": grant}))
                    }),
                    Err(error) => {
                        json!({"error": "invalid_params", "detail": error.to_string()})
                    }
                }
            }
            "permissions.list" => permissions_call(supervisor, |handle| {
                handle
                    .permissions_list()
                    .map(|grants| json!({"grants": grants}))
            }),
            "media.state" => {
                match serde_json::from_value::<MediaStateParams>(request.params.clone()) {
                    Ok(params) => match MediaState::new(
                        params.generation,
                        &params.playback,
                        params.title,
                        params.artist,
                        params.album,
                        params.position_seconds,
                        params.duration_seconds,
                    ) {
                        Ok(state) => supervisor_call(supervisor, |handle| {
                            handle.media_state(params.generation, state)
                        }),
                        Err(error) => {
                            json!({"error": "invalid_params", "detail": error.to_string()})
                        }
                    },
                    Err(error) => {
                        json!({"error": "invalid_params", "detail": error.to_string()})
                    }
                }
            }
            "renderer.start" | "renderer.retry" => {
                let parsed = serde_json::from_value::<RendererStartParams>(request.params.clone());
                match parsed {
                    Ok(params)
                        if (params.test_fault.is_some() || params.stderr_lines.is_some())
                            && !allow_test_faults =>
                    {
                        json!({
                            "error": "test_faults_disabled",
                            "detail": "restart the daemon with --allow-test-faults for synthetic testing"
                        })
                    }
                    Ok(params) => match StartSpec::try_from(params).and_then(|spec| {
                        // S1 review #5: the same assets root spawn_worker
                        // forwards to the worker unconditionally, so
                        // preflight (here) and spawn agree regardless of
                        // which RPC validated the spec.
                        let assets_dir = apply.and_then(|handle| handle.scene_assets_dir());
                        spec.into_validated(assets_dir)
                    }) {
                        Ok(spec) if request.method == "renderer.retry" => {
                            supervisor_call(supervisor, |handle| handle.retry(spec))
                        }
                        Ok(spec) => supervisor_call(supervisor, |handle| handle.start(spec)),
                        Err(error) => {
                            json!({"error": "invalid_params", "detail": error.to_string()})
                        }
                    },
                    Err(error) => {
                        json!({"error": "invalid_params", "detail": error.to_string()})
                    }
                }
            }
            "wallpaper.outputs" => apply_call(apply, |handle| handle.outputs()),
            "wallpaper.apply" => {
                // A daemon whose binary was replaced on disk must not keep
                // driving applies: its preflight and failure contracts no
                // longer match the renderers it would spawn (B4 cause 2).
                // Refuse with the restart command; the playlist lane is
                // not gated (it never reaches this arm) so an active
                // playlist keeps rotating until the restart.
                if let Some(stale) = selfcheck::stale_error() {
                    return Ok((false, stale));
                }
                match serde_json::from_value::<ApplyWallpaperParams>(request.params.clone()) {
                    Ok(params) => apply_call(apply, |handle| handle.apply(params)),
                    Err(error) => {
                        json!({"error": "invalid_params", "detail": error.to_string()})
                    }
                }
            }
            "wallpaper.restore" => {
                match serde_json::from_value::<RestoreWallpaperParams>(request.params.clone()) {
                    Ok(params) => apply_call(apply, |handle| handle.restore(params.output)),
                    Err(error) => {
                        json!({"error": "invalid_params", "detail": error.to_string()})
                    }
                }
            }
            "wallpaper.assignments" => apply_call(apply, |handle| handle.assignments()),
            // R1 (SR-0b review): production traffic never reaches this arm —
            // handle_client special-cases scene.inspect onto a dedicated
            // thread before process_request is ever called (see
            // dispatch_scene_inspect) so one inspection cannot block every
            // other RPC on the single-threaded accept loop. This arm stays
            // for the RPC unit tests that drive process_request directly
            // (process_with_inspect) and shares the same validation helper
            // dispatch_scene_inspect uses, so both paths answer identically.
            "scene.inspect" => match validate_scene_inspect_params(&request.params) {
                Err(error) => error,
                Ok(path) => match inspect {
                    Some(config) => inspect::run_inspection(config, Path::new(&path)),
                    None => json!({"error": "inspect_unavailable"}),
                },
            },
            _ => json!({"error": "unknown_method"}),
        }
    };
    let ok = request.version == API_VERSION && result.get("error").is_none();
    Ok((ok, result))
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct RendererStartParams {
    wallpaper_id: String,
    content_hash: String,
    #[serde(default = "default_width")]
    width: u32,
    #[serde(default = "default_height")]
    height: u32,
    #[serde(default = "default_fps")]
    fps: u32,
    #[serde(default = "default_kind")]
    kind: RendererKind,
    /// Content path required for video/web/scene; rejected for test.
    content: Option<std::path::PathBuf>,
    test_fault: Option<TestFaultParams>,
    /// Development-only: ask the test renderer for this many stderr lines.
    stderr_lines: Option<u32>,
    /// F1: `aspect` (default) | `fill` | `stretch`.
    #[serde(default)]
    scaling: ScalingMode,
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct TestFaultParams {
    kind: String,
    after: u64,
    mib: Option<u64>,
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct RendererAckParams {
    generation: u64,
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct RendererInputParams {
    generation: u64,
    phase: PointerPhase,
    x: f64,
    y: f64,
    button: Option<PointerButton>,
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct AudioForwardParams {
    generation: u64,
    frame: AudioFrameParams,
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct AudioFrameParams {
    left: Vec<f32>,
    right: Vec<f32>,
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct MediaStateParams {
    generation: u64,
    playback: String,
    title: Option<String>,
    artist: Option<String>,
    album: Option<String>,
    position_seconds: Option<f64>,
    duration_seconds: Option<f64>,
}

/// `permissions.get` params (BETA_M2c): read the effective grant record for
/// one wallpaper.
#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct PermissionsGetParams {
    wallpaper_id: String,
}

/// `permissions.set` params (BETA_M2c): patch the stored record. Provided
/// fields replace their current values; omitted fields keep them. Unknown
/// fields are rejected so a typo cannot silently change policy.
#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct PermissionsSetParams {
    wallpaper_id: String,
    network: Option<bool>,
    audio: Option<bool>,
    pointer: Option<bool>,
}

/// `scene.inspect` params (SR-0b): the daemon rejects a relative or empty
/// `path` itself, the same way `permissions.get`/`permissions.set` reject a
/// malformed `wallpaper_id` before it ever reaches the handle.
#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct SceneInspectParams {
    path: String,
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct PlaylistPutParams {
    playlist: kwe_core::Playlist,
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct PlaylistRemoveParams {
    id: String,
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct PlaylistActivateParams {
    id: Option<String>,
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct PlaylistImportParams {
    playlists: Vec<ImportPlaylist>,
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct PlaylistDebugClockSkipParams {
    ms: u64,
}

impl TryFrom<RendererStartParams> for StartSpec {
    type Error = anyhow::Error;

    fn try_from(params: RendererStartParams) -> Result<Self> {
        let test_fault = match params.test_fault {
            None => None,
            Some(fault) => Some(match fault.kind.as_str() {
                "startup_hang" => TestFault::StartupHang,
                "hang" => TestFault::Hang { after: fault.after },
                "corrupt" => TestFault::Corrupt { after: fault.after },
                "exit" => TestFault::Exit { after: fault.after },
                "ignore_term_hang" => TestFault::IgnoreTermHang { after: fault.after },
                "memory_pressure" => TestFault::MemoryPressure {
                    after: fault.after,
                    mib: fault.mib.context("memory_pressure requires mib")?,
                },
                _ => bail!("unknown synthetic fault kind"),
            }),
        };
        let content = match (params.kind, params.content) {
            (RendererKind::Test, None) => None,
            (RendererKind::Video, Some(path)) => Some(ContentSpec::Video { path }),
            (RendererKind::Web, Some(path)) => Some(ContentSpec::Web { root: path }),
            (RendererKind::Scene, Some(path)) => Some(ContentSpec::Scene { path }),
            _ => bail!(
                "renderer kind {} requires a content path (test takes none)",
                params.kind.as_str()
            ),
        };
        let spec = Self {
            wallpaper_id: params.wallpaper_id,
            content_hash: params.content_hash,
            width: params.width,
            height: params.height,
            fps: params.fps,
            kind: params.kind,
            content,
            test_fault,
            stderr_lines: params.stderr_lines,
            scaling: params.scaling,
            capability_limitations: Vec::new(),
        };
        // Field mapping only — NOT validated here (S1 review #5): the
        // caller (the "renderer.start"/"renderer.retry" RPC arm) calls
        // `into_validated` with the daemon's configured assets root, read
        // from its `ApplyHandle`, so a scene started through this
        // low-level RPC gets the same scene preflight outcome the primary
        // `wallpaper.apply` path (apply.rs) would give it — `spawn_worker`
        // (supervisor.rs) already forwards the real assets root to the
        // worker unconditionally regardless of which RPC validated the
        // spec, so preflight disagreeing with it here was an avoidable,
        // fails-closed inconsistency between the two entry points.
        Ok(spec)
    }
}

/// The daemon's own worker is expected to keep capturing while no renderer is
/// promoted: its `audio.forward` calls are rejected with
/// `supervisor_failed` / "no promoted renderer is available for audio
/// forwarding" every window until a renderer appears. For the daemon's own
/// worker that is a silent latest-wins drop (the worker holds one frame and
/// keeps the generation refreshed); for every other caller the error shape is
/// preserved unchanged.
const NO_PROMOTED_RENDERER_DETAIL: &str = "no promoted renderer is available for audio forwarding";

/// Counter for the rate-limited silent-drop diagnostic (first 5, then every
/// thousandth) so a renderer-less session cannot flood the daemon log.
static AUDIO_DROP_LOGS: AtomicU64 = AtomicU64::new(0);

fn log_audio_drop() {
    let calls = AUDIO_DROP_LOGS.fetch_add(1, Ordering::Relaxed);
    if calls < 5 || calls.is_multiple_of(1000) {
        eprintln!(
            "event=audio.forward.dropped detail=no promoted renderer, daemon worker frames dropped latest-wins"
        );
    }
}

/// `Some({"status": "dropped"})` exactly when the request came from the
/// daemon's own audio worker (identified by pid and uid) and the supervisor
/// rejected it only because no renderer is currently promoted. Everything
/// else (stale generations, other callers, unknown credentials) returns
/// `None` and the original error result is surfaced.
fn classify_audio_error(
    peer: PeerCred,
    worker_pid: &Arc<AtomicU32>,
    result: &Value,
) -> Option<Value> {
    let managed_pid = worker_pid.load(Ordering::Acquire);
    // Same pid AND same user: a pid alone can be recycled after the worker
    // exits, so a later connection reusing that pid must not be mistaken
    // for our worker (pid-reuse hardening). geteuid never fails; it returns
    // the real effective uid of this process.
    // SAFETY: geteuid takes no arguments and cannot fault.
    let effective_uid = unsafe { libc::geteuid() };
    let is_managed_worker =
        managed_pid != 0 && peer.pid != 0 && peer.pid == managed_pid && peer.uid == effective_uid;
    if !is_managed_worker {
        return None;
    }
    if result.get("error").and_then(Value::as_str) != Some("supervisor_failed") {
        return None;
    }
    if result.get("detail").and_then(Value::as_str) != Some(NO_PROMOTED_RENDERER_DETAIL) {
        return None;
    }
    log_audio_drop();
    Some(json!({"status": "dropped"}))
}

fn audio_call(
    audio: Option<&AudioCaptureHandle>,
    call: impl FnOnce(&AudioCaptureHandle) -> Result<audio::AudioCaptureStatus>,
) -> Value {
    let Some(audio) = audio else {
        return json!({"error": "audio_unavailable"});
    };
    match call(audio) {
        Ok(status) => serde_json::to_value(status).unwrap_or_else(
            |error| json!({"error": "serialization_failed", "detail": error.to_string()}),
        ),
        Err(error) => json!({"error": "audio_failed", "detail": error.to_string()}),
    }
}

/// Permission grant RPC helper (BETA_M2c): serializes the effective record or
/// surfaces the failure as `permissions_failed` (bounds, identity errors, and
/// a full store all land here).
fn permissions_call<T: Serialize>(
    supervisor: Option<&SupervisorHandle>,
    call: impl FnOnce(&SupervisorHandle) -> Result<T>,
) -> Value {
    let Some(supervisor) = supervisor else {
        return json!({"error": "permissions_unavailable"});
    };
    match call(supervisor) {
        Ok(value) => serde_json::to_value(value).unwrap_or_else(
            |error| json!({"error": "serialization_failed", "detail": error.to_string()}),
        ),
        Err(error) => json!({"error": "permissions_failed", "detail": error.to_string()}),
    }
}

/// Live wallpaper apply RPC helper (BETA_M4a): maps the transaction result
/// to the wire contract error codes (docs/SUPERVISOR_API_V1.md).
fn apply_call(
    apply: Option<&ApplyHandle>,
    call: impl FnOnce(&ApplyHandle) -> Result<Value, apply::ApplyError>,
) -> Value {
    let Some(apply) = apply else {
        return json!({"error": "apply_unavailable"});
    };
    match call(apply) {
        Ok(value) => value,
        Err(error) => {
            let mut response = match error.detail() {
                Some(detail) => json!({"error": error.code(), "detail": detail}),
                None => json!({"error": error.code()}),
            };
            // SR-1c: `CapabilityGate`'s structured `missing`/
            // `inspection_reason` fields ride along as top-level siblings
            // of `error`/`detail` (`extra_fields` is `None` for every other
            // variant, so this is a no-op for them).
            if let (Some(object), Some(Value::Object(extra))) =
                (response.as_object_mut(), error.extra_fields())
            {
                object.extend(extra);
            }
            response
        }
    }
}

fn supervisor_call(
    supervisor: Option<&SupervisorHandle>,
    call: impl FnOnce(&SupervisorHandle) -> Result<WorkerStatus>,
) -> Value {
    let Some(supervisor) = supervisor else {
        return json!({"error": "supervisor_unavailable"});
    };
    match call(supervisor) {
        Ok(status) => serde_json::to_value(status).unwrap_or_else(
            |error| json!({"error": "serialization_failed", "detail": error.to_string()}),
        ),
        Err(error) => json!({"error": "supervisor_failed", "detail": error.to_string()}),
    }
}

fn playlist_call<T: Serialize>(
    playlist: Option<&PlaylistSessionHandle>,
    call: impl FnOnce(&PlaylistSessionHandle) -> Result<T, SessionError>,
) -> Value {
    let Some(playlist) = playlist else {
        return json!({"error": "playlist_unavailable"});
    };
    match call(playlist) {
        Ok(value) => serde_json::to_value(value).unwrap_or_else(
            |error| json!({"error": "serialization_failed", "detail": error.to_string()}),
        ),
        Err(error) => {
            let name = match &error {
                SessionError::NotFound(_) => "playlist_not_found",
                SessionError::ImportBlocked => "playlist_import_blocked",
                SessionError::StoreUnavailable(_) => "playlist_store_unavailable",
                SessionError::Invalid(_) => "invalid_playlist",
                SessionError::Busy(_) => "playlist_busy",
            };
            json!({"error": name, "detail": error.to_string()})
        }
    }
}

/// Installed, playable workshop ids: local or subscribed-and-present content
/// with valid project metadata. Everything else (absent, subscribed_missing,
/// downloading, invalid) is treated as unavailable by the playlist session.
fn compute_valid_ids(catalog: &Arc<RwLock<Catalog>>) -> Arc<BTreeSet<String>> {
    let Ok(guard) = catalog.read() else {
        eprintln!("event=playlist.availability_error detail=catalog lock poisoned");
        return Arc::new(BTreeSet::new());
    };
    Arc::new(
        guard
            .items
            .iter()
            .filter(|item| {
                matches!(
                    item.workshop_state.as_str(),
                    "local" | "subscribed_installed"
                ) && item.kind != ProjectKind::Invalid
            })
            .map(|item| item.workshop_id.clone())
            .collect(),
    )
}

const fn default_width() -> u32 {
    960
}

const fn default_height() -> u32 {
    540
}

const fn default_fps() -> u32 {
    30
}

const fn default_kind() -> RendererKind {
    RendererKind::Test
}

/// Per-kind pre-exec resource ceilings. The video kind carries its own
/// UID-scoped process ceiling (`--renderer-video-processes`) because the
/// kernel's RLIMIT_NPROC check counts every thread of the uid, so the
/// global default guards the whole desktop rather than the worker
/// (docs/BETA_M1.md open risk 1); the web kind keeps its separate
/// address-space/descriptor budget. Pure so the defaults and the CLI
/// override are unit-testable.
fn resource_limits_for_kinds(
    global: RendererResourceLimits,
    video: RendererResourceLimits,
    web: RendererResourceLimits,
    scene: RendererResourceLimits,
) -> BTreeMap<RendererKind, RendererResourceLimits> {
    BTreeMap::from([
        (RendererKind::Test, global),
        (RendererKind::Video, video),
        (RendererKind::Web, web),
        (RendererKind::Scene, scene),
    ])
}

fn default_socket_path() -> Result<PathBuf> {
    let runtime =
        std::env::var_os("XDG_RUNTIME_DIR").context("XDG_RUNTIME_DIR is not set; pass --socket")?;
    Ok(PathBuf::from(runtime).join("kwe/daemon-v1.sock"))
}

/// Default per-kind renderer binaries beside the daemon executable. Absent
/// binaries are tolerated at startup; requesting that kind fails closed at
/// spawn time instead of launching the wrong renderer.
fn default_renderer_paths() -> Result<BTreeMap<RendererKind, PathBuf>> {
    let executable = std::env::current_exe().context("resolve daemon executable")?;
    let directory = executable
        .parent()
        .context("daemon executable has no parent")?;
    Ok(BTreeMap::from([
        (RendererKind::Test, directory.join("kwe-test-renderer")),
        (RendererKind::Video, directory.join("kwe-video-renderer")),
        (RendererKind::Web, directory.join("kwe-web-renderer")),
        (RendererKind::Scene, directory.join("kwe-scene-renderer")),
    ]))
}

/// Default audio capture worker binary beside the daemon executable.
fn default_audio_worker_path() -> Result<PathBuf> {
    let executable = std::env::current_exe().context("resolve daemon executable")?;
    let directory = executable
        .parent()
        .context("daemon executable has no parent")?;
    Ok(directory.join("kwe-audio-worker"))
}

/// Default `kwe-scene-inspector` binary beside the daemon executable
/// (SR-0b). Unlike the renderer paths, this is allowed to resolve to
/// `None` — the inspector is optional/experimental, and a daemon that
/// cannot resolve its own executable path should still start; `scene.inspect`
/// then simply fails closed with `inspector-unavailable`.
fn default_inspector_path() -> Option<PathBuf> {
    let executable = std::env::current_exe().ok()?;
    let directory = executable.parent()?;
    Some(directory.join("kwe-scene-inspector"))
}

fn default_state_dir() -> Result<PathBuf> {
    if let Some(path) = std::env::var_os("XDG_STATE_HOME") {
        return Ok(PathBuf::from(path).join("kwe"));
    }
    let home = std::env::var_os("HOME").context("HOME is not set; pass --state-dir")?;
    Ok(PathBuf::from(home).join(".local/state/kwe"))
}

fn unix_ms() -> u128 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap_or_default()
        .as_millis()
}

/// Peer credentials of a Unix stream connection. `pid == 0` when the
/// credential query fails, which also makes the no-renderer drop never
/// apply.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
struct PeerCred {
    pid: u32,
    uid: u32,
}

/// Peer credentials of a Unix stream connection. SO_PEERCRED is read
/// directly because std's peer_cred() is still feature-gated.
fn peer_cred(stream: &UnixStream) -> PeerCred {
    let mut cred: libc::ucred = unsafe { std::mem::zeroed() };
    let mut len = std::mem::size_of::<libc::ucred>() as libc::socklen_t;
    // SAFETY: `cred` is a valid mutable ucred buffer and `len` its bound;
    // getsockopt fills it with the peer credentials of our own descriptor.
    let rc = unsafe {
        libc::getsockopt(
            stream.as_raw_fd(),
            libc::SOL_SOCKET,
            libc::SO_PEERCRED,
            (&mut cred as *mut libc::ucred).cast(),
            &mut len,
        )
    };
    if rc == 0 {
        PeerCred {
            pid: cred.pid as u32,
            uid: cred.uid,
        }
    } else {
        PeerCred::default()
    }
}

fn validate_socket_parent(socket: &Path) -> Result<()> {
    let parent = socket.parent().context("socket path has no parent")?;
    fs::create_dir_all(parent).with_context(|| format!("create {}", parent.display()))?;
    let metadata = fs::symlink_metadata(parent)?;
    if metadata.file_type().is_symlink() || !metadata.is_dir() {
        bail!("socket parent must be a real directory")
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use kwe_core::{Playlist, PlaylistDecision};
    use playlist_session::PlaylistApplyLane;

    use super::*;
    use supervisor::WorkerPhase;

    fn empty_catalog() -> Arc<RwLock<Catalog>> {
        Arc::new(RwLock::new(scan_installed(&[], &ScanLimits::default())))
    }

    fn sample_limits(processes: u64) -> RendererResourceLimits {
        RendererResourceLimits {
            address_space_mib: 4096,
            file_size_mib: 160,
            open_files: 256,
            processes,
            core_dump_bytes: 0,
        }
    }

    #[test]
    fn video_nproc_default_sits_above_the_desktop_thread_ceiling() {
        // The CLI defaults are the contract: the video AND web kinds get
        // 32768 (top of the validated range) while every other kind keeps
        // the global 1024. A desktop session commonly runs more than 1024
        // threads of the uid (this machine measures ~1265), and libmpv then
        // fails to create threads; the web worker's bwrap fork dies the same
        // way ("spawning bwrap" EAGAIN). The smoke runs without any override
        // as the proof.
        let arguments = Arguments::parse_from(["kwe-daemon"]);
        assert_eq!(arguments.renderer_processes, 1024);
        assert_eq!(arguments.renderer_video_processes, 32768);
        assert_eq!(arguments.renderer_web_processes, 32768);
        assert_eq!(arguments.renderer_scene_processes, 32768);
        assert_eq!(arguments.renderer_scene_startup_timeout_ms, 6000);
        // The web address-space budget sits above the measured CDP floor: a
        // chromium 151 process reserves ~53 GiB of virtual space for the V8
        // sandbox before main (16 GiB SIGTRAPs at startup), and the DevTools
        // pipe bootstrap silently refuses to answer below ~98 GiB. 128 GiB is
        // the production default, clear of the floor with margin.
        assert_eq!(arguments.renderer_web_address_space_mib, 131072);
        // The web liveness heartbeat defaults: probe every 5 s, exit 73
        // after 3 consecutive failures (a page wedged after first paint
        // otherwise looks alive forever behind the keepalive).
        assert_eq!(arguments.renderer_web_heartbeat_ms, 5000);
        assert_eq!(arguments.renderer_web_heartbeat_max_failures, 3);
    }

    #[test]
    fn per_kind_process_ceiling_applies_to_video_web_and_scene() {
        let global = sample_limits(1024);
        let video = sample_limits(32768);
        let web = RendererResourceLimits {
            address_space_mib: 131_072,
            open_files: 1024,
            processes: 32768,
            ..global
        };
        let scene = sample_limits(32768);
        let map = resource_limits_for_kinds(global, video, web, scene);
        assert_eq!(map[&RendererKind::Video].processes, 32768);
        assert_eq!(map[&RendererKind::Web].processes, 32768);
        assert_eq!(map[&RendererKind::Test].processes, 1024);
        // The web budget is untouched by the video knob.
        assert_eq!(map[&RendererKind::Web].address_space_mib, 131_072);
        assert_eq!(map[&RendererKind::Web].open_files, 1024);
        // The CLI override feeds the same helper: an explicit
        // --renderer-web-processes replaces the 32768 default for web only,
        // and a kind not overridden keeps the global default.
        let overridden = sample_limits(4096);
        let web_overridden = RendererResourceLimits {
            processes: 8192,
            ..web
        };
        let scene_overridden = RendererResourceLimits {
            processes: 16384,
            ..scene
        };
        let map = resource_limits_for_kinds(global, overridden, web_overridden, scene_overridden);
        assert_eq!(map[&RendererKind::Video].processes, 4096);
        assert_eq!(map[&RendererKind::Web].processes, 8192);
        assert_eq!(map[&RendererKind::Test].processes, 1024);
        assert_eq!(map[&RendererKind::Scene].processes, 16384);
    }

    fn cache_for_tests() -> Arc<std::sync::Mutex<WorkshopCache>> {
        let dir = std::env::temp_dir().join(format!(
            "kwe-daemon-cache-{}-{}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap_or_default()
                .as_nanos()
        ));
        std::fs::create_dir_all(&dir).unwrap();
        Arc::new(std::sync::Mutex::new(WorkshopCache::open(&dir)))
    }

    /// A real supervisor service (grants store included) for the
    /// permissions.* round trips; the test binary is a valid renderer path
    /// and no worker is ever launched by these tests.
    fn supervisor_service() -> SupervisorService {
        let dir = std::env::temp_dir().join(format!(
            "kwe-daemon-api-{}-{}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap_or_default()
                .as_nanos()
        ));
        let limits = RendererResourceLimits {
            address_space_mib: 4096,
            file_size_mib: 160,
            open_files: 256,
            processes: 1024,
            core_dump_bytes: 0,
        };
        SupervisorService::start(SupervisorConfig {
            renderer_paths: BTreeMap::from([(
                RendererKind::Test,
                std::env::current_exe().unwrap(),
            )]),
            runtime_dir: dir.join("runtime"),
            state_dir: dir.join("state"),
            startup_timeout_ms_by_kind: BTreeMap::from([
                (RendererKind::Test, 3000),
                (RendererKind::Video, 6000),
                (RendererKind::Web, 10_000),
                (RendererKind::Scene, 3000),
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
                (RendererKind::Test, limits),
                (RendererKind::Video, limits),
                (RendererKind::Web, limits),
                (RendererKind::Scene, limits),
            ]),
            scene_assets_dir: None,
        })
        .unwrap()
    }

    fn session_service() -> PlaylistSessionService {
        let dir = std::env::temp_dir().join(format!(
            "kwe-daemon-api-{}-{}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap_or_default()
                .as_nanos()
        ));
        PlaylistSessionService::start(PlaylistSessionConfig {
            state_dir: dir,
            tick_ms: 50,
            supervisor: None,
            valid_ids: Arc::new(BTreeSet::new()),
            output: None,
            apply: None,
        })
    }

    #[test]
    fn health_round_trip_preserves_request_id() {
        let catalog = empty_catalog();
        let request: Request =
            serde_json::from_str(r#"{"version":1,"id":"test-7","method":"health"}"#).unwrap();
        let (ok, result) = process_request(
            &request,
            &catalog,
            &[],
            None,
            None,
            &cache_for_tests(),
            None,
            None,
            None,
            PeerCred::default(),
            &empty_worker_pid(),
            false,
        )
        .unwrap();
        assert_eq!(request.id, "test-7");
        assert!(ok);
        assert_eq!(result["status"], "ready");
    }

    #[test]
    fn rejects_unsupported_protocol_version() {
        let catalog = empty_catalog();
        let request: Request =
            serde_json::from_str(r#"{"version":99,"id":1,"method":"health"}"#).unwrap();
        let (ok, result) = process_request(
            &request,
            &catalog,
            &[],
            None,
            None,
            &cache_for_tests(),
            None,
            None,
            None,
            PeerCred::default(),
            &empty_worker_pid(),
            false,
        )
        .unwrap();
        assert!(!ok);
        assert_eq!(result["error"], "unsupported_api_version");
    }

    #[test]
    fn rejects_test_faults_unless_explicitly_enabled() {
        let catalog = empty_catalog();
        let request: Request = serde_json::from_str(
            r#"{"version":1,"id":3,"method":"renderer.start","params":{"wallpaper_id":"synthetic","content_hash":"abc","test_fault":{"kind":"hang","after":2}}}"#,
        )
        .unwrap();
        let (ok, result) = process_request(
            &request,
            &catalog,
            &[],
            None,
            None,
            &cache_for_tests(),
            None,
            None,
            None,
            PeerCred::default(),
            &empty_worker_pid(),
            false,
        )
        .unwrap();
        assert!(!ok);
        assert_eq!(result["error"], "test_faults_disabled");
    }

    #[test]
    fn renderer_start_rejects_the_removed_network_hook_param() {
        // M2c removed the per-request allow_network test hook: the grant
        // store is the only network path, and the unknown field must fail
        // closed at the params boundary.
        let catalog = empty_catalog();
        let request: Request = serde_json::from_str(
            r#"{"version":1,"method":"renderer.start","params":{"wallpaper_id":"web-1","content_hash":"abc","kind":"web","content":"/tmp","allow_network":true}}"#,
        )
        .unwrap();
        let (ok, result) = process_request(
            &request,
            &catalog,
            &[],
            None,
            None,
            &cache_for_tests(),
            None,
            None,
            None,
            PeerCred::default(),
            &empty_worker_pid(),
            false,
        )
        .unwrap();
        assert!(!ok);
        assert_eq!(result["error"], "invalid_params");
    }

    #[test]
    fn permissions_get_returns_the_documented_defaults_without_a_record() {
        let service = supervisor_service();
        let (ok, result) = process_with_supervisor(
            r#"{"version":1,"method":"permissions.get","params":{"wallpaper_id":"431960-123"}}"#,
            &service.handle(),
        );
        assert!(ok, "{result}");
        assert_eq!(result["granted"]["network"], false);
        assert_eq!(result["granted"]["audio"], false);
        assert_eq!(result["granted"]["pointer"], true);
    }

    #[test]
    fn permissions_set_patches_and_round_trips_through_get_and_list() {
        let service = supervisor_service();
        let handle = service.handle();
        let (ok, result) = process_with_supervisor(
            r#"{"version":1,"method":"permissions.set","params":{"wallpaper_id":"431960-123","network":true}}"#,
            &handle,
        );
        assert!(ok, "{result}");
        assert_eq!(result["granted"]["network"], true);
        assert_eq!(result["granted"]["audio"], false);
        assert_eq!(result["granted"]["pointer"], true);
        // A partial set keeps the stored network grant.
        let (ok, result) = process_with_supervisor(
            r#"{"version":1,"method":"permissions.set","params":{"wallpaper_id":"431960-123","audio":true}}"#,
            &handle,
        );
        assert!(ok, "{result}");
        assert_eq!(result["granted"]["network"], true);
        assert_eq!(result["granted"]["audio"], true);
        // get returns the patched effective record.
        let (ok, result) = process_with_supervisor(
            r#"{"version":1,"method":"permissions.get","params":{"wallpaper_id":"431960-123"}}"#,
            &handle,
        );
        assert!(ok, "{result}");
        assert_eq!(result["granted"]["network"], true);
        assert_eq!(result["granted"]["audio"], true);
        // list returns every stored record, bounded by the store.
        let (ok, result) =
            process_with_supervisor(r#"{"version":1,"method":"permissions.list"}"#, &handle);
        assert!(ok, "{result}");
        let grants = result["grants"].as_object().unwrap();
        assert_eq!(grants.len(), 1);
        assert_eq!(grants["431960-123"]["network"], true);
        assert_eq!(grants["431960-123"]["audio"], true);
    }

    #[test]
    fn permissions_reject_unknown_fields_and_invalid_wallpaper_ids() {
        let service = supervisor_service();
        let handle = service.handle();
        for bad in [
            r#"{"version":1,"method":"permissions.set","params":{"wallpaper_id":"431960-123","bogus":true}}"#,
            r#"{"version":1,"method":"permissions.set","params":{"wallpaper_id":"../escape","network":true}}"#,
            r#"{"version":1,"method":"permissions.set","params":{"network":true}}"#,
            r#"{"version":1,"method":"permissions.get","params":{"wallpaper_id":""}}"#,
            r#"{"version":1,"method":"permissions.get","params":{"wallpaper_id":"../escape"}}"#,
            r#"{"version":1,"method":"permissions.get","params":{"bogus":1}}"#,
        ] {
            let (ok, result) = process_with_supervisor(bad, &handle);
            assert!(!ok, "{bad}");
            assert_eq!(result["error"], "invalid_params", "{bad}");
        }
        // None of the rejected sets stored anything.
        let (ok, result) =
            process_with_supervisor(r#"{"version":1,"method":"permissions.list"}"#, &handle);
        assert!(ok, "{result}");
        assert!(result["grants"].as_object().unwrap().is_empty());
    }

    #[test]
    fn scene_inspect_rejects_relative_and_empty_paths() {
        // Mirrors permissions_reject_unknown_fields_and_invalid_wallpaper_ids:
        // a malformed path is rejected before it ever reaches `inspect`, so
        // `inspect: None` here still proves the daemon fails closed on the
        // input, not merely on an absent inspector.
        for bad in [
            r#"{"version":1,"method":"scene.inspect","params":{"path":""}}"#,
            r#"{"version":1,"method":"scene.inspect","params":{"path":"relative/scene.json"}}"#,
            r#"{"version":1,"method":"scene.inspect","params":{"bogus":1}}"#,
        ] {
            let (ok, result) = process_with_inspect(bad, None);
            assert!(!ok, "{bad}");
            assert_eq!(result["error"], "invalid_params", "{bad}");
        }
    }

    /// R1 (SR-0b review): `dispatch_scene_inspect`'s single in-flight gate
    /// under real concurrency. Two threads race a barrier into
    /// `dispatch_scene_inspect` against a hang fake; exactly one gets the
    /// gate and runs the (slow) real inspection to `timeout`, the other
    /// sees it already held and answers `inspector-busy` immediately.
    /// Once the first finishes, the gate is clear: a third call proceeds to
    /// another real (slow) inspection instead of `inspector-busy` again.
    #[test]
    fn concurrent_scene_inspect_calls_serialize_through_the_single_inspection_gate() {
        let root = temp_dir("inspect-busy");
        let hang = root.join("hang-inspector.py");
        fs::write(
            &hang,
            "#!/usr/bin/env python3\nimport time\ntime.sleep(60)\n",
        )
        .unwrap();
        fs::set_permissions(&hang, fs::Permissions::from_mode(0o755)).unwrap();
        let config = Arc::new(InspectConfig {
            inspector_path: Some(hang),
            runtime_dir: root.join("runtime"),
            wall_timeout: Duration::from_millis(500),
            resource_limits: sample_limits(1024),
        });

        fn request(id: i64) -> Request {
            Request {
                version: 1,
                id: json!(id),
                method: "scene.inspect".to_string(),
                params: json!({"path": "/nonexistent/scene.json"}),
            }
        }

        fn read_response(stream: &mut UnixStream) -> Value {
            use std::io::BufRead;
            stream
                .set_read_timeout(Some(Duration::from_secs(5)))
                .unwrap();
            let mut reader = BufReader::new(stream);
            let mut line = Vec::new();
            reader.read_until(b'\n', &mut line).unwrap();
            serde_json::from_slice(&line).unwrap()
        }

        let (a_write, mut a_read) = UnixStream::pair().unwrap();
        let (b_write, mut b_read) = UnixStream::pair().unwrap();
        // Race both calls into the gate at (as close to) the same instant,
        // so the test exercises the atomic swap under real contention
        // instead of one call always winning by construction order.
        let barrier = Arc::new(std::sync::Barrier::new(2));

        let thread_a = {
            let config = config.clone();
            let barrier = barrier.clone();
            std::thread::spawn(move || {
                barrier.wait();
                dispatch_scene_inspect(a_write, request(1), &config).unwrap();
            })
        };
        let thread_b = {
            let config = config.clone();
            let barrier = barrier.clone();
            std::thread::spawn(move || {
                barrier.wait();
                dispatch_scene_inspect(b_write, request(2), &config).unwrap();
            })
        };
        thread_a.join().unwrap();
        thread_b.join().unwrap();

        let response_a = read_response(&mut a_read);
        let response_b = read_response(&mut b_read);
        let mut reasons = [
            response_a["result"]["reason"].as_str().unwrap().to_string(),
            response_b["result"]["reason"].as_str().unwrap().to_string(),
        ];
        reasons.sort();
        assert_eq!(
            reasons,
            ["inspector-busy", "timeout"],
            "a={response_a} b={response_b}"
        );

        // The gate is clear again by the time either response is
        // observable: dispatch_scene_inspect's spawned thread drops the
        // InspectInFlightGuard right after run_inspection finishes, before
        // it writes the response. So having read both responses above is
        // proof enough that a third call now proceeds to a real
        // inspection instead of `inspector-busy`.
        let (c_write, mut c_read) = UnixStream::pair().unwrap();
        dispatch_scene_inspect(c_write, request(3), &config).unwrap();
        let response_c = read_response(&mut c_read);
        assert_eq!(response_c["result"]["reason"], "timeout", "{response_c}");

        fs::remove_dir_all(&root).unwrap();
    }

    #[test]
    fn permissions_are_bounded_to_256_records() {
        let service = supervisor_service();
        let handle = service.handle();
        for index in 0..256 {
            let (ok, result) = process_with_supervisor(
                &format!(
                    r#"{{"version":1,"method":"permissions.set","params":{{"wallpaper_id":"wallpaper-{index:03}"}}}}"#
                ),
                &handle,
            );
            assert!(ok, "set {index}: {result}");
        }
        // The 257th record is rejected without touching the store.
        let (ok, result) = process_with_supervisor(
            r#"{"version":1,"method":"permissions.set","params":{"wallpaper_id":"wallpaper-256"}}"#,
            &handle,
        );
        assert!(!ok);
        assert_eq!(result["error"], "permissions_failed");
        assert!(result["detail"].as_str().unwrap().contains("safety limit"));
        let (ok, result) =
            process_with_supervisor(r#"{"version":1,"method":"permissions.list"}"#, &handle);
        assert!(ok, "{result}");
        assert_eq!(result["grants"].as_object().unwrap().len(), 256);
    }

    fn process(
        request_json: &str,
        catalog: &Arc<RwLock<Catalog>>,
        playlist: Option<&PlaylistSessionHandle>,
    ) -> (bool, Value) {
        let request: Request = serde_json::from_str(request_json).unwrap();
        process_request(
            &request,
            catalog,
            &[],
            None,
            playlist,
            &cache_for_tests(),
            None,
            None,
            None,
            PeerCred::default(),
            &empty_worker_pid(),
            true,
        )
        .unwrap()
    }

    fn process_with_supervisor(request_json: &str, handle: &SupervisorHandle) -> (bool, Value) {
        let catalog = empty_catalog();
        let request: Request = serde_json::from_str(request_json).unwrap();
        process_request(
            &request,
            &catalog,
            &[],
            Some(handle),
            None,
            &cache_for_tests(),
            None,
            None,
            None,
            PeerCred::default(),
            &empty_worker_pid(),
            true,
        )
        .unwrap()
    }

    /// A `scene.inspect` RPC test helper (SR-0b): no supervisor/playlist/
    /// apply handle is needed for the bad-input path validation this
    /// exercises, so every other service stays `None` as in `process()`.
    fn process_with_inspect(request_json: &str, inspect: Option<&InspectConfig>) -> (bool, Value) {
        let catalog = empty_catalog();
        let request: Request = serde_json::from_str(request_json).unwrap();
        process_request(
            &request,
            &catalog,
            &[],
            None,
            None,
            &cache_for_tests(),
            None,
            None,
            inspect,
            PeerCred::default(),
            &empty_worker_pid(),
            true,
        )
        .unwrap()
    }

    fn temp_dir(label: &str) -> PathBuf {
        let dir = std::env::temp_dir().join(format!(
            "kwe-daemon-{label}-{}-{}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap_or_default()
                .as_nanos()
        ));
        std::fs::create_dir_all(&dir).unwrap();
        dir
    }

    /// An apply handle for the wallpaper.* RPC tests: a stub probe over a
    /// temporary assignment store (tests that need a real catalog or
    /// supervisor construct their own).
    fn apply_handle(
        probe: Arc<dyn apply::ShellProbe>,
        catalog: &Arc<RwLock<Catalog>>,
        supervisor: SupervisorHandle,
    ) -> apply::ApplyHandle {
        let dir = temp_dir("apply");
        apply_handle_with_store(
            probe,
            catalog,
            supervisor,
            apply::AssignmentStore::open(&dir).unwrap(),
        )
    }

    fn apply_handle_with_store(
        probe: Arc<dyn apply::ShellProbe>,
        catalog: &Arc<RwLock<Catalog>>,
        supervisor: SupervisorHandle,
        store: apply::AssignmentStore,
    ) -> apply::ApplyHandle {
        apply::ApplyHandle::for_test(
            store,
            probe,
            catalog.clone(),
            supervisor,
            Duration::from_millis(1500),
        )
    }

    fn process_with_apply(
        request_json: &str,
        handle: &apply::ApplyHandle,
        catalog: &Arc<RwLock<Catalog>>,
    ) -> (bool, Value) {
        let request: Request = serde_json::from_str(request_json).unwrap();
        process_request(
            &request,
            catalog,
            &[],
            None,
            None,
            &cache_for_tests(),
            None,
            Some(handle),
            None,
            PeerCred::default(),
            &empty_worker_pid(),
            true,
        )
        .unwrap()
    }

    fn empty_worker_pid() -> Arc<AtomicU32> {
        Arc::new(AtomicU32::new(0))
    }

    const DAILY_JSON: &str = r#"{"id":"daily","title":"Daily","entries":[],"shuffle":false,"repeat":true,"duration_seconds":300,"transition":"none","transition_seconds":0}"#;

    #[test]
    fn playlist_put_list_and_status_round_trip() {
        let catalog = empty_catalog();
        let service = session_service();
        let handle = service.handle();
        let (ok, result) = process(
            &format!(
                r#"{{"version":1,"method":"playlist.put","params":{{"playlist":{DAILY_JSON}}}}}"#
            ),
            &catalog,
            Some(&handle),
        );
        assert!(ok, "{result}");
        assert_eq!(result["id"], "daily");
        let (ok, result) = process(
            r#"{"version":1,"method":"playlist.list"}"#,
            &catalog,
            Some(&handle),
        );
        assert!(ok);
        assert_eq!(result["playlists"].as_array().unwrap().len(), 1);
        let (ok, result) = process(
            r#"{"version":1,"method":"playlist.status"}"#,
            &catalog,
            Some(&handle),
        );
        assert!(ok);
        assert_eq!(result["definitions"]["count"], 1);
        assert_eq!(result["definitions"]["store_health"], "ok");
        assert!(!result["active"].as_bool().unwrap());
    }

    #[test]
    fn playlist_put_rejects_unknown_params_and_invalid_playlists() {
        let catalog = empty_catalog();
        let service = session_service();
        let handle = service.handle();
        // Unknown params field.
        let (ok, result) = process(
            r#"{"version":1,"method":"playlist.put","params":{"playlist":{"id":"x","title":"X","entries":[]},"bogus":1}}"#,
            &catalog,
            Some(&handle),
        );
        assert!(!ok);
        assert_eq!(result["error"], "invalid_params");
        // Unknown playlist field (deny_unknown_fields on Playlist).
        let (ok, result) = process(
            r#"{"version":1,"method":"playlist.put","params":{"playlist":{"id":"x","title":"X","entries":[],"shuffle":false,"repeat":true,"duration_seconds":300,"transition":"none","transition_seconds":0,"entrirs":[]}}}"#,
            &catalog,
            Some(&handle),
        );
        assert!(!ok);
        assert_eq!(result["error"], "invalid_params");
        // Semantically invalid timing.
        let (ok, result) = process(
            r#"{"version":1,"method":"playlist.put","params":{"playlist":{"id":"x","title":"X","entries":[],"shuffle":false,"repeat":true,"duration_seconds":5,"transition":"none","transition_seconds":0}}}"#,
            &catalog,
            Some(&handle),
        );
        assert!(!ok);
        assert_eq!(result["error"], "invalid_playlist");
    }

    #[test]
    fn playlist_activate_rejects_unknown_playlist() {
        let catalog = empty_catalog();
        let service = session_service();
        let handle = service.handle();
        let (ok, result) = process(
            r#"{"version":1,"method":"playlist.activate","params":{"id":"nope"}}"#,
            &catalog,
            Some(&handle),
        );
        assert!(!ok);
        assert_eq!(result["error"], "playlist_not_found");
    }

    #[test]
    fn playlist_import_blocked_when_store_non_empty() {
        let catalog = empty_catalog();
        let service = session_service();
        let handle = service.handle();
        process(
            &format!(
                r#"{{"version":1,"method":"playlist.put","params":{{"playlist":{DAILY_JSON}}}}}"#
            ),
            &catalog,
            Some(&handle),
        );
        let (ok, result) = process(
            r#"{"version":1,"method":"playlist.import","params":{"playlists":[{"title":"Legacy","entries":[]}]}}"#,
            &catalog,
            Some(&handle),
        );
        assert!(!ok);
        assert_eq!(result["error"], "playlist_import_blocked");
    }

    #[test]
    fn playlist_debug_clock_skip_rejected_without_test_faults() {
        let catalog = empty_catalog();
        let service = session_service();
        let handle = service.handle();
        let request: Request = serde_json::from_str(
            r#"{"version":1,"method":"playlist.debug-clock-skip","params":{"ms":1000}}"#,
        )
        .unwrap();
        let (ok, result) = process_request(
            &request,
            &catalog,
            &[],
            None,
            Some(&handle),
            &cache_for_tests(),
            None,
            None,
            None,
            PeerCred::default(),
            &empty_worker_pid(),
            false,
        )
        .unwrap();
        assert!(!ok);
        assert_eq!(result["error"], "test_faults_disabled");
    }

    #[test]
    fn playlist_methods_fail_closed_without_session() {
        let catalog = empty_catalog();
        let (ok, result) = process(r#"{"version":1,"method":"playlist.list"}"#, &catalog, None);
        assert!(!ok);
        assert_eq!(result["error"], "playlist_unavailable");
    }

    fn generous_deadline() -> Instant {
        Instant::now() + Duration::from_secs(5)
    }

    #[test]
    fn oversized_non_import_request_is_rejected_early() {
        let mut payload = Vec::with_capacity(MAX_REQUEST_BYTES + 2048);
        payload
            .extend_from_slice(b"{\"version\":1,\"id\":\"big\",\"method\":\"health\",\"pad\":\"");
        payload.resize(MAX_REQUEST_BYTES + 1024, b'a');
        payload.extend_from_slice(b"\"}\n");
        let error = read_request_line(&mut &payload[..], generous_deadline()).unwrap_err();
        assert!(
            format!("{error}").contains(&format!("{MAX_REQUEST_BYTES}")),
            "unexpected error: {error}"
        );
    }

    #[test]
    fn import_marker_allows_requests_beyond_the_normal_cap() {
        let mut payload = Vec::with_capacity(MAX_REQUEST_BYTES + 2048);
        payload.extend_from_slice(
            b"{\"version\":1,\"id\":\"i\",\"method\":\"playlist.import\",\"pad\":\"",
        );
        payload.resize(MAX_REQUEST_BYTES + 1024, b'a');
        payload.extend_from_slice(b"\"}\n");
        let line = read_request_line(&mut &payload[..], generous_deadline()).unwrap();
        assert_eq!(line.len(), payload.len());
    }

    #[test]
    fn import_marker_spanning_a_chunk_boundary_is_detected() {
        // Reads happen in 8192-byte chunks; place the marker so it starts in
        // the first chunk and ends in the second. Without the overlap scan
        // the marker would be missed and the request rejected.
        let mut payload = vec![b'x'; 8190];
        payload.extend_from_slice(IMPORT_MARKER);
        payload.resize(MAX_REQUEST_BYTES + 1024, b'y');
        payload.push(b'\n');
        let line = read_request_line(&mut &payload[..], generous_deadline()).unwrap();
        assert_eq!(line.len(), payload.len());
    }

    #[test]
    fn import_requests_are_capped_at_the_import_limit() {
        let mut payload = Vec::with_capacity(MAX_IMPORT_REQUEST_BYTES + 2048);
        payload.extend_from_slice(b"{\"method\":\"playlist.import\",\"pad\":\"");
        payload.resize(MAX_IMPORT_REQUEST_BYTES + 1024, b'a');
        payload.extend_from_slice(b"\"}\n");
        let error = read_request_line(&mut &payload[..], generous_deadline()).unwrap_err();
        assert!(
            format!("{error}").contains(&format!("{MAX_IMPORT_REQUEST_BYTES}")),
            "unexpected error: {error}"
        );
    }

    #[test]
    fn request_without_trailing_newline_is_rejected() {
        let payload = br#"{"version":1,"id":"x","method":"health"}"#.to_vec();
        let error = read_request_line(&mut &payload[..], generous_deadline()).unwrap_err();
        assert!(
            format!("{error}").contains("without a newline"),
            "unexpected error: {error}"
        );
    }

    /// S1 review #5: `renderer.start`'s `StartSpec::try_from` used to call
    /// `into_validated(None)` unconditionally, so a model-layer scene that
    /// resolves and draws fine at runtime (through `wallpaper.apply`,
    /// which threads the daemon's configured assets root) could still be
    /// needlessly refused at preflight through this lower-level RPC.
    /// Exercises the exact two-step the RPC arm now runs: `try_from` for
    /// field mapping, then `into_validated` with the assets dir.
    #[test]
    fn renderer_start_scene_preflight_honors_the_configured_assets_dir() {
        let root = temp_dir("renderer-start-scene");
        let assets = temp_dir("renderer-start-assets");
        std::fs::create_dir_all(root.join("models")).unwrap();
        std::fs::create_dir_all(root.join("materials")).unwrap();
        std::fs::create_dir_all(assets.join("materials")).unwrap();
        std::fs::write(
            root.join("models/a.json"),
            br#"{"material": "materials/a.json"}"#,
        )
        .unwrap();
        std::fs::write(
            root.join("materials/a.json"),
            br#"{"passes": [{"textures": ["a"]}]}"#,
        )
        .unwrap();
        // Bytes need not be a real TEXV container: this test is about
        // whether the assets root is consulted at all, not about texture
        // decodability (kwe-core's own tests cover that).
        std::fs::write(assets.join("materials/a.tex"), b"placeholder").unwrap();
        let scene = root.join("scene.json");
        std::fs::write(
            &scene,
            br#"{"objects": [{"name": "a", "image": "models/a.json"}]}"#,
        )
        .unwrap();

        let params: RendererStartParams = serde_json::from_str(&format!(
            r#"{{"wallpaper_id":"x","content_hash":"y","kind":"scene","content":{:?}}}"#,
            scene.to_string_lossy()
        ))
        .unwrap();
        let spec = StartSpec::try_from(params).unwrap();
        assert!(
            spec.clone().into_validated(None).is_err(),
            "no assets root configured: the model stays unresolved"
        );
        assert!(
            spec.into_validated(Some(&assets)).is_ok(),
            "assets root configured: the model resolves and preflight accepts"
        );
        let _ = std::fs::remove_dir_all(&root);
        let _ = std::fs::remove_dir_all(&assets);
    }

    #[test]
    fn renderer_start_rejects_unknown_kind_and_kind_content_mismatches() {
        let catalog = empty_catalog();
        // Unknown kind string.
        let (ok, result) = process(
            r#"{"version":1,"method":"renderer.start","params":{"wallpaper_id":"x","content_hash":"y","kind":"bogus"}}"#,
            &catalog,
            None,
        );
        assert!(!ok);
        assert_eq!(result["error"], "invalid_params");
        // Video without a content path.
        let (ok, result) = process(
            r#"{"version":1,"method":"renderer.start","params":{"wallpaper_id":"x","content_hash":"y","kind":"video"}}"#,
            &catalog,
            None,
        );
        assert!(!ok);
        assert_eq!(result["error"], "invalid_params");
        // Test kind with a content path.
        let (ok, result) = process(
            r#"{"version":1,"method":"renderer.start","params":{"wallpaper_id":"x","content_hash":"y","kind":"test","content":"/tmp/kwe-any.mp4"}}"#,
            &catalog,
            None,
        );
        assert!(!ok);
        assert_eq!(result["error"], "invalid_params");
        // Video content path that does not exist fails the static preflight.
        let (ok, result) = process(
            r#"{"version":1,"method":"renderer.start","params":{"wallpaper_id":"x","content_hash":"y","kind":"video","content":"/nonexistent/kwe-m1a-video.mp4"}}"#,
            &catalog,
            None,
        );
        assert!(!ok);
        assert_eq!(result["error"], "invalid_params");
        // Web content without a preflightable index.html is rejected.
        let (ok, result) = process(
            r#"{"version":1,"method":"renderer.start","params":{"wallpaper_id":"x","content_hash":"y","kind":"web","content":"/nonexistent/kwe-m1a-web"}}"#,
            &catalog,
            None,
        );
        assert!(!ok);
        assert_eq!(result["error"], "invalid_params");
    }

    #[test]
    fn audio_and_media_rpc_validate_via_the_protocol_types() {
        let catalog = empty_catalog();
        // Bad band count fails inside AudioFrame::new.
        let (ok, result) = process(
            r#"{"version":1,"method":"audio.forward","params":{"generation":1,"frame":{"left":[0.5,0.5,0.5,0.5,0.5,0.5,0.5,0.5],"right":[0.5,0.5,0.5,0.5,0.5,0.5,0.5,0.5]}}}"#,
            &catalog,
            None,
        );
        assert!(!ok);
        assert_eq!(result["error"], "invalid_params");
        // Well-formed frame parses and reaches the supervisor boundary.
        let bands = serde_json::json!({"left": vec![0.5_f32; 64], "right": vec![0.5_f32; 64]});
        let request_json = format!(
            r#"{{"version":1,"method":"audio.forward","params":{{"generation":1,"frame":{bands}}}}}"#
        );
        let (ok, result) = process(&request_json, &catalog, None);
        assert!(!ok);
        assert_eq!(result["error"], "supervisor_unavailable");
        // Out-of-range timeline fails inside MediaState::new.
        let (ok, result) = process(
            r#"{"version":1,"method":"media.state","params":{"generation":1,"playback":"playing","position_seconds":-1}}"#,
            &catalog,
            None,
        );
        assert!(!ok);
        assert_eq!(result["error"], "invalid_params");
        // Valid media state parses and reaches the supervisor boundary.
        let (ok, result) = process(
            r#"{"version":1,"method":"media.state","params":{"generation":1,"playback":"paused","title":"Track"}}"#,
            &catalog,
            None,
        );
        assert!(!ok);
        assert_eq!(result["error"], "supervisor_unavailable");
    }

    #[test]
    fn media_state_encoding_round_trips_through_the_protocol_type() {
        let state = MediaState::new(
            7,
            "playing",
            Some("Track".into()),
            Some("Artist".into()),
            None,
            Some(12.5),
            Some(240.0),
        )
        .unwrap();
        assert_eq!(
            kwe_input_protocol::decode_media_state(
                &kwe_input_protocol::encode_media_state(&state).unwrap()
            )
            .unwrap(),
            state
        );
    }

    #[test]
    fn only_the_daemons_own_worker_no_renderer_rejection_is_dropped_silently() {
        let worker_pid = Arc::new(AtomicU32::new(4321));
        let no_renderer = json!({
            "error": "supervisor_failed",
            "detail": "no promoted renderer is available for audio forwarding"
        });
        // The daemon's own worker (matching pid AND uid) with the
        // no-promoted-renderer rejection: silent latest-wins drop.
        let own_worker = PeerCred {
            pid: 4321,
            // SAFETY: geteuid takes no arguments and cannot fault.
            uid: unsafe { libc::geteuid() },
        };
        let dropped = classify_audio_error(own_worker, &worker_pid, &no_renderer);
        assert_eq!(dropped, Some(json!({"status": "dropped"})));
        // Stale-generation rejections still surface unchanged: the worker
        // refreshes its display generation on them.
        let stale = json!({
            "error": "supervisor_failed",
            "detail": "audio frame display generation is stale or invalid"
        });
        assert_eq!(classify_audio_error(own_worker, &worker_pid, &stale), None);
        // Same pid but a different user is not our worker: pids can be
        // recycled after the worker exits (pid-reuse hardening).
        let recycled_pid_other_user = PeerCred {
            pid: 4321,
            // SAFETY: geteuid takes no arguments and cannot fault.
            uid: unsafe { libc::geteuid() }.wrapping_add(1),
        };
        assert_eq!(
            classify_audio_error(recycled_pid_other_user, &worker_pid, &no_renderer),
            None
        );
        // Other callers, unknown credentials, and a worker that never
        // spawned keep the original error.
        let other_pid = PeerCred {
            pid: 9999,
            // SAFETY: geteuid takes no arguments and cannot fault.
            uid: unsafe { libc::geteuid() },
        };
        assert_eq!(
            classify_audio_error(other_pid, &worker_pid, &no_renderer),
            None
        );
        assert_eq!(
            classify_audio_error(PeerCred::default(), &worker_pid, &no_renderer),
            None
        );
        assert_eq!(
            classify_audio_error(own_worker, &Arc::new(AtomicU32::new(0)), &no_renderer),
            None
        );
        // Non-supervisor failures are never dropped.
        let invalid = json!({"error": "invalid_params", "detail": "x"});
        assert_eq!(
            classify_audio_error(own_worker, &worker_pid, &invalid),
            None
        );
    }

    #[test]
    fn audio_status_reports_the_capture_service_state_and_fails_closed() {
        let catalog = empty_catalog();
        let service = AudioCaptureService::start(AudioCaptureConfig {
            enabled: false,
            worker_path: PathBuf::from("/bin/sleep"),
            socket: PathBuf::from("/nonexistent/kwe.sock"),
            capture_node: None,
        })
        .unwrap();
        let handle = service.handle();
        let request: Request =
            serde_json::from_str(r#"{"version":1,"method":"audio.status"}"#).unwrap();
        let (ok, result) = process_request(
            &request,
            &catalog,
            &[],
            None,
            None,
            &cache_for_tests(),
            Some(&handle),
            None,
            None,
            PeerCred::default(),
            &empty_worker_pid(),
            false,
        )
        .unwrap();
        assert!(ok, "{result}");
        assert_eq!(result["enabled"], false);
        assert!(result["pid"].is_null());
        assert_eq!(result["restarts"], 0);
        assert!(result["disabled_reason"].is_null());
        // Without a running service the method fails closed.
        let (ok, result) = process_request(
            &request,
            &catalog,
            &[],
            None,
            None,
            &cache_for_tests(),
            None,
            None,
            None,
            PeerCred::default(),
            &empty_worker_pid(),
            false,
        )
        .unwrap();
        assert!(!ok);
        assert_eq!(result["error"], "audio_unavailable");
    }

    // -------------------------------------------------------------------
    // wallpaper.* (BETA_M4a): the daemon apply transaction
    // -------------------------------------------------------------------

    /// A fake scene renderer for the promotion path: creates the frame
    /// protocol file at --output and publishes frames (odd generation ->
    /// slot toggle -> even generation) every 50 ms for 8 s, giving the
    /// supervisor a canary sequence of >= 3. Runs under the supervisor's
    /// env allowlist (PATH=/usr/bin:/usr/sbin:/bin), hence the env shebang.
    const FAKE_SCENE_RENDERER: &str = r#"#!/usr/bin/env python3
import argparse
import os
import struct
import time

parser = argparse.ArgumentParser()
parser.add_argument("--output", required=True)
parser.add_argument("--width", type=int, default=320)
parser.add_argument("--height", type=int, default=180)
parser.add_argument("--fps", type=int, default=30)
parser.add_argument("--scaling", default="aspect")
parser.add_argument("--content")
args = parser.parse_args()

width = args.width
height = args.height
stride = width * 4
slot_bytes = stride * height
file_bytes = 64 + 2 * slot_bytes

header = bytearray(64)
struct.pack_into("<8s", header, 0, b"KWEFRM1\0")
struct.pack_into("<I", header, 8, 1)      # version
struct.pack_into("<I", header, 12, 64)    # header bytes
struct.pack_into("<Q", header, 16, file_bytes)
struct.pack_into("<I", header, 24, width)
struct.pack_into("<I", header, 28, height)
struct.pack_into("<I", header, 32, stride)
struct.pack_into("<I", header, 36, 1)     # BGRA premultiplied
struct.pack_into("<I", header, 40, 2)     # slot count
struct.pack_into("<Q", header, 48, 0)     # generation (even)
struct.pack_into("<I", header, 56, 0)     # active slot
struct.pack_into("<I", header, 60, 2)     # producer state: Running

with open(args.output, "wb") as frame:
    frame.write(bytes(header) + bytes(slot_bytes * 2))
    frame.flush()
    os.fsync(frame.fileno())
    generation = 0
    active = 0
    deadline = time.monotonic() + 8.0
    while time.monotonic() < deadline:
        generation += 1          # odd
        struct.pack_into("<Q", header, 48, generation)
        active = 1 - active
        struct.pack_into("<I", header, 56, active)
        generation += 1          # even
        struct.pack_into("<Q", header, 48, generation)
        frame.seek(48)
        frame.write(header[48:64])
        frame.flush()
        os.fsync(frame.fileno())
        time.sleep(0.05)
"#;

    /// Fake steam root with one subscribed scene project (workshop id "1").
    fn scene_catalog() -> Arc<RwLock<Catalog>> {
        scene_catalog_with(&["1"])
    }

    /// Fake steam root with subscribed scene projects for `ids` (workshop
    /// ids). Every project carries the runnable scene.json inside its
    /// content root (the resolved catalog content, BETA_M4a review fix 5:
    /// the renderer runs the catalog content, and a client-supplied
    /// content must match it).
    fn scene_catalog_with(ids: &[&str]) -> Arc<RwLock<Catalog>> {
        let root = temp_dir("apply-catalog");
        let mut subscriptions = String::new();
        for id in ids {
            let content_dir = root.join(format!("steamapps/workshop/content/431960/{id}"));
            std::fs::create_dir_all(&content_dir).unwrap();
            std::fs::write(
                content_dir.join("project.json"),
                format!(r#"{{"title":"Synthetic {id}","type":"scene","tags":[]}}"#),
            )
            .unwrap();
            std::fs::write(content_dir.join("scene.json"), br#"{"general":{}}"#).unwrap();
            subscriptions.push_str(&format!(" \"{id}\" \"1\""));
        }
        std::fs::write(
            root.join("steamapps/libraryfolders.vdf"),
            "\"LibraryFolders\" { }\n",
        )
        .unwrap();
        std::fs::write(
            root.join("steamapps/appworkshop_431960.acf"),
            format!("\"AppWorkshop\" {{ \"WorkshopItems\" {{ {subscriptions} }} }}\n"),
        )
        .unwrap();
        Arc::new(RwLock::new(scan_installed(&[root], &ScanLimits::default())))
    }

    /// The resolved catalog content path for the fixture item "1".
    fn fixture_scene_path(catalog: &Catalog) -> PathBuf {
        fixture_scene_path_for(catalog, "1")
    }

    /// The resolved catalog content path for a fixture item.
    fn fixture_scene_path_for(catalog: &Catalog, id: &str) -> PathBuf {
        let item = catalog
            .items
            .iter()
            .find(|item| item.workshop_id == id)
            .unwrap_or_else(|| panic!("fixture item {id}"));
        item.content_root.join("scene.json")
    }

    /// A supervisor fast enough for the promotion wait (150 ms canary) with
    /// the python3 fake renderer wired as the scene kind.
    fn fast_scene_supervisor(root: &Path) -> SupervisorService {
        SupervisorService::start(fast_scene_supervisor_config(root)).unwrap()
    }

    /// The config behind `fast_scene_supervisor`, exposed so a test can
    /// compute the build identity its state file must carry (B4).
    fn fast_scene_supervisor_config(root: &Path) -> SupervisorConfig {
        let script = root.join("fake-scene-renderer.py");
        std::fs::write(&script, FAKE_SCENE_RENDERER).unwrap();
        std::fs::set_permissions(&script, fs::Permissions::from_mode(0o755)).unwrap();
        let dir = root.join("supervisor");
        let limits = sample_limits(1024);
        SupervisorConfig {
            renderer_paths: BTreeMap::from([(RendererKind::Scene, script)]),
            runtime_dir: dir.join("runtime"),
            state_dir: dir.join("state"),
            startup_timeout_ms_by_kind: BTreeMap::from([
                (RendererKind::Test, 3000),
                (RendererKind::Video, 6000),
                (RendererKind::Web, 10_000),
                (RendererKind::Scene, 3000),
            ]),
            frame_timeout: Duration::from_secs(2),
            stop_grace: Duration::from_millis(500),
            restart_delay: Duration::from_millis(250),
            canary_duration: Duration::from_millis(150),
            handoff_timeout: Duration::from_secs(5),
            max_failures: 3,
            web_heartbeat_ms: 5000,
            web_heartbeat_max_failures: 3,
            resource_limits_by_kind: BTreeMap::from([
                (RendererKind::Test, limits),
                (RendererKind::Video, limits),
                (RendererKind::Web, limits),
                (RendererKind::Scene, limits),
            ]),
            scene_assets_dir: None,
        }
    }

    // -------------------------------------------------------------------
    // SR-1c: the scene apply gate (staged preflight inspection before any
    // renderer/wallpaper touch, apply.rs's `apply()`).
    // -------------------------------------------------------------------

    /// Python source for a `write_frame(kind, payload)` helper writing to
    /// fd 3, mirroring `inspect.rs`'s private test-module copy exactly
    /// (docs/REPORT_PROTOCOL_V1.md's wire format: 12-byte header — magic
    /// `KWR1`, kind, flags=0, reserved=0 as a u16 LE, payload_len as a u32
    /// LE). Duplicated here because that module's copy is private to its
    /// own test module.
    const SR1C_WRITE_FRAME_HELPER: &str = r#"
import os
import struct

def write_frame(kind, payload):
    header = b"KWR1" + bytes([kind, 0]) + struct.pack("<H", 0) + struct.pack("<I", len(payload))
    os.write(3, header + payload)
"#;

    fn write_script(root: &Path, name: &str, body: &str) -> PathBuf {
        let path = root.join(name);
        fs::write(&path, body).unwrap();
        fs::set_permissions(&path, fs::Permissions::from_mode(0o755)).unwrap();
        path
    }

    fn sr1c_inspect_config(
        root: &Path,
        inspector_path: Option<PathBuf>,
        wall_timeout: Duration,
    ) -> InspectConfig {
        InspectConfig {
            inspector_path,
            runtime_dir: root.join("inspect-runtime"),
            wall_timeout,
            resource_limits: sample_limits(1024),
        }
    }

    /// A fake `kwe-scene-inspector` reporting `outcome: "inventoried"` with
    /// the given `required` capability ids (a well-formed `scene-inspection-v1`
    /// record, digest-verified, mirroring `inspect.rs`'s own fake-inspector
    /// fixture).
    fn fake_inspector_inventoried(root: &Path, name: &str, required: &[&str]) -> PathBuf {
        let required_json = serde_json::to_string(required).unwrap();
        let script = format!(
            r#"#!/usr/bin/env python3
import argparse
import hashlib
import json
{SR1C_WRITE_FRAME_HELPER}
parser = argparse.ArgumentParser()
parser.add_argument("--input", required=True)
parser.add_argument("--max-wall-ms", type=int, default=10000)
parser.add_argument("--report-fd", type=int, required=True)
args = parser.parse_args()

record = {{
    "schema": "scene-inspection-v1",
    "capabilities_schema": "scene-capabilities-v1",
    "content": {{"hash": "sha256:deadbeef", "source_bytes": 1, "kind": "json-dir"}},
    "inspector": {{"build": "dev", "abi": 0}},
    "outcome": "inventoried",
    "reason": "ok",
    "required": {required_json},
    "detected": [],
    "unknown": {{"keys": 0, "types": 0, "objects": 0, "samples": [], "truncated": False}},
    "bounds": {{"wall_ms": 1, "peak_bytes": 0, "limits_hit": []}},
    "backend": None,
    "digest": "",
}}
serialized = json.dumps(record, sort_keys=True, separators=(",", ":")).encode()
record["digest"] = hashlib.sha256(serialized).hexdigest()
payload = json.dumps(record, sort_keys=True, separators=(",", ":")).encode()
write_frame(1, payload)
"#
        );
        write_script(root, name, &script)
    }

    /// A fake `kwe-scene-inspector` reporting `outcome: "incompatible"`
    /// (the content itself is refused, e.g. parse-error/oversize/
    /// unrecognized-input) with the given `reason`.
    fn fake_inspector_incompatible(root: &Path, name: &str, reason: &str) -> PathBuf {
        let script = format!(
            r#"#!/usr/bin/env python3
import argparse
import hashlib
import json
{SR1C_WRITE_FRAME_HELPER}
parser = argparse.ArgumentParser()
parser.add_argument("--input", required=True)
parser.add_argument("--max-wall-ms", type=int, default=10000)
parser.add_argument("--report-fd", type=int, required=True)
args = parser.parse_args()

record = {{
    "schema": "scene-inspection-v1",
    "capabilities_schema": "scene-capabilities-v1",
    "content": {{"hash": "sha256:deadbeef", "source_bytes": 1, "kind": "json-dir"}},
    "inspector": {{"build": "dev", "abi": 0}},
    "outcome": "incompatible",
    "reason": {reason:?},
    "required": [],
    "detected": [],
    "unknown": {{"keys": 0, "types": 0, "objects": 0, "samples": [], "truncated": False}},
    "bounds": {{"wall_ms": 1, "peak_bytes": 0, "limits_hit": []}},
    "backend": None,
    "digest": "",
}}
serialized = json.dumps(record, sort_keys=True, separators=(",", ":")).encode()
record["digest"] = hashlib.sha256(serialized).hexdigest()
payload = json.dumps(record, sort_keys=True, separators=(",", ":")).encode()
write_frame(1, payload)
"#
        );
        write_script(root, name, &script)
    }

    /// A fake inspector that hangs forever (times out under a short wall
    /// clock, mirroring `inspect.rs`'s own hang fixture).
    fn fake_inspector_hang(root: &Path, name: &str) -> PathBuf {
        write_script(
            root,
            name,
            "#!/usr/bin/env python3\nimport time\ntime.sleep(600)\n",
        )
    }

    /// A fake inspector that would prove it ran, if invoked, by writing a
    /// marker file — then exits nonzero. Used to assert a video-kind apply
    /// never runs an inspection at all (decision: non-scene kinds are
    /// untouched).
    fn fake_inspector_marker(root: &Path, name: &str, marker: &Path) -> PathBuf {
        let script = format!(
            "#!/usr/bin/env python3\nimport pathlib\npathlib.Path({marker:?}).write_text('ran')\nraise SystemExit(99)\n",
        );
        write_script(root, name, &script)
    }

    /// Seeds `store` with an assignment for `output_name` so a test can
    /// assert a refused apply leaves it untouched.
    fn seed_assignment(store: &mut apply::AssignmentStore, output_name: &str, wallpaper_id: &str) {
        store
            .set(
                output_name,
                apply::Assignment {
                    wallpaper_id: wallpaper_id.to_string(),
                    kind: RendererKind::Scene,
                    content: "/tmp/prior-scene.json".into(),
                    width: 320,
                    height: 180,
                    fps: 30,
                    scaling: ScalingMode::Aspect,
                    applied_at_unix_seconds: 1,
                    previous: None,
                },
            )
            .unwrap();
    }

    #[test]
    fn scene_apply_gate_refuses_a_missing_required_capability() {
        let root = temp_dir("gate-blocking");
        let catalog = scene_catalog();
        let supervisor_service = fast_scene_supervisor(&root);
        let supervisor = supervisor_service.handle();
        let probe = stub_probe_with_switch(vec![dp1_output()]);
        let inspector = fake_inspector_inventoried(
            &root,
            "fake-inspector.py",
            &["scene.layer.image", "scene.future-thing"],
        );
        let store_dir = temp_dir("gate-blocking-store");
        let mut store = apply::AssignmentStore::open(&store_dir).unwrap();
        seed_assignment(&mut store, "DP-1", "prior");
        let handle = apply_handle_with_store(probe.clone(), &catalog, supervisor.clone(), store)
            .with_inspect_config(sr1c_inspect_config(
                &root,
                Some(inspector),
                Duration::from_secs(5),
            ));
        let scene = fixture_scene_path(&catalog.read().unwrap());
        let (ok, result) = process_with_apply(
            &format!(
                r#"{{"version":1,"method":"wallpaper.apply","params":{{"output":"DP-1","wallpaper_id":"1","kind":"scene","content":"{}"}}}}"#,
                scene.display()
            ),
            &handle,
            &catalog,
        );
        assert!(!ok, "gate must refuse: {result}");
        assert_eq!(result["error"], "apply_incompatible");
        assert_eq!(result["missing"], json!(["scene.future-thing"]));
        // Output enumeration (screenForConnector/desktops(), which also
        // reads a generic `wallpaperPlugin` JS property) always runs at
        // step 3, before the gate; what must NEVER run before the gate
        // decides is the Plasma wallpaper SWITCH script (step 6) — the one
        // that assigns OUR plugin identity to a desktop.
        assert!(
            !probe
                .scripts()
                .iter()
                .any(|script| script.contains("org.kde.kwe.wallpaper")),
            "no switch script may run before the gate decides: {:?}",
            probe.scripts()
        );
        let status = supervisor.status().unwrap();
        assert_eq!(
            status.phase,
            WorkerPhase::Idle,
            "no renderer may be spawned when the gate refuses"
        );
        let (ok, result) = process_with_apply(
            r#"{"version":1,"method":"wallpaper.assignments"}"#,
            &handle,
            &catalog,
        );
        assert!(ok);
        assert_eq!(result["outputs"]["DP-1"]["wallpaper_id"], "prior");
    }

    #[test]
    fn scene_apply_gate_proceeds_with_a_tolerated_limitation() {
        let root = temp_dir("gate-tolerated");
        let catalog = scene_catalog();
        let supervisor_service = fast_scene_supervisor(&root);
        let supervisor = supervisor_service.handle();
        let probe = stub_probe_with_switch(vec![dp1_output()]);
        let inspector = fake_inspector_inventoried(
            &root,
            "fake-inspector.py",
            &["scene.layer.image", "scene.layer.sound"],
        );
        let handle = apply_handle(probe.clone(), &catalog, supervisor.clone()).with_inspect_config(
            sr1c_inspect_config(&root, Some(inspector), Duration::from_secs(5)),
        );
        let scene = fixture_scene_path(&catalog.read().unwrap());
        let (ok, result) = process_with_apply(
            &format!(
                r#"{{"version":1,"method":"wallpaper.apply","params":{{"output":"DP-1","wallpaper_id":"1","kind":"scene","content":"{}"}}}}"#,
                scene.display()
            ),
            &handle,
            &catalog,
        );
        assert!(ok, "apply must proceed: {result}");
        assert_eq!(result["limitations"], json!(["scene.layer.sound"]));
        let status = supervisor.status().unwrap();
        assert_eq!(status.phase, WorkerPhase::Live);
        assert_eq!(
            status.capability_limitations,
            vec!["scene.layer.sound".to_string()]
        );
    }

    #[test]
    fn scene_apply_gate_refuses_content_the_inspector_itself_rejects() {
        let root = temp_dir("gate-incompatible");
        let catalog = scene_catalog();
        let supervisor_service = fast_scene_supervisor(&root);
        let supervisor = supervisor_service.handle();
        let probe = stub_probe_with_switch(vec![dp1_output()]);
        let inspector = fake_inspector_incompatible(&root, "fake-inspector.py", "parse-error");
        let handle = apply_handle(probe.clone(), &catalog, supervisor.clone()).with_inspect_config(
            sr1c_inspect_config(&root, Some(inspector), Duration::from_secs(5)),
        );
        let scene = fixture_scene_path(&catalog.read().unwrap());
        let (ok, result) = process_with_apply(
            &format!(
                r#"{{"version":1,"method":"wallpaper.apply","params":{{"output":"DP-1","wallpaper_id":"1","kind":"scene","content":"{}"}}}}"#,
                scene.display()
            ),
            &handle,
            &catalog,
        );
        assert!(!ok, "gate must refuse: {result}");
        assert_eq!(result["error"], "apply_incompatible");
        assert_eq!(result["missing"], json!([]));
        assert_eq!(result["inspection_reason"], "parse-error");
        let status = supervisor.status().unwrap();
        assert_eq!(status.phase, WorkerPhase::Idle);
    }

    #[test]
    fn scene_apply_gate_proceeds_when_the_inspector_hangs() {
        let root = temp_dir("gate-hang");
        let catalog = scene_catalog();
        let supervisor_service = fast_scene_supervisor(&root);
        let supervisor = supervisor_service.handle();
        let probe = stub_probe_with_switch(vec![dp1_output()]);
        let inspector = fake_inspector_hang(&root, "fake-inspector.py");
        let handle = apply_handle(probe.clone(), &catalog, supervisor.clone()).with_inspect_config(
            sr1c_inspect_config(&root, Some(inspector), Duration::from_millis(300)),
        );
        let scene = fixture_scene_path(&catalog.read().unwrap());
        let started = Instant::now();
        let (ok, result) = process_with_apply(
            &format!(
                r#"{{"version":1,"method":"wallpaper.apply","params":{{"output":"DP-1","wallpaper_id":"1","kind":"scene","content":"{}"}}}}"#,
                scene.display()
            ),
            &handle,
            &catalog,
        );
        // Decision (b): the gate never blocks on an infrastructure failure.
        // The wall clock stays bounded (inspection timeout + the normal
        // apply, no double-wait) — well under the promotion timeout used
        // elsewhere in this module (1500 ms) plus the 300 ms inspection cap.
        assert!(started.elapsed() < Duration::from_secs(5), "no double-wait");
        assert!(ok, "apply must proceed: {result}");
        assert_eq!(result["inspection"], "unavailable");
        assert_eq!(result["inspection_reason"], "timeout");
        let status = supervisor.status().unwrap();
        assert_eq!(status.phase, WorkerPhase::Live);
    }

    #[test]
    fn scene_apply_gate_proceeds_with_an_unconfigured_inspector() {
        let root = temp_dir("gate-unconfigured");
        let catalog = scene_catalog();
        let supervisor_service = fast_scene_supervisor(&root);
        let supervisor = supervisor_service.handle();
        let probe = stub_probe_with_switch(vec![dp1_output()]);
        let handle = apply_handle(probe.clone(), &catalog, supervisor.clone())
            .with_inspect_config(sr1c_inspect_config(&root, None, Duration::from_secs(5)));
        let scene = fixture_scene_path(&catalog.read().unwrap());
        let (ok, result) = process_with_apply(
            &format!(
                r#"{{"version":1,"method":"wallpaper.apply","params":{{"output":"DP-1","wallpaper_id":"1","kind":"scene","content":"{}"}}}}"#,
                scene.display()
            ),
            &handle,
            &catalog,
        );
        assert!(ok, "apply must proceed: {result}");
        assert_eq!(result["inspection"], "unavailable");
        assert_eq!(result["inspection_reason"], "inspector-unavailable");
        let status = supervisor.status().unwrap();
        assert_eq!(status.phase, WorkerPhase::Live);
    }

    /// A video-kind apply runs the video (not scene) renderer kind, whose
    /// supervisor config here has no renderer path at all — proving apply
    /// succeeding is unrelated to this test's point, which is narrower:
    /// the fake inspector configured on the handle must never be invoked
    /// for a non-scene kind (SR-1c decision: non-scene kinds are
    /// untouched, zero inspection run). Asserted via a marker file the
    /// fake would have written if the gate had (wrongly) run it.
    /// Fake steam root with one subscribed VIDEO project (workshop id
    /// `id`), a real (garbage-content but correctly-extensioned) `.mp4`
    /// entry so `StartSpec::validate`'s video preflight passes.
    fn video_catalog_with_content(root: &Path, id: &str) -> Arc<RwLock<Catalog>> {
        let content_dir = root.join(format!("steamapps/workshop/content/431960/{id}"));
        std::fs::create_dir_all(&content_dir).unwrap();
        std::fs::write(content_dir.join("clip.mp4"), b"not a real video").unwrap();
        std::fs::write(
            content_dir.join("project.json"),
            format!(
                r#"{{"title":"Synthetic video {id}","type":"video","file":"clip.mp4","tags":[]}}"#
            ),
        )
        .unwrap();
        std::fs::write(
            root.join("steamapps/libraryfolders.vdf"),
            "\"LibraryFolders\" { }\n",
        )
        .unwrap();
        std::fs::write(
            root.join("steamapps/appworkshop_431960.acf"),
            format!("\"AppWorkshop\" {{ \"WorkshopItems\" {{ \"{id}\" \"1\" }} }}\n"),
        )
        .unwrap();
        Arc::new(RwLock::new(scan_installed(
            &[root.to_path_buf()],
            &ScanLimits::default(),
        )))
    }

    #[test]
    fn scene_apply_gate_runs_no_inspection_for_a_video_kind_apply() {
        let root = temp_dir("gate-video-skip");
        let catalog = video_catalog_with_content(&root, "1");
        let script = root.join("fake-video-renderer.py");
        fs::write(&script, FAKE_SCENE_RENDERER).unwrap();
        fs::set_permissions(&script, fs::Permissions::from_mode(0o755)).unwrap();
        let mut config = fast_scene_supervisor_config(&root);
        config
            .renderer_paths
            .insert(RendererKind::Video, script.clone());
        let supervisor_service = SupervisorService::start(config).unwrap();
        let supervisor = supervisor_service.handle();
        let probe = stub_probe_with_switch(vec![dp1_output()]);
        let marker = root.join("inspector-ran.marker");
        let inspector = fake_inspector_marker(&root, "fake-inspector.py", &marker);
        let handle = apply_handle(probe.clone(), &catalog, supervisor.clone()).with_inspect_config(
            sr1c_inspect_config(&root, Some(inspector), Duration::from_secs(5)),
        );
        let (ok, result) = process_with_apply(
            r#"{"version":1,"method":"wallpaper.apply","params":{"output":"DP-1","wallpaper_id":"1","kind":"video"}}"#,
            &handle,
            &catalog,
        );
        assert!(ok, "video apply must be unaffected by the gate: {result}");
        assert!(
            !marker.exists(),
            "the scene inspector must never run for a video-kind apply"
        );
    }

    #[test]
    fn scene_apply_gate_retry_re_runs_the_gate_no_negative_caching() {
        let root = temp_dir("gate-retry");
        let catalog = scene_catalog();
        let supervisor_service = fast_scene_supervisor(&root);
        let supervisor = supervisor_service.handle();
        let probe = stub_probe_with_switch(vec![dp1_output()]);
        let blocking_inspector = fake_inspector_inventoried(
            &root,
            "fake-inspector-blocking.py",
            &["scene.layer.image", "scene.future-thing"],
        );
        let handle = apply_handle(probe.clone(), &catalog, supervisor.clone()).with_inspect_config(
            sr1c_inspect_config(&root, Some(blocking_inspector), Duration::from_secs(5)),
        );
        let scene = fixture_scene_path(&catalog.read().unwrap());
        let (ok, result) = process_with_apply(
            &format!(
                r#"{{"version":1,"method":"wallpaper.apply","params":{{"output":"DP-1","wallpaper_id":"1","kind":"scene","content":"{}"}}}}"#,
                scene.display()
            ),
            &handle,
            &catalog,
        );
        assert!(!ok, "first apply must be refused: {result}");
        assert_eq!(result["error"], "apply_incompatible");

        // Swap in a NEW, now-compatible fake and retry: no cache means the
        // gate re-runs against the new inspector and this time proceeds —
        // proving the first refusal was not negatively cached.
        let compatible_inspector = fake_inspector_inventoried(
            &root,
            "fake-inspector-compatible.py",
            &["scene.layer.image"],
        );
        let handle = handle.with_inspect_config(sr1c_inspect_config(
            &root,
            Some(compatible_inspector),
            Duration::from_secs(5),
        ));
        let (ok, result) = process_with_apply(
            &format!(
                r#"{{"version":1,"method":"wallpaper.apply","params":{{"output":"DP-1","wallpaper_id":"1","kind":"scene","content":"{}","retry":true}}}}"#,
                scene.display()
            ),
            &handle,
            &catalog,
        );
        assert!(ok, "retry after a fixed inspector must apply: {result}");
        let status = supervisor.status().unwrap();
        assert_eq!(status.phase, WorkerPhase::Live);
    }

    #[test]
    fn apply_quarantined_names_the_reason_and_retry_clears_it() {
        // B4: a quarantined identity answers `apply_quarantined` with the
        // record's last detail (not a bare phase name), touches no shell
        // script, and `retry: true` clears exactly that record and applies.
        let root = temp_dir("apply-quarantined");
        let catalog = scene_catalog();
        let config = fast_scene_supervisor_config(&root);
        let build_id = supervisor::build_identity(&config.clone().validate().unwrap());
        let (identity, scene) = {
            let guard = catalog.read().unwrap();
            let item = guard
                .items
                .iter()
                .find(|item| item.workshop_id == "1")
                .unwrap();
            let content = apply::catalog_content_path(item, RendererKind::Scene);
            let hash = apply::content_hash_for(item, &content);
            (format!("1:{hash}:scene"), fixture_scene_path(&guard))
        };
        let state_dir = root.join("supervisor").join("state");
        fs::create_dir_all(&state_dir).unwrap();
        fs::write(
            state_dir.join("supervisor-v1.json"),
            serde_json::to_vec(&json!({
                "schema_version": 1,
                "build_id": build_id,
                "records": { identity.clone(): {
                    "wallpaper_id": "1", "content_hash": identity.split(':').nth(1).unwrap(),
                    "failures": 3, "quarantined": true, "last_failure": "process_exit",
                    "last_detail": "exit_code_73 stderr=[Zygote could not fork]",
                    "updated_unix_seconds": 1 }},
                "last_good": null,
            }))
            .unwrap(),
        )
        .unwrap();
        let supervisor_service = SupervisorService::start(config).unwrap();
        let supervisor = supervisor_service.handle();
        let probe = stub_probe_with_switch(vec![dp1_output()]);
        let handle = apply_handle(probe.clone(), &catalog, supervisor.clone());
        let request = |retry: &str| {
            format!(
                r#"{{"version":1,"method":"wallpaper.apply","params":{{"output":"DP-1","wallpaper_id":"1","kind":"scene","content":"{}","width":320,"height":180,"fps":30{retry}}}}}"#,
                scene.display()
            )
        };
        let (ok, result) = process_with_apply(&request(""), &handle, &catalog);
        assert!(!ok, "quarantined apply must fail: {result}");
        assert_eq!(result["error"], "apply_quarantined");
        let detail = result["detail"].as_str().unwrap();
        assert!(detail.contains("disabled after 3 failures"), "{detail}");
        assert!(detail.contains("Zygote could not fork"), "{detail}");
        assert!(
            probe
                .scripts()
                .iter()
                .all(|script| script.contains("screenForConnector")),
            "no switch script may run for a refused start"
        );
        // The transaction rolled back (renderer stopped if ours) and the
        // record is still there: a plain re-apply would answer the same.
        let (ok, result) = process_with_apply(&request(""), &handle, &catalog);
        assert!(!ok);
        assert_eq!(result["error"], "apply_quarantined");
        // Try again: clears the record, applies, promotes.
        let (ok, result) = process_with_apply(&request(r#","retry":true"#), &handle, &catalog);
        assert!(ok, "retry apply failed: {result}");
        assert_eq!(result["applied"]["wallpaper_id"], "1");
        let status = supervisor.status().unwrap();
        assert_eq!(status.phase, WorkerPhase::Live);
        assert!(!status.quarantined);
        assert_eq!(status.failures, 0);
    }

    #[test]
    fn apply_derives_the_canvas_from_the_output_and_persists_the_scaling_mode() {
        // F1: no width/height in the request -> the canvas follows the
        // output geometry (2926x823 -> long edge capped at 2560, aspect
        // kept, even pixels); `scaling` rides into the renderer argv, the
        // status and the persisted assignment.
        let root = temp_dir("apply-scaling");
        let catalog = scene_catalog();
        let supervisor_service = fast_scene_supervisor(&root);
        let supervisor = supervisor_service.handle();
        let probe = stub_probe_with_switch(vec![dp1_output()]);
        let handle = apply_handle(probe.clone(), &catalog, supervisor.clone());
        let scene = fixture_scene_path(&catalog.read().unwrap());
        let (ok, result) = process_with_apply(
            &format!(
                r#"{{"version":1,"method":"wallpaper.apply","params":{{"output":"DP-1","wallpaper_id":"1","kind":"scene","content":"{}","scaling":"fill"}}}}"#,
                scene.display()
            ),
            &handle,
            &catalog,
        );
        assert!(ok, "apply failed: {result}");
        assert_eq!(result["applied"]["scaling"], "fill");
        let (width, height) = apply::frame_size_for(None, None, Some([0, 0, 2926, 823]));
        assert_eq!(result["applied"]["width"], width);
        assert_eq!(result["applied"]["height"], height);
        assert_eq!(width, 2560);
        assert_eq!(height, 720);
        let status = supervisor.status().unwrap();
        assert_eq!(status.phase, WorkerPhase::Live);
        assert_eq!(status.scaling, ScalingMode::Fill);
        // Persisted and reported back.
        let (ok, listing) = process_with_apply(
            r#"{"version":1,"method":"wallpaper.assignments"}"#,
            &handle,
            &catalog,
        );
        assert!(ok);
        assert_eq!(listing["outputs"]["DP-1"]["scaling"], "fill");
        assert_eq!(listing["outputs"]["DP-1"]["width"], 2560);
        // Explicit size still wins, unknown mode is rejected at the boundary.
        let (ok, result) = process_with_apply(
            &format!(
                r#"{{"version":1,"method":"wallpaper.apply","params":{{"output":"DP-1","wallpaper_id":"1","kind":"scene","content":"{}","width":320,"height":180}}}}"#,
                scene.display()
            ),
            &handle,
            &catalog,
        );
        assert!(ok, "explicit-size apply failed: {result}");
        assert_eq!(result["applied"]["width"], 320);
        assert_eq!(result["applied"]["scaling"], "aspect");
        let (ok, result) = process_with_apply(
            r#"{"version":1,"method":"wallpaper.apply","params":{"output":"DP-1","wallpaper_id":"1","kind":"scene","scaling":"tile"}}"#,
            &handle,
            &catalog,
        );
        assert!(!ok);
        assert_eq!(result["error"], "invalid_params");
    }

    /// One stub system output on "DP-1".
    fn dp1_output() -> apply::SystemOutput {
        apply::SystemOutput {
            name: "DP-1".into(),
            enabled: true,
            connected: true,
            geometry: Some([0, 0, 2926, 823]),
        }
    }

    /// Probe reply matching dp1_output: desktop 111 on screen 0 with the
    /// stock image plugin and a saved Image value.
    const DP1_PROBE_REPLY: &str = r#"{"desktops":[{"index":1,"id":111,"screen":0,"wp":"org.kde.image","image":"file:///usr/share/wallpapers/fallback.png"}],"connectors":{"DP-1":0}}"#;

    /// The same output after the kwe switch script ran: the desktop now
    /// reports our plugin (and the stock Image is out of reach).
    const KWE_PROBE_REPLY: &str = r#"{"desktops":[{"index":1,"id":111,"screen":0,"wp":"org.kde.kwe.wallpaper","image":null}],"connectors":{"DP-1":0}}"#;

    fn stub_probe(outputs: Vec<apply::SystemOutput>, reply: Option<&str>) -> Arc<apply::StubProbe> {
        Arc::new(apply::StubProbe::new(outputs, reply.map(str::to_string)))
    }

    /// A stub probe that flips its enumeration reply after the kwe switch
    /// script runs (the post-switch verification then sees our plugin).
    fn stub_probe_with_switch(outputs: Vec<apply::SystemOutput>) -> Arc<apply::StubProbe> {
        Arc::new(
            apply::StubProbe::new(outputs, Some(DP1_PROBE_REPLY.to_string()))
                .after_switch(KWE_PROBE_REPLY.to_string()),
        )
    }

    #[test]
    fn wallpaper_outputs_enumerates_and_caches_for_five_seconds() {
        let catalog = empty_catalog();
        let supervisor = supervisor_service();
        let probe = stub_probe(vec![dp1_output()], Some(DP1_PROBE_REPLY));
        let handle = apply_handle(probe.clone(), &catalog, supervisor.handle());
        let (ok, result) = process_with_apply(
            r#"{"version":1,"method":"wallpaper.outputs"}"#,
            &handle,
            &catalog,
        );
        assert!(ok);
        let outputs = result["outputs"].as_array().unwrap();
        assert_eq!(outputs.len(), 1);
        assert_eq!(outputs[0]["name"], "DP-1");
        assert_eq!(outputs[0]["screen"], 0);
        assert_eq!(outputs[0]["desktop_id"], 111);
        assert_eq!(outputs[0]["desktop_index"], 1);
        assert_eq!(outputs[0]["geometry"], json!([0, 0, 2926, 823]));
        assert_eq!(outputs[0]["enabled"], true);
        assert_eq!(outputs[0]["connected"], true);
        assert_eq!(outputs[0]["wallpaper_plugin"], "org.kde.image");
        assert_eq!(
            outputs[0]["config_group"],
            json!(["Wallpaper", "org.kde.image", "General"])
        );
        assert_eq!(
            outputs[0]["image"],
            "file:///usr/share/wallpapers/fallback.png"
        );
        // The enumeration is cached for OUTPUT_CACHE_TTL: a second call
        // probes the shell again only after the window expires.
        let before = probe.scripts().len();
        let (ok, result) = process_with_apply(
            r#"{"version":1,"method":"wallpaper.outputs"}"#,
            &handle,
            &catalog,
        );
        assert!(ok);
        assert_eq!(
            probe.scripts().len(),
            before,
            "cache must absorb the second call"
        );
        assert_eq!(result["outputs"][0]["name"], "DP-1");
        // The probe script is the exact enumeration template with the
        // validated connector map.
        assert!(
            probe
                .scripts()
                .last()
                .unwrap()
                .contains("screenForConnector(\"DP-1\")")
        );
    }

    #[test]
    fn apply_unknown_wallpaper_id_fails_closed_without_touching_the_shell() {
        let catalog = empty_catalog();
        let supervisor = supervisor_service();
        let probe = stub_probe(vec![dp1_output()], Some(DP1_PROBE_REPLY));
        let handle = apply_handle(probe.clone(), &catalog, supervisor.handle());
        // The scene content must pass preflight before the catalog lookup
        // (the spec validation order is part of the contract).
        let root = temp_dir("apply-unknown-id");
        let scene = root.join("scene.json");
        fs::write(&scene, br#"{"general":{}}"#).unwrap();
        let (ok, result) = process_with_apply(
            &format!(
                r#"{{"version":1,"method":"wallpaper.apply","params":{{"output":"DP-1","wallpaper_id":"1","kind":"scene","content":"{}"}}}}"#,
                scene.display()
            ),
            &handle,
            &catalog,
        );
        assert!(!ok);
        assert_eq!(result["error"], "apply_unknown_wallpaper");
        assert!(result["detail"].as_str().unwrap().contains("1"));
        // The shell was never probed and no renderer was started.
        assert!(probe.scripts().is_empty());
        let (ok, result) = process_with_apply(
            r#"{"version":1,"method":"wallpaper.assignments"}"#,
            &handle,
            &catalog,
        );
        assert!(ok);
        assert_eq!(result["outputs"].as_object().unwrap().len(), 0);
    }

    #[test]
    fn apply_unknown_output_reports_output_missing() {
        let catalog = scene_catalog();
        let supervisor = supervisor_service();
        let probe = stub_probe(vec![], Some(DP1_PROBE_REPLY));
        let handle = apply_handle(probe, &catalog, supervisor.handle());
        // No content is supplied: the catalog content is used (and the
        // content verification passes before the output enumeration).
        let (ok, result) = process_with_apply(
            r#"{"version":1,"method":"wallpaper.apply","params":{"output":"DP-1","wallpaper_id":"1","kind":"scene"}}"#,
            &handle,
            &catalog,
        );
        assert!(!ok);
        assert_eq!(result["error"], "output_missing");
    }

    #[test]
    fn apply_incompatible_kind_reports_apply_incompatible() {
        let catalog = scene_catalog();
        let supervisor = supervisor_service();
        let probe = stub_probe(vec![dp1_output()], Some(DP1_PROBE_REPLY));
        let handle = apply_handle(probe, &catalog, supervisor.handle());
        // The catalog item "1" is a scene; asking for a video is
        // incompatible (the content file must exist for video preflight,
        // so it is created first).
        let root = temp_dir("apply-video");
        let video = root.join("clip.mp4");
        fs::write(&video, b"not really a video").unwrap();
        let (ok, result) = process_with_apply(
            &format!(
                r#"{{"version":1,"method":"wallpaper.apply","params":{{"output":"DP-1","wallpaper_id":"1","kind":"video","content":"{}"}}}}"#,
                video.display()
            ),
            &handle,
            &catalog,
        );
        assert!(!ok);
        assert_eq!(result["error"], "apply_incompatible");
        let detail = result["detail"].as_str().unwrap();
        assert!(
            detail.contains("scene") && detail.contains("video"),
            "{detail}"
        );
    }

    #[test]
    fn apply_invalid_params_are_rejected_at_the_boundary() {
        let catalog = scene_catalog();
        let supervisor = supervisor_service();
        let probe = stub_probe(vec![dp1_output()], Some(DP1_PROBE_REPLY));
        let handle = apply_handle(probe, &catalog, supervisor.handle());
        // The test renderer kind is never assignable.
        let (ok, result) = process_with_apply(
            r#"{"version":1,"method":"wallpaper.apply","params":{"output":"DP-1","wallpaper_id":"1","kind":"test"}}"#,
            &handle,
            &catalog,
        );
        assert!(!ok);
        assert_eq!(result["error"], "invalid_params");
        // Unknown fields fail closed.
        let (ok, result) = process_with_apply(
            r#"{"version":1,"method":"wallpaper.apply","params":{"output":"DP-1","wallpaper_id":"1","kind":"scene","content":"/tmp/x.json","bogus":1}}"#,
            &handle,
            &catalog,
        );
        assert!(!ok);
        assert_eq!(result["error"], "invalid_params");
        // Zero dimensions fail the StartSpec validation rules.
        let (ok, result) = process_with_apply(
            r#"{"version":1,"method":"wallpaper.apply","params":{"output":"DP-1","wallpaper_id":"1","kind":"scene","content":"/tmp/x.json","width":0,"height":540,"fps":30}}"#,
            &handle,
            &catalog,
        );
        assert!(!ok);
        assert_eq!(result["error"], "invalid_params");
    }

    #[test]
    fn concurrent_apply_reports_apply_busy() {
        let catalog = empty_catalog();
        let supervisor = supervisor_service();
        let probe = stub_probe(vec![dp1_output()], Some(DP1_PROBE_REPLY));
        let handle = apply_handle(probe, &catalog, supervisor.handle());
        let _guard = handle.acquire_apply_lock().unwrap();
        let (ok, result) = process_with_apply(
            r#"{"version":1,"method":"wallpaper.apply","params":{"output":"DP-1","wallpaper_id":"1","kind":"scene","content":"/tmp/scene.json"}}"#,
            &handle,
            &catalog,
        );
        assert!(!ok);
        assert_eq!(result["error"], "apply_busy");
    }

    #[test]
    fn apply_promotes_persists_and_switches_the_plasma_config() {
        let root = temp_dir("apply-happy");
        let catalog = scene_catalog();
        let supervisor_service = fast_scene_supervisor(&root);
        let supervisor = supervisor_service.handle();
        let probe = stub_probe_with_switch(vec![dp1_output()]);
        let handle = apply_handle(probe.clone(), &catalog, supervisor.clone());
        // The client content must match the catalog item's resolved path
        // (the renderer runs the catalog content).
        let scene = fixture_scene_path(&catalog.read().unwrap());
        let (ok, result) = process_with_apply(
            &format!(
                r#"{{"version":1,"method":"wallpaper.apply","params":{{"output":"DP-1","wallpaper_id":"1","kind":"scene","content":"{}","width":320,"height":180,"fps":30}}}}"#,
                scene.display()
            ),
            &handle,
            &catalog,
        );
        assert!(ok, "apply failed: {result}");
        assert_eq!(result["output"], "DP-1");
        assert_eq!(result["applied"]["wallpaper_id"], "1");
        assert_eq!(result["applied"]["kind"], "scene");
        assert_eq!(result["applied"]["width"], 320);
        assert_eq!(
            result["applied"]["previous"]["wallpaper_plugin"],
            "org.kde.image"
        );
        assert_eq!(
            result["applied"]["previous"]["config_group"],
            json!(["Wallpaper", "org.kde.image", "General"])
        );
        assert!(
            result["applied"]["applied_at_unix_seconds"]
                .as_u64()
                .unwrap()
                > 0
        );
        // The renderer really promoted (Live phase), not just started.
        let status = supervisor.status().unwrap();
        assert_eq!(status.phase, WorkerPhase::Live);
        assert_eq!(status.wallpaper_id.as_deref(), Some("1"));
        // The switch script was the exact pure function of the desktop
        // index and the kwe plugin — never wallpaper content. (The
        // enumeration probe also mentions wallpaperPlugin, so the switch
        // is the recorded script that is not a probe.)
        let switch = probe
            .scripts()
            .into_iter()
            .find(|script| !script.contains("screenForConnector"))
            .expect("the switch script must have been evaluated");
        assert_eq!(
            switch,
            "var d = desktops()[1]; if (!d) throw \"no desktop 1\"; d.wallpaperPlugin = \"org.kde.kwe.wallpaper\";"
        );
        // Wallpaper content never reaches the script.
        assert!(!switch.contains("scene.json"));
        // The assignment is persisted and round-trips.
        let (ok, result) = process_with_apply(
            r#"{"version":1,"method":"wallpaper.assignments"}"#,
            &handle,
            &catalog,
        );
        assert!(ok);
        let record = &result["outputs"]["DP-1"];
        assert_eq!(record["wallpaper_id"], "1");
        assert_eq!(record["kind"], "scene");
        assert_eq!(
            record["previous"]["image"],
            "file:///usr/share/wallpapers/fallback.png"
        );
    }

    #[test]
    fn apply_switch_failure_rolls_back_the_transaction() {
        let root = temp_dir("apply-rollback");
        let catalog = scene_catalog();
        let supervisor_service = fast_scene_supervisor(&root);
        let supervisor = supervisor_service.handle();
        let probe = stub_probe(vec![dp1_output()], Some(DP1_PROBE_REPLY));
        probe
            .reject_scripts
            .store(true, std::sync::atomic::Ordering::SeqCst);
        let handle = apply_handle(probe.clone(), &catalog, supervisor.clone());
        // No content: the catalog content is used, so the switch rejection
        // is reached without a content-match detour.
        let (ok, result) = process_with_apply(
            r#"{"version":1,"method":"wallpaper.apply","params":{"output":"DP-1","wallpaper_id":"1","kind":"scene"}}"#,
            &handle,
            &catalog,
        );
        assert!(!ok);
        assert_eq!(result["error"], "shell_unreachable");
        // The renderer was stopped and the assignment was dropped.
        let status = supervisor.status().unwrap();
        assert_eq!(status.phase, WorkerPhase::Stopped);
        let (ok, result) = process_with_apply(
            r#"{"version":1,"method":"wallpaper.assignments"}"#,
            &handle,
            &catalog,
        );
        assert!(ok);
        assert_eq!(result["outputs"].as_object().unwrap().len(), 0);
        // The switch script was attempted (the probe recorded it).
        assert!(
            probe
                .scripts()
                .iter()
                .any(|s| s.contains("wallpaperPlugin"))
        );
    }

    #[test]
    fn reapply_carries_the_original_previous_forward_and_rollback_restores_it() {
        let root = temp_dir("apply-reapply");
        let catalog = scene_catalog();
        let supervisor_service = fast_scene_supervisor(&root);
        let supervisor = supervisor_service.handle();
        let probe = stub_probe_with_switch(vec![dp1_output()]);
        let handle = apply_handle(probe.clone(), &catalog, supervisor.clone());
        let content = fixture_scene_path(&catalog.read().unwrap());

        // Apply #1 succeeds: previous = the live org.kde.image config.
        let (ok, result) = process_with_apply(
            &format!(
                r#"{{"version":1,"method":"wallpaper.apply","params":{{"output":"DP-1","wallpaper_id":"1","kind":"scene","content":"{}"}}}}"#,
                content.display()
            ),
            &handle,
            &catalog,
        );
        assert!(ok, "first apply failed: {result}");
        assert_eq!(
            result["applied"]["previous"]["image"],
            "file:///usr/share/wallpapers/fallback.png"
        );

        // Apply #2: the live enumeration now reports our plugin (the stub
        // flipped after the switch), so the stored record's ORIGINAL
        // previous must be carried forward — never replaced by our own
        // plugin state. Failing the switch exercises the rollback: the
        // pre-apply record must be set back, not removed.
        probe
            .reject_scripts
            .store(true, std::sync::atomic::Ordering::SeqCst);
        let (ok, result) = process_with_apply(
            &format!(
                r#"{{"version":1,"method":"wallpaper.apply","params":{{"output":"DP-1","wallpaper_id":"1","kind":"scene","content":"{}"}}}}"#,
                content.display()
            ),
            &handle,
            &catalog,
        );
        assert!(!ok);
        assert_eq!(result["error"], "shell_unreachable");
        // The original wallpaper config survives: the record is intact with
        // the original previous (org.kde.image + fallback.png), and the
        // failed re-apply did not destroy it.
        let (ok, result) = process_with_apply(
            r#"{"version":1,"method":"wallpaper.assignments"}"#,
            &handle,
            &catalog,
        );
        assert!(ok);
        let record = &result["outputs"]["DP-1"];
        assert_eq!(record["wallpaper_id"], "1");
        assert_eq!(record["previous"]["wallpaper_plugin"], "org.kde.image");
        assert_eq!(
            record["previous"]["image"],
            "file:///usr/share/wallpapers/fallback.png"
        );
    }

    #[test]
    fn apply_persist_failure_rolls_back_and_stops_the_renderer() {
        let root = temp_dir("apply-persist-fail");
        let catalog = scene_catalog();
        let supervisor_service = fast_scene_supervisor(&root);
        let supervisor = supervisor_service.handle();
        let probe = stub_probe(vec![dp1_output()], Some(DP1_PROBE_REPLY));
        // A store already at the 16-output bound: the persist of the new
        // output must fail AFTER the renderer promoted.
        let dir = temp_dir("apply-persist-store");
        let mut store = apply::AssignmentStore::open(&dir).unwrap();
        for index in 0..apply::MAX_ASSIGNED_OUTPUTS {
            store
                .set(
                    &format!("Synthetic-{index}"),
                    apply::Assignment {
                        wallpaper_id: format!("Synthetic-{index}"),
                        kind: RendererKind::Scene,
                        content: "/tmp/x.json".into(),
                        width: 320,
                        height: 180,
                        fps: 30,
                        scaling: ScalingMode::Aspect,
                        applied_at_unix_seconds: 1,
                        previous: None,
                    },
                )
                .unwrap();
        }
        let handle = apply_handle_with_store(probe.clone(), &catalog, supervisor.clone(), store);
        let (ok, result) = process_with_apply(
            r#"{"version":1,"method":"wallpaper.apply","params":{"output":"DP-1","wallpaper_id":"1","kind":"scene"}}"#,
            &handle,
            &catalog,
        );
        assert!(!ok);
        assert_eq!(result["error"], "apply_failed");
        assert!(
            result["detail"]
                .as_str()
                .unwrap()
                .contains("persist assignment failed"),
            "{result}"
        );
        // The renderer that promoted was stopped by the rollback — it must
        // not come up live later, unassigned and invisible to restore.
        let status = supervisor.status().unwrap();
        assert_eq!(status.phase, WorkerPhase::Stopped);
        // The 16 seeded records are untouched; DP-1 was never stored.
        let (ok, result) = process_with_apply(
            r#"{"version":1,"method":"wallpaper.assignments"}"#,
            &handle,
            &catalog,
        );
        assert!(ok);
        assert_eq!(result["outputs"].as_object().unwrap().len(), 16);
        assert!(result["outputs"]["DP-1"].is_null());
    }

    #[test]
    fn apply_promotion_timeout_rolls_back_and_stops_the_renderer() {
        let root = temp_dir("apply-promotion-timeout");
        let catalog = scene_catalog();
        // A renderer that never publishes a frame: it can never promote,
        // and the bounded wait must time out instead of hanging.
        let hang = root.join("hang-renderer.py");
        fs::write(
            &hang,
            "#!/usr/bin/env python3\nimport time\ntime.sleep(60)\n",
        )
        .unwrap();
        fs::set_permissions(&hang, fs::Permissions::from_mode(0o755)).unwrap();
        let dir = root.join("supervisor");
        let limits = sample_limits(1024);
        let supervisor_service = SupervisorService::start(SupervisorConfig {
            renderer_paths: BTreeMap::from([(RendererKind::Scene, hang)]),
            runtime_dir: dir.join("runtime"),
            state_dir: dir.join("state"),
            startup_timeout_ms_by_kind: BTreeMap::from([
                (RendererKind::Test, 3000),
                (RendererKind::Video, 6000),
                (RendererKind::Web, 10_000),
                (RendererKind::Scene, 3000),
            ]),
            frame_timeout: Duration::from_secs(2),
            stop_grace: Duration::from_millis(500),
            restart_delay: Duration::from_millis(250),
            canary_duration: Duration::from_millis(150),
            handoff_timeout: Duration::from_secs(5),
            max_failures: 3,
            web_heartbeat_ms: 5000,
            web_heartbeat_max_failures: 3,
            resource_limits_by_kind: BTreeMap::from([
                (RendererKind::Test, limits),
                (RendererKind::Video, limits),
                (RendererKind::Web, limits),
                (RendererKind::Scene, limits),
            ]),
            scene_assets_dir: None,
        })
        .unwrap();
        let supervisor = supervisor_service.handle();
        let probe = stub_probe(vec![dp1_output()], Some(DP1_PROBE_REPLY));
        let store_dir = temp_dir("apply-promotion-store");
        let handle = apply::ApplyHandle::for_test(
            apply::AssignmentStore::open(&store_dir).unwrap(),
            probe,
            catalog.clone(),
            supervisor.clone(),
            // Far below the 3 s startup timeout so the wait times out while
            // the renderer is still starting.
            Duration::from_millis(300),
        );
        let (ok, result) = process_with_apply(
            r#"{"version":1,"method":"wallpaper.apply","params":{"output":"DP-1","wallpaper_id":"1","kind":"scene"}}"#,
            &handle,
            &catalog,
        );
        assert!(!ok);
        assert_eq!(result["error"], "apply_failed");
        assert!(
            result["detail"]
                .as_str()
                .unwrap()
                .contains("did not promote"),
            "{result}"
        );
        // The never-promoting renderer was stopped by the rollback.
        let status = supervisor.status().unwrap();
        assert_eq!(status.phase, WorkerPhase::Stopped);
    }

    #[test]
    fn apply_ownership_change_fails_fast_without_stopping_the_other_renderer() {
        let root = temp_dir("apply-ownership");
        let catalog = scene_catalog();
        let supervisor_service = fast_scene_supervisor(&root);
        let supervisor = supervisor_service.handle();
        let probe = stub_probe(vec![dp1_output()], Some(DP1_PROBE_REPLY));
        let handle = apply_handle(probe, &catalog, supervisor.clone());
        let content = fixture_scene_path(&catalog.read().unwrap());

        // Apply runs on its own thread; while it waits for promotion, the
        // "playlist session" replaces the renderer with a different
        // wallpaper. The apply must fail fast — not wait out a misleading
        // timeout — and must NOT stop the renderer it no longer owns.
        let foreign_content = content.clone();
        let apply_thread = {
            let handle = handle.clone();
            let catalog = catalog.clone();
            std::thread::spawn(move || {
                process_with_apply(
                    &format!(
                        r#"{{"version":1,"method":"wallpaper.apply","params":{{"output":"DP-1","wallpaper_id":"1","kind":"scene","content":"{}"}}}}"#,
                        content.display()
                    ),
                    &handle,
                    &catalog,
                )
            })
        };
        std::thread::sleep(Duration::from_millis(150));
        let foreign = supervisor
            .start(StartSpec {
                wallpaper_id: "other".into(),
                content_hash: "foreign-hash".into(),
                width: 320,
                height: 180,
                fps: 30,
                kind: RendererKind::Scene,
                content: Some(ContentSpec::Scene {
                    path: foreign_content,
                }),
                test_fault: None,
                stderr_lines: None,
                scaling: ScalingMode::Aspect,
                capability_limitations: Vec::new(),
            })
            .unwrap();
        assert_eq!(foreign.requested_wallpaper_id.as_deref(), Some("other"));

        let (ok, result) = apply_thread.join().expect("apply thread panicked");
        assert!(!ok);
        assert_eq!(result["error"], "apply_failed");
        assert!(
            result["detail"]
                .as_str()
                .unwrap()
                .contains("ownership changed"),
            "{result}"
        );
        // The foreign renderer is still running: the rollback only stops
        // the renderer it started.
        let status = supervisor.status().unwrap();
        assert_eq!(status.requested_wallpaper_id.as_deref(), Some("other"));
        assert_ne!(status.phase, WorkerPhase::Stopped);
        assert_ne!(status.phase, WorkerPhase::Idle);
        // Nothing was persisted.
        let (ok, result) = process_with_apply(
            r#"{"version":1,"method":"wallpaper.assignments"}"#,
            &handle,
            &catalog,
        );
        assert!(ok);
        assert_eq!(result["outputs"].as_object().unwrap().len(), 0);
    }

    #[test]
    fn apply_content_mismatch_is_rejected_before_the_shell_is_touched() {
        let catalog = scene_catalog();
        let supervisor = supervisor_service();
        let probe = stub_probe(vec![dp1_output()], Some(DP1_PROBE_REPLY));
        let handle = apply_handle(probe.clone(), &catalog, supervisor.handle());
        // A real scene.json that is NOT the catalog item's resolved content
        // must be rejected — the renderer only runs catalog content.
        let root = temp_dir("apply-content-mismatch");
        let scene = root.join("scene.json");
        fs::write(&scene, br#"{"general":{}}"#).unwrap();
        let (ok, result) = process_with_apply(
            &format!(
                r#"{{"version":1,"method":"wallpaper.apply","params":{{"output":"DP-1","wallpaper_id":"1","kind":"scene","content":"{}"}}}}"#,
                scene.display()
            ),
            &handle,
            &catalog,
        );
        assert!(!ok);
        assert_eq!(result["error"], "invalid_params");
        assert!(
            result["detail"]
                .as_str()
                .unwrap()
                .contains("does not match the catalog"),
            "{result}"
        );
        // The shell was never probed and no renderer was started.
        assert!(probe.scripts().is_empty());
    }

    #[test]
    fn apply_content_path_is_bounded_at_the_params_boundary() {
        let catalog = empty_catalog();
        let supervisor = supervisor_service();
        let probe = stub_probe(vec![dp1_output()], Some(DP1_PROBE_REPLY));
        let handle = apply_handle(probe, &catalog, supervisor.handle());
        let oversized = format!("/tmp/{}", "x".repeat(4096));
        let (ok, result) = process_with_apply(
            &format!(
                r#"{{"version":1,"method":"wallpaper.apply","params":{{"output":"DP-1","wallpaper_id":"1","kind":"scene","content":"{}"}}}}"#,
                oversized
            ),
            &handle,
            &catalog,
        );
        assert!(!ok);
        assert_eq!(result["error"], "invalid_params");
        assert!(
            result["detail"].as_str().unwrap().contains("characters"),
            "{result}"
        );
    }

    #[test]
    fn restore_without_assignment_falls_back_to_the_stock_image() {
        let catalog = empty_catalog();
        let supervisor = supervisor_service();
        let probe = stub_probe(vec![dp1_output()], Some(DP1_PROBE_REPLY));
        let handle = apply_handle(probe.clone(), &catalog, supervisor.handle());
        let (ok, result) = process_with_apply(
            r#"{"version":1,"method":"wallpaper.restore","params":{"output":"DP-1"}}"#,
            &handle,
            &catalog,
        );
        assert!(ok);
        assert_eq!(result["mode"], "stock");
        assert_eq!(result["restored"]["wallpaper_plugin"], "org.kde.image");
        // The last non-probe script is the restore (the verification probe
        // runs after it and reports the stock plugin back).
        let scripts = probe.scripts();
        let restore = scripts
            .iter()
            .rev()
            .find(|script| !script.contains("screenForConnector"))
            .expect("the restore script must have been evaluated");
        match result["stock_image"].as_str() {
            // A stock image present on this system was recorded and
            // scripted into the restore.
            Some(stock) => {
                assert!(
                    stock.starts_with("/usr/share/"),
                    "unexpected stock image: {stock}"
                );
                assert!(
                    restore.contains(&format!("d.writeConfig(\"Image\", \"{stock}\")")),
                    "{restore}"
                );
            }
            // No stock image on this system: the plugin still restores via
            // its theme default, with no Image write at all.
            None => assert!(!restore.contains("writeConfig"), "{restore}"),
        }
        assert!(restore.ends_with("d.wallpaperPlugin = \"org.kde.image\";"));
        assert!(!restore.contains("kwe.wallpaper"));
    }

    #[test]
    fn restore_with_stored_assignment_reverts_and_clears_it() {
        let catalog = empty_catalog();
        let supervisor = supervisor_service();
        let probe = stub_probe(vec![dp1_output()], Some(DP1_PROBE_REPLY));
        let dir = temp_dir("apply-restore");
        let mut store = apply::AssignmentStore::open(&dir).unwrap();
        store
            .set(
                "DP-1",
                apply::Assignment {
                    wallpaper_id: "1".into(),
                    kind: RendererKind::Scene,
                    content: "/tmp/scene.json".into(),
                    width: 320,
                    height: 180,
                    fps: 30,
                    scaling: ScalingMode::Aspect,
                    applied_at_unix_seconds: 42,
                    previous: Some(apply::PreviousWallpaper {
                        wallpaper_plugin: "org.kde.image".into(),
                        config_group: vec![
                            "Wallpaper".into(),
                            "org.kde.image".into(),
                            "General".into(),
                        ],
                        image: Some("file:///usr/share/wallpapers/old.png".into()),
                    }),
                },
            )
            .unwrap();
        let handle = apply_handle_with_store(probe.clone(), &catalog, supervisor.handle(), store);
        let (ok, result) = process_with_apply(
            r#"{"version":1,"method":"wallpaper.restore","params":{"output":"DP-1"}}"#,
            &handle,
            &catalog,
        );
        assert!(ok);
        assert_eq!(result["mode"], "assignment");
        assert_eq!(
            result["restored"]["image"],
            "file:///usr/share/wallpapers/old.png"
        );
        let scripts = probe.scripts();
        let restore = scripts
            .iter()
            .rev()
            .find(|script| !script.contains("screenForConnector"))
            .expect("the restore script must have been evaluated");
        assert!(
            restore.contains("d.writeConfig(\"Image\", \"file:///usr/share/wallpapers/old.png\")"),
            "{restore}"
        );
        // The assignment is cleared once the restore script ran.
        let (ok, result) = process_with_apply(
            r#"{"version":1,"method":"wallpaper.assignments"}"#,
            &handle,
            &catalog,
        );
        assert!(ok);
        assert_eq!(result["outputs"].as_object().unwrap().len(), 0);
    }

    #[test]
    fn restore_unknown_output_reports_output_missing() {
        let catalog = empty_catalog();
        let supervisor = supervisor_service();
        let probe = stub_probe(vec![], Some(DP1_PROBE_REPLY));
        let handle = apply_handle(probe, &catalog, supervisor.handle());
        let (ok, result) = process_with_apply(
            r#"{"version":1,"method":"wallpaper.restore","params":{"output":"DP-1"}}"#,
            &handle,
            &catalog,
        );
        assert!(!ok);
        assert_eq!(result["error"], "output_missing");
    }

    #[test]
    fn wallpaper_methods_fail_closed_without_an_apply_handle() {
        let catalog = empty_catalog();
        let requests = [
            r#"{"version":1,"method":"wallpaper.outputs"}"#,
            r#"{"version":1,"method":"wallpaper.apply","params":{"output":"DP-1","wallpaper_id":"1","kind":"scene","content":"/tmp/scene.json"}}"#,
            r#"{"version":1,"method":"wallpaper.restore","params":{"output":"DP-1"}}"#,
            r#"{"version":1,"method":"wallpaper.assignments"}"#,
        ];
        for request_json in requests {
            let request: Request = serde_json::from_str(request_json).unwrap();
            let method = request.method.clone();
            let (ok, result) = process_request(
                &request,
                &catalog,
                &[],
                None,
                None,
                &cache_for_tests(),
                None,
                None,
                None,
                PeerCred::default(),
                &empty_worker_pid(),
                false,
            )
            .unwrap();
            assert!(!ok, "{method} must fail closed");
            assert_eq!(result["error"], "apply_unavailable", "{method}");
        }
    }

    // --- BETA_M4c: playlist renderer assignment through the apply lane ----

    /// A three-entry 10 s daily playlist over the scene fixture.
    fn daily_scene_playlist() -> Playlist {
        let mut playlist = Playlist::new("daily".into(), "Daily".into()).unwrap();
        for id in ["1", "2", "3"] {
            playlist.add(id.into()).unwrap();
        }
        playlist.duration_seconds = 10;
        playlist
    }

    /// The wallpaper id of a decision serialized over the API (the state
    /// machine drives `maybe_apply` from the same serde shape the protocol
    /// exposes).
    fn decision_wallpaper(decision: &PlaylistDecision) -> String {
        serde_json::to_value(decision).unwrap()["wallpaper_id"]
            .as_str()
            .unwrap()
            .to_string()
    }

    /// A session wired to the real apply lane (50 ms tick).
    fn session_with_apply(
        dir: PathBuf,
        supervisor: SupervisorHandle,
        lane: Arc<dyn PlaylistApplyLane>,
        valid: &[&str],
    ) -> PlaylistSessionService {
        PlaylistSessionService::start(PlaylistSessionConfig {
            state_dir: dir,
            tick_ms: 50,
            supervisor: Some(supervisor),
            valid_ids: Arc::new(valid.iter().map(|id| id.to_string()).collect()),
            output: None,
            apply: Some(lane),
        })
    }

    /// Poll renderer.status until the supervisor is live on `wallpaper_id`
    /// AND the apply transaction has reached (and passed) its switch
    /// script: `expected_switches` scripts evaluated. Promotion alone can
    /// return while the transaction is still between persist and switch,
    /// so every side effect the assertions read (store write precedes the
    /// switch) is settled once this returns.
    fn wait_for_settled(
        supervisor: &SupervisorHandle,
        probe: &apply::StubProbe,
        wallpaper_id: &str,
        expected_switches: usize,
    ) -> WorkerStatus {
        let deadline = Instant::now() + Duration::from_secs(15);
        loop {
            let status = supervisor.status().unwrap();
            let switches = kwe_switch_count(probe);
            if status.requested_wallpaper_id.as_deref() == Some(wallpaper_id)
                && matches!(status.phase, WorkerPhase::Live | WorkerPhase::AwaitingAck)
                && switches >= expected_switches
            {
                return status;
            }
            assert!(
                Instant::now() < deadline,
                "timed out waiting for renderer {wallpaper_id} to settle (switch count {switches}/{expected_switches}): {status:?}"
            );
            std::thread::sleep(Duration::from_millis(20));
        }
    }

    /// Number of kwe switch scripts the stub probe evaluated so far.
    fn kwe_switch_count(probe: &apply::StubProbe) -> usize {
        probe
            .scripts()
            .iter()
            .filter(|script| script.contains("wallpaperPlugin = \"org.kde.kwe.wallpaper\""))
            .count()
    }

    /// The scene catalog with entries 1..=3 shared by the playlist tests.
    fn playlist_catalog() -> Arc<RwLock<Catalog>> {
        scene_catalog_with(&["1", "2", "3"])
    }

    #[test]
    fn playlist_entry_change_applies_through_the_real_transaction() {
        let root = temp_dir("playlist-apply");
        let catalog = playlist_catalog();
        let supervisor_service = fast_scene_supervisor(&root);
        let supervisor = supervisor_service.handle();
        let probe = stub_probe_with_switch(vec![dp1_output()]);
        let handle = apply_handle(probe.clone(), &catalog, supervisor.clone());
        let lane: Arc<dyn PlaylistApplyLane> = Arc::new(handle.clone());
        let session = session_with_apply(
            temp_dir("playlist-apply-state"),
            supervisor.clone(),
            lane,
            &["1", "2", "3"],
        );
        let session_handle = session.handle();
        session_handle.put(daily_scene_playlist()).unwrap();
        let activate = session_handle.activate(Some("daily".into())).unwrap();
        assert_eq!(decision_wallpaper(activate.decision.as_ref().unwrap()), "1");

        // The session applies the first entry through the shared apply
        // transaction: the fake scene renderer goes live on 1, the switch
        // stub runs, and the assignment store records DP-1 -> 1 (scene).
        wait_for_settled(&supervisor, &probe, "1", 1);
        let assignments = handle.assignments().unwrap();
        assert_eq!(assignments["outputs"]["DP-1"]["wallpaper_id"], "1");
        assert_eq!(assignments["outputs"]["DP-1"]["kind"], "scene");
        assert_eq!(kwe_switch_count(&probe), 1);

        // Timer advance drives the next entry: once the 10 s entry expires
        // on the real clock, the session displaces its own stale renderer
        // with a hard cut through the same transaction. debug_clock_skip is
        // unusable here — it freezes remaining time (suspend simulation),
        // so the advance must be real.
        std::thread::sleep(Duration::from_millis(10_800));
        wait_for_settled(&supervisor, &probe, "2", 2);
        let assignments = handle.assignments().unwrap();
        assert_eq!(assignments["outputs"]["DP-1"]["wallpaper_id"], "2");

        std::thread::sleep(Duration::from_millis(10_800));
        wait_for_settled(&supervisor, &probe, "3", 3);
        let assignments = handle.assignments().unwrap();
        assert_eq!(assignments["outputs"]["DP-1"]["wallpaper_id"], "3");

        // Exactly one apply per entry change (1 + 1 + 1 switch scripts).
        assert_eq!(kwe_switch_count(&probe), 3);

        // Steady state: while the entry is live the session never re-applies
        // (no churn on the supervisor slot, no extra switch scripts).
        let before = supervisor.status().unwrap();
        std::thread::sleep(Duration::from_millis(700));
        let after = supervisor.status().unwrap();
        assert_eq!(after.phase, before.phase);
        assert_eq!(after.requested_wallpaper_id, before.requested_wallpaper_id);
        assert_eq!(after.restart_count, before.restart_count);
        assert_eq!(kwe_switch_count(&probe), 3);
    }

    #[test]
    fn playlist_output_resolution_falls_through_when_the_stored_output_is_stale() {
        // Finding 3: the store-derived output (per-display intent) must be
        // validated against the fresh enumeration. A hotplugged-away display
        // falls through to the first enabled+connected output instead of
        // failing with output_missing + backoff as the docs promise.
        let catalog = empty_catalog();
        let supervisor = supervisor_service();
        let store_dir = temp_dir("playlist-stale-store");
        let mut store = apply::AssignmentStore::open(&store_dir).unwrap();
        store
            .set(
                "DP-1",
                apply::Assignment {
                    wallpaper_id: "1".into(),
                    kind: RendererKind::Scene,
                    content: "/tmp/scene.json".into(),
                    width: 320,
                    height: 180,
                    fps: 30,
                    scaling: ScalingMode::Aspect,
                    applied_at_unix_seconds: 1,
                    previous: None,
                },
            )
            .unwrap();
        // The live bus no longer has DP-1 (hotplugged away); DP-2 is the
        // first enabled+connected output.
        let dp2 = apply::SystemOutput {
            name: "DP-2".into(),
            enabled: true,
            connected: true,
            geometry: Some([0, 0, 1920, 1080]),
        };
        let probe = stub_probe(
            vec![dp2],
            Some(
                r#"{"desktops":[{"index":1,"id":112,"screen":0,"wp":"org.kde.image","image":null}],"connectors":{"DP-2":0}}"#,
            ),
        );
        let handle = apply_handle_with_store(probe, &catalog, supervisor.handle(), store);
        let entries: BTreeSet<String> = ["1", "2", "3"].iter().map(|id| id.to_string()).collect();
        let resolved = handle
            .resolve_playlist_output(None, &entries)
            .expect("the stale stored output must fall through to a live output");
        assert_eq!(
            resolved, "DP-2",
            "a hotplugged-away stored output must fall through to the first enabled+connected output"
        );
    }

    #[test]
    fn playlist_lane_yields_to_a_live_foreign_renderer_after_the_lock() {
        // Finding 1 (TOCTOU): the session computes its verdict from a
        // supervisor.status() read taken BEFORE the apply lock. If a user
        // apply completes in that window, the lane's post-lock re-read must
        // catch the now-live foreign renderer and yield (a non-failure
        // `Yielded`), not displace the user's fresh renderer.
        let root = temp_dir("playlist-lane-yield");
        let catalog = playlist_catalog();
        let supervisor_service = fast_scene_supervisor(&root);
        let supervisor = supervisor_service.handle();
        let probe = stub_probe_with_switch(vec![dp1_output()]);
        let handle = apply_handle(probe.clone(), &catalog, supervisor.clone());
        // A user apply brings wallpaper 2 live through the shared transaction.
        handle
            .apply(ApplyWallpaperParams {
                output: "DP-1".into(),
                wallpaper_id: "2".into(),
                kind: RendererKind::Scene,
                content: Some(fixture_scene_path_for(&catalog.read().unwrap(), "2")),
                width: Some(320),
                height: Some(180),
                fps: 30,
                retry: false,
                scaling: ScalingMode::Aspect,
            })
            .unwrap();
        wait_for_settled(&supervisor, &probe, "2", 1);
        // The lane must yield to the foreign renderer instead of displacing
        // it: a Yielded outcome, no new switch script, the user's renderer
        // stays live.
        let entries: BTreeSet<String> = ["1", "2", "3"].iter().map(|id| id.to_string()).collect();
        let result = handle.apply_playlist(None, "1".into(), &entries, None);
        assert!(
            matches!(result, Err(apply::ApplyError::Yielded(_))),
            "the lane must yield to a live foreign renderer, got {result:?}"
        );
        let status = supervisor.status().unwrap();
        assert_eq!(
            status.requested_wallpaper_id.as_deref(),
            Some("2"),
            "the user's renderer must stay live after the playlist yields"
        );
        assert_eq!(
            kwe_switch_count(&probe),
            1,
            "the lane must not switch when it yields to a foreign renderer"
        );
    }

    #[test]
    fn user_apply_takes_precedence_and_the_playlist_reasserts_after_stop() {
        let root = temp_dir("playlist-precedence");
        let catalog = playlist_catalog();
        let supervisor_service = fast_scene_supervisor(&root);
        let supervisor = supervisor_service.handle();
        let probe = stub_probe_with_switch(vec![dp1_output()]);
        let handle = apply_handle(probe.clone(), &catalog, supervisor.clone());
        let lane: Arc<dyn PlaylistApplyLane> = Arc::new(handle.clone());
        let session = session_with_apply(
            temp_dir("playlist-precedence-state"),
            supervisor.clone(),
            lane,
            &["1", "2", "3"],
        );
        let session_handle = session.handle();
        session_handle.put(daily_scene_playlist()).unwrap();
        session_handle.activate(Some("daily".into())).unwrap();
        wait_for_settled(&supervisor, &probe, "1", 1);

        // The USER applies wallpaper 2 through the API on the same
        // transaction: it displaces the session's renderer and stays live.
        let user_apply = handle.apply(ApplyWallpaperParams {
            output: "DP-1".into(),
            wallpaper_id: "2".into(),
            kind: RendererKind::Scene,
            content: Some(fixture_scene_path_for(&catalog.read().unwrap(), "2")),
            width: Some(320),
            height: Some(180),
            fps: 30,
            retry: false,
            scaling: ScalingMode::Aspect,
        });
        assert!(
            user_apply.is_ok(),
            "user apply must succeed: {user_apply:?}"
        );
        wait_for_settled(&supervisor, &probe, "2", 2);

        // The session yields: the user's renderer stays live and the
        // playlist does not fight it (no third switch script while the
        // user's wallpaper is live).
        std::thread::sleep(Duration::from_millis(700));
        let status = supervisor.status().unwrap();
        assert_eq!(status.requested_wallpaper_id.as_deref(), Some("2"));
        assert!(matches!(
            status.phase,
            WorkerPhase::Live | WorkerPhase::AwaitingAck
        ));
        assert_eq!(kwe_switch_count(&probe), 2);

        // When the user's renderer stops, the session re-asserts its entry
        // through the lane (manual-stop re-assert path).
        supervisor.stop().unwrap();
        wait_for_settled(&supervisor, &probe, "1", 3);
        let assignments = handle.assignments().unwrap();
        assert_eq!(assignments["outputs"]["DP-1"]["wallpaper_id"], "1");
    }

    #[test]
    fn playlist_restart_restore_reapplies_the_entry_once() {
        let root = temp_dir("playlist-restart");
        let catalog = playlist_catalog();
        let state_dir = temp_dir("playlist-restart-state");

        // First daemon: the session applies entry 1; the runtime persists
        // its position (entry 1) at shutdown.
        let (first_switches, first_assignments) = {
            let supervisor_service = fast_scene_supervisor(&root);
            let supervisor = supervisor_service.handle();
            let probe = stub_probe_with_switch(vec![dp1_output()]);
            let handle = apply_handle(probe.clone(), &catalog, supervisor.clone());
            let lane: Arc<dyn PlaylistApplyLane> = Arc::new(handle.clone());
            let session = session_with_apply(
                state_dir.clone(),
                supervisor.clone(),
                lane,
                &["1", "2", "3"],
            );
            let session_handle = session.handle();
            session_handle.put(daily_scene_playlist()).unwrap();
            session_handle.activate(Some("daily".into())).unwrap();
            wait_for_settled(&supervisor, &probe, "1", 1);
            // The block's drop order is reverse declaration: the session
            // drops before the supervisor service, so its shutdown tick
            // persists the runtime before the renderer is torn down.
            let probes = kwe_switch_count(&probe);
            let assignments = handle.assignments().unwrap();
            (probes, assignments)
        };
        assert_eq!(first_switches, 1);
        assert_eq!(first_assignments["outputs"]["DP-1"]["wallpaper_id"], "1");

        // Second daemon on the same state: the supervisor is fresh (the
        // restored renderer is dead) even though the assignment store still
        // records DP-1 -> 1 — the session must re-apply its restored entry
        // exactly once.
        let supervisor_service = fast_scene_supervisor(&root);
        let supervisor = supervisor_service.handle();
        let probe = stub_probe_with_switch(vec![dp1_output()]);
        let handle = apply_handle(probe.clone(), &catalog, supervisor.clone());
        let lane: Arc<dyn PlaylistApplyLane> = Arc::new(handle.clone());
        let session = session_with_apply(state_dir, supervisor.clone(), lane, &["1", "2", "3"]);
        let session_handle = session.handle();
        session_handle.activate(Some("daily".into())).unwrap();
        wait_for_settled(&supervisor, &probe, "1", 1);

        // The restored renderer is live and the assignment matches; exactly
        // one new switch script ran and nothing re-applied afterwards.
        let status = supervisor.status().unwrap();
        assert!(matches!(
            status.phase,
            WorkerPhase::Live | WorkerPhase::AwaitingAck
        ));
        assert_eq!(status.requested_wallpaper_id.as_deref(), Some("1"));
        let assignments = handle.assignments().unwrap();
        assert_eq!(assignments["outputs"]["DP-1"]["wallpaper_id"], "1");
        assert_eq!(kwe_switch_count(&probe), 1);
        std::thread::sleep(Duration::from_millis(700));
        assert_eq!(kwe_switch_count(&probe), 1);
        assert_eq!(
            supervisor
                .status()
                .unwrap()
                .requested_wallpaper_id
                .as_deref(),
            Some("1")
        );
    }
}
