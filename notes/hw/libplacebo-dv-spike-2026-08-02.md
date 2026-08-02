# libplacebo Dolby Vision tonemap spike (2026-08-02)

**Claim class:** measurement only. No product change, no Dockerfile change, no ADR.

**Status (2026-08-02, revised): INCONCLUSIVE — not negative.**

### Proposed ADR (title + decision only — not drafted)

- **Title:** Dolby Vision Profile 5 playback (libplacebo+libdovi vs dovi_tool P5→P8.1)
- **Decision (one sentence):** P5→SDR needs either libplacebo linked with libdovi, or a dovi_tool P5→P8.1 conversion step before encode — both are new dependencies in the transcode path and require an ADR before implementation; until then decide refuses P5 tonemap with a named reason.

## Proven by inspection (human, 2026-08-02)

Not measured. Reported by Garrett, 2026-08-02, viewing the product web player
(founder Mac, product HLS). Agent did not see the output and does not assess
picture here. Verdict column is his report only: `correct` | `visibly wrong`
(artefact named) | `failed session`. Where he declined those three, his wording
is kept.

Record stills (session grab, not an assessment):
`notes/hw/libplacebo-dv-spike-2026-08-02-product-stills/{HDR10,P84,P81,P7_MEL,P7_FEL}.jpg`

| Label | Corpus path | Garrett report | His notes |
|---|---|---|---|
| HDR10 | `testdata/files/hevc_hdr10_mp4.mp4` | correct | Looks great. |
| P8.4 | `testdata/files/hevc_dv_p84_hlg_mkv.mkv` | (not called) | Hard to tell; maybe some moving parts aren’t supposed to be green — he said unknown. |
| P8.1 | `testdata/files/dolby-vision-makemkv/P81_GlassBlowing2_….mkv` | correct | Looks great; buffering like P7. |
| P7 MEL | `testdata/files/dolby-vision-makemkv/P7_MEL_GIJoe_….mkv` | correct | Looks great; lots of buffering like P7 FEL. |
| P7 FEL | `testdata/files/dolby-vision-makemkv/P7_FEL_GIJoe_….mkv` | correct | Looks good; buffers every 1–2 seconds. |
| P5 | `…/P5_Dolby_Amaze.mkv` | failed session | Named refuse at decide/session (`dolby_vision_p5`); no tonemap attempt. Forced encode exits 187 with no output. |

Buffering (Garrett): observed on P7 FEL, P7 MEL, and P8.1. Not a picture verdict.

### Same zscale error string, two causes (do not conflate)

Product / spike `zscale` can fail with `no path between colorspaces` for different
reasons:

1. **P8.4 fixture bug (fixed in corpus generation):** earlier `hevc_dv_p84_hlg_*`
   encodes omitted HEVC VUI colour tags (`x265-params` colourprim/transfer/
   colormatrix). After inject-rpu/mkvmerge, transfer/primaries were unknown and
   zscale+hable failed. That is a corpus generation bug, not a Profile 8.4
   tonemap limitation. Regenerated fixtures carry HLG/BT.2020 in the VUI.
2. **P5 genuine limitation:** Profile 5 is IPT-PQ; there is no P5→SDR path in the
   current product tonemap chain. Decide refuses with a named reason
   (`dolby_vision_p5`); do not treat that as a missing-tag fixture problem.

One symptom string, two causes — keep them distinct.

### Withdrawn (do not cite)

All prior conclusions that path B “does / does not tonemap Dolby Vision correctly,”
or that green B frames prove a libplacebo DV failure, are **withdrawn**.

**Reason:** the measured libplacebo build reported `libdovi: NO`. Without
`libdovi`, this spike did not establish that RPUs were applied. Green output on
non-DV material (operator report on HDR10; P7/P81 B frames green while A looks
normal) indicates a **pipeline fault upstream of DV** (hwupload / Vulkan /
hwdownload / Intel Mesa path), not a measured DV comparison. `dovi: YES` in
meson is not a substitute for `libdovi: YES`.

Wall-clock timings, lavapipe enumeration, macOS ICD facts, and ffprobe tags
below remain valid as **pipeline / host** measurements. DV fidelity claims
require a rebuild with `libdovi: YES` and a passing HDR10 control on that build.

## Versions (pinned before build)

| Component | Pin | Why |
|---|---|---|
| FFmpeg | `n8.1.2` | Matches player `FFmpegBuild` / local tonemap host (`notes/hdr-tonemap-delta-2026-08-01.md`). Product image today is Debian bookworm `ffmpeg` (no libplacebo); this is the upstream tag a custom ship build would use. |
| libplacebo | `v7.351.0` | FFmpeg `n8.1.2` configure requires `libplacebo >= 5.229.0`; `vf_libplacebo.c` on that tag has `PL_API_VER >= 351` branches. Pin the matching API release, not tip-of-tree. |
| Vulkan headers | Khronos `v1.3.290` in `/work/Vulkan-Headers` | FFmpeg `n8.1.2` requires headers `>= 1.3.277`; bookworm `libvulkan-dev` is 1.3.239 and fails configure. |
| Vulkan loader | bookworm `libvulkan1` / `libvulkan-dev` 1.3.239 | Runtime loader from distro; headers from Khronos pin above. |
| lcms2 | bookworm `liblcms2-dev` | Enabled in libplacebo meson (`lcms: YES`) and FFmpeg `--enable-lcms2`. |

