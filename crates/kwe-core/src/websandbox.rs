// SPDX-License-Identifier: Apache-2.0
use std::path::{Path, PathBuf};

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct WebSandboxCommand {
    pub program: String,
    pub arguments: Vec<String>,
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
    }
}

/// Command line for the supervised web renderer (M2b): bwrap with the same
/// isolation intent as [`chromium_command`], then chromium headless with
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
        "chromium".into(),
        "--headless=new".into(),
        "--no-sandbox".into(),
        "--disable-dev-shm-usage".into(),
        "--disable-gpu".into(),
        "--no-first-run".into(),
        "--no-default-browser-check".into(),
        "--disable-extensions".into(),
        "--remote-debugging-pipe".into(),
        format!("--window-size={width},{height}"),
        "--user-data-dir=/tmp/kwe-profile".into(),
        "file:///wallpaper/index.html".into(),
    ];
    if !network_allowed {
        arguments.insert(0, "--unshare-net".into());
    }
    WebSandboxCommand {
        program: "bwrap".into(),
        arguments,
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

#[cfg(test)]
mod tests {
    use super::*;
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

    #[test]
    fn web_renderer_command_formats_window_size_and_toggles_network() {
        let command = web_renderer_command(Path::new("/tmp/wallpaper"), false, 960, 540);
        assert!(command.arguments.contains(&"--window-size=960,540".into()));
        assert!(command.arguments.contains(&"--unshare-net".into()));
        let open = web_renderer_command(Path::new("/tmp/wallpaper"), true, 160, 90);
        assert!(open.arguments.contains(&"--window-size=160,90".into()));
        assert!(open.arguments.iter().all(|arg| arg != "--unshare-net"));
    }
}
