# ADR-0051: Adaptive bitrate ships in v1

- Status: **proposed**
- Date: 2026-08-23
- Supersedes: [ADR-0008](0008-abr-post-v1.md) §1. Sections 2, 3 and 4 of that
  ADR stand and are carried forward here as immediate obligations rather than
  as conditions for later.
- Depends on: ADR-0050 (the session shape a ladder runs on); ADR-0052 (GOP
  alignment across renditions); ADR-0022 (capability profiles, which pick the
  ceiling a ladder sits under)
- Gate: Gate 2 — the corpus plays in the web player
- Related: `nightjar-meta/notes/hw/stay-ahead-vt-2026-08-20.md` § Locked spike
  decisions, which measured the three-rung shape

## Context

ADR-0008 parked adaptive bitrate after v1. Its reasoning was that v1 already
picks one rendition server-side from the client capability profile, that a
ladder would reopen an encode path which took several rounds of patches to
stabilise, and that most household viewers on a LAN would not need it.

It also identified two things that had to be right in v1 regardless, because
changing them later invalidates every cached segment and blocks clean
switches: segment duration, and keyframe alignment across renditions. Those
were locked at the time as conditions ABR would later depend on.

The judgement has changed. Recovery-prone playback and ABR are both wanted in
v1, because the server-to-client contract is cheaper to get right before the
Flutter clients exist than after. A ladder is not a feature bolted onto a
session; it is the same session contract with more than one rendition, and
the client picks. Building the contract once, with the ladder in view, avoids
designing it twice.

The title of ADR-0008 asserts the opposite of this decision, so it cannot be
amended in place. This record supersedes its first section only.

## Decision

1. **ABR ships in v1.** A transcode session offers a master playlist with
   more than one video rendition. The client chooses, and switches as
   conditions change. Manual quality selection stays.

2. **Three rungs: 1080p at 6M, 1080p at 3M, 720p at 2M.** Measured on the M1
   in the 2026-08-20 spike and human-stable there. All three transcode on the
   same 2 s IDR grid. Every client gets all three, including native iOS. A
   High-only native master was measured and rejected. Do not "fix" an iOS hop
   by removing rungs.

3. **ADR-0008 §2, §3 and §4 carry forward and are now load-bearing.** The 2 s
   segment lock, time-based forced IDRs, and additive playlist URLs stop
   being preconditions for a later feature and become requirements of the
   shipping product. `SEGMENT_MS` remains the single owner of that value
   (Rule 4.9).

4. **The client picks; the server does not switch for it.** The server offers
   the ladder and serves whatever is requested. Rule 2.1 puts logic on the
   server, and rendition choice is the exception the HLS contract already
   assigns to the player, which sees the throughput the server cannot.

5. **A rung hop starts an encoder for the new rung and does not kill the
   old one.** The same policy as a seek (ADR-0050 §4), for the same measured
   reason. A hop abandons whatever was cooked ahead on the rung it leaves,
   and that work is dead even if ABR oscillates back later, because the
   playhead moves forward at 1x and never returns to that media time.

6. **Hop cost is an observation to collect, not a constant to guess.** A
   synthetic 60 s hop period cost about one extra window of encode per hop
   and moved overproduction from 1.02 to 1.30-1.49. No real hop rate has ever
   been measured. The origin sees every rung change in the GET stream, so
   this gets collected after shipping. Do not tune against the synthetic
   figure.

## Consequences

`build_master` currently emits one `EXT-X-STREAM-INF` with a hardcoded
`BANDWIDTH=5000000`. It grows real variants, and the advertised bandwidth
becomes video plus audio per rung rather than a constant.

Admission (ADR-0050 §7) gains its natural form: a newcomer arriving at a
short box gets a reduced rung set rather than a refusal, and an incumbent is
never degraded.

Encoder load per session rises with the number of live rungs, so the capacity
signal in ADR-0050 §8 measures encoder work rather than session count.

ADR-0022's line that "ABR ladder selection stays post-v1" is superseded here.
Its bitrate and resolution ceilings still decide which rungs a given client
may be offered.

ADR-0008 stays in the register as partially superseded. Read §2 through §4
there; its §1 is dead.
