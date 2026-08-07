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
| QSV | `h264_qsv` | Linux, Windows (Intel) | 1 | Verified on household Unraid RM400 (UHD 770) and Intel N150 host FFmpeg + oneVPL (`libmfx-gen`). Sysmem encode leg (no hwupload). Concurrent 1080p floor lastOk 5 on both (`nightjar-meta/notes/hw/concurrency-ceiling-n150-2026-08-03.md`; Unraid prior note in nightjar-meta). Needs the iGPU enabled in BIOS when a discrete GPU is also present. Raw FFmpeg also encodes on Arc A380 via device pin; product DRM picker is Phase 3. **Docker image QSV is not a tier claim** — the product image installs Intel VA packages, not a proven oneVPL story |
| VAAPI | `h264_vaapi` | Linux (Intel/AMD) | 1 | Verified on Unraid (`renderD128` + hwupload) and AMD Renoir iGPU host FFmpeg (encode-leg session dogfood; concurrency lastOk 5 in `nightjar-meta/notes/hw/concurrency-ceiling-amd-2026-08-03.md`). Probe tries `/dev/dri/renderD*` and records the winning path as `preferredDevice` on `GET /api/v0/system/transcode`. Containers without `/dev/dri` passthrough correctly fail probe |
| NVENC | `h264_nvenc` | Linux, Windows | 1 | Verified 2026-08-03 on team RTX 2080 SUPER host: startup preferred `h264_nvenc`, live encode-leg sessions, concurrency lastOk 5 (`nightjar-meta/notes/hw/concurrency-ceiling-nvenc-2026-08-03.md`). Sysmem leg (`yuv420p`); no device field |
| Media Foundation | `h264_mf` | Windows | 2 | Candidate on Windows builds only |
| V4L2 M2M | `h264_v4l2m2m` | Linux (Pi 4 and similar) | 2 | H.264 only, roughly a 1080p ceiling; Pi is weak for transcode |
| RKMPP and similar | (varies) | Rockchip SBCs | 3 | Not in the startup candidate list yet |

Gate 2 required at least one VAAPI machine and one QSV or NVENC machine in
tier 1; that bar is met (VideoToolbox + software + Unraid QSV/VAAPI + N150
QSV + AMD VAAPI + NVENC 2080 SUPER). Host-binary claims use operator FFmpeg
on PATH (dogfood often jellyfin-ffmpeg or distro FFmpeg 8.x).

**Product Docker image packaging is still open for HW encode.** The shipped
image is Debian bookworm `ffmpeg` + VA driver packages. On N150, that stack
failed to init VA while host Ubuntu FFmpeg succeeded
(`nightjar-meta/notes/hw/encode-leg-spike-2026-08-03.md`). Do not claim product-image HW
encode on N150 from current evidence. Re-verify on a host where bookworm VA
actually inits (e.g. Unraid with `--device=/dev/dri`) before any image HW
tier language. Bare binary still expects an operator-provided FFmpeg.

Remaining hardware poles for Gate 2 sizing included Intel N100/N150 and Pi 4
(ADR-0005 scan carry). SMB or other remote-share runs are storage-admission
observations, not the encoder-ceiling number
(`nightjar-meta/notes/spike-smb-gate-2026-08-02.md`). Arc as a pinned QSV device is a
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
