#!/usr/bin/env python3

import argparse
import json
from pathlib import Path


def scenario_key(item: dict) -> tuple:
    return (
        item.get("image_cache_backend", ""),
        item.get("endpoint", ""),
        item.get("hit_mode", ""),
        item.get("cache_size", 0),
        item.get("concurrency", 0),
    )


def parse_args() -> argparse.Namespace:
    parser = argparse.ArgumentParser(description="Merge multiple benchmark JSON reports.")
    parser.add_argument(
        "--inputs",
        nargs="+",
        required=True,
        help="Input benchmark JSON files",
    )
    parser.add_argument("--output", required=True, help="Output merged benchmark JSON file")
    return parser.parse_args()


def main() -> int:
    args = parse_args()
    merged = {
        "generated_at": "unknown",
        "note": "",
        "scenarios": [],
    }
    scenario_map: dict[tuple, dict] = {}

    for raw_path in args.inputs:
        path = Path(raw_path)
        report = json.loads(path.read_text(encoding="utf-8"))

        generated_at = report.get("generated_at", "unknown")
        if generated_at > merged["generated_at"]:
            merged["generated_at"] = generated_at

        note = report.get("note", "")
        if note and not merged["note"]:
            merged["note"] = note

        for scenario in report.get("scenarios", []):
            scenario_map[scenario_key(scenario)] = scenario

    merged["scenarios"] = [scenario_map[key] for key in sorted(scenario_map)]

    output_path = Path(args.output)
    output_path.parent.mkdir(parents=True, exist_ok=True)
    output_path.write_text(json.dumps(merged, indent=2), encoding="utf-8")
    return 0


if __name__ == "__main__":
    raise SystemExit(main())
