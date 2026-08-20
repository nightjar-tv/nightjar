#!/usr/bin/env bash
# Re-measure the oracle against a warmed cache WITHOUT changing the population.
#
#   measure_warmed.sh <worktree> <cache-dir> <out-tag>
#
# **Do not use `./run.sh` for this.** `run.sh all` re-runs `inventory.py` and
# `pick_entities.py`, and those pick entities *from the cache* — an entity is
# kept only when the cache can serve it offline. A warmed cache can serve more,
# so a full `run.sh` after warming silently enlarges the entity set, and every
# before/after in notes 00-05 is then computed over a different population. The
# rows would not join, and the comparison would look like a result.
#
# So this re-drains the libraries already generated in `out/lib` — which encode
# the 2,410-entity population — against the warmed cache, and scores those. It
# refuses to run if the generated population has moved.
#
# Strict, both arms, `requests=0` expected: warming is finished by the time this
# runs, and a request here means it was not.
set -euo pipefail
unset CARGO_TARGET_DIR
WT=${1:?worktree}
CACHE=${2:?cache dir}
TAG=${3:?out tag, e.g. warm1}
SPIKE=${SPIKE:-$HOME/nightjar-spikes/matcher-oracle-2026-08-19}
OUT="$SPIKE/out"

# The population, pinned. `manifest.json` and the generated tree are what
# `gen_library.py` wrote for 2,410 entities; if either has moved, stop.
files=$(find "$OUT/lib" -name '*.jsonl' -exec cat {} + | wc -l | tr -d ' ')
ents=$(python3 - "$OUT/entities.json" <<'PY'
import json, sys
d = json.load(open(sys.argv[1]))
rows = d if isinstance(d, list) else d.get("entities", d.get("kept", []))
print(len(rows))
PY
)
echo "population: $ents entities, $files generated rows"
# **The entity count is the invariant; the row count is not.** Adding a *shape*
# adds rows and leaves the entity set alone, so every earlier comparison still
# joins — that is why `tv.episodetitle` was added as a new shape rather than by
# widening an existing one. Adding *entities* is the thing that breaks joins, and
# it is what re-running `pick_entities.py` against a warmed cache would do: an
# entity is kept only when the cache can serve it offline, and a warmed cache
# serves more.
#
# So: entities are pinned hard at 2,410, and the row count must match what is
# expected for the shape set in play — 67,982 for the original 17, 73,626 with
# `tv.episodetitle`. Pass EXPECT_ROWS to change it deliberately.
EXPECT_ROWS=${EXPECT_ROWS:-73626}
if [ "$ents" != "2410" ]; then
  echo "ABORT: $ents entities, not the 2,410 every earlier measurement used."
  echo "       Something re-ran pick_entities.py — probably ./run.sh all against"
  echo "       a warmed cache. Earlier comparisons will not join."
  exit 1
fi
if [ "$files" != "$EXPECT_ROWS" ]; then
  echo "ABORT: $files generated rows, expected $EXPECT_ROWS."
  echo "       If a shape was added on purpose, pass EXPECT_ROWS=$files."
  exit 1
fi

before=$(find "$CACHE" -type f -name '*.json' | wc -l | tr -d ' ')
echo "cache entries: $before"

export TMDB_CACHE="$CACHE"
export WT
"$SPIKE/run_all.sh" "$OUT/run"   > "$OUT/run-a-$TAG.log" 2>&1
"$SPIKE/run_all.sh" "$OUT/run-b" > "$OUT/run-b-$TAG.log" 2>&1

after=$(find "$CACHE" -type f -name '*.json' | wc -l | tr -d ' ')
[ "$before" = "$after" ] || { echo "ABORT: the cache changed ($before -> $after) — this arm was not offline"; exit 1; }

python3 "$SPIKE/score_binding.py" "$OUT/run"   "$OUT/scored-$TAG.json" | tee "$OUT/scored-$TAG.txt"
python3 "$SPIKE/score_binding.py" "$OUT/run-b" "$OUT/scored-$TAG-b.json" > /dev/null
echo
python3 "$SPIKE/identity_control.py" "$OUT/scored-$TAG.json" "$OUT/scored-$TAG-b.json" | head -6
echo
echo "requests must be 0 on every run below, or nothing above holds:"
grep -oE 'requests=[0-9]+' "$OUT/run-a-$TAG.log" | sort -u
