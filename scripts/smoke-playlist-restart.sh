#!/usr/bin/env bash
# SPDX-License-Identifier: GPL-3.0-or-later
set -euo pipefail

project_root="$(cd -- "$(dirname -- "${BASH_SOURCE[0]}")/.." && pwd)"
smoke_root="$(mktemp -d -t kwe-playlist-smoke.XXXXXX)"
socket="$smoke_root/daemon.sock"
runtime_dir="$smoke_root/runtime"
state_dir="$smoke_root/state"
fixture_root="$smoke_root/fixture"
external_root="$smoke_root/extlib"
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
        --renderer-runtime-dir "$runtime_dir" \
        --state-dir "$state_dir" \
        --steam-root "$fixture_root" \
        --renderer-startup-timeout-ms 500 \
        --renderer-frame-timeout-ms 250 \
        --renderer-stop-grace-ms 80 \
        --renderer-restart-delay-ms 20 \
        --renderer-canary-ms 150 \
        --renderer-handoff-timeout-ms 1000 \
        --renderer-max-failures 3 \
        --renderer-address-space-mib 384 \
        --playlist-tick-ms 100 \
        --allow-test-faults \
        "${boundary_flags[@]}" \
        "$@" >"$smoke_root/daemon.log" 2>&1 &
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

# Poll renderer.status until a jq filter is true; prints the final status.
wait_renderer() {
    local filter="$1"
    local label="$2"
    local output=""
    for _attempt in {1..600}; do
        output="$(call_daemon renderer.status)"
        if [[ "$(jq -r "$filter" <<<"$output")" == "true" ]]; then
            printf '%s\n' "$output"
            return
        fi
        sleep 0.05
    done
    echo "timed out waiting for renderer $label" >&2
    printf '%s\n' "$output" >&2
    sed -n '1,200p' "$smoke_root/daemon.log" >&2
    return 1
}

# Poll playlist.status until a jq filter is true; prints the final status.
wait_playlist() {
    local filter="$1"
    local label="$2"
    local output=""
    for _attempt in {1..500}; do
        output="$(call_daemon playlist.status)"
        if [[ "$(jq -r "$filter" <<<"$output")" == "true" ]]; then
            printf '%s\n' "$output"
            return
        fi
        sleep 0.04
    done
    echo "timed out waiting for playlist $label" >&2
    printf '%s\n' "$output" >&2
    sed -n '1,200p' "$smoke_root/daemon.log" >&2
    return 1
}

fail() {
    echo "FAILED: $1" >&2
    exit 1
}

build_steam_fixture() {
    mkdir -p "$fixture_root/steamapps/workshop/content/431960/1"
    mkdir -p "$external_root/steamapps/workshop/content/431960"
    cat >"$fixture_root/steamapps/libraryfolders.vdf" <<EOF
"LibraryFolders" { "0" { "path" "$external_root" } }
EOF
    cat >"$external_root/steamapps/appmanifest_431960.acf" <<'EOF'
"AppState" { "appid" "431960" }
EOF
    # Subscriptions: 1 (installed) and 2 (deliberately absent).
    cat >"$fixture_root/steamapps/appworkshop_431960.acf" <<'EOF'
"AppWorkshop" { "WorkshopItems" { "1" "1" "2" "1" } }
EOF
    # Item 3 subscribes through the external library but is absent at first.
    cat >"$external_root/steamapps/appworkshop_431960.acf" <<'EOF'
"AppWorkshop" { "WorkshopItems" { "3" "1" } }
EOF
    cat >"$fixture_root/steamapps/workshop/content/431960/1/project.json" <<'EOF'
{"title":"Synthetic One","type":"scene"}
EOF
    # The runnable scene.json inside the content root (the apply lane runs
    # the catalog content; the test kind is never assignable).
    cat >"$fixture_root/steamapps/workshop/content/431960/1/scene.json" <<'EOF'
{"general":{}}
EOF
}

