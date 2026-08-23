#!/usr/bin/env python3
"""Split `tv.mixedroot`'s verdicts by which half of the shape a row is in.

  mixedroot_population.py <scored.json> [more.json ...]

`tv.mixedroot` renders one library holding **both** loose episode files at the
root and shows in their own folders — that mix is the whole shape. F5 is about
the root half only: `folder_titles_from_db` builds `LIKE '%'` when the group has
no show folder, so a root group's "folder episode titles" is every episode in the
library, including every episode of every *foldered* show beside it.

A shape-level number cannot show that, because the two halves are in it
together. The split is by the row's own path: a path with no `/` in it is a file
at the library root.
"""
import collections
import json
import sys


def split(path):
    d = json.load(open(path))
    per = collections.defaultdict(collections.Counter)
    for r in d["rows"]:
        if r["shape"] != "tv.mixedroot":
            continue
        half = "root" if "/" not in r["path"] else "foldered"
        per[half][r["verdict"]] += 1
    return per


def main():
    verdicts = set()
    tables = []
    for p in sys.argv[1:]:
        per = split(p)
        tables.append((p, per))
        for c in per.values():
            verdicts |= set(c)
    order = ["correct"] + sorted(v for v in verdicts if v != "correct")
    for p, per in tables:
        print(p.rsplit("/", 1)[-1])
        print("   %-10s %6s  %s" % ("half", "rows",
                                    "  ".join("%14s" % v for v in order)))
        for half in ("root", "foldered"):
            c = per[half]
            print("   %-10s %6d  %s"
                  % (half, sum(c.values()),
                     "  ".join("%14d" % c[v] for v in order)))
        print()


if __name__ == "__main__":
    main()
