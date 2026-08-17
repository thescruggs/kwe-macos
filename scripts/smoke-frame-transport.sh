#!/usr/bin/env bash
# SPDX-License-Identifier: Apache-2.0
set -euo pipefail

project_root="$(cd -- "$(dirname -- "${BASH_SOURCE[0]}")/.." && pwd)"
smoke_root="$(mktemp -d -t kwe-frame-smoke.XXXXXX)"
renderer_pid=""
preview_pid=""

cleanup_renderer() {
    if [[ -n "$renderer_pid" ]]; then
        kill "$renderer_pid" 2>/dev/null || true
        wait "$renderer_pid" 2>/dev/null || true
        renderer_pid=""
    fi
}
cleanup() {
    if [[ -n "$preview_pid" ]]; then
        kill "$preview_pid" 2>/dev/null || true
        wait "$preview_pid" 2>/dev/null || true
        preview_pid=""
    fi
    cleanup_renderer
    rm -rf -- "$smoke_root"
}
trap cleanup EXIT INT TERM

run_case() {
    local name="$1"
    local expected="$2"
    local timeout="$3"
    shift 3
    local frame_file="$smoke_root/$name.bin"

    target/debug/kwe-test-renderer \
        --output "$frame_file" --width 320 --height 180 --fps 30 "$@" &
    renderer_pid=$!
    for _attempt in {1..100}; do
        [[ -f "$frame_file" ]] && break
        kill -0 "$renderer_pid" 2>/dev/null || {
            wait "$renderer_pid"
            exit 1
        }
        sleep 0.02
    done
    build/cmake/apps/kwe-frame-preview/kwe-frame-preview \
        --platform offscreen \
        --frame-file "$frame_file" \
        --smoke-test-ms "$timeout" \
        --expect-status "$expected"
    cleanup_renderer
    echo "frame transport case passed: $name ($expected)"
}

run_truncate_case() {
    local frame_file="$smoke_root/truncate.bin"
    target/debug/kwe-test-renderer \
        --output "$frame_file" --width 320 --height 180 --fps 30 &
    renderer_pid=$!
    for _attempt in {1..100}; do
        [[ -f "$frame_file" ]] && break
        kill -0 "$renderer_pid" 2>/dev/null || {
            wait "$renderer_pid"
            exit 1
        }
        sleep 0.02
    done
    build/cmake/apps/kwe-frame-preview/kwe-frame-preview \
        --platform offscreen \
        --frame-file "$frame_file" \
        --smoke-test-ms 2500 \
        --expect-status invalid &
    preview_pid=$!
    sleep 0.6
    truncate -s 64 "$frame_file"
    wait "$preview_pid"
    preview_pid=""
    cleanup_renderer
    echo "frame transport case passed: truncate (invalid, no mapped-file fault)"
}

cd "$project_root"
run_case live live 1000
run_case hang frozen 2500 --hang-after 10
run_case corrupt invalid 5000 --corrupt-after 120
run_case exit frozen 6000 --exit-after 120
run_truncate_case
