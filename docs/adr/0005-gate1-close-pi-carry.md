# ADR-0005: Close Gate 1 with Pi hardware scan carried forward

- Status: accepted
- Date: 2026-07-25

## Context

Gate 1's remaining open criteria were (1) the ADR-0004 index-pass numbers on a
Raspberry Pi 4, and (2) dogfooding begun. The software criteria (browsers, idle
RAM, kill -9 / startup, corpus structured fails) already pass. Dogfooding has
started: a household build is serving real library playback.

The Pi 4 named in ADR-0004 and the plan is not available. A Pi 3 is. Treating a
Mac Air proxy, a cloud ARM VM, or an unmeasured Pi 3 as a Pi 4 pass would make
the gate dishonest. Quietly starting Phase 2 with those boxes open would violate
the plan's "no next phase until the gate passes" rule and Rule 6.4.

## Decision

### 1. Gate 1 closes

Gate 1 is passed for the purpose of starting Phase 2 when:

- All software Gate 1 criteria remain green (as recorded in the plan checklist).
- Dogfooding has begun: at least one household runs this build as a daily
  player (Rule 4.6 / standing rule 1). A full title playing from the household
  library counts as the start of that clock, not as permanent completion of
  continuous dogfooding.

### 2. Pi hardware scan is carried forward, not waived

The ADR-0004 index-pass criterion (10k index under 60s, unchanged rescan index
under 5s, probe floor recorded) is **not** marked passed. It moves to a Phase 2
entry condition that must land before Gate 2:

- Run `scripts/gate1_scan_10k.sh` on the available **Raspberry Pi 3** as soon as
  practical. Record index seconds, rescan seconds, and probe files/sec in the
  plan checklist.
- If those numbers miss the ADR-0004 budgets, write a follow-up ADR that sets
  Pi-3-calibrated floors. Do not silently keep claiming "under 60s on a Pi 4."
- When a Pi 4 (or equal BCM2711 board) is available, re-run and record. The
  public "runs on a Pi" claim at launch still needs a measured number on the
  slowest board we commit to support.

Mac Air proxy numbers remain scaffolding only.

### 3. What this does not change

- ADR-0004's async scan API and index-vs-probe split stand.
- Marketing and README must not claim a Pi scan time until a Pi harness run
  exists.
- Continuous dogfooding continues through Phase 2; starting the clock is not
  the same as finishing Gate 3's grandma gate.

## Consequences

Phase 2 (transcoding) may begin. The first Phase 2 checklist item is the Pi 3
harness run. Gate 2 cannot pass while the carried scan criterion is still blank.
