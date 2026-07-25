#!/usr/bin/env bash
# Gate 1 smoke checks for CI (startup, idle RAM, WAL kill-9, open-ended Range).
set -euo pipefail

ROOT="$(cd "$(dirname "$0")/.." && pwd)"
export PATH="/opt/homebrew/bin:${PATH:-}"

BIN="${NIGHTJAR_BIN:-$ROOT/server/target/release/nightjar}"
PORT="${NIGHTJAR_PORT:-18097}"
DATA="$(mktemp -d)"
MEDIA="$(mktemp -d)"
LOG="$(mktemp)"
PID=""

cleanup() {
  if [[ -n "${PID}" ]] && kill -0 "$PID" 2>/dev/null; then
    kill "$PID" 2>/dev/null || true
    wait "$PID" 2>/dev/null || true
  fi
  rm -rf "$DATA" "$MEDIA" "$LOG"
}
trap cleanup EXIT

if [[ ! -x "$BIN" ]]; then
  echo "missing binary: $BIN (build release nightjar first)" >&2
  exit 1
fi

ffmpeg -y -hide_banner -loglevel error \
  -f lavfi -i "testsrc=size=320x240:rate=24:duration=1" \
  -f lavfi -i "sine=frequency=440:duration=1" \
  -c:v libx264 -pix_fmt yuv420p -c:a aac -ac 2 -shortest \
  "$MEDIA/sample.mp4"

# Startup gate: sample several launches and gate on the median, not one run.
# First exec of a fresh binary on macOS pays a one-off Gatekeeper assessment
# (~300-800ms pre-main, measured 2026-07); it does not exist on Linux/Pi.
STARTUP_RUNS="${STARTUP_RUNS:-5}"
BIN_PATH="$BIN" NIGHTJAR_DATA_DIR="$DATA" NIGHTJAR_PORT="$PORT" LOG_PATH="$LOG" \
  STARTUP_RUNS="$STARTUP_RUNS" python3 - <<'PY'
import os, statistics, subprocess, time, urllib.request

bin_path = os.environ["BIN_PATH"]
port = os.environ["NIGHTJAR_PORT"]
runs = int(os.environ["STARTUP_RUNS"])
samples = []
for _ in range(runs):
    log = open(os.environ["LOG_PATH"], "w")
    t0 = time.perf_counter()
    p = subprocess.Popen([bin_path], stdout=log, stderr=subprocess.STDOUT, env=os.environ)
    ok = False
    for _ in range(600):
        try:
            with urllib.request.urlopen(f"http://127.0.0.1:{port}/api/health", timeout=0.2) as r:
                if r.status == 200:
                    ok = True
                    break
        except Exception:
            time.sleep(0.005)
    samples.append(int((time.perf_counter() - t0) * 1000))
    p.kill()
    p.wait()
    if not ok:
        raise SystemExit("health never became ready")
median = statistics.median(samples)
print(f"startup_ms samples={samples} median={median:.0f} min={min(samples)} max={max(samples)}")
if median > 500:
    raise SystemExit(f"FAIL: median startup {median:.0f}ms > 500ms over {runs} runs")
PY

NIGHTJAR_DATA_DIR="$DATA" NIGHTJAR_PORT="$PORT" "$BIN" >"$LOG" 2>&1 &
PID=$!
for _ in $(seq 1 200); do
  curl -sf "http://127.0.0.1:${PORT}/api/health" >/dev/null && break
  sleep 0.05
done

curl -sf -X POST "http://127.0.0.1:${PORT}/api/v0/libraries" \
  -H 'content-type: application/json' \
  -d "{\"name\":\"t\",\"path\":\"${MEDIA}\",\"kind\":\"movies\"}" >/dev/null
LIB=$(curl -sf "http://127.0.0.1:${PORT}/api/v0/libraries" | python3 -c 'import sys,json; print(json.load(sys.stdin)["libraries"][0]["id"])')
JOB=$(curl -sf -X POST "http://127.0.0.1:${PORT}/api/v0/libraries/${LIB}/scan" | python3 -c 'import sys,json; print(json.load(sys.stdin)["jobId"])')
python3 - <<PY
import json, time, urllib.request
port = "${PORT}"
job = "${JOB}"
for _ in range(200):
    with urllib.request.urlopen(f"http://127.0.0.1:{port}/api/v0/scan-jobs/{job}", timeout=5) as r:
        body = json.load(r)
    if body["state"] in ("completed", "failed"):
        if body["state"] != "completed":
            raise SystemExit(f"scan failed: {body}")
        break
    time.sleep(0.05)
else:
    raise SystemExit("scan never completed")
PY
ITEM=$(curl -sf "http://127.0.0.1:${PORT}/api/v0/libraries/${LIB}/items" | python3 -c 'import sys,json; print(json.load(sys.stdin)["items"][0]["id"])')

# Criterion is idle RAM with the library loaded, not cold empty process.
sleep 0.5
RSS_KB=$(ps -o rss= -p "$PID" | tr -d ' ')
RSS_MB=$(( RSS_KB / 1024 ))
echo "idle_rss_mb_with_library=${RSS_MB}"
if [[ "$RSS_MB" -gt 50 ]]; then
  echo "FAIL: idle RSS ${RSS_MB}MB > 50MB with library loaded" >&2
  exit 1
fi

# Range dance: open-ended (Chrome), tiny probe then mid-file seek (Safari-ish).
python3 - <<PY
import urllib.request, time

