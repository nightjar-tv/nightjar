#!/usr/bin/env bash
# Part 1 — pin encode to Arc (renderD129) without a Nightjar binary change.
#
# Two shapes (both recorded):
#   A. Raw Jellyfin-FFmpeg invocations with explicit device args (the contract
#      a future NIGHTJAR_DRM_DEVICE setting must emit).
#   B. Nightjar container with host renderD129 remapped to container
#      renderD128 so today's hardcoded VAAPI path and default QSV bind hit Arc.
#
# Usage (on Unraid host, nightjar-test already known-good):
#   SRC='/media/Movies/Some Title (2020)/Some Title (2020) Bluray-1080p.mkv' \
#   OUT=/mnt/user/appdata/nightjar-test/measure \
#   ./unraid_arc_pin_measure.sh
#
# Optional: PAUSE_EMBY=1 docker pause emby before timing (recommended).
set -euo pipefail

OUT="${OUT:-/mnt/user/appdata/nightjar-test/measure}"
CTN="${CTN:-nightjar-test}"
NJ_BIN_HOST="${NJ_BIN_HOST:-/mnt/user/appdata/nightjar-test/nightjar}"
DATA_HOST="${DATA_HOST:-/mnt/user/appdata/nightjar-test/data}"
MEDIA_HOST="${MEDIA_HOST:-/mnt/user/media}"
PORT="${PORT:-18096}"
DURATION_S="${DURATION_S:-30}"
SRC="${SRC:-}"
PAUSE_EMBY="${PAUSE_EMBY:-0}"
STAMP="$(date -u +%Y%m%dT%H%M%SZ)"
mkdir -p "$OUT"
NOTE="$OUT/arc-pin-raw-$STAMP.md"
JSON="$OUT/arc-pin-raw-$STAMP.json"

log() { printf '%s %s\n' "$(date -u +%Y-%m-%dT%H:%M:%SZ)" "$*" | tee -a "$OUT/arc-pin-$STAMP.log"; }

need() { command -v "$1" >/dev/null || { echo "missing $1" >&2; exit 1; }; }
need docker
need curl
# python3 optional — JSON summary is nicer with it; timings still write .sec/.rc files.
HAVE_PY=0
command -v python3 >/dev/null && HAVE_PY=1

if [[ -z "$SRC" ]]; then
  echo "Set SRC to a container-visible 1080p mkv path, e.g. SRC='/media/Movies/.../file.mkv'" >&2
  exit 1
fi

EMBY_WAS_RUNNING=0
if docker ps --format '{{.Names}}' | grep -qx emby; then
  EMBY_WAS_RUNNING=1
fi
if [[ "$PAUSE_EMBY" == "1" && "$EMBY_WAS_RUNNING" == "1" ]]; then
  log "pausing emby for clean Arc timings"
  docker pause emby
fi
restore_emby() {
  if [[ "$PAUSE_EMBY" == "1" && "$EMBY_WAS_RUNNING" == "1" ]]; then
    docker unpause emby 2>/dev/null || true
    log "unpaused emby"
  fi
}
trap restore_emby EXIT

# --- timed encode helper inside current nightjar-test (has jellyfin ffmpeg) ---
run_timed() {
  local label="$1"
  shift
  local t0 t1 rc=0
  t0=$(date +%s)
  set +e
  docker exec "$CTN" ffmpeg -nostdin -hide_banner -loglevel error -y \
    -t "$DURATION_S" "$@" -f null - 2>"$OUT/${label}.err"
  rc=$?
  set -e
  t1=$(date +%s)
  echo $((t1 - t0)) >"$OUT/${label}.sec"
  echo "$rc" >"$OUT/${label}.rc"
  log "$label rc=$rc sec=$(cat "$OUT/${label}.sec")"
}

log "=== raw device invocations (duration=${DURATION_S}s src=$SRC) ==="

# Baseline: whatever Nightjar already prefers on full /dev/dri (usually iGPU QSV)
run_timed "qsv_default" -i "$SRC" -map 0:v:0 -an -c:v h264_qsv -pix_fmt nv12