# The playlist session drives the apply lane on entry changes (BETA_M4c),
# so EVERY daemon in this smoke must stub the whole Plasma boundary (no
# live session is ever touched) and run the fake python scene renderer
# (the test kind is never assignable).
build_playlist_boundary() {
    # Fake scene renderer: real KWEFRM1 frame protocol, one 50 ms frame
    # loop for the whole (long) scenario. The supervisor is the only
    # arbiter of its lifecycle.
    cat >"$smoke_root/fake-scene-renderer.py" <<'EOF'
#!/usr/bin/env python3
import argparse
import os
import struct
import time

parser = argparse.ArgumentParser()
parser.add_argument("--output", required=True)
parser.add_argument("--width", type=int, default=320)
parser.add_argument("--height", type=int, default=180)
parser.add_argument("--fps", type=int, default=30)
parser.add_argument("--scaling", default="aspect")
parser.add_argument("--content")
args = parser.parse_args()

width = args.width
height = args.height
stride = width * 4
slot_bytes = stride * height
file_bytes = 64 + 2 * slot_bytes

header = bytearray(64)
struct.pack_into("<8s", header, 0, b"KWEFRM1\0")
struct.pack_into("<I", header, 8, 1)      # version
struct.pack_into("<I", header, 12, 64)    # header bytes
struct.pack_into("<Q", header, 16, file_bytes)
struct.pack_into("<I", header, 24, width)
struct.pack_into("<I", header, 28, height)
struct.pack_into("<I", header, 32, stride)
struct.pack_into("<I", header, 36, 1)     # BGRA premultiplied
struct.pack_into("<I", header, 40, 2)     # slot count
struct.pack_into("<Q", header, 48, 0)     # generation (even)
struct.pack_into("<I", header, 56, 0)     # active slot
struct.pack_into("<I", header, 60, 2)     # producer state: Running

with open(args.output, "wb") as frame:
    frame.write(bytes(header) + bytes(slot_bytes * 2))
    frame.flush()
    os.fsync(frame.fileno())
    generation = 0
    active = 0
    deadline = time.monotonic() + 90.0
    while time.monotonic() < deadline:
        generation += 1          # odd
        struct.pack_into("<Q", header, 48, generation)
        active = 1 - active
        struct.pack_into("<I", header, 56, active)
        generation += 1          # even
        struct.pack_into("<Q", header, 48, generation)
        frame.seek(48)
        frame.write(header[48:64])
        frame.flush()
        os.fsync(frame.fileno())
        time.sleep(0.05)
EOF
    chmod +x "$smoke_root/fake-scene-renderer.py"

    # Stub for --plasma-switch-command: the whole evaluation boundary
    # (enumeration + switch) replaced by this script. It answers
    # enumeration probes with canned JSON, appends every switch script to
    # the switch log, and flips the canned reply to the kwe plugin once
    # the kwe switch script has been evaluated (post-switch verification
    # then sees our plugin).
    cat >"$smoke_root/plasma-stub.sh" <<EOF
#!/usr/bin/env bash
# SPDX-License-Identifier: GPL-3.0-or-later
log_file="$smoke_root/switch.log"
script="\$1"
if [[ "\$script" == *'wallpaperPlugin = "org.kde.kwe.wallpaper"'* ]]; then
    : >"\$log_file.kwe"
fi
if [[ "\$script" == *'var d = desktops();'* ]]; then
    if [[ -f "\$log_file.kwe" ]]; then
        printf '%s' '{"desktops":[{"index":1,"id":111,"screen":0,"wp":"org.kde.kwe.wallpaper","image":null}],"connectors":{"DP-1":0}}'
    else
        printf '%s' '{"desktops":[{"index":1,"id":111,"screen":0,"wp":"org.kde.image","image":"file:///usr/share/wallpapers/fallback.png"}],"connectors":{"DP-1":0}}'
    fi
else
    printf '%s\n' "\$script" >>"\$log_file"
    printf '%s' 'true'
fi
exit 0
EOF
    chmod +x "$smoke_root/plasma-stub.sh"

    # Fake kscreen-doctor: one enabled, connected output "DP-1".
    cat >"$smoke_root/fake-kscreen-doctor.sh" <<'EOF'
#!/usr/bin/env bash
# SPDX-License-Identifier: GPL-3.0-or-later
printf 'Output: 1 DP-1 62b8c814-6503-41cf-a04d-8743a967c99b\n\tenabled\n\tconnected\n\tGeometry: 0,0 2926x823\n'
EOF
    chmod +x "$smoke_root/fake-kscreen-doctor.sh"
    : >"$smoke_root/switch.log"
    boundary_flags=(--renderer-scene "$smoke_root/fake-scene-renderer.py"
        --plasma-switch-command "$smoke_root/plasma-stub.sh"
        --kscreen-doctor-binary "$smoke_root/fake-kscreen-doctor.sh")
}

