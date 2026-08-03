# Encode-leg argv walk — probe vs HLS Transcode session

**Date:** 2026-08-03  
**Code:** `server/crates/transcode/src/hwaccel.rs` (`verify_encode_leg`),
`server/crates/transcode/src/hls.rs` (`spawn_ffmpeg` Transcode path).

Purpose: show that probe and session share one encode-leg contract, and that
the only intentional differences are input/output shape and software prefilters
outside the leg (scale / tonemap / burn), not a second VAAPI path.

## Shared encode-leg pieces

For a preferred leg `L`, both paths call:

1. `L.push_pre_input(cmd)` — before any `-i` (e.g. `-vaapi_device PATH`)
2. `L.compose_video_filter(software)` → optional `-vf`
3. `L.push_encoder_args(cmd)` — `-c:v`, optional `-pix_fmt`, `encoder_extra`

No global non-x264 `-pix_fmt yuv420p` remains on the Transcode spawn path.

## Software prefilter

| Path | Software chain fed to `compose_video_filter` |
|---|---|
| Probe | Fixed stub `format=yuv420p` (`PROBE_SOFTWARE_CHAIN`) |
| Session (SDR, no scale) | `sidedata=delete,setparams=…bt709` (`SDR_RETAG_CHAIN`) |
| Session (HDR tonemap) | zscale/tonemap chain **ending** `format=yuv420p` |
| Session (height cap) | `scale=-2:'min(H,ih)'` + retag or tonemap |

Probe deliberately includes the **surface-changing** tail (`format=yuv420p`) so
VAAPI verify is not upload-only. Session SDR retag does not reformat pixels;
HDR and some scale graphs do. Compose for VAAPI:

```text
probe:   format=yuv420p,format=nv12,hwupload
session: <software…>,format=nv12,hwupload
```

## Backend argv sketches

Placeholders: `{in}` = media path; `{device}` = probed render node;
`{sw}` = session software chain; segment flags abbreviated as `…hls…`.

### libx264 (software)

**Probe**

```text
ffmpeg -nostdin -hide_banner -loglevel error -y
  -f lavfi -i testsrc=size=320x240:rate=24:duration=2
  -f lavfi -i sine=frequency=440:duration=2
  -vf format=yuv420p
  -c:v libx264 -pix_fmt yuv420p -preset veryfast
  -c:a aac -ac 2 -shortest probe.mp4
```

**Session (Transcode, no burn)**

```text
ffmpeg -nostdin -hide_banner -loglevel error -y
  [-ss …] -i {in} [-output_ts_offset …]
  -map 0:v:0 -map 0:a:…
  -c:v libx264 -pix_fmt yuv420p -preset veryfast
  -map_metadata -1 -vf {sw}
  -colorspace bt709 -color_primaries bt709 -color_trc bt709
  -c:a aac … …hls…
```

### h264_qsv (sysmem)

**Probe**

```text
… lavfi inputs …
  -vf format=yuv420p
  -c:v h264_qsv -pix_fmt nv12
  -c:a aac -ac 2 -shortest probe.mp4
```

**Session**

```text
… -i {in} …
  -c:v h264_qsv -pix_fmt nv12
  -map_metadata -1 -vf {sw}
  -colorspace bt709 … …hls…
```

No pre-input; no `upload_vf`.

### h264_vaapi (device + upload)

**Probe** (device from `list_render_nodes` try-list; first verified wins)

```text
ffmpeg …
  -vaapi_device {device}
  -f lavfi -i testsrc=…
  -f lavfi -i sine=…
  -vf format=yuv420p,format=nv12,hwupload
  -c:v h264_vaapi
  -c:a aac -ac 2 -shortest probe.mp4
```

No software `-pix_fmt` (leg `pix_fmt` is `None`).

**Session**

```text
ffmpeg …
  -vaapi_device {device}
  -i {in} …
  -c:v h264_vaapi
  -map_metadata -1 -vf {sw},format=nv12,hwupload
  -colorspace bt709 … …hls…
```

`preferredDevice` on the capabilities API is `{device}`.

### h264_nvenc / generic HW (sysmem)

**Probe**

```text
… lavfi inputs …
  -vf format=yuv420p
  -c:v h264_nvenc -pix_fmt yuv420p
  -c:a aac -ac 2 -shortest probe.mp4
```

**Session**

```text
… -i {in} …
  -c:v h264_nvenc -pix_fmt yuv420p
  -map_metadata -1 -vf {sw}
  -colorspace bt709 … …hls…
```

No pre-input; `preferredDevice` null. Measured on RTX 2080 SUPER
(`notes/hw/concurrency-ceiling-nvenc-2026-08-03.md`).

### h264_videotoolbox

Same shape as NVENC generic: no pre-input, `-pix_fmt yuv420p`, software `{sw}`
or probe stub `format=yuv420p`.

## Name-only `EncodeLeg::from`

Tests may rebuild a leg from an encoder **name**. That path hardcodes VAAPI
`renderD128` and must not feed production sessions. Product wiring clones
`preferred_encode_leg` from startup probe (`api` main → session manager).

## What this walk does not claim

- Full HLS flag parity on probe (probe writes a short mp4 + demux check).
- Product Docker image FFmpeg/driver parity (see packaging notes in the spike).
- Decode `-hwaccel` or HW scale (out of first encode-leg field budget).
