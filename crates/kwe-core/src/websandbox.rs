// SPDX-License-Identifier: GPL-3.0-or-later
use std::path::{Path, PathBuf};

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct WebSandboxCommand {
    pub program: String,
    pub arguments: Vec<String>,
    /// The page URL the browser was asked to open; the renderer matches
    /// CDP targets against it (`file:///wallpaper/index.html` inside the
    /// Linux namespace, the real content path on macOS).
    pub page_url: String,
    /// Working directory for the browser process. Linux: `None` (bwrap's
    /// `--chdir /wallpaper` handles it inside the namespace). macOS: the
    /// content root, so the browser never runs with a cwd inside a tree
    /// the Seatbelt profile denies (getcwd/realpath would fail there).
    pub working_dir: Option<PathBuf>,
    /// The OS sandbox this command runs under, for the renderer's spawn
    /// log: `bwrap`, `seatbelt`, `seatbelt:net-only`, `seatbelt:no-home`,
    /// or `none` (KWE_WEB_SANDBOX=off). Anything but the first two is a
    /// weakened configuration the renderer logs as a warning.
    pub sandbox: &'static str,
}

/// Marker the renderer looks for in a CDP target URL to recognise the
/// wallpaper page: the sandbox-side page URL minus the scheme.
pub fn page_url_marker(command: &WebSandboxCommand) -> String {
    command
        .page_url
        .strip_prefix("file://")
        .unwrap_or(&command.page_url)
        .to_string()
}

pub fn chromium_command(root: &Path, network_allowed: bool) -> WebSandboxCommand {
    let mut arguments = vec![
        "--die-with-parent".into(),
        "--new-session".into(),
        "--ro-bind".into(),
        root.display().to_string(),
        "/wallpaper".into(),
        "--proc".into(),
        "/proc".into(),
        "--dev".into(),
        "/dev".into(),
        "--tmpfs".into(),
        "/tmp".into(),
        "--chdir".into(),
        "/wallpaper".into(),
        "--".into(),
        "chromium".into(),
        "--no-first-run".into(),
        "--no-default-browser-check".into(),
        "--disable-extensions".into(),
        "--user-data-dir=/tmp/kwe-chromium".into(),
        "file:///wallpaper/index.html".into(),
    ];
    if !network_allowed {
        arguments.insert(0, "--unshare-net".into());
    }
    WebSandboxCommand {
        program: "bwrap".into(),
        arguments,
        page_url: LINUX_PAGE_URL.into(),
        working_dir: None,
        sandbox: "bwrap",
    }
}

/// Shared bwrap isolation prefix for the web sandbox (the M2b bind set):
/// `--die-with-parent --new-session`, the browser's system paths bound in
/// read-only (/usr, /etc, /lib, /lib64, /bin, /sbin — verified: chromium 151
/// launches and answers CDP through these), the content root overlaid at
/// /wallpaper, /proc and /dev, a writable /tmp tmpfs for the throwaway
/// profile, and `--unshare-net` unless the content permission set grants
/// network access (the M1a default is OFF; grants land in M2c). The `--`
/// separator ends the prefix so the wrapped program's own argv follows.
fn sandbox_prefix(root: &Path, network_allowed: bool) -> Vec<String> {
    let mut arguments = vec![
        "--die-with-parent".into(),
        "--new-session".into(),
        "--ro-bind".into(),
        "/usr".into(),
        "/usr".into(),
        "--ro-bind".into(),
        "/etc".into(),
        "/etc".into(),
        "--ro-bind".into(),
        "/lib".into(),
        "/lib".into(),
        "--ro-bind".into(),
        "/lib64".into(),
        "/lib64".into(),
        "--ro-bind".into(),
        "/bin".into(),
        "/bin".into(),
        "--ro-bind".into(),
        "/sbin".into(),
        "/sbin".into(),
        "--ro-bind".into(),
        root.display().to_string(),
        "/wallpaper".into(),
        "--proc".into(),
        "/proc".into(),
        "--dev".into(),
        "/dev".into(),
        "--tmpfs".into(),
        "/tmp".into(),
        "--chdir".into(),
        "/wallpaper".into(),
        "--".into(),
    ];
    if !network_allowed {
        arguments.insert(0, "--unshare-net".into());
    }
    arguments
}

/// Command line for the supervised web renderer (M2b): the shared
/// [`sandbox_prefix`] isolation, then chromium headless with
/// `--remote-debugging-pipe` on fds 3/4, a throwaway profile inside the
/// sandboxed tmpfs, and the screencast viewport. `--unshare-net` is dropped
/// only when the content permission set grants network access (the M1a
/// default is OFF; grants land in M2c).
///
/// M2b addition over the M2a string: bwrap's root namespace starts
/// completely empty, so the browser's own system paths are bound in
/// read-only first (/usr, /etc, /lib, /lib64, /bin, /sbin — verified:
/// chromium 151 launches and answers CDP through this command). The content
/// root overlays /wallpaper, /tmp is a writable tmpfs for the profile, and
/// nothing else on the host is reachable.
pub fn web_renderer_command(
    root: &Path,
    network_allowed: bool,
    width: u32,
    height: u32,
) -> WebSandboxCommand {
    if cfg!(target_os = "macos") {
        return macos::web_renderer_command(root, network_allowed, width, height);
    }
    linux_web_renderer_command(root, network_allowed, width, height)
}

const LINUX_PAGE_URL: &str = "file:///wallpaper/index.html";

