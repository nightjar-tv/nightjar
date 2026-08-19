#!/usr/bin/env python3
"""Why the absents are absent, per shape, in groups and in items.

`scored-a.json` says a row is `absent`. It does not say why, and the four
reasons behind it are different mechanisms with different fixes. The resolver
prints its reason per *group*, so this joins the two: groups by reason from
`run.err`, and the items behind them from the scored rows.

A group count and an item count are both reported because they answer different
questions. One group can hold ten files, so groups size the *mechanism* and
items size the *cost*.

Usage: reasons.py <run-root> <scored.json>
"""
import collections, json, os, re, sys

RUNROOT, SCORED = sys.argv[1], sys.argv[2]

# `unmatched <query> reason=...`. The query is the group's title, which is what
# lets a group be tied back to the rows that carry it.
LINE = re.compile(r'^\s*unmatched (.*) reason=(\w+)(?: \{ confidence: ([0-9.]+), method: "(\w+)" \})?')

groups = collections.Counter()      # (shape, reason) -> groups
queries = collections.defaultdict(set)   # (shape, reason) -> {query}
for run in sorted(os.listdir(RUNROOT)):
    err = os.path.join(RUNROOT, run, "run.err")
    if not os.path.exists(err):
        continue
    shape = run.rpartition("-")[0]
    for line in open(err, errors="replace"):
        m = LINE.match(line.rstrip("\n"))
        if not m:
            continue
        query, kind, _conf, method = m.groups()
        reason = method or kind
        groups[(shape, reason)] += 1
        queries[(shape, reason)].add(query.strip())

rows = json.load(open(SCORED))["rows"]
absent_by_shape = collections.Counter(
    r["shape"] for r in rows if r["verdict"] == "absent")

# Items behind a reason, matched through the group's query. `norm` is not in the
# scored rows, so the join is on the cleaned title the resolver printed against a
# lowercased title of the row — approximate, and labelled as such below.
def rowkey(r):
    return r["name"].lower()

items = collections.Counter()
for r in rows:
    if r["verdict"] != "absent":
        continue
    for (shape, reason), qs in queries.items():
        if shape == r["shape"] and rowkey(r) in qs:
            items[(shape, reason)] += 1
            break

print("%-16s %-34s %8s %8s" % ("shape", "reason", "groups", "items~"))
print("-" * 70)
for (shape, reason), n in sorted(groups.items(), key=lambda kv: -kv[1]):
    print("%-16s %-34s %8d %8d" % (shape, reason, n, items[(shape, reason)]))

print()
print("%-16s %8s" % ("shape", "absent"))
for shape, n in absent_by_shape.most_common():
    print("%-16s %8d" % (shape, n))

print()
print("totals by reason (groups):")
tot = collections.Counter()
for (shape, reason), n in groups.items():
    tot[reason] += n
for reason, n in tot.most_common():
    print("   %-34s %6d" % (reason, n))
print()
print("item counts are joined on the lowercased entity name against the query")
print("the resolver printed, so they are approximate. Group counts are exact.")
