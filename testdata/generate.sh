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
# ≥12s so seek/tonemap samples (e.g. -ss 5 -t 10) are valid.
gen hevc_hdr10_mp4.mp4 \
  -f lavfi -i "testsrc=size=1280x720:rate=24:duration=12" \
  -f lavfi -i "sine=frequency=440:sample_rate=48000:duration=12" \
  -c:v libx265 -pix_fmt yuv420p10le -tag:v hvc1 \
  -x265-params "colorprim=bt2020:transfer=smpte2084:colormatrix=bt2020nc:master-display=G(13250,34500)B(7500,3000)R(34000,16000)WP(15635,16450)L(10000000,1):max-cll=1000,400" \
  -c:a aac -ac 2 -shortest

# 22b. HLG: HEVC 10-bit + arib-std-b67 / BT.2020 (plain HLG, no DV)
gen hevc_hlg_mp4.mp4 \
  -f lavfi -i "testsrc=size=1280x720:rate=24:duration=2" \
  -f lavfi -i "sine=frequency=440:sample_rate=48000:duration=2" \
  -c:v libx265 -pix_fmt yuv420p10le -tag:v hvc1 \
  -x265-params "colorprim=bt2020:transfer=arib-std-b67:colormatrix=bt2020nc" \
  -c:a aac -ac 2 -shortest

# 22c. SDR BT.709 control (explicit colour tags; contrasts HDR axis)
gen h264_sdr_bt709_mp4.mp4 \
  -f lavfi -i "testsrc=size=1280x720:rate=24:duration=2" \
  -f lavfi -i "sine=frequency=440:sample_rate=48000:duration=2" \
  -c:v libx264 -pix_fmt yuv420p -profile:v high \
  -colorspace bt709 -color_primaries bt709 -color_trc bt709 \
  -x264-params "colorprim=bt709:transfer=bt709:colormatrix=bt709" \
  -c:a aac -ac 2 -shortest

# Tool helpers for HDR10+ / DV synthesis (optional; skip with a named reason).
TOOLS_BIN="$ROOT/../scripts/.tools/bin"
find_tool() {
  local name="$1"
  if command -v "$name" >/dev/null 2>&1; then
    command -v "$name"
    return 0
  fi
  if [[ -x "$TOOLS_BIN/$name" ]]; then
    echo "$TOOLS_BIN/$name"
    return 0
  fi
  return 1
}

# 22d. HDR10+: PQ HEVC + SMPTE 2094-40 dynamic metadata (control)
HDR10PLUS_JSON="$ROOT/assets/hdr10plus_48f.json"
if HDR10PLUS_TOOL="$(find_tool hdr10plus_tool)" && MKVMERGE="$(find_tool mkvmerge)" \
  && [[ -f "$HDR10PLUS_JSON" ]]; then
  echo "→ hevc_hdr10plus_mp4.mp4"
  TMP_H10P="$(mktemp -d)"
  "$FFMPEG" -y -hide_banner -loglevel error \
    -f lavfi -i "testsrc2=size=1280x720:rate=24:duration=2" \
    -f lavfi -i "sine=frequency=440:sample_rate=48000:duration=2" \
    -c:v libx265 -pix_fmt yuv420p10le -crf 28 \
    -x265-params "colorprim=bt2020:transfer=smpte2084:colormatrix=bt2020nc:master-display=G(13250,34500)B(7500,3000)R(34000,16000)WP(15635,16450)L(10000000,1):max-cll=1000,400:repeat-headers=1" \
    -color_primaries bt2020 -color_trc smpte2084 -colorspace bt2020nc \
    -c:a aac -ac 2 -shortest "$TMP_H10P/base.mp4"
  "$FFMPEG" -y -hide_banner -loglevel error -i "$TMP_H10P/base.mp4" -an -c:v copy \
    -bsf:v hevc_mp4toannexb "$TMP_H10P/base.hevc"
  "$HDR10PLUS_TOOL" inject -i "$TMP_H10P/base.hevc" -j "$HDR10PLUS_JSON" \
    -o "$TMP_H10P/inj.hevc"
  "$FFMPEG" -y -hide_banner -loglevel error -i "$TMP_H10P/base.mp4" -vn -c:a copy \
    "$TMP_H10P/a.m4a"
  "$MKVMERGE" -o "$TMP_H10P/mux.mkv" --default-duration 0:24fps \
    "$TMP_H10P/inj.hevc" "$TMP_H10P/a.m4a" >/dev/null
  "$FFMPEG" -y -hide_banner -loglevel error -i "$TMP_H10P/mux.mkv" -c copy -tag:v hvc1 \
    -color_primaries bt2020 -color_trc smpte2084 -colorspace bt2020nc \
    "$OUT/hevc_hdr10plus_mp4.mp4"
  rm -rf "$TMP_H10P"
