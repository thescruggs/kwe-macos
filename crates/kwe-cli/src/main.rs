// SPDX-License-Identifier: Apache-2.0
use std::{
    fs,
    io::{BufRead, BufReader, Read, Write},
    os::unix::net::UnixStream,
    path::PathBuf,
    time::Duration,
};

use anyhow::{Context, Result};
use clap::{Parser, Subcommand};
use kwe_core::{
    ScanLimits, chromium_command, default_steam_roots, preflight_scene, preflight_web, probe_mpris,
    probe_pipewire, sandbox_root, scan_installed,
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
    /// Statically validate a scene entry without launching a renderer.
    Preflight {
        #[arg(long)]
        path: PathBuf,
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
    /// Print the sandboxed Chromium command for a web wallpaper.
    WebSandbox {
        #[arg(long)]
        path: PathBuf,
        #[arg(long)]
        allow_network: bool,
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
            println!("Run `kwe-vulkan --json` for renderer capability details.");
        }
        Command::Preflight { path } => {
            let report = preflight_scene(&path);
            println!("{}", serde_json::to_string_pretty(&report)?);
            if !report.safe {
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
        } => {
            let root = sandbox_root(&path).context("path must contain a regular index.html")?;
            let command = chromium_command(&root, allow_network);
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
            stream.set_read_timeout(Some(Duration::from_secs(5)))?;
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
