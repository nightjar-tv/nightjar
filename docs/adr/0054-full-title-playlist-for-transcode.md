# ADR-0054: The transcode playlist lists the whole title

- Status: **proposed**
- Date: 2026-08-23
- Supersedes: [ADR-0020](0020-copy-mode-segment-boundaries.md) §4's per-run
  window listing, for `SessionMode::Transcode` only. Copy and remux keep it.
- Depends on: [ADR-0023](0023-cluster-map-byte-offset-start.md) (keyframe map,
  byte-offset start); [ADR-0050](0050-lead-held-session-shape.md) (the session
  shape, and the seek that answers a cold URI);
  [ADR-0052](0052-keyframe-cadence-per-encode-leg.md) (the grid the listing
  asserts)
- Gate: Gate 2 — a seek into untranscoded media starts under three seconds
- Related: `nightjar-meta/notes/hw/stay-ahead-vt-2026-08-20.md` §S7, §S7b, §S7d
  (the player evidence); `docs/plans/2026-08-23-hybrid-session-shape.md` slice S3

## Context

ADR-0020 rejected a full-title playlist and set the bar for reopening it: new
player evidence. That evidence exists, and the two constraints that made the
shape unbuildable in July are gone.

**What the July probe measured.** Three shapes on AVPlayer and hls.js, each
listing a window of roughly 40 s. Seekable range tracked the listed window, and
a far seek past it either failed or clamped. Those are findings about listing a
*window*. A playlist that lists the whole title has a seekable range equal to
the title and the far seek is in range by construction. That shape was not
tested, because listing every URI means any URI may be requested, and starting
an encoder at an arbitrary media time was the open Gate 2 far-seek failure.
[ADR-0023](0023-cluster-map-byte-offset-start.md) closed that on 2026-08-01.

**What the August probe measured.** The spike origin serves exactly this shape:
every segment from 0 to title duration on the 2 s grid, `PLAYLIST-TYPE:VOD`,
`ENDLIST`, segments cooked on demand. Against real players on 2026-08-20:

| Trial | Result |
|---|---|
| AVPlayer `seekable.end` | `[0, 1200]`, the full title, with 150 of 600 segments on disk |
| AVPlayer seek 0.25 / 0.50 / 0.90 | `seek_finished=true`, landed exactly; **no clamp** |
| AVPlayer re-seek back | cache hit, no new encoder |
| iPhone Safari, human | native scrubber spans the title; seeks land forward and back; 148 of 663 on disk |
| Chrome hls.js, human | "works, very responsive"; 92 of 663 on disk, max wait 966 ms |

A sparse full-title VOD plays, and the scrubber is the title.

**Init identity across a sparse listing.** Measured on 2026-08-23, QSV, one
source, three encoder runs starting at 100 s, 700 s and 1200 s: `init.mp4` is
**byte-identical** across all three. Across ABR rungs it differs, correctly,
because SPS carries dimensions and bitrate, and each rung has its own playlist
and its own map. The 2 s grid holds on all three rungs.

## Decision

1. **A transcode session's media playlist lists every segment in the title.**
   `0` to the usable extent on the `SEGMENT_MS` grid, time-keyed
   (`seg_<ms:011>.m4s`), `#EXT-X-PLAYLIST-TYPE:VOD`, `#EXT-X-ENDLIST`. The
   listing is honest because forced IDRs put every boundary on
   `N × SEGMENT_MS`, which ADR-0020 §2 states itself.

2. **Copy and remux keep the per-run map-assembled playlist** of ADR-0020 §4.
   Copy cuts at source keyframes and cannot hold a uniform grid: on a healthy
   title, 77% of the URIs a synthetic grid listed were never written. That is a
   measurement about where copy cuts, and no keyframe map changes it. Rule 4.11
   asks which field distinguishes two cases rather than which branch; the field
   is session mode, and the reason is recorded here rather than left as an
   unexplained fork.

3. **A listed URI is never 404 and never 503.** The request is held until that
   segment lands in the store, and released then — not when the encoder that
   produced it finishes. A cold URI is a seek: the session starts an encoder at
   that media time (ADR-0050 §4), measured at a 976 to 1132 ms median and under
   2.4 s worst case. 404 is reserved for a URI outside the title or off the
   grid.

   Holding is viable because the wait is bounded by a spawn, and only because
   of that. ADR-0011 paired full-title listing with an unbounded wait and
   ADR-0020 §7 withdrew it for that reason.

4. **One `EXT-X-MAP` per rung, covering every run in that rung's session.**
   Measured identical across runs; the code already relies on this, since a
   seek that lands on mapped media copies the prior run's init.

5. **One playlist URI per session for transcode**, replacing one per run. The
   session API stays the authority on land, and clients still do not construct
   segment URLs.

## Consequences

A rung change becomes a variant switch inside a stable listing rather than a
new playlist handshake, which is what makes ADR-0051's ladder switchable.

The scrubber is the playlist again. Item duration and usable extent still
populate the product timeline, but a client seeking inside the listed range no
longer needs a new playlist URI to do it.

The segment map stays the serving authority. A listed URI is served only from
bytes the map has validated against `sidx.earliest_presentation_time`
(ADR-0020 §9).

This depends on ADR-0052. A full-title grid on an encoder that does not hold
the grid lists URIs that will never exist, which is ADR-0020's failure mode
reintroduced.

**Known UX defect, from the August probe.** On native iOS the system scrubber
snaps back to the old playhead while a seek GET is in flight, then jumps to the
target when the segment arrives. The spike held the GET for a whole cook window,
which is where the visible delay came from; decision 3 releases on the fragment
instead. Whether that removes the snap is unmeasured.

Rejected: extending the full-title listing to copy and remux, for the reason in
decision 2.
