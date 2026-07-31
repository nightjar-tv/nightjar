# Feasibility: precomputed keyframe map

- Status: note only (ready to become an ADR; no schema, no code)
- Date: 2026-07-31

One artifact keeps answering four separate questions: trickplay thumbnails,
damaged-file detection (8519-class truncated indexes), restart latency (skip
probe, byte-offset seek, deterministic `-segment_times`), and publishing a
full-title playlist whose names and `EXTINF` are true. That pattern usually
means a missing piece, not an optimisation.

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
rescan. Say which before the estimate reads as a blocker. A timed-seek
proxy on 2026-07-31 suggested ~half the library may need a walk fallback —
that figure is not quotable until replaced by a direct Cues/`stss` /
`index_entries` read.

## Gating experiment: can we predict cut boundaries?

Recording keyframes is not enough for a full-title playlist. The unproven
step is whether FFmpeg’s HLS cut rule is a pure function of the keyframe
list and `hls_time`.

From measured copy runs the rule looks deterministic: cut at the first
keyframe at or after `hls_time` has elapsed since the previous cut.
Elementary with ~10.4s GOPs and `hls_time` 2 cut at every keyframe; Rick
and Morty at `hls_time` 10 packed 5–8 keyframes per segment.

**Gating test (no encoding, no server, about an hour):** take the keyframe
lists for those two titles, simulate the cut rule offline, and compare the
predicted segment start times to the boundaries those titles actually
produced. If simulation matches production, full-title playlists can return
with honest time-keyed names. If it does not, the map still buys land,
damage, trickplay, and offsets — but not pre-declared full-title `EXTINF`.

That single test decides whether full-title comes back. Run it before the
ADR commits to playlist shape.

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
- Damage UI keys off map usable-extent instead of discovering empty EOF
  mid-title after a hung seek.
- Trickplay samples mapped times; restart skips emergency probe when
  offsets exist.

Relative-URI discipline stays until playlist emitters stop using
depth-sensitive hops (ADR-0008 note).

## Recommendation

1. Sample how many dogfood-library files lack a usable container index
   (defines scan cost and 8519 coverage).
2. Run the cut-rule simulation on Elementary + Rick and Morty (gating).
3. ADR: map storage + index-first scan write + session land from map;
   revive cook-on-miss with the “justification returning” framing; full-
   title playlist only if step 2 passes.
4. Trickplay after the map exists.

No code in this note.
