// SPDX-License-Identifier: GPL-3.0-or-later
use std::{
    fs,
    io::{BufRead, BufReader, Read, Write},
    os::unix::net::UnixStream,
    os::unix::process::CommandExt,
    path::PathBuf,
    time::Duration,
};

use anyhow::{Context, Result};
use clap::{Parser, Subcommand};
use kwe_core::{
    ScanLimits, default_steam_roots, preflight_scene, preflight_video, preflight_web, probe_mpris,
    probe_pipewire, sandbox_root, scan_installed, web_renderer_command,
};

#[derive(Debug, Parser)]
#[command(name = "kwe", version, about = "Safe KDE Wallpaper Engine alpha tools")]
struct Arguments {
    #[command(subcommand)]
    command: Command,
}

#[derive(Debug, Subcommand)]
enum Command {
    /// Discover Steam libraries and index installed Wallpaper Engine projects.
    Scan {
        /// Override Steam root discovery. May be specified more than once.
        #[arg(long = "steam-root")]
        steam_roots: Vec<PathBuf>,
        /// Write the catalog atomically to this path instead of stdout.
        #[arg(long)]
        output: Option<PathBuf>,
        #[arg(long)]
        compact: bool,
    },
    /// Print a concise, payload-free environment and library report.
    Diagnose {
        #[arg(long = "steam-root")]
        steam_roots: Vec<PathBuf>,
    },
    /// Statically validate a scene, video, or web entry without launching a
    /// renderer.
    Preflight {
        /// Scene entry to validate.
        #[arg(long)]
        path: Option<PathBuf>,
        /// Video file to validate.
        #[arg(long)]
        video: Option<PathBuf>,
        /// Web wallpaper directory to validate.
        #[arg(long)]
        web: Option<PathBuf>,
    },
    /// Statically validate a web wallpaper directory without launching a browser.
    WebPreflight {
        #[arg(long)]
        path: PathBuf,
        #[arg(long = "permission")]
        permissions: Vec<String>,
    },
    /// Probe PipeWire availability without opening an audio stream.
    AudioStatus,
    /// Print the sandboxed Chromium command for a web wallpaper. This is
    /// the M2b web-renderer bwrap command (ro-binds for /usr /etc /lib
    /// /lib64 /bin /sbin, content at /wallpaper, tmpfs profile, unshared
    /// network unless --allow-network, chromium
    /// --headless=new --remote-debugging-pipe), NOT the M2a chromium
    /// command — a renderer spawned from the M2a form would lack the
    /// system ro-binds and fail to exec inside the empty root.
    WebSandbox {
        #[arg(long)]
        path: PathBuf,
        #[arg(long)]
        allow_network: bool,
        /// Frame width in pixels (matches the worker's --width default).
        #[arg(long, default_value_t = 960, value_parser = clap::value_parser!(u32).range(1..=8192))]
        width: u32,
        /// Frame height in pixels (matches the worker's --height default).
        #[arg(long, default_value_t = 540, value_parser = clap::value_parser!(u32).range(1..=8192))]
        height: u32,
    },
    /// Probe the user MPRIS bus without controlling a player.
    MediaStatus,
    /// Call a bounded daemon API method (alpha diagnostics and smoke tests).
    DaemonCall {
        #[arg(long)]
        socket: PathBuf,
        #[arg(long)]
        method: String,
        /// JSON object supplied as the method params.
        #[arg(long, default_value = "{}")]
        params: String,
    },
}

