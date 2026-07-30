#!/usr/bin/env bash
# Characterise intermittent first-scrub resume (not CI — Rule 4.2).
# Keep while issue #9 is open; delete when #9 closes. Repo-yes, CI-no.
#
# Four cells × two seek axes × N trials. Failure rates go on the scrub-resume
# issue. If cell (a) matches burned cells → general HLS race; if only (c)/(d)
# fail → burn-path bug.
#
#   ./scripts/soak_scrub.sh
#   TRIALS=10 CELLS=a,c AXES=far_ahead ./scripts/soak_scrub.sh
#   THROTTLE_BPS=5242880 ./scripts/soak_scrub.sh   # Linux IO read cap (no delay knob)
#
# Requires: release nightjar binary, ffmpeg, Chrome, node. Fixture is synthetic
# (Rule 4.3): HEVC + AAC + soft SRT + ASS + PGS, generated via SOAK=1.
set -euo pipefail

ROOT="$(cd "$(dirname "$0")/.." && pwd)"
export PATH="/opt/homebrew/bin:${PATH:-}"

BIN="${NIGHTJAR_BIN:-$ROOT/server/target/release/nightjar}"
PORT="${NIGHTJAR_PORT:-18098}"
TRIALS="${TRIALS:-30}"
CELLS="${CELLS:-a,b,c,d}"
AXES="${AXES:-behind_head,far_ahead}"
OUT_DIR="${OUT_DIR:-/tmp/nj-soak-scrub-$$}"
THROTTLE_BPS="${THROTTLE_BPS:-}"
SKIP_GEN="${SKIP_GEN:-0}"
CHROME_PATH="${CHROME_PATH:-/Applications/Google Chrome.app/Contents/MacOS/Google Chrome}"
FIXTURE_NAME="soak_scrub_hevc_aac_subs_mkv.mkv"
FIXTURE="${FIXTURE:-$ROOT/testdata/files/$FIXTURE_NAME}"

DATA="$(mktemp -d "${TMPDIR:-/tmp}/nj-soak-data.XXXXXX")"
MEDIA="$(mktemp -d "${TMPDIR:-/tmp}/nj-soak-media.XXXXXX")"
LOG="$OUT_DIR/server.log"
JSONL="$OUT_DIR/trials.jsonl"
SUMMARY="$OUT_DIR/summary.txt"
PID=""
THROTTLE_UNIT=""

mkdir -p "$OUT_DIR"

cleanup() {
  if [[ -n "${PID}" ]] && kill -0 "$PID" 2>/dev/null; then
    kill "$PID" 2>/dev/null || true
    wait "$PID" 2>/dev/null || true
  fi
  if [[ -n "${THROTTLE_UNIT}" ]]; then
    systemctl --user stop "$THROTTLE_UNIT" 2>/dev/null || true
  fi
  # Keep OUT_DIR (results); drop ephemeral server state.
  rm -rf "$DATA" "$MEDIA"
}
trap cleanup EXIT

usage() {
  cat <<'EOF'
Characterise intermittent first-scrub resume (not CI — Rule 4.2).

Four cells × two seek axes × N trials. Failure rates go on the scrub-resume
issue. If cell (a) matches burned cells → general HLS race; if only (c)/(d)
fail → burn-path bug.

  ./scripts/soak_scrub.sh
  TRIALS=10 CELLS=a,c AXES=far_ahead ./scripts/soak_scrub.sh
  THROTTLE_BPS=5242880 ./scripts/soak_scrub.sh   # Linux IO read cap (no delay knob)
  SETUP_ONLY=1 ./scripts/soak_scrub.sh           # fixture + scan + inventory only

Requires: release nightjar binary, ffmpeg, Chrome (desktop CDP), node.
Fixture is synthetic (Rule 4.3): HEVC + AAC + soft SRT + ASS + PGS.
On macOS, put FIXTURE on a slow share instead of THROTTLE_BPS.
EOF
}

if [[ "${1:-}" == "-h" || "${1:-}" == "--help" ]]; then
  usage
  exit 0
fi

EXTERNAL="${EXTERNAL:-0}"
BASE_EXT="${BASE:-}"
ITEM_EXT="${ITEM:-}"

