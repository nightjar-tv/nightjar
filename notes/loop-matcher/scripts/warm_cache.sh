#!/usr/bin/env bash
# Warm the replay cache by running the oracle non-strict, until it converges.
#
#   TMDB_API_KEY=… warm_cache.sh <worktree> <cache-dir> [max-rounds]
#
# **This is the only thing in the loop that touches the network.** Everything
# else proves `requests=0`.
#
# Why not a bespoke fetcher: the patched `tmdb/mod.rs` already writes every
# response it fetches under `measure_cache_key`, so running non-strict issues
# exactly the calls the matcher wants, through the shipped call sites, and stores
# them under keys that are correct by construction. A hand-written fetcher would
# reimplement the key function and the query shapes — and a reimplemented
# predicate has misreported this project before, by 25 against 12.
#
# Why rounds: `warm_list.py` says the plan is a lower bound — *"answering these
# can raise calls that nothing has asked for yet."* A candidate that becomes
# reachable asks for its own seasons. So this repeats until a whole round makes
# zero requests, and reports the count per round so the convergence is visible
# rather than assumed.
#
# The cache is copied, never written in place: the 8,185-entry cache is a shared
# instrument other spikes read, and the loop's hard limits forbid writing to it.
# Point `<cache-dir>` at the copy.
set -euo pipefail
WT=${1:?worktree}
CACHE=${2:?cache dir to warm (a copy, never the shared one)}
ROUNDS=${3:-6}
HERE=$(cd "$(dirname "$0")" && pwd)
SPIKE=${SPIKE:-$HOME/nightjar-spikes/matcher-oracle-2026-08-19}
WORK=${WORK:-$HOME/nightjar-wt-matcher-scratch/warm}

# Prefer a *path* to the key over the key itself. A credential passed on a
# command line is visible in `ps` for the life of the process and lands in shell
# history; a path is neither. `TMDB_SECRETS_FILE` points at a `key=value` file
# holding a `tmdb_api_key=` line — the same format the resolver reads — and the
# value is read here, never echoed and never passed as an argument.
if [ -z "${TMDB_API_KEY:-}" ] && [ -n "${TMDB_SECRETS_FILE:-}" ]; then
  [ -r "$TMDB_SECRETS_FILE" ] || { echo "cannot read $TMDB_SECRETS_FILE" >&2; exit 1; }
  TMDB_API_KEY=$(grep -m1 -E '^[[:space:]]*tmdb_api_key[[:space:]]*=' "$TMDB_SECRETS_FILE"                  | cut -d= -f2- | tr -d '[:space:]')
  case "$TMDB_API_KEY" in
    "")            echo "no tmdb_api_key= line in $TMDB_SECRETS_FILE" >&2; exit 1;;
    *not-a-key*)   echo "$TMDB_SECRETS_FILE holds the replay placeholder, not a real key" >&2; exit 1;;
  esac
  echo "key: read from $TMDB_SECRETS_FILE (${#TMDB_API_KEY} chars, value not printed)"
fi

if [ -z "${TMDB_API_KEY:-}" ]; then
  cat >&2 <<'MSG'
TMDB_API_KEY is not set, and nothing here can run without it.

Provide it for this command only, so it is never written to a file that
outlives the run and never echoed:

    TMDB_SECRETS_FILE=/path/to/secrets \
      notes/loop-matcher/scripts/warm_cache.sh <worktree> <cache-dir>

or, less safely, the key inline (visible in `ps` and in shell history):

    TMDB_API_KEY=xxxx notes/loop-matcher/scripts/warm_cache.sh <worktree> <cache-dir>

The key is written to each run's own data/secrets, which is what the resolver
reads, and those directories live under the scratch tree.
MSG
  exit 1
fi

[ -d "$SPIKE/out/lib" ] || { echo "no generated libraries at $SPIKE/out/lib — run ./run.sh first" >&2; exit 1; }
mkdir -p "$CACHE" "$WORK"

count() { find "$CACHE" -type f -name '*.json' | wc -l | tr -d ' '; }
start=$(count)
echo "cache entries at start: $start"
echo

