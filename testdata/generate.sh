#!/usr/bin/env bash
# Generate synthetic, legally redistributable corpus files (Rule 4.3).
# Requires ffmpeg on PATH. Color bars + sine tone only. Never commercial media.
set -euo pipefail

ROOT="$(cd "$(dirname "$0")" && pwd)"
OUT="$ROOT/files"
mkdir -p "$OUT"
FFMPEG="${FFMPEG:-ffmpeg}"

gen() {
  local name="$1"
  shift
  echo "→ $name"
  "$FFMPEG" -y -hide_banner -loglevel error "$@" "$OUT/$name"
}

# 1. Phase 1 browser baseline
gen h264_aac_mp4.mp4 \
  -f lavfi -i "testsrc=size=1280x720:rate=24:duration=2" \
  -f lavfi -i "sine=frequency=440:sample_rate=48000:duration=2" \
  -c:v libx264 -pix_fmt yuv420p -profile:v high -c:a aac -ac 2 -shortest

# 2. H.264 + AC3 (browser-unplayable audio)
gen h264_ac3_mp4.mp4 \
  -f lavfi -i "testsrc=size=1280x720:rate=24:duration=2" \
  -f lavfi -i "sine=frequency=440:sample_rate=48000:duration=2" \
  -c:v libx264 -pix_fmt yuv420p -c:a ac3 -ac 2 -shortest

# 3. H.264 + AAC in MKV
gen h264_aac_mkv.mkv \
  -f lavfi -i "testsrc=size=1280x720:rate=24:duration=2" \
  -f lavfi -i "sine=frequency=440:sample_rate=48000:duration=2" \
  -c:v libx264 -pix_fmt yuv420p -c:a aac -ac 2 -shortest

# 4. HEVC 8-bit
gen hevc_aac_mp4.mp4 \
  -f lavfi -i "testsrc=size=1280x720:rate=24:duration=2" \
  -f lavfi -i "sine=frequency=440:sample_rate=48000:duration=2" \
  -c:v libx265 -pix_fmt yuv420p -tag:v hvc1 -c:a aac -ac 2 -shortest

# 5. HEVC 10-bit
gen hevc10_aac_mp4.mp4 \
  -f lavfi -i "testsrc=size=1280x720:rate=24:duration=2" \
  -f lavfi -i "sine=frequency=440:sample_rate=48000:duration=2" \
  -c:v libx265 -pix_fmt yuv420p10le -tag:v hvc1 -c:a aac -ac 2 -shortest

# 6. AV1
gen av1_aac_mp4.mp4 \
  -f lavfi -i "testsrc=size=640x360:rate=24:duration=1" \
  -f lavfi -i "sine=frequency=440:sample_rate=48000:duration=1" \
  -c:v libsvtav1 -c:a aac -ac 2 -shortest

# 7. Interlaced MPEG-2
gen mpeg2_interlaced.ts \
  -f lavfi -i "testsrc=size=720x480:rate=30:duration=2" \
  -f lavfi -i "sine=frequency=440:sample_rate=48000:duration=2" \
  -vf "tinterlace=interleave_top,fieldorder=tff" \
  -c:v mpeg2video -flags +ildct+ilme -c:a mp2 -shortest

# 8. ASS/SSA subs in MKV
ASS="$OUT/_tmp.ass"
cat > "$ASS" <<'EOF'
[Script Info]
ScriptType: v4.00+
PlayResX: 1280
PlayResY: 720

[V4+ Styles]
Format: Name, Fontname, Fontsize, PrimaryColour, SecondaryColour, OutlineColour, BackColour, Bold, Italic, Underline, StrikeOut, ScaleX, ScaleY, Spacing, Angle, BorderStyle, Outline, Shadow, Alignment, MarginL, MarginR, MarginV, Encoding
Style: Default,Arial,48,&H00FFFFFF,&H000000FF,&H00000000,&H00000000,0,0,0,0,100,100,0,0,1,2,0,2,10,10,20,1

[Events]
Format: Layer, Start, End, Style, Name, MarginL, MarginR, MarginV, Effect, Text
Dialogue: 0,0:00:00.00,0:00:02.00,Default,,0,0,0,,Nightjar ASS sample
EOF
gen h264_aac_ass_mkv.mkv \
  -f lavfi -i "testsrc=size=1280x720:rate=24:duration=2" \
  -f lavfi -i "sine=frequency=440:sample_rate=48000:duration=2" \
  -i "$ASS" \
  -c:v libx264 -pix_fmt yuv420p -c:a aac -ac 2 -c:s ass -shortest
rm -f "$ASS"

