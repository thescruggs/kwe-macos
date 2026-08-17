#!/usr/bin/env bash
# SPDX-License-Identifier: Apache-2.0
set -euo pipefail

project_root="$(cd -- "$(dirname -- "${BASH_SOURCE[0]}")/.." && pwd)"
runtime_parent="${XDG_RUNTIME_DIR:-/tmp}"
demo_root="$(mktemp -d -p "$runtime_parent" kwe-frame-demo.XXXXXX)"
frame_file="$demo_root/frame-v1.bin"
renderer_pid=""

cleanup() {
    if [[ -n "$renderer_pid" ]]; then
        kill "$renderer_pid" 2>/dev/null || true
        wait "$renderer_pid" 2>/dev/null || true
    fi
    rm -rf -- "$demo_root"
}
trap cleanup EXIT INT TERM

cd "$project_root"
cargo build -p kwe-test-renderer
cmake -S . -B build/cmake -G Ninja -DCMAKE_BUILD_TYPE=Debug
cmake --build build/cmake --parallel

renderer_args=(--output "$frame_file" --width 960 --height 540 --fps 30)
case "${KWE_FRAME_FAULT:-live}" in
    live) ;;
    hang) renderer_args+=(--hang-after 120) ;;
    corrupt) renderer_args+=(--corrupt-after 120) ;;
    exit) renderer_args+=(--exit-after 120) ;;
    *) echo "KWE_FRAME_FAULT must be live, hang, corrupt, or exit" >&2; exit 2 ;;
esac

target/debug/kwe-test-renderer "${renderer_args[@]}" &
renderer_pid=$!
for _attempt in {1..100}; do
    [[ -f "$frame_file" ]] && break
    kill -0 "$renderer_pid" 2>/dev/null || {
        wait "$renderer_pid"
        exit 1
    }
    sleep 0.02
done

build/cmake/apps/kwe-frame-preview/kwe-frame-preview --frame-file "$frame_file"

