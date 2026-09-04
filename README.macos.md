# KDE Wallpaper Engine — macOS fork

This repository is the macOS port of
[KDE Wallpaper Engine](https://github.com/kde-wallpaper-engine/kde-wallpaper-engine)
(the Linux/Plasma project is `upstream`). It runs Wallpaper Engine Workshop
wallpapers (video, web, scene) as the macOS desktop picture through the
same supervised daemon, renderers, and frame protocol; only the platform
layer differs. Status: **pre-alpha, not yet run on macOS hardware** —
everything here was built and smoke-tested on Linux with the Rust
workspace cross-checked for `aarch64-apple-darwin`.

- Plan and gates: `docs/macos/MacOS-Port-Plan.md`
- What works / what is stubbed / review log: `docs/macos/PORTING_STATUS.md`
- Toolchain: `docs/macos/TOOLCHAIN.md`
- Getting content onto a Mac: `docs/macos/CONTENT.md`

## First run on a Mac (Apple Silicon, macOS 14+)

```sh
xcode-select --install
brew install rustup pkg-config cmake ninja mpv shaderc molten-vk vulkan-loader vulkan-headers qt@6 ffmpeg jq
brew install --cask chromium          # web wallpapers (Chrome/Brave/Edge also detected)
rustup-init -y && . "$HOME/.cargo/env"

git clone https://github.com/thescruggs/kwe-macos.git && cd kwe-macos
packaging/macos/install-dev.sh        # builds everything, installs two LaunchAgents, starts them
```

Then:

```sh
target/release/kwe scan                                   # what content was found (see docs/macos/CONTENT.md)
target/release/kwe daemon-call --method wallpaper.outputs # your displays as the daemon sees them
build/agent/apps/kwe-manager/kwe-manager                  # gallery; Apply covers the chosen display
```

Smoke without touching the desktop:

```sh
target/release/kwe-vulkan                     # MoltenVK device present?
scripts/macos/smoke-display-agent.sh build/agent
```

Logs: `~/Library/Logs/kwe/`. Undo: `packaging/macos/uninstall-dev.sh`.
First visual check on the real desktop: `scripts/macos/desktop-test.sh 20`.

GitHub Actions (`.github/workflows/macos.yml`) builds and smoke-tests
every push on hosted macOS runners; it needs Actions minutes (macOS bills
at 10×) — see the billing note in `docs/macos/PORTING_STATUS.md`.

## What to expect

The hardware-verify list in `docs/macos/PORTING_STATUS.md` names the six
behaviours that could not be proven from Linux (desktop window ordering,
mouse monitor permissions, Seatbelt vs Chromium, MoltenVK, audio capture,
Homebrew Qt configure). Please file what you see as issue reports
(`kwe reports`) or notes in that file.
