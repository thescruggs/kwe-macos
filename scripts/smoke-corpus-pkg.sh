#!/usr/bin/env bash
# SPDX-License-Identifier: Apache-2.0
# Corpus scene.pkg smoke (BETA_M3b): run the kwe-cli structural preflight
# over real Workshop scene packages and record version/count/safe stats.
# Reproduces the M3b corpus evidence (docs/BETA_M3.md acceptance table) on
# any machine that has the corpus.
#
#   KWE_CORPUS_DIR=/path/to/workshop/content/431960 ./scripts/smoke-corpus-pkg.sh
#
# The directory may hold scene.pkg files directly or one directory per
# wallpaper (the Steam layout). SKIPPED with exit 0 when KWE_CORPUS_DIR is
# unset or missing; a set-but-empty corpus dir fails loudly instead.
set -euo pipefail

project_root="$(cd -- "$(dirname -- "${BASH_SOURCE[0]}")/.." && pwd)"
target_dir="${CARGO_TARGET_DIR:-$project_root/target}"
corpus="${KWE_CORPUS_DIR:-}"

if [[ -z "$corpus" ]]; then
    echo "corpus pkg smoke: SKIPPED (KWE_CORPUS_DIR is not set)"
    exit 0
fi
if [[ ! -d "$corpus" ]]; then
    echo "corpus pkg smoke: SKIPPED (KWE_CORPUS_DIR=$corpus does not exist)"
    exit 0
fi

echo "corpus pkg smoke: building workspace"
cargo build --workspace >/dev/null

mapfile -t pkgs < <(find "$corpus" -maxdepth 2 -name scene.pkg | sort)
if (( ${#pkgs[@]} == 0 )); then
    echo "corpus pkg smoke: FAILED ($corpus contains no scene.pkg files)" >&2
    exit 1
fi

safe=0
unsafe=()
declare -A versions=()
total_entries=0
min_size=0
max_size=0
for pkg in "${pkgs[@]}"; do
    read -r version count size < <(python3 - "$pkg" <<'PY'
import struct
import os
import sys

path = sys.argv[1]
data = open(path, "rb").read(64)
magic_len = struct.unpack("<I", data[:4])[0]
magic = data[4 : 4 + magic_len].decode("ascii", "replace")
count = struct.unpack("<I", data[4 + magic_len : 8 + magic_len])[0]
print(magic[4:], count, os.path.getsize(path))
PY
    )
    versions["$version"]=$(( ${versions["$version"]:-0} + 1 ))
    total_entries=$(( total_entries + count ))
    (( min_size == 0 || size < min_size )) && min_size=$size
    (( size > max_size )) && max_size=$size
    verdict="$("$target_dir/debug/kwe" preflight --path "$pkg" 2>/dev/null || true)"
    if [[ "$(jq -r '.safe' <<<"$verdict")" == "true" ]]; then
        safe=$(( safe + 1 ))
    else
        unsafe+=("$(basename "$(dirname "$pkg")"): $(jq -r '.reasons | join("; ")' <<<"$verdict")")
    fi
done

version_list="$(printf '%s ' "${!versions[@]}" | tr ' ' '\n' | sort | tr '\n' ' ')"
version_hist=""
for v in $(printf '%s\n' "${!versions[@]}" | sort); do
    version_hist="${version_hist}${v}x${versions[$v]} "
done

echo "corpus pkg smoke: pkgs=$safe/${#pkgs[@]} safe entries=$total_entries versions=${#versions[@]} ($version_list) size_min=$min_size size_max=$max_size"
echo "corpus pkg smoke: version histogram: $version_hist"
if (( ${#unsafe[@]} > 0 )); then
    for reason in "${unsafe[@]}"; do
        echo "corpus pkg smoke: FAILED: $reason" >&2
    done
    echo "corpus pkg smoke: FAILED (${#unsafe[@]} packages rejected)" >&2
    exit 1
fi
echo "corpus pkg smoke passed: $safe/${#pkgs[@]} real scene packages preflight clean"
