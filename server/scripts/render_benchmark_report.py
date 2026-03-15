#!/usr/bin/env python3

import argparse
import json
from pathlib import Path


def format_float(value: float) -> str:
    return f"{value:.2f}"


def scenario_key(item: dict) -> tuple:
    return (
        item.get("image_cache_backend", ""),
        item.get("endpoint", ""),
        item.get("hit_mode", ""),
        item.get("cache_size", 0),
        item.get("concurrency", 0),
    )


def build_markdown(report: dict) -> str:
    scenarios = sorted(report.get("scenarios", []), key=scenario_key)
    generated_at = report.get("generated_at", "unknown")
    note = report.get("note", "")

    lines = [
        "# Server Benchmark Report",
        "",
        f"- Generated at: `{generated_at}`",
        f"- Scenario count: `{len(scenarios)}`",
    ]

    if note:
        lines.append(f"- Note: {note}")

    lines.extend(
        [
            "",
            "## Scenario Summary",
            "",
            "| Backend | Endpoint | Hit Mode | Cache Size | Concurrency | Avg (ms) | P95 (ms) | Hit Avg (ms) | Miss Avg (ms) | Peak Mem (MB) | Peak CPU (%) |",
            "| --- | --- | --- | ---: | ---: | ---: | ---: | ---: | ---: | ---: | ---: |",
        ]
    )

    for scenario in scenarios:
        total_latency = scenario.get("total_latency", {})
        hit_latency = scenario.get("hit_latency", {})
        miss_latency = scenario.get("miss_latency", {})
        resources = scenario.get("resources", {})
        lines.append(
            "| {backend} | {endpoint} | {hit_mode} | {cache_size} | {concurrency} | {avg_ms} | {p95_ms} | {hit_avg_ms} | {miss_avg_ms} | {peak_memory_mb} | {peak_cpu_percent} |".format(
                backend=scenario.get("image_cache_backend", ""),
                endpoint=scenario.get("endpoint", ""),
                hit_mode=scenario.get("hit_mode", ""),
                cache_size=scenario.get("cache_size", 0),
                concurrency=scenario.get("concurrency", 0),
                avg_ms=format_float(total_latency.get("avg_ms", 0.0)),
                p95_ms=format_float(total_latency.get("p95_ms", 0.0)),
                hit_avg_ms=format_float(hit_latency.get("avg_ms", 0.0)),
                miss_avg_ms=format_float(miss_latency.get("avg_ms", 0.0)),
                peak_memory_mb=format_float(resources.get("peak_memory_mb", 0.0)),
                peak_cpu_percent=format_float(resources.get("peak_cpu_percent", 0.0)),
            )
        )

    if scenarios:
        slowest = max(scenarios, key=lambda item: item.get("total_latency", {}).get("avg_ms", 0.0))
        highest_memory = max(scenarios, key=lambda item: item.get("resources", {}).get("peak_memory_mb", 0.0))
        highest_cpu = max(scenarios, key=lambda item: item.get("resources", {}).get("peak_cpu_percent", 0.0))

        lines.extend(
            [
                "",
                "## Highlights",
                "",
                "- Slowest average latency: `{backend}/{endpoint}/{hit_mode}` at cache size `{cache_size}` and concurrency `{concurrency}` with `{avg_ms} ms`.".format(
                    backend=slowest.get("image_cache_backend", ""),
                    endpoint=slowest.get("endpoint", ""),
                    hit_mode=slowest.get("hit_mode", ""),
                    cache_size=slowest.get("cache_size", 0),
                    concurrency=slowest.get("concurrency", 0),
                    avg_ms=format_float(slowest.get("total_latency", {}).get("avg_ms", 0.0)),
                ),
                "- Highest peak memory: `{backend}/{endpoint}/{hit_mode}` at cache size `{cache_size}` and concurrency `{concurrency}` with `{peak_memory_mb} MB`.".format(
                    backend=highest_memory.get("image_cache_backend", ""),
                    endpoint=highest_memory.get("endpoint", ""),
                    hit_mode=highest_memory.get("hit_mode", ""),
                    cache_size=highest_memory.get("cache_size", 0),
                    concurrency=highest_memory.get("concurrency", 0),
                    peak_memory_mb=format_float(highest_memory.get("resources", {}).get("peak_memory_mb", 0.0)),
                ),
                "- Highest peak CPU: `{backend}/{endpoint}/{hit_mode}` at cache size `{cache_size}` and concurrency `{concurrency}` with `{peak_cpu_percent}%`.".format(
                    backend=highest_cpu.get("image_cache_backend", ""),
                    endpoint=highest_cpu.get("endpoint", ""),
                    hit_mode=highest_cpu.get("hit_mode", ""),
                    cache_size=highest_cpu.get("cache_size", 0),
                    concurrency=highest_cpu.get("concurrency", 0),
                    peak_cpu_percent=format_float(highest_cpu.get("resources", {}).get("peak_cpu_percent", 0.0)),
                ),
            ]
        )

    lines.append("")
    return "\n".join(lines)


def parse_args() -> argparse.Namespace:
    parser = argparse.ArgumentParser(description="Render a benchmark JSON report to Markdown.")
    parser.add_argument("--input", required=True, help="Path to benchmark-results.json")
    parser.add_argument("--output", required=True, help="Path to write the Markdown report")
    return parser.parse_args()


def main() -> int:
    args = parse_args()
    input_path = Path(args.input)
    output_path = Path(args.output)

    report = json.loads(input_path.read_text(encoding="utf-8"))
    markdown = build_markdown(report)

    output_path.parent.mkdir(parents=True, exist_ok=True)
    output_path.write_text(markdown, encoding="utf-8")
    return 0


if __name__ == "__main__":
    raise SystemExit(main())