port = "${PORT}"
item = "${ITEM}"
base = f"http://127.0.0.1:{port}/api/v0/items/{item}/stream"

def get(range_header):
    req = urllib.request.Request(base, headers={"Range": range_header})
    t0 = time.perf_counter()
    with urllib.request.urlopen(req, timeout=3) as r:
        status = r.status
        cr = r.headers.get("Content-Range", "")
        ar = r.headers.get("Accept-Ranges", "")
        chunk = r.read(65536)
        elapsed = time.perf_counter() - t0
    return status, cr, ar, len(chunk), elapsed

status, cr, ar, n, elapsed = get("bytes=0-")
print(f"open_ended_range status={status} accept={ar} content_range={cr} first_chunk={n} elapsed_s={elapsed:.3f}")
if status != 206 or ar != "bytes" or n == 0 or elapsed > 2.0:
    raise SystemExit("open-ended Range failed")

status, cr, ar, n, elapsed = get("bytes=0-1")
print(f"safari_probe status={status} content_range={cr} bytes={n} elapsed_s={elapsed:.3f}")
if status != 206 or not cr.startswith("bytes 0-1/") or n == 0:
    raise SystemExit("Safari-style bytes=0-1 probe failed")
size = int(cr.split("/")[1])
mid = max(size // 2, 2)
end = min(mid + 65535, size - 1)
status, cr, ar, n, elapsed = get(f"bytes={mid}-{end}")
print(f"mid_seek status={status} content_range={cr} first_chunk={n} elapsed_s={elapsed:.3f}")
if status != 206 or n == 0 or elapsed > 2.0:
    raise SystemExit("mid-file seek Range failed")
PY

# Kill -9 after a completed scan; WAL must still serve the library.
kill -9 "$PID"
wait "$PID" 2>/dev/null || true
PID=""

NIGHTJAR_DATA_DIR="$DATA" NIGHTJAR_PORT="$PORT" "$BIN" >"$LOG" 2>&1 &
PID=$!
for _ in $(seq 1 100); do
  if curl -sf "http://127.0.0.1:${PORT}/api/health" >/dev/null; then
    break
  fi
  sleep 0.05
done
COUNT=$(curl -sf "http://127.0.0.1:${PORT}/api/v0/libraries/${LIB}/items" | python3 -c 'import sys,json; print(len(json.load(sys.stdin)["items"]))')
echo "wal_items_after_kill9=${COUNT}"
if [[ "$COUNT" -lt 1 ]]; then
  echo "FAIL: library empty or corrupt after kill -9" >&2
  cat "$LOG" >&2 || true
  exit 1
fi

# Kill -9 mid-scan: start a second library scan and shoot the process while probing.
MEDIA2="$(mktemp -d)"
ffmpeg -y -hide_banner -loglevel error \
  -f lavfi -i "testsrc=size=320x240:rate=24:duration=2" \
  -f lavfi -i "sine=frequency=440:duration=2" \
  -c:v libx264 -pix_fmt yuv420p -c:a aac -ac 2 -shortest \
  "$MEDIA2/during.mp4" >/dev/null
for i in $(seq 1 40); do
  ln "$MEDIA2/during.mp4" "$MEDIA2/during_${i}.mp4" 2>/dev/null || cp "$MEDIA2/during.mp4" "$MEDIA2/during_${i}.mp4"
done
curl -sf -X POST "http://127.0.0.1:${PORT}/api/v0/libraries" \
  -H 'content-type: application/json' \
  -d "{\"name\":\"during\",\"path\":\"${MEDIA2}\",\"kind\":\"movies\"}" >/dev/null
LIB2=$(curl -sf "http://127.0.0.1:${PORT}/api/v0/libraries" | python3 -c 'import sys,json; print([l["id"] for l in json.load(sys.stdin)["libraries"] if l["name"]=="during"][0])')
JOB2=$(curl -sf -X POST "http://127.0.0.1:${PORT}/api/v0/libraries/${LIB2}/scan" | python3 -c 'import sys,json; print(json.load(sys.stdin)["jobId"])')
# Give the index pass a moment to land rows, then kill during probe.
sleep 0.2
kill -9 "$PID"
wait "$PID" 2>/dev/null || true
PID=""
rm -rf "$MEDIA2"

NIGHTJAR_DATA_DIR="$DATA" NIGHTJAR_PORT="$PORT" "$BIN" >"$LOG" 2>&1 &
PID=$!
for _ in $(seq 1 100); do
  if curl -sf "http://127.0.0.1:${PORT}/api/health" >/dev/null; then
    break
  fi
  sleep 0.05
done
COUNT1=$(curl -sf "http://127.0.0.1:${PORT}/api/v0/libraries/${LIB}/items" | python3 -c 'import sys,json; print(len(json.load(sys.stdin)["items"]))')
COUNT2=$(curl -sf "http://127.0.0.1:${PORT}/api/v0/libraries/${LIB2}/items" | python3 -c 'import sys,json; print(len(json.load(sys.stdin)["items"]))')
echo "wal_after_midscan_kill9 lib1=${COUNT1} lib2=${COUNT2} (job ${JOB2} interrupted)"
if [[ "$COUNT1" -lt 1 ]]; then
  echo "FAIL: prior library lost after mid-scan kill -9" >&2
  cat "$LOG" >&2 || true
  exit 1
fi

echo "gate1_smoke=PASS"
