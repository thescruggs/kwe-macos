#!/usr/bin/env bash
# SPDX-License-Identifier: GPL-3.0-or-later
# BETA_M2a CDP smoke suite: drives installed Chromium over
# --remote-debugging-pipe and asserts the screencast contract that M2b's
# renderer will rely on (ASCIIZ framing on fds 3/4, flattened session
# attach, the bounded unacked-frame stall, first-frame latency, jpeg size).
# The spike binary spawns its own chromium with a fresh throwaway profile
# and a runtime-generated animated fixture; no network, no user profile is
# touched. If chromium or jq is missing, the suite prints SKIPPED and exits
# 0, mirroring smoke-audio.sh.
set -euo pipefail

project_root="$(cd -- "$(dirname -- "${BASH_SOURCE[0]}")/.." && pwd)"
# Honor an external CARGO_TARGET_DIR (acceptance reuses the shared dep build).
target_dir="${CARGO_TARGET_DIR:-$project_root/target}"
smoke_root="$(mktemp -d -t kwe-cdp-smoke.XXXXXX)"
fixture="$smoke_root/fixture.html"
summary_file="$smoke_root/summary.json"

cleanup() {
    rm -rf -- "$smoke_root"
}
trap cleanup EXIT INT TERM

command -v jq >/dev/null || {
    echo "SKIPPED: jq is not installed"
    exit 0
}
if ! command -v chromium >/dev/null; then
    echo "SKIPPED: chromium is not installed (the bounded CDP pipe spike needs it)"
    exit 0
fi

# Runtime fixture: a continuously animated page on a dark background.
# Screencast frames only flow while the page animates, and the dark
# background keeps the q80 jpegs small.
cat >"$fixture" <<'EOF'
<!doctype html>
<html><head><meta charset="utf-8"><style>
  html, body { margin: 0; width: 100%; height: 100%; background: #101214; }
  #box { position: absolute; top: 20%; left: 0; width: 64px; height: 64px;
         border-radius: 8px; background: linear-gradient(135deg, #3a7bd5, #00d2ff);
         animation: slide 2s linear infinite alternate; }
  @keyframes slide { from { left: 0; } to { left: calc(100% - 64px); } }
</style></head><body><div id="box"></div></body></html>
EOF

cd "$project_root"
cargo build --package kwe-cdp --example spike >/dev/null

summary="$(timeout 180 "$target_dir/debug/examples/spike" \
    --chromium chromium --fixture "$fixture" \
    --target-timeout-secs 10 --phase-timeout-secs 25 \
    --stall-window-ms 1000 --silence-check-ms 1000 --min-frames 5)"
printf '%s\n' "$summary" >"$summary_file"

# Phase A: with no acks at all the stream must stall after at most 3 frames
# and stay silent. Spec deviation, documented in docs/BETA_M2.md: the task
# assumed <=1 additional frame; measured and source-verified (kMaxScreencast-
# FramesInFlight = 2) the stall lands at exactly 3, so the assertion is a
# 1..=3 band plus hard silence.
jq -e '.phase_a.stall_frames >= 1 and .phase_a.stall_frames <= 3' <<<"$summary" >/dev/null
echo "cdp smoke passed: screencast hard-stalls after at most 3 unacked frames"
jq -e '.phase_a.silence_confirmed == true' <<<"$summary" >/dev/null
echo "cdp smoke passed: the stall is hard silence (no late frames in the window)"

# Phase B: with a per-frame ack the stream flows, the first frame arrives
# well inside the 10 s bound, and the jpegs are real and small.
jq -e '.phase_b.frames >= 5' <<<"$summary" >/dev/null
jq -e '.phase_b.first_frame_after_start_ms < 3000' <<<"$summary" >/dev/null
jq -e '.phase_b.bytes_per_frame_avg > 100 and .phase_b.bytes_per_frame_avg < 5000' <<<"$summary" >/dev/null
echo "cdp smoke passed: >=5 acked frames, first frame <10 s after startScreencast, sane jpeg size"

# Stopping the acks must stop the stream within the same bounded tail.
jq -e '.phase_b.additional_after_ack_stop >= 1 and .phase_b.additional_after_ack_stop <= 3' <<<"$summary" >/dev/null
jq -e '.phase_b.silence_confirmed == true' <<<"$summary" >/dev/null
echo "cdp smoke passed: frames stop within 3 after acks cease, then hard silence"

echo "all cdp smoke cases passed"
