# ADR-0046: Multi-entity binding, one browse unit per folder

- Status: **proposed**
- Date: 2026-08-14
- Supersedes: the tiling sketch (one browse unit per provider entity, year on
  every tile), which was never written as an ADR and is refuted here
- Depends on: ADR-0033 (folder-keyed series rows, extended not replaced);
  ADR-0039 (the show entity and `series_key`, whose folder edge this keeps);
  ADR-0025 §1 (`item_key` grammar), §5 (one migrator per key change);
  ADR-0026 §2 (scoring floor), §8.6 (the show browse unit); ADR-0028 (manual
  fix, which owns every key change); ADR-0029 §1 (entity-keyed canonical
  projection); ADR-0035 item 8 (canonical season ordering); ADR-0037
  (certification along the entity edge)
- Gate: Gate 3 — metadata auto-match ≥95% correct; every mismatch fixable
  in-UI in under 30 seconds; rescan of an unchanged library issues no search
  requests
- Related: `nightjar-meta/notes/tmdb-entity-splits-2026-08-13.md` (the
  measurement this record rests on); `nightjar-meta/notes/design/adr-0045-multi-entity-questions-2026-08-12.md`
  (question sheet, written against the refuted design, superseded in framing)

## Context

A show folder can hold files belonging to more than one provider entity. Will &
Grace holds S1–S11 while TMDB models 1998 as 4454 and the 2017 revival as
74321; 51 of its files carry episode titles that exist only on 74321 and are
undescribed today. Monster holds three anthology installments TMDB models as
three entities. Battlestar Galactica (2003) holds a miniseries TMDB types
separately from the series. Grand Designs holds one episode of a spin-off.

`series` binds one folder to one `tmdb_show_id` (ADR-0033, migration 016), so
the second entity has nowhere to live and its files stay unmatched.

**The design this record replaces.** A sketch — *a folder spanning several
entities produces one browse unit per entity, with the year on every tile* —
was derived from those four folders and written as though it generalised.
Measured against TMDB across 697 shows, **it does not**, and the sketch is
refuted rather than deferred. The measurement is the related note; the three
findings that carry this record are below.

### The revival argument was a matching argument wearing a browse argument's clothes

Two certifications, two season number-spaces, `group_by_season` (`browse.rs`,
ADR-0035 item 8) interleaving 1998's S1 with 2017's S1, and three producers of
`series_key` disagreeing are all real. **Every one of them is a reason to bind
files to the right entity and to present the folder's numbering. None is a
reason to mint a second poster.** ADR-0037 already reads certification per item
along the entity edge, and that stays true inside one unit.

### The anthology case is where separate tiles are worst

The folder is `Monster`. Three tiles would be named *DAHMER - Monster: The
Jeffrey Dahmer Story*, *Monsters: The Lyle and Erik Menendez Story* and
*Monster: The Ed Gein Story*. **None of them is the folder.** American Horror
Story, Fargo, True Detective and Black Mirror are already that shape *inside a
single TMDB entity*, so splitting would make Nightjar more splitting than TMDB
is, and invent a third model for the anthologies TMDB happened to split.

### Battlestar would undo the user's own filing

The miniseries is in `Specials/` deliberately — the TVDB convention, filed
against a TMDB id, which Plex documents as a provider disagreement and not a
feature. The miniseries being a different production (TMDB `type` says so) is a
**matching fact, not a browse fact**.

### What every other client does

Plex, Jellyfin, Emby and Kodi all make the folder the browse unit and bind one
provider id at show level. Plex's Fix Match, Jellyfin's Identify and Kodi's
manual id search are all show-level; none offers "this season is a different
id". Users who want two units make two folders and put a year or a
`{tmdb-…}` hint on each. A design that turns one folder into several tiles
surprises both populations: the splitter already has two folders, and the
lumper asked for one unit.

## Decision

### 1. One browse unit per folder. Always.

**A show folder produces exactly one browse unit, whatever relationship holds
between the provider entities its files belong to.** Tested against eight
relationship types, all present in the measurement:

