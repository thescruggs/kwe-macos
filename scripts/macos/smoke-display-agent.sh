#!/usr/bin/env bash
# SPDX-License-Identifier: GPL-3.0-or-later
# End-to-end smoke of the desktop display agent against a real daemon and
# the generated test renderer, with no desktop involvement (offscreen Qt).
# Runs on Linux (windowed dev build) and macOS alike:
#
#   scripts/macos/smoke-display-agent.sh [build-dir]   (default build/agent)
#
# Proves: the agent follows renderer.status, opens and validates the frame
# file, acknowledges the display generation, and shows a frame.
set -euo pipefail

project_root="$(cd -- "$(dirname -- "${BASH_SOURCE[0]}")/../.." && pwd)"
build_dir="${1:-$project_root/build/agent}"
agent="$build_dir/apps/kwe-display-macos/kwe-display-macos"
smoke_root="$(mktemp -d -t kwe-display-agent-smoke.XXXXXX)"
socket="$smoke_root/daemon.sock"
daemon_pid=""

cleanup() {
    if [[ -n "$daemon_pid" ]]; then
        kill "$daemon_pid" 2>/dev/null || true
        wait "$daemon_pid" 2>/dev/null || true
    fi
    rm -rf -- "$smoke_root"
}
trap cleanup EXIT INT TERM

[[ -x "$agent" ]] || { echo "missing $agent (cmake --build $build_dir)" >&2; exit 1; }
cd "$project_root"
cargo build -q -p kwe-daemon -p kwe-test-renderer -p kwe-cli

call_daemon() {
    local params="{}"
    (( $# >= 2 )) && params="$2"
    target/debug/kwe daemon-call --socket "$socket" --method "$1" --params "$params"
}
wait_for_phase() {
    local expected="$1" status=""
    for _attempt in {1..250}; do
        status="$(call_daemon renderer.status)"
        if [[ "$(jq -r '.result.phase' <<<"$status")" == "$expected" ]]; then
            printf '%s\n' "$status"; return 0
        fi
        sleep 0.02
    done
    echo "renderer never reached $expected: $status" >&2
    return 1
}

target/debug/kwe-daemon \
    --socket "$socket" \
    --renderer "$project_root/target/debug/kwe-test-renderer" \
    --renderer-runtime-dir "$smoke_root/runtime" \
    --state-dir "$smoke_root/state" \
    --renderer-canary-ms 150 \
    --renderer-handoff-timeout-ms 5000 >"$smoke_root/daemon.log" 2>&1 &
daemon_pid=$!
for _attempt in {1..100}; do
    [[ -S "$socket" ]] && break
    kill -0 "$daemon_pid" 2>/dev/null || { sed -n '1,80p' "$smoke_root/daemon.log" >&2; exit 1; }
    sleep 0.02
done
[[ -S "$socket" ]]

call_daemon renderer.start '{"wallpaper_id":"agent-smoke","content_hash":"hash-agent-smoke","width":320,"height":180,"fps":30}' >/dev/null
wait_for_phase live >/dev/null

QT_FORCE_STDERR_LOGGING=1 QT_QPA_PLATFORM=offscreen "$agent" \
    --windowed --cover-all \
    --daemon-socket "$socket" \
    --exit-after-ms 2500 \
    --expect-frame \
    --screenshot "$smoke_root/agent.png" 2>&1 | tee "$smoke_root/agent.log"
[[ "${PIPESTATUS[0]}" == 0 ]]
grep -q 'hasFrame=1' "$smoke_root/agent.log"
[[ -s "$smoke_root/agent.png" ]]

status="$(call_daemon renderer.status)"
[[ "$(jq -r '.result.awaiting_display_ack' <<<"$status")" == "false" ]]
call_daemon renderer.stop >/dev/null
echo "display agent smoke passed: frame shown, display generation acknowledged"
