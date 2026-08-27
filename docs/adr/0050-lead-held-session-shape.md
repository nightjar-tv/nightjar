# ADR-0050: A transcode session is one throttled encoder holding a lead

- Status: **proposed** (§5 amended 2026-08-23 with the measured delay;
  2026-08-24: a superseded encoder runs until reap rather than being
  suspended; 2026-08-25: §5 separates what was measured from what is
  inferred, and Consequences reconciles its deletion list against the
  eighteen items it named)
- Date: 2026-08-23
- Supersedes: ADR-0007 §3 (the concurrency cap model) and §4 (seek as kill
  and restart)
- Depends on: ADR-0020 (producer-owned boundaries, time-keyed URIs, the
  session-global segment map); ADR-0023 (keyframe map, byte-offset start);
  ADR-0052 (the 2 s IDR grid this shape assumes)
- Gate: Gate 2 — the corpus plays, and a seek into untranscoded media starts
  under three seconds
- Related: `nightjar-meta/docs/plans/2026-08-23-hybrid-session-shape.md`,
  which carries the measurements and the raw data paths

## Context

A transcode session today runs one FFmpeg to EOF with no throttle, restarts
it on a seek, and is admitted against a fixed cap of three live sessions. All
three of those are wrong, and the measurements that say so also overturn the
reasoning that was going to replace them.

Two shapes were on the table. **Cook-on-miss windows** react: a GET misses and
a 20 s window cooks. **A long throttled encoder** holds a lead and idles. The
2026-08-20 direction was windows. The 2026-08-22 spike reversed it on the
strength of a keep-open arm that reached eight sessions with zero stalls.

That arm was measured wrong. `media_offset` staggers playhead `i` to
`i * 180` seconds into the title so sessions do not share page cache, but the
title is 1325 s and the runs were 600 s. Past playhead 4 there is not that
much title left, so the late playheads stopped early and the run silently
dropped to fewer sessions than it reported. "Eight sessions, zero stalls" was
eight for 66 seconds and five for the last 174. Every published run at N>=6
on both boxes has this defect. The tell was `cliff-n7` and `ko2-n7` reporting
identical request counts under different schedulers, which is what running out
of media looks like and is not what contention looks like.

Re-measured at offset 90 on the N150 with QSV, the capacity claim survives:
eight sessions, zero stalls. The cost is wait rather than stalls, p90 1150 ms.
Every figure below comes from runs where each playhead had title for the whole
run, and the bench now refuses a run where one would not.

## Decision

1. **A session is one long-running encoder that holds a lead, then idles.**
   Not a series of 20 s windows. Measured at a matched 40 s lead, N=6, both
   arms holding `slots = N`:

   | arm | served (s) | waste | p90 | p99 | max wait | stalls |
   |---|---|---|---|---|---|---|
   | long encoder | 3508 | 1.039 | 39 ms | 928 ms | 3.4 s | **0** |
   | 20 s windows | 3402 | 1.038 | 31 ms | 2447 ms | **15.1 s** | **3** |

   Windows win the body of the distribution because most requests hit an
   already-cooked store. They lose the tail, because a window boundary that
   misses pays a full cold cook. Three waits crossed the 8 s stall bar in
   each windowed run and none did in the long-encoder runs. Both arms
   reproduced to three decimals and the stall count repeated exactly.

   **The argument is the stall bar, not efficiency.** Waste is identical,
   1.038 against 1.039, and throughput is near-tied. The earlier claim that
   windows cost throughput and waste more came from comparing arms holding
   different leads, 40 s against 180 s. Hold the lead equal and that
   difference disappears. Do not reintroduce windows on an efficiency
   argument; the efficiency argument does not exist.

2. **Lead target 40 s, floor 20 s.** A product constant, not a per-box one.
   The knee replicated across two encoders, two operating systems, a fanless
   laptop and a fanned mini-PC, and both run orders. Below it you pay half a
   second of latency for no saving; above it you pay encode for no gain. It
   also improves the long-encoder shape over Jellyfin's 180/90: p90 39 ms
   against 54-60, p99 928 ms against about 1147.

   These are constants, not settings. Rule 4.12: a measured value is
   measured, not asked for.

