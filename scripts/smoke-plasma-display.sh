#!/usr/bin/env bash
# SPDX-License-Identifier: Apache-2.0
set -euo pipefail

project_root="$(cd -- "$(dirname -- "${BASH_SOURCE[0]}")/.." && pwd)"
smoke_root="$(mktemp -d -t kwe-plasma-display-smoke.XXXXXX)"
socket="$smoke_root/daemon.sock"
runtime_dir="$smoke_root/runtime"
state_dir="$smoke_root/state"
stage_dir="$smoke_root/stage"
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

wait_for_phase() {
    local expected="$1"
    local status=""
    for _attempt in {1..250}; do
        status="$(call_daemon renderer.status)"
        if [[ "$(jq -r '.result.phase' <<<"$status")" == "$expected" ]]; then
            printf '%s\n' "$status"
            return 0
        fi
        sleep 0.02
    done
    printf '%s\n' "$status" >&2
    return 1
}

cd "$project_root"
command -v jq >/dev/null
command -v qmllint >/dev/null
command -v kpackagetool6 >/dev/null
[[ -x /usr/lib/qt6/bin/qmlscene ]]
cargo build --workspace >/dev/null
cmake -S . -B build/cmake -G Ninja -DCMAKE_BUILD_TYPE=Debug >/dev/null
cmake --build build/cmake --parallel >/dev/null

cmake --install build/cmake --prefix "$stage_dir" >/dev/null
package="$stage_dir/share/plasma/wallpapers/org.kde.kwe.wallpaper"
module="$stage_dir/lib/qt6/qml/org/kde/kwe/display"
[[ -f "$package/metadata.json" ]]
[[ -f "$package/contents/ui/main.qml" ]]
[[ -f "$module/qmldir" ]]
[[ -f "$module/kwe_display.qmltypes" ]]
[[ -f "$module/libkwe_displayplugin.so" ]]
[[ "$(jq -r '.KPackageStructure' "$package/metadata.json")" == "Plasma/Wallpaper" ]]
[[ "$(jq -r '.KPlugin.Id' "$package/metadata.json")" == "org.kde.kwe.wallpaper" ]]
qmllint -I /usr/lib/qt6/qml -I "$stage_dir/lib/qt6/qml" \
    "$package/contents/ui/main.qml"
LD_LIBRARY_PATH="$stage_dir/lib${LD_LIBRARY_PATH:+:$LD_LIBRARY_PATH}" \
    QT_QPA_PLATFORM=offscreen /usr/lib/qt6/bin/qmlscene \
    -I "$stage_dir/lib/qt6/qml" \
    "$project_root/modules/org/kde/kwe/display/tests/ModuleLoad.qml"
kpackagetool6 --hash "$package" >/dev/null

target/debug/kwe-daemon \
    --socket "$socket" \
    --renderer "$project_root/target/debug/kwe-test-renderer" \
    --renderer-runtime-dir "$runtime_dir" \
    --state-dir "$state_dir" \
    --renderer-canary-ms 150 \
    --renderer-handoff-timeout-ms 5000 >"$smoke_root/daemon.log" 2>&1 &
daemon_pid=$!
for _attempt in {1..100}; do
    [[ -S "$socket" ]] && break
    kill -0 "$daemon_pid" 2>/dev/null || {
        sed -n '1,160p' "$smoke_root/daemon.log" >&2
        exit 1
    }
    sleep 0.02
done
[[ -S "$socket" ]]

first='{"wallpaper_id":"plasma-display-a","content_hash":"hash-plasma-display-a","width":320,"height":180,"fps":30}'
second='{"wallpaper_id":"plasma-display-b","content_hash":"hash-plasma-display-b","width":320,"height":180,"fps":30}'
call_daemon renderer.start "$first" >/dev/null
wait_for_phase live >/dev/null
call_daemon renderer.start "$second" >/dev/null
handoff="$(wait_for_phase awaiting_ack)"
[[ "$(jq -r '.result.awaiting_display_ack' <<<"$handoff")" == "true" ]]

build/cmake/apps/kwe-frame-preview/kwe-frame-preview \
    --platform offscreen \
    --follow-daemon \
    --daemon-socket "$socket" \
    --smoke-pointer \
    --smoke-test-ms 2200 \
    --expect-status live

status="$(wait_for_phase live)"
[[ "$(jq -r '.result.wallpaper_id' <<<"$status")" == "plasma-display-b" ]]
[[ "$(jq -r '.result.awaiting_display_ack' <<<"$status")" == "false" ]]
[[ "$(jq -r '.result.input_sequence' <<<"$status")" -ge 1 ]]
[[ "$(jq -r '.result.input_ack_sequence' <<<"$status")" -ge 1 ]]
call_daemon renderer.stop >/dev/null

echo "Plasma display module, staged package, handoff ack, and passive input passed"