### Build deviations from the original configure line (measured)

- **`--enable-libshaderc` omitted.** Bookworm `libshaderc.so` leaves unresolved `glslang` / `spvtools` symbols against `glslang-dev` 12.0.0 static archives; link probe never succeeded. libplacebo rebuilt with `-Dshaderc=disabled -Dglslang=enabled` instead (meson: `glslang: YES`, `shaderc: NO`).
- **`libdovi: NO` on the first build** (meson). That voids DV comparison claims (see Status). Rebuild with `-Dlibdovi=enabled` is required before any further DV run; if link fails in this environment, that failure is the finding.
- **`PKG_CONFIG_PATH` must include** `/work/prefix/lib/x86_64-linux-gnu/pkgconfig` (libplacebo) and a synthetic `vulkan.pc` Version `1.3.290` pointing at Khronos headers.
- **Runtime `LD_LIBRARY_PATH`:** `/work/prefix/lib` (FFmpeg `.so`) and `/work/prefix/lib/x86_64-linux-gnu` (libplacebo).

Recorded after first build (pre-libdovi):

```text
FFmpeg tag:        n8.1.2
FFmpeg commit:     38b88335f99e76ed89ff3c93f877fdefce736c13
configuration:     --prefix=/work/prefix --enable-gpl --enable-shared --enable-libplacebo --enable-vulkan --enable-lcms2 --enable-libzimg --enable-libx264 --extra-cflags='-I/work/prefix/include -I/work/Vulkan-Headers/include' --extra-ldflags=-L/work/prefix/lib/x86_64-linux-gnu --disable-doc --disable-htmlpages
filters present:   libplacebo, zscale, tonemap
libplacebo tag:    v7.351.0
libplacebo commit: 3188549fba13bbdf3a5a98de2a38c2e71f04e21e
libplacebo pkg:    7.351.0
meson features:    dovi: YES, libdovi: NO, glslang: YES, shaderc: NO, vulkan: YES, lcms: YES
shaderc:           not linked (see deviations)
glslang:           Debian glslang-dev 12.0.0-2 (libplacebo backend)
Vulkan headers:    Khronos v1.3.290
vulkan loader:     Debian libvulkan-dev 1.3.239.0-1
Mesa (measure):    22.3.6 (Intel ANV on UHD 770; lavapipe llvmpipe same Mesa)
```

## Host

```text
Host:     RM400 (Unraid)
GPU path: Mesa Vulkan on UHD 770 (renderD128 / pci 00:02.0)
Arc:      present as renderD129 — not used for this spike
Emby:     pause before timed runs if it holds the iGPU (PAUSE_EMBY=1)
```

Filled from measure container `vulkaninfo --summary`:

```text
Vulkan device name: Intel(R) Graphics (RPL-S)  (deviceID 0xa780, UHD 770)
Driver / Mesa:      Intel open-source Mesa driver, Mesa 22.3.6
API version:        1.3.230 (instance 1.3.239)
ICD:                /usr/share/vulkan/icd.d/intel_icd.x86_64.json
render node:        /dev/dri/renderD128
```

## Corpus

| Label | Path (repo-relative) |
|---|---|
| P5 | `testdata/files/dolby-vision-makemkv/P5_Dolby_Amaze.mkv` |
| P7 MEL | `testdata/files/dolby-vision-makemkv/P7_MEL_GIJoe_The_Rise_of_Cobra.mkv` |
| P7 FEL | `testdata/files/dolby-vision-makemkv/P7_FEL_GIJoe_The_Rise_of_Cobra.mkv` |
| P8.1 | `testdata/files/dolby-vision-makemkv/P81_GlassBlowing2_3840x2160@59_94fps_15200kbps.mkv` |
| P8.4 | `testdata/files/hevc_dv_p84_hlg_mkv.mkv` |
| HDR10 (control) | `testdata/files/hevc_hdr10_mp4.mp4` |

On-box corpus root after sync: `/mnt/user/appdata/nightjar-test/libplacebo-spike/corpus/`

## Filter strings

Path A (current Nightjar `HDR_TONEMAP_CHAIN` from `server/crates/transcode/src/hls.rs`):

```text
zscale=t=linear:npl=100,format=gbrpf32le,zscale=p=bt709,tonemap=tonemap=hable:desat=0,zscale=t=bt709:m=bt709:r=tv,format=yuv420p,sidedata=delete
```

Path B (libplacebo, DV apply on, SDR out, 1080p for timing runs):

```text
hwupload,libplacebo=w=1920:h=1080:force_original_aspect_ratio=decrease:tonemapping=hable:colorspace=bt709:color_primaries=bt709:color_trc=bt709:range=tv:format=yuv420p:apply_dolbyvision=true,hwdownload,format=yuv420p
```

