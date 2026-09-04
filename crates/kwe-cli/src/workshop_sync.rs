// SPDX-License-Identifier: GPL-3.0-or-later
//! `kwe workshop-sync`: download Wallpaper Engine Workshop items with
//! SteamCMD into a Steam-library-shaped root the scanner indexes
//! (docs/macos/CONTENT.md). Built for macOS, where Steam cannot install
//! the Windows-only app and therefore never syncs its Workshop items; it
//! runs anywhere SteamCMD does.
//!
//! Subscription sources, in the order they are combined:
//! - Steam's own `steamapps/appworkshop_431960.acf` manifest (lists every
//!   subscribed item) from one or more Steam roots — the local library on
//!   Linux, or a copy of that file from the machine that has Wallpaper
//!   Engine installed (`--manifest-root`);
//! - the Steam Web API with the user's own key (`--api-key` + `--steamid`,
//!   `ISteamRemoteStorage/EnumerateUserSubscribedFiles`), which Valve may
//!   restrict to publisher keys — reported plainly when it refuses;
//! - public Workshop collections (`--collection`) and explicit item ids or
//!   URLs (`--item`), resolved and filtered to app 431960 through the
//!   key-less `GetCollectionDetails` / `GetPublishedFileDetails` calls.
//!
//! Credentials never pass through kwe: SteamCMD is run with
//! `@NoPromptForPassword 1` against its own cached session, so a missing
//! or expired login fails fast with an instruction to run the one
//! interactive `steamcmd +login <user>` yourself. Every subprocess has a
//! deadline and a bounded output buffer; every id is validated before it
//! reaches a command line.

use std::collections::BTreeSet;
use std::io::Read;
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::time::{Duration, Instant};

use anyhow::{Context, Result, anyhow, bail};
use serde_json::{Value, json};

pub const WALLPAPER_ENGINE_APP_ID: &str = "431960";
const MAX_IDS: usize = 5000;
const IDS_PER_STEAMCMD_RUN: usize = 25;
const STEAMCMD_RUN_BUDGET: Duration = Duration::from_secs(20 * 60);
const APP_UPDATE_BUDGET: Duration = Duration::from_secs(60 * 60);
const MAX_OUTPUT_BYTES: usize = 8 * 1024 * 1024;
const CURL_BUDGET_SECONDS: u32 = 30;
const MAX_HTTP_BYTES: usize = 4 * 1024 * 1024;
const WEB_API: &str = "https://api.steampowered.com";