# 9. Soft SRT in MKV
SRT="$OUT/_tmp.srt"
cat > "$SRT" <<'EOF'
1
00:00:00,000 --> 00:00:02,000
Nightjar SRT sample
EOF
gen h264_aac_srt_mkv.mkv \
  -f lavfi -i "testsrc=size=1280x720:rate=24:duration=2" \
  -f lavfi -i "sine=frequency=440:sample_rate=48000:duration=2" \
  -i "$SRT" \
  -c:v libx264 -pix_fmt yuv420p -c:a aac -ac 2 -c:s srt -shortest
rm -f "$SRT"

# 9b. Sidecar .en.srt beside a video (ADR-0010)
SIDE_DIR="$OUT/sidecar_beside"
mkdir -p "$SIDE_DIR"
echo "→ sidecar_beside/Movie.mp4 + Movie.en.srt"
"$FFMPEG" -y -hide_banner -loglevel error \
  -f lavfi -i "testsrc=size=640x360:rate=24:duration=2" \
  -f lavfi -i "sine=frequency=440:sample_rate=48000:duration=2" \
  -c:v libx264 -pix_fmt yuv420p -c:a aac -ac 2 -shortest \
  "$SIDE_DIR/Movie.mp4"
cat > "$SIDE_DIR/Movie.en.srt" <<'EOF'
1
00:00:00,000 --> 00:00:02,000
Nightjar sidecar SRT
EOF

# 9c. Sidecar under Subs/ sibling directory
SUBS_FIX="$OUT/sidecar_subs_dir"
mkdir -p "$SUBS_FIX/Subs"
echo "→ sidecar_subs_dir/Show.mp4 + Subs/Show.en.srt"
"$FFMPEG" -y -hide_banner -loglevel error \
  -f lavfi -i "testsrc=size=640x360:rate=24:duration=2" \
  -f lavfi -i "sine=frequency=440:sample_rate=48000:duration=2" \
  -c:v libx264 -pix_fmt yuv420p -c:a aac -ac 2 -shortest \
  "$SUBS_FIX/Show.mp4"
cat > "$SUBS_FIX/Subs/Show.en.srt" <<'EOF'
1
00:00:00,000 --> 00:00:02,000
Nightjar Subs-dir SRT
EOF

# 10. 7.1 AAC channel layout
gen h264_aac_71_mp4.mp4 \
  -f lavfi -i "testsrc=size=1280x720:rate=24:duration=2" \
  -f lavfi -i "sine=frequency=440:sample_rate=48000:duration=2" \
  -c:v libx264 -pix_fmt yuv420p -c:a aac -ac 8 -shortest

# 11. E-AC3 audio
gen h264_eac3_mp4.mp4 \
  -f lavfi -i "testsrc=size=1280x720:rate=24:duration=2" \
  -f lavfi -i "sine=frequency=440:sample_rate=48000:duration=2" \
  -c:v libx264 -pix_fmt yuv420p -c:a eac3 -ac 2 -shortest

# 12. VFR (variable frame rate via minterpolate-ish: setpts jitter)
gen h264_aac_vfr_mp4.mp4 \
  -f lavfi -i "testsrc=size=640x360:rate=30:duration=3" \
  -f lavfi -i "sine=frequency=440:sample_rate=48000:duration=3" \
  -vf "setpts=N/(30*TB)+0.01*sin(N/5)" \
  -vsync vfr -c:v libx264 -pix_fmt yuv420p -c:a aac -ac 2 -shortest

# 13. Broken moov: truncate a valid mp4 so the moov is incomplete
gen _good_for_break.mp4 \
  -f lavfi -i "testsrc=size=640x360:rate=24:duration=2" \
  -f lavfi -i "sine=frequency=440:sample_rate=48000:duration=2" \
  -c:v libx264 -pix_fmt yuv420p -c:a aac -ac 2 -shortest
# Default mp4 muxing writes moov at the end; keep only the first 60% so the
# moov is definitely gone and ffprobe reports a structured error.
head -c $(( $(wc -c < "$OUT/_good_for_break.mp4") * 60 / 100 )) \
  "$OUT/_good_for_break.mp4" > "$OUT/broken_moov.mp4"
rm -f "$OUT/_good_for_break.mp4"
echo "→ broken_moov.mp4"

# 14. VP9 + Opus WebM
gen vp9_opus_webm.webm \
  -f lavfi -i "testsrc=size=640x360:rate=24:duration=1" \
  -f lavfi -i "sine=frequency=440:sample_rate=48000:duration=1" \
  -c:v libvpx-vp9 -c:a libopus -ac 2 -shortest

