# ADR-0039: The show entity and `series_key`

- Status: accepted
- Date: 2026-08-06
- Accepted: 2026-08-06, with all fourteen questions signed off in
  `nightjar-meta/notes/design/adr-0039-0040-questions-2026-08-06.md`.
  Q10 and Q4 change shipped pipeline behaviour and were put explicitly; Q2
  corrected the brief's `entity_kind` to the shipped `tv`. ADR-0035, ADR-0037
  and ADR-0038 were accepted in the same action
- Depends on: ADR-0033 (folder-keyed series rows, which this extends and does
  not replace); ADR-0029 §1 (entity-keyed canonical projection), §2.2 (derived
  keys), §2.5 (the two show handles, which this closes); ADR-0025 §1 (`item_key`
  grammar and opacity), §4 (path keys), §5 (one migrator per key change);
  ADR-0026 §8.1 / §8.4 (widened `unmatched`), §8.6 (Visible proxy and the show
  browse unit); ADR-0028 (manual fix, which owns every key change); ADR-0030
  (library-relative paths)
- Gate: Gate 3 — rescan of an unchanged library generates no search requests;
  watch history survives rename, re-encode, and library remove-and-re-add;
  full v1 API frozen
- Related: Block 2 plan B2-F (`nightjar-meta/docs/BLOCK_2_PLAN.md`); decision
  sheet `nightjar-meta/notes/design/adr-0039-0040-questions-2026-08-06.md`;
  ADR-0035 (rollup), ADR-0037 (certification inheritance), ADR-0038
  (`series_key`), all three of which were blocked on this record

## Context

Three Block 2 records were written and signed off against a series entity that
does not have a name. ADR-0035's rollup walks a series to find the next
unwatched episode. ADR-0037's evaluator has to read an episode's certification
from its series, because TMDB's `content_ratings` is series-level and a literal
reading of fail-closed hides every episode in the library. ADR-0038 stores a
track choice against a series so a corrected audio track survives to episode
two. All three stayed `proposed` waiting for this.

The reason they could not simply cite ADR-0033 is that the project already
holds three different strings that all mean "this show". ADR-0037 reaches for
the canonical row, ADR-0038 for the folder-keyed series row, and ADR-0035 for
"the series entity" without saying which — three records, three readings, none
of them wrong against a document that does not exist:

| String | Where it lives | What it actually answers |
|---|---|---|
| `metadata_canonical (tmdb, tv, {show_id})` | ADR-0029 §1 | What are this show's facts — title, genres, cast, certification |
| `series (library_id, relpath, tmdb_show_id)` | ADR-0033, migration 016 | Which show is this *folder* bound to |
| `tv\|tmdb:{show_id}` / `tv\|{query_key}` | `queue.rs:289`, ADR-0026 §8.6 | Which browse unit does this file belong to |

ADR-0029 §2.5 named two of them, said "do not merge them by intuition", and
left unification to "future series-identity work". ADR-0033 delivered the
folder binding and deliberately stopped short of the rest, because Block 1 did
not need it. This is that work, and Rule 4.11 is the reason it cannot be
deferred again: three strings for one concept, each with its own fallback
behaviour, is exactly the fork that rule exists to prevent, and the three
records above would otherwise each pick one and drift.

There is also a hole. `series` rows are written only on a successful TV match
(`upsert_series_row`, `queue.rs:326`), so an **unmatched show folder has no
row at all**. That is what leaves ADR-0038 with nothing to hang a track choice
on for the population least likely to have a correct default, and it is what
makes ADR-0035's rollup unable to group unmatched episodes.

## Decision

**One show entity, three roles, one key. The show entity is the canonical row;
`series` is the folder binding; `series_key` is the only reference that
crosses a wire or keys another table.**

### 1. The three roles, and why they are not one table

Each role answers a different question, so unifying them would be wrong. Rule
4.11 asks the ADR to say so rather than leave the fork unexplained.

1. **The entity** is `metadata_canonical` at
   `(provider = 'tmdb', entity_kind = 'tv', provider_id = {show_id})`. It holds
   the show's facts, including the `certifications_json` column ADR-0037 item 8
   adds. It exists once per show, no matter how many libraries or folders hold
   episodes of it, and it exists even when no file is bound to it.

   **The stored value is `tv`, not `show`.** Migration 011 constrains
   `entity_kind IN ('movie', 'tv', 'episode')`, and the wire reference below
   says `tmdb:show:{id}` because ADR-0025 §1 chose that grammar. Those are two
   spellings of one entity and this ADR adds no third: nothing writes
   `entity_kind = 'show'`, and the CHECK constraint is not widened.

2. **The folder binding** is the `series` table from ADR-0033: one row per show
   folder per library, saying which entity that folder is bound to. It answers
   a question about *this installation's filesystem*, which the entity cannot,
   and it is what suppresses a search on rescan.