# `SHAPES` restricts the warm to a space-separated list, because the plan splits
# into two tranches of very different size and cost:
#
#   457 calls   candidate details and seasons, blocking 3,862 groups. Every one
#               is a call the matcher reached with real evidence.
#   4,876 calls searches for queries carrying no show title — the M1/M2 tranche.
#               `tv.handmade` is the only route to M2 and it is 100% of this.
#
# Default is everything. `SHAPES="tv.root tv.scene"` warms one tranche.
FILTER=${SHAPES:-}

total=0
prev=$start
for round in $(seq 1 "$ROUNDS"); do
  echo "=== round $round ==="
  rq=0
  failed=0
  for cap in "$SPIKE"/out/lib/*/*/; do
    shape=$(basename "$(dirname "$cap")")
    batch=$(basename "$cap")
    if [ -n "$FILTER" ] && ! printf '%s\n' $FILTER | grep -qx "$shape"; then
      continue
    fi
    d="$WORK/$shape-$batch"
    rm -rf "$d"; mkdir -p "$d/data"
    # The real key, for this run's resolver only. Never echoed; the client
    # scrubs it out of error strings (`scrub_tmdb_url_secret`).
    printf 'tmdb_api_key=%s\n' "$TMDB_API_KEY" > "$d/data/secrets"
    chmod 600 "$d/data/secrets"
    # STRICT deliberately unset: a miss falls through to a live call and the
    # response is written into the cache under its own key.
    NIGHTJAR_DATA_DIR="$d/data" \
    NIGHTJAR_CAPTURE="$cap/capture.jsonl" \
    NIGHTJAR_TMDB_CACHE="$CACHE" \
    NIGHTJAR_REPARSE=1 \
      "$WT/server/target/release/replay" > "$d/run.out" 2> "$d/run.err" || {
        echo "  !! $shape-$batch replay FAILED; see $d/run.err" >&2
        tail -3 "$d/run.err" >&2
        failed=$((failed + 1)); }
    n=$(grep -oE 'requests=[0-9]+' "$d/run.out" 2>/dev/null | tail -1 | cut -d= -f2 || true)
    n=${n:-0}
    rq=$((rq + n))
    [ "$n" -gt 0 ] && printf '  %-24s %6d requests\n' "$shape-$batch" "$n"
  done
  now=$(count)
  total=$((total + rq))
  grew=$((now - prev))
  echo "  round $round: $rq requests attempted, $grew entries written, cache now $now (+$((now - start)) overall)"
  # **`requests=N` counts attempts, not successes.** `http_requests.fetch_add(1)`
  # runs before `agent.get(..).call()`, so a request that times out or is refused
  # still increments it. A round can therefore report thousands of requests and
  # write nothing, which reads as "warming happened" if only the request total is
  # believed. **The cache count is the ground truth.**
  #
  # This fired for real: every request timed out at the 30s read deadline while
  # the counter climbed and the cache stayed at 8,185.
  if [ "$rq" -gt 0 ] && [ "$grew" -eq 0 ]; then
    echo
    echo "ABORT: $rq requests attempted and 0 entries written. The requests are"
    echo "       failing, not succeeding. Check a batch's run.err:"
    echo "         grep -m3 'provider error' $WORK/*/run.err"
    echo "       A 'timed out reading response' with curl working means the"
    echo "       sandbox blackholes this binary's TLS — warm outside it."
    exit 1
  fi
  prev=$now
  # **A batch that never ran makes no request.** So does a batch with nothing
  # left to fetch, and the round total cannot tell them apart. This reported
  # "converged" once while `tv.episodetitle-b0` panicked on a UNIQUE constraint
  # in every round — 5,604 rows never drained, and the summary said the warm was
  # complete. Convergence is only meaningful over batches that actually ran.
  if [ "$failed" -gt 0 ]; then
    echo
    echo "ABORT: $failed batch(es) failed this round. Their rows were never"
    echo "       drained, so a zero request count says nothing about them."
    echo "       Fix the failure before trusting any total."
    exit 1
  fi
  if [ "$rq" -eq 0 ]; then
    echo
    echo "converged: a whole round made no request, and every batch ran."
    break
  fi
done

end=$(count)
echo
echo "cache entries: $start -> $end  (+$((end - start)))"
echo "TOTAL LIVE REQUESTS: $total"
echo
echo "The request total is the number to report. Re-run the oracle strict"
echo "against this cache next; requests=0 there is the proof warming is done."
