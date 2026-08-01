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
| QSV | `h264_qsv` | Linux, Windows (Intel) | 1 | Verified 2026-08-01 on household Unraid (RM400): Raptor Lake UHD 770 iGPU, Jellyfin FFmpeg 7.1.4, live session opened `/dev/dri/renderD128`. Raw FFmpeg also encodes on Arc A380 via `-qsv_device /dev/dri/renderD129` and `init_hw_device` child-VAAPI (see `nightjar-meta/notes/hw/unraid-arc-pin-2026-08.md`). Nightjar session-on-Arc still needs a device setting or PATH-fixed remap dogfood |
| VAAPI | `h264_vaapi` | Linux (Intel/AMD) | 1 | Verified same Unraid run (startup encode+demux on iGPU). Raw FFmpeg VAAPI on Arc `renderD129` timed equal/faster than iGPU on the pin matrix. Product default still binds `renderD128` |
| Media Foundation | `h264_mf` | Windows | 2 | Candidate on Windows builds only |
| V4L2 M2M | `h264_v4l2m2m` | Linux (Pi 4 and similar) | 2 | H.264 only, roughly a 1080p ceiling; Pi is weak for transcode |
| RKMPP and similar | (varies) | Rockchip SBCs | 3 | Not in the startup candidate list yet |

Gate 2 still requires at least one VAAPI machine and one QSV or NVENC machine in
tier 1 before the gate can close. **Unraid RM400 now covers VAAPI + QSV on the
iGPU** (`renderD128` / UHD 770). VideoToolbox + software remain tier 1 on Mac.
Arc A380 (`renderD129`) was present but did not carry the default session; pin
results land in measurement notes when run. The stock Nightjar Docker image
does not yet ship an FFmpeg with these paths — Unraid dogfood borrowed
Jellyfin’s image (packaging gap tracked in
`nightjar-meta/notes/hw/packaging-ffmpeg-image.md`). Remaining hardware for
Gate 2 / Phase 2 entry: Intel N100/N150 (concurrent 1080p capacity) and Pi 4
(ADR-0005 scan carry).

## What detection reports

On a MacBook with a working VideoToolbox path, expect preferred
`h264_videotoolbox`, `libx264` verified, and Linux/Windows-only backends
`unavailable`. On a container without device passthrough, expect preferred
`libx264` and hardware candidates `failed` or `unavailable` with reasons.

HEVC hardware encode and decode `-hwaccel` are not part of this matrix yet.
