// SPDX-License-Identifier: Apache-2.0
//! Small Alpha control service. The newline-delimited protocol is deliberately
//! bounded and versioned so the UI never parses Workshop content itself.

mod supervisor;

use std::{
    fs,
    io::{BufRead, BufReader, Read, Write},
    os::unix::fs::{FileTypeExt, PermissionsExt},
    os::unix::net::{UnixListener, UnixStream},
    path::{Path, PathBuf},
    sync::{Arc, RwLock},
    time::Duration,
};

use anyhow::{Context, Result, anyhow, bail};
use clap::Parser;
use kwe_core::{Catalog, ScanLimits, default_steam_roots, scan_installed};
use kwe_input_protocol::PointerPhase;
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};
use supervisor::{
    RendererResourceLimits, StartSpec, SupervisorConfig, SupervisorHandle, SupervisorService,
    TestFault, WorkerStatus,
};

const API_VERSION: u32 = 1;
const MAX_REQUEST_BYTES: usize = 64 * 1024;

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
    /// Generated renderer executable supervised by this alpha daemon.
    #[arg(long)]
    renderer: Option<PathBuf>,
    /// Private directory for ephemeral renderer frame files.
    #[arg(long)]
    renderer_runtime_dir: Option<PathBuf>,
    /// Private directory for quarantine state and the last-good still image.
    #[arg(long)]
    state_dir: Option<PathBuf>,
    #[arg(long, default_value_t = 3000, value_parser = clap::value_parser!(u64).range(100..=30000))]
    renderer_startup_timeout_ms: u64,
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
    /// UID-scoped process ceiling inherited by each renderer.
    #[arg(long, default_value_t = 1024, value_parser = clap::value_parser!(u64).range(64..=32768))]
    renderer_processes: u64,
    /// Enable synthetic hang/corruption/exit requests for development tests.
    #[arg(long)]
    allow_test_faults: bool,
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
    let renderer_path = arguments.renderer.unwrap_or(default_renderer_path()?);
    let renderer_runtime_dir = arguments
        .renderer_runtime_dir
        .unwrap_or_else(|| socket.parent().unwrap_or(Path::new(".")).join("renderers"));
    let state_dir = match arguments.state_dir {
        Some(path) => path,
        None => default_state_dir()?,
    };
    let supervisor_service = SupervisorService::start(SupervisorConfig {
        renderer_path,
        runtime_dir: renderer_runtime_dir,
        state_dir,
        startup_timeout: Duration::from_millis(arguments.renderer_startup_timeout_ms),
        frame_timeout: Duration::from_millis(arguments.renderer_frame_timeout_ms),
        stop_grace: Duration::from_millis(arguments.renderer_stop_grace_ms),
        restart_delay: Duration::from_millis(arguments.renderer_restart_delay_ms),
        canary_duration: Duration::from_millis(arguments.renderer_canary_ms),
        handoff_timeout: Duration::from_millis(arguments.renderer_handoff_timeout_ms),
        max_failures: arguments.renderer_max_failures,
        resource_limits: RendererResourceLimits {
            address_space_mib: arguments.renderer_address_space_mib,
            file_size_mib: 160,
            open_files: arguments.renderer_open_files,
            processes: arguments.renderer_processes,
            core_dump_bytes: 0,
        },
    })?;
    let supervisor = supervisor_service.handle();
    let catalog = Arc::new(RwLock::new(scan_installed(&roots, &ScanLimits::default())));
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

