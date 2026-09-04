#!/bin/sh
# Developer install of the kwe daemon as a user LaunchAgent, pointing at the
# binaries in this checkout's target/release (no copying: rebuild, then
# `launchctl kickstart -k gui/$(id -u)/org.kde.kwe.daemon`).
#
#   packaging/macos/install-dev.sh            build + install + start
#   packaging/macos/install-dev.sh --no-build install + start only
#
# Undo with packaging/macos/uninstall-dev.sh. Nothing touches the macOS
# desktop picture; the display agent (apps/kwe-display-macos) is separate.
set -eu
here="$(cd "$(dirname "$0")" && pwd -P)"
root="$(cd "$here/../.." && pwd -P)"
case "$(uname -s)" in Darwin) ;; *) echo "macOS only" >&2; exit 2;; esac

if [ "${1:-}" != "--no-build" ]; then
  (cd "$root" && cargo build --workspace --release)
fi

bin_dir="$root/target/release"
[ -x "$bin_dir/kwe-daemon" ] || { echo "missing $bin_dir/kwe-daemon (build first)" >&2; exit 1; }
brew_prefix="$(brew --prefix 2>/dev/null || echo /opt/homebrew)"
icd="$(brew --prefix molten-vk 2>/dev/null || echo "$brew_prefix/opt/molten-vk")/share/vulkan/icd.d/MoltenVK_icd.json"
[ -f "$icd" ] || echo "warning: MoltenVK ICD not found at $icd (brew install molten-vk); scene wallpapers will not work" >&2

app_support="$HOME/Library/Application Support/kwe"
log_dir="$HOME/Library/Logs/kwe"
agents="$HOME/Library/LaunchAgents"
mkdir -p "$app_support" "$log_dir" "$agents"
plist="$agents/org.kde.kwe.daemon.plist"

sed -e "s|@KWE_BIN_DIR@|$bin_dir|g" \
    -e "s|@BREW_PREFIX@|$brew_prefix|g" \
    -e "s|@MOLTENVK_ICD@|$icd|g" \
    -e "s|@KWE_LOG_DIR@|$log_dir|g" \
    "$here/org.kde.kwe.daemon.plist.in" > "$plist"
plutil -lint "$plist" >/dev/null

uid="$(id -u)"
launchctl bootout "gui/$uid/org.kde.kwe.daemon" 2>/dev/null || true
launchctl bootstrap "gui/$uid" "$plist"
launchctl kickstart -k "gui/$uid/org.kde.kwe.daemon"
sleep 1
sock="$app_support/daemon-v1.sock"
if [ -S "$sock" ]; then
  echo "kwe-daemon running; socket $sock"
  echo "log: $log_dir/kwe-daemon.log"
else
  echo "daemon did not create $sock yet; check $log_dir/kwe-daemon.log" >&2
  exit 1
fi