fn linux_web_renderer_command(
    root: &Path,
    network_allowed: bool,
    width: u32,
    height: u32,
) -> WebSandboxCommand {
    let mut arguments = sandbox_prefix(root, network_allowed);
    arguments.extend([
        "chromium".into(),
        "--headless=new".into(),
        "--no-sandbox".into(),
        "--disable-dev-shm-usage".into(),
        "--disable-gpu".into(),
        "--no-first-run".into(),
        "--no-default-browser-check".into(),
        "--disable-extensions".into(),
        // BETA B6 (docs/bugs/WEB_FILE_ACCESS_BLACK_CANVAS.md): a file://
        // page's origin is opaque, so WITHOUT this flag its own images are
        // cross-origin for WebGL (texImage2D throws SecurityError) and
        // same-directory XHR/fetch is blocked — real Workshop wallpapers
        // render black. The flag makes every file: URL the page can name
        // readable, which inside this namespace means the content root and
        // the read-only system binds (/usr /etc /lib …); nothing from the
        // user's home is reachable and the network still needs the grant.
        // Narrowing the /etc bind is the follow-up recorded with B6.
        "--allow-file-access-from-files".into(),
        // Chromium 151 deprecates the silent software-WebGL fallback behind
        // --disable-gpu; this keeps WebGL (SwiftShader) available when the
        // fallback is removed. There is no GPU in the sandbox either way.
        "--enable-unsafe-swiftshader".into(),
        "--remote-debugging-pipe".into(),
        format!("--window-size={width},{height}"),
        "--user-data-dir=/tmp/kwe-profile".into(),
        "file:///wallpaper/index.html".into(),
    ]);
    WebSandboxCommand {
        program: "bwrap".into(),
        arguments,
        page_url: LINUX_PAGE_URL.into(),
        working_dir: None,
        sandbox: "bwrap",
    }
}

/// Parse a local (socket) X11 display number from a DISPLAY value. Local
/// displays are `:N` or `:N.S` — the socket file lives in
/// /tmp/.X11-unix/X&lt;N&gt;. A hostname-prefixed DISPLAY (`host:N`) reaches a
/// remote server (or an abstract socket) and has no file to bind, so it
/// parses to None.
fn x11_display_number(display: &str) -> Option<u32> {
    let digits = display.strip_prefix(':')?.split('.').next()?;
    if digits.is_empty() || !digits.bytes().all(|b| b.is_ascii_digit()) {
        return None;
    }
    digits.parse().ok()
}

/// The bwrap display-socket binds for the WINDOWED preview namespace
/// (BETA_M2d): the namespace shadows /tmp with an empty tmpfs and leaves
/// /run unbound, so an inherited DISPLAY/WAYLAND_DISPLAY points at sockets
/// that do not exist inside the sandbox — the preview could never connect
/// to any display without these. The selection is PURE (no filesystem
/// access): the caller drops any bind whose source does not exist, because
/// bwrap refuses to start on a missing source. Only socket files are ever
/// bound — never $XDG_RUNTIME_DIR as a whole, which would leak
/// kwallet/pipewire/ssh sockets to wallpaper JS. Neither display set
/// (offscreen preview) binds nothing.
pub fn display_binds(
    display: Option<&str>,
    wayland_display: Option<&str>,
    xdg_runtime_dir: Option<&str>,
) -> Vec<String> {
    let mut binds = Vec::new();
    if let Some(display) = display
        && x11_display_number(display).is_some()
    {
        binds.extend([
            "--ro-bind".into(),
            "/tmp/.X11-unix".into(),
            "/tmp/.X11-unix".into(),
        ]);
    }
    if let (Some(wayland), Some(runtime)) = (wayland_display, xdg_runtime_dir)
        && !wayland.is_empty()
        && wayland != "none"
    {
        let socket = format!("{}/{}", runtime.trim_end_matches('/'), wayland);
        binds.extend(["--ro-bind".into(), socket.clone(), socket]);
    }
    binds
}

/// Command line for the manager's user-visible web preview (BETA_M2d): the
/// same [`sandbox_prefix`] isolation as [`web_renderer_command`], but
/// chromium runs WINDOWED — no `--headless`, no `--remote-debugging-pipe`,
/// no screencast viewport — with the shared throwaway preview profile.
/// DISPLAY/WAYLAND_DISPLAY are inherited from the manager's environment
/// (the preview is the user-facing window; the sandbox does not clear
/// them, unlike the supervised renderer's stripped env), and the session's
/// display socket files are bound into the namespace (see
/// [`display_binds`]) so the window can actually connect. The old M2a
/// `chromium_command` form (empty bwrap root, no system ro-binds, no
/// `--no-sandbox`) could not exec chromium at all; this command is what
/// the manager actually launches.
pub fn web_preview_command(root: &Path, network_allowed: bool) -> WebSandboxCommand {
    if cfg!(target_os = "macos") {
        return macos::web_preview_command(root, network_allowed);
    }
    let mut arguments = sandbox_prefix(root, network_allowed);
    let display = std::env::var("DISPLAY").ok();
    let wayland_display = std::env::var("WAYLAND_DISPLAY").ok();
    let xdg_runtime_dir = std::env::var("XDG_RUNTIME_DIR").ok();
    let binds = display_binds(
        display.as_deref(),
        wayland_display.as_deref(),
        xdg_runtime_dir.as_deref(),
    );
    // Each bind is the flat triple --ro-bind SOURCE DEST.
    for bind in binds.chunks(3) {
        if bind[0] == "--ro-bind" && Path::new(&bind[1]).exists() {
            arguments.extend_from_slice(bind);
        }
    }
    arguments.extend([
        "chromium".into(),
        "--no-sandbox".into(),
        "--disable-dev-shm-usage".into(),
        "--no-first-run".into(),
        "--no-default-browser-check".into(),
        "--disable-extensions".into(),
        // BETA B6: same file-access and software-WebGL flags as the renderer
        // so the preview shows what the wallpaper will show. A file://
        // page's own images are otherwise cross-origin for WebGL.
        "--allow-file-access-from-files".into(),
        "--enable-unsafe-swiftshader".into(),
        "--user-data-dir=/tmp/kwe-preview-profile".into(),
        "file:///wallpaper/index.html".into(),
    ]);
    WebSandboxCommand {
        program: "bwrap".into(),
        arguments,
        page_url: LINUX_PAGE_URL.into(),
        working_dir: None,
        sandbox: "bwrap",
    }
}

