#!/usr/bin/env python3
"""What evidence could break a yearless movie collision, measured from the cache.

`movie.noyear` renders `Name/Name.1080p.BluRay.mkv` — no year in the filename and
none in the folder. 705 of 1,712 rows go unmatched, every one
`exact_title_collision_unpinned`, and the candidates are real distinct films
sharing a title: `CODA` has 17, `The Visitor` 16.

A movie has no seasons, so none of the coverage evidence that fixed the TV shapes
exists. The question ADR territory has to answer is *what else may decide*, and
this measures the two candidates for that from the provider's own response.

**Runtime is excluded on purpose.** `gen_library.py:328` sets each generated
file's `duration_ms` from the correct entity's own `runtime`, so a runtime
tie-break would score near-perfectly here by marking its own homework. That is a
property of the instrument, not of the idea, and it is why this script does not
score it.

`popularity` and `vote_count` are read from the recorded search response and are
not generated, so they are honest evidence the oracle can weigh.

Usage: movie_noyear_evidence.py <scored.json>
"""
import collections, json, os, sys

SPIKE = os.path.expanduser("~/nightjar-spikes/matcher-oracle-2026-08-19")
sys.path.insert(0, SPIKE)
import cachekey as ck

CACHE = os.environ.get("TMDB_CACHE", os.path.expanduser(
    "~/nightjar-wt-matcher-scratch/tmdb-cache-warm"))
rows = json.load(open(sys.argv[1]))["rows"]
sel = [r for r in rows if r["shape"] == "movie.noyear"]

seen, cases = set(), []
for r in sel:
    if r["entity"] in seen:
        continue
    seen.add(r["entity"])
    cases.append(r)

stats = collections.Counter()
detail = collections.Counter()
for r in cases:
    f = os.path.join(CACHE, ck.k_search("movie", r["name"]) + ".json")
    if not os.path.exists(f):
        f2 = os.path.join(CACHE, ck.k_search("movie", r["name"].lower()) + ".json")
        f = f2 if os.path.exists(f2) else None
    if not f:
        stats["search not cached"] += 1
        continue
    res = json.load(open(f)).get("results") or []
    if not res:
        stats["search returned nothing"] += 1
        continue
    ids = [x.get("id") for x in res]
    want = r["entity"]
    if want not in ids:
        stats["correct entity absent from its own search"] += 1
        continue
    bucket = "absent" if r["verdict"] == "absent" else r["verdict"]
    stats["usable"] += 1

    # 1. TMDB's own ordering: is the right film first?
    if ids[0] == want:
        detail[(bucket, "top-ranked result is correct")] += 1
    # 2. Highest vote_count.
    vc = max(res, key=lambda x: (x.get("vote_count") or 0))
    if vc.get("id") == want:
        detail[(bucket, "highest vote_count is correct")] += 1
    # 3. Highest popularity.
    pop = max(res, key=lambda x: (x.get("popularity") or 0.0))
    if pop.get("id") == want:
        detail[(bucket, "highest popularity is correct")] += 1
    detail[(bucket, "TOTAL")] += 1

print("movie.noyear entities: %d" % len(cases))
for k, n in stats.most_common():
    print("   %-44s %5d" % (k, n))
print()
print("of the usable ones, how often each signal names the right film:")
for bucket in sorted({b for b, _ in detail}):
    tot = detail[(bucket, "TOTAL")]
    if not tot:
        continue
    print("  rows currently %s  (%d entities)" % (bucket, tot))
    for label in ("top-ranked result is correct",
                  "highest vote_count is correct",
                  "highest popularity is correct"):
        n = detail[(bucket, label)]
        print("     %-34s %5d  %5.1f%%" % (label, n, 100.0 * n / tot))
print()
print("A signal is only usable as a tie-break if it is right on the rows that")
print("currently fail AND does not move the rows that currently succeed.")