fn main() -> Result<()> {
    let arguments = Arguments::parse();
    match arguments.command {
        Command::Scan {
            steam_roots,
            output,
            compact,
        } => {
            let roots = roots_or_default(steam_roots);
            let catalog = scan_installed(&roots, &ScanLimits::default());
            let json = if compact {
                serde_json::to_vec(&catalog)?
            } else {
                serde_json::to_vec_pretty(&catalog)?
            };
            if let Some(path) = output {
                atomic_write(&path, &json)?;
                eprintln!(
                    "indexed {} projects into {}",
                    catalog.stats.total,
                    path.display()
                );
            } else {
                println!("{}", String::from_utf8(json)?);
            }
        }
        Command::Diagnose { steam_roots } => {
            let roots = roots_or_default(steam_roots);
            let catalog = scan_installed(&roots, &ScanLimits::default());
            println!("KDE Wallpaper Engine alpha diagnostic");
            println!("catalog schema: {}", catalog.schema_version);
            println!("Steam roots checked: {}", roots.len());
            println!("Steam libraries found: {}", catalog.libraries.len());
            for library in &catalog.libraries {
                println!(
                    "- {} (app={}, workshop={})",
                    library.path.display(),
                    library.wallpaper_engine_installed,
                    library.workshop_available
                );
            }
            println!(
                "projects: {} (scene {}, video {}, web {}, unknown {}, invalid {}; subscribed {}, awaiting download {})",
                catalog.stats.total,
                catalog.stats.scene,
                catalog.stats.video,
                catalog.stats.web,
                catalog.stats.unknown,
                catalog.stats.invalid,
                catalog.stats.subscribed,
                catalog.stats.missing
            );
            println!("global diagnostics: {}", catalog.diagnostics.len());
            // Video backend lane (M1e): invoke the video renderer's bounded
            // --probe, mirroring the kwe-vulkan lane below. The probe only
            // queries the loaded libmpv's client API version — no device,
            // no media — so it works on any session.
            match probe_video_backend() {
                VideoProbeOutcome::Report(report) => print!("video backend: {report}"),
                VideoProbeOutcome::Missing => println!(
                    "video backend: kwe-video-renderer not found beside this binary; \
                     run it with --probe manually"
                ),
                VideoProbeOutcome::Failed { exit, reason } => println!(
                    "video backend: kwe-video-renderer --probe failed ({reason}, exit {})",
                    exit.map_or_else(|| "unknown".to_string(), |code| code.to_string())
                ),
                VideoProbeOutcome::Hung => println!(
                    "video backend: kwe-video-renderer --probe did not finish within 10 s; \
                     killed"
                ),
            }
            // Web backend lane (M2e): the web probe boots the real sandboxed
            // browser (bwrap + headless chromium) over a throwaway content
            // root and answers Browser.getVersion over the CDP pipe — a
            // missing sandbox runtime or a browser that cannot boot reports
            // as a failed probe instead of a blanket "not found".
            // B4: the daemon's unit caps the cgroup task count (TasksMax),
            // and chromium 151.173 needs more than the 96 the alpha shipped
            // — a probe run from the shell passes while the same browser
            // under the unit dies at bootstrap. When the unit is known, the
            // probe runs inside a transient unit with the SAME TasksMax so
            // this diagnostic fails exactly when the daemon's launch would.
            let confinement = unit_tasks_max();
            let lane = match confinement {
                Some(limit) => format!("web backend (under kwe-daemon TasksMax={limit})"),
                None => "web backend".to_string(),
            };
            match probe_web_backend(confinement) {
                WebProbeOutcome::Report(report) => print!("{lane}: {report}"),
                WebProbeOutcome::Missing => println!(
                    "{lane}: kwe-web-renderer not found beside this binary; \
                     run it with --probe manually"
                ),
                WebProbeOutcome::Failed { exit, reason } => println!(
                    "{lane}: kwe-web-renderer --probe failed ({reason}, exit {}); if the \
                     shell probe passes but this fails, the unit's TasksMax is too low \
                     (docs/bugs/APPLY_REJECTED_QUARANTINED.md)",
                    exit.map_or_else(|| "unknown".to_string(), |code| code.to_string())
                ),
                WebProbeOutcome::Hung => println!(
                    "{lane}: kwe-web-renderer --probe did not finish within 15 s; \
                     killed"
                ),
            }
            println!("Run `kwe-vulkan --json` for renderer capability details.");
        }
        Command::Preflight { path, video, web } => {
            let (json, safe) = match (path, video, web) {
                (Some(path), None, None) => {
                    let report = preflight_scene(&path);
                    (serde_json::to_string_pretty(&report)?, report.safe)
                }
                (None, Some(path), None) => {
                    let report = preflight_video(&path);
                    (serde_json::to_string_pretty(&report)?, report.safe)
                }
                (None, None, Some(path)) => {
                    let report = preflight_web(&path, &[]);
                    (serde_json::to_string_pretty(&report)?, report.safe)
                }
                _ => anyhow::bail!("preflight requires exactly one of --path, --video, or --web"),
            };
            println!("{json}");
            if !safe {
                std::process::exit(2);
            }
        }
        Command::WebPreflight { path, permissions } => {
            let report = preflight_web(&path, &permissions);
            println!("{}", serde_json::to_string_pretty(&report)?);
            if !report.safe {
                std::process::exit(2);
            }
        }
        Command::AudioStatus => println!("{}", serde_json::to_string_pretty(&probe_pipewire())?),
        Command::MediaStatus => println!("{}", serde_json::to_string_pretty(&probe_mpris())?),
        Command::WebSandbox {
            path,
            allow_network,
            width,
            height,
        } => {
            let root = sandbox_root(&path).context("path must contain a regular index.html")?;
            let command = web_renderer_command(&root, allow_network, width, height);
            println!(
                "{} {}",
                command.program,
                command
                    .arguments
                    .iter()
                    .map(|arg| shell_quote(arg))
                    .collect::<Vec<_>>()
                    .join(" ")
            );
        }
        Command::DaemonCall {
            socket,
            method,
            params,
        } => {
            let params: serde_json::Value =
                serde_json::from_str(&params).context("--params must be valid JSON")?;
            if !params.is_object() {
                anyhow::bail!("--params must be a JSON object");
            }
            let request = serde_json::json!({
                "version": 1,
                "id": "kwe-cli",
                "method": method,
                "params": params,
            });
            let mut stream = UnixStream::connect(&socket)
                .with_context(|| format!("connect to daemon {}", socket.display()))?;
            // The wallpaper.apply transaction waits (bounded) for the
            // renderer to promote before answering, so the read deadline
            // must cover the apply promotion window (BETA_M4a). The daemon
            // allows the promotion wait up to --apply-promotion-timeout-ms
            // = 60 s plus the bounded probes (enumerate x2 + switch), so
            // the CLI deadline covers the configured maximum with margin.
            stream.set_read_timeout(Some(Duration::from_secs(90)))?;
            stream.set_write_timeout(Some(Duration::from_secs(5)))?;
            serde_json::to_writer(&mut stream, &request)?;
            stream.write_all(b"\n")?;
            stream.flush()?;
            let mut reader = BufReader::new(stream).take(1024 * 1024);
            let mut response = Vec::new();
            reader.read_until(b'\n', &mut response)?;
            if response.is_empty() || response.len() >= 1024 * 1024 {
                anyhow::bail!("daemon returned an empty or oversized response");
            }
            let response: serde_json::Value = serde_json::from_slice(&response)?;
            println!("{}", serde_json::to_string_pretty(&response)?);
            if response.get("ok") != Some(&serde_json::Value::Bool(true)) {
                std::process::exit(2);
            }
        }
    }
    Ok(())
}

