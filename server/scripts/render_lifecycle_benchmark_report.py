#!/usr/bin/env python3

import argparse
import json
from pathlib import Path


def format_float(value: float) -> str:
    return f"{value:.2f}"


def parse_args() -> argparse.Namespace:
    parser = argparse.ArgumentParser(
        description="Render a lifecycle benchmark JSON report to Markdown."
    )
    parser.add_argument(
        "--input", required=True, help="Path to lifecycle-bench-results.json"
    )
    parser.add_argument("--output", required=True, help="Path to write the Markdown report")
    return parser.parse_args()


def build_markdown(report: dict) -> str:
    generated_at = report.get("generated_at", "unknown")
    phase_duration_secs = report.get("phase_duration_secs", 0)
    cache_size = report.get("cache_size", 0)
    note = report.get("note", "")
    results = report.get("results", [])

    lines = [
        "# Lifecycle Benchmark Report",
        "",
        f"- Generated at: `{generated_at}`",
        f"- Phase duration: `{phase_duration_secs}` seconds",
        f"- Cache size: `{cache_size}`",
    ]

    if note:
        lines.append(f"- Note: {note}")

    lines.extend(
        [
            "",
            "## Phase Summary",
            "",
            "| Memory Mode | Phase | Avg RPS | Avg Lat (ms) | P95 Lat (ms) | P99 Lat (ms) | Peak Mem (MB) | Peak CPU (%) | Success Ratio |",
            "| --- | --- | ---: | ---: | ---: | ---: | ---: | ---: | ---: |",
        ]
    )

    for mode in results:
        mode_name = mode.get("memory_mode", "")
        for phase in mode.get("phases", []):
            resources = phase.get("resources", {})
            latency = phase.get("latency", {})
            lines.append(
                "| {mode} | {phase} | {avg_rps} | {avg_ms} | {p95_ms} | {p99_ms} | {peak_mem} | {peak_cpu} | {success_ratio} |".format(
                    mode=mode_name,
                    phase=phase.get("phase", ""),
                    avg_rps=format_float(phase.get("avg_rps", 0.0)),
                    avg_ms=format_float(latency.get("avg_ms", 0.0)),
                    p95_ms=format_float(latency.get("p95_ms", 0.0)),
                    p99_ms=format_float(latency.get("p99_ms", 0.0)),
                    peak_mem=format_float(resources.get("peak_memory_mb", 0.0)),
                    peak_cpu=format_float(resources.get("peak_cpu_percent", 0.0)),
                    success_ratio=f"{phase.get('success_ratio', 0.0):.4f}",
                )
            )

    for mode in results:
        mode_name = mode.get("memory_mode", "")
        idle_phase = next(
            (phase for phase in mode.get("phases", []) if phase.get("phase") == "idle"),
            None,
        )
        if not idle_phase:
            continue

        memory_release = idle_phase.get("memory_release")
        if not memory_release:
            continue

        lines.extend(
            [
                "",
                f"## Idle Memory Release: `{mode_name}`",
                "",
                "| Start Mem (MB) | End Mem (MB) | Released (MB) | Release Ratio | Time to 50% (s) | Time to 80% (s) |",
                "| ---: | ---: | ---: | ---: | ---: | ---: |",
                "| {start_mb} | {end_mb} | {released_mb} | {release_ratio} | {time_50} | {time_80} |".format(
                    start_mb=format_float(memory_release.get("memory_at_phase_start_mb", 0.0)),
                    end_mb=format_float(memory_release.get("memory_at_phase_end_mb", 0.0)),
                    released_mb=format_float(memory_release.get("memory_released_mb", 0.0)),
                    release_ratio=f"{memory_release.get('release_ratio', 0.0) * 100:.2f}%",
                    time_50=format_float(
                        memory_release.get("time_to_50pct_release_secs", 0.0)
                    )
                    if memory_release.get("time_to_50pct_release_secs") is not None
                    else "N/A",
                    time_80=format_float(
                        memory_release.get("time_to_80pct_release_secs", 0.0)
                    )
                    if memory_release.get("time_to_80pct_release_secs") is not None
                    else "N/A",
                ),
            ]
        )

    lines.append("")
    return "\n".join(lines)


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
