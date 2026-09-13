#!/usr/bin/env bash
# PERF-1C: release-acceptance qualification protocol.
#
# Generates one deterministic 100k disposable library, starts the real
# authenticated release server against it, and measures each endpoint cell for
# the requested reader counts. A fail-closed summarizer then requires the full
# matrix and applies the proposed warm targets to the contracts they fit.
#
# Credentials live in a mode-0600 file and never appear on argv or in evidence.
# Generated data, the database, and results stay under a worktree-local,
# gitignored directory that is removed on exit. The run has a finite time box
# and a free-disk reserve; either one stops it.
set -euo pipefail

ROOT="$(cd "$(dirname "$0")/.." && pwd)"
export PATH="/opt/homebrew/bin:${PATH:-}"

BIN="${NIGHTJAR_BIN:-$ROOT/server/target/release/nightjar}"
SCALE="${PERF_SCALE:-100000}"
READERS="${PERF_READERS:-1 4}"
ENDPOINTS="${PERF_ENDPOINTS:-units,detail,rail}"
REQUESTS="${PERF_REQUESTS:-1000}"
WARM="${PERF_WARM:-10}"
SEED="${PERF_SEED:-1}"
TIME_BOX_S="${PERF_TIME_BOX_S:-10800}"
MIN_FREE_MB="${PERF_MIN_FREE_MB:-3000}"
PORT="${PERF_PORT:-18130}"
OUT_DIR="${PERF_OUT_DIR:-$ROOT/.perf-1c-run}"
rm -rf "$OUT_DIR"
mkdir -p "$OUT_DIR"
STARTED_AT="$(date +%s)"

if [[ "${PERF_SKIP_BUILD:-0}" != "1" ]]; then
  cargo build --release --manifest-path "$ROOT/server/Cargo.toml"
fi

if [[ ! -x "$BIN" ]]; then
  echo "missing binary: $BIN (build release first)" >&2
  exit 1
fi

# Both instruments are proven before they are trusted (Rule 4.15).
python3 "$ROOT/scripts/perf_http_driver.py" selftest
python3 "$ROOT/scripts/perf_release_summary.py" selftest

free_mb() {
  df -m "$ROOT" | awk 'NR==2 {print $4}'
}

DATA=""
MEDIA=""
PID=""
PASSWORD_FILE=""
TOKEN_FILE=""

cleanup() {
  if [[ -n "$PID" ]] && kill -0 "$PID" 2>/dev/null; then
    kill "$PID" 2>/dev/null || true
    wait "$PID" 2>/dev/null || true
  fi
  PID=""
  [[ -n "$DATA" ]] && rm -rf "$DATA"
  [[ -n "$MEDIA" ]] && rm -rf "$MEDIA"
  [[ -n "$PASSWORD_FILE" ]] && rm -f "$PASSWORD_FILE"
  [[ -n "$TOKEN_FILE" ]] && rm -f "$TOKEN_FILE"
}
trap 'cleanup || true' EXIT
trap 'trap - EXIT INT TERM; cleanup || true; exit 130' INT
trap 'trap - EXIT INT TERM; cleanup || true; exit 143' TERM

DATA="$(mktemp -d "${TMPDIR:-/tmp}/nightjar-perf-data.XXXXXX")"
MEDIA="$(mktemp -d "${TMPDIR:-/tmp}/nightjar-perf-media.XXXXXX")"
rmdir "$MEDIA"

{
  echo "hostname: $(hostname)"
  echo "hw_model: $(sysctl -n hw.model 2>/dev/null || echo unknown)"
  echo "cpu: $(sysctl -n machdep.cpu.brand_string 2>/dev/null || echo unknown)"
  echo "ncpu: $(sysctl -n hw.ncpu 2>/dev/null || echo unknown)"
  echo "memsize_bytes: $(sysctl -n hw.memsize 2>/dev/null || echo unknown)"
  echo "storage_device: $(df -m "$ROOT" | awk 'NR==2 {print $1}')"
  echo "free_mb_start: $(free_mb)"
  echo "time_box_s: $TIME_BOX_S"
  echo "min_free_mb: $MIN_FREE_MB"
  echo "scale: $SCALE"
  echo "readers: $READERS"
  echo "endpoints: $ENDPOINTS"
  echo "requests_per_endpoint: $REQUESTS"
  echo "cleanup: data and media are removed on exit"
} >"$OUT_DIR/host.txt"

