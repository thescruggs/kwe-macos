#!/usr/bin/env bash
# SPDX-License-Identifier: GPL-3.0-or-later
# SR-0d: a local, metadata-only inventory run over a directory of Wallpaper
# Engine Workshop items using kwe-scene-inspector (SR-0b/c). Nothing this
# script produces is ever committed; no source bytes leave the machine;
# Workshop IDs (item basenames) may appear in the local records only,
# matching the existing content policy for the local corpus (see
# scripts/smoke-corpus-pkg.sh and scripts/scene-corpus-byte-identity-sweep.sh,
# neither of which commit or transmit corpus content either).
#
# Conductor scope decisions (docs/SR0.md SR-0d):
#  (1) This script invokes kwe-scene-inspector DIRECTLY under `timeout`, NOT
#      through the daemon's scene.inspect RPC
#      (crates/kwe-daemon/src/inspect.rs). This is an uncommitted-output
#      local lab harness over the maintainer's own corpus; the inspector is
#      still its own process with its own bounds (byte/time caps, a bounded
#      report size), but it does NOT get the daemon's containment (private
#      HOME, PDEATHSIG, rlimits, process-group kill). Daemon-grade
#      containment for inspection remains the production path
#      (scene.inspect); this script is a maintainer-only convenience.
#  (2) The original SR-0d line "captures the current S7d failure cases as
#      reproducible local diagnostic records" is OUT of scope here: those
#      diagnoses and maintainer reports already exist locally
#      (~/.local/share/kwe/reports/, project memory notes), and a
#      renderer-side capture harness is not inventory work.
#
# Usage:
#   scripts/scene-corpus-inventory.sh --corpus-dir /path/to/workshop/content/431960 \
#       [--inspector <path>] [--out <dir>] [--per-item-timeout-s <n>] \
#       [--max-source-mib <n>]
set -euo pipefail

project_root="$(cd -- "$(dirname -- "${BASH_SOURCE[0]}")/.." && pwd)"
target_dir="${CARGO_TARGET_DIR:-$project_root/target}"

corpus_dir=""
inspector="$target_dir/debug/kwe-scene-inspector"
out_dir=""
per_item_timeout_s=15
max_source_mib=512

usage() {
    cat >&2 <<EOF
usage: $(basename -- "$0") --corpus-dir <dir> [--inspector <path>] [--out <dir>]
       [--per-item-timeout-s <n>] [--max-source-mib <n>]
EOF
}

while [[ $# -gt 0 ]]; do
    case "$1" in
        --corpus-dir)
            corpus_dir="${2:-}"
            shift 2
            ;;
        --inspector)
            inspector="${2:-}"
            shift 2
            ;;
        --out)
            out_dir="${2:-}"
            shift 2
            ;;
        --per-item-timeout-s)
            per_item_timeout_s="${2:-}"
            shift 2
            ;;
        --max-source-mib)
            max_source_mib="${2:-}"
            shift 2
            ;;
        -h | --help)
            usage
            exit 0
            ;;
        *)
            echo "scene-corpus-inventory: unknown argument: $1" >&2
            usage
            exit 2
            ;;
    esac
done

if [[ -z "$corpus_dir" ]]; then
    echo "scene-corpus-inventory: --corpus-dir is required" >&2
    usage
    exit 2
fi
if [[ ! -d "$corpus_dir" ]]; then
    echo "scene-corpus-inventory: --corpus-dir $corpus_dir does not exist" >&2
    exit 2
fi
if [[ ! -x "$inspector" ]]; then
    echo "scene-corpus-inventory: inspector binary not found or not executable: $inspector" >&2
    echo "  (build it with: cargo build -p kwe-scene-inspector)" >&2
    exit 1
fi
if ! [[ "$per_item_timeout_s" =~ ^[1-9][0-9]*$ ]]; then
    echo "scene-corpus-inventory: --per-item-timeout-s must be a positive integer" >&2
    exit 2
fi
if ! [[ "$max_source_mib" =~ ^[0-9]+$ ]]; then
    echo "scene-corpus-inventory: --max-source-mib must be a non-negative integer" >&2
    exit 2
fi

if [[ -z "$out_dir" ]]; then
    data_home="${XDG_DATA_HOME:-$HOME/.local/share}"
    out_dir="$data_home/kwe/corpus/$(date -u +%Y%m%d-%H%M%S)"