else
  echo "skip hevc_hdr10plus_mp4.mp4: need hdr10plus_tool + mkvmerge + assets/hdr10plus_48f.json"
fi

# 22e. DV P8.4 (HLG base): dovi_tool generate + inject-rpu into short HLG HEVC
DOVI_P84_JSON="$ROOT/assets/dovi_p84_gen.json"
# dvvC from Dolby Browser Kit HLG-P8.4 (compat id 4); grafted after ffmpeg remux drops it.
DVVC_P84_HEX="00000020647676430100101d4000000000000000000000000000000000000000"
if DOVI_TOOL="$(find_tool dovi_tool)" && MKVMERGE="$(find_tool mkvmerge)" \
  && [[ -f "$DOVI_P84_JSON" ]]; then
  echo "→ hevc_dv_p84_hlg_mkv.mkv (+ mp4 when inject_dvvc.py works)"
  TMP_P84="$(mktemp -d)"
  # ≥12s (see HDR10 control); RPU length comes from assets/dovi_p84_gen.json.
  # Colour must live in the HEVC VUI (x265-params). Container tags alone are
  # lost after inject-rpu/mkvmerge; without transfer/primaries, product
  # zscale+hable fails with "no path between colorspaces".
  "$FFMPEG" -y -hide_banner -loglevel error \
    -f lavfi -i "testsrc2=size=1280x720:rate=24:duration=12" \
    -c:v libx265 -pix_fmt yuv420p10le -crf 28 \
    -x265-params "colorprim=bt2020:transfer=arib-std-b67:colormatrix=bt2020nc:repeat-headers=1:annexb=1:keyint=24:min-keyint=24" \
    -color_primaries bt2020 -color_trc arib-std-b67 -colorspace bt2020nc \
    -an -f hevc "$TMP_P84/hlg.hevc"
  "$FFMPEG" -y -hide_banner -loglevel error \
    -f lavfi -i "sine=frequency=440:sample_rate=48000:duration=12" \
    -c:a aac -ac 2 "$TMP_P84/a.m4a"
  "$DOVI_TOOL" generate -j "$DOVI_P84_JSON" -o "$TMP_P84/p84.rpu"
  "$DOVI_TOOL" inject-rpu -i "$TMP_P84/hlg.hevc" --rpu-in "$TMP_P84/p84.rpu" \
    -o "$TMP_P84/hlg_p84.hevc"
  "$MKVMERGE" -o "$OUT/hevc_dv_p84_hlg_mkv.mkv" --default-duration 0:24fps \
    "$TMP_P84/hlg_p84.hevc" "$TMP_P84/a.m4a" >/dev/null
  "$FFMPEG" -y -hide_banner -loglevel error -i "$OUT/hevc_dv_p84_hlg_mkv.mkv" \
    -c copy -tag:v hvc1 \
    -color_primaries bt2020 -color_trc arib-std-b67 -colorspace bt2020nc \
    "$TMP_P84/p84.mp4"
  if python3 "$ROOT/inject_dvvc.py" "$TMP_P84/p84.mp4" "$DVVC_P84_HEX"; then
    cp "$TMP_P84/p84.mp4" "$OUT/hevc_dv_p84_hlg_mp4.mp4"
  else
    echo "skip hevc_dv_p84_hlg_mp4.mp4: dvvC graft failed (MKV still written)"
  fi
  rm -rf "$TMP_P84"
else
  echo "skip hevc_dv_p84_hlg_*: need dovi_tool + mkvmerge + assets/dovi_p84_gen.json"
fi