command -v jq >/dev/null
cd "$project_root"
cargo build --workspace >/dev/null
build_steam_fixture
build_playlist_boundary

# --- Scenario 1: import contract and definition validation ---------------
start_daemon
call_daemon health >/dev/null
[[ "$(jq -r '.result.playlists | length' <<<"$(call_daemon playlist.list)")" == "0" ]] \
    || fail "playlist store must start empty"

import_params='{"playlists":[
  {"title":"Legacy One","entries":["1","2"],"shuffle":false,"repeat":true,"duration_seconds":300,"transition":"none","transition_seconds":0},
  {"title":"Legacy Two","entries":["1"]}
]}'
imported="$(jq -r '.result.imported' <<<"$(call_daemon playlist.import "$import_params")")"
[[ "$imported" == "2" ]] || fail "legacy import should accept 2 playlists"
ids="$(jq -r '[.result.playlists[].id] | sort | join(",")' <<<"$(call_daemon playlist.list)")"
[[ "$ids" == "Legacy One,Legacy Two" ]] || fail "import ids were not derived from titles: $ids"

if call_daemon playlist.import '{"playlists":[]}' >/dev/null 2>&1; then
    fail "second import must be blocked on a non-empty store"
fi

huge_import="$(jq -cn '{playlists: [range(0;257) | {title: ("P" + tostring), entries: []}]}')"
if call_daemon playlist.import "$huge_import" >/dev/null 2>&1; then
    fail "oversized import must be rejected"
fi

daily='{"id":"daily","title":"Daily","entries":["1","2","3"],"shuffle":false,"repeat":true,"duration_seconds":10,"transition":"none","transition_seconds":0}'
[[ "$(jq -r '.result.id' <<<"$(call_daemon playlist.put "{\"playlist\":$daily}")")" == "daily" ]] \
    || fail "playlist.put should echo the stored id"

bad_daily="${daily/duration_seconds\":10/duration_seconds\":5}"
if call_daemon playlist.put "{\"playlist\":$bad_daily}" >/dev/null 2>&1; then
    fail "out-of-bounds duration must be rejected"
fi
if call_daemon playlist.put '{"playlist":{"id":"x","title":"X","entries":[],"shuffle":false,"repeat":true,"duration_seconds":300,"transition":"none","transition_seconds":0,"entrirs":[]}}' >/dev/null 2>&1; then
    fail "unknown playlist fields must be rejected"
fi
echo "playlist import and definition validation passed"

# --- Scenario 2: activation and missing-library availability --------------
status="$(call_daemon playlist.activate '{"id":"daily"}')"
[[ "$(jq -r '.result.decision.state' <<<"$status")" == "started" ]] \
    || fail "activation should report started, got $(jq -c '.result.decision' <<<"$status")"
[[ "$(jq -r '.result.decision.wallpaper_id' <<<"$status")" == "1" ]] \
    || fail "first eligible wallpaper should be 1"

if call_daemon playlist.activate '{"id":"nope"}' >/dev/null 2>&1; then
    fail "activating an unknown playlist must fail"
fi

sleep 1.1
status="$(call_daemon playlist.status)"
[[ "$(jq -r '.result.decision.state' <<<"$status")" == "waiting" ]] \
    || fail "session should be waiting, got $(jq -c '.result.decision' <<<"$status")"
[[ "$(jq -r '.result.decision.wallpaper_id' <<<"$status")" == "1" ]] \
    || fail "current wallpaper should still be 1"
remaining="$(jq -r '.result.decision.remaining_ms' <<<"$status")"
(( remaining < 9900 )) || fail "remaining time did not advance: $remaining"
[[ "$(jq -r '.result.unavailable_ids | index("2") != null and index("3") != null' <<<"$status")" == "true" ]] \
    || fail "missing items 2 and 3 must be reported unavailable"
