# Gate 2 concurrency ceiling — AMD Renoir (local disk)

**Date:** 2026-08-03  
**Host:** `nightjar-dogfood` / `192.168.1.184` — AMD Ryzen 5 5500U (Lucienne / Renoir iGPU), 6.6 GiB RAM  
**Harness:** `scripts/gate2_concurrency_ceiling.py`  
**Media:** 6× synthetic 10‑min 1080p HEVC+AAC on local disk (`~/gate2/media`)  
**Binary:** product release with **VAAPI session-spawn fix** (device + `format=nv12,hwupload`; see below)  
**Session cap:** `NIGHTJAR_HLS_MAX_SESSIONS=12`  
**Realtime bar:** min stream ratio ≥ 0.90 over 40 s after 15 s warm-up  

Raw JSON: `notes/hw/concurrency-ceiling-amd-h264_vaapi-20260803T061851Z.json`  
(earlier fail run `…061007Z` is pre-fix — ignore)

## Results (`h264_vaapi`)

| n | minRealtimeRatio |
|---:|---:|
| 1 | 5.45 |
| 2 | 2.90 |
| 3 | 1.90 |
| 4 | 1.45 |
| 5 | 1.15 |
| 6 | 0.00 |

**lastOk = 5** (fails at n=6). Gate bar ≥3 **pass**.

## Product bug fixed for this measure

Startup **verified** `h264_vaapi`, but an earlier dual path let HLS session spawn use the software graph (`-pix_fmt yuv420p` + SDR retag) **without** `-vaapi_device` / `hwupload`. FFmpeg exited **218**; playlist never grew.

**Superseded by shared `EncodeLeg` (ADR-0009):** probe and session both use
`push_pre_input` + `compose_video_filter` + `push_encoder_args`. Device comes
from the probed leg (render-node try-list), not a session-only `is_vaapi`
branch. See `notes/hw/encode-leg-argv-walk-2026-08-03.md`.

## Stack notes

- Mesa radeonsi VA driver; H.264 EncSlice present.  
- Preferred after probe: `h264_vaapi`.  
- Dogfood left on `:8096` with this binary.