Stills use the same chains without an extra scale (native frame, then PNG). Timing runs always scale/encode 1080p H.264 (`libx264 -preset veryfast -crf 23`) so A and B share the encode cost; wall-clock delta is the tonemap path.

Still seek: `-ss 5` (input seek), one frame. Segment: `-t 10` from the same seek.

## Output layout

```text
/mnt/user/appdata/nightjar-test/libplacebo-spike/
  build/          # sources + prefix (outside product tree)
  corpus/         # synced inputs
  out/<label>/
    A.mp4         # 10s path A
    B.mp4         # 10s path B
    A.png         # still @ 5s path A
    B.png         # still @ 5s path B
    A.time        # /usr/bin/time -p (or TIMEFORMAT)
    B.time
  concurrent/     # 1x and 3x path-B 1080p
  lavapipe/       # no-GPU / software Vulkan
  VERSIONS.txt
  vulkaninfo.txt
```

Repo copy of stills (optional, after scp): `notes/hw/libplacebo-dv-spike-2026-08-02-stills/`

## Commands (run by operator — not agent)

### 0) Mac → RM400: sync corpus

```bash
SPIKE=/mnt/user/appdata/nightjar-test/libplacebo-spike
ssh root@rm400 "mkdir -p $SPIKE/corpus $SPIKE/out $SPIKE/build $SPIKE/concurrent $SPIKE/lavapipe"

scp \
  testdata/files/dolby-vision-makemkv/P5_Dolby_Amaze.mkv \
  testdata/files/dolby-vision-makemkv/P7_MEL_GIJoe_The_Rise_of_Cobra.mkv \
  testdata/files/dolby-vision-makemkv/P7_FEL_GIJoe_The_Rise_of_Cobra.mkv \
  testdata/files/dolby-vision-makemkv/P81_GlassBlowing2_3840x2160@59_94fps_15200kbps.mkv \
  testdata/files/hevc_dv_p84_hlg_mkv.mkv \
  testdata/files/hevc_hdr10_mp4.mp4 \
  root@rm400:$SPIKE/corpus/
```

### 1) RM400: build FFmpeg + libplacebo in Debian bookworm (out of tree)

Build container keeps sources and prefix under `$SPIKE/build`. Does not touch the Nightjar image or Dockerfile.

```bash
SPIKE=/mnt/user/appdata/nightjar-test/libplacebo-spike
# optional: docker pause emby

docker run --rm -it \
  --name libplacebo-spike-build \
  -v "$SPIKE/build:/work" \
  -w /work \
  debian:bookworm bash -lc '
set -euo pipefail
export DEBIAN_FRONTEND=noninteractive
apt-get update
apt-get install -y --no-install-recommends \
  ca-certificates git build-essential pkg-config nasm yasm \
  python3 python3-pip ninja-build meson \
  libvulkan-dev vulkan-tools \
  libshaderc-dev glslang-tools \
  liblcms2-dev \
  libzimg-dev \
  libx264-dev libx265-dev libmp3lame-dev libopus-dev libvpx-dev \
  libfribidi-dev libfreetype6-dev libharfbuzz-dev \
  xxd wget

# --- libplacebo v7.351.0 ---
if [[ ! -d libplacebo-src/.git ]]; then
  git clone --depth 1 --branch v7.351.0 \
    https://code.videolan.org/videolan/libplacebo.git libplacebo-src
  cd libplacebo-src && git submodule update --init --recursive && cd ..
fi
cd libplacebo-src
meson setup build --prefix=/work/prefix --buildtype=release \
  -Dvulkan=enabled -Dshaderc=enabled -Dlcms=enabled \
  -Ddemos=false -Dtests=false -Dbench=false
ninja -C build
ninja -C build install
cd /work
echo "libplacebo $(pkg-config --modversion libplacebo || true)" | tee -a VERSIONS.txt
pkg-config --modversion libplacebo | tee PREFIX_LIBPLACEBO_VER.txt
git -C libplacebo-src rev-parse HEAD | tee LIBPLACEBO_COMMIT.txt

# --- FFmpeg n8.1.2 ---
if [[ ! -d ffmpeg-src/.git ]]; then
  git clone --depth 1 --branch n8.1.2 \
    https://git.ffmpeg.org/ffmpeg.git ffmpeg-src
fi
cd ffmpeg-src
export PKG_CONFIG_PATH=/work/prefix/lib/pkgconfig${PKG_CONFIG_PATH:+:$PKG_CONFIG_PATH}
export LD_LIBRARY_PATH=/work/prefix/lib${LD_LIBRARY_PATH:+:$LD_LIBRARY_PATH}
./configure --prefix=/work/prefix \
  --enable-gpl --enable-shared \
  --enable-libplacebo --enable-vulkan --enable-libshaderc --enable-lcms2 \
  --enable-libzimg --enable-libx264 \
  --disable-doc --disable-htmlpages
make -j"$(nproc)"
make install
cd /work
/work/prefix/bin/ffmpeg -version | tee FFMPEG_VERSION.txt
git -C ffmpeg-src rev-parse HEAD | tee FFMPEG_COMMIT.txt
/work/prefix/bin/ffmpeg -hide_banner -filters | grep -E "libplacebo|zscale|tonemap" | tee FILTERS.txt
dpkg -l | grep -E "shaderc|lcms2|vulkan|libzimg" | tee DEB_PKGS.txt
'
```