if [[ ! -x "$CHROME_PATH" ]]; then
  echo "missing Chrome: $CHROME_PATH (set CHROME_PATH)" >&2
  exit 1
fi
if ! command -v node >/dev/null; then
  echo "node required for soak_scrub_trial.mjs" >&2
  exit 1
fi

if [[ "$EXTERNAL" == "1" ]]; then
  # Use an already-running server + real library item (Spike B closeout).
  if [[ -z "$BASE_EXT" || -z "$ITEM_EXT" ]]; then
    echo "EXTERNAL=1 requires BASE and ITEM" >&2
    exit 1
  fi
  BASE="${BASE_EXT%/}"
  ITEM="$ITEM_EXT"
  LOG="$OUT_DIR/external.log"
  : >"$LOG"
  curl -sf "$BASE/api/health" >/dev/null || {
    echo "EXTERNAL server not healthy: $BASE/api/health" >&2
    exit 1
  }
  INFO=$(curl -sf "$BASE/api/v0/items/${ITEM}/playback-info")
  eval "$(python3 - "$INFO" <<'PY'
import json,sys,shlex
info=json.loads(sys.argv[1])
soft=ass=pgs=""
for t in info.get("subtitleTracks") or []:
    tid=t.get("trackId") or ""
    render=t.get("render") or ""
    codec=(t.get("codec") or "").lower()
    label=(t.get("label") or "").lower()
    if render=="soft" and not soft:
        soft=tid
    if render=="burnIn":
        if ("ass" in codec or "ssa" in codec or "ass" in label) and not ass:
            ass=tid
        if ("pgs" in codec or "hdmv" in codec or "pgs" in label) and not pgs:
            pgs=tid
print(f"SOFT_ID={shlex.quote(soft)}")
print(f"ASS_ID={shlex.quote(ass)}")
print(f"PGS_ID={shlex.quote(pgs)}")
print(f"SUB_STATUS={shlex.quote(info.get('subtitleStatus') or '')}")
print(f"METHOD={shlex.quote(info.get('playbackMethod') or '')}")
print(f"DURATION_MS={int(info.get('durationMs') or 0)}")
PY
)"
  echo "EXTERNAL item=$ITEM method=$METHOD soft=$SOFT_ID ass=$ASS_ID pgs=$PGS_ID dur_ms=$DURATION_MS"
  if [[ "${DURATION_MS:-0}" -lt 90000 ]]; then
    echo "FAIL: item duration ${DURATION_MS}ms < 90s" >&2
    exit 1
  fi
  # Skip synthetic fixture requirements when only burn cells are requested.
  :
else

if [[ ! -x "$BIN" ]]; then
  echo "missing binary: $BIN (cargo build -p nightjar-api --release)" >&2
  exit 1
fi

