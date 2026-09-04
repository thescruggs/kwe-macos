#!/bin/sh
# Cross-check the whole Rust workspace for aarch64-apple-darwin from a Linux
# host without an Apple SDK. Type-checks only (no linking, no C compile):
# a stub C compiler emits empty Mach-O objects for the C build scripts
# (QuickJS, the stb shim, shaderc-sys' link-cplusplus probe) so cargo can
# reach the Rust code. `rustup target add aarch64-apple-darwin` first.
set -eu
here="$(cd "$(dirname "$0")" && pwd -P)"
root="$(cd "$here/../.." && pwd -P)"
stub="$(mktemp -d)/fake-darwin-cc"
cat > "$stub" <<'STUB'
#!/bin/sh
out=""; compile=0
for a in "$@"; do case "$a" in -c) compile=1;; esac; done
prev=""
for a in "$@"; do if [ "$prev" = "-o" ]; then out="$a"; fi; prev="$a"; done
if [ "$compile" = 1 ] && [ -n "$out" ]; then
  exec clang --target=arm64-apple-darwin -c -x c /dev/null -o "$out"
fi
exec clang --target=arm64-apple-darwin "$@"
STUB
chmod +x "$stub"
export CARGO_TARGET_DIR="${CARGO_TARGET_DIR:-$root/target-mac}"
export CC_aarch64_apple_darwin="$stub" CXX_aarch64_apple_darwin="$stub"
export AR_aarch64_apple_darwin="$(command -v ar)" PKG_CONFIG_ALLOW_CROSS=1
cd "$root"
exec cargo check --workspace --all-targets --target aarch64-apple-darwin "$@"
