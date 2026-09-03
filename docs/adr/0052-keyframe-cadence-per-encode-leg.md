# ADR-0052: The 2 s IDR grid is per encode leg, and derived from source fps

- Status: **accepted 2026-09-03**
- Date: 2026-08-23
- Amends: [ADR-0008](0008-abr-post-v1.md) §3, which assumed one recipe holds
  the grid on every encoder
- Depends on: ADR-0009 (encode-leg detection and per-leg arguments); ADR-0020
  (the producer owns boundaries, and the map refuses entries whose media time
  disagrees)
- Gate: Gate 2 — the corpus plays on hardware encoders
- Measured 2026-08-22 on an Intel N150: `h264_qsv` ignores
  `-force_key_frames`, and a 20 s window came out as 5 segments of 4.004 s.
  Method and raw data are maintainer-private

## Context

**`h264_qsv` ignores `-force_key_frames`, and the dogfood box runs QSV.**
Measured 2026-08-23 with the product's exact arguments
(`-force_key_frames expr:gte(t,n_forced*2)`, `-g 600`, `-keyint_min 48`,
`-sc_threshold 0`) against a 23.976 fps 1080p source over a 90 s window:

```
#EXTINF:25.025000   #EXTINF:11.011000   #EXTINF:25.025000
#EXTINF:25.025000   #EXTINF:3.920583
```

Five segments of 4.7 to 6.3 MB where the grid calls for forty-five of about
500 KB. `-force_key_frames` is discarded, so `-g 600` governs, and 600 frames
at 23.976 fps is 25.025 s. With `-g 48 -forced_idr 1` the grid returns
exactly: ten segments of 2.002 s across a 20 s window.

**The product does not 404, and that is why this went unnoticed.** The bench
harness that found it builds a synthetic `N × SEGMENT_MS` playlist, so there
it shows up as URIs that never exist and GETs that wait out the full segment
timeout. The product assembles its playlist from muxer truth (ADR-0020), so
it advertises the segments the encoder actually wrote. The dogfood instance
selected `h264_qsv` at probe, has served ten transcode sessions on it, and
logged no not-ready and no not-found. ADR-0020's producer-owned boundaries
absorbed an encoder defect that has been live since at least 2026-08-13.

What the product loses is silent rather than loud. Seek granularity is a
25 s segment instead of 2 s. A first segment is 6 MB instead of 500 KB.
`TARGETDURATION` is twelve times what the design assumes, and players size
buffers from it. Most importantly, **renditions cannot align**, so the ABR
ladder that [ADR-0051](0051-abr-is-v1.md) puts in v1 cannot work on Intel at
all: ADR-0008 §3 makes identical GOP alignment the condition for a clean
switch, and there is no alignment to have.

The obvious patch is the wrong one. `48` is two seconds at 23.976 fps and
nothing else. At 60 fps it is 0.8 s, which is the exact failure Rule 4.9
names in its own text: "a frame-count `-g 48` looked like a 2-second GOP until
a 60 fps source made segments 0.8s." Hardcoding it trades an Intel bug for a
high-frame-rate bug.

The frame rate is not available to make that calculation. There is no column
on `items`, no field on `ProbeUpdate`, and nothing in `VideoEncodePlan`.

## Decision

1. **The IDR interval is derived from the source frame rate and
   `SEGMENT_MS`.** `-g` is the frame count for one segment at that source's
   rate, rounded to the nearest frame. `SEGMENT_MS` stays the single owner of
   the duration (Rule 4.9), and the frame count is computed from it rather
   than written down twice.

2. **Frame rate is stored as a rational, not a float.** Numerator and
   denominator, from ffprobe's `avg_frame_rate`. 24000/1001 must not become
   23.976, because the rounding error accumulates against a title-absolute
   grid over an hour of media. A new migration adds the pair to `items`;
   `ProbeUpdate`, the media row and `VideoEncodePlan` carry it. The migration
   and the code that reads it land in the same commit (GIT_RULES §2).

3. **Each encode leg declares how it is made to hit the grid.** ADR-0009
   already gives each leg its own arguments. The IDR cadence joins them:
   `libx264` and `h264_videotoolbox` honour `-force_key_frames`;
   `h264_qsv` needs `-g <frames> -forced_idr 1`. Legs are not assumed to
   behave alike, and a leg whose cadence has not been verified on hardware is
   not claimed to hold the grid.

4. **An unprobed or variable frame rate is resolved before the session
   encodes, not guessed.** When the stored rate is missing, the session
   probes the one video stream and writes the result back, which is Rule
   4.13's derive-on-demand shape. A source whose rate genuinely varies gets
   the grid from its average rate, and the map gate in ADR-0020 §9 remains
   the backstop: an entry whose EXTINF-derived start disagrees with the
   file's `sidx.earliest_presentation_time` is not published.

5. **The corpus grows a case per leg.** Rule 4.3. The existing
   `keyframes_align_to_segment_duration` test covers 60 fps and VFR on
   software; it runs per available leg, and QSV joins the weird-files suite
   where that hardware exists.

## Consequences

This ships before anything that depends on the grid being real. ADR-0050's
long encoder and ADR-0051's ladder both assume every rendition cuts at
`N × SEGMENT_MS`, and a full-title playlist would list URIs that never exist
on Intel until this lands.

A ladder makes the requirement stricter, not looser: identical GOP alignment
across renditions is what makes a switch clean, so every rung must hit the
same grid on whatever leg the host actually uses.

Items probed before the migration have no stored rate until they are
re-probed or a session resolves one. That is a gradual fill, not a
library-wide pass (Rule 4.13).

`-forced_idr` has been verified on QSV on the N150. NVENC and VAAPI need the
same check on hardware before either is claimed to hold the grid; until then
they inherit decision 3's rule that an unverified leg is not claimed.