# --- fixture (synthetic; gitignored) -----------------------------------------
generate_soak_fixture() {
  local out="$1"
  local dur="${SOAK_DUR:-180}"
  local tmp
  tmp="$(mktemp -d "${TMPDIR:-/tmp}/nj-soak-fix.XXXXXX")"
  echo "generating soak fixture ${dur}s → $out"
  cat >"$tmp/soak.ass" <<EOF
[Script Info]
ScriptType: v4.00+
PlayResX: 640
PlayResY: 360

[V4+ Styles]
Format: Name, Fontname, Fontsize, PrimaryColour, SecondaryColour, OutlineColour, BackColour, Bold, Italic, Underline, StrikeOut, ScaleX, ScaleY, Spacing, Angle, BorderStyle, Outline, Shadow, Alignment, MarginL, MarginR, MarginV, Encoding
Style: Default,Arial,36,&H00FFFFFF,&H000000FF,&H00000000,&H00000000,0,0,0,0,100,100,0,0,1,2,0,2,10,10,20,1

[Events]
Format: Layer, Start, End, Style, Name, MarginL, MarginR, MarginV, Effect, Text
Dialogue: 0,0:00:00.00,0:00:05.00,Default,,0,0,0,,soak ASS t0
Dialogue: 0,0:00:40.00,0:00:50.00,Default,,0,0,0,,soak ASS mid
Dialogue: 0,0:02:00.00,0:02:10.00,Default,,0,0,0,,soak ASS late
EOF
  cat >"$tmp/soak.srt" <<EOF
1
00:00:00,000 --> 00:00:05,000
soak SRT t0

2
00:00:40,000 --> 00:00:50,000
soak SRT mid

3
00:02:00,000 --> 00:02:10,000
soak SRT late
EOF
  python3 - "$tmp/soak.sup" <<'PY'
import struct, sys
path = sys.argv[1]
def seg(pts90, typ, payload: bytes) -> bytes:
    return b"PG" + struct.pack(">II", pts90, 0) + bytes([typ]) + struct.pack(">H", len(payload)) + payload
pcs = struct.pack(">HHBHBBb", 640, 360, 0x10, 0, 0x00, 0, 0)
wds = bytes([1]) + struct.pack(">BHHHH", 0, 0, 0, 1, 1)
pds = bytes([0, 0])
open(path, "wb").write(seg(0, 0x16, pcs) + seg(0, 0x17, wds) + seg(0, 0x14, pds) + seg(0, 0x80, b""))
PY
  # Moderate bitrate so encode lags seeks without a product delay knob (Rule 4.7).
  ffmpeg -y -hide_banner -loglevel error \
    -f lavfi -i "testsrc=size=640x360:rate=24:duration=${dur}" \
    -f lavfi -i "sine=frequency=440:sample_rate=48000:duration=${dur}" \
    -i "$tmp/soak.srt" -i "$tmp/soak.ass" -i "$tmp/soak.sup" \
    -map 0:v:0 -map 1:a:0 -map 2:0 -map 3:0 -map 4:0 \
    -c:v libx265 -pix_fmt yuv420p -tag:v hvc1 -b:v 4M \
    -c:a aac -ac 2 \
    -c:s:0 srt -c:s:1 ass -c:s:2 copy \
    -metadata:s:s:0 language=eng -metadata:s:s:0 title="soft" \
    -metadata:s:s:1 language=eng -metadata:s:s:1 title="ass" \
    -metadata:s:s:2 language=eng -metadata:s:s:2 title="pgs" \
    -t "$dur" \
    "$out"
  rm -rf "$tmp"
}

if [[ ! -f "$FIXTURE" ]]; then
  if [[ "$SKIP_GEN" == "1" ]]; then
    echo "missing fixture $FIXTURE (SKIP_GEN=1)" >&2
    exit 1
  fi
  mkdir -p "$(dirname "$FIXTURE")"
  generate_soak_fixture "$FIXTURE"
fi
if [[ ! -f "$FIXTURE" ]]; then
  echo "fixture still missing: $FIXTURE" >&2
  exit 1
fi
# Reject a too-short leftover from a SOAK_DUR smoke run.
DUR_S=$(ffprobe -v error -show_entries format=duration -of csv=p=0 "$FIXTURE" 2>/dev/null | cut -d. -f1)
if [[ "${DUR_S:-0}" -lt 90 ]]; then
  echo "fixture duration ${DUR_S}s < 90s; regenerating with SOAK_DUR=${SOAK_DUR:-180}"
  generate_soak_fixture "$FIXTURE"
fi

