#!/usr/bin/env bash
# Gate 2 orphan soak: churn sessions for HOURS (default 48) and assert no
# leftover FFmpeg children whose cmdline still names this Nightjar data dir.
#
# Usage:
#   BASE=http://127.0.0.1:8096 HOURS=48 ./scripts/gate2_orphan_soak.sh
#
# Writes notes/gate2/orphan-soak-<stamp>.log and a final JSON summary beside it.
# Exit 0 only if the soak completed with zero orphan samples at the end and
# never saw an orphan count rise after a quiet reap window.
set -euo pipefail

ROOT="$(cd "$(dirname "$0")/.." && pwd)"
BASE="${BASE:-http://127.0.0.1:8096}"
HOURS="${HOURS:-48}"
INTERVAL_S="${INTERVAL_S:-120}"
ITEM_ID="${ITEM_ID:-}"
OUT_DIR="${OUT_DIR:-$ROOT/notes/gate2}"
STAMP="$(date -u +%Y%m%dT%H%M%SZ)"
mkdir -p "$OUT_DIR"
LOG="$OUT_DIR/orphan-soak-$STAMP.log"
SUMMARY="$OUT_DIR/orphan-soak-$STAMP.json"

DATA_DIR="${NIGHTJAR_DATA_DIR:-$HOME/nightjar-data}"
END_EPOCH=$(( $(date +%s) + HOURS * 3600 ))

log() { printf '%s %s\n' "$(date -u +%Y-%m-%dT%H:%M:%SZ)" "$*" | tee -a "$LOG"; }

ffmpeg_orphans() {
  # Children whose argv still references the Nightjar HLS cache / data dir.
  pgrep -lf '[f]fmpeg' 2>/dev/null | grep -F "$DATA_DIR" || true
}

orphan_count() {
  ffmpeg_orphans | wc -l | tr -d ' '
}

pick_item() {
  if [[ -n "$ITEM_ID" ]]; then
    echo "$ITEM_ID"
    return
  fi
  python3 - <<PY
import json, urllib.request
libs = json.load(urllib.request.urlopen("${BASE}/api/v0/libraries", timeout=10))["libraries"]
for lib in libs:
    items = json.load(urllib.request.urlopen(
        f"${BASE}/api/v0/libraries/{lib['id']}/items?limit=50", timeout=10
    ))["items"]
    for it in items:
        if it.get("playbackMethod") in ("remux", "transcode"):
            print(it["id"])
            raise SystemExit
print("", end="")
PY
}

health="$(curl -sf -o /dev/null -w '%{http_code}' "$BASE/api/health" || true)"
if [[ "$health" != "200" ]]; then
  echo "FAIL: $BASE/api/health → $health" >&2
  exit 1
fi

ITEM="$(pick_item)"
if [[ -z "$ITEM" ]]; then
  echo "FAIL: no remux/transcode item found; set ITEM_ID=" >&2
  exit 1
fi

log "start hours=$HOURS interval=${INTERVAL_S}s item=$ITEM data_dir=$DATA_DIR base=$BASE"
MAX_ORPHANS=0
SAMPLES=0
CHURNS=0
FAIL=0

while (( $(date +%s) < END_EPOCH )); do
  SAMPLES=$((SAMPLES + 1))
  # Start a session, touch the playlist, then DELETE (session teardown path).
  # POST returns 202; -f only fails on >=400.
  body="$(curl -sf -X POST "$BASE/api/v0/items/$ITEM/sessions" || true)"
  read -r sid playlist < <(python3 -c "
import json,sys
try:
    d=json.loads(sys.argv[1])
    print(d.get('sessionId',''), d.get('playlistUrl',''))
except Exception:
    print('','')
" "$body" 2>/dev/null || echo ' ')
  if [[ -n "$sid" ]]; then
    CHURNS=$((CHURNS + 1))
    if [[ -n "$playlist" ]]; then
      curl -sf -o /dev/null "$BASE$playlist" || true
    else
      curl -sf -o /dev/null "$BASE/api/v0/sessions/$sid/master.m3u8" || true
    fi
    sleep 2
    curl -sf -X DELETE "$BASE/api/v0/sessions/$sid" -o /dev/null || true
  else
    log "WARN: session create failed body=${body:0:200}"
  fi

  # Allow idle reaper a beat, then sample.
  sleep 5
  n="$(orphan_count)"
  if (( n > MAX_ORPHANS )); then MAX_ORPHANS=$n; fi
  if (( n > 0 )); then
    log "orphan_sample count=$n"
    ffmpeg_orphans | tee -a "$LOG" || true
    # One more quiet window — reaper may still be within idle grace.
    sleep 30
    n2="$(orphan_count)"
    if (( n2 > 0 )); then
      log "FAIL sticky_orphans count=$n2"
      FAIL=1
      ffmpeg_orphans | tee -a "$LOG" || true
    fi
  else
    log "ok sample=$SAMPLES churns=$CHURNS orphans=0"
  fi

  remaining=$(( END_EPOCH - $(date +%s) ))
  if (( remaining <= 0 )); then break; fi
  sleep "$INTERVAL_S"
done

final="$(orphan_count)"
python3 - <<PY | tee "$SUMMARY"
import json
print(json.dumps({
  "stamp": "$STAMP",
  "hours_requested": $HOURS,
  "base": "$BASE",
  "itemId": int("$ITEM"),
  "dataDir": "$DATA_DIR",
  "samples": $SAMPLES,
  "churns": $CHURNS,
  "maxOrphansObserved": $MAX_ORPHANS,
  "finalOrphans": $final,
  "fail": bool($FAIL) or $final > 0,
}, indent=2))
PY

if (( FAIL != 0 )) || (( final > 0 )); then
  log "FAIL final_orphans=$final"
  exit 1
fi
log "PASS final_orphans=0 max_observed=$MAX_ORPHANS"
exit 0