pub fn sandbox_root(path: &Path) -> Option<PathBuf> {
    let canonical = std::fs::canonicalize(path).ok()?;
    if canonical.is_dir() && canonical.join("index.html").is_file() {
        Some(canonical)
    } else {
        None
    }
}

/// macOS web sandbox (docs/macos/MacOS-Port-Plan.md, MP-5b, gate G6).
/// There is no bubblewrap; the browser runs under `sandbox-exec` with a
/// generated SBPL profile and, as under bwrap on Linux, `--no-sandbox`
/// (Chromium's nested sandbox cannot initialise inside an outer Seatbelt
/// profile). The profile is last-match-wins SBPL: allow by default,
/// then deny every write outside the throwaway profile dir and the
/// temp/dev trees, deny reading the user's home except the content root,
/// and deny the network unless the content permission set grants it.
/// `KWE_WEB_SANDBOX=off` runs the browser bare (diagnosis only).
///
/// The browser binary: `KWE_CHROMIUM` (a path), else the first existing of
/// Chromium.app / Google Chrome.app / Brave / Microsoft Edge under
/// /Applications or ~/Applications.
pub mod macos {
    use std::path::{Path, PathBuf};

    use super::WebSandboxCommand;

    pub const DEFAULT_BROWSER_CANDIDATES: &[&str] = &[
        "/Applications/Chromium.app/Contents/MacOS/Chromium",
        "/Applications/Google Chrome.app/Contents/MacOS/Google Chrome",
        "/Applications/Brave Browser.app/Contents/MacOS/Brave Browser",
        "/Applications/Microsoft Edge.app/Contents/MacOS/Microsoft Edge",
    ];

    /// Resolves the browser binary. `None` when nothing is installed; the
    /// renderer then fails closed with an actionable diagnostic.
    pub fn browser_binary() -> Option<PathBuf> {
        if let Some(explicit) = std::env::var_os("KWE_CHROMIUM") {
            let path = PathBuf::from(explicit);
            return path.is_file().then_some(path);
        }
        let home = std::env::var_os("HOME").map(PathBuf::from);
        for candidate in DEFAULT_BROWSER_CANDIDATES {
            let system = PathBuf::from(candidate);
            if system.is_file() {
                return Some(system);
            }
            if let Some(home) = &home {
                let user = home.join(candidate.trim_start_matches('/'));
                if user.is_file() {
                    return Some(user);
                }
            }
        }
        None
    }

    /// Escapes a path for an SBPL string literal.
    fn sbpl_string(path: &Path) -> String {
        let text = path.to_string_lossy();
        let mut out = String::with_capacity(text.len() + 2);
        out.push('"');
        for ch in text.chars() {
            match ch {
                '"' => out.push_str("\\\""),
                '\\' => out.push_str("\\\\"),
                _ => out.push(ch),
            }
        }
        out.push('"');
        out
    }

    /// The `.app` bundle directory containing `binary`, when it is inside
    /// one (`~/Applications/Chromium.app/Contents/MacOS/Chromium` ->
    /// `~/Applications/Chromium.app`).
    pub fn bundle_root(binary: &Path) -> Option<PathBuf> {
        binary
            .ancestors()
            .find(|ancestor| ancestor.extension().is_some_and(|ext| ext == "app"))
            .map(Path::to_path_buf)
    }

    /// Which rule groups the profile carries. `Full` is production;
    /// `NetworkOnly` and `NoHomeDeny` exist so `KWE_WEB_SANDBOX=net-only` /
    /// `no-home` can bisect a browser that fails to boot under the full
    /// profile (scripts/macos/smoke-web-macos.sh runs every variant on CI).
    #[derive(Debug, Clone, Copy, PartialEq, Eq)]
    pub enum ProfileVariant {
        Full,
        NetworkOnly,
        NoHomeDeny,
        /// `Full` plus the hardening backlog: Mach service allow-list,
        /// no IOKit, process-exec only inside the browser bundle.
        /// Opt-in (`KWE_WEB_SANDBOX=strict`) and measured on CI until it
        /// renders reliably; then it becomes `Full`.
        Strict,
    }

    impl ProfileVariant {
        fn from_env() -> Option<Self> {
            match std::env::var("KWE_WEB_SANDBOX").as_deref() {
                Ok("off") => None,
                Ok("net-only") => Some(Self::NetworkOnly),
                Ok("no-home") => Some(Self::NoHomeDeny),
                Ok("strict") => Some(Self::Strict),
                _ => Some(Self::Full),
            }
        }
    }

