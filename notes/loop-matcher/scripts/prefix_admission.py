#!/usr/bin/env python3
"""Split the collision-tier wrong binds by *how the candidate was admitted*.

`name_matches_query` calls a TV candidate a title hit three ways: an exact fold,
the candidate being the query plus a longer tail ("The Continental" ->
"The Continental: From the World of John Wick"), or the head before a colon. The
prefix arm is deliberate and useful. It is also the exact shape of a franchise
spin-off, and it cannot tell the two apart.

So: of the wrong binds the collision tier produced, how many involve a candidate
that is an exact fold of the query, and how many a strict extension of it? The two
need different fixes and they have been discussed as separate mechanisms — this
decides whether they are.

Normalisation comes from the shipped chain, by feeding `<Name> - S01E01.mkv`
through `oracle_query` and reading its `norm` column. A reimplemented `norm_key`
has misreported this project by 25 against 12, so it is not reimplemented here.

Usage: prefix_admission.py <run-root> <scored.json>   (needs $ORACLE_QUERY)
"""
import collections, json, os, re, subprocess, sys

SPIKE = os.path.expanduser("~/nightjar-spikes/matcher-oracle-2026-08-19")
sys.path.insert(0, SPIKE)
import cachekey as ck

RUNROOT, SCORED = sys.argv[1], sys.argv[2]
OQ = os.environ.get("ORACLE_QUERY")
if not OQ or not os.path.exists(OQ):
    sys.exit("set ORACLE_QUERY")
CACHE = os.environ.get("TMDB_CACHE", os.path.expanduser(
    "~/nightjar-wt-matcher-scratch/tmdb-cache-warm"))
MATCH = re.compile(r'^\s*match (.*?) → tmdb:Some\((\d+)\) method=(\S+)')
METHODS = ("exact_title_episode_count", "exact_title_season_count",
           "exact_title_library_year", "exact_title_year")


def tv_name(sid):
    for key in (ck.k_tv_detail(sid), ck.k_tv_candidate_shape(sid, 1),
                ck.k_tv_candidate_shape(sid, None)):
        f = os.path.join(CACHE, key + ".json")
        if os.path.exists(f):
            return json.load(open(f)).get("name")
    return None


bound = {}
for run in sorted(os.listdir(RUNROOT)):
    err = os.path.join(RUNROOT, run, "run.err")
    if not os.path.exists(err):
        continue
    shape, _, batch = run.rpartition("-")
    for line in open(err, errors="replace"):
        m = MATCH.match(line.rstrip("\n"))
        if m:
            bound[(shape, batch, m.group(1).strip())] = (int(m.group(2)), m.group(3))

rows = json.load(open(SCORED))["rows"]
cases, seen = [], set()
for r in rows:
    if not r["verdict"].startswith("wrong."):
        continue
    hit = bound.get((r["shape"], r["batch"], r["name"].lower()))
    if not hit:
        continue
    bid, method = hit
    if method not in METHODS or bid == r["entity"]:
        continue
    k = (r["entity"], bid)
    if k in seen:
        continue
    seen.add(k)
    bn = tv_name(bid)
    if bn:
        cases.append((r["name"], bn, method))

# One oracle_query pass for every name involved, so the fold is the shipped one.
names = sorted({n for c in cases for n in (c[0], c[1])})
inp = "".join("%s - S01E01.mkv\tx\n" % n.replace("/", "-") for n in names)
out = subprocess.run([OQ], input=inp, capture_output=True, text=True)
norm = {}
for line in out.stdout.splitlines():
    f = line.split("\t")
    if len(f) > 6:
        norm[f[0][:-len(" - S01E01.mkv")]] = f[6]

kinds = collections.Counter()
examples = collections.defaultdict(list)
for want, got, method in cases:
    a = norm.get(want.replace("/", "-"))
    b = norm.get(got.replace("/", "-"))
    if a is None or b is None:
        kinds["could not fold one of the names"] += 1
        continue
    if a == b:
        k = "exact fold — a genuine same-title collision"
    elif b.startswith(a + " "):
        k = "PREFIX — candidate is the query plus a tail"
    elif a.startswith(b + " "):
        k = "reverse prefix — query is the candidate plus a tail"
    else:
        k = "neither — admitted some other way"
    kinds[k] += 1
    if len(examples[k]) < 6:
        examples[k].append((want, got, method))

print("collision-tier wrong binds, by how the candidate was admitted (%d pairs)\n"
      % len(cases))
for k, n in kinds.most_common():
    print("  %-46s %5d  (%.1f%%)" % (k, n, 100.0 * n / max(1, len(cases))))
for k, ex in examples.items():
    print("\n%s:" % k)
    for want, got, method in ex:
        print("   %-30s -> %-38s via %s" % (want[:30], got[:38],
                                            method.replace("exact_title_", "")))
