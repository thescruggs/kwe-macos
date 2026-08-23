#!/usr/bin/env bash
# SPDX-License-Identifier: GPL-3.0-or-later
set -euo pipefail

project_root="$(cd -- "$(dirname -- "${BASH_SOURCE[0]}")/.." && pwd)"
runtime_root="${XDG_RUNTIME_DIR:?XDG_RUNTIME_DIR must be set}/kwe-alpha"
socket_path="$runtime_root/daemon-v1.sock"

cd "$project_root"
cargo build --workspace
cmake -S . -B build/cmake -G Ninja -DCMAKE_BUILD_TYPE=Debug
cmake --build build/cmake --parallel
install -d -m 700 "$runtime_root"

target/debug/kwe-daemon --socket "$socket_path" &
daemon_pid=$!
cleanup() {
    kill "$daemon_pid" 2>/dev/null || true
    wait "$daemon_pid" 2>/dev/null || true
}
trap cleanup EXIT INT TERM

for _attempt in {1..50}; do
    [[ -S "$socket_path" ]] && break
    kill -0 "$daemon_pid" 2>/dev/null || {
        wait "$daemon_pid"
        exit 1
    }
    sleep 0.05
done

build/cmake/apps/kwe-manager/kwe-manager --socket "$socket_path"