    /// Mach services a headless Chromium needs, drawn from Chromium's own
    /// sandbox profiles (common.sb and friends) and grown from measured
    /// `deny(1) mach-lookup` lines on CI. Anything not listed is denied
    /// under `Strict` — notably LaunchServices (unconfined app launch) and
    /// the pasteboard.
    pub const STRICT_MACH_SERVICES: &[&str] = &[
        "com.apple.system.logger",
        "com.apple.system.notification_center",
        "com.apple.distributed_notifications@Uv3",
        "com.apple.cfprefsd.daemon",
        "com.apple.cfprefsd.agent",
        "com.apple.SecurityServer",
        "com.apple.trustd",
        "com.apple.trustd.agent",
        "com.apple.system.opendirectoryd.libinfo",
        "com.apple.system.opendirectoryd.membership",
        "com.apple.system.opendirectoryd.api",
        "com.apple.CoreServices.coreservicesd",
        "com.apple.lsd.mapdb",
        // Chrome's browser process cannot start without LaunchServices
        // (measured: denied lookup -> exit before CDP). Kept; the strict
        // variant's remaining value is pasteboard, IOKit, and exec scope.
        "com.apple.coreservices.launchservicesd",
        "com.apple.fonts",
        "com.apple.FontObjectsServer",
        "com.apple.FontServer",
        "com.apple.logd",
        "com.apple.diagnosticd",
        "com.apple.analyticsd",
        "com.apple.PowerManagement.control",
        "com.apple.audio.audiohald",
        "com.apple.audio.coreaudiod",
        "com.apple.audio.SandboxHelper",
        "com.apple.windowserver.active",
        "com.apple.CARenderServer",
        "com.apple.dyld.closured",
        "com.apple.system.DirectoryService.libinfo_v1",
        "com.apple.system.DirectoryService.membership_v1",
        "com.apple.mobileassetd",
        "com.apple.tccd",
        "com.apple.tccd.system",
        "com.apple.nesessionmanager.content-filter",
        "com.apple.networkd",
        "com.apple.usymptomsd",
        "com.apple.symptomsd",
        "com.apple.SystemConfiguration.configd",
        "com.apple.SystemConfiguration.DNSConfiguration",
        "com.apple.SystemConfiguration.NetworkInformation",
        "com.apple.system.libinfo.muser",
        "com.apple.bsd.dirhelper",
        "com.apple.iconservices",
        "com.apple.iconservices.store",
        "com.apple.ocspd",
        "com.apple.securityd.xpc",
        "com.apple.xpc.activity.unmanaged",
        "com.apple.lskdd",
        "com.apple.metalcompilerservice",
        "com.apple.cvmsServ",
        "com.apple.gpumemd.source",
        "com.apple.backupd.sandbox.xpc",
        "com.apple.language.assetd",
        "com.apple.runningboard",
        // NSApplication start-up registers with the Dock even headless.
        "com.apple.dock.server",
    ];

    /// The production SBPL profile for one renderer launch (`Full`). Pure.
    /// Seatbelt matches RESOLVED paths, so temp dirs are allowed both as
    /// given and under `/private/var/folders` (where `$TMPDIR` really
    /// lives), and a browser bundle under the (otherwise denied) home is
    /// re-allowed explicitly. Used by the tests; production goes through
    /// `profile_variant` with the env-selected variant.
    #[allow(dead_code)]
    pub fn profile(
        root: &Path,
        profile_dir: &Path,
        home: Option<&Path>,
        browser_bundle: Option<&Path>,
        network_allowed: bool,
    ) -> String {
        profile_variant(root, profile_dir, home, browser_bundle, network_allowed, ProfileVariant::Full)
    }

    pub fn profile_variant(
        root: &Path,
        profile_dir: &Path,
        home: Option<&Path>,
        browser_bundle: Option<&Path>,
        network_allowed: bool,
        variant: ProfileVariant,
    ) -> String {
        profile_variant_with_temp(
            root,
            profile_dir,
            temp_root().as_deref(),
            home,
            browser_bundle,
            network_allowed,
            variant,
        )
    }

    /// The resolved per-user temp directory (`$TMPDIR`, usually
    /// `/private/var/folders/<xx>/<yyy>/T`). The browser binds its IPC and
    /// crash-handler sockets there, so the profile grants that directory —
    /// not the whole `/private/var/folders` tree, which also holds every
    /// other app's caches for the same user.
    fn temp_root() -> Option<PathBuf> {
        let base = std::env::temp_dir();
        Some(std::fs::canonicalize(&base).unwrap_or(base))
    }

