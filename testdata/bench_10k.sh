#!/usr/bin/env bash
# Volume fixture for Gate 1: 10,000 tiny valid MP4s (not hostility — scan throughput).
# Hardlinks to one seed so fixture creation is cheap; the scanner still walks 10k paths.
set -euo pipefail

ROOT="$(cd "$(dirname "$0")" && pwd)"
OUT="${BENCH_DIR:-$ROOT/bench_10k}"
COUNT="${COUNT:-10000}"
FFMPEG="${FFMPEG:-ffmpeg}"
SEED="${SEED_FILE:-$ROOT/.bench_seed.mp4}"

mkdir -p "$OUT"

if [[ ! -f "$SEED" ]]; then
  echo "→ seed MP4 ($SEED)"
  "$FFMPEG" -y -hide_banner -loglevel error \
    -f lavfi -i "testsrc=size=160x120:rate=24:duration=0.5" \
    -f lavfi -i "sine=frequency=440:sample_rate=48000:duration=0.5" \
    -c:v libx264 -pix_fmt yuv420p -c:a aac -ac 1 -shortest \
    "$SEED"
fi

echo "→ linking ${COUNT} files into $OUT"
find "$OUT" -maxdepth 1 -type f -name 'item_*.mp4' -delete 2>/dev/null || true
for i in $(seq -w 1 "$COUNT"); do
  ln "$SEED" "$OUT/item_${i}.mp4" 2>/dev/null || cp "$SEED" "$OUT/item_${i}.mp4"
done

echo "Done. $(find "$OUT" -maxdepth 1 -name 'item_*.mp4' | wc -l | tr -d ' ') files in $OUT"
du -sh "$OUT"