# Optional FS throttle: cap read bandwidth for the server process so source
# demux (esp. burn-in) races the client the way a slow share does. No delay-
# injection config on the product (Rule 4.7 / 5.2).
start_server() {
  export NIGHTJAR_DATA_DIR="$DATA"
  export NIGHTJAR_PORT="$PORT"
  export RUST_LOG="${RUST_LOG:-nightjar=info,tower_http=info}"
  if [[ -n "$THROTTLE_BPS" ]]; then
    if command -v systemd-run >/dev/null 2>&1 && [[ "$(uname -s)" == "Linux" ]]; then
      THROTTLE_UNIT="nj-soak-$$.scope"
      echo "THROTTLE_BPS=$THROTTLE_BPS via systemd-run IOReadBandwidthMax"
      systemd-run --user --scope --unit="$THROTTLE_UNIT" \
        -p "IOReadBandwidthMax=$THROTTLE_BPS" \
        "$BIN" >"$LOG" 2>&1 &
      PID=$!
    else
      echo "WARN: THROTTLE_BPS=$THROTTLE_BPS ignored on this host (need Linux systemd-run)." >&2
      echo "      Put the fixture on a slow share and set FIXTURE=/path/on/share instead." >&2
      "$BIN" >"$LOG" 2>&1 &
      PID=$!
    fi
  else
    "$BIN" >"$LOG" 2>&1 &
    PID=$!
  fi
  for _ in $(seq 1 200); do
    curl -sf "http://127.0.0.1:${PORT}/api/health" >/dev/null && return 0
    sleep 0.05
  done
  echo "server failed to become healthy; log:" >&2
  tail -n 80 "$LOG" >&2 || true
  exit 1
}

# Copy (or symlink) fixture into the library tree. When THROTTLE_BPS is set on
# Darwin we still place a real file; use a slow volume path via FIXTURE=.
cp -f "$FIXTURE" "$MEDIA/$FIXTURE_NAME"

start_server

curl -sf -X POST "http://127.0.0.1:${PORT}/api/v0/libraries" \
  -H 'content-type: application/json' \
  -d "{\"name\":\"soak\",\"path\":\"${MEDIA}\",\"kind\":\"movies\"}" >/dev/null
LIB=$(curl -sf "http://127.0.0.1:${PORT}/api/v0/libraries" | python3 -c \
  'import sys,json; print([l["id"] for l in json.load(sys.stdin)["libraries"] if l["name"]=="soak"][0])')
JOB=$(curl -sf -X POST "http://127.0.0.1:${PORT}/api/v0/libraries/${LIB}/scan" | python3 -c \
  'import sys,json; print(json.load(sys.stdin)["jobId"])')

echo -n "scanning"
for _ in $(seq 1 600); do
  ST=$(curl -sf "http://127.0.0.1:${PORT}/api/v0/scan-jobs/${JOB}" | python3 -c \
    'import sys,json; print(json.load(sys.stdin).get("state",""))' 2>/dev/null || echo "")
  if [[ "$ST" == "completed" || "$ST" == "failed" ]]; then
    echo " $ST"
    break
  fi
  echo -n "."
  sleep 0.25
done
if [[ "$ST" != "completed" ]]; then
  echo "scan did not complete: $ST" >&2
  exit 1
fi

ITEM=$(curl -sf "http://127.0.0.1:${PORT}/api/v0/libraries/${LIB}/items" | python3 -c \
  'import sys,json; print(json.load(sys.stdin)["items"][0]["id"])')

# Wait for soft-text extract (cell b) and inventory burn ids.
echo -n "waiting subtitle inventory"
SOFT_ID=""
ASS_ID=""
PGS_ID=""
for _ in $(seq 1 240); do
  INFO=$(curl -sf "http://127.0.0.1:${PORT}/api/v0/items/${ITEM}/playback-info")
  eval "$(python3 - "$INFO" <<'PY'
import json,sys,shlex
info=json.loads(sys.argv[1])
soft=ass=pgs=""
for t in info.get("subtitleTracks") or []:
    tid=t.get("trackId") or ""
    render=t.get("render") or ""
    codec=(t.get("codec") or "").lower()
    label=(t.get("label") or "").lower()
    if render=="soft" and not soft:
        soft=tid
    if render=="burnIn":
        if ("ass" in codec or "ssa" in codec or "ass" in label) and not ass:
            ass=tid
        if ("pgs" in codec or "hdmv" in codec or "pgs" in label) and not pgs:
            pgs=tid