#[derive(Debug, Clone)]
pub struct SyncOptions {
    pub steam_user: String,
    pub steamcmd: PathBuf,
    pub root: PathBuf,
    pub manifest_roots: Vec<PathBuf>,
    pub collections: Vec<String>,
    pub items: Vec<String>,
    pub api_key: Option<String>,
    pub steamid: Option<String>,
    pub assets: bool,
    pub dry_run: bool,
    pub rescan_socket: Option<PathBuf>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ItemOutcome {
    Downloaded { path: String },
    Failed { reason: String },
    Skipped { reason: String },
}

#[derive(Debug, Default)]
pub struct SyncReport {
    pub sources: Vec<String>,
    pub items: Vec<(String, ItemOutcome)>,
    pub assets: Option<Result<String, String>>,
    pub login_failure: Option<String>,
    pub rescan: Option<String>,
}

impl SyncReport {
    pub fn downloaded(&self) -> usize {
        self.items
            .iter()
            .filter(|(_, outcome)| matches!(outcome, ItemOutcome::Downloaded { .. }))
            .count()
    }
    pub fn failed(&self) -> usize {
        self.items
            .iter()
            .filter(|(_, outcome)| matches!(outcome, ItemOutcome::Failed { .. }))
            .count()
    }
    pub fn to_json(&self) -> Value {
        json!({
            "sources": self.sources,
            "downloaded": self.downloaded(),
            "failed": self.failed(),
            "login_failure": self.login_failure,
            "assets": self.assets.as_ref().map(|result| match result {
                Ok(path) => json!({"ok": true, "path": path}),
                Err(reason) => json!({"ok": false, "reason": reason}),
            }),
            "rescan": self.rescan,
            "items": self.items.iter().map(|(id, outcome)| match outcome {
                ItemOutcome::Downloaded { path } => json!({"id": id, "state": "downloaded", "path": path}),
                ItemOutcome::Failed { reason } => json!({"id": id, "state": "failed", "reason": reason}),
                ItemOutcome::Skipped { reason } => json!({"id": id, "state": "skipped", "reason": reason}),
            }).collect::<Vec<_>>(),
        })
    }
}

// ---------------------------------------------------------------------------
// Validation
// ---------------------------------------------------------------------------

/// A Workshop id is 1..=20 ASCII digits, not zero. URLs of the form
/// `https://steamcommunity.com/sharedfiles/filedetails/?id=123` (any
/// query order) are accepted and reduced to the id.
pub fn parse_item_id(text: &str) -> Option<String> {
    let trimmed = text.trim();
    let candidate = if trimmed.contains("://") || trimmed.contains('?') {
        trimmed
            .split(['?', '&'])
            .find_map(|part| part.strip_prefix("id="))?
            .to_string()
    } else {
        trimmed.to_string()
    };
    let valid = !candidate.is_empty()
        && candidate.len() <= 20
        && candidate.bytes().all(|b| b.is_ascii_digit())
        && candidate.bytes().any(|b| b != b'0');
    valid.then_some(candidate)
}

/// Steam account names: letters, digits, `_`, `.`, `-`, 1..=64 chars —
/// the only thing that ever reaches a SteamCMD command line besides ids.
pub fn validate_steam_user(user: &str) -> Result<()> {
    let ok = !user.is_empty()
        && user.len() <= 64
        && user
            .bytes()
            .all(|b| b.is_ascii_alphanumeric() || matches!(b, b'_' | b'.' | b'-'));
    if !ok {
        bail!("--user must be 1..=64 letters, digits, '_', '.', or '-' (the Steam account name, not the profile name)");
    }
    Ok(())
}

fn validate_steamid(steamid: &str) -> Result<()> {
    if steamid.len() != 17 || !steamid.bytes().all(|b| b.is_ascii_digit()) {
        bail!("--steamid must be the 17-digit SteamID64");
    }
    Ok(())
}

// ---------------------------------------------------------------------------
// Steam's Workshop manifest
// ---------------------------------------------------------------------------

const MAX_MANIFEST_BYTES: u64 = 16 * 1024 * 1024;

/// Candidate manifest paths under a Steam library root. Steam writes
/// `steamapps/workshop/appworkshop_431960.acf` (verified on a live Linux
/// library); the upstream scanner's `steamapps/appworkshop_431960.acf`
/// is kept as a fallback for copies placed there.
pub fn manifest_candidates(root: &Path) -> [PathBuf; 2] {
    let name = format!("appworkshop_{WALLPAPER_ENGINE_APP_ID}.acf");
    [
        root.join("steamapps/workshop").join(&name),
        root.join("steamapps").join(&name),
    ]
}

/// The subscribed item ids a Steam library's manifest records: the union
/// of `WorkshopItemDetails` (every subscription, downloaded or not),
/// `WorkshopItemsInstalled`, and the legacy `WorkshopItems` section.
/// `Ok(None)` when the library has no manifest.
pub fn manifest_subscriptions(root: &Path) -> Result<Option<BTreeSet<String>>> {
    let Some(path) = manifest_candidates(root).into_iter().find(|p| p.is_file()) else {
        return Ok(None);
    };
    let metadata = std::fs::metadata(&path)?;
    if metadata.len() > MAX_MANIFEST_BYTES {
        bail!("{} is larger than {MAX_MANIFEST_BYTES} bytes", path.display());
    }
    let text = std::fs::read_to_string(&path)
        .with_context(|| format!("read {}", path.display()))?;
    let tree = kwe_core::parse_key_values(&text)
        .map_err(|error| anyhow!("{}: {error}", path.display()))?;
    let app = tree
        .get_case_insensitive("appworkshop")
        .and_then(kwe_core::KvValue::object)
        .with_context(|| format!("{}: no AppWorkshop section", path.display()))?;
    let mut ids = BTreeSet::new();
    for (key, value) in app {
        let section = key.to_ascii_lowercase();
        if !matches!(
            section.as_str(),
            "workshopitemdetails" | "workshopitemsinstalled" | "workshopitems"
        ) {
            continue;
        }
        for id in value.object().into_iter().flatten().map(|(id, _)| id) {
            if let Some(id) = parse_item_id(id) {
                ids.insert(id);
            }
        }
    }
    Ok(Some(ids))
}

// ---------------------------------------------------------------------------
// Steam Web API (key-less unless noted) through curl, bounded
// ---------------------------------------------------------------------------

fn http_post_form(url: &str, fields: &[(String, String)]) -> Result<String> {
    let mut command = Command::new("curl");
    command
        .arg("-sS")
        .arg("--fail-with-body")
        .arg("--max-time")
        .arg(CURL_BUDGET_SECONDS.to_string())
        .arg("--max-filesize")
        .arg(MAX_HTTP_BYTES.to_string());
    for (name, value) in fields {
        command.arg("--data-urlencode").arg(format!("{name}={value}"));
    }
    command
        .arg(url)
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped());
    let output = command
        .output()
        .with_context(|| format!("run curl for {url} (is curl installed?)"))?;
    let body = String::from_utf8_lossy(&output.stdout).into_owned();
    if !output.status.success() {
        let error = String::from_utf8_lossy(&output.stderr);
        bail!(
            "{url}: {} {}",
            output.status,
            error.trim().chars().take(200).collect::<String>()
        );
    }
    if body.len() > MAX_HTTP_BYTES {
        bail!("{url}: reply larger than {MAX_HTTP_BYTES} bytes");
    }
    Ok(body)
}

