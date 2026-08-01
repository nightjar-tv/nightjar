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
| QSV | `h264_qsv` | Linux, Windows (Intel) | 1 | Verified 2026-08-01 on household Unraid RM400 (UHD 770, `/dev/dri/renderD128`). Preferred on that box; live session used QSV. Concurrent 1080p floor: 5 (`nightjar-meta/notes/hw/concurrency-ceiling-unraid.md`). Needs the iGPU enabled in BIOS when a discrete GPU is also present. Raw FFmpeg also encodes on Arc A380 via device pin (`nightjar-meta/notes/hw/unraid-arc-pin-2026-08.md`); product DRM picker is Phase 3 |
| VAAPI | `h264_vaapi` | Linux (Intel/AMD) | 1 | Verified same Unraid host (`renderD128` + hwupload). Containers without `/dev/dri` passthrough correctly fail probe. Raw FFmpeg VAAPI on Arc `renderD129` also timed OK; default verify path remains `renderD128` until a device setting lands |
| NVENC | `h264_nvenc` | Linux, Windows | 2 | Implemented in the candidate list; needs a real Nvidia box for tier 1 |
| Media Foundation | `h264_mf` | Windows | 2 | Candidate on Windows builds only |
| V4L2 M2M | `h264_v4l2m2m` | Linux (Pi 4 and similar) | 2 | H.264 only, roughly a 1080p ceiling; Pi is weak for transcode |
| RKMPP and similar | (varies) | Rockchip SBCs | 3 | Not in the startup candidate list yet |

Gate 2 required at least one VAAPI machine and one QSV or NVENC machine in
tier 1; that bar is met (VideoToolbox + software + Unraid QSV/VAAPI). Dogfood
used jellyfin-ffmpeg on PATH; the shipped Docker image uses Debian `ffmpeg` +
VA drivers — re-verify on Unraid with that image. Pass `--device=/dev/dri` for
hardware. Bare binary still expects an operator-provided FFmpeg. NVENC remains
tier 2 until a team Nvidia box is verified. Remaining hardware poles for Gate 2
sizing: Intel N100/N150 (concurrent 1080p capacity) and Pi 4 (ADR-0005 scan
carry). Arc as a pinned QSV device is a Phase 3 product choice
(`nightjar-meta/notes/design/drm-device-selection.md`), not a matrix tier gap.
Concurrency floors on RM400: 5×1080p QSV and 5×1080p libx264 realtime
(`nightjar-meta/notes/hw/concurrency-ceiling-unraid.md`).

## What detection reports

On a MacBook with a working VideoToolbox path, expect preferred
`h264_videotoolbox`, `libx264` verified, and Linux/Windows-only backends
`unavailable`. On a container without device passthrough, expect preferred
`libx264` and hardware candidates `failed` or `unavailable` with reasons.
On the Unraid verify host with `/dev/dri`, expect preferred `h264_qsv`,
`h264_vaapi` verified, and `libx264` verified.

HEVC hardware encode and decode `-hwaccel` are not part of this matrix yet.