status=info.get("subtitleStatus") or ""
print(f"SOFT_ID={shlex.quote(soft)}")
print(f"ASS_ID={shlex.quote(ass)}")
print(f"PGS_ID={shlex.quote(pgs)}")
print(f"SUB_STATUS={shlex.quote(status)}")
print(f"METHOD={shlex.quote(info.get('playbackMethod') or '')}")
PY
)"
  # Soft URL appears once extract has written WebVTT. Burn tracks need no extract.
  HAS_SOFT_URL=$(python3 -c 'import json,sys; info=json.loads(sys.argv[1]);
print("1" if any((t.get("render")=="soft" and t.get("url")) for t in (info.get("subtitleTracks") or [])) else "0")' "$INFO")
  if [[ -n "$ASS_ID" && -n "$PGS_ID" && "$HAS_SOFT_URL" == "1" ]]; then
    echo " ready"
    break
  fi
  echo -n "."
  sleep 0.5
done

# One more pull so printed status matches inventory.
INFO=$(curl -sf "http://127.0.0.1:${PORT}/api/v0/items/${ITEM}/playback-info")
eval "$(python3 - "$INFO" <<'PY'
import json,sys,shlex
info=json.loads(sys.argv[1])
soft=ass=pgs=""
for t in info.get("subtitleTracks") or []:
    tid=t.get("trackId") or ""
    render=t.get("render") or ""
    codec=(t.get("codec") or "").lower()
    label=(t.get("label") or "").lower()
    if render=="soft" and not soft:
        soft=tid
    if render=="burnIn":
        if ("ass" in codec or "ssa" in codec or "ass" in label) and not ass:
            ass=tid
        if ("pgs" in codec or "hdmv" in codec or "pgs" in label) and not pgs:
            pgs=tid
print(f"SOFT_ID={shlex.quote(soft)}")
print(f"ASS_ID={shlex.quote(ass)}")
print(f"PGS_ID={shlex.quote(pgs)}")
print(f"SUB_STATUS={shlex.quote(info.get('subtitleStatus') or '')}")
print(f"METHOD={shlex.quote(info.get('playbackMethod') or '')}")
PY
)"

echo "item=$ITEM method=$METHOD soft=$SOFT_ID ass=$ASS_ID pgs=$PGS_ID status=$SUB_STATUS"
if [[ "$METHOD" != "transcode" ]]; then
  echo "WARN: expected playbackMethod=transcode (HEVC fixture); got $METHOD" >&2
fi
if [[ -z "$ASS_ID" || -z "$PGS_ID" ]]; then
  echo "FAIL: need ASS + PGS burn tracks on fixture; playback-info:" >&2
  echo "$INFO" | python3 -m json.tool >&2 || echo "$INFO" >&2
  exit 1
fi

BASE="http://127.0.0.1:${PORT}"

fi  # end EXTERNAL else (synthetic fixture path)

if [[ "${SETUP_ONLY:-0}" == "1" ]]; then
  echo "SETUP_ONLY=1: fixture scanned, tracks soft=$SOFT_ID ass=$ASS_ID pgs=$PGS_ID"
  echo "item=$ITEM base=$BASE"
  echo "ready for trials; unset SETUP_ONLY to run the matrix"
  exit 0
fi

: >"$JSONL"
CDP_BASE=19551
trial_n=0
if [[ "$EXTERNAL" == "1" ]]; then
  RESUME_WAIT_MS="${RESUME_WAIT_MS:-90000}"
  ADVANCE_S="${ADVANCE_S:-1.5}"
else
  RESUME_WAIT_MS="${RESUME_WAIT_MS:-25000}"
  ADVANCE_S="${ADVANCE_S:-1.5}"
fi

IFS=',' read -r -a CELL_ARR <<<"$CELLS"
IFS=',' read -r -a AXIS_ARR <<<"$AXES"

