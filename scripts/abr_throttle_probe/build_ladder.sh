#!/usr/bin/env bash
# Rebuild the three-rung ABR ladder used by Step 3.
set -euo pipefail
ROOT="$(cd "$(dirname "$0")" && pwd)"
SRC="${ABR_SRC:-/Volumes/media/TV Shows/Elementary/Season 3/Elementary - 3x05 - Rip Off - WEBDL-1080p.mkv}"
SS="${ABR_SS:-600}"
DUR="${ABR_DUR:-60}"
rm -rf "$ROOT/static/hi" "$ROOT/static/mid" "$ROOT/static/lo"
mkdir -p "$ROOT/static/hi" "$ROOT/static/mid" "$ROOT/static/lo"
for spec in "hi:4000k:1280:720" "mid:1500k:854:480" "lo:600k:640:360"; do
  name=${spec%%:*}; rest=${spec#*:}; br=${rest%%:*}; rest=${rest#*:}; w=${rest%%:*}; h=${rest#*:}
  echo "encode $name $br ${w}x${h}"
  ffmpeg -nostdin -hide_banner -loglevel error -y -ss "$SS" -t "$DUR" -i "$SRC" \
    -map '0:v:0' -map '0:a:0?' \
    -c:v libx264 -preset veryfast -b:v "$br" -maxrate "$br" -bufsize "$((${br%k}*2))k" \
    -vf "scale=${w}:${h}" -pix_fmt yuv420p \
    -force_key_frames 'expr:gte(t,n_forced*2)' -g 600 -keyint_min 48 -sc_threshold 0 \
    -c:a aac -b:a 128k -ac 2 \
    -f hls -hls_time 2 -hls_list_size 0 -hls_flags independent_segments+temp_file \
    -hls_segment_type fmp4 -hls_fmp4_init_filename init.mp4 \
    -hls_segment_filename "$ROOT/static/$name/seg%d.m4s" \
    "$ROOT/static/$name/index.m3u8"
done
cat > "$ROOT/static/master.m3u8" <<'EOF'
#EXTM3U
#EXT-X-VERSION:7
#EXT-X-STREAM-INF:BANDWIDTH=4500000,RESOLUTION=1280x720,CODECS="avc1.64001f,mp4a.40.2"
hi/index.m3u8
#EXT-X-STREAM-INF:BANDWIDTH=1700000,RESOLUTION=854x480,CODECS="avc1.4d401e,mp4a.40.2"
mid/index.m3u8
#EXT-X-STREAM-INF:BANDWIDTH=750000,RESOLUTION=640x360,CODECS="avc1.42e01e,mp4a.40.2"
lo/index.m3u8
EOF
cp "$ROOT/static_page/page.html" "$ROOT/static/page.html"
mkdir -p "$ROOT/static/vendor"
cp "$(cd "$ROOT/../.." && pwd)/web/node_modules/hls.js/dist/hls.mjs" "$ROOT/static/vendor/hls.mjs"
echo "done: $ROOT/static/master.m3u8"
