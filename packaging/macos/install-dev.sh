#!/bin/sh
# Developer install on macOS: the daemon (and, when built, the desktop
# display agent) as user LaunchAgents pointing at this checkout's build
# outputs. Nothing is copied: rebuild, then
#   launchctl kickstart -k gui/$(id -u)/org.kde.kwe.daemon
#   launchctl kickstart -k gui/$(id -u)/org.kde.kwe.display
#
#   packaging/macos/install-dev.sh              build Rust + Qt, install, start
#   packaging/macos/install-dev.sh --no-build   install + start only
#   packaging/macos/install-dev.sh --no-agent   daemon only
#
# Undo with packaging/macos/uninstall-dev.sh. The macOS desktop picture is
# never touched; the agent draws under Finder's icons and disappears when
# unloaded.
set -eu
here="$(cd "$(dirname "$0")" && pwd -P)"
root="$(cd "$here/../.." && pwd -P)"
case "$(uname -s)" in Darwin) ;; *) echo "macOS only" >&2; exit 2;; esac

build=1; agent=1
for arg in "$@"; do
  case "$arg" in
    --no-build) build=0 ;;
    --no-agent) agent=0 ;;
    *) echo "unknown option $arg" >&2; exit 2 ;;
  esac
done

brew_prefix="$(brew --prefix 2>/dev/null || echo /opt/homebrew)"
export PKG_CONFIG_PATH="$brew_prefix/opt/shaderc/lib/pkgconfig:${PKG_CONFIG_PATH:-}"

if [ "$build" = 1 ]; then
  (cd "$root" && cargo build --workspace --release)
  if [ "$agent" = 1 ]; then
    qt_prefix="$(brew --prefix qt@6 2>/dev/null || brew --prefix qt 2>/dev/null || true)"
    if [ -n "$qt_prefix" ]; then
      (cd "$root" && cmake -S . -B build/agent -G Ninja -DCMAKE_BUILD_TYPE=Release \
         -DCMAKE_PREFIX_PATH="$qt_prefix" -DBUILD_TESTING=OFF \
       && cmake --build build/agent --parallel)
    else
      echo "warning: Homebrew qt@6 not found; skipping the display agent and manager (brew install qt@6)" >&2
      agent=0
    fi
  fi
fi

bin_dir="$root/target/release"
[ -x "$bin_dir/kwe-daemon" ] || { echo "missing $bin_dir/kwe-daemon (build first)" >&2; exit 1; }
agent_bin="$root/build/agent/apps/kwe-display-macos/kwe-display-macos"
manager_bin="$root/build/agent/apps/kwe-manager/kwe-manager"
# MoltenVK's ICD manifest: Homebrew has moved it between share/ and etc/;
# search the usual places (following the opt/ symlink) before giving up.
icd=""
for candidate in \
  "$(brew --prefix molten-vk 2>/dev/null || echo "$brew_prefix/opt/molten-vk")/share/vulkan/icd.d/MoltenVK_icd.json" \
  "$(brew --prefix molten-vk 2>/dev/null || echo "$brew_prefix/opt/molten-vk")/etc/vulkan/icd.d/MoltenVK_icd.json" \
  "$brew_prefix/share/vulkan/icd.d/MoltenVK_icd.json" \
  "$brew_prefix/etc/vulkan/icd.d/MoltenVK_icd.json"; do
  if [ -f "$candidate" ]; then icd="$candidate"; break; fi
done
if [ -z "$icd" ]; then
  icd="$(find -L "$brew_prefix/opt/molten-vk" -name 'MoltenVK_icd.json' -print 2>/dev/null | head -1)"
fi
if [ -z "$icd" ]; then
  echo "warning: MoltenVK ICD manifest not found (brew install molten-vk); scene wallpapers will not work" >&2
  icd="$brew_prefix/share/vulkan/icd.d/MoltenVK_icd.json"
else
  echo "MoltenVK ICD: $icd"
fi

app_support="$HOME/Library/Application Support/kwe"
log_dir="$HOME/Library/Logs/kwe"
agents="$HOME/Library/LaunchAgents"
mkdir -p "$app_support" "$log_dir" "$agents"
uid="$(id -u)"

# The manager looks for the agent beside itself or on PATH; a symlink in the
# release dir covers both launch styles.
if [ -x "$agent_bin" ]; then
  ln -sfn "$agent_bin" "$bin_dir/kwe-display-macos"
  ln -sfn "$manager_bin" "$bin_dir/kwe-manager" 2>/dev/null || true
fi

daemon_plist="$agents/org.kde.kwe.daemon.plist"
sed -e "s|@KWE_BIN_DIR@|$bin_dir|g" \
    -e "s|@BREW_PREFIX@|$brew_prefix|g" \
    -e "s|@MOLTENVK_ICD@|$icd|g" \
    -e "s|@KWE_LOG_DIR@|$log_dir|g" \
    "$here/org.kde.kwe.daemon.plist.in" > "$daemon_plist"
plutil -lint "$daemon_plist" >/dev/null
launchctl bootout "gui/$uid/org.kde.kwe.daemon" 2>/dev/null || true
launchctl bootstrap "gui/$uid" "$daemon_plist"
launchctl kickstart -k "gui/$uid/org.kde.kwe.daemon"

sock="$app_support/daemon-v1.sock"
i=0
while [ ! -S "$sock" ] && [ $i -lt 50 ]; do sleep 0.1; i=$((i+1)); done
if [ -S "$sock" ]; then
  echo "kwe-daemon running; socket $sock; log $log_dir/kwe-daemon.log"
else
  echo "daemon did not create $sock; check $log_dir/kwe-daemon.log" >&2
  exit 1
fi

if [ "$agent" = 1 ] && [ -x "$agent_bin" ]; then
  agent_plist="$agents/org.kde.kwe.display.plist"
  sed -e "s|@KWE_AGENT@|$agent_bin|g" \
      -e "s|@BREW_PREFIX@|$brew_prefix|g" \
      -e "s|@KWE_LOG_DIR@|$log_dir|g" \
      "$here/org.kde.kwe.display.plist.in" > "$agent_plist"
  plutil -lint "$agent_plist" >/dev/null
  launchctl bootout "gui/$uid/org.kde.kwe.display" 2>/dev/null || true
  launchctl bootstrap "gui/$uid" "$agent_plist"
  launchctl kickstart -k "gui/$uid/org.kde.kwe.display"
  echo "kwe-display-macos running; log $log_dir/kwe-display.log"
  [ -x "$manager_bin" ] && echo "manager: $manager_bin"
fi
echo "quick test: $bin_dir/kwe scan   |   $bin_dir/kwe daemon-call --method wallpaper.outputs"
