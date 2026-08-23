#!/usr/bin/env bash
# SPDX-License-Identifier: GPL-3.0-or-later
#
# smoke-apply.sh: daemon live-apply transaction smoke (BETA_M4a).
#
# Everything here is read-only against the live Plasma session:
#   - wallpaper.outputs enumerates the real outputs (kscreen-doctor + one
#     read-only evaluateScript probe) and prints them for the record;
#   - wallpaper.apply with an unknown wallpaper id fails before any probe;
#   - wallpaper.apply to a nonexistent output runs only the read-only
#     enumeration probe and fails with output_missing;
#   - a seeded assignments-v1.json round-trips through wallpaper.assignments;
#   - wallpaper.restore on the seeded (nonexistent) output fails with
#     output_missing.
#
# The restore-to-image path (the safe-mode fallback that switches a real
# output to org.kde.image) is deliberately NOT executed here: it writes
# live wallpaper config, which is BETA_M4d territory. The contract is
# documented in docs/BETA_M4.md.
#
# Gated behind KWE_LIVE_APPLY=1; without it the script exits 0 with a
# SKIPPED note so acceptance runs stay green. BETA_M4d decision: this lane
# stays the READ-ONLY live lane (enumeration + fail-closed error cases);
# the DESTRUCTIVE live smoke (apply/rollback/safe-mode on the real session)
# lives in scripts/smoke-live-apply.sh under KWE_RUN_LIVE_APPLY_SMOKE=1.
set -euo pipefail

project_root="$(cd -- "$(dirname -- "${BASH_SOURCE[0]}")/.." && pwd)"
# Honor an external CARGO_TARGET_DIR (acceptance reuses the shared dep build).
target_dir="${CARGO_TARGET_DIR:-$project_root/target}"

if [[ "${KWE_LIVE_APPLY:-0}" != "1" ]]; then
    echo "SKIPPED: smoke-apply.sh needs KWE_LIVE_APPLY=1 (flipped on by BETA_M4d);"
    echo "        this run is read-only against the live Plasma session"
    exit 0
fi

smoke_root="$(mktemp -d -t kwe-apply-smoke.XXXXXX)"
socket="$smoke_root/daemon.sock"
runtime_dir="$smoke_root/runtime"
state_dir="$smoke_root/state"
steam_root="$smoke_root/steam"
# The runnable scene.json lives INSIDE the fixture content root: the
# renderer runs the catalog content, and a supplied content must match it
# (BETA_M4a review fix 5).
scene_json="$steam_root/steamapps/workshop/content/431960/1/scene.json"
daemon_pid=""
stripped_pid=""

cleanup() {
    if [[ -n "$stripped_pid" ]]; then
        kill "$stripped_pid" 2>/dev/null || true
        wait "$stripped_pid" 2>/dev/null || true
    fi
    if [[ -n "$daemon_pid" ]]; then
        kill "$daemon_pid" 2>/dev/null || true
        wait "$daemon_pid" 2>/dev/null || true
    fi
    rm -rf -- "$smoke_root"
}
trap cleanup EXIT INT TERM

