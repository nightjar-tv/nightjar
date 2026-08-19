#!/usr/bin/env python3
"""Compare two scored oracle runs, per shape and per verdict.

The judgement the loop makes is "did the oracle go up and did anything else
move", and a per-shape table answers both at once. Rows are joined on
(shape, path) so a shape that gained rows in `measured` cannot hide a shape
that lost them.

Correct% is over `measured = total - stalled`, so it can move for two reasons
at once: a verdict changed, or a row left the stalled class and entered the
denominator. Both deltas are printed for that reason — a rate on its own does
not say which happened.

Usage: compare.py <before-scored.json> <after-scored.json>
"""
import collections, json, sys

VERDICTS = ["correct", "wrong.entity", "wrong.ep", "partial", "absent", "stalled"]
WRONG = ("wrong.entity", "wrong.ep")


def load(p):
    rows = json.load(open(p))["rows"]
    unknown = {r["verdict"] for r in rows} - set(VERDICTS)
    if unknown:
        raise SystemExit("unknown verdicts in %s: %s" % (p, sorted(unknown)))
    return rows


before, after = load(sys.argv[1]), load(sys.argv[2])


def by_shape(rows):
    d = collections.defaultdict(collections.Counter)
    for r in rows:
        d[r["shape"]][r["verdict"]] += 1
    return d


b, a = by_shape(before), by_shape(after)


def rate(c):
    meas = sum(c[v] for v in VERDICTS if v != "stalled")
    return (100.0 * c["correct"] / meas) if meas else float("nan"), meas


hdr = "%-18s %8s %8s   %8s %8s   %7s %7s   %7s" % (
    "shape", "corr.b", "corr.a", "wrong.b", "wrong.a", "rate.b", "rate.a", "stall d")
print(hdr)
print("-" * len(hdr))
moved, still = [], []
for shape in sorted(set(b) | set(a)):
    cb, ca = b[shape], a[shape]
    rb, mb = rate(cb)
    ra, ma = rate(ca)
    wb, wa = sum(cb[v] for v in WRONG), sum(ca[v] for v in WRONG)
    line = "%-18s %8d %8d   %8d %8d   %6.1f%% %6.1f%%   %+7d" % (
        shape, cb["correct"], ca["correct"], wb, wa, rb, ra,
        ca["stalled"] - cb["stalled"])
    (moved if cb != ca else still).append((shape, line))

for _, line in moved:
    print(line + "   <-- moved")
for _, line in still:
    print(line)

print()
print("shapes that moved:     %d  (%s)" % (len(moved), ", ".join(s for s, _ in moved) or "none"))
print("shapes byte-identical: %d" % len(still))

print()
tb = collections.Counter(r["verdict"] for r in before)
ta = collections.Counter(r["verdict"] for r in after)
print("%-14s %9s %9s %9s" % ("verdict", "before", "after", "delta"))
for v in VERDICTS:
    print("%-14s %9d %9d %+9d" % (v, tb[v], ta[v], ta[v] - tb[v]))
mb = sum(tb[v] for v in VERDICTS if v != "stalled")
ma = sum(ta[v] for v in VERDICTS if v != "stalled")
print("%-14s %9d %9d %+9d" % ("measured", mb, ma, ma - mb))
print("%-14s %8.1f%% %8.1f%%  %+8.2f pt"
      % ("correct%", 100.0 * tb["correct"] / mb, 100.0 * ta["correct"] / ma,
         100.0 * ta["correct"] / ma - 100.0 * tb["correct"] / mb))

# Per-row transitions, so "wrong became correct" is told apart from
# "wrong became absent". A count of bound items cannot tell them apart.
print()
kb = {(r["shape"], r["path"]): r for r in before}
ka = {(r["shape"], r["path"]): r for r in after}
trans = collections.Counter()
for k, rb_ in kb.items():
    ra_ = ka.get(k)
    if ra_ is None:
        trans[(rb_["verdict"], "<row gone>")] += 1
    elif ra_["verdict"] != rb_["verdict"]:
        trans[(rb_["verdict"], ra_["verdict"])] += 1
    elif ra_["verdict"] == "correct" and ra_["bound"] != rb_["bound"]:
        trans[("correct", "correct/other-entity")] += 1
print("row transitions (before -> after):")
for (x, y), n in trans.most_common(20):
    print("   %-14s -> %-22s %6d" % (x, y, n))
if not trans:
    print("   none")
