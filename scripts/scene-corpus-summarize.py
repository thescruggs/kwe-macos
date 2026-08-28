#!/usr/bin/env python3
# SPDX-License-Identifier: GPL-3.0-or-later
"""SR-0d: aggregate scripts/scene-corpus-inventory.sh's records.ndjson into a
deterministic summary.json plus a compact stdout table.

Metadata only: this never reads or reports a Workshop item's title, tags, or
source bytes -- only the item basename (Workshop ID) already present in
records.ndjson, and the draft scene-feature-inventory-v0 record fields
(docs/SCENE_CAPABILITIES.md). stdlib only, matching this repo's existing
python3 scripts (scripts/scene-corpus-byte-identity-sweep.sh,
scripts/frame-read.py).

Determinism: summary.json is serialized with sort_keys=True, so byte-for-byte
identical input always produces a byte-for-byte identical file. The stdout
table additionally sorts each histogram by count descending, then name
ascending, for a human-readable "biggest first" ordering -- an ordering
property JSON's own key sort cannot express, which is why the two outputs
sort differently on purpose.
"""

import argparse
import json
import statistics
import sys
from collections import Counter, defaultdict
from datetime import datetime, timezone

# Mirrors docs/SCENE_CAPABILITIES.md's "top N sample paths" convention
# (SR-0c's own inventory.rs caps at 16 per record); the corpus-wide view
# affords a slightly larger window since it aggregates many records into one.
TOP_UNKNOWN_SAMPLES = 20


def load_records(path):
    """One JSON object per non-empty line, in scripts/scene-corpus-inventory.sh's
    wrapped shape: {"item", "status", "exit", "timed_out", "record", ...}."""
    records = []
    with open(path, "r", encoding="utf-8") as handle:
        for line_number, line in enumerate(handle, start=1):
            line = line.strip()
            if not line:
                continue
            try:
                records.append(json.loads(line))
            except json.JSONDecodeError as error:
                raise SystemExit(
                    f"scene-corpus-summarize: {path}:{line_number}: malformed NDJSON: {error}"
                )
    return records


def summarize(records):
    total = len(records)
    inspected = [entry for entry in records if entry.get("status") == "inspected"]
    skipped_count = total - len(inspected)

    timed_out = sum(1 for entry in inspected if entry.get("timed_out"))
    stdout_invalid = sum(1 for entry in inspected if entry.get("stdout_invalid"))

    outcomes = Counter()
    detected_items = defaultdict(set)
    detected_total = Counter()
    required_items = defaultdict(set)
    unknown_keys_total = 0
    unknown_types_total = 0
    unknown_objects_total = 0
    unknown_sample_items = defaultdict(set)
    limits_hit = Counter()
    wall_ms_values = []
    source_bytes_values = []
    builds = set()

    for entry in inspected:
        item = entry.get("item", "")
        record = entry.get("record")
        if not isinstance(record, dict):
            # A timeout with no stdout, or stdout_invalid: nothing further
            # to aggregate for this item, but it still counted toward
            # `inspected`/`timed_out`/`stdout_invalid` above.
            continue

        outcome = record.get("outcome", "unknown")
        reason = record.get("reason", "unknown")
        outcomes[f"{outcome}:{reason}"] += 1

        for capability in record.get("detected") or []:
            name = capability.get("capability")
            if not name:
                continue
            detected_items[name].add(item)
            detected_total[name] += capability.get("count", 0) or 0

        for capability in record.get("required") or []:
            required_items[capability].add(item)

        unknown = record.get("unknown") or {}
        unknown_keys_total += unknown.get("keys", 0) or 0
        unknown_types_total += unknown.get("types", 0) or 0
        unknown_objects_total += unknown.get("objects", 0) or 0
        for sample in unknown.get("samples") or []:
            unknown_sample_items[sample].add(item)

        bounds = record.get("bounds") or {}
        for code in bounds.get("limits_hit") or []:
            limits_hit[code] += 1
        wall_ms = bounds.get("wall_ms")
        if wall_ms is not None:
            wall_ms_values.append(wall_ms)

        content = record.get("content") or {}
        source_bytes = content.get("source_bytes")
        if source_bytes is not None:
            source_bytes_values.append(source_bytes)

        inspector = record.get("inspector") or {}
        if inspector.get("build"):
            builds.add(inspector["build"])

    top_unknown_samples = sorted(
        ((path, len(items)) for path, items in unknown_sample_items.items()),
        key=lambda pair: (-pair[1], pair[0]),
    )[:TOP_UNKNOWN_SAMPLES]

    return {
        "generated_utc": datetime.now(timezone.utc).strftime("%Y-%m-%dT%H:%M:%SZ"),
        "corpus_items": {
            "total": total,
            "skipped": skipped_count,
            "inspected": len(inspected),
        },
        "outcomes": dict(outcomes),
        "timed_out": timed_out,
        "stdout_invalid": stdout_invalid,
        "detected": {
            name: {"items": len(items), "total_count": detected_total[name]}
            for name, items in detected_items.items()
        },
        "required": {name: len(items) for name, items in required_items.items()},
        "unknown": {
            "keys": unknown_keys_total,
            "types": unknown_types_total,
            "objects": unknown_objects_total,
            "top_samples": top_unknown_samples,
        },
        "limits_hit": dict(limits_hit),
        "wall_ms": {
            "max": max(wall_ms_values) if wall_ms_values else 0,
            "median": statistics.median(wall_ms_values) if wall_ms_values else 0,
        },
        "source_bytes": {
            "max": max(source_bytes_values) if source_bytes_values else 0,
            "total": sum(source_bytes_values),
        },
        "inspector_build": sorted(builds),
    }


def print_table(summary):
    items = summary["corpus_items"]
    print(
        f"scene-corpus-summarize: {items['total']} items "
        f"(inspected={items['inspected']} skipped={items['skipped']})"
    )
    for key, count in sorted(summary["outcomes"].items(), key=lambda kv: (-kv[1], kv[0])):
        print(f"  outcome {key}: {count}")
    print(f"  timed_out: {summary['timed_out']}")
    print(f"  stdout_invalid: {summary['stdout_invalid']}")

    print("  detected (top 10):")
    top_detected = sorted(
        summary["detected"].items(), key=lambda kv: (-kv[1]["items"], kv[0])
    )[:10]
    for capability, stats in top_detected:
        print(f"    {capability}: items={stats['items']} total={stats['total_count']}")
    if not top_detected:
        print("    (none)")

    unknown = summary["unknown"]
    print(
        f"  unknown: keys={unknown['keys']} types={unknown['types']} "
        f"objects={unknown['objects']}"
    )
    print("  unknown top 5 samples:")
    top_samples = unknown["top_samples"][:5]
    for path, count in top_samples:
        print(f"    {path}: items={count}")
    if not top_samples:
        print("    (none)")


def main(argv):
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--records", required=True, help="records.ndjson path")
    parser.add_argument(
        "--summary-out", required=True, help="summary.json output path"
    )
    args = parser.parse_args(argv)

    records = load_records(args.records)
    summary = summarize(records)

    with open(args.summary_out, "w", encoding="utf-8") as handle:
        json.dump(summary, handle, indent=2, sort_keys=True)
        handle.write("\n")

    print_table(summary)
    return 0


if __name__ == "__main__":
    sys.exit(main(sys.argv[1:]))
