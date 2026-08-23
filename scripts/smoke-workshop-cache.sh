#!/usr/bin/env bash
# SPDX-License-Identifier: GPL-3.0-or-later
set -euo pipefail

project_root="$(cd -- "$(dirname -- "${BASH_SOURCE[0]}")/.." && pwd)"
smoke_root="$(mktemp -d -t kwe-workshop-cache-smoke.XXXXXX)"
socket="$smoke_root/daemon.sock"
state_dir="$smoke_root/state"
fixture_root="$smoke_root/fixture"
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
    local method="$1"
    local params="{}"
    if (( $# >= 2 )); then
        params="$2"
    fi
    target/debug/kwe daemon-call --socket "$socket" --method "$method" --params "$params"
}

start_daemon() {
    target/debug/kwe-daemon \
        --socket "$socket" \
        --renderer "$project_root/target/debug/kwe-test-renderer" \
        --renderer-runtime-dir "$smoke_root/runtime" \
        --state-dir "$state_dir" \
        --steam-root "$fixture_root" \
        >"$smoke_root/daemon.log" 2>&1 &
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

stop_daemon() {
    kill "$daemon_pid" 2>/dev/null || true
    wait "$daemon_pid" 2>/dev/null || true
    daemon_pid=""
    rm -f -- "$socket"
}

fail() {
    echo "FAILED: $1" >&2
    exit 1
}

build_fixture() {
    mkdir -p "$fixture_root/steamapps/workshop/content/431960/1"
    echo '"LibraryFolders" { }' >"$fixture_root/steamapps/libraryfolders.vdf"
    cat >"$fixture_root/steamapps/appworkshop_431960.acf" <<'EOF'
"AppWorkshop" { "WorkshopItems" { "1" "1" "2" "1" } }
EOF
    cat >"$fixture_root/steamapps/workshop/content/431960/1/project.json" <<'EOF'
{"title":"Synthetic One","type":"scene","tags":["nature","calm"]}
EOF
}

item() {
    local id="$1"
    call_daemon catalog | jq -c ".result.items[] | select(.workshop_id == \"$id\")"
}

command -v jq >/dev/null
cd "$project_root"
cargo build --workspace >/dev/null
build_fixture

# --- Initial scan snapshots subscribed metadata ----------------------------
start_daemon
call_daemon health >/dev/null
[[ "$(jq -r '.title' <<<"$(item 1)")" == "Synthetic One" ]] \
    || fail "installed item must report its on-disk title"
[[ "$(jq -r '.workshop_state' <<<"$(item 1)")" == "subscribed_installed" ]] \
    || fail "item 1 must be subscribed_installed"
[[ "$(jq -r '.workshop_state' <<<"$(item 2)")" == "subscribed_missing" ]] \
    || fail "item 2 must be subscribed_missing"
[[ -s "$state_dir/workshop-metadata-v1.json" ]] \
    || fail "cache file must be persisted after the first scan"
[[ "$(jq -r '.items["1"].title' "$state_dir/workshop-metadata-v1.json")" == "Synthetic One" ]] \
    || fail "cache must snapshot the installed item's metadata"
echo "workshop cache snapshot passed"

# --- Unmount: metadata is restored from the cache --------------------------
rm -rf -- "$fixture_root/steamapps"
call_daemon rescan >/dev/null
restored="$(item 1)"
[[ "$(jq -r '.title' <<<"$restored")" == "Synthetic One" ]] \
    || fail "unmounted subscription must keep its cached title, got $(jq -r '.title' <<<"$restored")"
[[ "$(jq -r '.workshop_state' <<<"$restored")" == "subscribed_missing" ]] \
    || fail "restored item must be subscribed_missing"
[[ "$(jq -r '[.diagnostics[].code] | index("workshop.offline_metadata") != null' <<<"$restored")" == "true" ]] \
    || fail "restored item must carry the workshop.offline_metadata diagnostic"
echo "workshop cache unmount recovery passed"

# --- The restored metadata survives a daemon restart -----------------------
stop_daemon
start_daemon
call_daemon health >/dev/null
restored="$(item 1)"
[[ "$(jq -r '.title' <<<"$restored")" == "Synthetic One" ]] \
    || fail "cached metadata must survive a daemon restart"
echo "workshop cache restart persistence passed"

# --- Remount: the on-disk scan wins again --------------------------------
build_fixture
call_daemon rescan >/dev/null
[[ "$(jq -r '.workshop_state' <<<"$(item 1)")" == "subscribed_installed" ]] \
    || fail "remounted library must return item 1 to subscribed_installed"
echo "workshop cache remount recovery passed"

# --- Corrupt cache is quarantined, daemon stays up ------------------------
stop_daemon
echo "garbage" >"$state_dir/workshop-metadata-v1.json"
start_daemon
call_daemon health >/dev/null
if ! ls "$state_dir"/workshop-metadata-v1.json.invalid-* >/dev/null 2>&1; then
    fail "corrupt cache must be quarantined to an .invalid-* file"
fi
echo "workshop cache corrupt-state quarantine passed"

stop_daemon
echo "all workshop cache smoke cases passed"
