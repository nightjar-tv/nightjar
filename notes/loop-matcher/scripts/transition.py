#!/usr/bin/env python3
"""List the rows that made one named verdict transition between two runs.

A total says a change gained ground. It cannot say what it cost, and the loop's
severity rule is that a lost correct binding is not paid for by a count. So the
small adverse transitions get named and read individually.

Usage: transition.py <before.json> <after.json> <from-verdict> <to-verdict> [limit]
"""
import collections, json, sys

before = json.load(open(sys.argv[1]))["rows"]
after = json.load(open(sys.argv[2]))["rows"]
FROM, TO = sys.argv[3], sys.argv[4]
LIMIT = int(sys.argv[5]) if len(sys.argv) > 5 else 40

ka = {(r["shape"], r["batch"], r["path"]): r for r in after}
assert len(ka) == len(after), "row key is not unique"
hits = []
for rb in before:
    ra = ka.get((rb["shape"], rb["batch"], rb["path"]))
    if ra and rb["verdict"] == FROM and ra["verdict"] == TO:
        hits.append((rb, ra))

print("%s -> %s : %d rows" % (FROM, TO, len(hits)))
print("by shape:      %s" % dict(collections.Counter(r["shape"] for r, _ in hits)))
print("by entity:     %s" % dict(collections.Counter(r["name"] for r, _ in hits)))
print("by tags:       %s" % dict(collections.Counter(
    ",".join(r["tags"]) for r, _ in hits)))
print()
for rb, ra in hits[:LIMIT]:
    print("  %-14s %s" % (rb["shape"], rb["path"]))
    print("      entity %s (%s)  s%s e%s" % (rb["entity"], rb["name"],
                                             rb["season"], rb["episode"]))
    print("      before bound=%s status=%s   after bound=%s status=%s"
          % (rb["bound"], rb["status"], ra["bound"], ra["status"]))
