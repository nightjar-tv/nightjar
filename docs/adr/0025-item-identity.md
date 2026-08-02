# ADR-0025: Item identity

- Status: accepted
- Date: 2026-08-02
- Depends on: none (Phase 3 Block 1 root; watch state and metadata writers
  depend on this)
- Gate: Gate 3 — watch history survives rename, re-encode, and library
  remove-and-re-add
- Related: ADR-0022 §5 (version selection under effective ceiling, follow-up);
  watch-history design note
  (`nightjar-meta/notes/design/NIGHTJAR_WATCH_HISTORY_SPEC.md`); Phase 3
  Block 1 (`nightjar-meta/docs/PHASE_3_REVISED.md`)

## Context

Watch state and playback events must key on a logical item, not a file.
Internal `media_items.id` values churn when a library is removed and
re-added. File paths change on rename and remount. Content hashing 24,800
files over SMB is bandwidth-bound and serial by measured scan rules, and
metadata matching lands in the same phase anyway.

Phase 3 and the watch-history design note already settled the claim:
provider-id keys, path-key fallback for unmatched, no cascade-delete of
watch state on library remove, and the manual metadata-fix flow owning
watch-state migration when a provider id is reassigned. This ADR records
those decisions and closes the gaps that would otherwise leave the scanner
picking: episode key shape, file↔item cardinality, client opacity of the
key string, what the path fallback binds to, and how colliding watch rows
merge under a key change.

Dogfood inventory (`~/nightjar-data/nightjar.db`, 24,940 items) that bears
on the episode-key choice: 127 anime-series paths (Jujutsu Kaisen, Frieren,
Mushoku Tensei, One Piece, Bleach, and others), 16 files under
`Specials/`, deep multi-season TV, 33 multi-episode `NxMM-NN` files, and
dual 1080p versions of the same film (Bluray + WEBDL). No CD1/CD2 split-
film pairs are present today; the identity shape is still decided because
the library size class requires it. Named "Part 1" / "Part 2" sequels
(Deathly Hallows, Mockingjay, …) are separate TMDB movies and are not that
case.

## Decision

### 1. Logical item key

Watch state and playback events key on `item_key`, a string naming a
logical item. Clients treat `item_key` as opaque. The grammar below is a
server implementation detail documented for debugging; it is not a public
parse contract and must not be relied on by clients before or after the
Gate 3 API freeze.

Canonical auto-match provider is TMDB (matches the metadata pipeline:
NFO first, then TMDB).

| Kind | Key | Notes |
|---|---|---|
| Movie | `tmdb:movie:{id}` | Movie id on the detail record already fetched |
| Episode | `tmdb:episode:{id}` | TMDB episode object `id`, **not** show+season+episode |
| Unmatched | `path:{library_id}:{relpath}` | See §4 |

Season and episode numbers remain display and matching metadata. They are
not identity. Provider renumbers (season reshuffles, specials moved into
season 0, absolute-numbered anime re-slotted) are not a manual fix, so
the migrator would never run; keying on the tuple would silently orphan
watch state. Dogfood contains anime, specials, and deep multi-season
shows — that failure mode is inventory, not theory.

NFO-only non-TMDB ids (for example `tvdb:…`) may be the key until a TMDB
id is known. Upgrading to TMDB is a key migration (§5).

### 2. File↔item is many-to-many

`item_key` does not imply one file. Media files and logical items are
separate; a join (or equivalent) links them. Current file uniqueness on
`(library_id, path)` stays on the file side.

| Case | Identity |
|---|---|
| Multiple versions of one film (4K remux + 1080p, or Bluray + WEBDL) | One `item_key`, several media files |
| One movie split across part1/part2 (CD1/CD2) | One `item_key`, several media files |
| One file containing S01E01–E02 | Two `item_key`s, one media file |

Playback spanning, shared timelines, and UI are out of scope. This ADR
locks identity cardinality only.

Which version plays among files sharing an `item_key` is a
decision-engine question, not a scanner one. See ADR-0022 §5 (follow-up)
and §6 below.

### 3. Lifecycle

Library removal does **not** cascade-delete watch state. Re-adding and
rematching reattaches history for provider-keyed items. That is the Gate 3
claim this ADR exists for.

Unmatched path-keyed items lose history on rename and on library
remove-and-re-add. Plex and Jellyfin behave the same way; product docs say
so rather than pretending otherwise.

### 4. Path fallback and the unmatched-episode case

`path:{library_id}:{relpath}` binds to `libraries.id` (SQLite
`INTEGER PRIMARY KEY AUTOINCREMENT` in the data dir under
`NIGHTJAR_DATA_DIR`). Relative path is relative to that library root.

