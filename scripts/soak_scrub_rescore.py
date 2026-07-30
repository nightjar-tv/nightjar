#!/usr/bin/env python3
"""Re-score soak trial_*.json against the corrected resume criterion.

Corrected (Spike B closeout):
  !paused && readyState >= 3 && currentTime >= scrubS + ADVANCE_S
  && currentTime >= tLand + ADVANCE_S

Does not use FRAG_LOADED / BUFFER_APPENDED. Existing trials often early-exited
under the old `currentTime > scrubS + 0.3` branch (~0.35s past land), so a
strict ADVANCE_S=1.5 re-score of stored snaps is biased toward fail — report
both the strict table and a stuck-detection table (advance_from_land > 0).
"""
from __future__ import annotations

import argparse
import json
import math
from collections import defaultdict
from pathlib import Path


def load_final(data: dict) -> tuple[dict, float | None]:
    fin = dict(data.get("clientFinal") or {})
    t_land = None
    for e in data.get("allEvents") or []:
        if e.get("kind") != "final":
            continue
        detail = e.get("detail") or {}
        if isinstance(detail, dict):
            t_land = detail.get("tLand", t_land)
            for k in ("currentTime", "paused", "seeking", "readyState", "resumed"):
                if k in detail and k not in fin:
                    fin[k] = detail[k]
            if "currentTime" in detail:
                fin["currentTime"] = detail["currentTime"]
                fin["paused"] = detail.get("paused", fin.get("paused"))
                fin["seeking"] = detail.get("seeking", fin.get("seeking"))
                fin["readyState"] = detail.get("readyState", fin.get("readyState"))
    return fin, t_land


def score(fin: dict, scrub_s: float, t_land: float | None, advance_s: float) -> dict:
    ct = fin.get("currentTime")
    if ct is None:
        return {"ok": False, "reason": "no_currentTime"}
    if t_land is None:
        t_land = scrub_s
    paused = bool(fin.get("paused", True))
    rs = int(fin.get("readyState") or 0)
    past = ct - scrub_s
    moved = ct - t_land
    ok = (not paused) and rs >= 3 and past >= advance_s and moved >= advance_s
    return {
        "ok": ok,
        "paused": paused,
        "readyState": rs,
        "currentTime": ct,
        "tLand": t_land,
        "scrubS": scrub_s,
        "past_scrub": past,
        "adv_from_land": moved,
    }


def rule_of_three(n: int, fails: int) -> str:
    """95% upper bound on failure rate when fails==0: ~3/n."""
    if n <= 0:
        return "n=0"
    if fails == 0:
        return f"0/{n} ⇒ <~{100.0 * 3 / n:.2f}% at 95% (rule of three)"
    # Wilson-ish one-liner not required; just report rate
    return f"{fails}/{n} = {100.0 * fails / n:.2f}%"


def main() -> None:
    ap = argparse.ArgumentParser()
    ap.add_argument(
        "dirs",
        nargs="*",
        default=[],
        help="soak result dirs (default: newest /tmp/nj-soak-scrub-* with trials)",
    )
    ap.add_argument("--advance", type=float, default=1.5)
    args = ap.parse_args()
    dirs = [Path(d) for d in args.dirs]
    if not dirs:
        cands = sorted(
            Path("/tmp").glob("nj-soak-scrub-*"),
            key=lambda p: p.stat().st_mtime,
            reverse=True,
        )
        dirs = [p for p in cands if list(p.glob("trial_*.json"))][:3]

    advance = args.advance
    print(f"corrected criterion: !paused && readyState>=3 && ct>=scrub+{advance} && ct>=tLand+{advance}")
    print("(not FRAG_LOADED / BUFFER_APPENDED)\n")

    for d in dirs:
        trials = sorted(d.glob("trial_*.json"))
        if not trials:
            continue
        buckets = defaultdict(lambda: {"n": 0, "old_fail": 0, "strict_fail": 0, "stuck_fail": 0})
        early = []
        for p in trials:
            data = json.loads(p.read_text())
            fin, t_land = load_final(data)
            scrub_s = float(data.get("scrubMs") or 0) / 1000.0
            old = bool(data.get("resumed"))
            strict = score(fin, scrub_s, t_land, advance)
            # Stuck-at-land detector on stored snap: no motion from tLand.
            stuck = score(fin, scrub_s, t_land, 0.05)
            # invert: stuck_fail if no motion
            is_stuck = not stuck["ok"] or stuck["adv_from_land"] < 0.05
            key = (data.get("cell"), data.get("axis"))
            buckets[key]["n"] += 1
            if not old:
                buckets[key]["old_fail"] += 1
            if not strict["ok"]:
                buckets[key]["strict_fail"] += 1
            if is_stuck:
                buckets[key]["stuck_fail"] += 1
            if old and strict.get("past_scrub") is not None:
                early.append(strict["past_scrub"])

        print(f"=== {d} ({len(trials)} trials) ===")
        if early:
            early.sort()
            print(
                f"old-pass past_scrub at recorded final: "
                f"min={early[0]:.3f} med={early[len(early)//2]:.3f} max={early[-1]:.3f}"
            )
            if early[-1] < advance:
                print(
                    f"NOTE: recorded finals early-exited under old >scrub+0.3 branch "
                    f"(max past_scrub={early[-1]:.3f} < ADVANCE={advance}). "
                    f"Strict re-score of stored snaps is biased fail; "
                    f"stuck-detector (adv_from_land) is the fair read of this corpus."
                )
        print(f"{'cell':<4} {'axis':<14} {'n':>4} {'old_fail':>8} {'strict':>8} {'stuck':>8}")
        total_n = total_old = total_strict = total_stuck = 0
        for (cell, axis), s in sorted(buckets.items()):
            print(
                f"{cell:<4} {axis:<14} {s['n']:>4} {s['old_fail']:>8} "
                f"{s['strict_fail']:>8} {s['stuck_fail']:>8}"
            )
            total_n += s["n"]
            total_old += s["old_fail"]
            total_strict += s["strict_fail"]
            total_stuck += s["stuck_fail"]
        print(
            f"TOTAL old_fail={total_old}/{total_n}  "
            f"strict_fail={total_strict}/{total_n}  "
            f"stuck_fail={total_stuck}/{total_n}"
        )
        print(f"rule-of-three (old):    {rule_of_three(total_n, total_old)}")
        print(f"rule-of-three (stuck):  {rule_of_three(total_n, total_stuck)}")
        flips = total_strict - total_old
        print(
            f"re-score delta: strict flips {flips} additional fails vs old "
            f"(expected if early-exit bias)\n"
        )


if __name__ == "__main__":
    main()
