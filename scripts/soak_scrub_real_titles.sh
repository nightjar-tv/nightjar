#!/usr/bin/env bash
# Spike B closeout: real titles over the share (not CI — Rule 4.2).
# Run in a desktop terminal (Chrome CDP). Dogfood must already be on BASE.
set -euo pipefail
ROOT="$(cd "$(dirname "$0")/.." && pwd)"
BASE="${BASE:-http://127.0.0.1:8096}"
TRIALS="${TRIALS:-10}"
AXES="${AXES:-behind_head,far_ahead}"
OUT_BASE="${OUT_DIR:-/tmp/nj-soak-real-titles-$$}"
mkdir -p "$OUT_BASE"

pkill -f nj-soak-chrome- 2>/dev/null || true

echo "===== item 248 Bros / PGS e2 ====="
EXTERNAL=1 BASE="$BASE" ITEM=248 CELLS=d AXES="$AXES" TRIALS="$TRIALS" \
  COLD=1 OUT_DIR="$OUT_BASE/248" \
  "$ROOT/scripts/soak_scrub.sh"

echo "===== item 1574 Simpsons Movie / ASS e3 ====="
EXTERNAL=1 BASE="$BASE" ITEM=1574 CELLS=c AXES="$AXES" TRIALS="$TRIALS" \
  COLD=1 OUT_DIR="$OUT_BASE/1574" \
  "$ROOT/scripts/soak_scrub.sh"

echo
echo "===== summaries ====="
cat "$OUT_BASE/248/summary.txt" "$OUT_BASE/1574/summary.txt"
echo "OUT_BASE=$OUT_BASE"