3. **The key** is `series_key`, defined in item 2. It is what every per-series
   row and every route parameter uses, and it is the only one of the three that
   clients ever see.

The entity and the binding are not merged because a show entity outlives any
folder and can be bound by several; a folder binding is meaningless without a
library. The binding and the key are not merged because the key is derived
(item 5), not stored.

### 2. `series_key`

```
series_key := tmdb:show:{tmdb_show_id}          -- folder bound to an entity
            | folder:{library_id}:{relpath}     -- show folder with no entity
            | tmdb:movie:{tmdb_movie_id}        -- matched movie
            | path:{library_id}:{relpath}       -- unmatched movie
```

`relpath` in the `folder:` form is the show folder's library-relative path
(ADR-0030), the same string the `series` row is keyed on and the same one
`show_folder_relpath` computes, so migration, queue and rollup cannot disagree
about what a folder is.

**A movie is a series of one, and its series key is its own `item_key`.** That
is a reuse of the ADR-0025 grammar rather than two more grammars. ADR-0038 Q26
already required no nullable case and no second column; the alternative was a
`movie:` prefix that would have wrapped a key we already have in a synonym.
The four shapes do not collide: an unmatched movie's key names a file, an
unmatched folder's names a directory, and the prefixes differ.

**`series_key` is opaque on the wire, permanently**, on the same terms
ADR-0035 item 11 froze `item_key`. The grammar above is documented for
debugging and is not a parse contract. Clients pass back the string they
received. The server may change the grammar in any version.

### 3. Every show folder gets a series row, and `tmdb_show_id` becomes nullable

A `series` row is written when a show folder **forms a resolve group**, not
when it matches. `tmdb_show_id` becomes nullable and null means "no entity yet".

Without this, `folder:` keys exist in this ADR and nowhere in the database, and
the three consuming records get a key that is null for the unmatched fraction
they were each written to support. With it, "which series is this file part of"
has an answer for every episode file in the library from the first scan,
before any provider call.

SQLite cannot relax a `NOT NULL` in place, so the migration rebuilds the table
— create, copy, drop, rename, inside one transaction, preserving the
`(library_id, relpath)` primary key and the `ON DELETE CASCADE` to `libraries`.
It takes the next free migration number at implement time and reserves none
(the same discipline as ADR-0026 and ADR-0030).

`series_show_id_for_folder` (`queue.rs:309`) currently reads `tmdb_show_id`
into `Option<i64>` via row absence. After this change a present row with a null
id must read as `None` too, or the enqueue path throws on every unmatched
folder. That is a two-line change and it is named here because it is the one
place where "no row" and "row without identity" are the same answer to the
caller and different states in the table.

### 4. The browse unit is the `series_key`, and the soft key retires from it

`visible_show_unit_key` (`queue.rs:289`) and the group `unit_key`
(`queue.rs:674`) both return `series_key` — `tmdb:show:{id}` when bound, the
folder key when not. The soft key `clean_show_title` → `query_key` stays
exactly where ADR-0033 item 3 put it, in the matcher, matching-only. It is no
longer a browse unit, and this closes the "unification is future
series-identity work" pointer ADR-0029 §2.5 left open.

**This changes shipped behaviour and the change is the point.** Today the soft
key is the fallback for a folder with no `series` row, so two unmatched folders
that fold to one key — `Shameless (US)` and `Shameless (UK)` — share one browse
unit and one Visible proxy slot. That is the D2 collision ADR-0033 exists to
prevent, surviving on the browse side because ADR-0033 only removed it from
group formation. Enqueue groups are already folder-scoped (`ep_groups` keys on
`(library_id, show_folder)`); only the label was not.

The consequence is unmeasured and is named rather than assumed away: the Visible
proxy takes N ≈ 40 units (ADR-0026 §8.6), so splitting collided units changes
which units are on the first screen. How many units actually collide needs the
dogfood database to answer, so it is B2-0's to report and not this record's to
guess, and B2-0 records `T_first_screen` as touched. Nothing here restates a
`T_first_screen` figure, because none has been published. `unit_key`
is internal — it appears in no route and no OpenAPI schema — so this is a
pipeline behaviour change and not an API change.

### 5. The key is derived, never stored on the series row

`series` gains no `series_key` column. The key is computed from the row:
`tmdb:show:{id}` when `tmdb_show_id` is present, `folder:{library_id}:{relpath}`
when it is not.

This is ADR-0029 §2.2's argument applied to the same class of problem. A stored
key goes stale the moment the folder binds or the folder is renamed, and it
would need a rewrite path that duplicates the migrator item 7 already requires.
Rows that *key on* `series_key` — `profile_track_choice` (ADR-0038 item 4) and
the kids override table (ADR-0037 item 9) — store it, exactly as `watch_state`
stores `item_key`, and change it only through that migrator.

