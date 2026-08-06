# ADR-0038: Track selection persistence

- Status: accepted
- Date: 2026-08-06
- Accepted: 2026-08-06, once ADR-0039 was accepted. It supplies the `series_key`
  item 4 stores against, including for unmatched show folders, and the migrator
  that rewrites it — in both directions, since a movie's series key is its own
  `item_key` (sheet Q7). Sign-off: `nightjar-meta/notes/design/adr-0039-0040-questions-2026-08-06.md`
- Depends on: ADR-0024 (the ranked selection rule and its reason strings, which
  this extends and does not restate); ADR-0012 (audio inventory, `trackId`);
  ADR-0010 (subtitle inventory); ADR-0034 (profiles); ADR-0039 (`series_key`,
  its grammar, its opacity, and the migrator that rewrites it), extending
  ADR-0033
- Gate: Gate 3 — full v1 API frozen
- Related: Block 2 plan B2-E and B2-8 (`nightjar-meta/docs/BLOCK_2_PLAN.md`);
  ADR-0024 §3, which named this slice as the Block 2 half of that decision

## Context

ADR-0024 shipped the rank rule with a server-side default preference of `en` and
listed what it could not do without profiles: a per-profile default language, a
session override that survives moving to the next episode, and a persisted "off"
for subtitles. Profiles exist now (ADR-0034), so this record supplies the
missing input and the storage, and changes nothing about the ranking itself.

The thing to get right is scope. A viewer who switches to the Japanese audio
track three minutes into episode one is not expressing a preference about every
title in the library, and is also not expressing a preference about only that
one episode. They mean this show, for now. Season is the wrong unit because
rolling into season two mid-binge drops the choice at the worst possible moment.

## Decision

**A profile carries one default language, and a per-series override layered over
it. Both are stored as descriptions, never as stream indices, and both feed the
existing ADR-0024 rank function.**

1. **One profile default language.** A profile gains a preferred language, an
   ISO 639-1 code, which is the preference input ADR-0024 already takes. One
   field serves audio and subtitles, because a household member who wants English
   audio and Japanese subtitles is expressing a per-title choice and that is what
   item 3 is for.

   The profile also carries a persisted subtitle default of `auto` or `off`.
   `off` is a real choice and has to survive, and it is not the same as having no
   preference: `auto` runs the ADR-0024 rule and may select nothing, while `off`
   selects nothing on purpose.

2. **The stored choice is a description, never a stream index.** A choice is
   language, kind, and the SDH and forced flags, matching the shape ADR-0024 §3
   already named. Release groups mix sources within a season, so index 3 in
   episode one and index 3 in episode two are routinely different tracks, and a
   stored index is a promise to select the wrong thing by episode four.

   Resolving a stored description against the next file's inventory runs through
   the same ADR-0024 rank function, restricted to candidates matching the
   description. There is no second ranker (Rule 4.11).

3. **The override is scoped to a series and persisted.** A choice made while
   watching an episode is stored against that episode's series and applies to
   every episode of it until changed. A movie is a series of one, so a choice
   made on a movie applies to that movie and nothing else without needing a
   second rule. Moving to different content falls back to the profile default,
   which is what "resets" means here: nothing is cleared, the new content simply
   has no override of its own.

   Persisted rather than held in memory, because a binge crosses app restarts and
   television clients are killed by the platform routinely. The row is small and
   the alternative is a choice that survives four episodes and then quietly does
   not.

4. **Storage.** `profile_track_choice` keyed on `(profile_id, series_key)`,
   holding the audio description, the subtitle choice, and a timestamp. The
   subtitle choice is three-valued: unset, off, or a description. Two-valued
   storage cannot tell "this viewer turned subtitles off for this show" from
   "this viewer has not chosen", and those two must behave differently.

   **`series_key`, not a general `scope_key`.** ADR-0039 item 2 defines it and
   ADR-0039 item 3 guarantees every series has one, because every show folder
   gets a row whether or not it matched. A movie's series key is its own
   `item_key`, so a movie is a series of one and needs no second column and no
   nullable case. The earlier `scope_key` framing had a hole
   in it that this closes: a path-keyed episode under an unmatched folder had no
   scope to hang a choice on, so the viewer who fixed the audio track on episode
   one of an unmatched show would have had to fix it again on episode two, which
   is exactly the population least likely to have a correct default.

   `series_key` is opaque on the wire for the same reason `item_key` is
   (ADR-0035 item 11, ADR-0039 item 2). Clients pass back what they received,
   and they receive it as `seriesKey` on every item response (ADR-0039 item 10).

   **When a folder binds, these rows are rewritten by the ADR-0039 item 7
   migrator**, not by this table's writer, on the same discipline ADR-0025 §5
   set for `item_key`. Two folders binding one show collide on
   `(profile_id, series_key)` and the newer `updated_at` survives; a preference
   has no partial state to merge the way a resume position does.

