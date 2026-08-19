#!/usr/bin/env python3
"""Is `wrong.unknownepisode` a wrong binding, or the scorer's blind spot?

The scorer emits it when a file carries an episode link whose id it cannot place:
it rebuilds id -> (season, episode) from cached season payloads, and an id from an
uncached season has nowhere to go. **A correct bind into an uncached season looks
identical to a wrong one**, so the label on its own cannot say which, and it must
not be added to the wrong column until it is checked.

This resolves it from a different direction: the resolver prints the entity it
bound, per group, in `run.err`. If that entity differs from the one the filename
was generated from, the binding is wrong regardless of whether the episode id can
be placed. If it agrees, the row is a scorer limitation and nothing else.

Usage: unknown_episode.py <run-root> <scored.json>
"""
import collections, json, re, sys

RUNROOT, SCORED = sys.argv[1], sys.argv[2]
MATCH = re.compile(r'^\s*match (.*?) → tmdb:Some\((\d+)\) method=(\S+)')

# (shape, query) -> (bound_id, method), from the resolver's own log.
bound = {}
import os
for run in sorted(os.listdir(RUNROOT)):
    err = os.path.join(RUNROOT, run, "run.err")
    if not os.path.exists(err):
        continue
    shape = run.rpartition("-")[0]
    for line in open(err, errors="replace"):
        m = MATCH.match(line.rstrip("\n"))
        if m:
            bound[(shape, m.group(1).strip())] = (int(m.group(2)), m.group(3))

rows = json.load(open(SCORED))["rows"]
unk = [r for r in rows if r["verdict"] == "wrong.unknownepisode"]
print("wrong.unknownepisode rows: %d over %d entities"
      % (len(unk), len({r["entity"] for r in unk})))

# The resolver logs the *cleaned* query. Join on a lowercased entity name, which
# is what the generator built the filename from, and report what will not join
# rather than dropping it silently.
verdict = collections.Counter()
methods = collections.Counter()
examples = {}
nojoin = 0
for r in unk:
    key = (r["shape"], r["name"].lower())
    hit = bound.get(key)
    if hit is None:
        nojoin += 1
        continue
    bid, method = hit
    same = (bid == r["entity"])
    verdict["bound the SAME entity (scorer blind spot)" if same
            else "bound a DIFFERENT entity (a real wrong bind)"] += 1
    methods[method] += 1
    if not same and method not in examples:
        examples[method] = (r["name"], r["entity"], bid)

for k, n in verdict.most_common():
    print("  %-46s %6d" % (k, n))
print("  %-46s %6d" % ("could not join to a resolver line", nojoin))

print("\nby the method that chose the candidate:")
for m, n in methods.most_common():
    print("  %-40s %6d" % (m, n))

print("\none example per method, where the entity differed:")
for m, (name, want, got) in examples.items():
    print("  %-40s %s: oracle %s, bound %s" % (m, name, want, got))
