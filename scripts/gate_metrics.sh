#!/usr/bin/env bash
# Emit Gate / CI benchmark numbers as JSON on stdout (V1_PLAN Phase 2 item).
# Reporting only — hard floors stay in gate1_ci.sh / gate1_scan_10k.sh.
set -euo pipefail

ROOT="$(cd "$(dirname "$0")/.." && pwd)"
BIN="${NIGHTJAR_BIN:-$ROOT/server/target/release/nightjar}"
PORT="${NIGHTJAR_PORT:-18111}"
DATA="$(mktemp -d)"
LOG="$(mktemp)"
SCAN_OUT="$(mktemp)"

cleanup() { rm -rf "$DATA" "$LOG" "$SCAN_OUT"; }
trap cleanup EXIT

if [[ ! -x "$BIN" ]]; then
  echo "missing binary: $BIN" >&2
  exit 1
fi

export NIGHTJAR_DATA_DIR="$DATA" NIGHTJAR_PORT="$PORT" NIGHTJAR_SKIP_STALE_CHECK=1

STARTUP_MS="$(
  BIN_PATH="$BIN" PORT="$PORT" LOG_PATH="$LOG" python3 - <<'PY'
import os, statistics, subprocess, time, urllib.request
bin_path = os.environ["BIN_PATH"]
port = os.environ["PORT"]
samples = []
for _ in range(3):
    log = open(os.environ["LOG_PATH"], "w")
    env = {**os.environ}
    t0 = time.perf_counter()
    p = subprocess.Popen([bin_path], stdout=log, stderr=subprocess.STDOUT, env=env)
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
        raise SystemExit("health never ready")
print(int(statistics.median(samples)))
PY
)"

# Idle RSS: one settled process.
: >"$LOG"
NIGHTJAR_DATA_DIR="$DATA" NIGHTJAR_PORT="$PORT" "$BIN" >>"$LOG" 2>&1 &
PID=$!
for _ in $(seq 1 200); do
  curl -sf "http://127.0.0.1:$PORT/api/health" >/dev/null && break
  sleep 0.05
done
sleep 2
if [[ "$(uname -s)" == "Linux" ]]; then
  RSS_KB="$(awk '/VmRSS:/ {print $2}' "/proc/$PID/status")"
else
  RSS_KB="$(ps -o rss= -p "$PID" | tr -d ' ')"
fi
kill "$PID" 2>/dev/null || true
wait "$PID" 2>/dev/null || true
PID=""

# Scan harness owns its own process + port.
SCAN_PORT=$((PORT + 1))
set +e
SCAN_BUDGET_S="${SCAN_BUDGET_S:-120}" NIGHTJAR_BIN="$BIN" NIGHTJAR_PORT="$SCAN_PORT" \
  NIGHTJAR_SKIP_STALE_CHECK=1 \
  "$ROOT/scripts/gate1_scan_10k.sh" >"$SCAN_OUT" 2>&1
SCAN_RC=$?
set -e

python3 - <<PY
import json, pathlib, re
startup = int("$STARTUP_MS")
rss_kb = int("$RSS_KB")
size = pathlib.Path("$BIN").stat().st_size
blob = pathlib.Path("$SCAN_OUT").read_text(errors="replace")

index_s = None
rescan_s = None
probe_fps = None
m = re.search(r'"index_s":\s*([0-9.]+)', blob)
if m:
    index_s = float(m.group(1))
m = re.search(r"rescan_index_s=([0-9.]+)", blob)
if m:
    rescan_s = float(m.group(1))
m = re.search(r"files_per_sec=([0-9.]+)", blob)
if m:
    probe_fps = float(m.group(1))

print(json.dumps({
  "startupMedianMs": startup,
  "idleRssKb": rss_kb,
  "releaseBinaryBytes": size,
  "index10kSeconds": index_s,
  "rescanSeconds": rescan_s,
  "probeFilesPerSec": probe_fps,
  "scanHarnessExit": $SCAN_RC,
}, indent=2))
PY