/// Parses `GetCollectionDetails` into the collection's child item ids.
pub fn parse_collection_children(body: &str) -> Result<Vec<String>> {
    let value: Value = serde_json::from_str(body).context("collection reply is not JSON")?;
    let details = value["response"]["collectiondetails"]
        .as_array()
        .context("collection reply has no collectiondetails")?;
    let mut ids = Vec::new();
    for detail in details {
        if detail["result"].as_i64() != Some(1) {
            bail!(
                "collection {} is not accessible (result {})",
                detail["publishedfileid"].as_str().unwrap_or("?"),
                detail["result"]
            );
        }
        for child in detail["children"].as_array().into_iter().flatten() {
            if let Some(id) = child["publishedfileid"].as_str().and_then(parse_item_id) {
                ids.push(id);
            }
        }
    }
    Ok(ids)
}

/// Parses `GetPublishedFileDetails`: ids that belong to Wallpaper Engine,
/// and the ids Steam knows but which belong to another app.
pub fn parse_published_details(body: &str) -> Result<(Vec<(String, String)>, Vec<String>)> {
    let value: Value = serde_json::from_str(body).context("details reply is not JSON")?;
    let details = value["response"]["publishedfiledetails"]
        .as_array()
        .context("details reply has no publishedfiledetails")?;
    let mut ours = Vec::new();
    let mut foreign = Vec::new();
    for detail in details {
        let Some(id) = detail["publishedfileid"].as_str().and_then(parse_item_id) else {
            continue;
        };
        if detail["result"].as_i64() != Some(1) {
            foreign.push(id);
            continue;
        }
        let app = detail["consumer_app_id"]
            .as_u64()
            .map(|n| n.to_string())
            .or_else(|| detail["consumer_app_id"].as_str().map(str::to_string));
        if app.as_deref() == Some(WALLPAPER_ENGINE_APP_ID) {
            let title = detail["title"].as_str().unwrap_or("").chars().take(120).collect();
            ours.push((id, title));
        } else {
            foreign.push(id);
        }
    }
    Ok((ours, foreign))
}

/// Parses `EnumerateUserSubscribedFiles` (`response.files[].publishedfileid`).
pub fn parse_subscribed_files(body: &str) -> Result<(Vec<String>, u64)> {
    let value: Value = serde_json::from_str(body).context("subscriptions reply is not JSON")?;
    let response = &value["response"];
    let total = response["total"].as_u64().unwrap_or(0);
    let ids = response["files"]
        .as_array()
        .into_iter()
        .flatten()
        .filter_map(|file| {
            file["publishedfileid"]
                .as_str()
                .map(str::to_string)
                .or_else(|| file["publishedfileid"].as_u64().map(|n| n.to_string()))
        })
        .filter_map(|id| parse_item_id(&id))
        .collect();
    Ok((ids, total))
}

fn fetch_collection(collection: &str) -> Result<Vec<String>> {
    let id = parse_item_id(collection)
        .ok_or_else(|| anyhow!("--collection {collection:?} is not a Workshop id or URL"))?;
    let body = http_post_form(
        &format!("{WEB_API}/ISteamRemoteStorage/GetCollectionDetails/v1/?format=json"),
        &[
            ("collectioncount".into(), "1".into()),
            ("publishedfileids[0]".into(), id),
        ],
    )?;
    parse_collection_children(&body)
}

fn fetch_details(ids: &[String]) -> Result<(Vec<(String, String)>, Vec<String>)> {
    let mut fields = vec![("itemcount".to_string(), ids.len().to_string())];
    for (index, id) in ids.iter().enumerate() {
        fields.push((format!("publishedfileids[{index}]"), id.clone()));
    }
    let body = http_post_form(
        &format!("{WEB_API}/ISteamRemoteStorage/GetPublishedFileDetails/v1/?format=json"),
        &fields,
    )?;
    parse_published_details(&body)
}

