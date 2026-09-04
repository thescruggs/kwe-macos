// SPDX-License-Identifier: GPL-3.0-or-later
//! Linker search path for the system libmpv on macOS. Linux distributions
//! put `libmpv.so` on the default search path; Homebrew does not, so the
//! `#[link(name = "mpv")]` in `lib.rs` needs the directory spelled out.
//! Resolution order: `MPV_LIB_DIR`, `pkg-config --variable=libdir mpv`,
//! `brew --prefix mpv`, then the two conventional Homebrew prefixes.
use std::path::PathBuf;
use std::process::Command;

fn main() {
    println!("cargo:rerun-if-env-changed=MPV_LIB_DIR");
    // Build scripts run on the HOST; only CARGO_CFG_TARGET_OS names the target.
    if std::env::var("CARGO_CFG_TARGET_OS").as_deref() != Ok("macos") {
        return;
    }
    if let Some(dir) = std::env::var_os("MPV_LIB_DIR").map(PathBuf::from) {
        emit(&dir);
        return;
    }
    if let Some(dir) = command_output("pkg-config", &["--variable=libdir", "mpv"]) {
        emit(&PathBuf::from(dir));
        return;
    }
    if let Some(prefix) = command_output("brew", &["--prefix", "mpv"]) {
        emit(&PathBuf::from(prefix).join("lib"));
        return;
    }
    for candidate in ["/opt/homebrew/lib", "/usr/local/lib"] {
        let dir = PathBuf::from(candidate);
        if dir.join("libmpv.dylib").exists() {
            emit(&dir);
            return;
        }
    }
    println!(
        "cargo:warning=libmpv not found (set MPV_LIB_DIR or `brew install mpv`); linking will fail"
    );
}

fn command_output(program: &str, arguments: &[&str]) -> Option<String> {
    let output = Command::new(program).args(arguments).output().ok()?;
    if !output.status.success() {
        return None;
    }
    let text = String::from_utf8(output.stdout).ok()?;
    let text = text.trim();
    (!text.is_empty()).then(|| text.to_string())
}

fn emit(dir: &std::path::Path) {
    println!("cargo:rustc-link-search=native={}", dir.display());
}
