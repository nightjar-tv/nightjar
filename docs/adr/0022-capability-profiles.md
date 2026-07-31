# ADR-0022: Client capability profiles (bitrate, resolution, HDR)

- Status: proposed
- Date: 2026-08-01
- Depends on: ADR-0008 §6; ADR-0012 (channel ceiling); ADR-0021 (which
  profile ids exist once clients ship)
- Gate: Phase 2 / Gate 2; prerequisite for non-`BROWSER_V0` `/stream`

## Context

Today every decision and every `/stream` gate uses `BROWSER_V0`: H.264 + AAC
in MP4/MOV, stereo. That is correct for the web player and wrong for every
engine that Matroska-direct-plays. Nightjar `/stream` returns 415 on MKV
because the server assumes the browser profile. Bake-off direct-play
measurements had to stand up a side byte server for that reason.

ADR-0008 §6 said profiles grow in Phase 2 with max bitrate, max resolution,
and HDR. ADR-0021 lists which profile ids the product will need once clients
exist. This ADR defines the wire shape and decision rules so writers and
clients can land without inventing fields twice.

## Decision

### 1. Profile is client-reported data

The client sends a profile id (and, when the id is unknown to the server, the
field bag below). The server decides the playback method. Clients never pick
direct play / remux / transcode from local heuristics. That boundary is the
Dart player interface rule in ADR-0021 and applies to the web client too.

### 2. Profile ids this phase must define

| Id | Used by | Notes |
|---|---|---|
| `BROWSER_V0` | Web (and today's anonymous default) | Already shipped. Codecs + containers + `maxAudioChannels: 2`. |
| `MEDIA3_V0` | Android / Android TV Flutter | Scored in bake-off (~96.5% DP on dogfood). |
| `MPV_V0` | Windows / Linux media_kit; bake-off libmpv floor | Scored (~100% DP on dogfood). |
| `AETHER_V0` | Apple if ADR-0021 option (a) wins | **Unscored.** Needs a `t1_profile_counts.py` run before Gate 1-style DP language. |
| Tizen / webOS model-year ids | Vendor Flutter shells | Capability varies by TV year; client must report what that stick can decode. |

Ids are additive. Unknown id + full field bag still decides; unknown id with
no fields falls back to `BROWSER_V0` and logs the gap.

### 3. Fields (additive on `ClientCapabilityProfile`)

| Field | Meaning |
|---|---|
| `videoCodecs` / `audioCodecs` / `containers` / `extensions` | Already present. |
| `maxAudioChannels` | Already present (ADR-0012). |
| `maxBitrateBps` | Source video bitrate above this → session at a target at or under the ceiling. `null` = no ceiling (LAN engines). |
| `maxHeight` | Source height above this → scale in transcode. `null` = no ceiling. |
| `hdr` | `none` \| `hdr10` \| `dolbyVision`. Prefer passthrough when the profile accepts the source; tone-map only when it does not. |

Exact OpenAPI names land with the implementation commit (same commit as the
schema change per docs/GIT_RULES.md §2).

### 4. Decision engine additions

When codecs would remux or direct-play but bitrate or height exceeds the
profile, the method becomes **transcode** (or a bandwidth remux is not
enough: video must be re-encoded to meet the ceiling). Reason strings name
the field that fired (`source bitrate exceeds profile maxBitrateBps`, etc.).
HDR mismatch is its own reason and selects tone-map vs passthrough in the
FFmpeg graph; it does not silently strip HDR.

ABR ladder selection stays post-v1 (ADR-0008). v1 still picks one server
rendition (Auto / High / Original) from the profile ceiling.

### 5. `/stream` and sessions

`GET /api/v0/items/{id}/stream` honours the reported profile. Until a client
sends one, behaviour stays `BROWSER_V0` (today's 415 on MKV is correct for
anonymous browser callers). Session start accepts the same profile input so
decision and encode target agree.

### 6. N100 capacity (Gate 2 companion measure)

Before calling Gate 2 sized, measure how many concurrent 1080p transcodes an
Intel N100 sustains at the Auto target this profile ADR implies. Record the
number next to the Gate 2 checklist; do not guess from `NIGHTJAR_HLS_MAX_SESSIONS`.

Hardware long pole for Gate 2 / Phase 2 entry (after Unraid): **N100** (this
measure) and **Pi 4** (ADR-0005 scan carry). Unraid covers VAAPI and QSV for
the support matrix — confirm the box's iGPU is actually enabled in BIOS before
counting QSV (a discrete Arc card sometimes leaves the iGPU off).

## Alternatives considered

**Server-guessed profiles from User-Agent.** Rejected: Tizen/webOS model-year
capability and Flutter engine choice cannot be inferred reliably from UA.
Client-reported data is the boundary that avoids Jellyfin's wrong-method
failure mode.

**Keep only `BROWSER_V0` until Phase 4 clients ship.** Rejected: `/stream` stays
unusable for dogfood engines and Gate 2 "plays everything" cannot include
direct play on Matroska for the clients we already decided exist.

**Fold bitrate into ADR-0008 ABR now.** Rejected: ADR-0008 parked ABR post-v1;
a single ceiling for Auto is enough for Gate 2 remote watchability.

## Consequences

- OpenAPI + `decide_playback` + `/stream` grow additively; no break inside `/v0`.
- Bake-off Step 2 profile work has a home; `AETHER_V0` stays unscored until
  counted.
- Client work (ADR-0021) cannot claim real-server direct play until this ADR
  is accepted and implemented.
- N100 measurement remains a hardware task on the Gate 2 long pole (with Pi 4
  for the scan carry). Unraid is the VAAPI/QSV verification host once iGPU
  enablement is confirmed.