One function resolves an item to its series key, in `nightjar-metadata`
alongside `effective_item_key`, and every consumer calls it. There is no second
resolver (Rule 4.11).

### 6. Two edges, and they are deliberately not the same edge

An episode file has two paths to a show, and they answer different questions:

| Question | Edge | Why this one |
|---|---|---|
| Which show is this file grouped with in my library, and what preference scope does it sit in | the **folder binding** | Has an answer before any match, which is the whole reason item 3 exists |
| Which show entity's facts describe this item — title, genres, **certification** | episode canonical row's `tmdb_show` → tv canonical row | Follows what the item *is*, not where it sits |

**Certification therefore inherits along the entity edge.** An episode's
certification for the server region is read from its episode canonical row's
`tmdb_show`, then that show's `certifications_json`. Two indexed local hops,
no I/O, no new column, and the existing partial index
`idx_metadata_canonical_episode_show_season` already covers the first. ADR-0037
item 5's fail-closed rule is unchanged: no episode canonical row, no tv row, or
no label for the server region all still deny.

The divergence case is a single-file assign under a folder bound elsewhere: the
file's certification follows the entity it was assigned to while its rollup
grouping and its track-choice scope follow the folder it sits in. That is
correct in both directions — a fix that rebinds a file must move its
certification immediately, and must not silently move that file out of the show
the viewer sees it under. ADR-0033 item 7 makes the ordinary case a folder-wide
fan-out, so the two edges move together whenever a whole show is fixed.

An unmatched episode has no episode canonical row and therefore no
certification, so it denies. That is ADR-0037 item 5 unchanged, and it is the
below-floor fraction ADR-0025 already priced, not a new denial class.

### 7. One migrator for every `series_key` change

Any change of a folder's `series_key` runs one code path, the same discipline
ADR-0025 §5 set for `item_key`. It rewrites every table keyed on `series_key`:
`profile_track_choice` and the kids override table today, and anything added
later, which is why it is one function and not a rewrite at each call site.

**Direction and triggers.** Two directions, because item 2 gives movies a series
key too:

- **Shows:** `folder:{library_id}:{relpath}` → `tmdb:show:{id}`, when a folder
  acquires identity by first match, by the ADR-0028 series assign fan-out, or by
  re-binding from one show to another.
- **Movies:** `path:{library_id}:{relpath}` → `tmdb:movie:{id}`, which is the
  same string change ADR-0025 §5 is already making to that item's `item_key`.
  A movie assign therefore runs both migrators over the same value, and the
  reason that is not one function is that they rewrite different tables.

A folder's series key changes only when its `series` row acquires or changes
`tmdb_show_id`, which happens only inside a bind — which is where the ADR-0025
§5 migrator already runs. So this migrator is called by the same owner, in the
same transaction, and there is no separate trigger to remember.

**Merge on collision**, which is the common case rather than the exotic one.
Two folders can bind the same show (a split library, or extras in a sibling
directory), and two files can bind the same movie (ADR-0025 §2's dual 1080p
versions, which the dogfood inventory holds). Either way two keys arrive at one
and collide on `(profile_id, series_key)`.

- `profile_track_choice`: newer `updated_at` wins. There is no ratio to compare
  the way ADR-0025 §5 compares position, and a preference has no partial state.
- Kids overrides: **the more restrictive entry survives.** A "remove from kids
  mode" beats an "allow in kids mode". That is not a new rule; it is ADR-0037
  item 6's precedence — blocked, then allowed, then the ladder — applied to a
  merge, and inventing a second ordering here would be the fork Rule 4.11 names.

**Unbind does not run the migrator.** ADR-0028 clear-match on a folder returns
it to a `folder:` key, and the rows stay on `tmdb:show:{id}`. Rewriting them
would be wrong when a second folder is still bound to that show, and re-binding
the same folder to the same show reattaches them for free. This is ADR-0029
§1.4's rule — follow the new binding, do not mutate a shared entity — with the
same reasoning.

### 8. Two folders bound to one show are one `series_key`, on purpose

`series` is folder-scoped; `series_key` is show-scoped once bound. So a track
choice or an override set on `Show/Season 1/` applies to `Show (Extras)/` when
both bind the same show id, and that is what a viewer means by "this show".

This is not the D2 collision. ADR-0033 item 2 permits sibling folders to merge
"only by an explicit recorded rule, never by fold collision", and a shared
provider id is the explicit recorded rule. A title fold is not, and it still
never merges anything.

### 9. `series_key` is never an `item_key`

`is_watch_item_key` (`item_links.rs:111`) is an allowlist of
`tmdb:movie:`, `tmdb:episode:`, `path:`, `tvdb:`, so `tmdb:show:` and
`folder:` are both already rejected by construction. That property is asserted
with explicit negative cases for both prefixes rather than left incidental —
a green test whose subject is a boundary needs a negative case or it is
presumed vacuous, which is the class the Block 1 autopsy caught, and there is
already a `tmdb:show:` negative there to sit beside.

