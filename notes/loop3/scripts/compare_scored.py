#!/usr/bin/env python3
"""Row-level diff of two oracle scored runs.

  compare_scored.py <before.json> <after.json>

The per-shape table is a summary, and a summary can be identical while rows move
under it — one shape losing what another gains, or a row changing the entity it
bound to without changing its verdict. So this joins on the row key and reports
the transitions, not the totals.

Refuses to compare runs whose row sets differ: a shape added between two runs
makes every count incomparable, and a diff that silently drops the rows it
cannot join is the shape of instrument error this project keeps finding.
"""
import collections
import json
import sys


def rows(path):
    d = json.load(open(path))
    out = {}
    for r in d["rows"]:
        out[(r["shape"], r["batch"], r["entity"], r["path"])] = r
    # `(shape, path)` alone silently dropped 120 rows once. The entity and the
    # batch are both needed: a shared root puts several entities under one
    # library, and a shape is generated into more than one batch.
    if len(out) != len(d["rows"]):
        sys.exit("%s: %d rows collapse to %d keys — the key is not unique"
                 % (path, len(d["rows"]), len(out)))
    return out


def main():
    before, after = rows(sys.argv[1]), rows(sys.argv[2])
    only_b, only_a = set(before) - set(after), set(after) - set(before)
    if only_b or only_a:
        sys.exit("row sets differ: %d only in before, %d only in after — "
                 "these runs do not join" % (len(only_b), len(only_a)))
    print("rows joined: %d" % len(before))

    moved = collections.Counter()
    rebound = collections.Counter()
    for k, b in before.items():
        a = after[k]
        if b["verdict"] != a["verdict"]:
            moved[(b["shape"], b["verdict"], a["verdict"])] += 1
        elif b.get("bound") != a.get("bound"):
            # Same verdict, different entity. Two wrongs are not one wrong.
            rebound[b["shape"]] += 1

    total = sum(moved.values())
    print("verdict changed: %d" % total)
    print("same verdict, different entity: %d" % sum(rebound.values()))
    for (shape, x, y), n in sorted(moved.items(), key=lambda kv: -kv[1]):
        print("   %-20s %-14s -> %-14s %6d" % (shape, x, y, n))
    for shape, n in sorted(rebound.items(), key=lambda kv: -kv[1]):
        print("   %-20s rebound within verdict %6d" % (shape, n))
    if not total and not rebound:
        print("   no row moved")


if __name__ == "__main__":
    main()