fn fetch_subscriptions(api_key: &str, steamid: &str) -> Result<Vec<String>> {
    validate_steamid(steamid)?;
    let mut ids = Vec::new();
    let mut page = 1_u32;
    loop {
        let body = http_post_form(
            &format!("{WEB_API}/ISteamRemoteStorage/EnumerateUserSubscribedFiles/v1/?format=json"),
            &[
                ("key".into(), api_key.to_string()),
                ("steamid".into(), steamid.to_string()),
                ("appid".into(), WALLPAPER_ENGINE_APP_ID.into()),
                ("page".into(), page.to_string()),
            ],
        )
        .map_err(|error| {
            anyhow!(
                "{error}\nValve may restrict EnumerateUserSubscribedFiles to publisher keys; \
                 use --manifest-root (Steam's appworkshop_431960.acf from a machine that has \
                 Wallpaper Engine) or --collection instead"
            )
        })?;
        let (batch, total) = parse_subscribed_files(&body)?;
        if batch.is_empty() {
            break;
        }
        ids.extend(batch);
        if ids.len() as u64 >= total || page >= 200 {
            break;
        }
        page += 1;
    }
    Ok(ids)
}

// ---------------------------------------------------------------------------
// SteamCMD
// ---------------------------------------------------------------------------

/// The SteamCMD argv for one batch of item downloads. `force_install_dir`
/// precedes `login`, as SteamCMD requires; `@NoPromptForPassword 1` makes
/// a missing cached session fail instead of blocking on stdin.
pub fn steamcmd_download_args(root: &Path, user: &str, ids: &[String]) -> Vec<String> {
    let mut args = vec![
        "+@ShutdownOnFailedCommand".to_string(),
        "0".to_string(),
        "+@NoPromptForPassword".to_string(),
        "1".to_string(),
        "+force_install_dir".to_string(),
        root.to_string_lossy().into_owned(),
        "+login".to_string(),
        user.to_string(),
    ];
    for id in ids {
        args.push("+workshop_download_item".to_string());
        args.push(WALLPAPER_ENGINE_APP_ID.to_string());
        args.push(id.clone());
    }
    args.push("+quit".to_string());
    args
}

/// The SteamCMD argv that installs the Windows build of the app itself
/// (only its `assets/` folder is used, by scene wallpapers).
pub fn steamcmd_assets_args(install_dir: &Path, user: &str) -> Vec<String> {
    vec![
        "+@sSteamCmdForcePlatformType".into(),
        "windows".into(),
        "+@ShutdownOnFailedCommand".into(),
        "1".into(),
        "+@NoPromptForPassword".into(),
        "1".into(),
        "+force_install_dir".into(),
        install_dir.to_string_lossy().into_owned(),
        "+login".into(),
        user.into(),
        "+app_update".into(),
        WALLPAPER_ENGINE_APP_ID.into(),
        "validate".into(),
        "+quit".into(),
    ]
}

#[derive(Debug, Default, PartialEq, Eq)]
pub struct SteamcmdOutcome {
    pub login_failure: Option<String>,
    pub downloaded: Vec<(String, String)>,
    pub failed: Vec<(String, String)>,
    pub app_installed: bool,
    pub app_error: Option<String>,
}

/// Parses SteamCMD's console output. Lines seen in the wild:
/// `Success. Downloaded item 123 to "/path" (456 bytes)`,
/// `ERROR! Download item 123 failed (Failure).`,
/// `FAILED (Invalid Password)` / `FAILED (Cached credentials not found)`
/// after `Logging in user ...`, `Success! App '431960' fully installed.`,
/// `Error! App '431960' state is 0x202 after update job.`
pub fn parse_steamcmd_output(output: &str) -> SteamcmdOutcome {
    let mut outcome = SteamcmdOutcome::default();
    for raw in output.lines() {
        let line = raw.trim();
        if let Some(rest) = line.strip_prefix("Success. Downloaded item ") {
            let mut parts = rest.splitn(2, " to ");
            let id = parts.next().unwrap_or("").trim().to_string();
            let path = parts
                .next()
                .unwrap_or("")
                .trim()
                .trim_start_matches('"')
                .split('"')
                .next()
                .unwrap_or("")
                .to_string();
            if parse_item_id(&id).is_some() {
                outcome.downloaded.push((id, path));
            }
        } else if let Some(rest) = line.strip_prefix("ERROR! Download item ") {
            let mut parts = rest.splitn(2, " failed");
            let id = parts.next().unwrap_or("").trim().to_string();
            let reason = parts
                .next()
                .unwrap_or("")
                .trim()
                .trim_matches(|c| c == '(' || c == ')' || c == '.' || c == ' ')
                .to_string();
            if parse_item_id(&id).is_some() {
                outcome.failed.push((id, if reason.is_empty() { "failure".into() } else { reason }));
            }
        } else if line.starts_with("FAILED (") || line.contains("Login Failure") || line.starts_with("FAILED login") {
            outcome.login_failure = Some(line.chars().take(160).collect());
        } else if line.contains("fully installed") {
            outcome.app_installed = true;
        } else if line.starts_with("Error! App") || line.starts_with("ERROR! App") {
            outcome.app_error = Some(line.chars().take(200).collect());
        }
    }
    outcome
}

