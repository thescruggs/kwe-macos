#!/usr/bin/env bash
# SPDX-License-Identifier: Apache-2.0
set -euo pipefail

project_root="$(cd -- "$(dirname -- "${BASH_SOURCE[0]}")/.." && pwd)"
smoke_root="$(mktemp -d -t kwe-smoke.XXXXXX)"
socket_path="$smoke_root/daemon.sock"
daemon_pid=""
cleanup() {
    if [[ -n "$daemon_pid" ]]; then
        kill "$daemon_pid" 2>/dev/null || true
        wait "$daemon_pid" 2>/dev/null || true
    fi
    rm -rf -- "$smoke_root"
}
trap cleanup EXIT INT TERM

cd "$project_root"
target/debug/kwe-daemon --socket "$socket_path" &
daemon_pid=$!
for _attempt in {1..50}; do
    [[ -S "$socket_path" ]] && break
    kill -0 "$daemon_pid" 2>/dev/null || {
        wait "$daemon_pid"
        exit 1
    }
    sleep 0.05
done

build/cmake/apps/kwe-manager/kwe-manager \
    --platform offscreen \
    --socket "$socket_path" \
    --smoke-test-ms 3000