**Why `library_id`, knowingly:** the same ADR rejects keying watch state
on internal *media* row ids because those churn. Library rows are few and
operator-created. The id survives container recreate and edits to the
library's configured path when the data volume persists — the durability
class of the SQLite file that holds watch state anyway. It does **not**
survive wiping the data dir, or DELETE library + re-add (new id). Keying
on the configured root path instead would orphan unmatched history on a
remount (`/Volumes/media/...` vs `/mnt/user/media/...`) that matched
history must survive. Pick the remount-stable handle; accept
delete-and-re-add fragility for unmatched only.

**When the path key applies:**

- A home video or obscure file with no provider id → path key.
- A show that matched, but an individual episode under it failed to match
  → that episode still gets a **path key**. A matched parent does not
  mint episode identity. There is no "nothing" state for an indexed file:
  every indexed file that can carry watch state has an `item_key`. Leaving
  this unspecified would let the scanner invent a third shape.

### 5. One migrator for every key change

Any change of `item_key` runs the same code path: first match
(path → provider), manual mismatch reassignment, NFO-only → TMDB upgrade.
Manual-fix owns calling it; shipping the fix API without the migrator is
a bug. The same owner clears stale collection linkage when that column
exists (collections storage is separate Block 1 work; the obligation is
named here so it is not missed).

Rewrite `watch_state.item_key` and, when the table exists,
`playback_events.item_key`. If both old and new keys already have a watch
row for a profile, merge then delete the loser.

**Merge rule** (optimises for resume survival — the Gate 3 claim):

1. Higher position: if both rows have `duration_ms`, compare
   `position_ms / duration_ms`; otherwise compare absolute `position_ms`
2. Then `played = true`
3. Then newer `last_played_at`

Played-first would let a low-position "marked played" row beat an 80%
resume under rematch. That is backwards for the survival test. Manual
mark-played vs dual-key mid-watch during migration is rare; if the
watch-state ADR wants different mark-played merge semantics it can say
so there.

### 6. Version selection is not this ADR

Among files sharing an `item_key`, which file plays is decided under the
effective capability/policy ceiling (ADR-0022 §5): pick the highest
version the ceiling permits, and prefer a direct-playable smaller file
over transcoding a larger one. A remote user capped at 1080p should get
the 1080p file, not a transcode of the 4K. That follow-up reuses the
ADR-0024 rank-function *shape* (pure server-side function + reason
string), not the track ranker itself. Library view collapsing versions to
one card, and the version affordance on the item page, are Block 3 UI and
are currently unaccounted for — noted, not designed here.

## Alternatives considered

**Season/episode tuple as the episode watch key
(`tmdb:tv:{show}:s{SS}e{EE}`).** Rejected: renumber orphans watch state
with no migrator trigger. TMDB episode ids exist; use them.

**Content hash as primary key.** Rejected: hashing the dogfood library
over SMB is bandwidth-bound and serial; metadata matching is in-phase.

**Internal `media_items.id` as watch key.** Rejected: row ids churn on
remove-and-re-add; Gate 3 would fail by construction.

**Cascade-delete watch state on library remove.** Rejected: contradicts
the Gate 3 remove-and-re-add survival claim.

**Key unmatched items on the configured library root path.** Rejected:
remounts change the string; `library_id` does not.

**Fold this into the watch-state ADR.** Rejected: Rule 4.9 and the
watch-history sequencing require identity first; everything below
consumes it.

**Leave per-episode-unmatched identity to the scanner.** Rejected: path
key (§4); no third shape.

## Consequences

- Scanner and metadata writers set `item_key` at match time; unmatched
  (including unmatched episodes under a matched show) stay on the path
  key.
- **Episode-id acquisition.** Movie and show ids arrive on the detail
  record already fetched. Per-episode ids do not: the metadata pipeline
  must pull them via a season append (`append_to_response` season/N) or
  the season endpoint — one call per season, not per episode. Cheap, but
  a request shape the pipeline ADR (ADR-0026) inherits rather than discovers.
- Manual fix API cannot ship without the migrator.
- Gate 3 needs an explicit test: rename + re-encode + library
  remove-and-re-add → resume still attaches for a matched title.
- Kids allowlist / classification later keys the same logical item.
- Version selection follow-up lives on ADR-0022 §5 / decide_playback;
  check ADR-0024 shape before writing a second ranker. Block 3 owns
  version collapse UI.
- Watch-state and playback-events ADRs (Block 2) consume `item_key`; they
  do not redefine it.
- **Fragile watch state under the match floor (ADR-0026).** Items below
  confidence 0.80 stay on the path key, so the below-floor rate is the
  fraction of the library whose watch state dies on rename and on library
  remove-and-re-add. Calibration sample: 17/280 (6.1%). Library-weighted
  projection from those rates onto 24,940 dogfood files: about 11%. The
  Gate 3 full-library measure at threshold 0.80 owns the real fraction;
  raising coverage (cleaner, year extraction) shrinks it without changing
  this identity rule.