fn run_steamcmd(steamcmd: &Path, args: &[String], budget: Duration) -> Result<String> {
    let mut child = Command::new(steamcmd)
        .args(args)
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .with_context(|| {
            format!(
                "launch {} (install SteamCMD: macOS `brew install --cask steamcmd`, or pass --steamcmd)",
                steamcmd.display()
            )
        })?;
    let stdout = child.stdout.take().context("steamcmd stdout")?;
    let stderr = child.stderr.take().context("steamcmd stderr")?;
    let reader = std::thread::spawn(move || {
        let mut buffer = Vec::new();
        let _ = stdout.take(MAX_OUTPUT_BYTES as u64).read_to_end(&mut buffer);
        buffer
    });
    let error_reader = std::thread::spawn(move || {
        let mut buffer = Vec::new();
        let _ = stderr.take(64 * 1024).read_to_end(&mut buffer);
        buffer
    });
    let deadline = Instant::now() + budget;
    loop {
        if child.try_wait()?.is_some() {
            break;
        }
        if Instant::now() >= deadline {
            let _ = child.kill();
            let _ = child.wait();
            bail!("steamcmd exceeded its {}s budget and was killed", budget.as_secs());
        }
        std::thread::sleep(Duration::from_millis(200));
    }
    let output = reader.join().unwrap_or_default();
    let errors = error_reader.join().unwrap_or_default();
    let mut text = String::from_utf8_lossy(&output).into_owned();
    if !errors.is_empty() {
        text.push('\n');
        text.push_str(&String::from_utf8_lossy(&errors));
    }
    Ok(text)
}

// ---------------------------------------------------------------------------
// Sync
// ---------------------------------------------------------------------------

