#!/usr/bin/env python3
"""How many provider calls item 3's rule would need before it can be judged.

  kind_rule_warm_cost.py <tv-queries.tsv> [cache-dir]

Reads `relpath \t tv-query` (from `tv_query_for_paths.sh`) and asks the replay
cache's own key function whether each TV search is already there.

**A stall is not a result.** If the rule moves a shape's files onto queries the
cache cannot serve, the shape reads `stalled` and the scorer refuses to call that
a measurement — so the rule would ship unjudged. Warming is a human-run step, so
this prints the bill rather than paying it.
"""
import collections
import json
import os
import sys

SPIKE = os.path.expanduser("~/nightjar-spikes/matcher-oracle-2026-08-19")
sys.path.insert(0, SPIKE)
import cachekey as ck  # noqa: E402

queries = sys.argv[1]
cache = sys.argv[2] if len(sys.argv) > 2 else os.path.expanduser(
    "~/nightjar-wt-matcher-scratch/tmdb-cache-warm")

rows = []
for line in open(queries):
    parts = line.rstrip("\n").split("\t")
    if len(parts) == 2:
        rows.append(parts)

cached = collections.Counter()
missing = set()
empty = 0
for _rel, q in rows:
    if not q.strip():
        empty += 1
        continue
    f = os.path.join(cache, ck.k_search("tv", q) + ".json")
    if os.path.exists(f):
        cached["hit"] += 1
    else:
        cached["miss"] += 1
        missing.add(q)

print("paths            %d" % len(rows))
print("empty query      %d  (no search would be issued at all)" % empty)
print("tv search cached %d" % cached["hit"])
print("tv search missing %d rows, %d distinct queries" % (cached["miss"], len(missing)))
print("cache dir        %s (%d entries)"
      % (cache, sum(1 for _ in os.scandir(cache))))
print()
print("distinct queries a human would have to warm, first 15:")
for q in sorted(missing)[:15]:
    print("   %s" % q)