/// Outcome of a renderer `--probe` lane; each failure mode prints a
/// distinct diagnostic instead of a blanket "not found". Shared by the
/// video lane (M1e) and the web lane (M2e).
enum ProbeRun {
    /// The probe's JSON report (single line).
    Report(String),
    /// No renderer binary beside this one.
    Missing,
    /// Spawn failed, or the probe exited nonzero (its own bounded stderr
    /// diagnostic appears above).
    Failed { exit: Option<i32>, reason: String },
    /// The probe did not finish within the deadline and was killed.
    Hung,
}

/// Run one renderer's `--probe` (resolved beside this binary) and return
/// its JSON report. Bounded: the probe is a single backend query, and a
/// hung probe is killed after `deadline` instead of hanging the diagnostic.
fn run_renderer_probe(binary: &str, deadline: Duration) -> ProbeRun {
    run_renderer_probe_confined(binary, deadline, None)
}

/// The daemon unit's `TasksMax` as systemd reports it, when the unit is
/// known and the value is a finite number. `infinity`, a missing unit, a
/// missing `systemctl`, or a slow answer all yield None (the probe then
/// runs unconfined, exactly as before B4). Bounded to 3 s.
fn unit_tasks_max() -> Option<u64> {
    let mut child = std::process::Command::new("systemctl")
        .args([
            "--user",
            "show",
            "kwe-daemon.service",
            "-p",
            "TasksMax",
            "--value",
        ])
        .stdin(std::process::Stdio::null())
        .stdout(std::process::Stdio::piped())
        .stderr(std::process::Stdio::null())
        .spawn()
        .ok()?;
    let deadline = std::time::Instant::now() + Duration::from_secs(3);
    loop {
        match child.try_wait() {
            Ok(Some(status)) if status.success() => break,
            Ok(Some(_)) => return None,
            Ok(None) if std::time::Instant::now() >= deadline => {
                let _ = child.kill();
                let _ = child.wait();
                return None;
            }
            Ok(None) => std::thread::sleep(Duration::from_millis(20)),
            Err(_) => return None,
        }
    }
    let mut text = String::new();
    child.stdout.take()?.read_to_string(&mut text).ok()?;
    // systemd prints the raw pids.max; "infinity" (or an empty answer for an
    // unknown unit) is not a confinement worth reproducing.
    text.trim().parse::<u64>().ok().filter(|limit| *limit > 0)
}