echo "playlist activation and availability passed"

# --- Scenario 3: daemon restart recovers the session ----------------------
stop_daemon
start_daemon
call_daemon health >/dev/null
status="$(wait_playlist '.result.active and .result.decision.state == "waiting"' "restart recovery")"
[[ "$(jq -r '.result.playlist_id' <<<"$status")" == "daily" ]] \
    || fail "active playlist must survive a restart"
[[ "$(jq -r '.result.decision.wallpaper_id' <<<"$status")" == "1" ]] \
    || fail "current wallpaper must survive a restart"
restart_remaining="$(jq -r '.result.decision.remaining_ms' <<<"$status")"
(( restart_remaining >= 8000 && restart_remaining <= 10100 )) \
    || fail "restart remaining time out of range: $restart_remaining"
echo "playlist restart recovery passed (remaining ${restart_remaining}ms)"

# --- Scenario 4: missing library skip and return-to-eligible --------------
call_daemon rescan >/dev/null
mkdir -p "$external_root/steamapps/workshop/content/431960/3"
cat >"$external_root/steamapps/workshop/content/431960/3/project.json" <<'EOF'
{"title":"Synthetic Three","type":"scene"}
EOF
cat >"$external_root/steamapps/workshop/content/431960/3/scene.json" <<'EOF'
{"general":{}}
EOF
call_daemon rescan >/dev/null
status="$(wait_playlist '.result.decision.wallpaper_id == "3"' "skip of missing item 2")"
[[ "$(jq -r '.result.decision.index' <<<"$status")" == "2" ]] \
    || fail "item 3 must be selected at index 2"
rm -rf -- "$external_root"
call_daemon rescan >/dev/null
status="$(wait_playlist '.result.decision.wallpaper_id == "1" and .result.decision.state == "waiting"' "return to eligible item")"
# BETA_M4c: the return-to-eligible decision drives a re-apply through the
# apply transaction. The previous entry's handoff must commit and the
# backoff retry must land before scenario 6 can start its own renderer:
# the supervisor rejects a start while a handoff is pending
# ("display handoff is still awaiting acknowledgement"), and the smoke
# would die under set -e at an otherwise transient boundary.
wait_renderer '.result.wallpaper_id == "1" and .result.phase == "live"' "return-to-1 re-apply"
echo "playlist missing-library skip and recovery passed"

# --- Scenario 5: suspend/resume preserves remaining time ------------------
# Re-read the remaining time immediately before the skip: the M4c re-apply
# wait above lets the entry advance, so a snapshot from scenario 4 would
# be stale by the whole wait duration.
before_remaining="$(jq -r '.result.decision.remaining_ms' <<<"$(call_daemon playlist.status)")"
skip_status="$(call_daemon playlist.debug-clock-skip '{"ms":60000}')"
[[ "$(jq -r '.result.clock_skipped_ms' <<<"$skip_status")" == "60000" ]] \
    || fail "clock skip must be recorded"
after_remaining="$(jq -r '.result.decision.remaining_ms' <<<"$skip_status")"
(( after_remaining <= before_remaining )) || fail "remaining time must not grow across suspend"
(( before_remaining - after_remaining < 300 )) \
    || fail "remaining time must be preserved across suspend: $before_remaining -> $after_remaining"
echo "playlist suspend/resume freeze passed"

# --- Scenario 6: quarantined content is skipped, recovery re-enables ------
# The supervisor's quarantined phase is transient under BETA_M4c: the
# session keeps its desired entry live (and re-asserts it once the
# foreign renderer is no longer live), so the durable observable is the
# playlist decision — no_eligible while the only eligible entry is
# quarantined.
quarantine_params='{"wallpaper_id":"1","content_hash":"hash-1","width":160,"height":90,"fps":60,"test_fault":{"kind":"hang","after":3}}'
call_daemon renderer.start "$quarantine_params" >/dev/null
status="$(wait_playlist '.result.decision.state == "no_eligible"' "quarantine skip")"
echo "playlist quarantine skip passed"

healthy_params='{"wallpaper_id":"1","content_hash":"hash-1","width":160,"height":90,"fps":60}'
call_daemon renderer.retry "$healthy_params" >/dev/null
for _attempt in {1..250}; do
    renderer_status="$(call_daemon renderer.status)"
    [[ "$(jq -r '.result.phase' <<<"$renderer_status")" == "live" ]] && break
    sleep 0.02
