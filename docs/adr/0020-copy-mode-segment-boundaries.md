# ADR-0020: Producer-owned segment boundaries (time-keyed)

- Status: accepted
- Date: 2026-07-31

## Context

Gate 2 HLS sessions inherit a contract that only forced-IDR transcode
satisfies. ADR-0008 §2–3 lock a uniform 2 s grid and force IDRs on every
`SEGMENT_MS` boundary so playlist `EXTINF`, `-hls_time`, and `-start_number`
agree. Copy cannot place IDRs; FFmpeg cuts at source keyframes. ADR-0011
noted that caveat, then accepted a generated full-title VOD that still
indexes by `N × SEGMENT_MS`. The server deletes FFmpeg's own `index.m3u8` on
every `restart_at` and replaces it with that synthetic body.

Measured on a healthy WEBDL title (Elementary 3x05, mid-seek, Copy):

| Fact | Value |
|---|---|
| Files written in a 60 s `-t` window | 7 |
| URIs `build_playlist` lists for the same encode..+60 s index range | 30 |
| Never written | 23 / 30 (**77%**) |
| Mean gap between `sidx.earliest_presentation_time` values | ~10.4 s (source GOP) |
| Claimed advance across those 7 files | 12 s |
| Media advance across the same files | 62.4 s |
| Later-segment media vs claimed index | up to **+43 s** and growing |

