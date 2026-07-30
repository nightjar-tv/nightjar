#!/usr/bin/env bash
# Spike C — encoder restart / burn-in splice without client-visible break.
# Three-parser regression check for the deferred splice slice (ADR-0018).
# Keep until that slice lands or is formally dropped (then delete with
# findings only if the ADR citation is rewritten). Not CI — Rule 4.2.
#
# Builds a fMP4 HLS tree: segs before SPLICE_SEG with no ass=, then
# restarts the encoder at the head with ass= while holding init.mp4,
# encoder params, and PTS continuity. Serves two playlists (no
# EXT-X-DISCONTINUITY vs with) and drives Chromium/hls.js, Safari native,
# Firefox/hls.js.
set -euo pipefail

ROOT="$(cd "$(dirname "$0")/.." && pwd)"
WORK="${SPIKE_C_WORK:-$ROOT/scripts/spike_c_work}"
FFMPEG="${FFMPEG:-ffmpeg}"
SEGMENT_S=2
SPLICE_SEG="${SPLICE_SEG:-3}"          # first burned segment index
PRE_S=$((SPLICE_SEG * SEGMENT_S))      # seconds encoded without burn
POST_SEGS="${POST_SEGS:-5}"
POST_S=$((POST_SEGS * SEGMENT_S))
TOTAL_S=$((PRE_S + POST_S))
PORT="${SPIKE_C_PORT:-19641}"
OUT_JSON="${SPIKE_C_OUT:-$WORK/results.json}"
SKIP_BROWSERS="${SPIKE_C_SKIP_BROWSERS:-0}"

SDR='sidedata=delete,setparams=color_primaries=bt709:color_trc=bt709:colorspace=bt709'
FORCE_KF="expr:gte(t,n_forced*${SEGMENT_S})"
HLS_COMMON=(
  -c:v libx264 -preset veryfast -pix_fmt yuv420p
  -colorspace bt709 -color_primaries bt709 -color_trc bt709
  -force_key_frames "$FORCE_KF" -g 600 -keyint_min 48 -sc_threshold 0
  -c:a aac -ac 2 -b:a 128k
  -f hls -hls_time "$SEGMENT_S" -hls_list_size 0
  -hls_flags independent_segments+temp_file
  -hls_segment_type fmp4 -hls_fmp4_init_filename init.mp4
  -hls_segment_filename 'seg%03d.m4s'
)

rm -rf "$WORK"
mkdir -p "$WORK/hls"
cd "$WORK"

echo "→ source ${TOTAL_S}s + ASS (splice at seg ${SPLICE_SEG} = ${PRE_S}s)"
# Visible cue every 2s so a human (or frame grab) can see burn attach.
{
  cat <<'EOF'
[Script Info]
ScriptType: v4.00+
PlayResX: 1280
PlayResY: 720

[V4+ Styles]
Format: Name, Fontname, Fontsize, PrimaryColour, SecondaryColour, OutlineColour, BackColour, Bold, Italic, Underline, StrikeOut, ScaleX, ScaleY, Spacing, Angle, BorderStyle, Outline, Shadow, Alignment, MarginL, MarginR, MarginV, Encoding
Style: Default,Arial,64,&H0000FFFF,&H000000FF,&H00000000,&H00000000,0,0,0,0,100,100,0,0,1,3,0,2,20,20,40,1

[Events]
Format: Layer, Start, End, Style, Name, MarginL, MarginR, MarginV, Effect, Text
EOF
  for ((t = 0; t < TOTAL_S; t += 2)); do
    printf 'Dialogue: 0,0:00:%02d.00,0:00:%02d.80,Default,,0,0,0,,BURN t=%ds\n' "$t" "$((t + 1))" "$t"
  done
} >burn.ass

"$FFMPEG" -y -hide_banner -loglevel error \
  -f lavfi -i "testsrc=size=1280x720:rate=24:duration=${TOTAL_S}" \
  -f lavfi -i "sine=frequency=440:sample_rate=48000:duration=${TOTAL_S}" \
  -c:v libx264 -pix_fmt yuv420p -c:a aac -ac 2 -shortest \
  src.mp4

echo "→ phase A: encode segs 0..$((SPLICE_SEG - 1)) without ass="
(
  cd hls
  "$FFMPEG" -y -hide_banner -loglevel error -nostdin \
    -i ../src.mp4 -t "$PRE_S" \
    -map 0:v:0 -map 0:a:0 \
    -vf "$SDR" \
    "${HLS_COMMON[@]}" \
    -start_number 0 \
    index_ffmpeg_a.m3u8
)
test -f hls/init.mp4
cp hls/init.mp4 hls/init.keep
PRE_COUNT=$(ls hls/seg*.m4s 2>/dev/null | wc -l | tr -d ' ')
echo "   phase A wrote ${PRE_COUNT} segments + init.mp4"

echo "→ phase B: restart at ${PRE_S}s with ass=, start_number=${SPLICE_SEG}"
(
  cd hls
  "$FFMPEG" -y -hide_banner -loglevel error -nostdin \
    -ss "$PRE_S" -i ../src.mp4 -t "$POST_S" \
    -output_ts_offset "$PRE_S" \
    -map 0:v:0 -map 0:a:0 \
    -vf "ass=../burn.ass,${SDR}" \
    "${HLS_COMMON[@]}" \
    -start_number "$SPLICE_SEG" \
    index_ffmpeg_b.m3u8
)

# Hold the phase-A init segment constant (phase B may rewrite it).
cp hls/init.keep hls/init.mp4
POST_COUNT=0
for ((i = SPLICE_SEG; i < SPLICE_SEG + POST_SEGS; i++)); do
  printf -v name 'seg%03d.m4s' "$i"
  if [[ -f "hls/$name" ]]; then
    POST_COUNT=$((POST_COUNT + 1))
  fi
