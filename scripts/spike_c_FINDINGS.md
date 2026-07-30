# Spike C findings — encoder restart / burn-in splice

Date: 2026-07-30  
Harness: `scripts/spike_c_burn_splice.sh` (kept until the splice slice lands
or is dropped; not CI — Rule 4.2)  
Raw workdir: `scripts/spike_c_work/` (gitignored)

Assumption: Phase 2 burn-in / progressive ASS follow-on after Spike A
(ADR-0019 measurement: extract is minutes-class). This spike answers
parser compatibility only. Citation target for ADR-0018; keep permanently.

## Method

1. Build a 16 s fMP4 HLS tree matching production encoder flags
   (`libx264` + SDR `sidedata`/`setparams` chain, 2 s segments,
   `-force_key_frames` on `SEGMENT_MS`, AAC stereo).
2. Phase A: encode segs `000`–`002` with **no** `ass=`.
3. Phase B: kill/restart at 6 s with `ass=`, same init.mp4 (restored
   after phase B rewrite), same encoder params, `-output_ts_offset 6`,
   `-start_number 3`.
4. Serve two VOD playlists over the same segments: no
   `#EXT-X-DISCONTINUITY`, and one with the tag before `seg003`.
5. Drive attach → play across the splice → seek back into pre-splice:
   - Chromium + hls.js
   - Safari native HLS
   - Firefox + hls.js

Not tested here: live EVENT playlist append mid-attach (playlist was
complete before attach). Tizen / webOS remain Gate 3.

## Results

| Consumer | Tag-free cross | Uninterrupted | Pre-seg re-request after cross | Seek-back OK | With `EXT-X-DISCONTINUITY` |
|---|---|---|---|---|---|
| Chrome / hls.js | yes | yes | none | yes | yes (same) |
| Safari native | yes | yes | none | yes | yes (same) |
| Firefox / hls.js | yes | yes | none | yes | yes (same) |

Notes:

- Firefox tag-free emitted one non-fatal hls.js `bufferSeekOverHole`
  (~ms 738, before first `playing`). Playback still crossed the splice
  and seek-back succeeded. The disc variant did not emit that error in
  this run.
- Seek-back landed in already-buffered pre-splice segments (no network
  re-fetch observed). Those segments have no burned captions by
  construction.

## Answers for ADR-0018 input

**Which parsers need the tag?** None of the three, for this harness
shape (held init, held encoder params, continuous PTS, VOD attach).

**Is a tag-free splice achievable?** Yes, under those constants.

**Backward-seek decision:** **Accept** pre-splice segments (no burn)
and say so in copy. Do not drop/regenerate them for this path.

Rationale: captions literally could not exist before the local `.ass`
was ready; rewriting the lead is a second encode window that buys
little once the product story is “captions appear from the attach
point forward.” Drop/regenerate remains available later if dogfood
shows the bare lead as a real support load — it is not required for
parser continuity.

**Tizen / webOS:** Three desktop/parser stacks agreed; nothing here
looks sensitive enough to pull that verification earlier than Gate 3.
Revisit only if a live mid-play playlist append (not covered here)
fails on a binding client.

## Limits (honest)

- Synthetic 16 s source, not a household remux.
- Complete VOD after both encode phases, not an EVENT update while
  the player is already attached.
- “Uninterrupted” means no stall / fatal error past the splice, not
  pixel-perfect A/V continuity audited by eye in this run.
