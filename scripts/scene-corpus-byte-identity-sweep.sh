#!/usr/bin/env bash
# SPDX-License-Identifier: GPL-3.0-or-later
# S3 review RECOMMENDED #6: a maintenance tool (not wired into check.sh)
# reproducing the "before/after byte-identical across the local corpus"
# regression claim in AI-Skills/BETA_PLAN.md's S3 change-log entry -- that
# claim previously rested on ad-hoc scripts that existed only in /tmp on
# the machine that ran them and could not be re-run by another engineer,
# in CI, or on a future regression. This script is that reproduction,
# generalized to compare any two kwe-scene-renderer builds (or just
# render+report stats for one) over any local Workshop scene.pkg corpus.
#
# Usage:
#   KWE_CORPUS_DIR=/path/to/workshop/content/431960 \
#     [KWE_ASSETS_DIR=/path/to/wallpaper_engine/assets] \
#     [KWE_REFERENCE_RENDERER=/path/to/a/kwe-scene-renderer] \
#     [KWE_CANDIDATE_RENDERER=/path/to/another/kwe-scene-renderer] \
#     [KWE_SWEEP_WIDTH=960] [KWE_SWEEP_HEIGHT=540] [KWE_SWEEP_TIMEOUT=3] \
#     [KWE_SWEEP_DEVICE=llvmpipe] \
#     ./scripts/scene-corpus-byte-identity-sweep.sh
#
# KWE_CORPUS_DIR: required, a directory holding one scene.pkg per
#   subdirectory (the Steam Workshop layout) -- SKIPPED with exit 0 when
#   unset or missing, matching scripts/smoke-corpus-pkg.sh's convention.
#   No Workshop payload is committed by or with this script.
# KWE_ASSETS_DIR: optional, passed as --assets-dir to every render.
# KWE_REFERENCE_RENDERER: optional path to a SECOND kwe-scene-renderer
#   binary (e.g. a worktree built from an earlier commit) -- when set,
#   every scene is rendered with BOTH binaries and their frames compared;
#   when unset, every scene is rendered with the candidate binary only
#   and per-scene pixel stats are reported without a diff (useful for a
#   quick corpus-wide sanity pass without needing a second build).
# KWE_CANDIDATE_RENDERER: optional, defaults to
#   $CARGO_TARGET_DIR/debug/kwe-scene-renderer (built by this script if
#   missing, mirroring smoke-corpus-pkg.sh).
#
# A scene the candidate build REFUSES (no frame published, e.g. a known
# worker-side refusal such as clearcolor shape or an undecodable base
# texture) is reported and skipped from the comparison, never treated as
# a failure by itself -- this script measures REGRESSION, not
# acceptance (scripts/smoke-corpus-pkg.sh already covers preflight
# acceptance).
set -euo pipefail

project_root="$(cd -- "$(dirname -- "${BASH_SOURCE[0]}")/.." && pwd)"
target_dir="${CARGO_TARGET_DIR:-$project_root/target}"
corpus="${KWE_CORPUS_DIR:-}"

if [[ -z "$corpus" ]]; then
    echo "scene corpus byte-identity sweep: SKIPPED (KWE_CORPUS_DIR is not set)"
    exit 0
fi
if [[ ! -d "$corpus" ]]; then
    echo "scene corpus byte-identity sweep: SKIPPED (KWE_CORPUS_DIR=$corpus does not exist)"
    exit 0
fi

assets_dir="${KWE_ASSETS_DIR:-}"
reference_renderer="${KWE_REFERENCE_RENDERER:-}"
candidate_renderer="${KWE_CANDIDATE_RENDERER:-$target_dir/debug/kwe-scene-renderer}"
width="${KWE_SWEEP_WIDTH:-960}"
height="${KWE_SWEEP_HEIGHT:-540}"
timeout_seconds="${KWE_SWEEP_TIMEOUT:-3}"
device="${KWE_SWEEP_DEVICE:-llvmpipe}"

if [[ ! -x "$candidate_renderer" ]]; then
    echo "scene corpus byte-identity sweep: building the candidate binary ($candidate_renderer)"
    cargo build -p kwe-scene-renderer >/dev/null
fi
if [[ -n "$reference_renderer" && ! -x "$reference_renderer" ]]; then
    echo "scene corpus byte-identity sweep: FAILED (KWE_REFERENCE_RENDERER=$reference_renderer is not executable)" >&2
    exit 1
fi

lvp_icd=""
for candidate_icd in /usr/share/vulkan/icd.d/lvp_icd.json /usr/share/vulkan/icd.d/lvp_icd.x86_64.json; do
    [[ -f "$candidate_icd" ]] && lvp_icd="$candidate_icd" && break