### 2) RM400: measure container (Vulkan on UHD 770)

```bash
SPIKE=/mnt/user/appdata/nightjar-test/libplacebo-spike
FF="$SPIKE/build/prefix"

docker run --rm -it \
  --name libplacebo-spike-run \
  --device=/dev/dri/card0 \
  --device=/dev/dri/renderD128 \
  --group-add video \
  -e LD_LIBRARY_PATH=/opt/spike/lib \
  -e VK_ICD_FILENAMES=/usr/share/vulkan/icd.d/intel_icd.x86_64.json \
  -v "$FF:/opt/spike:ro" \
  -v "$SPIKE/corpus:/corpus:ro" \
  -v "$SPIKE/out:/out" \
  -v "$SPIKE/concurrent:/concurrent" \
  -v "$SPIKE/lavapipe:/lavapipe" \
  debian:bookworm bash -lc '
set -euo pipefail
export DEBIAN_FRONTEND=noninteractive
apt-get update
apt-get install -y --no-install-recommends \
  mesa-vulkan-drivers vulkan-tools libvulkan1 \
  libzimg2 libx264-163 liblcms2-2 \
  time ca-certificates
export PATH=/opt/spike/bin:$PATH
export LD_LIBRARY_PATH=/opt/spike/lib
ffmpeg -version | head -2
vulkaninfo --summary 2>&1 | tee /out/../build/vulkaninfo.txt || vulkaninfo 2>&1 | head -80 | tee /out/../build/vulkaninfo.txt

A_VF="zscale=t=linear:npl=100,format=gbrpf32le,zscale=p=bt709,tonemap=tonemap=hable:desat=0,zscale=t=bt709:m=bt709:r=tv,format=yuv420p,sidedata=delete,scale=1920:1080:force_original_aspect_ratio=decrease,pad=1920:1080:(ow-iw)/2:(oh-ih)/2"
B_VF="hwupload,libplacebo=w=1920:h=1080:force_original_aspect_ratio=decrease:tonemapping=hable:colorspace=bt709:color_primaries=bt709:color_trc=bt709:range=tv:format=yuv420p:apply_dolbyvision=true,hwdownload,format=yuv420p"
A_STILL="zscale=t=linear:npl=100,format=gbrpf32le,zscale=p=bt709,tonemap=tonemap=hable:desat=0,zscale=t=bt709:m=bt709:r=tv,format=yuv420p,sidedata=delete"
B_STILL="hwupload,libplacebo=tonemapping=hable:colorspace=bt709:color_primaries=bt709:color_trc=bt709:range=tv:format=yuv420p:apply_dolbyvision=true,hwdownload,format=yuv420p"

run_one() {
  local label="$1" src="$2"
  mkdir -p "/out/$label"
  echo "=== $label A (zscale+hable) ==="
  /usr/bin/time -p -o "/out/$label/A.time" \
    ffmpeg -nostdin -hide_banner -loglevel error -y \
      -ss 5 -t 10 -i "$src" -an -map 0:v:0 \
      -vf "$A_VF" -c:v libx264 -preset veryfast -crf 23 \
      "/out/$label/A.mp4"
  ffmpeg -nostdin -hide_banner -loglevel error -y \
    -ss 5 -i "$src" -an -map 0:v:0 -vf "$A_STILL" -frames:v 1 \
    "/out/$label/A.png"
  echo "=== $label B (libplacebo+DV) ==="
  set +e
  /usr/bin/time -p -o "/out/$label/B.time" \
    ffmpeg -nostdin -hide_banner -loglevel warning -y \
      -init_hw_device vulkan=vk:0 -filter_hw_device vk \
      -ss 5 -t 10 -i "$src" -an -map 0:v:0 \
      -vf "$B_VF" -c:v libx264 -preset veryfast -crf 23 \
      "/out/$label/B.mp4" 2>"/out/$label/B.err"
  echo $? >"/out/$label/B.rc"
  ffmpeg -nostdin -hide_banner -loglevel warning -y \
    -init_hw_device vulkan=vk:0 -filter_hw_device vk \
    -ss 5 -i "$src" -an -map 0:v:0 -vf "$B_STILL" -frames:v 1 \
    "/out/$label/B.png" 2>>"/out/$label/B.err"
  set -e
  echo "A.time:"; cat "/out/$label/A.time"
  echo "B.time:"; cat "/out/$label/B.time" || true
  echo "B.rc=$(cat /out/$label/B.rc)"
}

run_one P5     /corpus/P5_Dolby_Amaze.mkv
run_one P7_MEL /corpus/P7_MEL_GIJoe_The_Rise_of_Cobra.mkv
run_one P7_FEL /corpus/P7_FEL_GIJoe_The_Rise_of_Cobra.mkv
run_one P81    /corpus/P81_GlassBlowing2_3840x2160@59_94fps_15200kbps.mkv
run_one P84    /corpus/hevc_dv_p84_hlg_mkv.mkv
run_one HDR10  /corpus/hevc_hdr10_mp4.mp4
'
```