# VAAPI explicit nodes
run_timed "vaapi_128" -vaapi_device /dev/dri/renderD128 -i "$SRC" -map 0:v:0 -an \
  -vf 'format=nv12,hwupload' -c:v h264_vaapi
run_timed "vaapi_129" -vaapi_device /dev/dri/renderD129 -i "$SRC" -map 0:v:0 -an \
  -vf 'format=nv12,hwupload' -c:v h264_vaapi

# QSV pinned to Arc — try the shapes a setting would emit
run_timed "qsv_qsv_device_129" -qsv_device /dev/dri/renderD129 -i "$SRC" \
  -map 0:v:0 -an -c:v h264_qsv -pix_fmt nv12

run_timed "qsv_init_hw_129" \
  -init_hw_device "vaapi=va:/dev/dri/renderD129" \
  -init_hw_device "qsv=hw@va" \
  -filter_hw_device hw \
  -i "$SRC" -map 0:v:0 -an -c:v h264_qsv -pix_fmt nv12

# libx264 reference on same clip length
run_timed "x264" -i "$SRC" -map 0:v:0 -an -c:v libx264 -preset veryfast -crf 23

log "=== recreate Nightjar with Arc remapped to /dev/dri/renderD128 ==="
# Stop the full-DRI instance so we do not share one data dir with two writers.
if docker ps --format '{{.Names}}' | grep -qx "$CTN"; then
  log "stopping $CTN (same data dir; restart after measure)"
  docker stop "$CTN"
fi
# No binary change: container path Nightjar hardcodes is Arc's host node.
docker rm -f "${CTN}-arc" 2>/dev/null || true
docker run -d \
  --name "${CTN}-arc" \
  --device=/dev/dri/renderD129:/dev/dri/renderD128 \
  -p 18097:8096 \
  -v "${DATA_HOST}:/config" \
  -v "${NJ_BIN_HOST}:/nightjar:ro" \
  -v "${MEDIA_HOST}:/media:ro" \
  -e NIGHTJAR_DATA_DIR=/config \
  -e NIGHTJAR_PORT=8096 \
  -e NIGHTJAR_POLL_ONLY=1 \
  -e NIGHTJAR_HLS_MAX_SESSIONS=8 \
  --entrypoint /nightjar \
  jellyfin/jellyfin:latest

sleep 3
for i in 1 2 3 4 5 6 7 8 9 10; do
  code=$(curl -sf -o /dev/null -w '%{http_code}' "http://127.0.0.1:18097/api/health" || true)
  [[ "$code" == "200" ]] && break
  sleep 1
done

curl -s "http://127.0.0.1:18097/api/v0/system/transcode" | tee "$OUT/transcode-arc-remap-$STAMP.json"
log "wrote transcode-arc-remap JSON"

# Pick a transcode item (python if present; else leave blank for manual).
ITEM=""
SESSION_NODE="(no session)"
SESSION_ENCODER="(unknown)"
if [[ "$HAVE_PY" == "1" ]]; then
  ITEM=$(python3 - <<'PY'
import json, urllib.request
base="http://127.0.0.1:18097"
try:
    libs=json.load(urllib.request.urlopen(base+"/api/v0/libraries", timeout=15))["libraries"]
except Exception:
    raise SystemExit
for lib in libs:
    items=json.load(urllib.request.urlopen(
        f"{base}/api/v0/libraries/{lib['id']}/items?limit=100", timeout=30))["items"]
    for it in items:
        if it.get("playbackMethod")=="transcode":
            print(it["id"]); raise SystemExit
PY
) || true
fi

