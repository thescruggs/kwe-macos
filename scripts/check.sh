#!/usr/bin/env bash
# SPDX-License-Identifier: Apache-2.0
set -euo pipefail

project_root="$(cd -- "$(dirname -- "${BASH_SOURCE[0]}")/.." && pwd)"
cd "$project_root"

cargo fmt --all -- --check
cargo clippy --workspace --all-targets -- -D warnings
cargo test --workspace --all-targets
cargo build --workspace
cmake -S . -B build/cmake -G Ninja -DCMAKE_BUILD_TYPE=Debug
cmake --build build/cmake --parallel
if command -v qmllint >/dev/null 2>&1; then
    qmllint -I /usr/lib/qt6/qml -I build/cmake/apps/kwe-manager apps/kwe-manager/qml/Main.qml
    qmllint -I /usr/lib/qt6/qml -I build/cmake/apps/kwe-frame-preview apps/kwe-frame-preview/qml/Preview.qml
fi
target/debug/kwe diagnose
target/debug/kwe-vulkan --json

if [[ "${KWE_RUN_UI_SMOKE:-0}" == "1" ]]; then
    scripts/smoke-ui.sh
fi
if [[ "${KWE_RUN_FRAME_SMOKE:-0}" == "1" ]]; then
    scripts/smoke-frame-transport.sh
fi
if [[ "${KWE_RUN_SUPERVISOR_SMOKE:-0}" == "1" ]]; then
    scripts/smoke-supervisor.sh
fi
if [[ "${KWE_RUN_INPUT_PREVIEW_SMOKE:-0}" == "1" ]]; then
    scripts/smoke-input-preview.sh
fi
if [[ "${KWE_RUN_PLASMA_DISPLAY_SMOKE:-0}" == "1" ]]; then
    scripts/smoke-plasma-display.sh
fi
if [[ "${KWE_RUN_PLAYLIST_SMOKE:-0}" == "1" ]]; then
    scripts/smoke-playlist-restart.sh
fi
if [[ "${KWE_RUN_WORKSHOP_CACHE_SMOKE:-0}" == "1" ]]; then
    scripts/smoke-workshop-cache.sh
fi
