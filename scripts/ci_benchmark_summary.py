#!/usr/bin/env python3
"""Build Gate benchmark JSON + markdown delta for CI step summary.

Reads tee'd gate1_ci / gate1_scan output and an optional baseline JSON.
Reporting only — never exits non-zero on a regression.
"""
from __future__ import annotations

import json
import os
import pathlib
import re
import sys


def parse_ci(text: str) -> dict:
    out: dict = {}
    m = re.search(r"startup_ms samples=.* median=(\d+)", text)
    if m:
        out["startupMedianMs"] = int(m.group(1))
    m = re.search(r"idle_rss_mb_with_library=(\d+)", text)
    if m:
        out["idleRssMbWithLibrary"] = int(m.group(1))
    return out


def parse_scan(text: str) -> dict:
    out: dict = {}
    m = re.search(r'"index_s":\s*([0-9.]+)', text)
    if m:
        out["index10kSeconds"] = float(m.group(1))
    m = re.search(r"rescan_index_s=([0-9.]+)", text)
    if m:
        out["rescanSeconds"] = float(m.group(1))
    m = re.search(r"files_per_sec=([0-9.]+)", text)
    if m:
        out["probeFilesPerSec"] = float(m.group(1))
    return out


def delta_line(key: str, cur, base) -> str:
    if cur is None:
        return f"| `{key}` | — | {base} | unknown |"
    if base is None:
        return f"| `{key}` | {cur} | — | no baseline |"
    try:
        c = float(cur)
        b = float(base)
    except (TypeError, ValueError):
        return f"| `{key}` | {cur} | {base} | — |"
    diff = c - b
    if abs(b) < 1e-9:
        direction = "flat"
    elif diff > 0:
        direction = f"worse (+{diff:g})"
    elif diff < 0:
        direction = f"better ({diff:g})"
    else:
        direction = "flat"
    return f"| `{key}` | {cur} | {base} | {direction} |"


def main() -> int:
    if len(sys.argv) < 3:
        print(
            "usage: ci_benchmark_summary.py gate1_ci.txt gate1_scan.txt [baseline.json]",
            file=sys.stderr,
        )
        return 2
    ci_text = pathlib.Path(sys.argv[1]).read_text(errors="replace")
    scan_text = pathlib.Path(sys.argv[2]).read_text(errors="replace")
    baseline_path = pathlib.Path(sys.argv[3]) if len(sys.argv) > 3 else None

    metrics = parse_ci(ci_text)
    metrics.update(parse_scan(scan_text))
    bin_path = os.environ.get("NIGHTJAR_BIN")
    if bin_path and pathlib.Path(bin_path).is_file():
        metrics["releaseBinaryBytes"] = pathlib.Path(bin_path).stat().st_size

    baseline = {}
    if baseline_path and baseline_path.is_file():
        baseline = json.loads(baseline_path.read_text())

    print(json.dumps(metrics, indent=2))

    keys = [
        "startupMedianMs",
        "idleRssMbWithLibrary",
        "index10kSeconds",
        "rescanSeconds",
        "probeFilesPerSec",
        "releaseBinaryBytes",
    ]
    lines = [
        "## Gate benchmark delta",
        "",
        "Reporting only. Hard floors stay in the gate scripts.",
        "",
        "| metric | this run | baseline | direction |",
        "|---|---:|---:|---|",
    ]
    for k in keys:
        lines.append(delta_line(k, metrics.get(k), baseline.get(k)))
    summary = "\n".join(lines) + "\n"

    step = os.environ.get("GITHUB_STEP_SUMMARY")
    if step:
        with open(step, "a", encoding="utf-8") as fh:
            fh.write(summary)
    else:
        print(summary, file=sys.stderr)
    return 0


if __name__ == "__main__":
    raise SystemExit(main())