for cell in "${CELL_ARR[@]}"; do
  cell="$(echo "$cell" | tr -d '[:space:]')"
  [[ -z "$cell" ]] && continue
  if [[ "$cell" == "b" && -z "$SOFT_ID" ]]; then
    echo "skip cell b: no soft track" >&2
    continue
  fi
  if [[ "$cell" == "c" && -z "$ASS_ID" ]]; then
    echo "skip cell c: no ASS burn track" >&2
    continue
  fi
  if [[ "$cell" == "d" && -z "$PGS_ID" ]]; then
    echo "skip cell d: no PGS burn track" >&2
    continue
  fi
  for axis in "${AXIS_ARR[@]}"; do
    axis="$(echo "$axis" | tr -d '[:space:]')"
    [[ -z "$axis" ]] && continue
    for i in $(seq 1 "$TRIALS"); do
      trial_n=$((trial_n + 1))
      mark="$(date -u +"%Y-%m-%dT%H:%M:%SZ")"
      echo "── trial cell=$cell axis=$axis i=$i/$TRIALS mark=$mark"
      # Drop zombies from a prior CDP hang (SIGKILL in trial; belt-and-braces here).
      pkill -f "nj-soak-chrome-" 2>/dev/null || true
      sleep 0.2
      if [[ "${COLD:-0}" == "1" ]]; then
        # Best-effort page-cache drop so share reads are cold-leaning.
        purge 2>/dev/null || true
      fi
      # Bookmark server log for correlation.
      log_byte=0
      if [[ -f "$LOG" ]]; then
        log_byte=$(wc -c <"$LOG" | tr -d ' ')
      fi
      trial_out="$OUT_DIR/trial_${cell}_${axis}_${i}.json"
      cdp=$((CDP_BASE + (trial_n % 40)))
      # Hard wall: pre-client waits ≤85s + player ≤70s. Override with TRIAL_TIMEOUT_S.
      trial_timeout="${TRIAL_TIMEOUT_S:-160}"
      if [[ "$EXTERNAL" == "1" ]]; then
        trial_timeout="${TRIAL_TIMEOUT_S:-360}"
      fi
      set +e
      BASE="$BASE" ITEM="$ITEM" CELL="$cell" AXIS="$axis" \
        SOFT_ID="$SOFT_ID" ASS_ID="$ASS_ID" PGS_ID="$PGS_ID" \
        OUT="$trial_out" LOG_MARK="$mark" CDP_PORT="$cdp" \
        CHROME_PATH="$CHROME_PATH" \
        RESUME_WAIT_MS="$RESUME_WAIT_MS" \
        ADVANCE_S="$ADVANCE_S" \
        perl -e 'alarm shift; exec @ARGV' "$trial_timeout" \
        node "$ROOT/scripts/soak_scrub_trial.mjs"
      rc=$?
      set -e
      pkill -f "nj-soak-chrome-" 2>/dev/null || true
      if [[ "$rc" -eq 142 || "$rc" -eq 14 ]]; then
        echo "   WARN: trial hard-timeout after ${trial_timeout}s" >&2
        echo "{\"cell\":\"$cell\",\"axis\":\"$axis\",\"resumed\":false,\"error\":\"trial_hard_timeout\"}" >"$trial_out"
        rc=1
      fi
      # Slice server log for this trial and attach served lines.
      python3 - "$trial_out" "$LOG" "$log_byte" "$JSONL" "$rc" <<'PY'
import json, sys
from pathlib import Path
out_path, log_path, byte_off, jsonl, rc = sys.argv[1:6]
byte_off = int(byte_off)
rc = int(rc)
try:
    trial = json.loads(Path(out_path).read_text())
except Exception as e:
    trial = {"resumed": False, "error": f"read trial json: {e}", "rc": rc}
raw = Path(log_path).read_bytes()[byte_off:]
text = raw.decode("utf-8", errors="replace")
sid = trial.get("sessionId") or ""
served = []
for line in text.splitlines():
    if sid and sid in line:
        served.append(line)
    elif "hls_client_req" in line or "request" in line.lower():
        # keep nearby tower_http / restart lines without session filter noise cap
        if any(k in line for k in ("restart_at", "desire", "cooking", "503", "start_ms", "startMs")):
            served.append(line)
trial["serverServed"] = served[-200:]
trial["rc"] = rc
# Compact correlation: FRAG/BUFFER/ERROR walls vs nearby served lines.
events = trial.get("hlsEvents") or trial.get("allEvents") or []
corr = []
for ev in events:
    if ev.get("kind") not in ("FRAG_LOADED", "BUFFER_APPENDED", "ERROR", "scrub_intent", "land_seg_ok"):
        continue
    wall = ev.get("wall") or ""
    nearby = [s for s in served if wall[:19] in s] if wall else []
    corr.append({"event": ev, "serverNearby": nearby[:8]})