ADR-0025 is unchanged and ADR-0033 item 6 is unchanged: there is no series
watch key, no series-level watch state row, and no watch history that a folder
rename can orphan through this key. `tmdb:movie:` and `path:` are watch keys
*and* movie series keys, which is not a contradiction — a movie's series is
itself, so one string legitimately answers both questions for the one kind
where they coincide.

### 10. Item responses carry `seriesKey`

Every item-returning response gains `seriesKey` beside `itemKey`. Without it a
client cannot call `PUT /profiles/{profileRef}/track-choice?seriesKey=…`
(ADR-0038 item 7) or reason about the rollup entry it was handed, and the only
alternative is a client deriving the key, which Rule 2.1 forbids and which the
opacity rule in item 2 forbids twice.

Additive under Rule 2.3 and decided here under Rule 4.9, before the writers.
The field is a string and is documented as opaque in OpenAPI.

## Alternatives considered

**Leave the three strings distinct and let each consuming ADR pick.** The
status quo, and it is what ADR-0029 §2.5 explicitly deferred. Rejected: the
three records blocked on this each reached for a different one, which is the
demonstration rather than the theory. Three fallbacks also means three places
where the unmatched case behaves differently.

**A `series_key` column on the `series` row.** One less computation and it
makes the key greppable. Rejected under ADR-0029 §2.2's argument: it stales on
bind and on rename, and keeping it fresh is a second write path for something
the row already determines.

**Give the show entity a fourth `entity_kind` value, `show`.** Rejected: it
would widen a CHECK constraint, require a migration, and leave `tv` and `show`
both live in a table that already has 25.7k rows. `tv` is the stored spelling
and `tmdb:show:` is the wire spelling, and the ADR names both rather than
adding a third.

**Make `series_key` the certification edge as well, so there is one edge.**
Tempting under Rule 4.11 and rejected in item 6: the folder binding has an
answer before a match and the entity edge does not, so a single edge would
either deny every unmatched folder's certification twice over or would certify
a file by the folder it sits in rather than by what it is. Two questions, two
edges, stated.

**Mint a series row only when a folder matches, and let `series_key` be null
until then.** The smaller change. Rejected: a nullable key propagates into
`profile_track_choice`, the override table, and the rollup, and each of them
then needs its own answer for null. ADR-0038 Q26 rejected exactly this and the
reason holds here — the unmatched fraction is the population least likely to
have a correct default and most in need of a scope to correct it in.

**Keep the soft key as the browse-unit fallback.** No behaviour change and no
effect on the Visible proxy. Rejected: it leaves two fold-colliding unmatched
folders sharing one card, which is the D2 class ADR-0033 was written to end,
and it keeps a fourth string alive in the one place users can see it.

## Consequences

**Good**

- ADR-0035, ADR-0037 and ADR-0038 can be accepted. They were signed off and
  held `proposed` for this record alone.
- Kids mode contains television. Without item 6's inheritance, ADR-0037
  item 5's fail-closed rule denies every episode in the library.
- The last D2 fold-collision leaves the tree. Group formation lost it in RC8;
  the browse unit loses it here.
- ADR-0029 §2.5's open unification question is answered rather than deferred a
  third time.
- Every episode file has a series scope from first scan, so a viewer who fixes
  an audio track on episode one of an unmatched show does not fix it again on
  episode two.

**Bad (accepted)**

- A `folder:`-keyed series loses its track choices and its overrides on a
  folder rename, because the key is the path. A `tmdb:show:`-keyed one does
  not. This is the fragility class ADR-0025 §3 accepted for path keys, in a
  second place, and there is no rename migrator because the scanner sees a
  rename as a delete and an add.
- The Visible proxy composition shifts when previously-collided folders split,
  so `T_first_screen` has a new small term. The landing slice records it.
- Two folders bound to one show share preference state (item 8). For a
  deliberately split library that is right; for a folder someone bound to the
  wrong show it means a preference follows the mistake until the fix flow runs.
- The migrator is a second key-change path beside ADR-0025 §5's, called from
  the same owner. Two migrators is one more than one; the alternative was a
  single function rewriting two unrelated key spaces, which is worse.

**Open, and deliberately not decided here**

- Series artwork, series pages, and the version affordance are Block 3.
- Whether the rollup hides a series whose next episode is not downloaded is a
  Block 3 presentation question (ADR-0035).
- The slice that lands item 3's migration and item 4's unit-key change is
  **B2-0** in the Block 2 plan. It is the first thing B2-3, B2-6 and B2-8 all
  need, and it is deliberately not folded into any of the three, because all
  three would then carry a pipeline behaviour change that belongs to none of
  them.
