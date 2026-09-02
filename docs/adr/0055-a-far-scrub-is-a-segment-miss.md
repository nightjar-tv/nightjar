# ADR-0055: A far scrub is a segment miss, not a POST

- Status: **proposed**
- Date: 2026-08-31
- Depends on: [ADR-0054](0054-full-title-playlist-for-transcode.md) decision 1
  (the full-title listing) and decision 3 (a cold URI is a seek);
  [ADR-0050](0050-lead-held-session-shape.md) §4 (a seek spawns and does not
  kill)
- Measured 2026-08-31 against the shipped build at `95a7735`: a human scrub,
  two sessions, both directions, restarted the encoder **nine times and posted
  zero seeks**. Capture and method are maintainer-private

## Context

**ADR-0054 decision 3 already decided this, and nobody traced what it cost.**
It overturned ADR-0020's miss policy (*"segment GETs never move the encode
window; far scrub is `POST /seek`"*) and replaced it with *"A cold URI is a
seek: the session starts an encoder at that media time."* That is a decision
about the trigger, made in a decision about 404s, and its consequence went
unread for eight days.

**The consequence is that `POST /seek` stopped being reached at all.** A human
scrub on the shipped build at `95a7735`, two sessions, both directions,
restarted the encoder **nine times and posted zero seeks**. Every restart came
through `desire_restart` and the debounce.

### Why the client stopped posting

`seekToTitleSeconds` (`web/src/lib/hlsPlayer.ts:430`) takes a fast path when the
target is inside the produced window, and posts only when it is not. Two
guards decide that, and S3 disabled both.

`mediaTimeInProducedWindow` (`web/src/lib/hlsTimeline.ts:52`) tests
`mediaSeconds <= seekable.end + 0.35`. A full-title `VOD` listing makes
`seekable.end` the whole title, so every in-range target passes.

`beforeLand` (`hlsPlayer.ts:439`) is `titleSeconds + 0.05 < landSec`, from
`landedMs`. **`landedMs` is a latch.** It is initialised at `hlsPlayer.ts:296`
from `startAtSeconds` and assigned in exactly one other place,
`hlsPlayer.ts:387`, inside `swapToPlaylist` — which runs only after a
`POST /seek`. A session attached at 0 therefore has `landedMs = 0` forever,
`beforeLand` is never true, and the post never fires. **The one thing that
would update the client's idea of the land is the path the stale value
suppresses.**

### What each path costs

Both spawn an encoder, through the same `restart_at` (`hls.rs:2356`). What
differs is the trigger and what sits between it and the spawn.

**Path A**, the segment miss, has four gates: `decide_segment_miss`
(`hls.rs:274`) returns `Wait` while `since_last_restart < RESTART_MIN_INTERVAL`
(2 s); `desire_restart` (`hls.rs:2593`) records a pending target rather than
acting; `pending_restart_due` (`hls.rs:346`) refuses before `first_segment_ready`
and holds for `RESTART_COALESCE_QUIET` (400 ms) of quiet;
`maybe_apply_pending_restart` (`hls.rs:2635`) then calls `restart_at`.

**Path B**, `POST /seek` (`hls.rs:1626`), has none. It aligns and calls
`restart_at` at `hls.rs:1646`.

**That asymmetry is not an oversight in path B.** Path A's trigger is one
segment GET per position the element passes through, emitted by the player and
not by a person; a dragged scrub emits many. Path B's trigger is one POST per
gesture, already deduplicated client-side by `lastStartMsSent`
(`hlsPlayer.ts:454`). Coalescing exists because path A's trigger is noisy, and
that is a property of the trigger rather than a feature one path forgot.

### What is measured

**Path A, on the shipped build**: six spawns during the scrub,
`h264_videotoolbox` on one Mac. Client-visible waits **953, 1021, 1024, 1035,
1096, 1328 ms**; encoder `first_segment_ready` 514 to 781 ms.

**Path B: nothing, against this product.** ADR-0054 decision 3 cites *"a 976 to
1132 ms median and under 2.4 s worst case"*. Those come from ADR-0050 §4's
table and its reap-delay table, both produced by the 2026-08-21 scheduler
spike origin — a standalone harness, not this product. **That harness never
calls the session API** — ADR-0054 decision 2's own correction says so of the same spike
family. The figures are real and they are not this path's.

## Decision

1. **A far scrub inside the title is a segment miss. That is the design, not an
   accident of S3.** The client seeks the element, the player requests a listed
   URI at the new position, and the session restarts an encoder there. This is
   what ADR-0054 decision 3 already says; this ADR names it as the trigger
   rather than leaving it as a consequence.

2. **The coalescing family is load-bearing and is not deleted.**
   `pending_play_ms`, `pending_since`, `desire_restart`,
   `classify_restart_desire`, `CoalesceDesire`, `pending_restart_due`,
   `maybe_apply_pending_restart`, `coalesce_preempt_before_land`,
   `prefetch_advances_pending` and `pending_waiter_action` are all live on this
   path at `95a7735`. Only `segment_waiters` is dead, and S5b removed it.
   **The slice that proposed deleting them was written on the premise that the
   full-title listing starves this path; it feeds it.**

3. **`POST /seek` stays, for the case it still serves**: a session attached at a
   non-zero land, where a scrub back before that land must post because the
   media is not in the listing's produced window. It is not removed, and it is
   not the ordinary path.

4. **The `landedMs` latch is a defect, not the mechanism this decision rests
   on.** Decision 1 stands whether or not the latch is fixed. Fixing it makes
   `POST /seek` reachable again for scrub-back before the land, which decision 3
   is entitled to; leaving it means that case silently takes path A too. **Do
   not fix it as a side effect of anything else** — it changes which path a
   scrub-back takes, and that is this ADR's subject.

## Consequences

**Path A's trigger is the player, so the encoder's workload follows buffering
rather than intent.** A player that prefetches aggressively asks for distant
URIs the person never asked for, and every such want is a restart candidate.
The 400 ms debounce and the 2 s minimum interval are what stand between that and
an encoder per prefetch. Both are measured constants, and neither has been
measured against a real prefetch pattern.

**A listed URI can still 503**, which decision 3 says it never does. The hold in
`asset_wait` is bounded by `SEGMENT_WAIT` (30 s) and the promise is not.
Observed twice in the same capture, on `seg_00000670000.m4s`. `OPEN-DEFECTS.md`
entry 17 carries it. **This ADR does not fix it and does not depend on it.**

**Neither path has a comparison.** Path A now has six product samples and path B
has none, so "which is faster" is unanswered rather than answered in path A's
favour. Anyone reopening this needs a path B measurement against the product,
not against the spike.

## Alternatives

**Make `POST /seek` the path again**, by narrowing the fast path so an
out-of-run target posts. This is the shape ADR-0020 chose and ADR-0054 decision 3
overturned, and reinstating it would mean a listed URI the player requests
cannot move the encoder — which is decision 3's failure mode, where `Wait` never
ends. Rejected on that ground, not on latency, since the latency comparison does
not exist.

**Collapse the two into one path.** Attractive, and it does not remove the work:
whichever survives inherits a noisy trigger and therefore needs the coalescing.
Worth doing for the second entry point rather than the first — `POST /seek` has
no debounce, so a client that posted rapidly would spawn per post, and only
`lastStartMsSent` prevents it. Deferred, and named here so it is a choice rather
than a gap.
