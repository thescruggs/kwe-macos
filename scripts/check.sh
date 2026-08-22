#!/usr/bin/env bash
# SPDX-License-Identifier: Apache-2.0
set -euo pipefail

project_root="$(cd -- "$(dirname -- "${BASH_SOURCE[0]}")/.." && pwd)"
cd "$project_root"

# The half of BETA B1 that no runtime test covers: the unit must stay part of
# the graphical session, or the daemon goes back to starting before any
# display exists (docs/bugs/OUTPUTS_EMPTY_AFTER_REBOOT.md).
unit_file="packaging/systemd/kwe-daemon.service"
for directive in "PartOf=graphical-session.target" \
                 "After=graphical-session.target" \
                 "WantedBy=graphical-session.target"; do
    if ! grep -qx -- "$directive" "$unit_file"; then
        echo "FAILED: $unit_file is missing '$directive'" >&2
        echo "        (BETA B1: the daemon must start with the graphical session)" >&2
        exit 1
    fi
done
if grep -qx -- "WantedBy=default.target" "$unit_file"; then
    echo "FAILED: $unit_file installs into default.target again (BETA B1)" >&2
    exit 1
fi

cargo fmt --all -- --check
cargo clippy --workspace --all-targets -- -D warnings
cargo test --workspace --all-targets
cargo build --workspace
cmake -S . -B build/cmake -G Ninja -DCMAKE_BUILD_TYPE=Debug
cmake --build build/cmake --parallel
# Resolves the Qt 6 qmllint itself (a bare `qmllint` is Qt 5's on this distro
# and passes everything), asserts the QML module really registers its C++
# types, and fails on any unresolved type.
scripts/qml-typecheck.sh
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
# Destructive live-session smoke (BETA_M4d): authorized on this machine only.
if [[ "${KWE_RUN_LIVE_APPLY_SMOKE:-0}" == "1" ]]; then
    scripts/smoke-live-apply.sh
fi
