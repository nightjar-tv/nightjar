# Feasibility: precomputed keyframe map

- Status: note only (ready to become an ADR; no schema, no code)
- Date: 2026-07-31

One artifact keeps answering five separate questions: trickplay thumbnails,
damaged-file detection (8519-class truncated indexes), restart latency (skip
probe, byte-offset seek, deterministic `-segment_times`), publishing a
full-title playlist whose names and `EXTINF` are true, and an **instant
damage banner** (usable extent known at scan time instead of ~56 s after a
hung mid-title EOF — the user's first experience of a damaged title today
is a stall, then an explanation). That pattern usually means a missing
piece, not an optimisation.

## What the map is

Per-title, ordered keyframe times and byte offsets for the primary video
stream, produced once at scan and stored beside other item media metadata.
Session land and seek consult the map instead of rediscovering GOP
boundaries under time pressure. Offsets come from the same container index
as the times — that is what makes probe-free seek possible.

## Scan cost: index-first, not a packet walk

Today's scan probe reads headers. A full keyframe **packet walk** reads
every byte of every file. Across ~25k items over SMB that is tens of hours,
not an incremental delta on the existing open. Do not frame the cost that
way.

The way out is already in the 8519 evidence: “cue index has 77 KFs, last
375.917” was read without walking the file. MP4 carries `stss` plus offsets
in the `moov`; Matroska carries Cues. After `find_stream_info`, libavformat
has populated `index_entries` from those. So:

1. Read the container index (milliseconds per title).
2. Fall back to a packet walk only when the index is missing or provably
   truncated (last indexed keyframe far short of `durationMs`).

That fallback condition is also the damage signal. Cost control and
8519-class detection are the same check. The scan question reframes from
“can we afford a library-wide packet walk” to “how many files lack a usable
index,” which is a cheap sample on the dogfood library before any ADR.

Byte offsets for seek land come free from the same `index_entries` source.

**Framing for wall-clock estimates.** A multi-hour number is fine if the
pass is **one-time** (or incremental on mtime/size change) and **resumable**
on a self-hosted server. It is not fine as a cost paid on every full
rescan. Say which before the estimate reads as a blocker.

Direct Cues/`stss` sample (2026-07-31, n=250): index present 86.0%,
truncated 2.0%, packet-walk fallback 16.0%. Full-library one-time header
parse @ p50 ≈ 10.07 h (incremental on mtime/size; not per-rescan). See
`nightjar-meta/notes/index-availability-direct-2026-07-31.*`. The earlier
timed-seek proxy (32.4% / 50.4% / ~22.4 h) is superseded — do not quote it.
At 2% truncated, the 8519-class is on the order of ~500 items in a 25k
library, not ~8,000.

**House rule.** Never derive a rate from a timing proxy when a direct read
exists. This week a timed 90% seek, a 0.9 fps `setState` path, and two
contradictory far-scrub runs each produced a quotable wrong number. Direct
container reads (Cues / `stss` / `index_entries`) settle integrity; timing
instruments settle latency.

## Gating experiment: dictated cuts, not prediction

Recording keyframes is not enough for a full-title playlist. `-hls_time` is
a heuristic; the residual short-GOP packing question only matters if
dictated cuts fail. The gate is therefore:

1. Were the requested cut times actual keyframes? (Elementary 10.4s GOP is
   the binding case — if they were not mapped KFs, a miss is expected.)
2. Does production `-f hls` honour `-segment_times` in copy mode? A yes on
   `-f segment` does not transfer. If HLS ignores the list, the fallback is
   `-f segment` writing fMP4 with our own playlist (already generated).

No further cut-rule prediction work until those two answers land.

## Honest full-title, cook-on-miss, and native scrubbing

Listing unproduced segments is not a revival of the old defect. The old
defect was two things: names that asserted a false time mapping, and misses
that 404’d. Honest time-keyed names (`seg_00003600000` means 3600s) fix the
first outright. A miss then means: the user wants that time; the server
restarts the producer there and holds. That is ADR-0011’s original design;
it failed only because the names lied so the restart went somewhere else.

A full-title listing with `ENDLIST` is also what gave AVPlayer a
title-length `seekable` range before — why it could form a request for
seg490 at 75% in the first place. Honest full-title restores native
scrubbing on stock players (and removes the windowed-EVENT ceiling that
Chromium already shows as `seekable` ≈ 52–60s on a 96-minute title).

**Dependency:** full-title listing requires cook-on-miss. Segment-miss
Restart was deleted in the ADR-0020 cutover; `POST /seek` is now the only
cook path. Bringing cook-on-miss back is not churn or a silent reversal —
it is the justification returning once names are honest. State it that way
in the ADR.

The map (plus a passed cut-rule simulation) makes names and durations
honest enough to list. Cook-on-miss makes the listed URIs reachable. Both
are required; neither alone is enough.

For **transcode**, known boundaries also let `-segment_times` / forced IDRs
align with the published grid without rediscovery. Copy still cuts at
source keyframes; the map does not invent IDRs.

## Storage shape and size

Minimal durable row per item (or per file):

- ordered keyframe PTS (ms or timescale ticks)
- byte offset per entry (from the same index; for probe-free seek)
- usable-extent / last-trusted index when the container index is truncated

Size: episode with a keyframe every ~2s over 45 minutes ≈ 1.3k entries.
Tens of KB per title even with times + offsets; a feature film still under
a megabyte. SQLite BLOB or scan sidecar. Not a new service.

Item `durationMs` stays authoritative for the title bar; the map explains
where decodes can start (and where the index stops being trustworthy).

## Client compensation that could then die

If land, usable extent, and (after the gating test) full-title cook-on-miss
return:

- Title↔media timeline math under windowed EVENT playlists (this week’s
  compensation) can go — native `seekable` is the title again.
- Much of the cook-window / land-ensure polling shrinks to “map says land
  L; wait for first real segment at L” (or miss-hold on the listed URI).
- Damage UI keys off map usable-extent at session start — banner is instant
  instead of painting after a ~56 s empty-EOF hang (8512 measured).
- Trickplay samples mapped times; restart skips emergency probe when
  offsets exist.

Relative-URI discipline stays until playlist emitters stop using
depth-sensitive hops (ADR-0008 note).

## Recommendation

1. ~~Sample index availability~~ — direct Cues/`stss`: 2% truncated, 16%
   walk fallback; scan is one-time/incremental.
2. Confirm Elementary dictated times were real keyframes; then test
   `-f hls` + `-segment_times` in copy mode. Full-title only if that path
   (or `-f segment` + own playlist) is honest.
3. ADR: map storage + index-first scan write + session land from map +
   scan-time usable extent (instant damage banner); revive cook-on-miss
   with the “justification returning” framing; full-title only if step 2
   passes.
4. Trickplay after the map exists.

No code in this note.
