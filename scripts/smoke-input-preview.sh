#!/usr/bin/env bash
# SPDX-License-Identifier: Apache-2.0
set -euo pipefail

project_root="$(cd -- "$(dirname -- "${BASH_SOURCE[0]}")/.." && pwd)"
smoke_root="$(mktemp -d -t kwe-input-preview-smoke.XXXXXX)"
socket="$smoke_root/daemon.sock"
runtime_dir="$smoke_root/runtime"
state_dir="$smoke_root/state"
daemon_pid=""

cleanup() {
    if [[ -n "$daemon_pid" ]]; then
        kill "$daemon_pid" 2>/dev/null || true
        wait "$daemon_pid" 2>/dev/null || true
    fi
    rm -rf -- "$smoke_root"
}
trap cleanup EXIT INT TERM

call_daemon() {
    local params="{}"
    if (( $# >= 2 )); then
        params="$2"
    fi
    target/debug/kwe daemon-call --socket "$socket" --method "$1" --params "$params"
}

cd "$project_root"
command -v jq >/dev/null
cargo build --workspace >/dev/null
cmake -S . -B build/cmake -G Ninja -DCMAKE_BUILD_TYPE=Debug >/dev/null
cmake --build build/cmake --parallel >/dev/null

target/debug/kwe-daemon \
    --socket "$socket" \
    --renderer "$project_root/target/debug/kwe-test-renderer" \
    --renderer-runtime-dir "$runtime_dir" \
    --state-dir "$state_dir" \
    --renderer-canary-ms 150 \
    --renderer-handoff-timeout-ms 1000 >"$smoke_root/daemon.log" 2>&1 &
daemon_pid=$!
for _attempt in {1..100}; do
    [[ -S "$socket" ]] && break
    kill -0 "$daemon_pid" 2>/dev/null || {
        sed -n '1,120p' "$smoke_root/daemon.log" >&2
        exit 1
    }
    sleep 0.02
done
[[ -S "$socket" ]]

params='{"wallpaper_id":"qt-input-preview","content_hash":"hash-qt-input-preview","width":320,"height":180,"fps":30}'
call_daemon renderer.start "$params" >/dev/null
status=""
for _attempt in {1..200}; do
    status="$(call_daemon renderer.status)"
    [[ "$(jq -r '.result.phase' <<<"$status")" == "live" ]] && break
    sleep 0.02
done
[[ "$(jq -r '.result.phase' <<<"$status")" == "live" ]]
frame_file="$(jq -r '.result.frame_file' <<<"$status")"
generation="$(jq -r '.result.display_generation' <<<"$status")"

build/cmake/apps/kwe-frame-preview/kwe-frame-preview \
    --platform offscreen \
    --frame-file "$frame_file" \
    --daemon-socket "$socket" \
    --display-generation "$generation" \
    --smoke-pointer \
    --smoke-test-ms 1500 \
    --expect-status live

status="$(call_daemon renderer.status)"
[[ "$(jq -r '.result.input_sequence' <<<"$status")" -ge 1 ]]
[[ "$(jq -r '.result.input_ack_sequence' <<<"$status")" -ge 1 ]]
call_daemon renderer.stop >/dev/null
echo "Qt passive pointer preview integration passed"