if [[ -n "$ITEM" ]]; then
  body=$(curl -sf -X POST "http://127.0.0.1:18097/api/v0/items/${ITEM}/sessions?startMs=0" || true)
  echo "$body" | tee "$OUT/session-arc-remap-$STAMP.json"
  SID=$(printf '%s' "$body" | sed -n 's/.*"sessionId"[[:space:]]*:[[:space:]]*"\([^"]*\)".*/\1/p' | head -1)
  sleep 3
  if [[ -n "$SID" ]]; then
    SESSION_NODE=$(docker exec "${CTN}-arc" sh -c '
      for p in /proc/[0-9]*; do
        tr "\0" " " < "$p/cmdline" 2>/dev/null | grep -q "[f]fmpeg" || continue
        tr "\0" " " < "$p/cmdline" 2>/dev/null | grep -q "h264_" || continue
        ls -l "$p/fd" 2>/dev/null | grep -E "renderD|dri" || true
      done
    ' | head -5 || true)
    SESSION_ENCODER=$(docker exec "${CTN}-arc" sh -c '
      for p in /proc/[0-9]*; do
        cmd=$(tr "\0" " " < "$p/cmdline" 2>/dev/null || true)
        echo "$cmd" | grep -q "[f]fmpeg" || continue
        echo "$cmd" | grep -oE "h264_[a-z0-9]+" | head -1
      done
    ' | head -1 || true)
    curl -sf -X DELETE "http://127.0.0.1:18097/api/v0/sessions/$SID" -o /dev/null || true
  fi
else
  log "WARN: no auto item pick (need python3 + scanned library on :18097). Start a transcode in the UI and check DRM fds manually."
fi

{
  echo "# Unraid Arc pin measure ($STAMP)"
  echo
  echo "Raw timed encodes (${DURATION_S}s of SRC) and Nightjar Arc remap (no binary change)."
  echo
  echo "## Emby"
  echo "- was running at start: $EMBY_WAS_RUNNING"
  echo "- paused for measure: $PAUSE_EMBY"
  echo
  echo "## Timed encodes (wall seconds, integer)"
  for lab in qsv_default vaapi_128 vaapi_129 qsv_qsv_device_129 qsv_init_hw_129 x264; do
    sec="?"; rc="?"
    [[ -f "$OUT/${lab}.sec" ]] && sec=$(cat "$OUT/${lab}.sec")
    [[ -f "$OUT/${lab}.rc" ]] && rc=$(cat "$OUT/${lab}.rc")
    echo "- ${lab}: rc=${rc} sec=${sec}"
    if [[ -f "$OUT/${lab}.err" && "$rc" != "0" ]]; then
      echo "  err: $(tail -1 "$OUT/${lab}.err" | cut -c1-200)"
    fi
  done
  echo
  echo "## Nightjar Arc remap"
  echo "- map: host renderD129 -> container renderD128"
  echo "- API JSON: $OUT/transcode-arc-remap-$STAMP.json"
  echo "- session encoder: $SESSION_ENCODER"
  echo "- session DRM fds:"
  echo '```'
  echo "$SESSION_NODE"
  echo '```'
  echo
  echo "## Invocation shapes for a future device setting"
  echo '- VAAPI: `ffmpeg -vaapi_device /dev/dri/renderD129 ... -vf format=nv12,hwupload -c:v h264_vaapi`'
  echo '- QSV try A: `ffmpeg -qsv_device /dev/dri/renderD129 ... -c:v h264_qsv`'
  echo '- QSV try B: `ffmpeg -init_hw_device vaapi=va:/dev/dri/renderD129 -init_hw_device qsv=hw@va -filter_hw_device hw ... -c:v h264_qsv`'
  echo '- Nightjar today: remap node into `/dev/dri/renderD128` (hardcoded verify path)'
} | tee "$NOTE"

# Copy a stub JSON pointer file even without python.
{
  echo "{"
  echo "  \"stamp\": \"$STAMP\","
  echo "  \"note\": \"$NOTE\","
  echo "  \"transcodeArcRemap\": \"$OUT/transcode-arc-remap-$STAMP.json\""
  echo "}"
} >"$JSON"

log "DONE note=$NOTE json=$JSON"
log "Arc Nightjar still running as ${CTN}-arc on :18097 — docker rm -f ${CTN}-arc when finished"