pub fn run(options: &SyncOptions) -> Result<SyncReport> {
    validate_steam_user(&options.steam_user)?;
    let mut report = SyncReport::default();
    let mut wanted: BTreeSet<String> = BTreeSet::new();

    for root in &options.manifest_roots {
        match manifest_subscriptions(root)? {
            None => report.sources.push(format!(
                "manifest {}: no steamapps/workshop/appworkshop_{WALLPAPER_ENGINE_APP_ID}.acf",
                root.display()
            )),
            Some(subscriptions) => {
                report.sources.push(format!(
                    "manifest {}: {} subscribed items",
                    root.display(),
                    subscriptions.len()
                ));
                wanted.extend(subscriptions);
            }
        }
    }
    if let (Some(key), Some(steamid)) = (&options.api_key, &options.steamid) {
        let ids = fetch_subscriptions(key, steamid)?;
        report
            .sources
            .push(format!("web api subscriptions for {steamid}: {} items", ids.len()));
        wanted.extend(ids);
    }
    for collection in &options.collections {
        let ids = fetch_collection(collection)?;
        report
            .sources
            .push(format!("collection {collection}: {} items", ids.len()));
        wanted.extend(ids);
    }
    for item in &options.items {
        let id = parse_item_id(item)
            .ok_or_else(|| anyhow!("--item {item:?} is not a Workshop id or URL"))?;
        wanted.insert(id);
    }
    if wanted.is_empty() && !options.assets {
        bail!(
            "nothing to sync: give --manifest-root <steam root with appworkshop_431960.acf>, \
             --collection <id|url>, --item <id|url>, or --api-key/--steamid (see docs/macos/CONTENT.md)"
        );
    }
    if wanted.len() > MAX_IDS {
        bail!("{} items requested; the bound is {MAX_IDS}", wanted.len());
    }

    // Validate against Steam: drop ids that are not Wallpaper Engine
    // items (a collection may mix apps; a manifest copy may be stale).
    let mut ids: Vec<String> = wanted.into_iter().collect();
    if !ids.is_empty() && !options.dry_run {
        let mut kept = Vec::new();
        for chunk in ids.chunks(100) {
            match fetch_details(chunk) {
                Ok((ours, foreign)) => {
                    for id in foreign {
                        report.items.push((
                            id,
                            ItemOutcome::Skipped {
                                reason: "not a Wallpaper Engine Workshop item (or not public)".into(),
                            },
                        ));
                    }
                    kept.extend(ours.into_iter().map(|(id, _)| id));
                }
                Err(error) => {
                    // Offline or blocked: download everything requested.
                    report
                        .sources
                        .push(format!("item validation skipped: {error}"));
                    kept.extend(chunk.iter().cloned());
                }
            }
        }
        ids = kept;
    }

    std::fs::create_dir_all(&options.root)
        .with_context(|| format!("create sync root {}", options.root.display()))?;
    let root = std::fs::canonicalize(&options.root).unwrap_or(options.root.clone());

    if options.dry_run {
        for id in &ids {
            report.items.push((
                id.clone(),
                ItemOutcome::Skipped {
                    reason: "dry run".into(),
                },
            ));
        }
        return Ok(report);
    }

    for chunk in ids.chunks(IDS_PER_STEAMCMD_RUN) {
        let args = steamcmd_download_args(&root, &options.steam_user, chunk);
        let output = run_steamcmd(&options.steamcmd, &args, STEAMCMD_RUN_BUDGET)?;
        let outcome = parse_steamcmd_output(&output);
        if let Some(failure) = outcome.login_failure {
            report.login_failure = Some(failure);
            for id in chunk {
                report.items.push((
                    id.clone(),
                    ItemOutcome::Failed {
                        reason: "steamcmd login failed".into(),
                    },
                ));
            }
            break;
        }
        for id in chunk {
            if let Some((_, path)) = outcome.downloaded.iter().find(|(done, _)| done == id) {
                report.items.push((id.clone(), ItemOutcome::Downloaded { path: path.clone() }));
            } else if let Some((_, reason)) = outcome.failed.iter().find(|(failed, _)| failed == id) {
                report.items.push((id.clone(), ItemOutcome::Failed { reason: reason.clone() }));
            } else {
                report.items.push((
                    id.clone(),
                    ItemOutcome::Failed {
                        reason: "steamcmd reported neither success nor failure".into(),
                    },
                ));
            }
        }
    }

    if options.assets && report.login_failure.is_none() {
        let install_dir = root.join("steamapps/common/wallpaper_engine");
        std::fs::create_dir_all(&install_dir)?;
        let args = steamcmd_assets_args(&install_dir, &options.steam_user);
        let output = run_steamcmd(&options.steamcmd, &args, APP_UPDATE_BUDGET)?;
        let outcome = parse_steamcmd_output(&output);
        report.assets = Some(if let Some(failure) = outcome.login_failure {
            report.login_failure = Some(failure.clone());
            Err(failure)
        } else if outcome.app_installed || install_dir.join("assets").is_dir() {
            Ok(install_dir.join("assets").to_string_lossy().into_owned())
        } else {
            Err(outcome
                .app_error
                .unwrap_or_else(|| "steamcmd did not report the app as installed".into()))
        });
    }

    if let Some(socket) = &options.rescan_socket
        && report.downloaded() > 0
    {
        report.rescan = Some(match crate::call_daemon(socket, "rescan", json!({})) {
            Ok(response) if response.get("ok") == Some(&Value::Bool(true)) => {
                "daemon rescanned".to_string()
            }
            Ok(response) => format!("daemon rescan refused: {response}"),
            Err(error) => format!("daemon not reached ({error}); it rescans on its own schedule"),
        });
    }
    Ok(report)
}

