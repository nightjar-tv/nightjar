# HDR tonemap vs retag pixel delta (2026-08-01)

**Claim of this note:** measure only. The regression floor proves
**not-retag**, not correct tonemap beauty (ADR-0022 / Rule 4.11).

## Method

- Source: `testdata/files/hevc_hdr10_mp4.mp4` (committed synthetic HDR10)
- Seek: `-ss 1`
- One frame each, converted to packed `rgb24` (1280×720 → 2 764 800 bytes)
- Retag chain (old bug):
  `sidedata=delete,setparams=color_primaries=bt709:color_trc=bt709:colorspace=bt709`
- Tonemap chain (Nightjar / `HDR_TONEMAP_CHAIN` in `hls.rs`):
  `zscale=t=linear:npl=100,format=gbrpf32le,zscale=p=bt709,tonemap=tonemap=hable:desat=0,zscale=t=bt709:m=bt709:r=tv,format=yuv420p,sidedata=delete`
- Host FFmpeg: 8.1.2 with `--enable-libzimg` (`zscale` present)

MAD = mean over bytes of `|retag[i] - tonemap[i]|` on the raw rgb24 buffers.

## Result

| metric | value |
|---|---:|
| bytes | 2 764 800 |
| sum abs | 30 629 582 |
| **MAD** | **11.078** |
| bytes differing | 51.73% |

## Floor for `tonemap_frame_differs_from_retag`

Use **MAD ≥ 5.0** (about half the measured delta). Room for encoder /
filter micro-variance; still far above a no-op / identical-frame case
(MAD ≈ 0).

A broken green/purple zscale chain can still clear this floor. That is
accepted: one test, one claim — not beauty.