fi

if ! mkdir -p "$out_dir"; then
    echo "scene-corpus-inventory: cannot create out dir $out_dir" >&2
    exit 1
fi
# Private, like every other local record the daemon writes (0700 HOME dirs,
# ~/.local/share/kwe/reports).
chmod 700 "$out_dir"
mkdir -p "$out_dir/stderr"
if [[ ! -w "$out_dir" ]]; then
    echo "scene-corpus-inventory: out dir $out_dir is not writable" >&2
    exit 1
fi

records_file="$out_dir/records.ndjson"
: >"$records_file"

# Wraps one NDJSON line, delegating JSON construction/escaping and inspector
# stdout validation to python3 so a malformed inspector report or an
# unusual item basename can never corrupt the NDJSON file. Defined once and
# reused (via `python3 -c`) for every item, including skipped ones, so
# every line in records.ndjson has the identical shape:
#   {"item", "status", "exit", "timed_out", "record", ["stdout_invalid"]}
# `status` is one of: inspected, skipped, skipped-symlink.
wrap_record_py="$(cat <<'PY'
import json
import sys

item, status, exit_code_raw, timed_out_raw, raw_stdout = sys.argv[1:6]
exit_code = int(exit_code_raw) if exit_code_raw != "" else None
timed_out = timed_out_raw == "true"

record = None
stdout_invalid = False
stripped = raw_stdout.strip()
if stripped:
    try:
        parsed = json.loads(stripped)
    except json.JSONDecodeError:
        stdout_invalid = True
    else:
        if isinstance(parsed, dict):
            record = parsed
        else:
            stdout_invalid = True

result = {
    "item": item,
    "status": status,
    "exit": exit_code,
    "timed_out": timed_out,
    "record": record,
}
if stdout_invalid:
    result["stdout_invalid"] = True
print(json.dumps(result, sort_keys=True))
PY
)"

emit_record() {
    python3 -c "$wrap_record_py" "$1" "$2" "$3" "$4" "$5" >>"$records_file"
}

echo "scene-corpus-inventory: corpus=$corpus_dir out=$out_dir"

item_count=0
inspected_count=0
mapfile -d '' -t entries < <(find "$corpus_dir" -mindepth 1 -maxdepth 1 -print0 | sort -z)
for entry in "${entries[@]}"; do
    item="$(basename -- "$entry")"
    item_count=$((item_count + 1))

    # Never follow a symlinked item dir (test with -L before anything that
    # would traverse it).
    if [[ -L "$entry" ]]; then
        emit_record "$item" "skipped-symlink" "" "false" ""
        continue
    fi
    if [[ ! -d "$entry" ]]; then
        emit_record "$item" "skipped" "" "false" ""
        continue
    fi

    if [[ -f "$entry/scene.pkg" ]]; then
        target="$entry/scene.pkg"
    elif [[ -f "$entry/scene.json" ]]; then
        target="$entry"
    else
        emit_record "$item" "skipped" "" "false" ""
        continue
    fi

    stderr_tmp="$out_dir/stderr/$item.log.tmp"
    stdout=""
    exit_code=0
    # A per-item failure (nonzero exit, timeout) must not abort the whole
    # corpus run — only capture-and-record it.
    set +e
    stdout="$(timeout "${per_item_timeout_s}s" "$inspector" \
        --input "$target" --max-source-mib "$max_source_mib" 2>"$stderr_tmp")"
    exit_code=$?
    set -e
    head -c 4096 "$stderr_tmp" >"$out_dir/stderr/$item.log"
    rm -f "$stderr_tmp"

    timed_out="false"
    if [[ "$exit_code" -eq 124 ]]; then
        timed_out="true"
    fi

    emit_record "$item" "inspected" "$exit_code" "$timed_out" "$stdout"
    inspected_count=$((inspected_count + 1))
done

echo "scene-corpus-inventory: $item_count items ($inspected_count inspected); running the aggregator"
python3 "$project_root/scripts/scene-corpus-summarize.py" \
    --records "$records_file" \
    --summary-out "$out_dir/summary.json"

echo "scene-corpus-inventory: records=$records_file summary=$out_dir/summary.json"