done
echo "   phase B wrote ${POST_COUNT} post-splice segments; init restored from phase A"

write_playlist() {
  local path=$1
  local with_disc=$2
  {
    echo '#EXTM3U'
    echo '#EXT-X-VERSION:7'
    echo '#EXT-X-TARGETDURATION:2'
    echo '#EXT-X-PLAYLIST-TYPE:VOD'
    echo '#EXT-X-MEDIA-SEQUENCE:0'
    echo '#EXT-X-MAP:URI="init.mp4"'
    for ((i = 0; i < SPLICE_SEG + POST_SEGS; i++)); do
      printf -v name 'seg%03d.m4s' "$i"
      if [[ ! -f "hls/$name" ]]; then
        echo "missing $name" >&2
        exit 1
      fi
      if [[ "$with_disc" == 1 && "$i" -eq "$SPLICE_SEG" ]]; then
        echo '#EXT-X-DISCONTINUITY'
      fi
      echo '#EXTINF:2.000000,'
      echo "$name"
    done
    echo '#EXT-X-ENDLIST'
  } >"$path"
}

write_playlist hls/index_nodisc.m3u8 0
write_playlist hls/index_disc.m3u8 1
cat >hls/master_nodisc.m3u8 <<'EOF'
#EXTM3U
#EXT-X-VERSION:7
#EXT-X-STREAM-INF:BANDWIDTH=2500000
index_nodisc.m3u8
EOF
cat >hls/master_disc.m3u8 <<'EOF'
#EXTM3U
#EXT-X-VERSION:7
#EXT-X-STREAM-INF:BANDWIDTH=2500000
index_disc.m3u8
EOF

# Serve HLS + local hls.js + probe page.
HLS_JS="$ROOT/web/node_modules/hls.js/dist/hls.min.js"
cp "$HLS_JS" hls/hls.min.js
cp "$ROOT/scripts/spike_c_page.html" hls/index.html

echo "→ http://127.0.0.1:${PORT}/ (splice_s=${PRE_S})"
python3 - <<PY &
from http.server import ThreadingHTTPServer, SimpleHTTPRequestHandler
import os
os.chdir("$WORK/hls")
class H(SimpleHTTPRequestHandler):
    def log_message(self, fmt, *args):
        pass
ThreadingHTTPServer(("127.0.0.1", $PORT), H).serve_forever()
PY
SRV_PID=$!
cleanup() { kill "$SRV_PID" 2>/dev/null || true; }
trap cleanup EXIT
sleep 0.3

META_JSON=$(cat <<EOF
{
  "spliceSeg": $SPLICE_SEG,
  "spliceS": $PRE_S,
  "preSegCount": $PRE_COUNT,
  "postSegCount": $POST_COUNT,
  "totalS": $TOTAL_S,
  "port": $PORT
}
EOF
)
echo "$META_JSON" >"$WORK/meta.json"

if [[ "$SKIP_BROWSERS" == 1 ]]; then
  echo "SPIKE_C_SKIP_BROWSERS=1 — media ready at $WORK/hls"
  echo "$META_JSON"
  # Keep server up for manual dogfood when skipped.
  trap - EXIT
  echo "server pid $SRV_PID on :$PORT (kill manually)"
  exit 0
fi

BASE="http://127.0.0.1:${PORT}"
PROBE_DIR="$WORK/probes"
mkdir -p "$PROBE_DIR"
: >"$WORK/probes.jsonl"

run_one() {
  local consumer=$1
  local variant=$2   # nodisc|disc
  local master="master_${variant}.m3u8"
  local out="$PROBE_DIR/${consumer}_${variant}.json"
  echo "→ probe consumer=$consumer variant=$variant"
  case "$consumer" in
    chrome)
      SPIKE_BASE="$BASE" SPIKE_MASTER="$master" SPIKE_SPLICE_S="$PRE_S" \
        SPIKE_OUT="$out" SPIKE_CDP_PORT=19651 \
        node "$ROOT/scripts/spike_c_probe_chrome.mjs" || true
      ;;
    firefox)
      SPIKE_BASE="$BASE" SPIKE_MASTER="$master" SPIKE_SPLICE_S="$PRE_S" \
        SPIKE_OUT="$out" SPIKE_GECKO_PORT=19652 \
        python3 "$ROOT/scripts/spike_c_probe_firefox.py" || true
      ;;
    safari)
      SPIKE_BASE="$BASE" SPIKE_MASTER="$master" SPIKE_SPLICE_S="$PRE_S" \
        SPIKE_OUT="$out" SPIKE_SAFARI_PORT=19653 \
        python3 "$ROOT/scripts/spike_c_probe_safari.py" || true
      ;;
  esac
  if [[ -f "$out" ]]; then
    python3 -c "import json,sys; print(json.dumps(json.load(open(sys.argv[1]))))" "$out" >>"$WORK/probes.jsonl"
  else
    printf '{"consumer":"%s","variant":"%s","error":"no_probe_output"}\n' "$consumer" "$variant" >>"$WORK/probes.jsonl"
  fi
}

for variant in nodisc disc; do
  for consumer in chrome firefox safari; do
    run_one "$consumer" "$variant"
  done
done

python3 - <<PY
import json
from pathlib import Path
meta = json.loads(Path("$WORK/meta.json").read_text())
rows = []
for line in Path("$WORK/probes.jsonl").read_text().splitlines():
    line = line.strip()
    if line:
        rows.append(json.loads(line))
out = {"meta": meta, "probes": rows}
Path("$OUT_JSON").write_text(json.dumps(out, indent=2) + "\n")
print(json.dumps(out, indent=2))
PY

echo "wrote $OUT_JSON"
