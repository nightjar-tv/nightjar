# ADR-0024: Ranked track selection

- Status: accepted
- Date: 2026-08-02
- Depends on: ADR-0012 (audio inventory / `trackId`); ADR-0010 (subtitle
  inventory); Phase 2 item 5 (forced / SDH, signed off)
- Gate: Phase 3 entry (rule only); profile persistence is Block 2

## Context

Session start and progressive playback still pick audio by container default,
else the first stream (`resolve_audio` in ADR-0012). Subtitle auto-select has
the same first-stream failure mode. That is the Emby bug: a file with no
`default` flag and alphabetical language order puts Arabic at the lowest
subtitle index, and English preference never gets a vote.

Evidence and proposed shape live in
`nightjar-meta/notes/design/track-selection-rules.md`. Two synthetic corpus
fixtures make the ambiguities concrete (Rule 4.3 — generated, not commercial):

| Fixture | What it holds |
|---|---|
| `h264_aac_adv_track_select_mkv.mkv` | 32 soft SRT tracks, none default; Arabic at lowest subtitle index; duplicate `eng` / `spa` / `por` / `chi`; SDH only in title (`English [SDH]`); commentary `eng` audio before Main |
| `h264_aac_forced_track_select_mkv.mkv` | Japanese audio; English forced + English full dialogue |

ADR-0021 remains reserved for client platforms (referenced by ADR-0022). This
number is 0024 on purpose — ADR numbers are a known collision hazard.

## Decision

### 1. One pure rank function for audio and subtitles

Selection is computed server-side from a preference language and the track
inventory. Clients never reimplement the rule. The function lives in
`nightjar-core` next to `decide_playback` (policy), not in the FFmpeg
orchestration crate.

Every result is either a `trackId` plus a short reason string, or no track
plus a reason. The reason is logged at resolve time and is the test
assertion. A rule that cannot explain itself in a short phrase is too clever.

### 2. Rank order (stated against the fixtures)

Candidates below the floor are discarded; never fall back to stream order.

1. **Language match** against the preference (ISO 639-1 after the same
   normalisation inventory already applies). No language match → not a
   candidate. Preference with no match → select nothing
   (`no track matched language <pref>`).
2. **Kind** — commentary, SDH / hearing-impaired, and signs rank below a
   normal dialogue track. On the adversarial fixture, Main beats Commentary
   and `English` beats `English [SDH]` for preference `en`.
3. **Forced mode (subtitles only).** Forced auto-selects only when the
   selected (or primary) audio language is foreign relative to the preference.
   On the forced fixture: preference `en` + audio `ja` → English forced.
   Preference `ja` (matching audio) → nothing, even though an English full
   track exists. Forced is identified by disposition **or** a closed title
   token (`forced`); absence of the disposition bit is not proof a track is
   not forced.
4. **Title / disposition tiebreak** against a short closed list only: SDH,
   CC, forced, signs, commentary (and common synonyms). Treat the rest of a
   title as decoration. `default` disposition is evidence among already-
   matching candidates, never an override that beats language.
5. **Image-track delivery cost** ranks below language (prefer a
   right-language text track over a right-language PGS). Coverage waits on
   the pending PGS corpus fixture; the term is named here so the order is
   not rediscovered later.

### 3. What this slice ships vs Block 2

| Now | Block 2 (needs profiles) |
|---|---|
| Rank rule + reason string | Profile default language |
| Preference input (server/test default `en`) | Series-scoped session override (description, never stream index); Off persists |
| Wire into `resolve_audio` and default subtitle resolve | UI display of the reason string |

Stored choice shape for Block 2: language + kind (+ SDH / forced flags) —
never a stream index. Release groups mix sources within a season.

### 4. Audio always needs a track

Subtitles may select nothing. Audio must pick something to map. After the
rank above, if no language preference is set, prefer a non-commentary track
with `default` disposition; if still tied, the honest last resort is the
lowest stream index among remaining candidates with reason
`first eligible audio track in file`. That last resort is for the no-
preference case only and must not run when a preference was given and missed.

### 5. Dogfood

Replacing container-default in `resolve_audio` changes which audio plays on
titles already in dogfood. Log the reason at session start from day one. A
badly changed title is usually a missing closed-list token (fixture /
title-list gap), not a reason to retune weights.

## Alternatives considered

**Keep container-default / first-stream until profiles exist.** Rejected: the
Emby bug is already the default path; waiting for accounts does not make
stream order meaningful.

**Per-item stored selection only (Plex model).** Rejected as the sole
mechanism: new episodes need daemons to rewrite state. Rank-from-rule handles
new files; session override layers on in Block 2.

**Fill ADR-0021 with this decision.** Rejected: ADR-0022 already depends on
0021 for client platform profile ids.

## Consequences

- `nightjar-core` gains `select_audio_track` / `select_subtitle_track` (or one
  shared entry with a media kind) and table tests driven by the two fixtures.
- `resolve_audio` stops honouring container-default as the sole rule.
- Transcode inventory grows disposition flags on text subtitle rows so forced
  / default evidence reaches the ranker.
- Image-track cost term stays untested until the PGS fixture lands.
- Public API preference wire can wait for Block 2 profiles; internal default
  `en` is enough for dogfood and tests (Rule 4.7).