fn handle_client(
    mut stream: UnixStream,
    catalog: &Arc<RwLock<Catalog>>,
    roots: &[PathBuf],
    supervisor: &SupervisorHandle,
    allow_test_faults: bool,
) -> Result<()> {
    let cloned = stream.try_clone()?;
    let mut reader = BufReader::new(cloned).take((MAX_REQUEST_BYTES + 1) as u64);
    let mut line = Vec::new();
    reader.read_until(b'\n', &mut line)?;
    if line.len() > MAX_REQUEST_BYTES {
        bail!("request exceeded {MAX_REQUEST_BYTES} bytes");
    }
    let request: Request = serde_json::from_slice(&line).context("invalid request JSON")?;
    let (ok, result) = process_request(
        &request,
        catalog,
        roots,
        Some(supervisor),
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
                let updated = scan_installed(roots, &ScanLimits::default());
                let count = updated.stats.total;
                *catalog
                    .write()
                    .map_err(|_| anyhow!("catalog lock poisoned"))? = updated;
                json!({"catalog_items": count})
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
                        handle.pointer_input(params.generation, params.phase, params.x, params.y)
                    }),
                    Err(error) => {
                        json!({"error": "invalid_params", "detail": error.to_string()})
                    }
                }
            }
            "renderer.start" | "renderer.retry" => {
                let parsed = serde_json::from_value::<RendererStartParams>(request.params.clone());
                match parsed {
                    Ok(params) if params.test_fault.is_some() && !allow_test_faults => json!({
                        "error": "test_faults_disabled",
                        "detail": "restart the daemon with --allow-test-faults for synthetic testing"
                    }),
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
    test_fault: Option<TestFaultParams>,
    scene_path: Option<std::path::PathBuf>,
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
        let spec = Self {
            wallpaper_id: params.wallpaper_id,
            content_hash: params.content_hash,
            width: params.width,
            height: params.height,
            fps: params.fps,
            test_fault,
            scene_path: params.scene_path,
        };
        spec.validate()?;
        Ok(spec)
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

const fn default_width() -> u32 {
    960
}

const fn default_height() -> u32 {
    540
}

const fn default_fps() -> u32 {
    30
}

fn default_socket_path() -> Result<PathBuf> {
    let runtime =
        std::env::var_os("XDG_RUNTIME_DIR").context("XDG_RUNTIME_DIR is not set; pass --socket")?;
    Ok(PathBuf::from(runtime).join("kwe/daemon-v1.sock"))
}

fn default_renderer_path() -> Result<PathBuf> {
    let executable = std::env::current_exe().context("resolve daemon executable")?;
    let directory = executable
        .parent()
        .context("daemon executable has no parent")?;
    Ok(directory.join("kwe-test-renderer"))
}

fn default_state_dir() -> Result<PathBuf> {
    if let Some(path) = std::env::var_os("XDG_STATE_HOME") {
        return Ok(PathBuf::from(path).join("kwe"));
    }
    let home = std::env::var_os("HOME").context("HOME is not set; pass --state-dir")?;
    Ok(PathBuf::from(home).join(".local/state/kwe"))
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

    #[test]
    fn health_round_trip_preserves_request_id() {
        let catalog = Arc::new(RwLock::new(scan_installed(&[], &ScanLimits::default())));
        let request: Request =
            serde_json::from_str(r#"{"version":1,"id":"test-7","method":"health"}"#).unwrap();
        let (ok, result) = process_request(&request, &catalog, &[], None, false).unwrap();
        assert_eq!(request.id, "test-7");
        assert!(ok);
        assert_eq!(result["status"], "ready");
    }

    #[test]
    fn rejects_unsupported_protocol_version() {
        let catalog = Arc::new(RwLock::new(scan_installed(&[], &ScanLimits::default())));
        let request: Request =
            serde_json::from_str(r#"{"version":99,"id":1,"method":"health"}"#).unwrap();
        let (ok, result) = process_request(&request, &catalog, &[], None, false).unwrap();
        assert!(!ok);
        assert_eq!(result["error"], "unsupported_api_version");
    }

    #[test]
    fn rejects_test_faults_unless_explicitly_enabled() {
        let catalog = Arc::new(RwLock::new(scan_installed(&[], &ScanLimits::default())));
        let request: Request = serde_json::from_str(
            r#"{"version":1,"id":3,"method":"renderer.start","params":{"wallpaper_id":"synthetic","content_hash":"abc","test_fault":{"kind":"hang","after":2}}}"#,
        )
        .unwrap();
        let (ok, result) = process_request(&request, &catalog, &[], None, false).unwrap();
        assert!(!ok);
        assert_eq!(result["error"], "test_faults_disabled");
    }
}