/// `run_renderer_probe`, optionally inside a transient systemd user unit
/// carrying the given `TasksMax` (B4). Falls back to the direct probe when
/// `systemd-run` cannot be spawned, so a host without a user manager still
/// gets the unconfined answer.
fn run_renderer_probe_confined(
    binary: &str,
    deadline: Duration,
    tasks_max: Option<u64>,
) -> ProbeRun {
    let executable = match std::env::current_exe() {
        Ok(path) => path,
        Err(error) => {
            return ProbeRun::Failed {
                exit: None,
                reason: error.to_string(),
            };
        }
    };
    let directory = match executable.parent() {
        Some(path) => path,
        None => {
            return ProbeRun::Failed {
                exit: None,
                reason: "no parent directory for this binary".to_string(),
            };
        }
    };
    let probe = directory.join(binary);
    if !probe.is_file() {
        return ProbeRun::Missing;
    }
    // The probe child gets its own process group (mirror of the web
    // renderer's spawn_browser setpgid): a hung probe may have spawned
    // bwrap -> chromium beneath it, and only a negative-pid kill reaches
    // the whole tree.
    let mut command = match tasks_max {
        Some(limit) => {
            // --wait returns the service's exit status; --pipe hands the
            // report through; RuntimeMaxSec bounds the unit itself so a
            // hung probe cannot outlive this diagnostic even though the
            // process-group kill below only reaches systemd-run.
            let mut command = std::process::Command::new("systemd-run");
            command
                .args(["--user", "--quiet", "--wait", "--pipe", "--collect"])
                .arg(format!("--property=TasksMax={limit}"))
                .arg(format!(
                    "--property=RuntimeMaxSec={}",
                    deadline.as_secs().max(1)
                ))
                .arg(&probe)
                .arg("--probe");
            command
        }
        None => {
            let mut command = std::process::Command::new(&probe);
            command.arg("--probe");
            command
        }
    };
    let spawned = command
        .stdout(std::process::Stdio::piped())
        .process_group(0)
        .spawn();
    let mut child = match spawned {
        Ok(child) => child,
        Err(_) if tasks_max.is_some() => {
            // No systemd-run on this host: answer unconfined rather than
            // not at all.
            return run_renderer_probe_confined(binary, deadline, None);
        }
        Err(error) => {
            return ProbeRun::Failed {
                exit: None,
                reason: error.to_string(),
            };
        }
    };
    let deadline = std::time::Instant::now() + deadline;
    loop {
        match child.try_wait() {
            Ok(Some(status)) if status.success() => break,
            Ok(Some(status)) => {
                let _ = child.wait();
                return ProbeRun::Failed {
                    exit: status.code(),
                    reason: "probe exited nonzero".to_string(),
                };
            }
            Ok(None) if std::time::Instant::now() >= deadline => {
                let pid = child.id() as libc::pid_t;
                // SAFETY: the probe child was placed in its own process
                // group above; a negative pid restricts delivery to it,
                // reaching any bwrap/chromium descendants too.
                unsafe {
                    libc::kill(-pid, libc::SIGKILL);
                }
                let _ = child.wait();
                return ProbeRun::Hung;
            }
            Ok(None) => std::thread::sleep(Duration::from_millis(20)),
            Err(error) => {
                let _ = child.kill();
                let _ = child.wait();
                return ProbeRun::Failed {
                    exit: None,
                    reason: error.to_string(),
                };
            }
        }
    }
    // The report is a single small line; the pipe buffer is more than
    // enough, so reading after exit cannot deadlock.
    let mut stdout = String::new();
    match child.stdout.take() {
        Some(mut pipe) => match pipe.read_to_string(&mut stdout) {
            Ok(_) => ProbeRun::Report(stdout),
            Err(error) => ProbeRun::Failed {
                exit: None,
                reason: error.to_string(),
            },
        },
        None => ProbeRun::Failed {
            exit: None,
            reason: "probe stdout was not captured".to_string(),
        },
    }
}