trial["correlation"] = corr
Path(out_path).write_text(json.dumps(trial, indent=2))
with open(jsonl, "a") as f:
    f.write(json.dumps({
        "cell": trial.get("cell"),
        "axis": trial.get("axis"),
        "resumed": trial.get("resumed"),
        "sessionId": trial.get("sessionId"),
        "scrubMs": trial.get("scrubMs"),
        "error": trial.get("error"),
        "rc": rc,
        "clientRequestedN": len(trial.get("clientRequested") or []),
        "serverServedN": len(trial.get("serverServed") or []),
        "hlsErrorN": sum(1 for e in events if e.get("kind")=="ERROR"),
        "out": out_path,
    }) + "\n")
print("   resumed=%s rc=%s served_lines=%d" % (
    trial.get("resumed"), rc, len(trial.get("serverServed") or [])))
PY
      # Brief settle so session DELETE and encoder teardown do not overlap.
      sleep 0.4
    done
  done
done

python3 - "$JSONL" "$SUMMARY" <<'PY'
import json, sys
from collections import defaultdict
from pathlib import Path
jsonl, summary = sys.argv[1:3]
rows = [json.loads(l) for l in Path(jsonl).read_text().splitlines() if l.strip()]
buckets = defaultdict(lambda: {"n": 0, "fail": 0})
for r in rows:
    key = (r.get("cell"), r.get("axis"))
    buckets[key]["n"] += 1
    if not r.get("resumed"):
        buckets[key]["fail"] += 1

lines = []
lines.append("soak_scrub failure rates (paste onto the first-scrub resume issue)")
lines.append("")
lines.append(f"{'cell':<6} {'axis':<14} {'n':>4} {'fail':>5} {'rate':>8}")
lines.append("-" * 42)
cell_fail = defaultdict(lambda: {"n": 0, "fail": 0})
for (cell, axis), s in sorted(buckets.items()):
    rate = (100.0 * s["fail"] / s["n"]) if s["n"] else 0.0
    lines.append(f"{cell:<6} {axis:<14} {s['n']:>4} {s['fail']:>5} {rate:>7.1f}%")
    cell_fail[cell]["n"] += s["n"]
    cell_fail[cell]["fail"] += s["fail"]
lines.append("")
lines.append("per cell (axes combined):")
for cell, s in sorted(cell_fail.items()):
    rate = (100.0 * s["fail"] / s["n"]) if s["n"] else 0.0
    label = {"a": "transcode, no subs", "b": "transcode, soft WebVTT",
             "c": "transcode, burned ASS", "d": "transcode, burned PGS"}.get(cell, cell)
    lines.append(f"  {cell} ({label}): {s['fail']}/{s['n']} = {rate:.1f}%")
lines.append("")
a = cell_fail.get("a", {"n": 0, "fail": 0})
c = cell_fail.get("c", {"n": 0, "fail": 0})
d = cell_fail.get("d", {"n": 0, "fail": 0})
def rate(s):
    return (100.0 * s["fail"] / s["n"]) if s["n"] else None
ra, rc, rd = rate(a), rate(c), rate(d)
if ra is not None and rc is not None and rd is not None:
    burn = max(rc, rd)
    total_fail = sum(s["fail"] for s in cell_fail.values())
    if total_fail == 0 and sum(s["n"] for s in cell_fail.values()) > 0:
        lines.append(
            "label hint: no failures — soak did not reproduce the dogfood "
            "stuck-after-land; leave issue open, do not clear or relabel from this run alone"
        )
    elif a["n"] and burn - ra < 5 and ra > 0:
        lines.append("label hint: cell (a) ≈ burned → evidence for general HLS race")
    elif a["n"] and ra < 5 and burn >= 10:
        lines.append("label hint: only (c)/(d) fail → burn-path bug")
    else:
        lines.append("label hint: inconclusive from these rates alone; inspect correlation JSON")
lines.append("")
lines.append(f"jsonl: {jsonl}")
text = "\n".join(lines) + "\n"
Path(summary).write_text(text)
print(text)
PY

echo "results in $OUT_DIR"
echo "summary: $SUMMARY"