`sidx.earliest_presentation_time` is the usable timing primitive (`tfdt` is
always 0 in this muxer's fMP4). `-output_ts_offset` stamps title-absolute
time into init `elst` and per-segment `sidx`, not into `tfdt`/`trun`.

Damaged DVDRip item 8519 is the same assertion failure at demux ceiling
(requested ~998 s, written name claims that index, media at ~375.9 s). That
is a Gate 2 correctness bug on healthy titles, with DEF-8519 as the damaged
tail.

**Root question:** who owns segment boundaries — not "windowed vs full-title"
as a first fork.

### Why the probe killed full-title VOD

`scripts/playlist_shape_probe/RESULTS.md` tested three static shapes
(mutated EVENT same URI; fresh EVENT per land; fresh VOD+ENDLIST per land)
on AVPlayer and hls.js with ~40 s real windows from a healthy title (~1325 s
claimed duration):

- Seekable range tracked the **listed window** (~40 s / sum of EXTINF), not
  title duration.
- Far seek to ~600 s on a land-A playlist **failed** (AVPlayer
  `seek_finished=false`) or **clamped** to the window end (hls.js / VOD).
- The only shape that played mid-title media after a far scrub was a
  **fresh playlist URI** whose entries covered that land; seeks inside it
  stayed window-local.

A synthetic full-title playlist cannot be reintroduced without new player
evidence that overturns that probe. Drawing a scrubber from item duration
covers the bar, not the seek — that is the ADR-0016 failure mode.

## Decision

1. **Producer owns boundaries.** Muxer cut rules are the source of truth for
   video segment start, duration, and whether a URI exists. The playlist does
   not assert a uniform grid the producer did not write. Transcode keeps
   forced IDRs (ADR-0008 §3); that makes its boundaries fall on
   `N × SEGMENT_MS` as a **property of the encoder**, not of the URI scheme.

2. **One wire grammar: time-keyed URIs everywhere.** Segment URIs are
   `seg_<ms>.m4s` with title-absolute start time in milliseconds, zero-padded
   to 11 digits (e.g. `seg_00001277151.m4s`). Unit documented on the
   server-side map. Copy and transcode share this grammar so clients and
   tests never branch on session mode. Transcode loses nothing: forced IDRs
   mean its keys are exactly `N × 2000`.

3. **Per-run directories; one global map.** FFmpeg's `-hls_segment_filename`
   only supports `%d`. Each producer run writes `segNNN.m4s` into `run_<n>/`.
   The server parses that run's honest `index.m3u8` and merges validated
   entries into a **session-global** map
   `start_ms → (run, file, duration_ms)`. Runs are producer bookkeeping;
   time-keyed URIs are title-absolute. A new run's playlist is assembled from
   **any** map entries that cover the window — including segments prior runs
   already produced — so scrub-back into finished media stays a plain file
   serve (the property `restart_at`'s retain comment promises). Per-run dirs
   underneath avoid on-disk collisions when two runs would both claim start T
   with different packed durations.

4. **Playlist from the map; fresh URI per run.**
   - Assemble from the global map (real EXTINF, real boundaries), not a
     synthetic `N × SEGMENT_MS` grid. Do not delete honest muxer output only
     to replace it with a lying full-title body.
   - **EVENT** while the current run is cooking; **ENDLIST** when that run
     reaches EOF.
   - Each producer run gets a **distinct playlist URI**. Far scrub /
     `?startMs=` starts a new run and returns that URI. Mutating one EVENT
     under a live player is not the seek mechanism.
   - `#EXT-X-START` is **window-relative** (near zero). Title land is a
     session API field.
   - Product scrubber range comes from item duration / usable extent. The
     player timeline is the current run's window; the client translates bar
     position → session API (`startMs`) → new playlist URI → source swap.
     Clients must not construct playlist or segment URLs themselves.

5. **Hard cutover.** No feature flag, no dual URI grammar, no compatibility
   shim. Delete `build_playlist`'s uniform grid and the
   `segNNN ↔ N × SEGMENT_MS` mapping. Delete client derivation of segment
   URLs from `playhead / SEGMENT_MS`. Session dirs are ephemeral; wipe the
   HLS session cache on deploy. Do not leave the old shape compiling.

6. **Amend ADR-0008.**
   - **§2** — The 2 s lock remains the **encoded-rendition** invariant
     (force-IDR cadence, ABR alignment). Playlist `EXTINF` /
     `TARGETDURATION` follow the producer for every session mode; for
     transcode they happen to equal `SEGMENT_MS` because of §3.
   - **§4** — Segment URI shape changes from derived index to time-keys for
     all modes. Deliberate: current `segNNN.m4s` ↔ `N × SEGMENT_MS` is false
     for copy and redundant for transcode. Master `master.m3u8` stays
     additive; media playlist URIs are per-run.

7. **Amend ADR-0011.** Copy-mode caveat and amendment §6–7 synthetic
   full-title VOD with load-bearing 503 for every cold index URI are
   withdrawn. 503 remains for a URI the **current run's playlist has listed**
   but not yet on disk; 404 for a URI never claimed by a served playlist.

8. **ADR-0016.** Still rejected: do not list only servable segments under a
   VOD ENDLIST while pretending the playlist is the title scrubber. Scrub
   authority is item duration + session API + fresh playlist URI.

9. **Map-build gate.** When merging a muxer playlist into the global map,
   verify each entry's EXTINF-derived start against that file's
   `sidx.earliest_presentation_time`. Mismatch → do not publish. Serve
   trusts the map; never return bytes whose media time disagrees with the
   URI's claim.

10. **Subtitles** stay on the 2 s VTT grid (ADR-0010 / ADR-0013), decoupled
    from video boundaries. Delete
    `subtitle_media_playlist_matches_video_segment_count`.

11. **Honest duration and landed position (same wire ADR).**
    - **Usable extent:** when a producer reaches EOF at a media time
      **materially short of container / probed duration**, treat the item as
      damaged and record usable extent lazily (pay once per damaged title).
      EOF near the container duration is a finished run, not damage.
    - Session responses expose **landed** media start and usable extent when
      known. No silent Infuse-style relocate. Fields in `api/openapi.yaml`.

12. **Retention and cache accounting are per-run.** Prior runs' segments stay
    on disk so the global map can serve scrub-back without restart. Eviction:
    when the session's on-disk HLS bytes would exceed the configured HLS
    session cache budget (or on session reap / idle teardown), drop the
    **oldest finished run directories** first (never the current cooking run),
    remove their map entries, and recount. A long scrubbing session must not
    grow unbounded. Exact byte cap may share or sit beside existing session
    disk bounds; the owner is the session registry cleanup path.

13. **Client source replacement.** A new playlist URI means `hls.js`
    `loadSource` and, on Safari native, a new `src` and re-attach. Buffered
    media is discarded. On HDR titles this may re-trigger the display-mode
    handshake — a TV that re-syncs on every scrub is a worse experience than
    a slow one. Verify in dogfood; do not assume it is free.

14. **Track selections do not survive a run swap.** If master / media
    playlists are per-run, subtitle group and audio selection reset on every
    far seek. The client re-applies them after the swap — the same reapply
    path an audio track change already needs (one path, not two).

15. **Upgrade path.** A keyframe map can later precompute copy boundaries,
    drive `-segment_times`, and restore a full-title VOD over the same
    time-keyed URIs. Out of scope here.

16. **`independent_segments`.** Confirmed for multi-GOP copy segments: first
    sample remains a sync sample. Keep the flag.

17. **`spawn_ffmpeg`:** document that `-output_ts_offset` is load-bearing for
    title-absolute `elst` / `sidx` under copy (and any path that relies on
    those for the map).

## Consequences

**Server.** Per-run dirs; global time-keyed map; playlists from the map;
rework `decide_segment_miss`, `note_first_segment_ready`,
`latest_segment_in_window`, and related index arithmetic into time/map
lookups; delete synthetic full-title `build_playlist`; per-run eviction
against the cache budget.

**Clients.** After `startMs`, fetch the new playlist URI from the session
API, then swap the source. Re-apply audio/subtitle selection after the swap.
Scrub bar from item / usable duration. No client-constructed segment URLs.

**Stock platform player UIs cannot title-scrub producer-truth EVENT.**
On iPhone, a `<video>` entering fullscreen is handed to the system player:
our scrub bar disappears and the native scrubber operates on
`video.seekable`, which under this ADR is the produced window, not the
title. It cannot reach `POST /seek`. The same bind applies to AirPlay, PiP,
lock-screen controls, and tvOS AVKit. General form: no stock platform player
UI can scrub a title it cannot see. That removes the stock-player option
from the client architecture decision for Nightjar scrubbing.

Partial mitigations, and what they do not recover:

- Fullscreen the container rather than the `<video>` element — keeps our
  scrub bar in web UI, loses true native fullscreen chrome on iPhone.
- Intercept `seeking` during native fullscreen and translate to
  `POST /seek` — can retarget land; does **not** recover an honest title
  range on the system scrubber (`seekable` remains the window).

Recorded from web dogfood (item 33, 2026-07-31): evidence for the ADR entry
above, not a fix in the same slice.

**Tests / acceptance.** Warm-restart sweep on 8519 / 8512 / 8517 and
Elementary; no 404 on a listed URI; map-build gate holds; backward scrub into
mapped media is a file serve, not a restart; landed / usable fields match
produced media. Web: 75% on 8519 reports damage and plays within usable
extent; Elementary mid-scrub plays via fresh playlist URI. Relative URI
class: `session_hls_link_walk_resolves_to_real_bytes` walks master → media →
init → first segment with client resolution.

**Out of scope.** Keyframe-map / trickplay; restart-latency tuning; changing
the 2 s force-IDR cadence for transcode.
