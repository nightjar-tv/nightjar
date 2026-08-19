#!/usr/bin/env bash
# The dogfood strict pair: one capture, two binaries, offline, scored side by side.
#
#   dogfood_pair.sh <control-tree> <treatment-tree> <out-dir>
#
# The rules this encodes, each of which has been got wrong here before:
#
#   * `requests=0` on both arms. The API key is a deliberate non-key and strict
#     mode turns a cache miss into an error, so an offline run is provable.
#   * The two binaries are sha256'd and the run aborts if they are the same
#     file. A shared build directory has produced identical control and
#     treatment binaries on this project.
#   * `errors=0` on both arms, or the verdict is meaningless. A cold pair
#     reports FAIL for a binary compared against itself.
#   * `NIGHTJAR_REPARSE=1` on both, so neither arm reads a title the scanner
#     already interpreted.
#   * The cache is counted before and after. It must not change: nothing here
#     is allowed to write to it.
set -euo pipefail
CTL=${1:?control tree}; TRT=${2:?treatment tree}; OUT=${3:?out dir}
CAP=${CAPTURE:-$HOME/nightjar-wt-loop-scratch/replay/capture-media-mac.jsonl}
CACHE=${TMDB_CACHE:-$HOME/nightjar-wt-loop-scratch/replay/tmdb-cache}

[ -f "$CAP" ] || { echo "no capture at $CAP"; exit 1; }
before=$(find "$CACHE" -type f | wc -l | tr -d ' ')
echo "cache entries before: $before"

cbin="$CTL/server/target/release/replay"
tbin="$TRT/server/target/release/replay"
for b in "$cbin" "$tbin"; do
  [ -x "$b" ] || { echo "missing binary: $b"; exit 1; }
done
ch=$(shasum -a 256 "$cbin" | cut -d' ' -f1)
th=$(shasum -a 256 "$tbin" | cut -d' ' -f1)
echo "control   sha256 $ch"
echo "treatment sha256 $th"
if [ "$ch" = "$th" ]; then
  echo "ABORT: the two arms are the same binary. Verify the two things you compare are two things."
  exit 1
fi

mkdir -p "$OUT"
for arm in control treatment; do
  bin=$([ "$arm" = control ] && echo "$cbin" || echo "$tbin")
  d="$OUT/$arm"; rm -rf "$d"; mkdir -p "$d/data"
  echo 'tmdb_api_key=not-a-key-strict-replay-only' > "$d/data/secrets"
  NIGHTJAR_DATA_DIR="$d/data" \
  NIGHTJAR_CAPTURE="$CAP" \
  NIGHTJAR_TMDB_CACHE="$CACHE" \
  NIGHTJAR_TMDB_CACHE_STRICT=1 \
  NIGHTJAR_REPARSE=1 \
    "$bin" > "$d/run.out" 2> "$d/run.err" || {
      echo "$arm REPLAY FAILED"; tail -5 "$d/run.err"; exit 1; }
  line=$(grep -E '^DONE' "$d/run.out" | tail -1)
  echo "$arm: $line"
done

after=$(find "$CACHE" -type f | wc -l | tr -d ' ')
echo "cache entries after:  $after"
[ "$before" = "$after" ] || { echo "ABORT: the cache changed ($before -> $after)"; exit 1; }

python3 - "$OUT" <<'PY'
import re, sys, os
OUT = sys.argv[1]
def done(arm):
    txt = open(os.path.join(OUT, arm, "run.out")).read()
    line = [l for l in txt.splitlines() if l.startswith("DONE")][-1]
    return dict((k, int(v)) for k, v in re.findall(r"(\w+)=(\d+)", line))
c, t = done("control"), done("treatment")
keys = ["groups", "ready", "unmatched", "pending", "errors", "requests"]
print()
print("%-12s %10s %10s %10s" % ("field", "control", "treatment", "delta"))
for k in keys:
    print("%-12s %10d %10d %+10d" % (k, c.get(k, 0), t.get(k, 0), t.get(k, 0) - c.get(k, 0)))
bad = []
for arm, d in (("control", c), ("treatment", t)):
    if d.get("requests", 1) != 0:
        bad.append("%s made %d requests — nothing below holds" % (arm, d["requests"]))
    if d.get("errors", 1) != 0:
        bad.append("%s reported %d errors — the verdict is meaningless" % (arm, d["errors"]))
print()
if bad:
    for b in bad:
        print("INVALID: " + b)
    raise SystemExit(1)
# `ready` alone cannot tell a wrong binding from a right one. It is reported as
# a movement to explain, never as a score.
if t["ready"] == c["ready"] and t["unmatched"] == c["unmatched"]:
    print("PAIR CLEAN: identical ready/unmatched on both arms, errors=0, requests=0")
else:
    print("PAIR MOVED: ready %+d, unmatched %+d — explain before keeping"
          % (t["ready"] - c["ready"], t["unmatched"] - c["unmatched"]))
PY
