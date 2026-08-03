# Gate 2 concurrency ceiling — NVENC (RTX 2080 SUPER)

**Date:** 2026-08-03  
**Host:** `192.168.1.33` — 12th Gen Intel i3-12100 + **GeForce RTX 2080 SUPER**  
**OS:** Ubuntu 26.04 dogfood image; **nvidia-driver-595-open** 595.84  
**Harness:** `scripts/gate2_concurrency_ceiling.py`  
**Media:** 6× synthetic 10‑min 1080p HEVC+AAC on local disk (`/tmp/nvenc-media-long`)  
**Binary:** encode-leg build (shared `EncodeLeg` / ADR-0009)  
**Session cap:** `NIGHTJAR_HLS_MAX_SESSIONS=16`  
**Realtime bar:** min stream ratio ≥ 0.90 over 40 s after 15 s warm-up  
**Preferred encoder:** `h264_nvenc` (sysmem encode leg; no preferredDevice)

Raw JSON:
`notes/hw/concurrency-ceiling-nvenc-h264_nvenc-20260803T074033Z.json`

## Results (`h264_nvenc`)

| n | minRealtimeRatio |
|---:|---:|
| 1 | 9.375 |
| 2 | 8.875 |
| 3 | 6.250 |
| 4 | 4.750 |
| 5 | 1.772 |
| 6 | 0.000 |

**lastOk = 5** (fails at n=6 with one stream at 0.0). Gate bar ≥3 **pass**.

At n=5 all five streams stayed above realtime (min 1.77×). At n=6 five streams still multi-realtime and one produced no media growth in the sample window — same failure shape as N150 QSV / AMD VAAPI concurrency notes (one zeroed stream at n=6).

## Comparison (same harness, local disk, 1080p HEVC→H.264)

| Host | Encoder | lastOk |
|---|---|---:|
| N150 | `h264_qsv` | 5 |
| AMD Renoir iGPU | `h264_vaapi` | 5 |
| **2080 SUPER** | **`h264_nvenc`** | **5** |
| Unraid RM400 (prior) | `h264_qsv` / libx264 | 5 (see earlier unraid notes) |

Single-stream headroom on NVENC is higher (n=1 ~9× vs QSV/VAAPI ~5× on the mini-PC class boxes), but the **first n where a stream falls below 0.90** landed at the same **5** concurrent 1080p floor under this harness.

## Setup notes

- Proprietary NVIDIA driver required; nouveau does not expose NVENC.
- Local disk only (same Gate 2 rule as other ceilings).
- Encode-leg for product: generic sysmem `h264_nvenc` + `yuv420p` (spike note).