3. **Throttle by suspending the process.** SIGSTOP at the target, SIGCONT at
   the floor. FFmpeg's `-readrate` was measured as the declarative
   alternative and rejected: it paces the demuxer against wall clock rather
   than against the playhead, so it cannot see a viewer who has stopped. In a
   400 s pause its lead grew from 40 s to 418 s and **never recovered**,
   still climbing at 608 s when the run ended, because encoder and playhead
   both run at 1x and a gap once opened never closes. One pause cost 1,420 s
   of wasted encode. An encoder whose viewer never arrived wrote 1 segment
   under SIGSTOP and 46-60 under `-readrate`.

   `-readrate` is not merely worse. It holds a lead flatter than SIGSTOP
   while someone is watching, and wins the latency tail at low N. It fails
   because it has no way to stop.

   **Always SIGCONT before SIGTERM.** A stopped process ignores SIGTERM until
   it is continued. That leaked 23 FFmpeg processes in one bench sweep and
   will leak sessions in the product the same way.

4. **A seek starts a second encoder. It does not kill the first.** Measured
   at N=4 with a 300 s forward seek every 120 s, pooled over all seeks:

   | seek policy | n | median | p90 | max | held RSS |
   |---|---|---|---|---|---|
   | spawn, reap later | 7 | **1132 ms** | 1662 | 1960 | 1586 MB |
   | spawn, reap at once off the request path | 14 | 1761 ms | 2075 | 2268 | 0 |
   | kill, then restart (today) | 14 | 2187 ms | 2928 | 3418 | 0 |

   Two independent effects. Moving the teardown off the request path is worth
   about 430 ms at no memory cost. Not tearing down at all during the seek is
   worth a further 630 ms. Today's shape pays both penalties because it
   SIGTERMs the prior encoder and waits for it before spawning.

   The likely mechanism, inferred from the monotonic ordering and not from
   driver instrumentation: destroying a hardware encoder context contends
   with creating one. Confirming it needs instrumentation this bench does not
   have, so it is recorded as a hypothesis.

5. **A superseded encoder is suspended, then reaped on a short idle delay.**
   Suspending is not enough on its own. SIGSTOP frees encoder time and does
   not release memory: held encoders measured 1593 MB suspended against
   1586 MB running, identical within noise. Each costs 226 MB and never
   shrinks, seven accumulated in ten minutes at N=4, and about 22 fit in that
   box's free memory.

   **Amended 2026-08-24 — do not suspend it. It keeps running until reap.**
   The sentence above is what this decision said, and implementing it showed
   why it is wrong. This is the one place in the design where the obvious move
   is the wrong one, so the original claim stays visible rather than being
   quietly replaced.

   Suspending stops the encoder producing. A client may be waiting on a
   segment of that encoder's land which has not finished writing, and a
   suspended encoder never finishes it: the wait then runs to its timeout and
   the encoder is killed at reap having never produced the byte someone asked
   for. Under kill-and-restart this case had a guard —
   `may_kill_cooking_encode` deferred the kill until the cooking land had no
   waiter. Spawn-and-reap removes that guard, on the reasoning that nothing is
   destroyed. Suspension destroys production, which is the half of the
   reasoning that does not survive. The encoder must run until it is reaped.

   **The fast arm and the correct arm are the same arm.** Leaving the prior
   encoder running measured a 1132 ms median seek, the best of the three
   policies in §4, so keeping it alive costs nothing in latency. A later
   reader finding that suspension is required for correctness will not also
   have to trade it against speed; there is no trade to make.

   **Amended 2026-08-25 — that is an inference, and the shipped policy was
   never an arm.** The paragraph above is what this decision claimed, kept
   visible under the same convention as the amendment above it.

   §4's 1132 ms is the never-reap arm, which ends holding 1586 MB. The delay
   table below is the suspend arm: every row held the superseded encode
   SIGSTOPped for the delay, and every row but `never` then terminated it —
   `never` stayed suspended and ends holding 1591 MB. What ships — run, then
   reap at 5 s — is neither, and the bench data has no name for it (`spawn`,
   `spawn-suspend`, `spawn-then-kill`, `kill`).

   Two comparisons bear on whether running beats suspending, and they are
   not the same comparison:

   | pairing | run | suspend | gap | same experiment? |
   |---|---|---|---|---|
   | Spike B, four runs, n=7 each | `spawn` 1294 ms | `spawn-suspend` 1385 ms | 91 ms | **yes** |
   | §4 against the delay table | `spawn` 1132 ms (n=7) | `never` 1241 ms (n=14) | 109 ms | no — two passes |

   The second is the pairing this ADR carried, and it crosses passes: 1132 ms
   is Spike B2's `spawn` row and 1241 ms is the reap-delay pass's `never`
   row. Both gaps sit inside the run-to-run spread of about 110 ms this
   section states, so the conclusion holds either way — and the cross-pass
   pairing should be judged against cross-pass variance, which is larger
   still: Spike B's two `kill` repeats differ by 530 ms at the median.

   **Do not quote 1132 ms as the shipped policy's latency.** A run-then-reap
   arm would settle it; nothing gates on it, which is why this is written
   down rather than re-run.

   The memory figures above survive the amendment and are the reason
   suspending bought little regardless: it frees encoder time and not memory.
   The delay, not the suspension, is what bounds the held set.

   **The delay, measured 2026-08-23.** Five runs at N=4, a 300 s forward seek
   every 120 s, the superseded encode terminated after the delay:

   | delay | median seek | max | held RSS |
   |---|---|---|---|
   | 2 s | 1438 ms | 2381 | 0 |
   | 5 s | 976 ms | 1961 | 0 |
   | 15 s | 1083 ms | 2120 | 0 |
   | never | 1241 ms | 2065 | 1591 MB |

   Every delayed arm ends holding nothing, and 5 s and 15 s match or beat
   never reaping. **2 s is the worst arm because it is not clear of the seek
   it follows**: first byte lands at a 1.0-1.4 s median and a 2.4 s maximum, so
   at 2 s the destruction still fires while a new encoder is starting. That is
   the same contention as decision 4, and it sets the floor.

   **The delay must clear the measured first-byte maximum, with margin.** 5 s
   on this hardware. It is not a tuned optimum: it beats never reaping by
   200-300 ms against a 110 ms run-to-run spread, on one run of seven seeks.
   What is established is that a delay past the first byte costs nothing and
   removes 1591 MB.

   Memory is then bounded by seek-rate times delay, so there is no budget to
   size and no eviction order to choose. Do not ship an unbounded held set.

   **Open and unowned.** The bound is seek-rate times delay, and this design
   records the viewer seek rate as uncaptured. `HlsSessionRegistry::seek` has
   no rate limit: the only gate is that the aligned land differs from the
   current one. The web client mitigates by seeking on scrub commit rather
   than per drag position; the HTTP API does not. Nothing checks that the
   held set stays bounded, so the line above is currently an assertion rather
   than a guarantee.