| relationship | example | units |
|---|---|---|
| revival / same-title continuation | Will & Grace 1998 + 2017 | **1** |
| miniseries + continuing series | Battlestar 2003 + 2004 | **1** |
| anthology, separately titled | Monster / DAHMER / Menendez / Gein | **1** |
| franchise spin-off / companion | Grand Designs + Deconstructed | **1** |
| sequel miniseries, different subtitle | Farscape + Peacekeeper Wars | **1** |
| regional remake | The Office US + UK | **1** |
| anime cour split | Horimiya 2021 × 2 | **1** |
| duplicate provider rows | Peppa Pig 2004 × 2 | **1** |

**The corollary is what does the work: several folders give several units.** A
user who wants Will & Grace's revival separate makes a second folder. A user
who wants `Grand Designs: Deconstructed` as its own tile has already made it
one. **The folder is the user's decision and Nightjar does not override it in
either direction** — it neither splits a folder the user lumped nor merges
folders the user split.

Two consequences follow and are not separate decisions. A regional remake in a
single folder is a mis-organisation, not a tiling case; it is the ADR-0028
single-file assign path. And search residue never mints a tile — Grand Designs
returns 17 prefix siblings, Doctor Who 18, The Traitors 17 — which is an
argument for **not promoting search residue**, not for a year on every tile.

### 2. A folder may bind to several entities

This is the substantive change and it is what fixes Will & Grace's 51 files.

**Shape (Rule 4.9, decided here, written later).** `series` keeps
`tmdb_show_id` as the **primary** binding, and a new join table records every
entity a folder binds to, keyed on the same `(library_id, relpath)` pair:

- one row per (folder, entity)
- the folder-season range that binding covers, and what those seasons are
  called on the entity (see item 3)
- the primary binding is also a row, so there is one shape rather than a
  special case for the first entity

A nullable second column on `series` is rejected: Monster is already three, and
a column named for a cardinality is a shape you replace rather than extend. A
per-item provider id on `media_items` is rejected under Rule 4.11 —
`media_item_links` already holds the per-file provider binding.

**Migration ordering.** B2-0's `021_series_binding_without_entity.sql` relaxes
`tmdb_show_id` to nullable so a folder gets a row when it forms a group rather
than when it matches. **021 and this are orthogonal and 021 comes first**: 021
is a folder with *no* entity, this is a folder with *more than one*, and the
join table's foreign key is the `(library_id, relpath)` pair either way. This
record does **not** unblock 021, which is held by the scanner freeze for its
own reasons. The migration number is taken at implement time; nothing is
reserved.

**What already exists, because it narrows the work.** `media_item_links`
already persists per-file entity binding. `seasons_skipped` in
`bind_resolved_items` (`queue.rs`) is already computed from a call the drain
already makes. **The missing piece is only the folder→entity record that
suppresses the re-search** — without it, identity would be re-derived from
search on every drain, which is what ADR-0033 item 1 rejected.

**Existing rows.** A single-entity folder is one row in the join table with a
range covering every folder season, so migration is a copy rather than a
re-derivation, and a folder that never spans keeps behaving exactly as it does
today.

### 3. Numbering is the folder's, and this is now load-bearing

Under one unit the folder holds S1–S11 while 74321 numbers its own seasons
S1–S3. Something must record the mapping or `group_by_season` puts two runs of
"season 1" in one bucket. Nothing errors; it is simply wrong on screen.

**(a) The reader sees the folder's numbering.** One unit numbered by three
entities' schemes is not one unit to a viewer, and presenting the entity's
numbering is the interleave above — measurably wrong. Grouping by entity first
reintroduces the tile split inside the unit and changes a shipped response
shape (Rule 2.3). So the read path translates canonical numbers to folder
numbers for multi-entity folders.

