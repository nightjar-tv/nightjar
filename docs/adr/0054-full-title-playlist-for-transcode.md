# ADR-0054: The transcode playlist lists the whole title

- Status: **proposed** (decisions 1 and 2 corrected 2026-08-31 by measurement;
  decision 3 now names what it overturns; **decision 4 overturned 2026-08-31**,
  which bounds decision 5; see each in place)
- Date: 2026-08-23
- Supersedes: [ADR-0020](0020-copy-mode-segment-boundaries.md) §4's per-run
  window listing, **in every mode** — corrected 2026-08-31; it read
  "`SessionMode::Transcode` only, copy and remux keep it" until copy's own
  listing was measured. Also supersedes ADR-0020's **miss policy**: see
  decision 3.
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

   > **Corrected 2026-08-31 — the boundaries are frame-quantised, not
   > `N × SEGMENT_MS`.** The sentence above is what this decision claimed and
   > it is false. **An IDR can only be placed on a frame**, so
   > `-force_key_frames` picks the nearest one; it does not create a frame at
   > 2.000 s. `SEGMENT_MS` is only a boundary when the source rate divides it
   > into whole frames.
   >
   > Measured at `c43b440` on the N150, `h264_qsv`, 1080p h264: **1061 of 1062
   > distinct segment starts were off the 2000 ms grid**, modal consecutive
   > delta **2002 ms** across 923 pairs. Measured again through the transcode
   > start path on `libx264`, which *does* honour `-force_key_frames`:
   > `24000/1001` produced keys 83, 2085, 4087 — cadence 2002. `25` and `60`
   > produced 2000, because 2000 ms is 50 and 120 frames exactly.
   >
   > **So this applies to every leg, not only the ones that ignore the flag.**
   > At 23.976 the segments are 2002 ms apart on software and VideoToolbox as
   > well as on QSV.
   >
   > **The decision stands; its arithmetic does not.** The listing is honest
   > because it names the cadence the leg will actually produce, derived from
   > the source rate, rather than asserting a constant. `produced_segment_ms`
   > answers it and returns `None` when there is no honest answer — no source
   > rate, or a cadence that is not whole milliseconds — and the session then
   > keeps a per-run listing rather than naming a grid it cannot justify.
   > Shipped in #182.

2. **Copy and remux keep the per-run map-assembled playlist** of ADR-0020 §4.
   Copy cuts at source keyframes and cannot hold a uniform grid: on a healthy
   title, 77% of the URIs a synthetic grid listed were never written. That is a
   measurement about where copy cuts, and no keyframe map changes it. Rule 4.11
   asks which field distinguishes two cases rather than which branch; the field
   is session mode, and the reason is recorded here rather than left as an
   unexplained fork.

   > **Corrected 2026-08-31 — copy lists the whole title too, on a 20 s
   > grid.** What the 77% measured is that **copy cannot hold a 2 s grid**,
   > not that it cannot have a full-title listing. That figure is ADR-0020's,
   > dated 2026-07-31, on a synthetic 2 s grid over Elementary 3x05.
   >
   > **§S8 of `nightjar-meta`'s `stay-ahead-vt-2026-08-20.md` measured the
   > 20 s shape three days before this ADR was written, and this decision
   > cited the 77% instead.** A human trial on a real title: the 2 s grid gave
   > one FFmpeg per skipped cue, a 4 s worst wait, and on the second seek a
   > video stall with audio that went robotic and stayed. One MPEG-TS per 20 s
   > window was **"stable"** — 67 windows listed, 20 on disk, last first byte
   > 574 ms. §S8b confirmed it on iPhone with `-c:a copy`.
   >
   > **And it is now measured against this product**, which §S8 was not — that
   > spike never calls the session API. On the N150, two titles and both copy
   > variants: the producer cut one segment per 20 s window at exactly the
   > keyframes predicted, and **8 of 8 listed URIs served `200` in 42-288 ms**.
   >
   > **What makes it listable is that copy's cut points are already known.**
   > The keyframe map (ADR-0023) holds every one, so the listing is the greedy
   > 20 s walk of that map from 0 — run-independent, and therefore nameable
   > before anything is written. No per-window cook is needed.
   >
   > **What survives of this decision**: copy keeps a coarser grid, and
   > **scrub granularity is the window, not 2 s. Fine-grained seek stays
   > transcode.** Shipped in #182.