# 15. QuickTime MOV H.264 + AAC
gen h264_aac_mov.mov \
  -f lavfi -i "testsrc=size=1280x720:rate=24:duration=2" \
  -f lavfi -i "sine=frequency=440:sample_rate=48000:duration=2" \
  -c:v libx264 -pix_fmt yuv420p -c:a aac -ac 2 -shortest

# 16. MP3 audio in MP4 (not on Phase 1 AAC whitelist)
gen h264_mp3_mp4.mp4 \
  -f lavfi -i "testsrc=size=1280x720:rate=24:duration=2" \
  -f lavfi -i "sine=frequency=440:sample_rate=48000:duration=2" \
  -c:v libx264 -pix_fmt yuv420p -c:a libmp3lame -ac 2 -shortest

# 17. Odd sample rate
gen h264_aac_32k_mp4.mp4 \
  -f lavfi -i "testsrc=size=640x360:rate=24:duration=2" \
  -f lavfi -i "sine=frequency=440:sample_rate=32000:duration=2" \
  -c:v libx264 -pix_fmt yuv420p -c:a aac -ar 32000 -ac 1 -shortest

# 18. Mono AAC baseline variant
gen h264_aac_mono_mp4.mp4 \
  -f lavfi -i "testsrc=size=640x360:rate=24:duration=2" \
  -f lavfi -i "sine=frequency=440:sample_rate=48000:duration=2" \
  -c:v libx264 -pix_fmt yuv420p -c:a aac -ac 1 -shortest

# 19. AVI MPEG-4
gen mpeg4_mp2_avi.avi \
  -f lavfi -i "testsrc=size=640x360:rate=24:duration=2" \
  -f lavfi -i "sine=frequency=440:sample_rate=48000:duration=2" \
  -c:v mpeg4 -c:a mp2 -shortest

# 20. Range-trigger mid-size (~32–64 MB). Open-ended Range must stream, not buffer.
# Not multi-GB (that stays generate-only via LARGE=1). Still large enough to hang
# the old read_exact path for seconds if reintroduced.
echo "→ large-range-trigger.mp4 (this takes a bit)"
"$FFMPEG" -y -hide_banner -loglevel error \
  -f lavfi -i "testsrc=size=1920x1080:rate=30:duration=20" \
  -f lavfi -i "sine=frequency=440:sample_rate=48000:duration=20" \
  -c:v libx264 -pix_fmt yuv420p -b:v 20M -c:a aac -ac 2 -shortest \
  "$OUT/large-range-trigger.mp4"

# 21. PGS (HDMV) image subtitles: synthetic SUP muxed into MKV
echo "→ h264_aac_pgs_mkv.mkv"
python3 - "$OUT/_tmp.sup" <<'PY'
import struct, sys
path = sys.argv[1]
def seg(pts90, typ, payload: bytes) -> bytes:
    return b"PG" + struct.pack(">II", pts90, 0) + bytes([typ]) + struct.pack(">H", len(payload)) + payload
# Minimal PCS / WDS / PDS / END. Enough for ffprobe to report hdmv_pgs_subtitle.
pcs = struct.pack(">HHBHBBb", 1920, 1080, 0x10, 0, 0x00, 0, 0)
wds = bytes([1]) + struct.pack(">BHHHH", 0, 0, 0, 1, 1)
pds = bytes([0, 0])
open(path, "wb").write(seg(0, 0x16, pcs) + seg(0, 0x17, wds) + seg(0, 0x14, pds) + seg(0, 0x80, b""))
PY
gen _pgs_video.mp4 \
  -f lavfi -i "testsrc=size=1280x720:rate=24:duration=2" \
  -f lavfi -i "sine=frequency=440:sample_rate=48000:duration=2" \
  -c:v libx264 -pix_fmt yuv420p -c:a aac -ac 2 -shortest
"$FFMPEG" -y -hide_banner -loglevel error \
  -i "$OUT/_pgs_video.mp4" -i "$OUT/_tmp.sup" \
  -c copy -c:s copy \
  "$OUT/h264_aac_pgs_mkv.mkv"
rm -f "$OUT/_pgs_video.mp4" "$OUT/_tmp.sup"

# 22. HDR10: HEVC 10-bit + PQ / BT.2020 + master-display / max-cll
gen hevc_hdr10_mp4.mp4 \
  -f lavfi -i "testsrc=size=1280x720:rate=24:duration=2" \
  -f lavfi -i "sine=frequency=440:sample_rate=48000:duration=2" \
  -c:v libx265 -pix_fmt yuv420p10le -tag:v hvc1 \
  -x265-params "colorprim=bt2020:transfer=smpte2084:colormatrix=bt2020nc:master-display=G(13250,34500)B(7500,3000)R(34000,16000)WP(15635,16450)L(10000000,1):max-cll=1000,400" \
  -c:a aac -ac 2 -shortest

