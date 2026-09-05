#!/usr/bin/env python3
"""Probe-phase gate shared by the Gate 1 harness and its offline checks.

`scripts/gate1_scan_10k.sh` polls a scan job to completion, then hands the job
payload to `probe_report()`. The same function reads a stubbed payload from
stdin when this file runs as a script, so the gate's pass/fail logic is
exercised without a server. One code path serves both.

The gate's rules (why they exist):
- `probed == 0` is never a pass. A run that measured nothing fails, saying it
  measured nothing, not that throughput was acceptable. Zero is not a pass.
- `errors` must be 0. The bench corpus is 10,000 hardlinks to one valid seed
  MP4 (testdata/bench_10k.sh); nothing in it is unprobeable, so a nonzero
  count is a probe-infrastructure fault, the same failure the throughput
  floor exists to catch. If a future corpus legitimately contains
  unprobeable files, the bound belongs in an ADR, not in a silent tolerance
  kept here.
- `probeDurationMs` must be present once files were probed. Reading a missing
  duration as zero inflated files_per_sec into a passing number.

The throughput floor is a threshold owned by the harness (PROBE_FLOOR_FPS,
ADR-0004); this module never decides it.
"""

import json
import sys


def probe_report(job, probe_floor):
    """Return (report_line, fail_reason). A fail_reason of None is a pass."""
    probed = job.get("probed") or 0
    errors_raw = job.get("errors")
    errors = errors_raw or 0
    probe_ms = job.get("probeDurationMs")
    if probe_ms is not None:
        probe_s = max(probe_ms / 1000.0, 0.001)
        fps = probed / probe_s
        report = (
            f"probe_metric probed={probed} errors={errors} "
            f"probe_s={probe_s:.1f} files_per_sec={fps:.1f} floor={probe_floor}"
        )
    else:
        fps = None
        report = (
            f"probe_metric probed={probed} errors={errors} "
            f"probe_s=? files_per_sec=? floor={probe_floor}"
        )
    if probed == 0:
        return report, (
            "FAIL: probe measured nothing (probed=0); a run that probed no "
            "files cannot pass Gate 1"
        )
    if errors_raw is None:
        return report, (
            "FAIL: errors missing on a completed scan job; a missing count "
            "must not read as zero errors"
        )
    if errors != 0:
        return report, (
            f"FAIL: {errors} probe error(s); the 10k bench corpus must probe "
            "cleanly (errors must be 0)"
        )
    if probe_ms is None:
        return report, (
            f"FAIL: probeDurationMs missing after probing {probed} files; a "
            "missing duration must not read as a passing throughput"
        )
    if fps < probe_floor:
        return report, (
            f"FAIL: probe throughput {fps:.1f} files/sec < floor {probe_floor} "
            "(ADR-0004)"
        )
    return report, None


def main(argv):
    if len(argv) < 2:
        print(f"usage: {argv[0]} <probe_floor> < job.json", file=sys.stderr)
        return 2
    job = json.load(sys.stdin)
    report, fail_reason = probe_report(job, float(argv[1]))
    print(report)
    if fail_reason is not None:
        raise SystemExit(fail_reason)
    return 0


if __name__ == "__main__":
    raise SystemExit(main(sys.argv))