    pub fn profile_variant_with_temp(
        root: &Path,
        profile_dir: &Path,
        temp_root: Option<&Path>,
        home: Option<&Path>,
        browser_bundle: Option<&Path>,
        network_allowed: bool,
        variant: ProfileVariant,
    ) -> String {
        let mut rules = String::from("(version 1)\n(allow default)\n");
        if variant == ProfileVariant::NetworkOnly {
            if !network_allowed {
                rules.push_str("(deny network*)\n");
                rules.push_str("(allow network* (local unix-socket) (remote unix-socket))\n");
            }
            return rules;
        }
        // Writes: only the throwaway browser profile, the per-user temp
        // directory, /private/tmp, and /dev.
        rules.push_str("(deny file-write*)\n");
        let mut allowed_writes = vec![
            format!("(subpath {})", sbpl_string(profile_dir)),
            "(subpath \"/private/tmp\")".to_string(),
            "(subpath \"/dev\")".to_string(),
        ];
        if let Some(temp) = temp_root {
            allowed_writes.push(format!("(subpath {})", sbpl_string(temp)));
        }
        rules.push_str(&format!("(allow file-write* {})\n", allowed_writes.join(" ")));
        // Reads: every user home (/Users) is off limits except the content
        // root, the worker's own private HOME (a daemon-created per-launch
        // directory the browser resolves its default paths under — denying
        // it broke Chrome's user-data-dir lookup on macOS 14), the
        // browser's bundle when it lives under ~/Applications, and the
        // profile/temp trees.
        if variant != ProfileVariant::NoHomeDeny {
            rules.push_str("(deny file-read* (subpath \"/Users\"))\n");
            // Path resolution (realpath, getcwd) stats each parent directory;
            // the home directory NAMES are not secret, their contents are.
            // CoreFoundation start-up in the browser reads the real user's
            // ~/.CFUserTextEncoding and stats ~/Library and
            // ~/Library/Application Support (measured denials on macOS 14:
            // without these the browser fails its user-data-dir lookup).
            rules.push_str(
                "(allow file-read-metadata (literal \"/Users\") (regex #\"^/Users/[^/]+$\") (regex #\"^/Users/[^/]+/Library$\") (regex #\"^/Users/[^/]+/Library/Application Support$\"))\n",
            );
            rules.push_str("(allow file-read-data (regex #\"^/Users/[^/]+/\\.CFUserTextEncoding$\"))\n");
            // Google Chrome's first launch on a machine insists on creating
            // ~/Library/Application Support/Google (Keystone's shared dir)
            // and aborts its user-data-dir lookup when that mkdir is denied
            // (measured: deterministic on fresh runner images, passing on
            // images where the directory already existed). Creating that
            // one empty directory is allowed; nothing inside it is.
            rules.push_str(
                "(allow file-write-create (regex #\"^/Users/[^/]+/Library/Application Support/Google(/Chrome)?$\"))\n",
            );
            rules.push_str(
                "(allow file-read-metadata (regex #\"^/Users/[^/]+/Library/Application Support/Google(/[^/]+)?$\"))\n",
            );
        }
        let mut allowed_reads = vec![
            format!("(subpath {})", sbpl_string(root)),
            format!("(subpath {})", sbpl_string(profile_dir)),
            "(subpath \"/private/tmp\")".to_string(),
        ];
        if let Some(temp) = temp_root {
            allowed_reads.push(format!("(subpath {})", sbpl_string(temp)));
        }
        if let Some(worker_home) = home {
            allowed_reads.push(format!("(subpath {})", sbpl_string(worker_home)));
        }
        if let Some(bundle) = browser_bundle {
            allowed_reads.push(format!("(subpath {})", sbpl_string(bundle)));
        }
        rules.push_str(&format!("(allow file-read* {})\n", allowed_reads.join(" ")));
        // Network: IP networking off unless the content grant allows it.
        // Seatbelt's `network*` also covers Unix domain sockets, and the
        // browser binds its own IPC/crash-handler sockets under $TMPDIR
        // (measured on macOS 14: "Failed to bind() /var/folders/..." and a
        // backend_reject), so local sockets are re-allowed after the deny.
        if !network_allowed {
            rules.push_str("(deny network*)\n");
            rules.push_str("(allow network* (local unix-socket) (remote unix-socket))\n");
        }
        if variant == ProfileVariant::Strict {
            // Mach services: only the allow-list. The pasteboard is
            // deliberately absent; LaunchServices had to stay (see list).
            rules.push_str("(deny mach-lookup)\n");
            let names: Vec<String> = STRICT_MACH_SERVICES
                .iter()
                .map(|name| format!("(global-name \"{name}\")"))
                .collect();
            rules.push_str(&format!("(allow mach-lookup {})\n", names.join(" ")));
            // The browser's own per-process rendezvous port (browser <->
            // helper handshake): `<bundle id>.MachPortRendezvousServer.<pid>`
            // (measured: 181 denials, helpers never attach without it).
            rules.push_str(
                "(allow mach-lookup (global-name-regex #\"\\.MachPortRendezvousServer\\.[0-9]+$\"))\n",
            );
            // Device access: power assertions and the boot-disk identity
            // client the browser opens at start (measured); nothing else.
            rules.push_str("(deny iokit-open)\n");
            rules.push_str(
                "(allow iokit-open (iokit-user-client-class \"RootDomainUserClient\") (iokit-user-client-class \"AppleNVMeEANUC\"))\n",
            );
            // exec only inside the browser's own bundle (helpers, crashpad)
            // — and the system loader/shell stubs Chromium's helpers use.
            rules.push_str("(deny process-exec*)\n");
            // /usr/bin/profiles: Chrome's managed-device policy probe.
            let mut exec_allows = vec![
                "(subpath \"/usr/lib\")".to_string(),
                "(literal \"/usr/bin/profiles\")".to_string(),
            ];
            if let Some(bundle) = browser_bundle {
                exec_allows.push(format!("(subpath {})", sbpl_string(bundle)));
            }
            rules.push_str(&format!("(allow process-exec* {})\n", exec_allows.join(" ")));
        }
        rules
    }

    fn temp_profile_dir(label: &str) -> PathBuf {
        // Resolved (`/private/var/folders/...`) when possible: Seatbelt
        // subpath rules match the real path, not the `/var` symlink.
        let base = std::env::temp_dir();
        let base = std::fs::canonicalize(&base).unwrap_or(base);
        base.join(format!("kwe-{label}-{}", std::process::id()))
    }

    pub fn web_renderer_command(
        root: &Path,
        network_allowed: bool,
        width: u32,
        height: u32,
    ) -> WebSandboxCommand {
        let browser = browser_binary()
            .map(|path| path.to_string_lossy().into_owned())
            .unwrap_or_else(|| "chromium-not-found (set KWE_CHROMIUM)".into());
        let profile_dir = temp_profile_dir("web-profile");
        let page_url = format!("file://{}/index.html", root.display());
        let browser_arguments = vec![
            "--headless=new".into(),
            "--disable-dev-shm-usage".into(),
            "--disable-gpu".into(),
            "--no-first-run".into(),
            "--no-default-browser-check".into(),
            "--disable-extensions".into(),
            "--allow-file-access-from-files".into(),
            "--enable-unsafe-swiftshader".into(),
            "--remote-debugging-pipe".into(),
            format!("--window-size={width},{height}"),
            format!("--user-data-dir={}", profile_dir.display()),
            page_url.clone(),
        ];
        wrap(browser, browser_arguments, root, &profile_dir, network_allowed, page_url)
    }