done
[[ "$(jq -r '.result.phase' <<<"$renderer_status")" == "live" ]] \
    || fail "renderer 1 must recover to live"
status="$(wait_playlist '.result.decision.wallpaper_id == "1"' "post-quarantine recovery")"
echo "playlist post-quarantine recovery passed"

# --- Scenario 9: renderer assignment through the apply transaction --------
# BETA_M4c: on an entry change (timer advance, restart restore) the session
# drives the M4a apply transaction for the playlist's output. The test kind
# is never assignable, so the scenario runs the REAL scene kind with the
# fake python scene renderer, and stubs the whole Plasma boundary
# (--plasma-switch-command + fake kscreen-doctor) so no live session is
# touched. Scenarios 2/4/6 already ran applies through the boundary stubs,
# so the switch log is reset here for exact per-scenario counts.
stop_daemon
: >"$smoke_root/switch.log"
rm -f "$smoke_root/switch.log.kwe"

# Item 3's library was removed in scenario 4; recreate it as a scene.
mkdir -p "$external_root/steamapps/workshop/content/431960/3"
cat >"$external_root/steamapps/workshop/content/431960/3/project.json" <<'EOF'
{"title":"Synthetic Three","type":"scene"}
EOF
cat >"$external_root/steamapps/workshop/content/431960/3/scene.json" <<'EOF'
{"general":{}}
EOF
cat >"$external_root/steamapps/appworkshop_431960.acf" <<'EOF'
"AppWorkshop" { "WorkshopItems" { "3" "1" } }
EOF

# The configured-output path (--playlist-output); scenarios 1-8 exercise
# the fallback (fresh enumeration of the first enabled+connected output).
start_daemon --playlist-output DP-1
call_daemon health >/dev/null
assign='{"id":"assign","title":"Assign","entries":["1","3"],"shuffle":false,"repeat":true,"duration_seconds":10,"transition":"none","transition_seconds":0}'
[[ "$(jq -r '.result.id' <<<"$(call_daemon playlist.put "{\"playlist\":$assign}")")" == "assign" ]] \
    || fail "playlist.put should store the assign playlist"
call_daemon playlist.activate '{"id":"assign"}' >/dev/null

# The first entry goes live through the real apply transaction: the fake
# scene renderer reports wallpaper 1 / kind scene and the assignment store
# records DP-1 -> 1. Exactly one switch script ran.
wait_renderer '.result.wallpaper_id == "1" and .result.kind == "scene"' "entry 1 applied"
renderer_status="$(call_daemon renderer.status)"
[[ "$(jq -r '.result.phase' <<<"$renderer_status")" == "live" ]] \
    || fail "entry 1 must be live, got $(jq -c '.result' <<<"$renderer_status")"
assignments="$(call_daemon wallpaper.assignments)"
[[ "$(jq -r '.result.outputs["DP-1"].wallpaper_id' <<<"$assignments")" == "1" ]] \
    || fail "assignment store must record DP-1 -> 1"
[[ "$(jq -r '.result.outputs["DP-1"].kind' <<<"$assignments")" == "scene" ]] \
    || fail "assignment kind must be scene"
# The renderer reports the new entry the moment its promotion lands, while
# the transaction is still mid-flight (persist, switch script, verify).
# Wait for the switch-script marker — the transaction's last step before
# the verification probe — before asserting exact counts.
for _attempt in {1..200}; do
    [[ "$(wc -l <"$smoke_root/switch.log")" == "1" ]] && break
    sleep 0.05
done
[[ "$(wc -l <"$smoke_root/switch.log")" == "1" ]] \
    || fail "exactly one switch script after the first apply"

# Timer advance (the real 10 s entry expires; the debug clock skip freezes
# remaining time by design and cannot advance the entry) drives the hard
# cut to entry 3 through the same transaction.
wait_renderer '.result.wallpaper_id == "3" and .result.kind == "scene"' "timer advance to entry 3"
for _attempt in {1..200}; do
    [[ "$(wc -l <"$smoke_root/switch.log")" == "2" ]] && break
    sleep 0.05
