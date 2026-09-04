# macOS toolchain

Target floor: macOS 14 (Sonoma), Apple Silicon first (plan gate G5).

## Prerequisites

```sh
xcode-select --install
brew install rustup pkg-config cmake ninja mpv shaderc molten-vk vulkan-loader vulkan-headers qt@6
brew install --cask chromium      # web wallpapers (Google Chrome also works: KWE_CHROMIUM=/Applications/Google\ Chrome.app/Contents/MacOS/Google\ Chrome)
rustup-init -y && . "$HOME/.cargo/env"
```

`shaderc` must be discoverable by pkg-config and `libmpv.dylib` by the
linker. Put this in your shell profile:

```sh
export PKG_CONFIG_PATH="$(brew --prefix shaderc)/lib/pkgconfig:${PKG_CONFIG_PATH:-}"
export VK_ICD_FILENAMES="$(brew --prefix molten-vk)/share/vulkan/icd.d/MoltenVK_icd.json"
```

(`kwe-mpv/build.rs` finds libmpv through `pkg-config`, `brew --prefix mpv`,
or `MPV_LIB_DIR`.)

## Build

```sh
cargo build --workspace --release
```

Binaries land in `target/release/`: `kwe-daemon`, `kwe`, the renderers, and
workers. The daemon expects its workers beside itself, so run everything
from that directory or install them together.

## Smoke (no desktop involvement)

```sh
cd target/release
./kwe-vulkan                      # expects a MoltenVK device
./kwe-daemon --help
./kwe scan
```

## Cross-checking from Linux

`scripts/macos/cross-check.sh` type-checks the whole workspace for
`aarch64-apple-darwin` on a Linux host (no Apple SDK; C build scripts are
stubbed). It catches cfg and libc-surface mistakes but proves nothing about
linking or runtime.

## Known gaps on macOS at this stage

See `PORTING_STATUS.md`.
