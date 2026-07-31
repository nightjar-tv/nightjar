#!/usr/bin/env bash
# Gate 1: 10k index-pass harness (ADR-0004). Gates on indexDurationMs; reports probe throughput.
set -euo pipefail

ROOT="$(cd "$(dirname "$0")/.." && pwd)"
export PATH="/opt/homebrew/bin:${PATH:-}"

BIN="${NIGHTJAR_BIN:-$ROOT/server/target/release/nightjar}"
PORT="${NIGHTJAR_PORT:-18098}"
BUDGET_S="${SCAN_BUDGET_S:-60}"
PROBE_FLOOR_FPS="${PROBE_FLOOR_FPS:-50}"
DATA="$(mktemp -d)"
LOG="$(mktemp)"
PID=""
BENCH="${BENCH_DIR:-$ROOT/testdata/bench_10k}"

cleanup() {
  if [[ -n "${PID}" ]] && kill -0 "$PID" 2>/dev/null; then
    kill "$PID" 2>/dev/null || true
    wait "$PID" 2>/dev/null || true
  fi
  rm -rf "$DATA" "$LOG"
}
trap cleanup EXIT

if [[ ! -x "$BIN" ]]; then
  echo "missing binary: $BIN" >&2
  exit 1
fi

chmod +x "$ROOT/testdata/bench_10k.sh"
COUNT=10000 "$ROOT/testdata/bench_10k.sh"

NIGHTJAR_DATA_DIR="$DATA" NIGHTJAR_PORT="$PORT" "$BIN" >"$LOG" 2>&1 &
PID=$!
for _ in $(seq 1 200); do
  curl -sf "http://127.0.0.1:${PORT}/api/health" >/dev/null && break
  sleep 0.05
done

curl -sf -X POST "http://127.0.0.1:${PORT}/api/v0/libraries" \
  -H 'content-type: application/json' \
  -d "{\"name\":\"bench10k\",\"path\":\"${BENCH}\",\"kind\":\"movies\"}" >/dev/null
LIB=$(curl -sf "http://127.0.0.1:${PORT}/api/v0/libraries" | python3 -c 'import sys,json; print([l["id"] for l in json.load(sys.stdin)["libraries"] if l["name"]=="bench10k"][0])')

echo "scanning library ${LIB} (index budget ${BUDGET_S}s)…"
python3 - <<PY
import json, time, urllib.request, sys

port = "${PORT}"
lib = "${LIB}"
budget = float("${BUDGET_S}")
probe_floor = float("${PROBE_FLOOR_FPS}")

def get(url):
    with urllib.request.urlopen(url, timeout=30) as r:
        return json.load(r)

req = urllib.request.Request(
    f"http://127.0.0.1:{port}/api/v0/libraries/{lib}/scan",
    method="POST",
    data=b"",
    headers={"Content-Type": "application/json"},
)
with urllib.request.urlopen(req, timeout=30) as r:
    assert r.status == 202, r.status
    accepted = json.load(r)
job_id = accepted["jobId"]
print(f"job_id={job_id}")

# Gate: wait until index pass finishes (indexDurationMs set).
t0 = time.perf_counter()
job = None
while True:
    job = get(f"http://127.0.0.1:{port}/api/v0/scan-jobs/{job_id}")
    if job.get("indexDurationMs") is not None or job["state"] in ("completed", "failed"):
        break
    if time.perf_counter() - t0 > budget + 30:
        raise SystemExit(f"FAIL: timed out waiting for index pass; last={job}")
    time.sleep(0.05)

if job["state"] == "failed":
    raise SystemExit(f"FAIL: scan job failed: {job.get('error')}")

index_ms = job.get("indexDurationMs")
if index_ms is None:
    raise SystemExit(f"FAIL: indexDurationMs missing: {job}")
index_s = index_ms / 1000.0
items = get(f"http://127.0.0.1:{port}/api/v0/libraries/{lib}/items")
n = len(items["items"])
print(json.dumps({
    "index_s": round(index_s, 3),
    "indexDurationMs": index_ms,
    "added": job.get("added"),
    "updated": job.get("updated"),
    "unchanged": job.get("unchanged"),
    "removed": job.get("removed"),
    "items": n,
    "state": job["state"],
}, indent=2))
if n != 10000:
    raise SystemExit(f"FAIL: expected 10000 items after index, got {n}")
if index_s > budget:
    print(f"FAIL {index_s:.1f}s > {budget}s (budget {budget}s)", file=sys.stderr)
    raise SystemExit(1)
print(f"PASS {index_s:.1f}s (budget {budget}s)")

# Wait for probe phase to finish; floored metric (ADR-0004), not the index gate.
while job["state"] not in ("completed", "failed"):
    time.sleep(0.2)
    job = get(f"http://127.0.0.1:{port}/api/v0/scan-jobs/{job_id}")
if job["state"] == "failed":
    raise SystemExit(f"FAIL: probe phase failed: {job.get('error')}")
probe_ms = job.get("probeDurationMs") or 0
probed = job.get("probed") or 0
errors = job.get("errors") or 0
probe_s = max(probe_ms / 1000.0, 0.001)
fps = probed / probe_s
print(f"probe_metric probed={probed} errors={errors} probe_s={probe_s:.1f} files_per_sec={fps:.1f} floor={probe_floor}")
if probed > 0 and fps < probe_floor:
    raise SystemExit(f"FAIL: probe throughput {fps:.1f} files/sec < floor {probe_floor} (ADR-0004)")

# Unchanged rescan: index pass <5s
req2 = urllib.request.Request(
    f"http://127.0.0.1:{port}/api/v0/libraries/{lib}/scan",
    method="POST",
    data=b"",
    headers={"Content-Type": "application/json"},
)
with urllib.request.urlopen(req2, timeout=30) as r:
    job2_id = json.load(r)["jobId"]
t1 = time.perf_counter()
while True:
    job2 = get(f"http://127.0.0.1:{port}/api/v0/scan-jobs/{job2_id}")
    if job2.get("indexDurationMs") is not None or job2["state"] in ("completed", "failed"):
        break
    if time.perf_counter() - t1 > 30:
        raise SystemExit(f"FAIL: rescan index timeout: {job2}")
    time.sleep(0.05)
rescan_s = (job2.get("indexDurationMs") or 0) / 1000.0
print(f"rescan_index_s={rescan_s:.3f} unchanged={job2.get('unchanged')}")
if rescan_s > 5:
    raise SystemExit(f"FAIL: rescan index {rescan_s:.1f}s > 5s")
print("gate1_scan_10k=PASS")
PY
