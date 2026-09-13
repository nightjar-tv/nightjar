#!/usr/bin/env bash
# Gate 1: 10k index-pass harness (ADR-0004). Gates on indexDurationMs and on the probe phase having measured something cleanly.
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
AUTH=""
BODY=""
BENCH="${BENCH_DIR:-$ROOT/testdata/bench_10k}"

cleanup() {
  if [[ -n "${PID}" ]] && kill -0 "$PID" 2>/dev/null; then
    kill "$PID" 2>/dev/null || true
    wait "$PID" 2>/dev/null || true
  fi
  rm -rf "$DATA" "$LOG"
  [[ -n "${AUTH}" ]] && rm -f "$AUTH"
  [[ -n "${BODY}" ]] && rm -f "$BODY"
}
trap cleanup EXIT
# A signal must exit through cleanup once and must not continue the script.
# Drop the traps first so cleanup does not run a second time on exit.
trap 'trap - EXIT INT TERM; cleanup || true; exit 130' INT
trap 'trap - EXIT INT TERM; cleanup || true; exit 143' TERM

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

# ADR-0034 items 9 and 10. The harness owns one disposable account, created
# through the shipped bootstrap route after the startup timing above so no
# measurement includes Argon2 work. The token is captured into a shell
# variable and a 0600 curl config file and is never printed; the data
# directory cleanup removes is the only durable copy of the credential.
PASS="$(python3 -c 'import secrets; print(secrets.token_hex(16))')"
# The disposable password goes into a 0600 mktemp payload, not curl argv, so it
# never shows in ps. mktemp creates the file mode 0600.
BODY="$(mktemp)"
printf '{"username":"gate1_harness","password":"%s","clientLabel":"gate1 harness"}' "$PASS" > "$BODY"
TOKEN="$(curl -sf -X POST "http://127.0.0.1:${PORT}/api/v0/auth/bootstrap" \
  -H 'content-type: application/json' \
  --data @"$BODY" \
  | python3 -c 'import sys,json; print(json.load(sys.stdin)["token"])')"
AUTH="$(mktemp)"
printf 'header = "authorization: Bearer %s"\n' "$TOKEN" > "$AUTH"

curl -sf --config "$AUTH" -X POST "http://127.0.0.1:${PORT}/api/v0/libraries" \
  -H 'content-type: application/json' \
  -d "{\"name\":\"bench10k\",\"path\":\"${BENCH}\",\"kind\":\"movies\"}" >/dev/null
LIB=$(curl -sf --config "$AUTH" "http://127.0.0.1:${PORT}/api/v0/libraries" | python3 -c 'import sys,json; print([l["id"] for l in json.load(sys.stdin)["libraries"] if l["name"]=="bench10k"][0])')

echo "scanning library ${LIB} (index budget ${BUDGET_S}s)…"
python3 - <<PY
import json, sys, time, urllib.request

sys.path.insert(0, "${ROOT}/scripts")
import gate1_probe_gate

port = "${PORT}"
lib = "${LIB}"
budget = float("${BUDGET_S}")
probe_floor = float("${PROBE_FLOOR_FPS}")
token = "${TOKEN}"

def auth_headers(extra=None):
    headers = {"Authorization": f"Bearer {token}"}
    if extra:
        headers.update(extra)
    return headers

def get(url):
    req = urllib.request.Request(url, headers=auth_headers())
    with urllib.request.urlopen(req, timeout=30) as r:
        return json.load(r)

def post(url):
    req = urllib.request.Request(
        url,
        method="POST",
        data=b"",
        headers=auth_headers({"Content-Type": "application/json"}),
    )
    return urllib.request.urlopen(req, timeout=30)

with post(f"http://127.0.0.1:{port}/api/v0/libraries/{lib}/scan") as r:
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
report, fail_reason = gate1_probe_gate.probe_report(job, probe_floor)
print(report)
if fail_reason:
    raise SystemExit(fail_reason)

# Unchanged rescan: index pass <5s
with post(f"http://127.0.0.1:{port}/api/v0/libraries/{lib}/scan") as r:
    job2_id = json.load(r)["jobId"]
t1 = time.perf_counter()
while True:
    job2 = get(f"http://127.0.0.1:{port}/api/v0/scan-jobs/{job2_id}")
    if job2.get("indexDurationMs") is not None or job2["state"] in ("completed", "failed"):
        break
    if time.perf_counter() - t1 > 30:
        raise SystemExit(f"FAIL: rescan index timeout: {job2}")
    time.sleep(0.05)
if job2["state"] == "failed":
    raise SystemExit(f"FAIL: rescan job failed: {job2.get('error')}")
rescan_ms = job2.get("indexDurationMs")
if rescan_ms is None:
    raise SystemExit(f"FAIL: rescan indexDurationMs missing: {job2}")
rescan_s = rescan_ms / 1000.0
print(f"rescan_index_s={rescan_s:.3f} unchanged={job2.get('unchanged')}")
if rescan_s > 5:
    raise SystemExit(f"FAIL: rescan index {rescan_s:.1f}s > 5s")
print("gate1_scan_10k=PASS")
PY
