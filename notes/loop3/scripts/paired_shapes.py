#!/usr/bin/env python3
"""Compare two oracle shapes entity by entity, not by their totals.

  paired_shapes.py <scored.json> <shape-a> <shape-b>

`movie.specials` is `movie.noyear` with a `Specials/` directory inserted and
nothing else changed, so the two must give the **same entity the same verdict**.
Equal totals are not that: one shape can lose what the other gains and the
columns still agree. This joins on the entity.

The pairing is the guard. A rule that reads a `Specials/` directory as a season
directory — the rule that once scored as a free win here while destroying five
correct bindings in the real library — separates these two shapes, and nothing
else in the suite does.
"""
import collections
import json
import sys


def main():
    scored, a, b = sys.argv[1], sys.argv[2], sys.argv[3]
    d = json.load(open(scored))
    by = {a: {}, b: {}}
    for r in d["rows"]:
        if r["shape"] in by:
            by[r["shape"]][r["entity"]] = (r["verdict"], r.get("bound"))
    ea, eb = set(by[a]), set(by[b])
    print("%s: %d entities   %s: %d entities" % (a, len(ea), b, len(eb)))
    if ea != eb:
        print("!! entity sets differ: %d only in %s, %d only in %s — not a pair"
              % (len(ea - eb), a, len(eb - ea), b))
        return 1
    diff = collections.Counter()
    for e in ea:
        if by[a][e] != by[b][e]:
            diff[(by[a][e][0], by[b][e][0])] += 1
    print("entities joined: %d   differing: %d" % (len(ea), sum(diff.values())))
    for (va, vb), n in sorted(diff.items(), key=lambda kv: -kv[1]):
        print("   %-14s in %s -> %-14s in %s   %6d" % (va, a, vb, b, n))
    if not diff:
        print("   the two shapes give every entity the same verdict and the "
              "same binding")
    return 0


if __name__ == "__main__":
    sys.exit(main())
