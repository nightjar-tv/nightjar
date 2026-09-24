# Hardware acceleration support matrix

Published for Gate 2 (ADR-0009). Tiers are claims about what the team has
verified on real hardware, not what FFmpeg advertises on a given machine.

Runtime truth for one process is `GET /api/v0/system/transcode` (detection by
verification at startup). This page is the support commitment.

## Tiers

| Tier | Meaning |
|---|---|
| 1 | Verified by the team on real hardware we run. Expected to work; bugs are ours. |
| 2 | Implemented and expected to work; not yet verified by us on our own machines. |
| 3 | Community-reported or SBC-class paths. Best-effort; no team verification. |

Software `libx264` is always tier 1: every supported host can fall back to it.

## Encode backends (H.264)

| Backend | FFmpeg encoder | Platforms | Tier | Notes |
|---|---|---|---|---|
| Software | `libx264` | all | 1 | Always probed; always the fallback |
| VideoToolbox | `h264_videotoolbox` | macOS (Apple Silicon and Intel) | 1 | Verified on team Mac hardware. Fast and power-efficient; encode quality at low bitrates trails x264 (preference policy in ADR-0009 prefers it for throughput until quality tuning lands) |
| QSV | `h264_qsv` | Linux, Windows (Intel) | 1 | Verified on household Unraid RM400 (UHD 770) and Intel N150 host FFmpeg + oneVPL (`libmfx-gen`). Sysmem encode leg (no hwupload). The N150 sustained five concurrent 1080p encodes on 2026-08-03; the Unraid host also reached five. Needs the iGPU enabled in BIOS when a discrete GPU is also present. Raw FFmpeg also encodes on Arc A380 via device pin; product DRM picker is Phase 3. Container evidence is described below. |
| VAAPI | `h264_vaapi` | Linux (Intel/AMD) | 1 | Verified on Unraid (`renderD128` + hwupload) and AMD Renoir iGPU host FFmpeg (encode-leg session dogfood; concurrency lastOk 5 in the concurrency ceiling AMD note, 2026-08-03). Probe tries `/dev/dri/renderD*` and records the winning path as `preferredDevice` on `GET /api/v0/system/transcode`. Containers without `/dev/dri` passthrough correctly fail probe |
| NVENC | `h264_nvenc` | Linux, Windows | 1 | Verified 2026-08-03 on team RTX 2080 SUPER host: startup preferred `h264_nvenc`, live encode-leg sessions, concurrency lastOk 5 (the concurrency ceiling NVENC note, 2026-08-03). Sysmem leg (`yuv420p`); no device field |
| Media Foundation | `h264_mf` | Windows | 2 | Candidate on Windows builds only |
| V4L2 M2M | `h264_v4l2m2m` | Linux (Pi 4 and similar) | 2 | H.264 only, roughly a 1080p ceiling; Pi is weak for transcode |
| RKMPP and similar | (varies) | Rockchip SBCs | 3 | Not in the startup candidate list yet |

Gate 2 required at least one VAAPI machine and one QSV or NVENC machine in
tier 1; that bar is met (VideoToolbox + software + Unraid QSV/VAAPI + N150
QSV + AMD VAAPI + NVENC 2080 SUPER). Host-binary claims use operator FFmpeg
on PATH (dogfood often jellyfin-ffmpeg or distro FFmpeg 8.x).

**Product Docker image evidence is limited to Intel N150.** The Debian trixie
image contains FFmpeg 7.1.5, the non-free Intel media driver, oneVPL
(`libmfx-gen1.2`), Mesa VA drivers and the i965 VA driver. On 2026-09-25,
the image built from this Dockerfile verified QSV, VAAPI and software with
`--device=/dev/dri` and selected QSV; without that device it selected
`libx264`. The QSV image played a real H.264/DTS title in a browser. One
far seek to 7000 seconds resumed in 2.95 seconds; a clean audio-track switch
at the start took 7.43 seconds. This proves an image-backed QSV session on
that N150, but does not close the playback latency work or establish Docker
hardware support on other Intel, AMD or Nvidia hosts. Bare binary still
expects an operator-provided FFmpeg.

For a Linux container, Intel and AMD VAAPI need the host render device passed
as `/dev/dri` and permission to open it. Intel QSV also needs a compatible
host iGPU and the image's oneVPL runtime. Nvidia NVENC needs the host's
[NVIDIA Container Toolkit](https://docs.nvidia.com/datacenter/cloud-native/container-toolkit/latest/docker-specialized.html),
GPU passthrough and the `video` driver capability; the image does not bundle
the host Nvidia driver. The startup API reports which encoder actually passed
verification on each install. With no usable GPU it selects `libx264`.
AMD and Nvidia have not been verified in this Docker image. VideoToolbox and
Windows encoders apply to native server processes, not this Linux image.

Remaining hardware poles for Gate 2 sizing included Intel N100/N150 and Pi 4
(ADR-0005 scan carry). SMB or other remote-share runs are storage-admission
observations, not the encoder-ceiling number
(the spike SMB gate note, 2026-08-02). Arc as a pinned QSV device is a
Phase 3 product choice, not a matrix tier gap.

## What detection reports

On a MacBook with a working VideoToolbox path, expect preferred
`h264_videotoolbox`, `libx264` verified, and Linux/Windows-only backends
`unavailable`. On a container without device passthrough, expect preferred
`libx264` and hardware candidates `failed` or `unavailable` with reasons.
On the Unraid verify host with `/dev/dri`, expect preferred `h264_qsv`,
`h264_vaapi` verified (with `preferredDevice` when VAAPI wins), and
`libx264` verified. On the NVENC dogfood box, expect preferred `h264_nvenc`
and null `preferredDevice`.

HEVC hardware encode and decode `-hwaccel` are not part of this matrix yet.