**(b) The mapping is a stored range, not an offset.** An offset does not
generalise: an anthology has no offset (Monster's entities both start at season
1 and the folder's number is an ordinal over entities), and a sequel miniseries
may be a season, a special, or unnumbered. The stored shape is **which folder
seasons this binding covers, and what they are called on the entity**, as
columns on item 2's join row. Deriving it from links on each drain is again
ADR-0033 item 1's rejected pattern.

### 4. The second entity is found by title search, not by walking the bound entity

**There is no provider related-series edge to walk.** `belongs_to_collection`
is null on **697 of 697** TV entities — it is movie-only and not a TV graph.
Recommendations are asymmetric and partial: The Office US and UK reference each
other; Doctor Who 2005 references 1963 and 2024 while 2024 references neither;
DAHMER reaches Menendez only once you already hold DAHMER's id. Searching Hill
House's bound name returns only itself. Given 4454, nothing in its payload
points at 74321.

**So ADR-0045's Q3 option C collapses to title search**, and this record says so
rather than leaving it open. `append_to_response=recommendations,keywords` on a
detail already being fetched is free and does not replace the search.

**The discriminator is the episode-title signal applied to the unplaced files**
— whichever candidate contains their episode titles. That is now trustworthy:
the comparator's false refutations went 16 → 2, confirmations rose to 598
folders, and confirmation is one-directional by construction
(`candidate_confirms_reference_episode` has no `Some(false)` to return).

**A mechanism change is required, and it is named here rather than discovered
later.** #110's confirmation reads `CandidateShape.reference_season_episodes`,
populated by appending **one season chosen by the folder's number**
(`append_to_response=season/{ref_season}`, `tmdb/mod.rs`). Appending `season/9`
to `/tv/74321` returns nothing, so on a renumbered split the shipped machinery
is **silent, not wrong**. Finding a second entity needs the candidate's *own*
seasons appended. That is a change to how the shape is built, not a new
predicate.

### 5. Not decided here

- **Which entity supplies the unit's primary metadata** — poster, overview,
  tile name. Will & Grace has two candidates and the unit has one identity. The
  measurement does not settle it and a guess in an ADR is worse than an open
  question. It blocks no part of items 1–4: the folder→entity record, the range
  mapping and the search discriminator are all indifferent to which row is
  displayed.
- **The year on the tile.** Under item 1 a lumper folder never becomes several
  tiles, so the year is not doing the work of telling two entities apart. It
  remains a label rule for tiles that already exist, and it is not this
  record's to decide.

## Alternatives considered

**Tiling: one browse unit per provider entity, with the year on every tile.**
Rejected, and **measured before rejection** rather than argued away. Derived
from four folders in one library; tested against 697 shows. It produces tiles
named after none of the folder they came from (Monster), undoes the user's own
filing (Battlestar's `Specials/`), reads as a duplicate for the case it was
designed for (two Will & Grace tiles side by side), and is what no other client
does. Its mechanical arguments survive as arguments for items 2 and 3 — bind
the right entity, present the folder's numbering — and not for a second poster.

**A nullable second entity column on `series`.** Rejected: Monster is already
three on the day it would ship.

**A per-item provider id on `media_items`.** Rejected under Rule 4.11:
`media_item_links` is that record already.

**Walking the provider graph to find the sibling.** Rejected on measurement:
`belongs_to_collection` is dead for TV at 697/697, and recommendations are not
a relationship graph.

## Consequences

- The browse response shape does not change for any folder that binds one
  entity, which is 720 of 723 in the measured library.
- `series_key` keeps reading the primary column (ADR-0039 item 5) and does not
  move when a folder gains a second entity. One folder stays one unit and one
  key.
- ADR-0035 item 8's canonical ordering gains an exception for multi-entity
  folders, stated in item 3(a). It is the one place a shipped rule forks, and
  it forks on a folder property rather than per request.
- ADR-0037's certification stays per item along the entity edge; no
  folder-level tie-break is introduced, because the unit does not need one.
- The empty-shell exclusion (ADR-0026, amended 2026-08-13) already prevents a
  zero-episode search hit from becoming a binding, which is what stops item 4's
  search from binding residue.
- Cost is at fix time and first bind, not per scan: a title search already paid
  after #106, collision-tier details already paid for title-exact hits, and a
  handful of season fetches per candidate. It becomes a per-scan cost only if
  identity is re-derived from search on every drain, which item 2's stored
  record exists to prevent.
- The primary-metadata question (item 5) has to be answered before a
  multi-entity folder can render, but not before the schema lands.