# 22f. P8.1 mkv/mp4 pair (same content; Matroska vs MP4 DV signalling)
# Prefer local Browser Kit; else MakeMKV P8.1 after fetch. ffmpeg -c copy drops dvvC
# on MP4, so graft the kit/source dvvC (compat id 1 for HDR10 BL).
DVVC_P81_HEX="00000020647676430100101d1000000000000000000000000000000000000000"
P81_KIT="$OUT/dolby-vision-browser-kit/24fps/FHD/Patterns_Of_Nature_HDR10-P8.1_FHD_24_H265-4Mbps_DD+JOC-768Kbps.mp4"
P81_MAKEMKV="$OUT/dolby-vision-makemkv/P81_GlassBlowing2_3840x2160@59_94fps_15200kbps.mkv"
P81_SRC=""
if [[ -f "$P81_KIT" ]]; then
  P81_SRC="$P81_KIT"
elif [[ -f "$P81_MAKEMKV" ]]; then
  P81_SRC="$P81_MAKEMKV"
fi
if [[ -n "$P81_SRC" ]]; then
  echo "→ hevc_dv_p81_pair.{mkv,mp4} from $(basename "$P81_SRC")"
  TMP_P81="$(mktemp -d)"
  "$FFMPEG" -y -hide_banner -loglevel error -i "$P81_SRC" -t 2 -c copy \
    "$OUT/hevc_dv_p81_pair.mkv"
  "$FFMPEG" -y -hide_banner -loglevel error -i "$P81_SRC" -t 2 -c copy -tag:v hvc1 \
    "$TMP_P81/pair.mp4"
  if python3 "$ROOT/inject_dvvc.py" "$TMP_P81/pair.mp4" "$DVVC_P81_HEX"; then
    cp "$TMP_P81/pair.mp4" "$OUT/hevc_dv_p81_pair.mp4"
  else
    echo "skip hevc_dv_p81_pair.mp4: dvvC graft failed (MKV still written)"
  fi
  rm -rf "$TMP_P81"
else
  echo "skip hevc_dv_p81_pair.*: need Browser Kit P8.1 or fetched MakeMKV P81"
fi

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

