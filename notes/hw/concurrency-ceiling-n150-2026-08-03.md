# Gate 2 concurrency ceiling — Intel N150 (local disk)

**Date:** 2026-08-03  
**Host:** `nightjar-dev` / `192.168.1.183` — Intel N150 (4C/4T), 6.9 GiB RAM, NVMe root  
**Harness:** `scripts/gate2_concurrency_ceiling.py`  
**Media:** 6× synthetic 10‑min 1080p HEVC+AAC on local NVMe  
(`~/gate2/media/hevc1080_*.mp4`, ~296 MiB each). **Not** SMB.  
**Binary:** `dist/unraid-test/nightjar` (x86_64), host FFmpeg 8.0.1 (QSV path);  
Debian bookworm container FFmpeg without `/dev/dri` (libx264 path).  
**Session cap:** `NIGHTJAR_HLS_MAX_SESSIONS=12`  
**Realtime bar:** min stream ratio ≥ 0.90 over 40 s after 15 s warm-up.

Raw JSON:

- `notes/hw/concurrency-ceiling-n150-h264_qsv-20260803T045437Z.json`
- `notes/hw/concurrency-ceiling-n150-libx264-20260803T050154Z.json`

## Results

| Encoder | Preferred | lastOk (all ≥0.90) | nFail | Limit |
|---|---|---:|---:|---|
| `h264_qsv` | yes (host) | **5** | 6 | one stream 0.0 at n=6 |
| `libx264` | container, no DRI | **2** | 3 | min ratio 0.85 at n=3 |

### QSV ratios (min across streams)

| n | minRealtimeRatio |
|---:|---:|
| 1 | 5.25 |
| 2 | 2.75 |
| 3 | 1.75 |
| 4 | 1.25 |
| 5 | 1.00 |
| 6 | 0.00 |

### libx264 ratios

| n | minRealtimeRatio |
|---:|---:|
| 1 | 2.55 |
| 2 | 1.30 |
| 3 | 0.85 |

## Gate 2 checklist

V1_PLAN / ADR-0022 §7: **≥3 simultaneous 1080p transcodes on mini-PC-class N100/N150, local disk, no stutter.**

- **HW path (QSV): pass** — lastOk **5** (≥3 with headroom).  
- Software-only on this 4‑core / 7 GiB box does **not** hold 3 at the 0.90 bar (lastOk 2). That is a capacity note, not the gate encoder claim.

Default `NIGHTJAR_HLS_MAX_SESSIONS=3` remains aligned with the gate figure; measured HW floor is higher.

## Setup notes (for re-run)

1. QSV needs **both** `intel-media-va-driver-non-free` **and** `libmfx-gen1.2` (oneVPL GPU runtime). Dispatcher alone (`libvpl2`) yields MFX session **-9** (`MFX_ERR_NOT_FOUND`). VAAPI works without the runtime; product prefer order is QSV first.  
2. User in `render` + `video` for bare-binary host FFmpeg.  
3. Do not use `//192.168.1.2/media` for this number — storage observation, not encoder ceiling.

## Still open (not this host)

- Pi 4 scan carry (ADR-0005)  
- Unraid packaging re-verify against product Docker image  
- 48 h orphan soak  
- AMD VAAPI diversity host  