SEED_FILE="$DATA/perf_seed.mp4"
ffmpeg -y -hide_banner -loglevel error \
  -f lavfi -i "testsrc=size=160x120:rate=24:duration=0.5" \
  -f lavfi -i "sine=frequency=440:sample_rate=48000:duration=0.5" \
  -c:v libx264 -pix_fmt yuv420p -c:a aac -ac 1 -shortest "$SEED_FILE"

MANIFEST="$OUT_DIR/manifest-${SCALE}.json"
python3 "$ROOT/scripts/perf_large_library.py" generate \
  --items "$SCALE" --data-dir "$DATA" --media-root "$MEDIA" \
  --seed-file "$SEED_FILE" --seed "$SEED" --nightjar-bin "$BIN" \
  --manifest-out "$MANIFEST"

NIGHTJAR_DATA_DIR="$DATA" NIGHTJAR_PORT="$PORT" "$BIN" \
  >"$DATA/server.log" 2>&1 &
PID=$!
for _ in $(seq 1 400); do
  if curl -sf "http://127.0.0.1:${PORT}/api/health" >/dev/null; then
    break
  fi
  sleep 0.05
done

PASSWORD_FILE="$(mktemp)"
printf '%s' "$(python3 -c 'import secrets; print(secrets.token_hex(16))')" >"$PASSWORD_FILE"
chmod 600 "$PASSWORD_FILE"
TOKEN_FILE="$(mktemp)"
STATE_FILE="$OUT_DIR/state-${SCALE}.json"
python3 "$ROOT/scripts/perf_http_driver.py" prepare \
  --base-url "http://127.0.0.1:${PORT}" \
  --password-file "$PASSWORD_FILE" \
  --token-out "$TOKEN_FILE" \
  --state-out "$STATE_FILE"

CAPPED="$(python3 -c 'import json,sys; print(json.load(open(sys.argv[1]))["capped_profile_ref"])' "$STATE_FILE")"
CAP="$(python3 -c 'import json,sys; print(json.load(open(sys.argv[1])).get("capped_profile_cap", "teen"))' "$STATE_FILE")"
REGION="$(python3 -c 'import json,sys; print(json.load(open(sys.argv[1])).get("classification_region", "US"))' "$STATE_FILE")"
python3 "$ROOT/scripts/perf_large_library.py" seed-history \
  --data-dir "$DATA" --profile-ref "$CAPPED" --cap "$CAP" --region "$REGION"
python3 "$ROOT/scripts/perf_large_library.py" verify-history \
  --data-dir "$DATA" --profile-ref "$CAPPED" --cap "$CAP" --region "$REGION" \
  --manifest-out "$MANIFEST"

RESULT_FILES=()
for R in $READERS; do
  NOW="$(date +%s)"
  if (( NOW - STARTED_AT > TIME_BOX_S )); then
    echo "FAIL: time box ${TIME_BOX_S}s exceeded before cell ${SCALE}/${R}r" >&2
    exit 1
  fi
  if (( $(free_mb) < MIN_FREE_MB )); then
    echo "FAIL: free disk below ${MIN_FREE_MB}MB before cell ${SCALE}/${R}r" >&2
    exit 1
  fi

  RESULT="$OUT_DIR/cell-${SCALE}-${R}r.json"
  # The driver exits nonzero when a cell records an error. Do not abort here:
  # let the summarizer fail closed on the written result so the evidence is
  # complete and the failure is attributable.
  if ! python3 "$ROOT/scripts/perf_http_driver.py" run \
    --base-url "http://127.0.0.1:${PORT}" \
    --token-file "$TOKEN_FILE" \
    --state "$STATE_FILE" \
    --scale "$SCALE" \
    --requests "$REQUESTS" --warm "$WARM" \
    --readers "$R" --endpoints "$ENDPOINTS" \
    --server-pid "$PID" \
    --db-path "$DATA/nightjar.db" \
    --root "$ROOT" \
    --manifest "$MANIFEST" \
    --seed "$SEED" \
    --out "$RESULT"; then
    echo "FAIL: driver reported an error in cell ${SCALE}/${R}r" >&2
  fi
  RESULT_FILES+=("$RESULT")
done

python3 "$ROOT/scripts/perf_release_summary.py" summarize \
  --results "${RESULT_FILES[@]}" \
  --endpoints "$ENDPOINTS" \
  --manifest "$MANIFEST" \
  --out "$OUT_DIR/summary.json"

echo "PERF-1C release acceptance complete: $OUT_DIR"
