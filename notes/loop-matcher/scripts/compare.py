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

# The scorer's own label set, transcribed from `score_binding.py`'s ORDER
# rather than guessed. Guessing it has now cost twice: once reporting wrong=0 for
# a shape with 2,553 wrong, and once aborting on `wrong.unknownepisode`, a label
# that only appears after warming.
#
# `wrong.unknownepisode` is NOT known to be a wrong binding. It means the file
# carries an episode link whose id the scorer cannot place, because it rebuilds
# id -> (season, episode) from cached season payloads and that season is not
# cached. A correct bind into an uncached season looks identical. It is counted
# apart from `wrong.entity` for exactly that reason.
VERDICTS = ["correct", "wrong.entity", "wrong.episode", "wrong.unknownepisode",
            "partial", "absent", "stalled"]
WRONG = ("wrong.entity", "wrong.episode")   # unknownepisode excluded: unproven


# **The row key must include the batch.** `(shape, path)` is not unique: two
# entities with the same title and no year render the same relpath — two films
# called `Aladdin` both become `Aladdin/Aladdin.1080p.BluRay.mkv` — and
# `gen_library.py` puts them in different batches precisely so they cannot share
# a database. Keying on `(shape, path)` silently dropped 120 of 67,982 rows from
# every transition table, while the verdict totals stayed right. A join that
# loses rows quietly is worse than one that fails.
def rowkey(r):
    return (r["shape"], r["batch"], r["path"])


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
kb = {rowkey(r): r for r in before}
ka = {rowkey(r): r for r in after}
assert len(kb) == len(before) and len(ka) == len(after), "row key is not unique"
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