6. **Do not cap concurrent encoding below the live transcode-session count.**
   `slots = N`. Capping saves no work; it selects which session waits. At
   `slots=1` one session waited 19.5 s while another waited 3.3 s; at
   `slots=7` every session landed between 2.2 and 2.8 s and stalls went to
   zero. The encoder time-shares better than a queue does.

7. **Admission is a session-start decision, and it never touches an
   incumbent.** `NIGHTJAR_HLS_MAX_SESSIONS` stops being a live cap. A new
   session is admitted against measured encoder load, where a transcode
   counts 1.0, a remux about 0.33 (a copy window finishes in 0.76-0.88 s
   against 2.6-2.8 s), and direct play 0. When the box is short the newcomer
   gets a reduced rung set. A session already playing is never degraded.

8. **Capacity is watched, not calibrated.** The signal that the box is short
   is that incumbent leads are sliding, not a number in a config file. That
   adapts to thermal drift for free: the M1's own N=4 p99 drifted 645.8 to
   687.8 ms across a single day. Gate 2's QSV `lastOk` of 5 was a
   run-to-EOF figure at 100% occupancy and does not transfer to this duty
   cycle.

9. **Windows is out of scope for throttling, and says so.** Decision 3 is
   POSIX. Windows has no SIGSTOP, and the property this design leans on is
   that a suspended encoder releases the hardware encoder, which is a driver
   question rather than a scheduling one. There is no Windows target in CI
   and no verified Windows build. Throttling ships on Linux and macOS.
   Windows, if it becomes a real build target, needs `NtSuspendProcess`
   measured for that release property before it can throttle, and runs
   against a fixed session cap until then.

   `docs/HW_ACCEL.md` currently lists Windows at tier 1 for QSV and NVENC.
   That is a claim about FFmpeg backends and should not be read as evidence
   that the server builds or runs there.

## Consequences

`Session.child: Option<Child>` becomes a per-run set.