/// Outcome of the video backend probe lane (M1e); each failure mode prints
/// a distinct diagnostic instead of a blanket "not found".
enum VideoProbeOutcome {
    Report(String),
    Missing,
    Failed { exit: Option<i32>, reason: String },
    Hung,
}

/// Run the video renderer's `--probe`: a single libmpv version query, no
/// device needed; a hung probe is killed after a 10 s deadline.
fn probe_video_backend() -> VideoProbeOutcome {
    match run_renderer_probe("kwe-video-renderer", Duration::from_secs(10)) {
        ProbeRun::Report(report) => VideoProbeOutcome::Report(report),
        ProbeRun::Missing => VideoProbeOutcome::Missing,
        ProbeRun::Failed { exit, reason } => VideoProbeOutcome::Failed { exit, reason },
        ProbeRun::Hung => VideoProbeOutcome::Hung,
    }
}

/// Outcome of the web backend probe lane (M2e); each failure mode prints a
/// distinct diagnostic instead of a blanket "not found".
enum WebProbeOutcome {
    Report(String),
    Missing,
    Failed { exit: Option<i32>, reason: String },
    Hung,
}

/// Run the web renderer's `--probe`: the probe boots the real sandboxed
/// browser and answers Browser.getVersion over the CDP pipe, so it needs
/// bwrap and chromium present; a hung probe is killed after a 15 s deadline
/// (cold browser boot measured at < 1 s in docs/BETA_M2.md §1.7, so the
/// budget is generous).
fn probe_web_backend(tasks_max: Option<u64>) -> WebProbeOutcome {
    match run_renderer_probe_confined("kwe-web-renderer", Duration::from_secs(15), tasks_max) {
        ProbeRun::Report(report) => WebProbeOutcome::Report(report),
        ProbeRun::Missing => WebProbeOutcome::Missing,
        ProbeRun::Failed { exit, reason } => WebProbeOutcome::Failed { exit, reason },
        ProbeRun::Hung => WebProbeOutcome::Hung,
    }
}

fn roots_or_default(roots: Vec<PathBuf>) -> Vec<PathBuf> {
    if roots.is_empty() {
        default_steam_roots()
    } else {
        roots
    }
}

fn atomic_write(path: &PathBuf, contents: &[u8]) -> Result<()> {
    let parent = path
        .parent()
        .filter(|p| !p.as_os_str().is_empty())
        .unwrap_or_else(|| std::path::Path::new("."));
    fs::create_dir_all(parent).with_context(|| format!("create {}", parent.display()))?;
    let temporary = path.with_extension(format!("tmp-{}", std::process::id()));
    fs::write(&temporary, contents).with_context(|| format!("write {}", temporary.display()))?;
    fs::rename(&temporary, path).with_context(|| format!("replace {}", path.display()))?;
    Ok(())
}

fn shell_quote(value: &str) -> String {
    if value
        .chars()
        .all(|character| character.is_ascii_alphanumeric() || "-_=/:.".contains(character))
    {
        return value.into();
    }
    format!("'{}'", value.replace('\'', "'\\''"))
}