    pub fn web_preview_command(root: &Path, network_allowed: bool) -> WebSandboxCommand {
        let browser = browser_binary()
            .map(|path| path.to_string_lossy().into_owned())
            .unwrap_or_else(|| "chromium-not-found (set KWE_CHROMIUM)".into());
        let profile_dir = temp_profile_dir("preview-profile");
        let page_url = format!("file://{}/index.html", root.display());
        let browser_arguments = vec![
            "--disable-dev-shm-usage".into(),
            "--no-first-run".into(),
            "--no-default-browser-check".into(),
            "--disable-extensions".into(),
            "--allow-file-access-from-files".into(),
            "--enable-unsafe-swiftshader".into(),
            format!("--user-data-dir={}", profile_dir.display()),
            page_url.clone(),
        ];
        wrap(browser, browser_arguments, root, &profile_dir, network_allowed, page_url)
    }

    fn wrap(
        browser: String,
        browser_arguments: Vec<String>,
        root: &Path,
        profile_dir: &Path,
        network_allowed: bool,
        page_url: String,
    ) -> WebSandboxCommand {
        let Some(variant) = ProfileVariant::from_env() else {
            return WebSandboxCommand {
                program: browser,
                arguments: browser_arguments,
                page_url,
                working_dir: Some(root.to_path_buf()),
                sandbox: "none",
            };
        };
        let home = std::env::var_os("HOME").map(PathBuf::from);
        let bundle = bundle_root(Path::new(&browser));
        let profile = profile_variant(
            root,
            profile_dir,
            home.as_deref(),
            bundle.as_deref(),
            network_allowed,
            variant,
        );
        // Inside the outer Seatbelt profile the browser's own nested sandbox
        // cannot initialise (measured on macOS 14: "Failed to initialize
        // sandbox", network service and GPU process crash loops), exactly
        // as under bwrap on Linux — so, as on Linux, the OS sandbox is the
        // boundary and the browser runs with --no-sandbox. The bare
        // (KWE_WEB_SANDBOX=off) lane keeps the browser's own sandbox.
        let mut arguments = vec!["-p".to_string(), profile, browser, "--no-sandbox".to_string()];
        arguments.extend(browser_arguments);
        WebSandboxCommand {
            program: "/usr/bin/sandbox-exec".into(),
            arguments,
            page_url,
            working_dir: Some(root.to_path_buf()),
            sandbox: match variant {
                ProfileVariant::Full => "seatbelt",
                ProfileVariant::Strict => "seatbelt:strict",
                ProfileVariant::NetworkOnly => "seatbelt:net-only",
                ProfileVariant::NoHomeDeny => "seatbelt:no-home",
            },
        }
    }

    #[cfg(test)]
    mod tests {
        use super::*;

        #[test]
        fn profile_denies_home_and_network_but_allows_content_and_profile() {
            let text = profile_variant_with_temp(
                Path::new("/Users/me/WE/steamapps/workshop/content/431960/1"),
                Path::new("/private/var/folders/x/T/kwe-web-profile-1"),
                Some(Path::new("/private/var/folders/x/T")),
                Some(Path::new("/Users/me/Library/Application Support/kwe/state/runtime/home-3")),
                Some(Path::new("/Users/me/Applications/Chromium.app")),
                false,
                ProfileVariant::Full,
            );
            assert!(text.contains("(allow file-write* (subpath \"/private/var/folders/x/T/kwe-web-profile-1\") (subpath \"/private/tmp\") (subpath \"/dev\") (subpath \"/private/var/folders/x/T\"))"));
            assert!(!text.contains("(subpath \"/private/var/folders\")"));
            assert!(text.contains("(deny file-read* (subpath \"/Users\"))"));
            assert!(text.contains("(allow file-read-data (regex #\"^/Users/[^/]+/\\.CFUserTextEncoding$\"))"));
            assert!(text.contains("(regex #\"^/Users/[^/]+/Library/Application Support$\")"));
            assert!(text.contains("(allow file-write-create (regex #\"^/Users/[^/]+/Library/Application Support/Google(/Chrome)?$\"))"));
            assert!(text.contains("(allow file-read-metadata (regex #\"^/Users/[^/]+/Library/Application Support/Google(/[^/]+)?$\"))"));
            assert!(text.contains("(subpath \"/Users/me/Applications/Chromium.app\")"));
            assert!(text.contains("(subpath \"/Users/me/Library/Application Support/kwe/state/runtime/home-3\")"));
            assert!(text.contains("(allow file-read* (subpath \"/Users/me/WE/steamapps/workshop/content/431960/1\") (subpath \"/private/var/folders/x/T/kwe-web-profile-1\") (subpath \"/private/tmp\") (subpath \"/private/var/folders/x/T\")"));
            assert_eq!(
                bundle_root(Path::new("/Users/me/Applications/Chromium.app/Contents/MacOS/Chromium")),
                Some(PathBuf::from("/Users/me/Applications/Chromium.app"))
            );
            assert_eq!(bundle_root(Path::new("/opt/homebrew/bin/chromium")), None);
            assert!(text.starts_with("(version 1)\n(allow default)\n"));
            assert!(text.contains("(deny file-write*)"));
            assert!(text.contains("(deny network*)\n(allow network* (local unix-socket) (remote unix-socket))\n"));
            let open = profile(Path::new("/a"), Path::new("/b"), None, None, true);
            assert!(!open.contains("network"));
            assert!(open.contains("(deny file-read* (subpath \"/Users\"))"));
        }