Realtime factor = `10 / real` from each `.time` file (media seconds / wall seconds). Values >1 mean faster than realtime.

### 3) Concurrent path B (1× and 3× 1080p)

Same container, after single-file runs. Source: pick one long UHD DV file (P5) so 10s is available.

```bash
# inside the measure container (or re-enter with same docker run flags)
B_VF="hwupload,libplacebo=w=1920:h=1080:force_original_aspect_ratio=decrease:tonemapping=hable:colorspace=bt709:color_primaries=bt709:color_trc=bt709:range=tv:format=yuv420p:apply_dolbyvision=true,hwdownload,format=yuv420p"
SRC=/corpus/P5_Dolby_Amaze.mkv

run_n() {
  local n="$1"
  mkdir -p "/concurrent/n$n"
  local pids=()
  local i
  for i in $(seq 1 "$n"); do
    /usr/bin/time -p -o "/concurrent/n$n/$i.time" \
      ffmpeg -nostdin -hide_banner -loglevel error -y \
        -init_hw_device vulkan=vk:0 -filter_hw_device vk \
        -ss 5 -t 10 -i "$SRC" -an -map 0:v:0 \
        -vf "$B_VF" -c:v libx264 -preset veryfast -crf 23 \
        "/concurrent/n$n/$i.mp4" &
    pids+=($!)
  done
  local rc=0
  for pid in "${pids[@]}"; do wait "$pid" || rc=1; done
  echo "n=$n aggregate_rc=$rc"
  for i in $(seq 1 "$n"); do echo -n "$i "; cat "/concurrent/n$n/$i.time"; done
}

run_n 1
run_n 3
```

Hold realtime: every session’s `10/real >= 1.0` (or state the floor used).

### 4) No-GPU / lavapipe

New container: **no** `--device=/dev/dri*`. Force lavapipe ICD.

```bash
SPIKE=/mnt/user/appdata/nightjar-test/libplacebo-spike
FF="$SPIKE/build/prefix"

docker run --rm -it \
  --name libplacebo-spike-lavapipe \
  -e LD_LIBRARY_PATH=/opt/spike/lib \
  -e VK_ICD_FILENAMES=/usr/share/vulkan/icd.d/lvp_icd.x86_64.json \
  -e LIBGL_ALWAYS_SOFTWARE=1 \
  -v "$FF:/opt/spike:ro" \
  -v "$SPIKE/corpus:/corpus:ro" \
  -v "$SPIKE/lavapipe:/lavapipe" \
  debian:bookworm bash -lc '
set -euo pipefail
export DEBIAN_FRONTEND=noninteractive
apt-get update
apt-get install -y --no-install-recommends \
  mesa-vulkan-drivers vulkan-tools libvulkan1 \
  libzimg2 libx264-163 liblcms2-2 time
export PATH=/opt/spike/bin:$PATH
export LD_LIBRARY_PATH=/opt/spike/lib
echo "ICD=$VK_ICD_FILENAMES"
ls -l /usr/share/vulkan/icd.d/ || true
vulkaninfo --summary 2>&1 | tee /lavapipe/vulkaninfo.txt
# Does lavapipe present a device?
rg -n "GPU|deviceName|driverName|lavapipe|llvmpipe" /lavapipe/vulkaninfo.txt || true

B_VF="hwupload,libplacebo=w=1920:h=1080:force_original_aspect_ratio=decrease:tonemapping=hable:colorspace=bt709:color_primaries=bt709:color_trc=bt709:range=tv:format=yuv420p:apply_dolbyvision=true,hwdownload,format=yuv420p"
set +e
/usr/bin/time -p -o /lavapipe/B.time \
  ffmpeg -nostdin -hide_banner -loglevel warning -y \
    -init_hw_device vulkan=vk:0 -filter_hw_device vk \
    -ss 5 -t 10 -i /corpus/P5_Dolby_Amaze.mkv -an -map 0:v:0 \
    -vf "$B_VF" -c:v libx264 -preset veryfast -crf 23 \
    /lavapipe/B.mp4 2>/lavapipe/B.err
echo $? | tee /lavapipe/B.rc
set -e
cat /lavapipe/B.time || true
'
```

### 5) Mac → repo: pull stills + timings

```bash
SPIKE=/mnt/user/appdata/nightjar-test/libplacebo-spike
mkdir -p notes/hw/libplacebo-dv-spike-2026-08-02-stills
scp -r root@rm400:$SPIKE/out notes/hw/libplacebo-dv-spike-2026-08-02-stills/
scp -r root@rm400:$SPIKE/concurrent notes/hw/libplacebo-dv-spike-2026-08-02-stills/concurrent
scp -r root@rm400:$SPIKE/lavapipe notes/hw/libplacebo-dv-spike-2026-08-02-stills/lavapipe
scp root@rm400:$SPIKE/build/{VERSIONS.txt,FFMPEG_VERSION.txt,FFMPEG_COMMIT.txt,LIBPLACEBO_COMMIT.txt,PREFIX_LIBPLACEBO_VER.txt,FILTERS.txt,DEB_PKGS.txt,vulkaninfo.txt} \
  notes/hw/libplacebo-dv-spike-2026-08-02-stills/ 2>/dev/null || true
```