**This deletes more than it adds.** Fifteen functions and three constants in
`hls.rs` exist to manage the races that killing an encoder mid-scrub creates:
may this cook be killed yet, is a client still holding the land about to be
abandoned, is this retained segment stale, should three rapid scrubs coalesce
into one restart. `classify_restart_desire`, `pending_restart_due`,
`may_kill_cooking_encode`, `coalesce_preempt_before_land`,
`no_fill_release_for_new_land`, `prefetch_advances_pending`,
`digback_behind_committed`, `pending_waiter_action`, `desire_restart`,
`maybe_apply_pending_restart`, `serve_ok_after_pending_apply`,
`serve_ok_retained_during_stale_guard`, `restart_at`, `restart_spawn_gap`,
`disable_preempt`, with `RESTART_MIN_INTERVAL`, `RESTART_COALESCE_QUIET` and
`STALE_RETAIN_REFUSE`, plus eighteen tests pinning their behaviour.

**Amended 2026-08-25 — that list is not one deletion, and it was wrong about
four of its own entries.** The paragraph above named eighteen items: fifteen
functions and three constants. Three have gone, eleven are live, and four
should never have been on the list. All eighteen are accounted for below.

**Gone (3).** `may_kill_cooking_encode` and `STALE_RETAIN_REFUSE` went with
the seek change; `serve_ok_retained_during_stale_guard` followed as a pure
deletion, its guard established never-armed first.

**Live, and behind `pending_play_ms` (11).** Seven take or set it directly:
`pending_restart_due`, `coalesce_preempt_before_land`,
`prefetch_advances_pending`, `digback_behind_committed`,
`pending_waiter_action`, `desire_restart` and `maybe_apply_pending_restart`.
Two are pure predicates that touch no pending state and are reachable only
through that machinery — `classify_restart_desire`, whose sole production
caller is `desire_restart`, and `serve_ok_after_pending_apply`, which exists
to check the result of an apply in `asset_wait`. `RESTART_MIN_INTERVAL` and
`RESTART_COALESCE_QUIET` are the family's own timing constants.

The list above said all of these "take or set `pending_play_ms`". Two do
not, and the difference matters: they go with the family by call graph
rather than by signature, so a search on the field name would not find them.

Removing any of them is **behavioural, not a tidy-up**: `desire_restart`
runs from two production paths in `asset_wait` and `disable_preempt()`
leaves preempt on unless an operator sets the variable, so removing it
changes whether an encoder spawns. It needs its own decision.

**Live, and wrongly listed (4).** These are not races that killing an
encoder creates, and no deletion of the coalescing family reaches them.
`restart_at` is the seek path itself — `HlsSessionRegistry::seek` calls it,
and this design keeps it. It does clear `pending_play_ms`, so it is not
independent of that family, but it cannot go with it.
`no_fill_release_for_new_land` neither reads nor writes any pending state.
`restart_spawn_gap` and `disable_preempt` read environment variables.

Three items removed alongside these — `RestartAtOutcome`, `segment_waiters`
and `preempt_defer_logged` — were never on the original list. Counting them
as part of it is what made an unbalanced ledger look balanced.

Spawn-and-reap answers all of those structurally. Nothing is abandoned while a
client is still asking for it, because the superseded encoder keeps serving its
land until it is reaped. The replacement is two steps: spawn, then reap after
the delay in decision 5. **There is no suspend step** — decision 5's amendment
of 2026-08-24 says why, and an implementer reading only this paragraph is
exactly the reader that amendment exists for. Rule 4.5 is satisfied by
subtraction.

Because the prior encoder outlives the seek, "the current run" stops being the
only live run. Anything reading a session-global maximum where it means one
particular run has to name the run: the encode frontier the throttle reads, and
any per-run cleanup that treats "not the current run" as "finished".

The 2 s IDR grid is load-bearing here and does not hold on Intel today
(ADR-0052). A long encoder makes that worse, not better, because it produces
for longer before anyone notices.

Copy and remux sessions keep ADR-0020's per-run map-assembled playlist. They
cut at source keyframes and cannot hold a uniform grid, which is the same
reason ADR-0020 gave and is unaffected by anything here.

Unmeasured, and not to be guessed: any Windows suspend behaviour. The reap
delay in decision 5 was measured on 2026-08-23 and is recorded there. Real seek and rung-hop rates are still uncaptured;
the origin sees every one of them in the GET stream, so they are an
observation to collect after shipping rather than a constant to invent.