# 23. Two AAC stereo tracks tagged eng/spa (ADR-0012 multi-track switch)
gen h264_aac_multilang_mkv.mkv \
  -f lavfi -i "testsrc=size=640x360:rate=24:duration=2" \
  -f lavfi -i "sine=frequency=440:sample_rate=48000:duration=2" \
  -f lavfi -i "sine=frequency=880:sample_rate=48000:duration=2" \
  -map 0:v:0 -map 1:a:0 -map 2:a:0 \
  -c:v libx264 -pix_fmt yuv420p -c:a aac -ac 2 \
  -metadata:s:a:0 language=eng -metadata:s:a:1 language=spa -shortest

# 24. Main + commentary. Unlabelled second tracks are how commentary ships;
# the label comes from the title tag, not the language.
gen h264_aac_commentary_mkv.mkv \
  -f lavfi -i "testsrc=size=640x360:rate=24:duration=2" \
  -f lavfi -i "sine=frequency=440:sample_rate=48000:duration=2" \
  -f lavfi -i "sine=frequency=220:sample_rate=48000:duration=2" \
  -map 0:v:0 -map 1:a:0 -map 2:a:0 \
  -c:v libx264 -pix_fmt yuv420p -c:a aac -ac 2 \
  -metadata:s:a:0 language=eng -metadata:s:a:0 title="Main" \
  -metadata:s:a:1 language=eng -metadata:s:a:1 title="Commentary" \
  -disposition:a:0 default -disposition:a:1 0 -shortest

# 25. 6.0 FLAC (no LFE). Same channel count as 5.1; pan table must not apply
# (ADR-0012 falls back to -ac 2 with a warning).
gen h264_flac_60_mkv.mkv \
  -f lavfi -i "testsrc=size=640x360:rate=24:duration=2" \
  -f lavfi -i "anullsrc=channel_layout=6.0:sample_rate=48000:duration=2" \
  -c:v libx264 -pix_fmt yuv420p -c:a flac -shortest

# 26. 5.1(side) AC3. AAC strips this layout tag; AC3 keeps it so the fallback
# path is reachable from inventory (ADR-0012).
gen h264_ac3_51side_mkv.mkv \
  -f lavfi -i "testsrc=size=640x360:rate=24:duration=2" \
  -f lavfi -i "anullsrc=channel_layout=5.1(side):sample_rate=48000:duration=2" \
  -c:v libx264 -pix_fmt yuv420p -c:a ac3 -shortest

# Optional multi-GB open-ended Range stress (never commit; gitignored)
if [[ "${LARGE:-}" == "1" ]]; then
  echo "→ large-open-ended-range.mp4 (~2 GB, gitignored)"
  "$FFMPEG" -y -hide_banner -loglevel error \
    -f lavfi -i "testsrc=size=1920x1080:rate=30:duration=120" \
    -f lavfi -i "sine=frequency=440:sample_rate=48000:duration=120" \
    -c:v libx264 -pix_fmt yuv420p -b:v 140M -c:a aac -ac 2 -shortest \
    "$OUT/large-open-ended-range.mp4"
fi

# Optional >4GB file for 32-bit offset bugs (gitignored). Valid moov at start, then
# sparse-extend past 4 GiB so Range requests past 2^32 exercise 64-bit offsets.
if [[ "${OVER4GB:-}" == "1" ]]; then
  echo "→ large-over-4gb.mp4 (>4 GiB, gitignored)"
  "$FFMPEG" -y -hide_banner -loglevel error \
    -f lavfi -i "testsrc=size=320x240:rate=24:duration=1" \
    -f lavfi -i "sine=frequency=440:sample_rate=48000:duration=1" \
    -c:v libx264 -pix_fmt yuv420p -c:a aac -ac 2 -movflags +faststart -shortest \
    "$OUT/large-over-4gb.mp4"
  python3 - "$OUT/large-over-4gb.mp4" <<'PY'
import os, sys
path = sys.argv[1]
target = 4 * 1024 * 1024 * 1024 + 1024  # 4 GiB + 1 KiB
with open(path, "r+b") as f:
    f.truncate(target)
print(f"  size={os.path.getsize(path)}")
PY
fi

touch "$OUT/.generated"
echo "Done. Files in $OUT"
ls -lh "$OUT" | sed -n '1,50p'