### 6) macOS story (founder Mac — facts only)

```bash
# Native Vulkan?
system_profiler SPDisplaysDataType | head -40
ls /usr/local/share/vulkan/icd.d /opt/homebrew/share/vulkan/icd.d 2>/dev/null || echo "no homebrew vulkan ICD dirs"
brew list --versions molten-vk vulkan-headers vulkan-loader 2>/dev/null || echo "no molten-vk/vulkan via brew"
ffmpeg -hide_banner -filters 2>/dev/null | grep libplacebo || echo "homebrew ffmpeg: no libplacebo filter"
ffmpeg -version | head -3
```

## Cheap checks on first-run artefacts (before rebuild)

### 1) ffprobe colour tags (verbatim)

Sources: `~/notes/hw/libplacebo-dv-spike-2026-08-02-stills/out/<label>/{A,B}.mp4`

**P5 A.mp4:** invalid (`moov atom not found`) — empty/failed encode.  
**P5 B.mp4:**
```text
codec_name=h264
width=1920
height=1080
pix_fmt=yuv420p
color_range=tv
color_space=bt709
color_transfer=bt709
color_primaries=bt709
duration=10.000000
size=8303440
```

**P7_MEL A.mp4** and **P7_MEL B.mp4** (identical tag lines):
```text
codec_name=h264
width=1920
height=1080
pix_fmt=yuv420p
color_range=tv
color_space=bt709
color_transfer=bt709
color_primaries=bt709
duration=10.010000
```
(sizes differ: A=305095 B=208172)

**P7_FEL A.mp4** and **P7_FEL B.mp4** (identical tags; duration=10.010000; sizes A=1544176 B=1496830): same bt709/tv/yuv420p block as P7_MEL.

**P81 A.mp4** and **P81 B.mp4** (identical tags; duration=10.009999; sizes A=8290719 B=5410525): same bt709/tv/yuv420p block.

**P84 / HDR10 A.mp4 and B.mp4:** `duration=N/A` `size=262` — not valid samples (`-ss 5` past ~2 s sources).

Colour tags do **not** distinguish A vs B; both label SDR bt709 when the mux succeeded. Green is not explained by mistagged transfer/primaries on the container.

### 2) mpv vs still (operator)

```bash
# Mac — from the scp tree
mpv --pause ~/notes/hw/libplacebo-dv-spike-2026-08-02-stills/out/P7_MEL/B.mp4
mpv --pause ~/notes/hw/libplacebo-dv-spike-2026-08-02-stills/out/P7_MEL/A.mp4
# optional: compare still
open ~/notes/hw/libplacebo-dv-spike-2026-08-02-stills/out/P7_MEL/B.jpg
```

```text
Operator mpv (Mac, gpu-next), P7_MEL:
  B.mp4: looked good ("great")
  A.mp4: green
  Both play as ~10s of essentially a still (promo art), not motion.

Contradiction — do not flatten yet:
  - Preview / agent read of A.jpg + frame from A.mp4: looked natural
  - Preview / agent read of B.jpg + frame from B.mp4: green / planar junk
  - Operator mpv: opposite (B good, A green)

Possible causes to separate later: swapped mental labels, mpv gpu-next
colour path vs Finder/ffmpeg still, or file mix-up. Re-confirm with
filename in the window title before treating either as settled.
```

## Results — first run (pipeline timings only; DV fidelity WITHDRAWN)

Realtime factor = `10 / real` where sample is valid. Stills are MJPEG `.jpg`.

| Label | A rc | A real (s) | A RT | B rc | B real (s) | B RT | Notes (non-DV claims only) |
|---|---:|---:|---:|---:|---:|---:|---|
| P5 | 187 | 1.03 | — | 0 | 24.42 | 0.41 | A: zscale “no path between colorspaces”. B timed only. |
| P7 MEL | 0 | 13.35 | 0.75 | 0 | 10.51 | 0.95 | Both muxed 10s SDR-tagged. |
| P7 FEL | 0 | 13.65 | 0.73 | 0 | 10.19 | 0.98 | Both muxed 10s; no dovi_tool BL extract used. |
| P8.1 | 0 | 33.81 | 0.30 | 0 | 27.35 | 0.37 | UHD ~60 fps → 1080p. |
| P8.4 | 0 | 0.27 | n/a | 0 | 0.13 | n/a | **invalid** — source ~2 s, `-ss 5` overshot. |
| HDR10 | 0 | 0.12 | n/a | 0 | 0.11 | n/a | **invalid** — source ~2 s, `-ss 5` overshot. |

Stills: `~/notes/hw/libplacebo-dv-spike-2026-08-02-stills/out/`.  
Mesa during measure: **22.3.6** (Intel ANV). Vulkan instance **1.3.239**, device API **1.3.230**.

## Concurrent path B (P7 MEL → 1080p)

Two back-to-back harness runs (same command). RT = `10/real`.

