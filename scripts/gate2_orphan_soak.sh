#!/usr/bin/env bash
# Gate 2 orphan soak: churn sessions for HOURS (default 48) and assert no
# leftover FFmpeg children whose cmdline still names this Nightjar data dir.
#
# Sequencing: run only after ADR-0020 (PR #11) is on the binary under test.
# Soaking pre-0020 session code measures a lifecycle about to be replaced.
#
# Host: prefer the Unraid (or other always-on) box. A sleeping laptop aborts
# the wall-clock run; nohup does not survive sleep. Unraid is also closer to
# a real deployment than founder desktop.
#
# Usage (on Unraid, post-#11 build):
#   BASE=http://127.0.0.1:8096 HOURS=48 ./scripts/gate2_orphan_soak.sh
#
# Every INTERVAL_S sample appends one JSON line to
# notes/gate2/orphan-soak-<stamp>.jsonl with the scheduled ffmpeg process
# count (after DELETE + short reap wait). That series distinguishes a leak
# that appears and gets reaped from one that accumulates. The .log is human
# tail; the JSONL is the Gate 2 artifact.
#
# Exit 0 only if the soak completed with final count 0 and no sticky orphan
# after an extra quiet window.
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
SERIES="$OUT_DIR/orphan-soak-$STAMP.jsonl"
SUMMARY="$OUT_DIR/orphan-soak-$STAMP.json"

DATA_DIR="${NIGHTJAR_DATA_DIR:-$HOME/nightjar-data}"
END_EPOCH=$(( $(date +%s) + HOURS * 3600 ))

log() { printf '%s %s\n' "$(date -u +%Y-%m-%dT%H:%M:%SZ)" "$*" | tee -a "$LOG"; }

ffmpeg_matching() {
  # Nightjar-owned FFmpeg: argv still references this data dir.
  pgrep -lf '[f]fmpeg' 2>/dev/null | grep -F "$DATA_DIR" || true
}

ffmpeg_count() {
  ffmpeg_matching | wc -l | tr -d ' '
}

record_sample() {
  # Always write the scheduled count — zeros matter as much as spikes.
  local sample="$1" churns="$2" count="$3" phase="$4"
  python3 -c "
import json, time
print(json.dumps({
  'ts': time.strftime('%Y-%m-%dT%H:%M:%SZ', time.gmtime()),
  'sample': int('$sample'),
  'churns': int('$churns'),
  'ffmpegCount': int('$count'),
  'phase': '$phase',
}))
" >>"$SERIES"
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

log "start hours=$HOURS interval=${INTERVAL_S}s item=$ITEM data_dir=$DATA_DIR base=$BASE series=$SERIES"
log "prerequisite: binary must include ADR-0020 (PR #11); host should not sleep"
MAX_FFMPEG=0
SAMPLES=0
CHURNS=0
FAIL=0
SPIKE_SAMPLES=0

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
    # Count while a session may still hold FFmpeg (transient is OK).
    live="$(ffmpeg_count)"
    record_sample "$SAMPLES" "$CHURNS" "$live" "post_attach"
    if (( live > MAX_FFMPEG )); then MAX_FFMPEG=$live; fi

    sleep 2
    curl -sf -X DELETE "$BASE/api/v0/sessions/$sid" -o /dev/null || true
  else
    log "WARN: session create failed body=${body:0:200}"
    record_sample "$SAMPLES" "$CHURNS" "$(ffmpeg_count)" "create_failed"
  fi

  # Allow idle reaper a beat, then scheduled post-reap sample.
  sleep 5
  n="$(ffmpeg_count)"
  if (( n > MAX_FFMPEG )); then MAX_FFMPEG=$n; fi
  record_sample "$SAMPLES" "$CHURNS" "$n" "post_reap"
  log "sample=$SAMPLES churns=$CHURNS ffmpeg_post_reap=$n"

  if (( n > 0 )); then
    SPIKE_SAMPLES=$((SPIKE_SAMPLES + 1))
    ffmpeg_matching | tee -a "$LOG" || true
    # Extra quiet window — reaper may still be within idle grace.
    sleep 30
    n2="$(ffmpeg_count)"
    record_sample "$SAMPLES" "$CHURNS" "$n2" "post_reap_quiet"
    if (( n2 > 0 )); then
      log "FAIL sticky_orphans count=$n2"
      FAIL=1
      ffmpeg_matching | tee -a "$LOG" || true
    else
      log "transient_reaped was=$n now=0 (not sticky)"
    fi
  fi

  remaining=$(( END_EPOCH - $(date +%s) ))
  if (( remaining <= 0 )); then break; fi
  sleep "$INTERVAL_S"
done

final="$(ffmpeg_count)"
record_sample "$SAMPLES" "$CHURNS" "$final" "final"
python3 - <<PY | tee "$SUMMARY"
import json
print(json.dumps({
  "stamp": "$STAMP",
  "hours_requested": $HOURS,
  "base": "$BASE",
  "itemId": int("$ITEM"),
  "dataDir": "$DATA_DIR",
  "seriesPath": "$SERIES",
  "samples": $SAMPLES,
  "churns": $CHURNS,
  "maxFfmpegObserved": $MAX_FFMPEG,
  "postReapSpikeSamples": $SPIKE_SAMPLES,
  "finalFfmpeg": $final,
  "fail": bool($FAIL) or $final > 0,
}, indent=2))
PY

if (( FAIL != 0 )) || (( final > 0 )); then
  log "FAIL final_ffmpeg=$final"
  exit 1
fi
log "PASS final_ffmpeg=0 max_observed=$MAX_FFMPEG post_reap_spikes=$SPIKE_SAMPLES"
exit 0
