# SMB / hard-path gate for cook/release (2026-08-02)

Harnesses: `scripts/spike_restart_cost.py`, `scripts/spike_session_contention.py`  
Machine: Mac → `/Volumes/media` SMB (same dogfood link as far-seek notes).

## Restart budget (SMB)

| Source | borrow restart → first seg p50 | Notes |
|---|---:|---|
| Local synthetic (earlier) | ~0.6–1.2 s | optimistic |
| **Up Bluray-1080p SMB** | **~8.1 s** (range ~1.8–8+) | cold start ~3–7 s |
| **HDR10 Patterns SMB** | **~5.2 s** | lighter file |

JSON: `spike-restart-cost-smb-up.json`, `spike-restart-cost-smb-hdr10.json`.

Lead must be sized for ~8–10 s SMB restarts, not local VT.

## Single-client cook/release on SMB

| Run | Finished | Rebuffer | Releases | Encoder-ms |
|---|---:|---:|---:|---:|
| Up Bluray VT borrow@1 60s | 1/1 | **4.2 s** | 1 | 55 s |
| Up WEBDL VT borrow@1 60s | 1/1 | **0** | 3 | 21 s |
| HDR10 VT borrow@1 45s | 1/1 | **0** | 2 | 21 s |
| Up WEBDL **libx264** borrow@1 45s | 1/1 | **38.7 s** | 0 | 98 s |

Cook/release **works** on SMB when encode ≥ realtime (WEBDL/HDR VT).  
Fails the zero-rebuffer bar when encode &lt; realtime (Bluray dual-stress, software x264).

## Multi-client (the “6 vs 3” question on SMB)

| Run | Finished | Peak | Rebuffer | Releases | Verdict |
|---|---:|---:|---:|---:|---|
| owning@6 vs borrow@3, Up Bluray, 90s | 0/6 both | 6 / 3 | huge | 0 | **NAS thrash** — 6 concurrent SMB encodes not viable |
| owning@3 vs borrow@3, Up Bluray, 90s | 0/6 both | 3 / 3 | huge | 0 | 3× Bluray still &lt; realtime |
| owning@3 vs borrow@3, Up WEBDL, 90s | 1/6 both | 3 / 3 | huge | 0 | still saturated |
| **owning@2 vs borrow@2, Up WEBDL, 75s** | owning **0/2**, borrow **2/2** | 2 / **1** | borrow **0** | **6** | **SAVE 85.8%** encoder-ms |

JSON: `spike-owning2-vs-borrow2-smb-webdl.json` (the clean SMB multi-client win).

## What this means

1. **Local “no-brainer” does not automatically transfer to SMB** at 3–6 concurrent remuxes. The share becomes the bottleneck; lead never builds → no release → no borrow benefit.
2. **Where encode can stay ahead** (WEBDL, lighter titles, fewer concurrent readers), borrow still wins hard: same finishes, zero rebuffer, ~86% less encoder-ms, peak 1 instead of 2.
3. **Admission must track storage, not only GPU slots.** A “3 transcode” cap that ignores SMB read concurrency will look like a borrow failure when it is really a media-link failure.
4. **libx264 over SMB** could not build lead in 45s — slow encode must not release (or must use a much larger lead / refuse borrow).

## Recommended product posture

- Keep cook/release as the architecture.
- Gate concurrent **source reads** (or measure media_read_mbps) separately from encoder slots — aligns with `MACHINE_PROFILE.md`.
- Size `LEAD` / restart budget from SMB p95 (~8 s on Bluray), not local.
- Dogfood success case: WEBDL-class + ≤2 concurrent readers on this Wi‑Fi SMB path; Unraid/local disk will look more like the local SAVE run.

## Not run (still open)

- Real hls.js client over borrow on SMB (sim is 1× perfect).
- Unraid docker path (encode next to the disks — should restore local-like SAVE).
- Full 45‑min episode wall-clock (not needed once lead math + 75 s multi-client holds).
