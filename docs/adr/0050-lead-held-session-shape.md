# ADR-0050: A transcode session is one throttled encoder holding a lead

- Status: **proposed**
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

   Reaping on a delay bounds memory at roughly seek-rate times delay while
   keeping decision 4's benefit. **The delay is not yet measured** and must
   be, before this ships. Do not guess it, and do not ship an unbounded held
   set.

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

`Session.child: Option<Child>` becomes a per-run set, and `restart_at`,
`may_kill_cooking_encode`, `coalesce_preempt_before_land` and the segment
waiter machinery are rewritten around spawn-and-reap rather than kill-and-
restart. That is the largest single change this ADR implies.

The 2 s IDR grid is load-bearing here and does not hold on Intel today
(ADR-0052). A long encoder makes that worse, not better, because it produces
for longer before anyone notices.

Copy and remux sessions keep ADR-0020's per-run map-assembled playlist. They
cut at source keyframes and cannot hold a uniform grid, which is the same
reason ADR-0020 gave and is unaffected by anything here.

Unmeasured, and not to be guessed: the reap delay in decision 5, and any
Windows suspend behaviour. Real seek and rung-hop rates are still uncaptured;
the origin sees every one of them in the GET stream, so they are an
observation to collect after shipping rather than a constant to invent.