done
[[ "$(wc -l <"$smoke_root/switch.log")" == "2" ]] \
    || fail "exactly two switch scripts after the timer advance"
# The assignment persists before the switch script, so the count above
# also guarantees the store already records the new entry.
assignments="$(call_daemon wallpaper.assignments)"
[[ "$(jq -r '.result.outputs["DP-1"].wallpaper_id' <<<"$assignments")" == "3" ]] \
    || fail "assignment store must record DP-1 -> 3"
echo "playlist renderer assignment timer advance passed"

# Daemon restart: the session restores its position and re-applies the
# restored entry exactly once (fresh supervisor, one more switch script),
# then stays put (no churn).
# TIMING DEPENDENCY: entry 3 runs on the real clock for its full 10 s
# duration. The stop below (and the restart re-apply + 1 s no-churn check)
# must all complete well inside entry 3's window — otherwise the runtime
# advances past entry 3 before shutdown and the persisted position is no
# longer entry 3, breaking the assertion that the restored session is on
# entry 3. The whole block completes in ~2 s, comfortably inside the window.
stop_daemon
start_daemon --playlist-output DP-1
call_daemon health >/dev/null
wait_renderer '.result.wallpaper_id == "3" and .result.kind == "scene"' "restart re-apply"
renderer_status="$(call_daemon renderer.status)"
[[ "$(jq -r '.result.phase' <<<"$renderer_status")" == "live" ]] \
    || fail "restored entry 3 must be live"
for _attempt in {1..200}; do
    [[ "$(wc -l <"$smoke_root/switch.log")" == "3" ]] && break
    sleep 0.05
done
[[ "$(wc -l <"$smoke_root/switch.log")" == "3" ]] \
    || fail "restart must re-apply exactly once (3 switch scripts total)"
sleep 1
[[ "$(wc -l <"$smoke_root/switch.log")" == "3" ]] \
    || fail "the restored session must not churn"
assignments="$(call_daemon wallpaper.assignments)"
[[ "$(jq -r '.result.outputs["DP-1"].wallpaper_id' <<<"$assignments")" == "3" ]] \
    || fail "assignment store must survive the restart"
echo "playlist renderer assignment restart restore passed"

# --- Scenario 7: corrupt runtime state is quarantined, daemon stays up ----
stop_daemon
echo "garbage" >"$state_dir/playlist-runtime-v1.json"
start_daemon
call_daemon health >/dev/null
if ! ls "$state_dir"/playlist-runtime-v1.json.invalid-* >/dev/null 2>&1; then
    fail "corrupt runtime state must be quarantined to an .invalid-* file"
fi
status="$(call_daemon playlist.status)"
[[ "$(jq -r '.result.active' <<<"$status")" == "false" ]] \
    || fail "corrupt runtime state must start the session fresh"
[[ "$(jq -r '.result.definitions.store_health' <<<"$status")" == "ok" ]] \
    || fail "definitions store must be unaffected"
fresh="$(call_daemon playlist.activate '{"id":"daily"}')"
[[ "$(jq -r '.result.decision.state' <<<"$fresh")" == "started" ]] \
    || fail "fresh activation after corruption should start from scratch"
echo "playlist corrupt-runtime quarantine passed"

# --- Scenario 8: corrupt definitions disable methods but not the daemon ---
stop_daemon
echo "garbage" >"$state_dir/playlists-v1.json"
start_daemon
call_daemon health >/dev/null
if call_daemon playlist.list >/dev/null 2>&1; then
    fail "playlist.list must fail on a corrupt definitions store"
fi
status="$(call_daemon playlist.status)"
[[ "$(jq -r '.result.definitions.store_health' <<<"$status")" == "corrupt" ]] \
    || fail "definitions health must report corrupt"
[[ "$(jq -r '.result.definitions.count' <<<"$status")" == "0" ]] \
    || fail "corrupt definitions must expose zero playlists"
if call_daemon playlist.put "{\"playlist\":$daily}" >/dev/null 2>&1; then
    fail "playlist.put must fail on a corrupt definitions store"
fi
echo "playlist corrupt-definitions containment passed"

stop_daemon
echo "all playlist smoke cases passed"