| Run | N | per-session real (s) | min RT | holds realtime (≥1.0)? |
|---|---:|---|---:|---|
| 1 | 1 | 10.35 | 0.97 | **no** |
| 1 | 3 | 31.04, 31.04, 31.04 | 0.32 | **no** |
| 2 | 1 | 11.09 | 0.90 | **no** |
| 2 | 3 | 30.66, 30.66, 30.66 | 0.33 | **no** |

Also from single-file table: P5 B N=1 RT **0.41** (no).

## Answers (evidence only)

### 1. Does P7 FEL work at all without `dovi_tool` extracting the base layer first?

**Withdrawn as a DV answer** (`libdovi: NO`). Pipeline-only fact retained:

```text
B.rc: 0
B.mp4: 1.5M present; B.jpg: 407K present
dovi_tool BL extract: not used
A also rc=0 on same file
```

Re-ask after libdovi-linked rebuild + HDR10 control pass.

### 2. Host with no GPU — does Mesa lavapipe present a Vulkan device, and what is its realtime factor?

Container: no `/dev/dri*`. `VK_ICD_FILENAMES=/usr/share/vulkan/icd.d/lvp_icd.x86_64.json`. Source: P7 MEL, path B, 10 s → 1080p.

```text
lavapipe ICD present:     Y (lvp_icd.x86_64.json alongside intel/radeon ICDs)
device present:           Y
deviceName:              llvmpipe (LLVM 15.0.6, 256 bits)
deviceType:              PHYSICAL_DEVICE_TYPE_CPU
driverName / Info:       llvmpipe / Mesa 22.3.6 (LLVM 15.0.6)
B.rc:                     0
B.mp4:                    289K
B real (s):               33.51
RT factor (10/real):      0.30
```

**Failure mode hit:** Vulkan device enumerates and encode exits 0, but RT **0.30** (not realtime). Same class of hazard as “detection says yes, capacity says no.”

### 3. macOS story (no native Vulkan)

```text
MoltenVK:            Homebrew molten-vk 1.4.2
vulkan-headers:      1.4.357.0
vulkan-loader:       1.4.357.0
ICD json:            /opt/homebrew/etc/vulkan/icd.d/MoltenVK_icd.json
                     (not under share/vulkan/icd.d — that path empty)
homebrew ffmpeg:     no libplacebo filter (8.1.2, libzimg yes)
Timed B encode:      not run
```

No native Vulkan on macOS. MoltenVK is installed as a Metal translation ICD. Stock Homebrew FFmpeg has no `libplacebo` filter, so this spike’s path B was not timed on the Mac.

## Build config echo (first build)

```text
configuration: --prefix=/work/prefix --enable-gpl --enable-shared --enable-libplacebo --enable-vulkan --enable-lcms2 --enable-libzimg --enable-libx264 --extra-cflags='-I/work/prefix/include -I/work/Vulkan-Headers/include' --extra-ldflags=-L/work/prefix/lib/x86_64-linux-gnu --disable-doc --disable-htmlpages
```

## Next: libdovi rebuild + valid controls (not yet run)

Gate: meson summary must show `libdovi: YES` before any re-measure. If libdovi cannot be built/linked here, stop and record that as the finding. Do not compare libplacebo vs zscale for DV until HDR10 control produces a real 10 s sample on the same build and path B is not green on that control.

### Generate longer control sources (Mac, repo root)

Corpus HDR10/P8.4 are ~2 s; use `-ss 0 -t 10` on ≥12 s files.

```bash
SPIKE_CORPUS_REMOTE=root@rm400:/mnt/user/appdata/nightjar-test/libplacebo-spike/corpus
FFMPEG="${FFMPEG:-ffmpeg}"
OUT=/tmp/nightjar-spike-controls
mkdir -p "$OUT"

# 12s HDR10 control (PQ / BT.2020)
"$FFMPEG" -y -hide_banner -loglevel error \
  -f lavfi -i "testsrc=size=1280x720:rate=24:duration=12" \
  -f lavfi -i "sine=frequency=440:sample_rate=48000:duration=12" \
  -c:v libx265 -pix_fmt yuv420p10le -tag:v hvc1 \
  -x265-params "colorprim=bt2020:transfer=smpte2084:colormatrix=bt2020nc:master-display=G(13250,34500)B(7500,3000)R(34000,16000)WP(15635,16450)L(10000000,1):max-cll=1000,400" \
  -c:a aac -ac 2 -shortest \
  "$OUT/hevc_hdr10_12s.mp4"

# 12s P8.4 if tools present (else skip and note)
DOVI_TOOL=scripts/.tools/bin/dovi_tool
# …reuse testdata/generate.sh P8.4 recipe with duration=12, or:
# concatenate six copies of the 2s fixture as a length stopgap (RPU continuity not guaranteed):
"$FFMPEG" -y -hide_banner -loglevel error \
  -stream_loop 5 -i testdata/files/hevc_dv_p84_hlg_mkv.mkv \
  -c copy -t 12 "$OUT/hevc_dv_p84_12s.mkv"

ffprobe -v error -show_entries format=duration -of default=nw=1 "$OUT/hevc_hdr10_12s.mp4"
ffprobe -v error -show_entries format=duration -of default=nw=1 "$OUT/hevc_dv_p84_12s.mkv"
scp "$OUT/hevc_hdr10_12s.mp4" "$OUT/hevc_dv_p84_12s.mkv" "$SPIKE_CORPUS_REMOTE/"
```