5. **Every selection carries its reason, and the reason reaches the client.**
   ADR-0024 already produces one. This ADR puts it on the wire so the track menu
   can show "English, matched your preference" against "Arabic, first track in
   file". That turns a six-episode evening into thirty seconds of diagnosis, and
   it constrains the rule honestly: a rule that cannot explain itself in a short
   phrase is too clever, and the reason string doubles as the test assertion.

6. **Precedence, stated once.** Explicit override for this scope, then profile
   default language and subtitle default, then the ADR-0024 rank rule, then
   ADR-0024 §4's audio last resort. Audio must always resolve to a track;
   subtitles may resolve to nothing, and below the floor they do. A
   wrong-language subtitle is worse than none, because silence is recoverable and
   wrong is a state the viewer has to notice, diagnose, and correct.

7. **Writing a choice is a profile-scope action on a profile-scope route.**
   `PUT /api/v0/profiles/{profileRef}/track-choice?seriesKey=…` records it, and a
   profile session may address only its own ref (ADR-0035 item 7). An
   account-scope session cannot write one, because account scope cannot start
   playback (ADR-0034 item 3) and a track choice with no playback is not a thing
   a user can mean.

   The key is a query parameter rather than a path segment for the reason
   ADR-0035 item 6 gives: a `folder:`-keyed series key contains slashes, and
   `%2F` in a path segment is rejected or normalised by nginx and by Axum's path
   extractor. The route shape is frozen at Gate 3, so getting this wrong breaks
   the unmatched fraction permanently.

## Alternatives considered

**Per-item stored selection only, the Plex model.** Rejected by ADR-0024 already,
restated because it is the obvious implementation: new episodes arrive with no
stored selection and need a daemon to write one, while rank-from-rule handles
them the day they appear.

**Season scope instead of series.** The narrower unit, and it is defensible for
anthology shows where seasons genuinely differ. Rejected for the binge case in
the context above; an anthology is handled by the viewer changing the track once
in the new season, which is one action rather than one per season.

**Separate audio and subtitle preferred languages on the profile.** More
expressive and it costs one column. Rejected under Rule 4.7: the case it serves
is per-title, item 3 covers it, and two fields invite a settings screen asking a
household member a question they do not have an answer to.

**Store the resolved `trackId` alongside the description as a fast path.**
Rejected: it is a cache that goes wrong silently on the next file, and the rank
function against an inventory we already have in memory is not the cost anyone
is optimising.

**Hold the override in the login session rather than a table.** Cheaper and it
matches the phrase "session override" in the plan. Rejected in item 3: television
clients are killed constantly, and a preference that evaporates on app restart is
worse than no persistence at all because the viewer cannot predict it.

## Consequences

**Good**

- The Emby failure ADR-0024 targeted stays fixed, and a viewer's correction to it
  now survives to the next episode.
- One rank function, one reason vocabulary, and one place a track decision is
  made.
- "Off" persists, which is the choice most products lose on the next file.

**Bad (accepted)**

- A stored description can fail to resolve when the next file genuinely lacks a
  matching track. The selection falls through to the profile default and the
  reason string says so, which is visible behaviour rather than silence.
- Series scope is wrong for anthologies and for shows whose dub quality changes
  between seasons. One correction per season is the accepted cost.
- A choice stored against a `folder:` key is lost when that folder is renamed,
  because the key is the path (ADR-0039). A choice against a `tmdb:show:` key is
  not. That is the fragility ADR-0025 §3 accepted for path keys showing up in a
  second place, and the viewer's cost is re-picking a track once.
- The image-track cost term from ADR-0024 §2 is still untested pending the PGS
  corpus fixture, and persistence does not change that.
- A profile with no language set behaves as ADR-0024's no-preference case, which
  falls back to the container default path for audio. That is the pre-profile
  behaviour showing through for a profile nobody configured.
