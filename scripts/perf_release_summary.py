#!/usr/bin/env python3
"""Fail-closed summarizer for slice PERF-1C release acceptance.

Reads the per-reader driver results, requires every cell in the plan's matrix,
and fails closed when a required cell is missing, has fewer than the required
measured requests, or records any request or correctness error. It then applies
the proposed warm targets only to the contracts they actually fit.

The summarizer is proven able to fail by `selftest`: it must report PASS on a
complete valid matrix, FAIL on a missing cell, on too few samples, and on a
request or correctness error, and it must detect a target breach (Rule 4.15).
When the matrix passes mechanically but an applicable proposed target fails, the
top-level `status` is `MECHANICAL_PASS_TARGETS_FAIL`, never plain `PASS`. A
target failure stays a finding: it never makes the summarizer exit nonzero.

Usage:
  perf_release_summary.py summarize --results cell-1r.json cell-4r.json \
      --endpoints units,detail,rail --manifest MANIFEST.json --out SUMMARY.json
  perf_release_summary.py selftest
"""

from __future__ import annotations

import argparse
import json
import sys
from pathlib import Path

# Plan PERF-1C item 2: 100k warm cells with one and four readers and at least
# 1,000 measured requests per endpoint/cell.
REQUIRED_READERS = (1, 4)
MIN_SAMPLES = 1000
# Plan "Outcome and boundaries": the proposed warm targets, defined for four
# active readers.
TARGET_READERS = 4
TARGET_P95_MS = 250.0
TARGET_P99_MS = 500.0

# Which measured contracts the proposed first-page-class target fits.
TARGET_CONTRACTS = {
    "detail": "real per-item detail contract named by the target",
    "rail": (
        "real rail contract named by the target; the server-side limit truncates "
        "after the full rollup, so the measured cost holds at any limit"
    ),
}
NON_TARGET_CONTRACTS = {
    "units": (
        "unpaginated complete-library response; the plan excludes it from a "
        "first-page SLO"
    ),
}


def _fail(message: str) -> None:
    print(f"perf_release_summary: {message}", file=sys.stderr)
    raise SystemExit(1)


def _cells_from_results(results: list[dict]) -> list[dict]:
    cells: list[dict] = []
    for result in results:
        readers = int(result["readers"])
        for endpoint, stats in result["endpoints"].items():
            cell = dict(stats)
            cell["endpoint"] = endpoint
            cell["readers"] = readers
            cell["server_after"] = result.get("server", {}).get("after")
            cell["database"] = result.get("database")
            cells.append(cell)
    return cells


def _evaluate(cells: list[dict], endpoints: list[str]) -> dict:
    index = {(cell.get("endpoint"), cell.get("readers")): cell for cell in cells}
    required = [(endpoint, readers) for endpoint in endpoints for readers in REQUIRED_READERS]

    missing: list[str] = []
    insufficient: list[dict] = []
    errored: list[dict] = []
    for endpoint, readers in required:
        label = f"{endpoint}/{readers}r"
        cell = index.get((endpoint, readers))
        if cell is None:
            missing.append(label)
            continue
        requests = int(cell.get("requests", 0))
        completed = int(cell.get("completed", 0))
        if requests < MIN_SAMPLES or completed < MIN_SAMPLES:
            insufficient.append(
                {"cell": label, "requests": requests, "completed": completed}
            )
        if int(cell.get("errors", 0)) > 0 or int(cell.get("correctness_errors", 0)) > 0:
            errored.append(
                {
                    "cell": label,
                    "errors": cell.get("errors"),
                    "correctness_errors": cell.get("correctness_errors"),
                }
            )

    targets: list[dict] = []
    for endpoint, readers in required:
        label = f"{endpoint}/{readers}r"
        cell = index.get((endpoint, readers))
        if endpoint in NON_TARGET_CONTRACTS:
            targets.append(
                {"cell": label, "applies": False, "reason": NON_TARGET_CONTRACTS[endpoint]}
            )
            continue
        if endpoint not in TARGET_CONTRACTS:
            targets.append(
                {
                    "cell": label,
                    "applies": False,
                    "reason": "endpoint is not named by the proposed target",
                }
            )
            continue
        if readers != TARGET_READERS:
            targets.append(
                {
                    "cell": label,
                    "applies": False,
                    "reason": "target is defined for four active readers",
                }
            )
            continue
        if cell is None:
            targets.append(
                {"cell": label, "applies": True, "pass": False, "reason": "cell missing"}
            )
            continue
        p95 = float(cell.get("p95_ms", float("inf")))
        p99 = float(cell.get("p99_ms", float("inf")))
        targets.append(
            {
                "cell": label,
                "applies": True,
                "p95_ms": p95,
                "p99_ms": p99,
                "p95_budget_ms": TARGET_P95_MS,
                "p99_budget_ms": TARGET_P99_MS,
                "p95_ok": p95 <= TARGET_P95_MS,
                "p99_ok": p99 <= TARGET_P99_MS,
                "pass": p95 <= TARGET_P95_MS and p99 <= TARGET_P99_MS,
                "reason": TARGET_CONTRACTS[endpoint],
            }
        )

    applicable = [target for target in targets if target["applies"]]
    mechanical_pass = not missing and not insufficient and not errored
    targets_pass = all(target.get("pass") for target in applicable)
    if not mechanical_pass:
        status = "FAIL"
    elif targets_pass:
        status = "PASS"
    else:
        status = "MECHANICAL_PASS_TARGETS_FAIL"
    return {
        "required_readers": list(REQUIRED_READERS),
        "required_endpoints": list(endpoints),
        "min_samples": MIN_SAMPLES,
        "missing_cells": missing,
        "insufficient_cells": insufficient,
        "error_cells": errored,
        "mechanical_pass": mechanical_pass,
        "targets": targets,
        "targets_pass": targets_pass,
        "status": status,
    }