3. **A listed URI is never 404 and never 503.** The request is held until that
   segment lands in the store, and released then — not when the encoder that
   produced it finishes. A cold URI is a seek: the session starts an encoder at
   that media time (ADR-0050 §4), measured at a 976 to 1132 ms median and under
   2.4 s worst case. 404 is reserved for a URI outside the title or off the
   grid.

   > **This overturns ADR-0020's miss policy, and did not say so until
   > 2026-08-31.** That policy is *"segment GETs never move the encode window;
   > far scrub is `POST /seek`"*, and it is the negation of the sentence above
   > on the same path for the same request. Both were on the books from
   > 2026-08-23 to 2026-08-31.
   >
   > It went unnoticed because fill-forward covers every listed URI while the
   > playlist lists one window. **A full-title listing names URIs no encoder is
   > near**, and for those `Wait` never ends: the hold runs to `IDLE_TIMEOUT`
   > and returns an empty 204.
   >
   > **The policy had three sites**, all narrowed in #182 to yield only to a
   > want the playlist lists: `decide_segment_miss`, the
   > `digback_behind_committed` gate, and a `want_ms < window_start` 404 in the
   > wait loop. **An unlisted want still declines**, which is what the dig-back
   > guard was measured to be for.

   Holding is viable because the wait is bounded by a spawn, and only because
   of that. ADR-0011 paired full-title listing with an unbounded wait and
   ADR-0020 §7 withdrew it for that reason.

4. **One `EXT-X-MAP` per rung, covering every run in that rung's session.**
   Measured identical across runs; the code already relies on this, since a
   seek that lands on mapped media copies the prior run's init.

   > **Overturned 2026-08-31 — `init.mp4` is not identical across runs, on any
   > path, including the one this was measured on.** Not clarified: the whole
   > of this decision is the identity, and it is false.
   >
   > **The init carries the land.** `spawn_ffmpeg`'s own comment says so:
   > `-output_ts_offset` *"stamps title-absolute time into init `elst`
   > empty-edit and each fragment's `sidx.earliest_presentation_time`"*. So
   > **two runs at different lands cannot have identical inits** — that is by
   > construction, and the measurement below only confirms it.
   >
   > Measured through the product's own session path, runs counted only when
   > they spawned:
   >
   > | path | where | runs | distinct inits |
   > |---|---|---:|---:|
   > | `h264_qsv` transcode | N150 | 3 | **3** |
   > | `libx264` transcode | Mac | 2 | **2** |
   > | `h264_videotoolbox` | Mac | 2 | **2** |
   > | copy (`-c copy`) | N150 | 3 | **3** |
   >
   > On `libx264` the whole file differs by **two bytes, one per track, both
   > inside `elst`**: `...00000050...` (80 ms) at land 0 against
   > `...0000c350...` (50000) at land 50000. On QSV the values are the snapped
   > lands, 44044 and 116116. On copy the size differs too, 1748 bytes against
   > 1423, so there it is not only the edit list.
   >
   > **Why this measured identical in August is a hypothesis, not a finding.**
   > The likely difference is the FFmpeg build: `sidx_title_offset_ms` exists
   > in the tree because *"Ubuntu apt 6.1 still emits encode-relative sidx from
   > 0 despite that flag"*, and a build that ignores `-output_ts_offset` writes
   > no land into the init either. The N150 now runs FFmpeg 8.0.1, which
   > honours it. **If that is right the claim was never a property of the
   > format — it was a property of one FFmpeg version**, which is worse than
   > being wrong, because the code was written against it.
   >
   > **What the code relies on is now resting on a different reason.** The
   > map-hit path copies the prior run's init, and that is probably still
   > correct — the new run serves the source run's segments, so an init
   > describing the source run's timeline is the matching one. **It is not
   > correct because the inits are identical.** Whether a copied init decodes
   > the segments it is served with is **untested**.
   >
   > Measurement and method: `nightjar-meta`
   > `notes/init-identity-across-runs-2026-08-31.md`.

5. **One playlist URI per session for transcode**, replacing one per run. The
   session API stays the authority on land, and clients still do not construct
   segment URLs.

   > **Bounded 2026-08-31 by decision 4's correction: the map cannot be
   > session-scoped.** A session-scoped `EXT-X-MAP` names one `init.mp4` for
   > runs that need different ones, and the init carries the land.
   >
   > **This does not block the rest of decision 5.** The master and media
   > playlist URIs can still become session-scoped — the full-title listing
   > removed the moving window that ADR-0020's probe rejected, which was the
   > original objection. **What is off the table is one map per session**, so
   > the playlist would carry a per-run `EXT-X-MAP` under a session-scoped URI.
   >
   > **Whether that shape is coherent is not settled here.** A stable playlist
   > URI whose `EXT-X-MAP` changes between fetches is a different question from
   > the one the probe answered, and nothing in this repository has measured a
   > player against it.

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