Re-run measure seek: `-ss 0 -t 10` (not `-ss 5`) on these controls.

### Rebuild libplacebo + FFmpeg with libdovi (host → build container)

`libplacebo` wants pkg-config `dovi >= 1.6.7`. Bookworm `rustc` is too old for current `dolby_vision`; use rustup. Pin `dovi_tool` **2.1.2** (`dolby_vision` 3.3.1, rust-version 1.79, edition 2021).

```bash
SPIKE=/mnt/user/appdata/nightjar-test/libplacebo-spike
docker rm -f libplacebo-spike-build 2>/dev/null || true

docker run --rm -it \
  --name libplacebo-spike-build \
  -v "$SPIKE/build:/work" \
  -w /work \
  debian:bookworm bash -lc '
set -euo pipefail
export DEBIAN_FRONTEND=noninteractive
apt-get update
apt-get install -y --no-install-recommends \
  ca-certificates curl git build-essential pkg-config nasm yasm \
  python3 ninja-build meson \
  libvulkan-dev libshaderc-dev liblcms2-dev libzimg-dev libx264-dev \
  glslang-dev spirv-tools \
  libssl-dev

# --- rustup (bookworm rustc cannot build dolby_vision) ---
curl https://sh.rustup.rs -sSf | sh -s -- -y --default-toolchain 1.79.0
. "$HOME/.cargo/env"
cargo install cargo-c --locked

# --- libdovi (pkg-config name: dovi) ---
if [[ ! -d dovi_tool/.git ]]; then
  git clone --depth 1 --branch 2.1.2 \
    https://github.com/quietvoid/dovi_tool.git dovi_tool
fi
cd /work/dovi_tool/dolby_vision
cargo cinstall --release --prefix=/work/prefix --features=capi
cd /work
export PKG_CONFIG_PATH=/work/prefix/lib/x86_64-linux-gnu/pkgconfig:/work/prefix/lib/pkgconfig
export LD_LIBRARY_PATH=/work/prefix/lib/x86_64-linux-gnu:/work/prefix/lib
pkg-config --modversion dovi
pkg-config --exists --print-errors "dovi >= 1.6.7" && echo DOVI_PC_OK

# --- libplacebo with libdovi ---
cd /work/libplacebo-src
rm -rf build
meson setup build --prefix=/work/prefix --buildtype=release \
  -Dvulkan=enabled -Dshaderc=disabled -Dglslang=enabled -Dlcms=enabled \
  -Ddovi=enabled -Dlibdovi=enabled \
  -Ddemos=false -Dtests=false -Dbench=false
ninja -C build
ninja -C build install
# MUST show libdovi: YES — stop if NO
meson configure build | tee /work/MESON_LIBPLACEBO.txt
grep -E "libdovi|dovi " /work/MESON_LIBPLACEBO.txt

# --- FFmpeg (same pins as before) ---
export PKG_CONFIG_PATH=/work/prefix/lib/x86_64-linux-gnu/pkgconfig:/work/prefix/lib/pkgconfig
test -f /work/prefix/lib/x86_64-linux-gnu/pkgconfig/vulkan.pc \
  || test -f /work/Vulkan-Headers/include/vulkan/vulkan.h
cd /work/ffmpeg-src
rm -f ffbuild/config.mak config.h
./configure --prefix=/work/prefix \
  --enable-gpl --enable-shared \
  --enable-libplacebo --enable-vulkan --enable-lcms2 \
  --enable-libzimg --enable-libx264 \
  --extra-cflags="-I/work/prefix/include -I/work/Vulkan-Headers/include" \
  --extra-ldflags="-L/work/prefix/lib/x86_64-linux-gnu -L/work/prefix/lib" \
  --disable-doc --disable-htmlpages
make -j"$(nproc)" && make install
export LD_LIBRARY_PATH=/work/prefix/lib:/work/prefix/lib/x86_64-linux-gnu
/work/prefix/bin/ffmpeg -version | head -5 | tee /work/FFMPEG_VERSION.txt
/work/prefix/bin/ffmpeg -hide_banner -filters | grep -E "libplacebo|zscale|tonemap"
'
```

If `cargo cinstall` or `-Dlibdovi=enabled` fails: paste the error, stop. Do not re-run DV files.

### Re-measure gate (after libdovi: YES)

1. HDR10 12s control, `-ss 0 -t 10`, paths A and B.  
2. If B is green on HDR10 → pipeline fault confirmed; stop DV comparison.  
3. Only if HDR10 B looks correct: re-run P5 / P7 MEL / P7 FEL / P8.1 / P8.4 12s with same graph.  
4. Record Mesa + Vulkan versions again beside new timings.  
5. If AMD/Ubuntu host available: same binary or same build recipe there; report both hosts.

```text
libdovi after rebuild (YES/NO/failed):
HDR10 A real / B real / B green in mpv:
```