def _cmd_summarize(args: argparse.Namespace) -> int:
    endpoints = [name.strip() for name in args.endpoints.split(",") if name.strip()]
    results = [json.loads(Path(path).read_text()) for path in args.results]
    cells = _cells_from_results(results)
    report = _evaluate(cells, endpoints)
    report["results"] = [str(Path(path).resolve()) for path in args.results]
    if args.manifest:
        manifest = json.loads(Path(args.manifest).read_text())
        report["manifest_sha256"] = manifest.get("manifest_sha256")
        report["library_counts"] = manifest.get("counts")
    report["cells"] = [
        {
            key: cell.get(key)
            for key in (
                "endpoint",
                "readers",
                "requests",
                "completed",
                "errors",
                "correctness_errors",
                "p50_ms",
                "p95_ms",
                "p99_ms",
                "max_ms",
                "wall_ms",
                "bytes_p50",
                "bytes_max",
                "server_after",
                "database",
            )
        }
        for cell in sorted(cells, key=lambda cell: (cell["endpoint"], cell["readers"]))
    ]
    output = Path(args.out).resolve()
    output.parent.mkdir(parents=True, exist_ok=True)
    output.write_text(json.dumps(report, indent=2, sort_keys=True) + "\n")
    print(json.dumps(report, sort_keys=True))
    return 0 if report["mechanical_pass"] else 1


def _selftest_cells() -> list[dict]:
    cells: list[dict] = []
    for endpoint in ("units", "detail", "rail"):
        for readers in REQUIRED_READERS:
            cells.append(
                {
                    "endpoint": endpoint,
                    "readers": readers,
                    "requests": MIN_SAMPLES,
                    "completed": MIN_SAMPLES,
                    "errors": 0,
                    "correctness_errors": 0,
                    "p50_ms": 10.0,
                    "p95_ms": 20.0,
                    "p99_ms": 30.0,
                    "max_ms": 40.0,
                }
            )
    return cells


def _cmd_selftest(_: argparse.Namespace) -> int:
    endpoints = ("units", "detail", "rail")
    checks: list[tuple[str, bool]] = []

    report = _evaluate(_selftest_cells(), endpoints)
    checks.append(("complete matrix passes mechanically", report["mechanical_pass"] is True))
    checks.append(("complete matrix meets the proposed targets", report["targets_pass"] is True))
    checks.append(("complete matrix reports PASS", report["status"] == "PASS"))

    missing = _selftest_cells()
    missing.pop(0)
    report = _evaluate(missing, endpoints)
    checks.append(
        (
            "missing cell fails closed",
            report["mechanical_pass"] is False and bool(report["missing_cells"]),
        )
    )
    checks.append(("missing cell reports FAIL", report["status"] == "FAIL"))

    short = _selftest_cells()
    short[0]["requests"] = MIN_SAMPLES - 1
    short[0]["completed"] = MIN_SAMPLES - 1
    report = _evaluate(short, endpoints)
    checks.append(
        (
            "insufficient samples fail closed",
            report["mechanical_pass"] is False and bool(report["insufficient_cells"]),
        )
    )

    errored = _selftest_cells()
    errored[0]["errors"] = 1
    report = _evaluate(errored, endpoints)
    checks.append(
        (
            "request error fails closed",
            report["mechanical_pass"] is False and bool(report["error_cells"]),
        )
    )

    incorrect = _selftest_cells()
    incorrect[0]["correctness_errors"] = 1
    report = _evaluate(incorrect, endpoints)
    checks.append(
        (
            "correctness error fails closed",
            report["mechanical_pass"] is False and bool(report["error_cells"]),
        )
    )

    breached = _selftest_cells()
    for cell in breached:
        if cell["endpoint"] == "rail" and cell["readers"] == TARGET_READERS:
            cell["p95_ms"] = TARGET_P95_MS + 1.0
    report = _evaluate(breached, endpoints)
    checks.append(
        (
            "target breach is detected",
            report["mechanical_pass"] is True and report["targets_pass"] is False,
        )
    )
    checks.append(
        (
            "target breach does not report plain PASS",
            report["status"] == "MECHANICAL_PASS_TARGETS_FAIL"
            and report["status"] != "PASS",
        )
    )

    units_only = _evaluate(_selftest_cells(), ("units",))
    checks.append(
        (
            "units is not target-applicable",
            all(
                not target["applies"]
                for target in units_only["targets"]
                if target["cell"].startswith("units/")
            ),
        )
    )

    passed = all(result for _, result in checks)
    print(
        json.dumps(
            {
                "checks": [{"name": name, "pass": result} for name, result in checks],
                "pass": passed,
            },
            sort_keys=True,
        )
    )
    return 0 if passed else 1


def main(argv: list[str]) -> int:
    parser = argparse.ArgumentParser(description=__doc__)
    sub = parser.add_subparsers(dest="command", required=True)

    summarize = sub.add_parser(
        "summarize", help="require the full cell matrix and apply the targets"
    )
    summarize.add_argument("--results", nargs="+", required=True)
    summarize.add_argument("--endpoints", default="units,detail,rail")
    summarize.add_argument("--manifest")
    summarize.add_argument("--out", required=True)
    summarize.set_defaults(func=_cmd_summarize)

    selftest = sub.add_parser(
        "selftest", help="prove the summarizer detects every failure class"
    )
    selftest.set_defaults(func=_cmd_selftest)

    args = parser.parse_args(argv)
    return args.func(args)


if __name__ == "__main__":
    raise SystemExit(main(sys.argv[1:]))
