#!/usr/bin/env bash
# PERF-1A: authenticated large-library baseline protocol.
#
# Builds the release binary, generates a deterministic 25k and 100k shows
# library, starts the real server against each, and drives the show-browse and
# rail routes with scripts/perf_http_driver.py. The harness self-test runs
# first and fails the protocol if it cannot detect deliberate slowness.
#
# Credentials live in a mode-0600 file and never on argv or in evidence. The
# run has a finite time box, a free-disk reserve, and a cleanup path that
# removes the disposable library and data directory on exit.
set -euo pipefail

ROOT="$(cd "$(dirname "$0")/.." && pwd)"
export PATH="/opt/homebrew/bin:${PATH:-}"

BIN="${NIGHTJAR_BIN:-$ROOT/server/target/release/nightjar}"
SCALES="${PERF_SCALES:-25000 100000}"
REQUESTS="${PERF_REQUESTS:-100}"
WARM="${PERF_WARM:-10}"
SEED="${PERF_SEED:-1}"
TIME_BOX_S="${PERF_TIME_BOX_S:-3600}"
MIN_FREE_MB="${PERF_MIN_FREE_MB:-3000}"
PORT_BASE="${PERF_PORT_BASE:-18110}"
# Evidence is worktree-local and disposable. Clean only this directory so a
# rerun never mixes runs, and never touch anything outside the worktree.
OUT_DIR="${PERF_OUT_DIR:-$ROOT/.perf-1a-run}"
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

# The timing instrument is proven before it is trusted (Rule 4.15).
python3 "$ROOT/scripts/perf_http_driver.py" selftest

free_mb() {
  df -m "$ROOT" | awk 'NR==2 {print $4}'
}

cleanup_scale() {
  if [[ -n "${PID:-}" ]] && kill -0 "$PID" 2>/dev/null; then
    kill "$PID" 2>/dev/null || true
    wait "$PID" 2>/dev/null || true
  fi
  PID=""
  [[ -n "${DATA:-}" ]] && rm -rf "$DATA"
  [[ -n "${MEDIA:-}" ]] && rm -rf "$MEDIA"
  [[ -n "${PASSWORD_FILE:-}" ]] && rm -f "$PASSWORD_FILE"
  [[ -n "${TOKEN_FILE:-}" ]] && rm -f "$TOKEN_FILE"
}

trap 'cleanup_scale || true' EXIT
trap 'trap - EXIT INT TERM; cleanup_scale || true; exit 130' INT
trap 'trap - EXIT INT TERM; cleanup_scale || true; exit 143' TERM

SCALE_RESULTS=()
for SCALE in $SCALES; do
  NOW="$(date +%s)"
  if (( NOW - STARTED_AT > TIME_BOX_S )); then
    echo "FAIL: time box ${TIME_BOX_S}s exceeded before scale ${SCALE}" >&2
    exit 1
  fi
  if (( $(free_mb) < MIN_FREE_MB )); then
    echo "FAIL: free disk below ${MIN_FREE_MB}MB before scale ${SCALE}" >&2
    exit 1
  fi

  DATA="$(mktemp -d "${TMPDIR:-/tmp}/nightjar-perf-data.XXXXXX")"
  MEDIA="$(mktemp -d "${TMPDIR:-/tmp}/nightjar-perf-media.XXXXXX")"
  rmdir "$MEDIA"
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

  PORT=$((PORT_BASE + SCALE / 1000))
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

  RESULT="$OUT_DIR/result-${SCALE}.json"
  python3 "$ROOT/scripts/perf_http_driver.py" run \
    --base-url "http://127.0.0.1:${PORT}" \
    --token-file "$TOKEN_FILE" \
    --state "$STATE_FILE" \
    --scale "$SCALE" \
    --requests "$REQUESTS" --warm "$WARM" \
    --server-pid "$PID" \
    --root "$ROOT" \
    --manifest "$MANIFEST" \
    --seed "$SEED" \
    --out "$RESULT"

  SCALE_RESULTS+=("$RESULT")
  cleanup_scale
done

python3 - "$OUT_DIR" "${SCALE_RESULTS[@]}" <<'PY'
import json, sys
from pathlib import Path

out_dir = Path(sys.argv[1])
results = [json.loads(Path(path).read_text()) for path in sys.argv[2:]]
summary = {
    "out_dir": str(out_dir),
    "scales": [
        {
            "scale": result["scale"],
            "units": {k: result["endpoints"]["units"][k]
                      for k in ("p50_ms", "p95_ms", "max_ms", "bytes_p50",
                                "bytes_max", "errors", "correctness_errors",
                                "observed")},
            "rail": {k: result["endpoints"]["rail"][k]
                     for k in ("p50_ms", "p95_ms", "max_ms", "bytes_p50",
                               "bytes_max", "errors", "correctness_errors",
                               "observed")},
            "server": result["server"]["after"],
            "library": result["library"],
            "provenance": result["provenance"],
        }
        for result in results
    ],
}
(out_dir / "summary.json").write_text(json.dumps(summary, indent=2, sort_keys=True) + "\n")
print(json.dumps(summary, indent=2, sort_keys=True))
PY

echo "PERF-1A baseline complete: $OUT_DIR"