        #[test]
        fn strict_variant_adds_mach_iokit_and_exec_rules_after_the_full_ones() {
            let strict = profile_variant_with_temp(
                Path::new("/a"),
                Path::new("/private/var/folders/x/T/p"),
                Some(Path::new("/private/var/folders/x/T")),
                None,
                Some(Path::new("/Applications/Google Chrome.app")),
                false,
                ProfileVariant::Strict,
            );
            assert!(strict.contains("(deny file-write*)"));
            assert!(strict.contains("(deny mach-lookup)\n(allow mach-lookup (global-name \"com.apple.system.logger\")"));
            assert!(strict.contains("(deny iokit-open)"));
            assert!(strict.contains("(deny process-exec*)\n(allow process-exec* (subpath \"/usr/lib\") (literal \"/usr/bin/profiles\") (subpath \"/Applications/Google Chrome.app\"))"));
            assert!(!strict.contains("pasteboard"));
        }

        #[test]
        fn sbpl_strings_escape_quotes_and_backslashes() {
            assert_eq!(sbpl_string(Path::new("/a \"b\"/c\\d")), "\"/a \\\"b\\\"/c\\\\d\"");
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn page_url_marker_strips_the_scheme() {
        let command = WebSandboxCommand {
            program: "x".into(),
            arguments: Vec::new(),
            page_url: "file:///wallpaper/index.html".into(),
            working_dir: None,
            sandbox: "bwrap",
        };
        assert_eq!(page_url_marker(&command), "/wallpaper/index.html");
    }

    #[cfg(target_os = "linux")]
    #[test]
    fn defaults_to_network_isolation_and_read_only_content() {
        let command = chromium_command(Path::new("/tmp/wallpaper"), false);
        assert_eq!(command.program, "bwrap");
        assert!(command.arguments.contains(&"--unshare-net".into()));
        assert!(
            command
                .arguments
                .windows(2)
                .any(|pair| pair == ["--ro-bind", "/tmp/wallpaper"])
        );
        assert!(
            chromium_command(Path::new("/tmp/wallpaper"), true)
                .arguments
                .iter()
                .all(|arg| arg != "--unshare-net")
        );
    }

    #[cfg(target_os = "linux")]
    #[test]
    fn web_renderer_command_carries_the_pinned_flags() {
        let command = web_renderer_command(Path::new("/tmp/wallpaper"), false, 160, 90);
        assert_eq!(command.program, "bwrap");
        let arguments = &command.arguments;
        for flag in [
            "--die-with-parent",
            "--new-session",
            "--unshare-net",
            "--remote-debugging-pipe",
            "--headless=new",
            "--no-sandbox",
            "--disable-dev-shm-usage",
            "--disable-gpu",
            "--no-first-run",
            "--no-default-browser-check",
            "--disable-extensions",
            "--allow-file-access-from-files",
            "--enable-unsafe-swiftshader",
            "--user-data-dir=/tmp/kwe-profile",
            "file:///wallpaper/index.html",
        ] {
            assert!(arguments.contains(&flag.into()), "missing flag {flag}");
        }
        // The M2b sandbox binds the browser's system paths read-only; the
        // content root overlays /wallpaper on top of them.
        for pair in [
            ["--ro-bind", "/usr"],
            ["--ro-bind", "/etc"],
            ["--ro-bind", "/lib"],
            ["--ro-bind", "/lib64"],
            ["--ro-bind", "/bin"],
            ["--ro-bind", "/sbin"],
            ["--ro-bind", "/tmp/wallpaper"],
        ] {
            assert!(arguments.windows(2).any(|w| w == pair), "missing {pair:?}");
        }
        assert!(
            arguments
                .windows(3)
                .any(|w| w == ["--ro-bind", "/tmp/wallpaper", "/wallpaper"])
        );
    }

    #[cfg(target_os = "linux")]
    #[test]
    fn web_renderer_command_formats_window_size_and_toggles_network() {
        let command = web_renderer_command(Path::new("/tmp/wallpaper"), false, 960, 540);
        assert!(command.arguments.contains(&"--window-size=960,540".into()));
        assert!(command.arguments.contains(&"--unshare-net".into()));
        let open = web_renderer_command(Path::new("/tmp/wallpaper"), true, 160, 90);
        assert!(open.arguments.contains(&"--window-size=160,90".into()));
        assert!(open.arguments.iter().all(|arg| arg != "--unshare-net"));
    }

    #[cfg(target_os = "linux")]
    #[test]
    fn web_preview_command_is_windowed_with_the_m2b_isolation() {
        let command = web_preview_command(Path::new("/tmp/wallpaper"), false);
        assert_eq!(command.program, "bwrap");
        let arguments = &command.arguments;
        for flag in [
            "--die-with-parent",
            "--new-session",
            "--unshare-net",
            "--no-sandbox",
            "--disable-dev-shm-usage",
            "--no-first-run",
            "--no-default-browser-check",
            "--disable-extensions",
            "--allow-file-access-from-files",
            "--enable-unsafe-swiftshader",
            "--user-data-dir=/tmp/kwe-preview-profile",
            "file:///wallpaper/index.html",
        ] {
            assert!(arguments.contains(&flag.into()), "missing flag {flag}");
        }
        // The preview is windowed: no headless flag, no CDP pipe, no
        // screencast viewport.
        for prefix in [
            "--headless=new",
            "--remote-debugging-pipe",
            "--window-size=",
        ] {
            assert!(
                arguments.iter().all(|arg| !arg.starts_with(prefix)),
                "unexpected flag {prefix}"
            );
        }
        // The M2b bind set: the browser's system paths and the content root
        // overlay, exactly as the supervised renderer builds them.
        for pair in [
            ["--ro-bind", "/usr"],
            ["--ro-bind", "/etc"],
            ["--ro-bind", "/lib"],
            ["--ro-bind", "/lib64"],
            ["--ro-bind", "/bin"],
            ["--ro-bind", "/sbin"],
            ["--ro-bind", "/tmp/wallpaper"],
        ] {
            assert!(arguments.windows(2).any(|w| w == pair), "missing {pair:?}");
        }
        assert!(
            arguments
                .windows(3)
                .any(|w| w == ["--ro-bind", "/tmp/wallpaper", "/wallpaper"])
        );
        let open = web_preview_command(Path::new("/tmp/wallpaper"), true);
        assert!(open.arguments.iter().all(|arg| arg != "--unshare-net"));
    }

    #[cfg(target_os = "linux")]
    #[test]
    fn x11_local_displays_parse_and_remote_displays_do_not() {
        assert_eq!(x11_display_number(":0"), Some(0));
        assert_eq!(x11_display_number(":10"), Some(10));
        assert_eq!(x11_display_number(":0.0"), Some(0));
        assert_eq!(x11_display_number(":99.15"), Some(99));
        assert_eq!(x11_display_number("workstation:10.0"), None);
        assert_eq!(x11_display_number(":abc"), None);
        assert_eq!(x11_display_number(":"), None);
        assert_eq!(x11_display_number(""), None);
    }

    #[cfg(target_os = "linux")]
    #[test]
    fn display_binds_binds_the_x11_socket_dir_for_a_local_display() {
        assert_eq!(
            display_binds(Some(":0"), None, None),
            ["--ro-bind", "/tmp/.X11-unix", "/tmp/.X11-unix"]
        );
        // A hostname-prefixed DISPLAY has no local socket file to bind.
        assert!(display_binds(Some("workstation:10.0"), None, None).is_empty());
    }

    #[cfg(target_os = "linux")]
    #[test]
    fn display_binds_binds_only_the_wayland_socket_file() {
        assert_eq!(
            display_binds(None, Some("wayland-0"), Some("/run/user/1000")),
            [
                "--ro-bind",
                "/run/user/1000/wayland-0",
                "/run/user/1000/wayland-0",
            ]
        );
        // The runtime dir itself is never bound — only the socket file
        // (the dir as a mount source would leak the user's other sockets).
        assert!(
            display_binds(None, Some("wayland-0"), Some("/run/user/1000"))
                .iter()
                .all(|arg| arg != "/run/user/1000")
        );
    }

    #[cfg(target_os = "linux")]
    #[test]
    fn display_binds_binds_both_displays_and_nothing_without_them() {
        assert_eq!(
            display_binds(Some(":0"), Some("wayland-0"), Some("/run/user/1000")),
            [
                "--ro-bind",
                "/tmp/.X11-unix",
                "/tmp/.X11-unix",
                "--ro-bind",
                "/run/user/1000/wayland-0",
                "/run/user/1000/wayland-0",
            ]
        );
        // Offscreen preview (no display at all): nothing to bind.
        assert!(display_binds(None, None, None).is_empty());
        // "none" is the explicit offscreen sentinel some sessions export.
        assert!(display_binds(None, Some("none"), Some("/run/user/1000")).is_empty());
        // No runtime dir means no socket path to bind.
        assert!(display_binds(None, Some("wayland-0"), None).is_empty());
    }

    #[cfg(target_os = "linux")]
    #[test]
    fn web_preview_command_binds_a_present_wayland_socket_and_skips_a_missing_one() {
        // The production function reads the process environment (set_var is
        // unsafe in the 2024 edition); the pure selection logic is covered
        // by the display_binds tests above. This covers the env plumbing
        // and the missing-source filter.
        let runtime = std::env::temp_dir().join(format!("kwe-wp-probe-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&runtime);
        std::fs::create_dir_all(&runtime).unwrap();
        let socket_path = runtime.join("wayland-probe");
        let _listener = std::os::unix::net::UnixListener::bind(&socket_path).unwrap();
        let old_wayland = std::env::var("WAYLAND_DISPLAY").ok();
        let old_runtime = std::env::var("XDG_RUNTIME_DIR").ok();
        unsafe {
            std::env::set_var("WAYLAND_DISPLAY", "wayland-probe");
            std::env::set_var("XDG_RUNTIME_DIR", runtime.display().to_string());
        }
        let command = web_preview_command(Path::new("/tmp/wallpaper"), false);
        let socket = socket_path.display().to_string();
        let present = ["--ro-bind".to_string(), socket.clone(), socket.clone()];
        assert!(
            command.arguments.windows(3).any(|w| w == present),
            "present wayland socket must be bound"
        );
        unsafe {
            std::env::set_var("WAYLAND_DISPLAY", "missing-socket");
        }
        let command = web_preview_command(Path::new("/tmp/wallpaper"), false);
        let missing = format!("{}/missing-socket", runtime.display());
        let absent = ["--ro-bind".to_string(), missing.clone(), missing];
        assert!(
            !command.arguments.windows(3).any(|w| w == absent),
            "missing wayland socket must not be bound"
        );
        match old_wayland {
            Some(value) => unsafe { std::env::set_var("WAYLAND_DISPLAY", value) },
            None => unsafe { std::env::remove_var("WAYLAND_DISPLAY") },
        }
        match old_runtime {
            Some(value) => unsafe { std::env::set_var("XDG_RUNTIME_DIR", value) },
            None => unsafe { std::env::remove_var("XDG_RUNTIME_DIR") },
        }
        let _ = std::fs::remove_dir_all(&runtime);
    }
}
