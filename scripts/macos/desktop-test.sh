#!/usr/bin/env bash
# SPDX-License-Identifier: GPL-3.0-or-later
# The first thing to run on a real Mac after packaging/macos/install-dev.sh:
# shows the generated test pattern UNDER the desktop icons of every screen
# for a while, then puts everything back. Nothing is applied or persisted.
#
#   scripts/macos/desktop-test.sh [seconds]      (default 20)
#
# What you should see: an animated test pattern behind the Finder icons on
# every display; the pointer position should be reflected by the pattern
# (passive input). Menu bar, Dock, icons and windows stay usable. When the
# script ends the normal desktop picture is back.
set -euo pipefail
seconds="${1:-20}"
root="$(cd -- "$(dirname -- "${BASH_SOURCE[0]}")/../.." && pwd)"
bin="$root/target/release"
agent="$root/build/agent/apps/kwe-display-macos/kwe-display-macos"
sock="$HOME/Library/Application Support/kwe/daemon-v1.sock"
[[ -x "$bin/kwe" && -x "$bin/kwe-daemon" ]] || { echo "build first: packaging/macos/install-dev.sh" >&2; exit 1; }
[[ -x "$agent" ]] || { echo "missing $agent (cmake build; see docs/macos/TOOLCHAIN.md)" >&2; exit 1; }
[[ -S "$sock" ]] || { echo "daemon socket missing at $sock; run packaging/macos/install-dev.sh" >&2; exit 1; }

call() { "$bin/kwe" daemon-call --socket "$sock" --method "$1" --params "${2:-{\}}"; }
cleanup() { call renderer.stop >/dev/null 2>&1 || true; }
trap cleanup EXIT INT TERM

# Size the renderer to the main display (logical points).
read -r w h < <(call wallpaper.outputs | jq -r '.result.outputs[0].geometry | "\(.[2]) \(.[3])"' 2>/dev/null || echo "1920 1080")
[[ "$w" =~ ^[0-9]+$ && "$h" =~ ^[0-9]+$ ]] || { w=1920; h=1080; }
echo "starting the test renderer at ${w}x${h}"
call renderer.start "{\"wallpaper_id\":\"desktop-test\",\"content_hash\":\"desktop-test\",\"width\":$w,\"height\":$h,\"fps\":30}" >/dev/null
for _ in $(seq 1 100); do
  phase="$(call renderer.status | jq -r '.result.phase')"
  [[ "$phase" == "live" ]] && break
  sleep 0.1
done
echo "renderer phase: $phase — covering every screen for ${seconds}s (Ctrl-C to stop early)"
"$agent" --cover-all --exit-after-ms "$((seconds * 1000))" --expect-frame
echo "done; desktop restored (renderer stopped)"