call_daemon() {
    local method="$1"
    local params="{}"
    if (( $# >= 2 )); then
        params="$2"
    fi
    "$target_dir/debug/kwe" daemon-call --socket "$socket" --method "$method" --params "$params"
}

start_daemon() {
    STEAM_ROOT="$steam_root" "$target_dir/debug/kwe-daemon" \
        --socket "$socket" \
        --renderer "$target_dir/debug/kwe-test-renderer" \
        --renderer-runtime-dir "$runtime_dir" \
        --state-dir "$state_dir" \
        --renderer-startup-timeout-ms 500 \
        --renderer-frame-timeout-ms 250 \
        --renderer-stop-grace-ms 80 \
        --renderer-restart-delay-ms 20 \
        --renderer-canary-ms 150 \
        --renderer-handoff-timeout-ms 1000 \
        --renderer-max-failures 3 \
        --renderer-address-space-mib 384 \
        --allow-test-faults >"$smoke_root/daemon.log" 2>&1 &
    daemon_pid=$!
    for _attempt in {1..100}; do
        [[ -S "$socket" ]] && return
        kill -0 "$daemon_pid" 2>/dev/null || {
            echo "daemon exited during startup" >&2
            sed -n '1,120p' "$smoke_root/daemon.log" >&2
            return 1
        }
        sleep 0.02
    done
    echo "daemon socket did not appear" >&2
    return 1
}

# Starts a daemon with ONLY the environment a systemd user unit provides —
# no WAYLAND_DISPLAY, no DISPLAY — which is what the boot path hands it
# (BETA B1, docs/bugs/OUTPUTS_EMPTY_AFTER_REBOOT.md). Everything else about
# the run is identical; extra arguments are appended.
start_stripped_daemon() {
    local socket_path="$1" state_path="$2" log_path="$3"
    shift 3
    mkdir -p "$state_path" "$smoke_root/stripped-runtime"
    env -i \
        HOME="$HOME" \
        PATH="$PATH" \
        XDG_RUNTIME_DIR="${XDG_RUNTIME_DIR:-/run/user/$(id -u)}" \
        DBUS_SESSION_BUS_ADDRESS="${DBUS_SESSION_BUS_ADDRESS:-unix:path=${XDG_RUNTIME_DIR:-/run/user/$(id -u)}/bus}" \
        STEAM_ROOT="$steam_root" \
        "$target_dir/debug/kwe-daemon" \
        --socket "$socket_path" \
        --renderer "$target_dir/debug/kwe-test-renderer" \
        --renderer-runtime-dir "$smoke_root/stripped-runtime" \
        --state-dir "$state_path" \
        "$@" >"$log_path" 2>&1 &
    stripped_pid=$!
    for _attempt in {1..100}; do
        [[ -S "$socket_path" ]] && return
        kill -0 "$stripped_pid" 2>/dev/null || {
            echo "stripped-environment daemon exited during startup" >&2
            sed -n '1,120p' "$log_path" >&2
            return 1
        }
        sleep 0.02
    done
    echo "stripped-environment daemon socket did not appear" >&2
    return 1
}

stop_stripped_daemon() {
    if [[ -n "$stripped_pid" ]]; then
        kill "$stripped_pid" 2>/dev/null || true
        wait "$stripped_pid" 2>/dev/null || true
        stripped_pid=""
    fi
}

command -v jq >/dev/null
cd "$project_root"
cargo build --workspace >/dev/null

# A deterministic fixture steam root: one subscribed scene project "1".
mkdir -p "$steam_root/steamapps/workshop/content/431960/1"
echo '"LibraryFolders" { }' >"$steam_root/steamapps/libraryfolders.vdf"
cat >"$steam_root/steamapps/appworkshop_431960.acf" <<'EOF'
"AppWorkshop" { "WorkshopItems" { "1" "1" } }
EOF
cat >"$steam_root/steamapps/workshop/content/431960/1/project.json" <<'EOF'
{"title":"Synthetic One","type":"scene","tags":[]}
EOF
echo '{"general":{}}' >"$scene_json"

# Seed the assignments store before the daemon reads it: one record on the
# synthetic output "Synthetic-1" (never a live output name) for the
# round-trip case.
mkdir -p "$state_dir"
cat >"$state_dir/assignments-v1.json" <<EOF
{
  "schema_version": 1,
  "outputs": {
    "Synthetic-1": {
      "wallpaper_id": "1",
      "kind": "scene",
      "content": "$scene_json",
      "width": 960,
      "height": 540,
      "fps": 30,
      "applied_at_unix_seconds": 1787188000,
      "previous": {
        "wallpaper_plugin": "org.kde.image",
        "config_group": ["Wallpaper", "org.kde.image", "General"],
        "image": "file:///usr/share/wallpapers/placeholder.png"
      }
    }
  }
}
EOF

start_daemon
call_daemon health >/dev/null
echo "apply smoke: daemon up with the seeded assignment store"

outputs="$(call_daemon wallpaper.outputs)"
if [[ "$(jq -r '.result.outputs | length' <<<"$outputs")" == "0" ]]; then
    echo "FAILED: the live enumeration returned no outputs" >&2
    exit 1
fi
jq -r '.result.outputs[] | "output " + .name + " screen=" + (.screen|tostring) + " desktop_id=" + ((.desktop_id // "none")|tostring) + " plugin=" + (.wallpaper_plugin // "none")' <<<"$outputs"
echo "apply smoke: wallpaper.outputs live enumeration passed (read-only)"

apply_params="$(jq -cn --arg content "$scene_json" \
    '{output:"DP-1",wallpaper_id:"synthetic-unknown-1",kind:"scene",content:$content,width:320,height:180,fps:30}')"
# The daemon-call CLI exits 2 on error responses; the error cases are
# exactly what this smoke verifies.
bad_id="$(call_daemon wallpaper.apply "$apply_params" || true)"
[[ "$(jq -r '.ok' <<<"$bad_id")" == "false" ]]
[[ "$(jq -r '.result.error' <<<"$bad_id")" == "apply_unknown_wallpaper" ]]
echo "apply smoke: unknown wallpaper id -> apply_unknown_wallpaper passed"

apply_params="$(jq -cn --arg content "$scene_json" \
    '{output:"Synthetic-1",wallpaper_id:"1",kind:"scene",content:$content,width:320,height:180,fps:30}')"
missing_output="$(call_daemon wallpaper.apply "$apply_params" || true)"
[[ "$(jq -r '.ok' <<<"$missing_output")" == "false" ]]
[[ "$(jq -r '.result.error' <<<"$missing_output")" == "output_missing" ]]
echo "apply smoke: apply to a nonexistent output -> output_missing passed"

assignments="$(call_daemon wallpaper.assignments)"
[[ "$(jq -r '.result.outputs["Synthetic-1"].wallpaper_id' <<<"$assignments")" == "1" ]]
[[ "$(jq -r '.result.outputs["Synthetic-1"].kind' <<<"$assignments")" == "scene" ]]
[[ "$(jq -r '.result.outputs["Synthetic-1"].previous.wallpaper_plugin' <<<"$assignments")" == "org.kde.image" ]]
echo "apply smoke: seeded assignments round-trip passed"

restore="$(call_daemon wallpaper.restore '{"output":"Synthetic-1"}' || true)"
[[ "$(jq -r '.ok' <<<"$restore")" == "false" ]]
[[ "$(jq -r '.result.error' <<<"$restore")" == "output_missing" ]]
echo "apply smoke: restore on a nonexistent output -> output_missing passed"

# --- BETA B1: the boot path ------------------------------------------------
# Before the fix, a daemon with no display in its environment ran
# kscreen-doctor (a QGuiApplication), which aborted on SIGABRT, and
# wallpaper.outputs answered shell_unreachable with an empty picker behind
# it. This is the case that reproduces that without a reboot.
stripped_socket="$smoke_root/stripped.sock"
start_stripped_daemon "$stripped_socket" "$smoke_root/stripped-state" \
    "$smoke_root/stripped-daemon.log"
stripped_outputs="$("$target_dir/debug/kwe" daemon-call --socket "$stripped_socket" \
    --method wallpaper.outputs --params '{}' || true)"
if [[ "$(jq -r '.ok' <<<"$stripped_outputs")" != "true" ]]; then
    echo "FAILED: enumeration from a unit-like environment did not succeed" >&2
    echo "        (BETA B1 regression: the daemon could not recover a display)" >&2
    jq -c '.result' <<<"$stripped_outputs" >&2
    exit 1
fi
if [[ "$(jq -r '.result.outputs | length' <<<"$stripped_outputs")" == "0" ]]; then
    echo "FAILED: enumeration from a unit-like environment returned no outputs" >&2
    exit 1
fi
jq -r '.result.outputs[] | "output " + .name' <<<"$stripped_outputs"
echo "apply smoke: enumeration with no display in the environment passed (BETA B1)"
stop_stripped_daemon

# Negative control: when the recovery genuinely finds no display, the daemon
# must say display_unavailable with something the user can act on — never an
# empty output list presented as success.
no_display_stub="$smoke_root/systemctl-no-display.sh"
cat >"$no_display_stub" <<'EOF'
#!/bin/sh
# Stands in for `systemctl --user show-environment` on a session that has
# not started yet: a valid answer with no display in it.
echo "LANG=en_US.UTF-8"
echo "XDG_RUNTIME_DIR=/run/user/1000"
EOF
chmod +x "$no_display_stub"
start_stripped_daemon "$smoke_root/nodisplay.sock" "$smoke_root/nodisplay-state" \
    "$smoke_root/nodisplay-daemon.log" --systemctl-binary "$no_display_stub"
no_display="$("$target_dir/debug/kwe" daemon-call --socket "$smoke_root/nodisplay.sock" \
    --method wallpaper.outputs --params '{}' || true)"
[[ "$(jq -r '.ok' <<<"$no_display")" == "false" ]]
if [[ "$(jq -r '.result.error' <<<"$no_display")" != "display_unavailable" ]]; then
    echo "FAILED: expected display_unavailable, got:" >&2
    jq -c '.result' <<<"$no_display" >&2
    exit 1
fi
detail="$(jq -r '.result.detail' <<<"$no_display")"
[[ "$detail" == *"restart kwe-daemon"* ]]
echo "apply smoke: no recoverable display -> display_unavailable passed (BETA B1)"
stop_stripped_daemon

echo "all apply smoke cases passed"