pub fn print_human(report: &SyncReport, root: &Path) {
    for source in &report.sources {
        println!("source: {source}");
    }
    for (id, outcome) in &report.items {
        match outcome {
            ItemOutcome::Downloaded { path } => println!("  {id}: downloaded -> {path}"),
            ItemOutcome::Failed { reason } => println!("  {id}: FAILED ({reason})"),
            ItemOutcome::Skipped { reason } => println!("  {id}: skipped ({reason})"),
        }
    }
    if let Some(assets) = &report.assets {
        match assets {
            Ok(path) => println!("assets: installed -> {path}"),
            Err(reason) => println!("assets: FAILED ({reason})"),
        }
    }
    if let Some(failure) = &report.login_failure {
        println!(
            "steamcmd login failed: {failure}\n  run once interactively to cache the session: steamcmd +login <user>  (then +quit)"
        );
    }
    if let Some(rescan) = &report.rescan {
        println!("{rescan}");
    }
    println!(
        "sync root: {} ({} downloaded, {} failed)",
        root.display(),
        report.downloaded(),
        report.failed()
    );
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::os::unix::fs::PermissionsExt;

    #[test]
    fn item_ids_and_urls_parse_and_junk_is_refused() {
        assert_eq!(parse_item_id("123456"), Some("123456".into()));
        assert_eq!(
            parse_item_id("https://steamcommunity.com/sharedfiles/filedetails/?id=2839491463&searchtext=x"),
            Some("2839491463".into())
        );
        assert_eq!(
            parse_item_id("steamcommunity.com/workshop/filedetails/?l=en&id=77"),
            Some("77".into())
        );
        for junk in ["", "0", "12a", "-1", "123456789012345678901", "?id=", "id=x"] {
            assert_eq!(parse_item_id(junk), None, "{junk}");
        }
        assert!(validate_steam_user("my_user.01").is_ok());
        assert!(validate_steam_user("bad user").is_err());
        assert!(validate_steam_user("+login").is_err());
    }

    #[test]
    fn manifest_reader_unions_details_and_installed_from_the_real_location() {
        let dir = std::env::temp_dir().join(format!("kwe-manifest-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(dir.join("steamapps/workshop")).unwrap();
        assert_eq!(manifest_subscriptions(&dir).unwrap(), None);
        std::fs::write(
            dir.join("steamapps/workshop/appworkshop_431960.acf"),
            "\"AppWorkshop\"\n{\n\t\"appid\"\t\t\"431960\"\n\t\"WorkshopItemsInstalled\"\n\t{\n\t\t\"779003202\"\n\t\t{\n\t\t\t\"size\"\t\t\"198678\"\n\t\t}\n\t}\n\t\"WorkshopItemDetails\"\n\t{\n\t\t\"779003202\"\n\t\t{\n\t\t\t\"manifest\"\t\t\"1\"\n\t\t}\n\t\t\"3000000000\"\n\t\t{\n\t\t\t\"manifest\"\t\t\"2\"\n\t\t}\n\t}\n}\n",
        )
        .unwrap();
        let ids = manifest_subscriptions(&dir).unwrap().unwrap();
        assert_eq!(ids.into_iter().collect::<Vec<_>>(), vec!["3000000000", "779003202"]);
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn collection_and_details_replies_parse() {
        let collection = r#"{"response":{"result":1,"resultcount":1,"collectiondetails":[{"publishedfileid":"5","result":1,"children":[{"publishedfileid":"100","sortorder":0,"filetype":0},{"publishedfileid":"200","sortorder":1,"filetype":0}]}]}}"#;
        assert_eq!(parse_collection_children(collection).unwrap(), vec!["100", "200"]);
        let private = r#"{"response":{"collectiondetails":[{"publishedfileid":"5","result":9}]}}"#;
        assert!(parse_collection_children(private).is_err());
        let details = r#"{"response":{"result":1,"resultcount":3,"publishedfiledetails":[
            {"publishedfileid":"100","result":1,"consumer_app_id":431960,"title":"Aurora"},
            {"publishedfileid":"200","result":1,"consumer_app_id":440,"title":"hat"},
            {"publishedfileid":"300","result":9}]}}"#;
        let (ours, foreign) = parse_published_details(details).unwrap();
        assert_eq!(ours, vec![("100".to_string(), "Aurora".to_string())]);
        assert_eq!(foreign, vec!["200", "300"]);
        let subs = r#"{"response":{"total":2,"startindex":0,"files":[{"publishedfileid":"7"},{"publishedfileid":8}]}}"#;
        assert_eq!(parse_subscribed_files(subs).unwrap(), (vec!["7".to_string(), "8".to_string()], 2));
    }

    #[test]
    fn steamcmd_output_parses_success_failure_and_login() {
        let text = concat!(
            "Redirecting stderr to '/x/logs/stderr.txt'\n",
            "Logging in user 'me' to Steam Public...OK\n",
            "Waiting for client config...OK\n",
            "Downloading item 100 ...\n",
            "Success. Downloaded item 100 to \"/root/steamapps/workshop/content/431960/100\" (1234 bytes) \n",
            "ERROR! Download item 200 failed (Failure).\n",
            "Success! App '431960' fully installed.\n",
        );
        let outcome = parse_steamcmd_output(text);
        assert_eq!(outcome.downloaded, vec![("100".to_string(), "/root/steamapps/workshop/content/431960/100".to_string())]);
        assert_eq!(outcome.failed, vec![("200".to_string(), "Failure".to_string())]);
        assert!(outcome.app_installed);
        assert!(outcome.login_failure.is_none());
        let login = parse_steamcmd_output("Logging in user 'me' to Steam Public...\nFAILED (Cached credentials not found)\n");
        assert_eq!(login.login_failure.as_deref(), Some("FAILED (Cached credentials not found)"));
    }

    #[test]
    fn download_args_put_install_dir_before_login_and_never_prompt() {
        let args = steamcmd_download_args(Path::new("/r"), "me", &["1".into(), "2".into()]);
        assert_eq!(
            args,
            vec![
                "+@ShutdownOnFailedCommand", "0", "+@NoPromptForPassword", "1",
                "+force_install_dir", "/r", "+login", "me",
                "+workshop_download_item", "431960", "1",
                "+workshop_download_item", "431960", "2", "+quit",
            ]
        );
        let assets = steamcmd_assets_args(Path::new("/r/steamapps/common/wallpaper_engine"), "me");
        assert_eq!(assets[0], "+@sSteamCmdForcePlatformType");
        assert_eq!(assets[1], "windows");
        assert!(assets.windows(2).any(|w| w == ["+app_update", "431960"]));
    }

    /// End to end against a stub `steamcmd`: items from a manifest copy
    /// and an explicit id, one failing, downloads land under the sync root.
    #[test]
    fn sync_with_a_stub_steamcmd_reports_per_item_outcomes() {
        let dir = std::env::temp_dir().join(format!("kwe-workshop-sync-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        let manifest_root = dir.join("linux-steam");
        std::fs::create_dir_all(manifest_root.join("steamapps/workshop")).unwrap();
        std::fs::write(
            manifest_root.join("steamapps/workshop/appworkshop_431960.acf"),
            r#""AppWorkshop" { "appid" "431960"
                "WorkshopItemsInstalled" { "100" { "size" "1" "manifest" "9" } }
                "WorkshopItemDetails" { "100" { "manifest" "9" } "200" { "manifest" "8" "timeupdated" "1" } } }"#,
        )
        .unwrap();
        let stub = dir.join("steamcmd");
        std::fs::write(
            &stub,
            concat!(
                "#!/bin/sh\n",
                "root=''; prev=''\n",
                "for a in \"$@\"; do [ \"$prev\" = '+force_install_dir' ] && root=\"$a\"; prev=\"$a\"; done\n",
                "echo \"Logging in user 'me' to Steam Public...OK\"\n",
                "prev=''; app=''\n",
                "for a in \"$@\"; do\n",
                "  if [ \"$prev\" = '431960' ] && [ \"$app\" = '+workshop_download_item' ]; then\n",
                "    if [ \"$a\" = '200' ]; then echo \"ERROR! Download item 200 failed (Failure).\";\n",
                "    else mkdir -p \"$root/steamapps/workshop/content/431960/$a\"; echo \"Success. Downloaded item $a to \\\"$root/steamapps/workshop/content/431960/$a\\\" (10 bytes) \"; fi\n",
                "  fi\n",
                "  app=\"$prev\"; prev=\"$a\"\n",
                "done\n",
            ),
        )
        .unwrap();
        std::fs::set_permissions(&stub, std::fs::Permissions::from_mode(0o755)).unwrap();
        let root = dir.join("sync-root");
        let options = SyncOptions {
            steam_user: "me".into(),
            steamcmd: stub,
            root: root.clone(),
            manifest_roots: vec![manifest_root],
            collections: Vec::new(),
            items: vec!["300".into()],
            api_key: None,
            steamid: None,
            assets: false,
            dry_run: false,
            rescan_socket: None,
        };
        // Validation would call the Web API; the stub environment has no
        // network guarantee, so the dry run proves source merging and the
        // real run tolerates validation being unavailable.
        let dry = run(&SyncOptions {
            dry_run: true,
            ..options.clone()
        })
        .unwrap();
        let dry_ids: Vec<&str> = dry.items.iter().map(|(id, _)| id.as_str()).collect();
        assert_eq!(dry_ids, vec!["100", "200", "300"]);
        assert!(dry.sources[0].contains("2 subscribed items"));

        let report = run(&SyncOptions {
            // curl to an unreachable host keeps the test offline-safe.
            ..options
        });
        // Whether validation ran or not, the stub outcome per id is fixed.
        let report = report.unwrap();
        let outcome = |id: &str| {
            report
                .items
                .iter()
                .find(|(item, _)| item == id)
                .map(|(_, outcome)| outcome.clone())
        };
        if matches!(outcome("100"), Some(ItemOutcome::Downloaded { .. })) {
            assert!(root.join("steamapps/workshop/content/431960/100").is_dir());
            assert!(matches!(outcome("200"), Some(ItemOutcome::Failed { .. })));
        } else {
            // Steam answered the validation call and knows none of the
            // synthetic ids: all skipped, nothing downloaded.
            assert!(report.items.iter().all(|(_, o)| matches!(o, ItemOutcome::Skipped { .. })));
        }
        let _ = std::fs::remove_dir_all(&dir);
    }
}
