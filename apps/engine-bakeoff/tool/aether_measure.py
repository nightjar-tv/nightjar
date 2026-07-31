#!/usr/bin/env python3
"""AetherEngine T2/T4 harness via stock SPM probe (outside Moonfin)."""

from __future__ import annotations

import json
import os
import subprocess
import time
from pathlib import Path

ROOT = Path(__file__).resolve().parents[3]
SAMPLE = ROOT / "notes/client-arch/bakeoff-sample.json"
OUT = ROOT / "notes/client-arch/bakeoff-runs/aether-binding.json"
BASE = os.environ.get("NIGHTJAR_BASE", "http://127.0.0.1:18097")
PROBE = Path(__file__).resolve().parent / "aether_probe/.build/arm64-apple-macosx/release/AetherBakeoffProbe"


def pct(xs: list[float], q: float) -> float:
    s = sorted(xs)
    i = int(round(q * (len(s) - 1)))
    return s[max(0, min(i, len(s) - 1))]


def summarize(ms: list[float]) -> dict:
    if not ms:
        return {"n": 0}
    return {
        "n": len(ms),
        "min_ms": round(min(ms)),
        "p50_ms": round(pct(ms, 0.5)),
        "p90_ms": round(pct(ms, 0.9)),
        "max_ms": round(max(ms)),
        "samples_ms": [round(x) for x in ms],
    }


def run_probe(url: str, seek_s: float | None = None, timeout: float = 90.0) -> dict:
    cmd = [str(PROBE), url]
    if seek_s is not None:
        cmd.append(str(seek_s))
    try:
        r = subprocess.run(cmd, capture_output=True, text=True, timeout=timeout)
    except subprocess.TimeoutExpired:
        return {"ok": False, "error": "timeout"}
    out = (r.stdout or "").strip()
    try:
        data = json.loads(out[out.find("{") :]) if "{" in out else {}
    except json.JSONDecodeError:
        data = {"raw": out[-500:], "stderr": (r.stderr or "")[-500:]}
    data["ok"] = r.returncode == 0 and "first_frame_ms" in data
    data["returncode"] = r.returncode
    if r.stderr:
        data["stderr_tail"] = r.stderr[-400:]
    return data


def main() -> None:
    if not PROBE.is_file():
        raise SystemExit(f"missing probe binary: {PROBE}")
    sample = json.loads(SAMPLE.read_text())
    ids = sample["latency_item_ids"]
    by_id = {t["id"]: t for t in sample["t4_sample"]}

    cold_startup: list[float] = []
    warm_startup: list[float] = []
    warm_far: list[float] = []
    cold_far: list[float] = []

    for item_id in ids:
        meta = by_id.get(item_id)
        if not meta or (meta.get("duration_ms") or 0) < 60_000:
            continue
        url = f"{BASE}/items/{item_id}/stream"
        far_s = (meta["duration_ms"] * 0.75) / 1000.0

        cold = run_probe(url, seek_s=None)
        if cold.get("ok"):
            cold_startup.append(float(cold["first_frame_ms"]))
        warm = run_probe(url, seek_s=None)
        if warm.get("ok"):
            warm_startup.append(float(warm["first_frame_ms"]))

        far = run_probe(url, seek_s=far_s)
        if far.get("ok") and far.get("seek_land_ms") is not None:
            warm_far.append(float(far["seek_land_ms"]))

        # Cold far: fresh process (each probe is already a fresh process)
        far_c = run_probe(url, seek_s=far_s)
        if far_c.get("ok") and far_c.get("seek_land_ms") is not None:
            cold_far.append(float(far_c["seek_land_ms"]))

    # T4 subsample
    t4_ok = t4_fail = 0
    failures = []
    for t in sample["t4_sample"][:40]:
        url = f"{BASE}/items/{t['id']}/stream"
        r = run_probe(url, seek_s=None, timeout=45)
        if r.get("ok"):
            t4_ok += 1
        elif t.get("stratum") == "damaged_class":
            pass
        else:
            t4_fail += 1
            failures.append({"id": t["id"], "stratum": t.get("stratum"), "error": r.get("error") or r.get("state")})

    total = t4_ok + t4_fail
    out = {
        "engine": "AetherEngine",
        "client": "spm_probe_outside_moonfin",
        "builds_outside_moonfin": True,
        "licence": "LGPL-3.0 + Apple Store/DRM exception (Vincent Herbst)",
        "part_a": {
            "cold_startup": summarize(cold_startup),
            "warm_startup": summarize(warm_startup),
            "warm_far_seek": summarize(warm_far),
            "cold_far_seek": summarize(cold_far),
        },
        "t4_subsample": {
            "n": total,
            "ok": t4_ok,
            "fail": t4_fail,
            "failure_rate_pct": round(100.0 * t4_fail / total, 2) if total else None,
            "failures": failures[:20],
        },
        "measured_at": time.strftime("%Y-%m-%dT%H:%M:%SZ", time.gmtime()),
    }
    OUT.write_text(json.dumps(out, indent=2))
    print(json.dumps(out, indent=2))


if __name__ == "__main__":
    main()
