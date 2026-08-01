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
| NVENC | `h264_nvenc` | Linux, Windows | 2 | Implemented in the candidate list; needs a real Nvidia box for tier 1 |
| QSV | `h264_qsv` | Linux, Windows (Intel) | 1 | Verified 2026-08-01 on household Unraid (RM400): Raptor Lake UHD 770 iGPU, jellyfin-ffmpeg, live session opened `/dev/dri/renderD128`. Raw FFmpeg also encodes on Arc A380 via device pin (see `nightjar-meta/notes/hw/unraid-arc-pin-2026-08.md`). Product DRM picker is Phase 3 |
| VAAPI | `h264_vaapi` | Linux (Intel/AMD) | 1 | Verified same Unraid run (startup encode+demux on iGPU). Raw FFmpeg VAAPI on Arc `renderD129` also timed OK. Default verify path remains `renderD128` until a device setting lands |
| Media Foundation | `h264_mf` | Windows | 2 | Candidate on Windows builds only |
| V4L2 M2M | `h264_v4l2m2m` | Linux (Pi 4 and similar) | 2 | H.264 only, roughly a 1080p ceiling; Pi is weak for transcode |
| RKMPP and similar | (varies) | Rockchip SBCs | 3 | Not in the startup candidate list yet |

Gate 2 still requires at least one VAAPI machine and one QSV or NVENC machine in
tier 1 before the gate can close. **Unraid RM400 covers VAAPI + QSV on the
iGPU** (dogfood used jellyfin-ffmpeg on PATH; the shipped Docker image uses
Debian `ffmpeg` + VA drivers — re-verify on Unraid with that image). VideoToolbox
+ software remain tier 1 on Mac. Pass `--device=/dev/dri` for hardware. Bare
binary still expects an operator-provided FFmpeg. Remaining hardware for Gate 2
sizing: Intel N150/N100 (concurrent 1080p) and Pi 4 (ADR-0005 scan carry).
Concurrency floors on RM400: 5×1080p QSV and 5×1080p libx264 realtime
(`nightjar-meta/notes/hw/concurrency-ceiling-unraid.md`).

## What detection reports

On a MacBook with a working VideoToolbox path, expect preferred
`h264_videotoolbox`, `libx264` verified, and Linux/Windows-only backends
`unavailable`. On a container without device passthrough, expect preferred
`libx264` and hardware candidates `failed` or `unavailable` with reasons.

HEVC hardware encode and decode `-hwaccel` are not part of this matrix yet.
