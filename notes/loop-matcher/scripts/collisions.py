#!/usr/bin/env python3
"""Are the exact-title collisions real, or is the fold agreeing with too much?

`exact_title_collision_unpinned` is the dominant remaining reason. It means two
or more candidates matched the query's title exactly and nothing could break the
tie. That is a hard limit if the candidates are genuinely same-titled films, and
a defect if the title fold is pulling in titles that are not equal.

The two look identical from the outside, so this reads the cached search response
the matcher actually saw and reports the candidate titles behind each collision.
It uses the spike's own `cachekey` so the key function is not reimplemented.

Usage: collisions.py <run-root> <shape> [limit]
"""
import collections, json, os, re, sys

SPIKE = os.path.expanduser("~/nightjar-spikes/matcher-oracle-2026-08-19")
sys.path.insert(0, SPIKE)
import cachekey as ck

RUNROOT, SHAPE = sys.argv[1], sys.argv[2]
LIMIT = int(sys.argv[3]) if len(sys.argv) > 3 else 25
CACHE = os.environ.get("TMDB_CACHE", os.path.expanduser(
    "~/nightjar-wt-loop-scratch/replay/tmdb-cache"))

LINE = re.compile(r'^\s*unmatched (.*) reason=BelowThreshold \{ confidence: [0-9.]+, method: "(\w+)" \}')

queries = []
for run in sorted(os.listdir(RUNROOT)):
    if not run.startswith(SHAPE + "-"):
        continue
    err = os.path.join(RUNROOT, run, "run.err")
    if not os.path.exists(err):
        continue
    for line in open(err, errors="replace"):
        m = LINE.match(line.rstrip("\n"))
        if m and m.group(2) == "exact_title_collision_unpinned":
            queries.append(m.group(1).strip())

print("%s: %d unpinned collision groups" % (SHAPE, len(queries)))

kind = "movie" if SHAPE.startswith("movie") else "tv"
path = "/search/%s" % kind
sizes = collections.Counter()
shown = 0
unreadable = 0
for q in queries:
    key = ck.cache_key(path, [("query", q), ("include_adult", "false"),
                              ("language", "en-US"), ("page", "1")])
    f = os.path.join(CACHE, key + ".json")
    if not os.path.exists(f):
        # Try the shipped query order variants the matcher may build.
        found = None
        for order in ([("language", "en-US"), ("query", q), ("page", "1"),
                       ("include_adult", "false")],
                      [("query", q), ("language", "en-US")],
                      [("query", q)]):
            k2 = ck.cache_key(path, order)
            if os.path.exists(os.path.join(CACHE, k2 + ".json")):
                found = os.path.join(CACHE, k2 + ".json")
                break
        if not found:
            unreadable += 1
            continue
        f = found
    body = json.load(open(f))
    results = body.get("results") or []
    namefield = "title" if kind == "movie" else "name"
    def norm(s):
        return re.sub(r"[^a-z0-9]+", " ", (s or "").lower()).strip()
    exact = [r for r in results if norm(r.get(namefield)) == norm(q)]
    sizes[len(exact)] += 1
    if shown < LIMIT and len(exact) >= 2:
        shown += 1
        print("  %-32s %d exact of %d results" % (q, len(exact), len(results)))
        for r in exact:
            print("       %-40s %s  id=%s" % (
                r.get(namefield),
                (r.get("release_date") or r.get("first_air_date") or "?")[:4],
                r.get("id")))

print()
print("exact-candidate count distribution: %s" % dict(sorted(sizes.items())))
print("queries whose cached search this script could not locate: %d" % unreadable)
print()
print("A count of 2+ with distinct years is a real collision the filename cannot")
print("break. A count of 2+ with the SAME title and year would mean the oracle")
print("should have dropped the entity; a fold pulling in a different title would")
print("mean the exact set is over-inclusive and this is a defect, not a limit.")
