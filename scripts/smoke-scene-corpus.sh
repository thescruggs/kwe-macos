#!/usr/bin/env bash
# SPDX-License-Identifier: GPL-3.0-or-later
# SR-0d smoke: scripts/scene-corpus-inventory.sh + scripts/scene-corpus-summarize.py
# over a synthetic 4-item corpus (no real Workshop content, nothing
# committed). Opt-in like the other smoke suites
# (KWE_RUN_SCENE_CORPUS_SMOKE=1; see scripts/check.sh).
#
# pkg-kind coverage lives in the kwe-scene-inspector Rust unit tests
# (crates/kwe-scene-inspector/src/main.rs's pkg_* tests, SR-0c); this smoke
# only needs to prove the shell/python harness wraps dir-kind items, a
# non-scene item, and a symlinked item correctly end to end.
#
#   KWE_RUN_SCENE_CORPUS_SMOKE=1 ./scripts/check.sh
#   ./scripts/smoke-scene-corpus.sh   # standalone
set -euo pipefail

project_root="$(cd -- "$(dirname -- "${BASH_SOURCE[0]}")/.." && pwd)"
target_dir="${CARGO_TARGET_DIR:-$project_root/target}"
smoke_root="$(mktemp -d -t kwe-scene-corpus-smoke.XXXXXX)"

cleanup() {
    rm -rf -- "$smoke_root"
}
trap cleanup EXIT INT TERM

fail() {
    echo "FAILED: $1" >&2
    exit 1
}

command -v jq >/dev/null || fail "jq is required for this smoke test"

cd "$project_root"
echo "scene corpus smoke: building kwe-scene-inspector"
cargo build -p kwe-scene-inspector >/dev/null
inspector="$target_dir/debug/kwe-scene-inspector"
[[ -x "$inspector" ]] || fail "kwe-scene-inspector did not build at $inspector"

corpus_dir="$smoke_root/corpus"
out_dir="$smoke_root/out"
mkdir -p "$corpus_dir/image-item" "$corpus_dir/text-item" "$corpus_dir/skipped-item"

# (1) A dir item with one visible image object.
cat >"$corpus_dir/image-item/scene.json" <<'EOF'
{"objects": [{"id": 1, "image": "textures/a.png", "visible": true}]}
EOF
# (2) A dir item with one visible text object -- deliberately a different
# capability from (1) so the smoke can assert both show up distinctly.
cat >"$corpus_dir/text-item/scene.json" <<'EOF'
{"objects": [{"id": 2, "text": "hi", "visible": true}]}
EOF
# (3) Neither scene.pkg nor scene.json -> skipped, no inspector run.
echo "not a scene" >"$corpus_dir/skipped-item/readme.txt"
# (4) A symlinked item dir -> skipped-symlink, never followed.
ln -s "$corpus_dir/image-item" "$corpus_dir/symlinked-item"

scripts/scene-corpus-inventory.sh \
    --corpus-dir "$corpus_dir" \
    --inspector "$inspector" \
    --out "$out_dir" \
    --per-item-timeout-s 5

records="$out_dir/records.ndjson"
summary="$out_dir/summary.json"

[[ -f "$records" ]] || fail "records.ndjson was not written"
[[ -f "$summary" ]] || fail "summary.json was not written"

line_count="$(wc -l <"$records")"
[[ "$line_count" -eq 4 ]] || fail "expected 4 NDJSON lines in records.ndjson, got $line_count"

status_for() {
    jq -r --arg item "$1" 'select(.item == $item) | .status' "$records"
}
[[ "$(status_for image-item)" == "inspected" ]] || fail "image-item must be inspected"
[[ "$(status_for text-item)" == "inspected" ]] || fail "text-item must be inspected"
[[ "$(status_for skipped-item)" == "skipped" ]] || fail "skipped-item must be skipped"
[[ "$(status_for symlinked-item)" == "skipped-symlink" ]] \
    || fail "symlinked-item must be skipped-symlink"

outcome_for() {
    jq -r --arg item "$1" 'select(.item == $item) | .record.outcome' "$records"
}
[[ "$(outcome_for image-item)" == "inventoried" ]] || fail "image-item must inventory cleanly"
[[ "$(outcome_for text-item)" == "inventoried" ]] || fail "text-item must inventory cleanly"
echo "records.ndjson: 4 items wrapped with the right statuses"

[[ "$(jq -r '.corpus_items.total' "$summary")" == "4" ]] \
    || fail "summary corpus_items.total must be 4"
[[ "$(jq -r '.corpus_items.inspected' "$summary")" == "2" ]] \
    || fail "summary corpus_items.inspected must be 2"
[[ "$(jq -r '.corpus_items.skipped' "$summary")" == "2" ]] \
    || fail "summary corpus_items.skipped must be 2"
[[ "$(jq -r '.outcomes["inventoried:ok"]' "$summary")" == "2" ]] \
    || fail "outcome histogram must count both inventoried:ok items"
echo "summary.json: corpus_items and outcome histogram match"

[[ "$(jq -r '.detected["scene.layer.image"].items' "$summary")" == "1" ]] \
    || fail "detected scene.layer.image items must be 1"
[[ "$(jq -r '.detected["scene.layer.text"].items' "$summary")" == "1" ]] \
    || fail "detected scene.layer.text items must be 1"
echo "summary.json: capability histogram has both scene.layer.image and scene.layer.text"

echo "scene corpus smoke passed: 4 items (2 inspected, 1 skipped, 1 skipped-symlink)"
