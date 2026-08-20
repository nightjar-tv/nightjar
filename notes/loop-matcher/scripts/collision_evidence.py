#!/usr/bin/env python3
"""What evidence the collision tier had, and what it chose, for each wrong bind.

`pin_collision` takes "the first discriminator that selects exactly one candidate",
in the order episode count -> season count -> premiere year. This reads, for every
wrong binding it produced, the shapes of both the entity the filename came from
and the entity it bound instead — so the ADR argues from what the code could see
rather than from a guess about it.

Reads the warmed cache directly. No provider call, no re-drain.

Usage: collision_evidence.py <run-root> <scored.json>
"""
import collections, json, os, re, sys

SPIKE = os.path.expanduser("~/nightjar-spikes/matcher-oracle-2026-08-19")
sys.path.insert(0, SPIKE)
import cachekey as ck

RUNROOT, SCORED = sys.argv[1], sys.argv[2]
CACHE = os.environ.get("TMDB_CACHE", os.path.expanduser(
    "~/nightjar-wt-matcher-scratch/tmdb-cache-warm"))

MATCH = re.compile(r'^\s*match (.*?) → tmdb:Some\((\d+)\) method=(\S+)')


def tv_shape(sid):
    """Total episodes and season count for a show id, from any cached payload."""
    for key in (ck.k_tv_detail(sid), ck.k_tv_candidate_shape(sid, 1),
                ck.k_tv_candidate_shape(sid, None)):
        f = os.path.join(CACHE, key + ".json")
        if not os.path.exists(f):
            continue
        b = json.load(open(f))
        seas = [s for s in (b.get("seasons") or [])
                if (s.get("season_number") or 0) > 0]
        eps = sum(s.get("episode_count") or 0 for s in seas)
        return eps, len(seas), b.get("name"), (b.get("first_air_date") or "")[:4]
    return None, None, None, None


bound = {}
for run in sorted(os.listdir(RUNROOT)):
    err = os.path.join(RUNROOT, run, "run.err")
    if not os.path.exists(err):
        continue
    # **The batch belongs in the key.** Two groups in different batches of one
    # shape can carry the same query — `gen_library.py` splits fold-colliding
    # entities into separate batches precisely so they do not share a database —
    # and keying on (shape, query) alone let a later batch overwrite an earlier
    # one. That produced pairings that cannot occur, like `The Blacklist` bound
    # to `The Blacklist: Redemption`, which is not an exact-title match for it.
    # Same defect as the (shape, path) join in compare.py, in a second script.
    shape, _, batch = run.rpartition("-")
    for line in open(err, errors="replace"):
        m = MATCH.match(line.rstrip("\n"))
        if m:
            bound[(shape, batch, m.group(1).strip())] = (int(m.group(2)), m.group(3))

rows = json.load(open(SCORED))["rows"]
METHODS = ("exact_title_episode_count", "exact_title_season_count",
           "exact_title_library_year", "exact_title_year")

seen, cases = set(), []
for r in rows:
    if not r["verdict"].startswith("wrong."):
        continue
    hit = bound.get((r["shape"], r["batch"], r["name"].lower()))
    if not hit:
        continue
    bid, method = hit
    if method not in METHODS or bid == r["entity"]:
        continue
    k = (r["entity"], bid, r["shape"])
    if k in seen:
        continue
    seen.add(k)
    we, ws, wn, wy = tv_shape(r["entity"])
    ge, gs, gn, gy = tv_shape(bid)
    cases.append((method, r["shape"], r["name"], we, ws, wy, gn, ge, gs, gy))

print("wrong binds from the collision tier: %d distinct (entity, bound, shape)\n"
      % len(cases))
by = collections.Counter(c[0] for c in cases)
for m, n in by.most_common():
    print("  %-30s %5d" % (m, n))

# The library always asserts one season of at most 10 files in these shapes.
print("\nThe library shape in every one of these: 1 season, <= 10 episode files.")
print("So `library.episode_count` is ~10 and `library.season_count` is 1.\n")

print("%-26s %-22s %8s %7s   %-24s %8s %7s" %
      ("method", "correct entity", "its eps", "its sns", "bound instead",
       "its eps", "its sns"))
print("-" * 116)
for method, shape, name, we, ws, wy, gn, ge, gs, gy in cases[:24]:
    print("%-26s %-22s %8s %7s   %-24s %8s %7s" %
          (method.replace("exact_title_", ""), (name or "?")[:22], we, ws,
           (gn or "?")[:24], ge, gs))

# The claim to test: the bound entity is shorter than the correct one, so a
# library holding one partial season looks more like the shorter candidate.
both = [(we, ge) for _, _, _, we, _, _, _, ge, _, _ in cases
        if we is not None and ge is not None]
shorter = sum(1 for we, ge in both if ge < we)
print("\nof %d cases where both episode counts are known:" % len(both))
print("  bound entity has FEWER episodes than the correct one: %d (%.1f%%)"
      % (shorter, 100.0 * shorter / max(1, len(both))))
print("  bound entity has more or equal:                       %d"
      % (len(both) - shorter))