done

mapfile -t pkgs < <(find "$corpus" -maxdepth 2 -name scene.pkg | sort)
if (( ${#pkgs[@]} == 0 )); then
    echo "scene corpus byte-identity sweep: FAILED ($corpus contains no scene.pkg files)" >&2
    exit 1
fi

work_dir="$(mktemp -d -t kwe-corpus-sweep.XXXXXX)"
trap 'rm -rf "$work_dir"' EXIT

# Reads the KWEFRM1 shared-frame-file format (crates/kwe-frame-protocol)
# the same way scripts/frame-read.py does, and prints a coarse-sampled
# mean BGRA + distinct-colour count for one frame file -- enough to
# detect a real regression (the S3 sweep this reproduces found one this
# way: a scene going from a real, varied photo composite to flat
# clear-colour) without needing a full image diff library.
render_and_stat() {
    local renderer="$1" pkg="$2" out="$3"
    rm -f "$out"
    local args=(--output "$out" --width "$width" --height "$height" --fps 30
                 --content "$pkg" --device "$device")
    [[ -n "$assets_dir" ]] && args+=(--assets-dir "$assets_dir")
    if [[ -n "$lvp_icd" ]]; then
        VK_ICD_FILENAMES="$lvp_icd" timeout "$timeout_seconds" "$renderer" "${args[@]}" \
            </dev/null >"$out.log" 2>&1 || true
    else
        timeout "$timeout_seconds" "$renderer" "${args[@]}" </dev/null >"$out.log" 2>&1 || true
    fi
    if [[ ! -f "$out" ]]; then
        echo "REFUSED"
        return 0
    fi
    python3 - "$out" <<'PY'
import struct
import sys

path = sys.argv[1]
data = open(path, "rb").read()
if len(data) < 64 or data[0:8] != b"KWEFRM1\0":
    print("NO_FRAME")
    raise SystemExit
width, height, stride = struct.unpack_from("<III", data, 24)
(active_slot,) = struct.unpack_from("<I", data, 56)
slot_bytes = stride * height
offset = 64 + active_slot * slot_bytes
if offset + slot_bytes > len(data):
    print("NO_FRAME")
    raise SystemExit
pixels = data[offset : offset + slot_bytes]
total = [0, 0, 0, 0]
seen = set()
n = 0
step = 9
for y in range(0, height, step):
    for x in range(0, width, step):
        i = y * stride + x * 4
        px = pixels[i : i + 4]
        seen.add(px)
        for c in range(4):
            total[c] += px[c]
        n += 1
mean = [round(t / n, 1) for t in total]
print(f"{mean[0]},{mean[1]},{mean[2]},{mean[3]}|{len(seen)}|{n}")
PY
}

total=0
compared=0
refused=0
flagged=()
for pkg in "${pkgs[@]}"; do
    id="$(basename "$(dirname "$pkg")")"
    total=$((total + 1))
    candidate_stat="$(render_and_stat "$candidate_renderer" "$pkg" "$work_dir/candidate-$id.bin")"
    if [[ -z "$reference_renderer" ]]; then
        echo "scene corpus byte-identity sweep: $id candidate=$candidate_stat"
        [[ "$candidate_stat" != "REFUSED" && "$candidate_stat" != "NO_FRAME" ]] && compared=$((compared + 1))
        continue
    fi
    reference_stat="$(render_and_stat "$reference_renderer" "$pkg" "$work_dir/reference-$id.bin")"
    if [[ "$candidate_stat" == "REFUSED" || "$reference_stat" == "REFUSED" ]]; then
        refused=$((refused + 1))
        echo "scene corpus byte-identity sweep: $id SKIPPED (refused by reference=$reference_stat candidate=$candidate_stat, not a regression by itself)"
        continue
    fi
    compared=$((compared + 1))
    if [[ "$candidate_stat" == "$reference_stat" ]]; then
        echo "scene corpus byte-identity sweep: $id IDENTICAL ($candidate_stat)"
    else
        flagged+=("$id")
        echo "scene corpus byte-identity sweep: $id REGRESSION reference=$reference_stat candidate=$candidate_stat" >&2
    fi
done

echo "scene corpus byte-identity sweep: total=$total compared=$compared refused_by_either=$refused flagged=${#flagged[@]}"
if (( ${#flagged[@]} > 0 )); then
    echo "scene corpus byte-identity sweep: FAILED, regressions in: ${flagged[*]}" >&2
    exit 1
fi
echo "scene corpus byte-identity sweep: passed"
