# HDR tonemap beauty inspection (2026-08-01)

**Claim class:** Proven by inspection (not a measured Gate 2 metric).
Two humans compared retag vs Nightjar `zscale`+hable stills (and one
browser play each) against the criteria below.

## Criteria

Pass when the tonemap side is not milky/washed, not green/purple cast, and
lamp/wood/sky look natural relative to the retag (broken) side.

## Passes

| Title | Path | Evidence | Result |
|---|---|---|---|
| Patterns of Nature HDR10-P8.1 FHD 24 | `testdata/files/dolby-vision-browser-kit/24fps/FHD/Patterns_Of_Nature_HDR10-P8.1_FHD_24_H265-4Mbps_DD+JOC-768Kbps.mp4` (`commit: false` kit) | `scripts/hdr_tonemap_compare.py` stills + browser play on founder Mac 2026-08-01 | Pass — tonemap side preferred |
| Patterns of Nature HLG-P8.4 FHD 24 | `testdata/files/dolby-vision-browser-kit/24fps/FHD/Patterns_Of_Nature_HLG-P8.4_FHD_24_H265-4Mbps_DD+JOC-768Kbps.mp4` (`commit: false` kit) | same harness; stills under `/tmp/nightjar-hdr-compare/20260801-212727/`; browser play 2026-08-01 | Pass — tonemap side preferred |

## Not claimed here

- DV Profile 5 beauty
- Golden-image CI / perceptual scores
- Retag-vs-tonemap MAD (`notes/hdr-tonemap-delta-2026-08-01.md`) — that is
  **not-retag only**, not beauty
