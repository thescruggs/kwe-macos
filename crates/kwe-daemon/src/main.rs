// SPDX-License-Identifier: Apache-2.0
//! Small Alpha control service. The newline-delimited protocol is deliberately
//! bounded and versioned so the UI never parses Workshop content itself.

mod persist;
mod playlist_session;
mod supervisor;
mod workshop_cache;

use std::{
    collections::{BTreeMap, BTreeSet},
    fs,
    io::{BufReader, Read, Write},
    os::unix::fs::{FileTypeExt, PermissionsExt},
    os::unix::net::{UnixListener, UnixStream},
    path::{Path, PathBuf},
    sync::{Arc, RwLock},
    time::{Duration, Instant},
};

use anyhow::{Context, Result, anyhow, bail};
use clap::Parser;
use kwe_core::{Catalog, ProjectKind, ScanLimits, default_steam_roots, scan_installed};
use kwe_input_protocol::{AudioFrame, MediaState, PointerButton, PointerPhase};
use playlist_session::{
    ImportPlaylist, PlaylistSessionConfig, PlaylistSessionHandle, PlaylistSessionService,
    SessionError,
};
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};
use supervisor::{
    ContentSpec, RendererKind, RendererResourceLimits, StartSpec, SupervisorConfig,
    SupervisorHandle, SupervisorService, TestFault, WorkerStatus,
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
    /// Web renderers reserve a 4 GiB virtual cage for V8 before main; the
    /// global RLIMIT_AS default would kill Chromium at exec.
    #[arg(long, default_value_t = 16384, value_parser = clap::value_parser!(u64).range(256..=65536))]
    renderer_web_address_space_mib: u64,
    /// Chromium needs more descriptors than the test renderer.
    #[arg(long, default_value_t = 1024, value_parser = clap::value_parser!(u64).range(32..=4096))]
    renderer_web_open_files: u64,
    /// UID-scoped process ceiling inherited by each renderer.
    #[arg(long, default_value_t = 1024, value_parser = clap::value_parser!(u64).range(64..=32768))]
    renderer_processes: u64,
    /// Enable synthetic hang/corruption/exit requests for development tests.
    #[arg(long)]
    allow_test_faults: bool,
    /// Playlist session tick interval.
    #[arg(long, default_value_t = 500, value_parser = clap::value_parser!(u64).range(50..=5000))]
    playlist_tick_ms: u64,
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
    let startup_timeout_ms_by_kind = BTreeMap::from([
        (RendererKind::Test, arguments.renderer_startup_timeout_ms),
        (
            RendererKind::Video,
            arguments.renderer_video_startup_timeout_ms,
        ),
        (RendererKind::Web, arguments.renderer_web_startup_timeout_ms),
        (RendererKind::Scene, arguments.renderer_startup_timeout_ms),
    ]);
    let global_limits = RendererResourceLimits {
        address_space_mib: arguments.renderer_address_space_mib,
        file_size_mib: 160,
        open_files: arguments.renderer_open_files,
        processes: arguments.renderer_processes,
        core_dump_bytes: 0,
    };
    let resource_limits_by_kind = BTreeMap::from([
        (RendererKind::Test, global_limits),
        (RendererKind::Video, global_limits),
        (
            RendererKind::Web,
            RendererResourceLimits {
                address_space_mib: arguments.renderer_web_address_space_mib,
                open_files: arguments.renderer_web_open_files,
                ..global_limits
            },
        ),
        (RendererKind::Scene, global_limits),
    ]);
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
        resource_limits_by_kind,
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
    let playlist_service = PlaylistSessionService::start(PlaylistSessionConfig {
        state_dir: playlist_state_dir,
        tick_ms: arguments.playlist_tick_ms,
        supervisor: Some(supervisor.clone()),
        valid_ids: compute_valid_ids(&catalog),
    });
    let playlist = playlist_service.handle();
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

fn handle_client(
    mut stream: UnixStream,
    catalog: &Arc<RwLock<Catalog>>,
    roots: &[PathBuf],
    supervisor: &SupervisorHandle,
    playlist: &PlaylistSessionHandle,
    workshop_cache: &Arc<std::sync::Mutex<WorkshopCache>>,
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
    let (ok, result) = process_request(
        &request,
        catalog,
        roots,
        Some(supervisor),
        Some(playlist),
        workshop_cache,
        allow_test_faults,
    )?;
    let response = Response {
        version: API_VERSION,
        id: &request.id,
        ok,
        result,
    };
    serde_json::to_writer(&mut stream, &response)?;
    stream.write_all(b"\n")?;
    stream.flush()?;
    Ok(())
}

fn process_request(
    request: &Request,
    catalog: &Arc<RwLock<Catalog>>,
    roots: &[PathBuf],
    supervisor: Option<&SupervisorHandle>,
    playlist: Option<&PlaylistSessionHandle>,
    workshop_cache: &Arc<std::sync::Mutex<WorkshopCache>>,
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
                json!({"status": "ready", "catalog_items": count})
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
                            Ok(frame) => supervisor_call(supervisor, |handle| {
                                handle.audio_frame(params.generation, frame)
                            }),
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
                    Ok(params) => match StartSpec::try_from(params) {
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
        };
        // Single validation point per start: the supervisor event loop no
        // longer re-validates, so content preflight cannot block it twice.
        spec.into_validated()
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
    use super::*;

    fn empty_catalog() -> Arc<RwLock<Catalog>> {
        Arc::new(RwLock::new(scan_installed(&[], &ScanLimits::default())))
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
            false,
        )
        .unwrap();
        assert!(!ok);
        assert_eq!(result["error"], "test_faults_disabled");
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
            true,
        )
        .unwrap()
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
}
