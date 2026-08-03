# UHD Mac Videotoolbox buffering (2026-08-02)

**Claim class:** measured on founder Mac (2026-08-02), plus dogfood. Not a
colour/tonemap beauty claim.

## Symptom

Product HLS web player on founder Mac (`h264_videotoolbox`): UHD MakeMKV titles
(P7 MEL, P7 FEL, P8.1) buffer about every 1–2 seconds when the encode cannot
stay ahead of realtime. Picture can still look correct.

## Bottleneck

Browser Transcode path for HDR→SDR:

software (or full-res) HEVC 10-bit decode → **CPU `zscale` float tonemap** →
`h264_videotoolbox` encode.

`BROWSER_V0` originally had no `maxHeight`, so Auto kept 3840×2160 through
tonemap. P81 is also ~60 fps (`19001/317`), which multiplies filter cost.

## Measured cook rates (10s media, product tonemap chain + VT encode)

| Graph | Title | Realtime |
|---|---|---:|
| 4K tonemap (no scale) | P7 MEL (~24fps) | 0.74× |
| 1080 scale → tonemap | P7 MEL | 1.6× |
| 4K tonemap | P81 (~60fps) | 0.30× |
| 1080 scale → tonemap | P81 | 0.26× |
| 720 scale → tonemap | P81 | 0.75× |
| VT `-hwaccel` + 1080 tonemap | P81 | 0.83× |
| VT `-hwaccel` + 1080 + `fps=30` → tonemap | P81 | **1.5×** |
| VT `-hwaccel` + 1080 + `fps=30` → tonemap | P7 MEL | **2.0×** |

Decode-only null sink on P81: software ~1.0×, `-hwaccel videotoolbox` ~2.1×.

## What smoother playback needs (not shipped here)

For founder-Mac browser Auto on UHD HDR→SDR:

1. **Scale before tonemap** (e.g. `maxHeight` 1080 on `BROWSER_V0`) — enough
   for ~24fps UHD (P7 MEL/FEL dogfood).
2. **Plus** for ~60fps UHD (P81): **Videotoolbox decode** (`-hwaccel
   videotoolbox`) and an **output frame-rate cap** (~30 fps) on the Auto
   ladder. 1080 alone does not fix P81.

Do not treat “skip tonemap” as the fix for these titles; they need a cheaper
SDR encode graph, not retag-of-PQ.

## Product follow-up

Decide in an ADR / profile slice whether browser Auto permanently carries
`maxHeight` (and optionally max fps / decode hwaccel policy), versus a
quality picker. Spike graphs above are the evidence floor.
