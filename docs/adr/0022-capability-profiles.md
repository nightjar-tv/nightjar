# ADR-0022: Client capability profiles (bitrate, resolution, HDR)

- Status: accepted
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
| `AETHER_V0` | Apple if ADR-0021 option (a) wins | **Modelled, not measured:** `decide_playback` rates on dogfood DB (~24 940 items, 2026-08-02): ~100% directPlay. Same modelled run: `APPLE_AVPLAYER_V0` ~**13.1%** directPlay (no Matroska; restricted codecs) — that gap is the argument for the Aether path. Matroska + wide codecs; client bridges audio AVPlayer rejects. Counts: `nightjar-meta/notes/t1-profile-counts-2026-08-02.txt`. |
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

**Host tonemap capability (slice 2).** HDR→SDR for an SDR encode path needs
FFmpeg `zscale` (libzimg). That is a host ceiling, same class as bitrate and
height: probe once at process start. When the source is HDR, the session
encode path requires tone-map, and `zscale` is absent:

- `decide_playback` still returns **transcode** (DirectPlay/Remux of HDR into
  an SDR H.264 session is wrong), with an explicit reason naming the gap
  (e.g. `host FFmpeg lacks zscale/libzimg; cannot tone-map HDR`).
- Session **start refuses before spawn** with **415** and that same reason.
  Hosts without zscale get a named failure, not a dead HLS session.

**Dolby Vision Profile 5.** Probe stores `dolby_vision_p5` when ffprobe reports
`dv_profile` 5. Decide returns **transcode** with reason
`Dolby Vision Profile 5 cannot be tone-mapped (IPT-PQ; no P5→SDR path)`;
session start **415**s with that reason. No tonemap attempt — P5 is IPT-PQ and
has no path through the current zscale+hable chain. Product support (e.g.
libplacebo+libdovi, or dovi_tool P5→P8.1 before encode) needs a new ADR before
those dependencies enter the transcode path. Do not conflate with missing HEVC
VUI colour tags on a fixture (same zscale error string, different cause).

**Packaging note.** Product Docker image installs Debian `ffmpeg` + VA drivers
(#19). Bookworm `libavfilter8` on **arm64** Declares `libzimg2` and contains
`zscale` / zimg symbols (`Depends` + `strings`; 2026-08-01). Bare binary still
accepts operator-provided FFmpeg on PATH. Missing `zscale` on a real host
remains a packaging or install gap; decide+415 makes that survivable at
runtime.

Regression coverage for the graph: committed synthetic PQ (`hevc_hdr10_mp4`)
and HLG (`hevc_hlg_mp4`) assert encode labels are BT.709; a measured
retag-vs-tonemap MAD floor proves **not-retag only** (not beauty) —
`notes/hdr-tonemap-delta-2026-08-01.md`.

**Beauty / product inspection (Proven by inspection, not measured).** Kit
titles: `notes/hdr-tonemap-beauty-2026-08-01.md` (2026-08-01). Product HLS
web-player inspection 2026-08-02 (Garrett): HDR10, P8.1, P7 MEL, P7 FEL
correct; P8.4 visual unknown; P5 failed session via named refuse —
`notes/hw/libplacebo-dv-spike-2026-08-02.md`.

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
- Slice 1: `profileId` on playback-info / stream / sessions; named
  `BROWSER_V0` / `MEDIA3_V0` / `MPV_V0`; bitrate / height / HDR ceilings force
  transcode; probe stores `video_bitrate_bps` and `hdr`.
- Slice 2: encode targets (`scale` + `-b:v` from ceilings), real HDR→SDR
  tonemap (`zscale` + hable), field-bag query overrides, `AETHER_V0`
  (modelled decide rates; not a measured client bake-off), host `zscale`
  probe with decide reason + **415 refuse-before-spawn** when tonemap is
  required and unavailable. Profile 5: named refuse, no tonemap attempt.
  MAD regression is not-retag only (`notes/hdr-tonemap-delta-2026-08-01.md`).
  Kit / product picture claims are **Proven by inspection** where dated —
  not measured Gate metrics.
- Client work (ADR-0021) cannot claim real-server direct play until this ADR
  is accepted and implemented.
- N100 measurement remains a hardware task on the Gate 2 long pole (with Pi 4
  for the scan carry). Unraid is the VAAPI/QSV verification host once iGPU
  enablement is confirmed.
- Product Docker image installs Debian `ffmpeg` + VA drivers (#19); see §4
  packaging note.
