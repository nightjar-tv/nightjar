# ADR-0051: Adaptive bitrate ships in v1

- Status: **accepted 2026-09-03**
- Date: 2026-08-23
- Supersedes: [ADR-0008](0008-abr-post-v1.md) §1. Sections 2, 3 and 4 of that
  ADR stand and are carried forward here as immediate obligations rather than
  as conditions for later.
- Depends on: ADR-0050 (the session shape a ladder runs on); ADR-0052 (GOP
  alignment across renditions); ADR-0022 (capability profiles, which pick the
  ceiling a ladder sits under)
- Gate: Gate 2 — the corpus plays in the web player
- Measured 2026-08-20 on an Apple M1 (VideoToolbox): the three-rung shape,
  human-stable, with a High-only native master measured and rejected. **That
  spike ran cook-on-miss with 20 s windows and MPEG-TS**, a session shape this
  ADR's own dependencies have since replaced, so the rungs are unverified
  against the shipping shape. Method and raw data are maintainer-private

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

---

## Amendment 2026-09-04 — the mechanics a ladder needs, decided before S6 is built

This ADR decided **that** a ladder ships and **which three rungs**. It did not
decide how one session serves three renditions, and a recon of the tree before
scoping S6 found four places where the current shape cannot express a ladder at
all. Deciding them in the slice would bury them; they are decided here.

### 1. A rung is a path segment, not a query parameter

Variant URIs become:

    /api/v0/sessions/{session_id}/v/{rung}/index.m3u8
    /api/v0/sessions/{session_id}/v/{rung}/seg_<ms:011>.m4s

mirroring the existing `runs/{run_id}/` precedent rather than inventing a shape.

A query parameter was the cheaper option — the segment route already parses one,
so no route would change. It is rejected because it leaves the segment namespace
flat, and decision 2 shows the namespace is exactly what has to become
rung-scoped. Paying for a route change here avoids a name-mangling scheme like
`seg_r1_<ms>.m4s`, which would break `parse_time_keyed_segment_name` and the
closed `is_safe_asset` set that keeps the session catch-all safe.

**This is the amendment `authority.rs`'s test asks for.** That test asserts
`COOKIE_ACCEPTED_ROUTES.len() == 9` and says in its own comment that changing
the count "is an ADR amendment". It is hereby changed: the two routes above join
the cookie-accepted set, and the test's expected list and count move with them.

### 2. The segment map is per rung

`SegmentMap` is keyed on `start_ms` alone, one map per session. Three rungs on
one grid produce segments at **identical** `start_ms` by construction — that
identity is what ADR-0052 exists to guarantee — so a single map means the last
encoder to write wins and rung selection becomes a race.

Each rung gets its own `SegmentMap`, and the run directory gains a rung level:

    {session}/v{rung}/run_{n}/{init.mp4, seg000.m4s, …}

Per-rung maps rather than a `(rung, start_ms)` composite key, so every existing
consumer — `get`, `overlapping`, `remove_run`, `run_is_referenced`, the eviction
walk — keeps working unchanged against one rung's map.

**`SESSION_RUN_CACHE_BUDGET_BYTES` is per session and now spans three rungs.**
S6 does not change the number. It is named here because a budget chosen for one
rendition is being asked to hold three, and the first eviction surprise should
find this sentence rather than look like a bug.

### 3. Audio stays muxed per rung in v1

Audio is muxed into the video segments today (`-map 0:v:0 -map <audio>`), and
there is no `EXT-X-MEDIA:TYPE=AUDIO` group anywhere in the product. Three rungs
therefore carry three copies of the audio.

That is accepted for v1, with the cost stated: at 192 kbps it is about 576 kbps
across the ladder against rungs of 6M, 3M and 2M — roughly 5% of the top rung and
**about 10% of the bottom one**. The line in this ADR's original text that
encoder load "becomes video plus audio per rung rather than a constant" is that
cost, now quantified.

Demuxing audio into its own group is the right end state and is **not** this
slice: it needs an audio playlist, an `AUDIO=` attribute on every `STREAM-INF`,
and a player path that selects it. Recorded as a follow-up rather than smuggled
into S6.

**Constraint on the ladder while audio is muxed:** every `STREAM-INF` must carry
the same `SUBTITLES` group, or hls.js loses subtitles on a rung hop.

### 4. The session cap must count encoders before the ladder is enabled

> **Discharged 2026-09-05 by [#224](https://github.com/nightjar-tv/nightjar/pull/224).**
> The first option below shipped: the constant is now `DEFAULT_MAX_ENCODERS`,
> it counts live encoding processes across every session and rung, and
> superseded encoders count too. The undercount this section names is closed.
>
> **The text below is left as written.** It describes the tree before #224, and
> its last paragraph is the part that still governs: this section never claimed
> to settle admission. S7 does that, against ADR-0050 §7-§8, and removes the
> fixed number entirely.

`DEFAULT_MAX_SESSIONS = 3` counts entries in the session map. A rung is not a
session, so three sessions each cooking three rungs is **nine encoders** against
a cap that reads three — on hardware where this ADR's own rung measurements are
still marked unverified.

The cap is already an undercount: superseded encoders are held live for
`REAP_AFTER` and are not counted either. A ladder does not open that gap, it
multiplies it.

**So S6 does not ship a ladder that is on by default against a session-counting
cap.** Either of these discharges it, and the slice picks one on evidence:

- re-express the cap in encoder units — count live encoding processes rather
  than sessions, which is a small change at the admission site and needs a live
  count that does not exist yet; or
- ship the ladder behind a setting that defaults to one rendition, so nothing
  regresses, and let S7 turn it on when admission is real.

This does not pre-empt S7. S7 decides admission on *measured load*; this decides
only that the existing cap must stop lying before a ladder can be enabled.

### 5. What this amendment does not decide

**Whether the rungs are right.** 1080p at 6M, 1080p at 3M, 720p at 2M is
unchanged from the original decision and still rests on a 2026-08-20 M1 spike
that ran a different session shape.

**How rungs are filtered against a client ceiling.** `ClientCapabilityProfile`
carries `max_bitrate_bps` and `max_height`, both `None` in every shipped profile,
and today `max_height: Some(1080)` on a 1080p source means "do not scale" rather
than "this rung is 1080p". Those two readings of one field conflict and S6 must
pick one, but which is not decided here.

**The efficiency question against cook-on-miss.** Measured 2026-09-03 on the
N150: under a hopping client the lead-held shape burns about 8.5x the encode of
cook-on-miss windows for the same delivered media, after abandoned encoders are
reaped. That is a live question against ADR-0050 and this ADR together, recorded
as `OPEN-DEFECTS` entry 29, and it is not settled by building the ladder.
