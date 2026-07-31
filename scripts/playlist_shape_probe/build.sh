#!/usr/bin/env bash
# Regenerate work windows + static time-keyed media/playlists.
set -euo pipefail
ROOT="$(cd "$(dirname "$0")" && pwd)"
export PROBE_ROOT="$ROOT"
SRC="${PROBE_SRC:-/Volumes/media/TV Shows/Rick and Morty/Season 9/Rick and Morty - 9x04 - A Ricker Runs Through It - WEBDL-1080p.mkv}"
rm -rf "$ROOT/work"
mkdir -p "$ROOT/work/run_a" "$ROOT/work/run_b"

gen_run() {
  local dir="$1" ss="$2" t="$3" start_number="$4"
  ffmpeg -nostdin -hide_banner -loglevel error -y \
    -ss "$ss" -i "$SRC" -output_ts_offset "$ss" \
    -map 0:v:0 -map '0:a:0?' -c copy -t "$t" \
    -f hls -hls_time 2 -hls_list_size 0 \
    -hls_flags independent_segments+temp_file \
    -hls_segment_type fmp4 -hls_fmp4_init_filename init.mp4 \
    -hls_segment_filename "$dir/seg%03d.m4s" \
    -start_number "$start_number" \
    "$dir/index.m3u8"
}

gen_run "$ROOT/work/run_a" 10.000 40 5
gen_run "$ROOT/work/run_b" 600.000 40 300
python3 "$ROOT/build_static.py"
echo "static ready under $ROOT/static"