# 27. Adversarial multi-sub MKV (ADR-0024 / Heartstopper-class).
# 32 soft SRT tracks, none flagged default; alphabetical language-name order
# puts Arabic at the lowest subtitle index. Duplicate language tags, SDH only
# in title, regional spa/por variants. Audio: commentary eng before main eng
# so first-stream-wins would pick the wrong dialogue.
ADV_SRT="$OUT/_adv_track_select.srt"
cat > "$ADV_SRT" <<'EOF'
1
00:00:00,000 --> 00:00:02,000
Nightjar adversarial subtitle
EOF
# language code, title — English display names sorted so ara is first.
ADV_SUBS=(
  "ara|Arabic"
  "chi|Chinese"
  "chi|Chinese (Traditional)"
  "cze|Czech"
  "dan|Danish"
  "dut|Dutch"
  "fin|Finnish"
  "fre|French"
  "ger|German"
  "gre|Greek"
  "heb|Hebrew"
  "hin|Hindi"
  "hun|Hungarian"
  "ind|Indonesian"
  "ita|Italian"
  "jpn|Japanese"
  "kor|Korean"
  "nor|Norwegian"
  "pol|Polish"
  "por|Portuguese"
  "por|Brazilian Portuguese"
  "rum|Romanian"
  "rus|Russian"
  "spa|Spanish"
  "spa|European Spanish"
  "swe|Swedish"
  "tha|Thai"
  "tur|Turkish"
  "ukr|Ukrainian"
  "vie|Vietnamese"
  "eng|English"
  "eng|English [SDH]"
)
if [[ ${#ADV_SUBS[@]} -ne 32 ]]; then
  echo "internal error: ADV_SUBS must be 32 entries, got ${#ADV_SUBS[@]}" >&2
  exit 1
fi
echo "→ h264_aac_adv_track_select_mkv.mkv (32 subs, commentary+main audio)"
ADV_ARGS=(
  -f lavfi -i "testsrc=size=640x360:rate=24:duration=2"
  -f lavfi -i "sine=frequency=220:sample_rate=48000:duration=2"
  -f lavfi -i "sine=frequency=440:sample_rate=48000:duration=2"
  -i "$ADV_SRT"
  -map 0:v:0 -map 1:a:0 -map 2:a:0
)
for _ in "${ADV_SUBS[@]}"; do
  ADV_ARGS+=(-map 3:0)
done
ADV_ARGS+=(
  -c:v libx264 -pix_fmt yuv420p -c:a aac -ac 2 -c:s srt
  -metadata:s:a:0 language=eng -metadata:s:a:0 title="Commentary"
  -metadata:s:a:1 language=eng -metadata:s:a:1 title="Main"
  -disposition:a:0 0 -disposition:a:1 0
)
si=0
for entry in "${ADV_SUBS[@]}"; do
  lang="${entry%%|*}"
  title="${entry#*|}"
  ADV_ARGS+=(-metadata:s:s:${si} language="$lang")
  ADV_ARGS+=(-metadata:s:s:${si} title="$title")
  ADV_ARGS+=(-disposition:s:${si} 0)
  si=$((si + 1))
done
ADV_ARGS+=(-shortest "$OUT/h264_aac_adv_track_select_mkv.mkv")
"$FFMPEG" -y -hide_banner -loglevel error "${ADV_ARGS[@]}"
rm -f "$ADV_SRT"

# 28. Forced-mode fixture (ADR-0024 / Phase 2 item 5). Non-English audio so
# forced selection can fire; English forced track present. Matching-audio
# control is the same file with preferred language = jpn (expect nothing).
FORCED_SRT="$OUT/_forced_track_select.srt"
cat > "$FORCED_SRT" <<'EOF'
1
00:00:00,000 --> 00:00:02,000
Nightjar forced signs
EOF
FORCED_FULL="$OUT/_forced_full.srt"
cat > "$FORCED_FULL" <<'EOF'
1
00:00:00,000 --> 00:00:02,000
Nightjar full English dialogue
EOF
echo "→ h264_aac_forced_track_select_mkv.mkv (jpn audio, eng forced + eng full)"
"$FFMPEG" -y -hide_banner -loglevel error \
  -f lavfi -i "testsrc=size=640x360:rate=24:duration=2" \
  -f lavfi -i "sine=frequency=440:sample_rate=48000:duration=2" \
  -i "$FORCED_SRT" -i "$FORCED_FULL" \
  -map 0:v:0 -map 1:a:0 -map 2:0 -map 3:0 \
  -c:v libx264 -pix_fmt yuv420p -c:a aac -ac 2 -c:s srt \
  -metadata:s:a:0 language=jpn -metadata:s:a:0 title="Japanese" \
  -metadata:s:s:0 language=eng -metadata:s:s:0 title="English (Forced)" \
  -metadata:s:s:1 language=eng -metadata:s:s:1 title="English" \
  -disposition:s:0 +forced -disposition:s:1 0 \
  -shortest \
  "$OUT/h264_aac_forced_track_select_mkv.mkv"
rm -f "$FORCED_SRT" "$FORCED_FULL"

# Optional multi-GB open-ended Range stress (never commit; gitignored)
if [[ "${LARGE:-}" == "1" ]]; then
  echo "→ large-open-ended-range.mp4 (~2 GB, gitignored)"
  "$FFMPEG" -y -hide_banner -loglevel error \
    -f lavfi -i "testsrc=size=1920x1080:rate=30:duration=120" \
    -f lavfi -i "sine=frequency=440:sample_rate=48000:duration=120" \
    -c:v libx264 -pix_fmt yuv420p -b:v 140M -c:a aac -ac 2 -shortest \
    "$OUT/large-open-ended-range.mp4"
fi

# Soak scrub resume fixture (nightjar-meta/scripts/soak_scrub.sh). HEVC forces Transcode
# (Rule 4.3 / first-scrub characterisation). Same file: soft SRT + ASS + PGS.
# Gitignored; regenerate with SOAK=1. Duration must clear ALIGN_BEHIND (32s)
# and leave room for far-ahead seeks past the encode head.
if [[ "${SOAK:-}" == "1" ]]; then
  SOAK_DUR="${SOAK_DUR:-180}"
  echo "→ soak_scrub_hevc_aac_subs_mkv.mkv (${SOAK_DUR}s, gitignored)"
  SOAK_ASS="$OUT/_soak.ass"
  SOAK_SRT="$OUT/_soak.srt"
  SOAK_SUP="$OUT/_soak.sup"
  cat > "$SOAK_ASS" <<EOF
[Script Info]
ScriptType: v4.00+
PlayResX: 640
PlayResY: 360

[V4+ Styles]
Format: Name, Fontname, Fontsize, PrimaryColour, SecondaryColour, OutlineColour, BackColour, Bold, Italic, Underline, StrikeOut, ScaleX, ScaleY, Spacing, Angle, BorderStyle, Outline, Shadow, Alignment, MarginL, MarginR, MarginV, Encoding
Style: Default,Arial,36,&H00FFFFFF,&H000000FF,&H00000000,&H00000000,0,0,0,0,100,100,0,0,1,2,0,2,10,10,20,1

[Events]
Format: Layer, Start, End, Style, Name, MarginL, MarginR, MarginV, Effect, Text
Dialogue: 0,0:00:00.00,0:00:05.00,Default,,0,0,0,,soak ASS t0
Dialogue: 0,0:00:40.00,0:00:50.00,Default,,0,0,0,,soak ASS mid
Dialogue: 0,0:02:00.00,0:02:10.00,Default,,0,0,0,,soak ASS late
EOF
  cat > "$SOAK_SRT" <<EOF
1
00:00:00,000 --> 00:00:05,000
soak SRT t0

2
00:00:40,000 --> 00:00:50,000
soak SRT mid

3
00:02:00,000 --> 00:02:10,000
soak SRT late
EOF
  python3 - "$SOAK_SUP" <<'PY'
import struct, sys
path = sys.argv[1]
def seg(pts90, typ, payload: bytes) -> bytes:
    return b"PG" + struct.pack(">II", pts90, 0) + bytes([typ]) + struct.pack(">H", len(payload)) + payload
pcs = struct.pack(">HHBHBBb", 640, 360, 0x10, 0, 0x00, 0, 0)
wds = bytes([1]) + struct.pack(">BHHHH", 0, 0, 0, 1, 1)
pds = bytes([0, 0])
open(path, "wb").write(seg(0, 0x16, pcs) + seg(0, 0x17, wds) + seg(0, 0x14, pds) + seg(0, 0x80, b""))
PY
  # Moderate bitrate so encode lags seeks without a delay-injection knob
  # (Rule 4.7). Throttle further via THROTTLE_BPS in nightjar-meta/scripts/soak_scrub.sh.
  "$FFMPEG" -y -hide_banner -loglevel error \
    -f lavfi -i "testsrc=size=640x360:rate=24:duration=${SOAK_DUR}" \
    -f lavfi -i "sine=frequency=440:sample_rate=48000:duration=${SOAK_DUR}" \
    -i "$SOAK_SRT" -i "$SOAK_ASS" -i "$SOAK_SUP" \
    -map 0:v:0 -map 1:a:0 -map 2:0 -map 3:0 -map 4:0 \
    -c:v libx265 -pix_fmt yuv420p -tag:v hvc1 -b:v 4M \
    -c:a aac -ac 2 \
    -c:s:0 srt -c:s:1 ass -c:s:2 copy \
    -metadata:s:s:0 language=eng -metadata:s:s:0 title="soft" \
    -metadata:s:s:1 language=eng -metadata:s:s:1 title="ass" \
    -metadata:s:s:2 language=eng -metadata:s:s:2 title="pgs" \
    -t "$SOAK_DUR" \
    "$OUT/soak_scrub_hevc_aac_subs_mkv.mkv"
  rm -f "$SOAK_ASS" "$SOAK_SRT" "$SOAK_SUP"
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

# ---------------------------------------------------------------------------
# Dolby Vision MakeMKV test clips (Rule 4.3 / official Dolby samples).
# Video (+ audio) MKVs hosted at makemkv.com/download/dvtest/. Do NOT commit
# to git/LFS — fetch locally, verify pinned sha256, skip with a named reason.
# ---------------------------------------------------------------------------
fetch_makemkv_dvtest() {
  local dir="$OUT/dolby-vision-makemkv"
  mkdir -p "$dir"

  # Self-pinned 2026-08-02: Wayback id_ fetches (makemkv.com returned Cloudflare
  # 525). Stability of the local cache only — not provenance. See testdata/README.md.
  local -a SPECS=(
    "P4_LG_Dolby_Trailer_4K_Demo.mkv|20220530161818|044642f88616f6b72a819c2719a7e07b377b3428de88bd1c10d64fc69a736515"
    "P5_Dolby_Amaze.mkv|20220530162104|1a97082e1f2e4d4cf56618370fc842f558dafe6156ef6a4878fcac5a0a65f476"
    "P7_FEL_GIJoe_The_Rise_of_Cobra.mkv|20220530162117|06e42fc4e06ee90c8eea0b7a31450f844ad4228ea639a5ba12d51c05d2930e63"
    "P7_MEL_GIJoe_The_Rise_of_Cobra.mkv|20220530162129|9deb64f1f07b367b62cf853fee1d4e54054d1eaf11da2c744dbe06062b5881cc"
    "P81_GlassBlowing2_3840x2160@59_94fps_15200kbps.mkv|20220530163158|db0e079a0c911ca351e228055d5cc147e0d7b7755d5255914318336624f80d04"
  )

  if ! command -v curl >/dev/null 2>&1; then
    echo "skip dolby-vision-makemkv/*: curl not on PATH"
    return 0
  fi
  if ! command -v shasum >/dev/null 2>&1 && ! command -v sha256sum >/dev/null 2>&1; then
    echo "skip dolby-vision-makemkv/*: no sha256 tool (shasum/sha256sum)"
    return 0
  fi

  digest_of() {
    if command -v shasum >/dev/null 2>&1; then
      shasum -a 256 "$1" | awk '{print $1}'
    else
      sha256sum "$1" | awk '{print $1}'
    fi
  }

  local entry name ts want primary archive got
  for entry in "${SPECS[@]}"; do
    IFS='|' read -r name ts want <<<"$entry"
    local dest="$dir/$name"
    if [[ -f "$dest" ]]; then
      got="$(digest_of "$dest")"
      if [[ "$got" == "$want" ]]; then
        echo "→ dolby-vision-makemkv/$name (cached, sha256 ok)"
        continue
      fi
      echo "skip dolby-vision-makemkv/$name: cached digest mismatch (got $got want $want); remove file to re-fetch"
      continue
    fi

    primary="https://www.makemkv.com/download/dvtest/${name}"
    archive="https://web.archive.org/web/${ts}id_/https://www.makemkv.com/download/dvtest/${name}"
    echo "→ fetch dolby-vision-makemkv/$name"
    if ! curl -fL --connect-timeout 20 --max-time 120 -A 'Mozilla/5.0' \
      -o "$dest.part" "$primary" 2>/dev/null; then
      rm -f "$dest.part"
      if ! curl -fL --connect-timeout 30 --retry 3 --retry-delay 2 -C - \
        -A 'Mozilla/5.0' -o "$dest.part" "$archive"; then
        rm -f "$dest.part"
        echo "skip dolby-vision-makemkv/$name: fetch failed (makemkv.com + Wayback unavailable)"
        continue
      fi
    fi
    mv "$dest.part" "$dest"
    got="$(digest_of "$dest")"
    if [[ "$got" != "$want" ]]; then
      rm -f "$dest"
      echo "skip dolby-vision-makemkv/$name: sha256 mismatch (got $got want $want)"
      continue
    fi
    echo "→ dolby-vision-makemkv/$name (sha256 ok)"
  done
}

fetch_makemkv_dvtest

# Rebuild P8.1 pair if MakeMKV P81 arrived after the earlier synth pass.
if [[ ! -f "$OUT/hevc_dv_p81_pair.mkv" ]] \
  && [[ -f "$OUT/dolby-vision-makemkv/P81_GlassBlowing2_3840x2160@59_94fps_15200kbps.mkv" ]]; then
  echo "→ hevc_dv_p81_pair.* (retry from MakeMKV P81)"
  P81_SRC="$OUT/dolby-vision-makemkv/P81_GlassBlowing2_3840x2160@59_94fps_15200kbps.mkv"
  TMP_P81="$(mktemp -d)"
  "$FFMPEG" -y -hide_banner -loglevel error -i "$P81_SRC" -t 2 -c copy \
    "$OUT/hevc_dv_p81_pair.mkv"
  "$FFMPEG" -y -hide_banner -loglevel error -i "$P81_SRC" -t 2 -c copy -tag:v hvc1 \
    "$TMP_P81/pair.mp4"
  python3 "$ROOT/inject_dvvc.py" "$TMP_P81/pair.mp4" "$DVVC_P81_HEX" \
    && cp "$TMP_P81/pair.mp4" "$OUT/hevc_dv_p81_pair.mp4" \
    || echo "skip hevc_dv_p81_pair.mp4: dvvC graft failed"
  rm -rf "$TMP_P81"
fi

touch "$OUT/.generated"
echo "Done. Files in $OUT"
ls -lh "$OUT" | sed -n '1,50p'