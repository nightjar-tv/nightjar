# ADR-0029: Canonical metadata store and file↔item links

- Status: accepted
- Date: 2026-08-03
- Amended: 2026-08-04 — sparse canonical upsert from search hit (ADR-0026
  §8.3); detail overwrite; cert on enrich (ADR-0026 §8.4)
- Depends on: ADR-0025 (item identity grammar; file↔item cardinality);
  ADR-0026 (§4 raw payloads; §6 collections; §8 two-tier status / sparse
  write); season bind on enrich (ADR-0026 §8.4)
- Gate: Gate 3 — auto-match coverage and requests per 1,000 items; watch
  history keys; fix flow under thirty seconds
- Related: ADR-0028 (manual fix consumes these shapes); ADR-0027 (artwork,
  reserved number — keys on `item_key`); Phase 3 Block 1

## Context

ADR-0026 §4 says canonical metadata is projected from entity-keyed raw
payloads and that mapping changes must not require a 24k-item re-fetch.
ADR-0026 §6 says collection id/name land "in the same transaction as the
canonical metadata row." Neither section defines that row. Today
`persist_hit_with_canonical(..., |_| Ok(()))` stores payloads and discards
the projection — honest incompleteness (Rule 4.8), not a provisional fake
column.

ADR-0025 §2 requires file↔item many-to-many. There is no join table and no
`item_key` column anywhere. `media_items` remains the file row (path,
probe, `metadata_status`). Watch state and playback events (Block 2) will
hang off `item_key`; they need a durable binding before those ADRs write.

This ADR decides only: (1) where projected canonical fields live, and
(2) how file rows attach to logical items. It completes the undefined
projection target in ADR-0026 §4/§6 and the join in ADR-0025 §2. It does
not supersede the ADR-0025 key grammar, nor ADR-0026 fetch, queue,
neg-cache, or payload keying.

**Numbering.** Highest accepted ADR on `main` at write time is 0028.
0021 is an empty hole; **0027 is reserved for artwork** (named in 0026 /
0028). This document is **0029**.

**Migrations.** 009/010 (neg-cache, payloads, `metadata_status`) landed
with #31. This ADR does **not** reserve a migration number; the
implementing slice takes the next free after that tip.

## Decision

### 1. Canonical metadata is entity-keyed, like payloads

One table (name left to the migration), primary key
`(provider, entity_kind, provider_id)`.

| `entity_kind` | `provider_id` | Source payload |
|---|---|---|
| `movie` | TMDB movie id | `metadata_raw_payloads` movie |
| `tv` | TMDB show id | tv |
| `episode` | TMDB episode id | projected from the **season** payload that contains that episode |

Season payloads stay in `metadata_raw_payloads` (`entity_kind = season`,
id `{show_id}:{season_number}`). There is no season canonical row —
seasons are a fetch / id-acquisition artifact, not a `CanonicalMetadata`
kind.

This makes ADR-0026 §4 true by construction: re-project rewrites canonical
rows from stored JSON; zero API. It inherits the episodes-share-show-rows
size argument that rejected per-file payloads (~420 MB vs ~2.5 GB).

#### 1.1 Episodes get their own rows

Dogfood: ~23,076 episode files, ~1,912 season folders, ~716 shows,
~1,864 movies.

| Approach | Canonical rows | Read path |
|---|---|---|
| Episode rows (chosen) | ~23k episode + ~716 tv + ~1.9k movie ≈ **~25.7k** | `tmdb:episode:{id}` → one indexed get; show fields via `tmdb_show` |
| Fields from season JSON at query time | ~2.6k (movie+tv only) | Parse ~54–79 KB season blob and scan `episodes[]` on every episode surface |

Episode `item_key` is already `tmdb:episode:{id}` (ADR-0025). Without
episode rows the key cannot resolve to canonical in one hop, and browse
keeps depending on scan titles after a successful match. Episode row
bodies are small (see §1.2); they are not a second copy of show cast.

#### 1.2 Field-by-kind (no inherited blobs on episode rows)

"Kind-specific fields nullable" is not enough. Copying show cast/genres
onto 23k episode rows repeats the §4 per-file payload mistake under a
canonical hat. Persistence is **kind-sparse**. `CanonicalMetadata` remains
the in-memory union; writers must not persist empty `Vec`s that mean
"inherit."

| Field | `movie` | `tv` | `episode` |
|---|---|---|---|
| title, original_title | ● | ● | ● (episode name) |
| release / first_air year | ● | ● | — |
| **air_date** | — | — | ● (source of truth for episode dating) |
| year derived from `air_date` | — | — | optional denorm only; same provenance as `air_date`, not a second clock |
| plot / overview | ● | ● (show) | ● (episode overview only) |
| season, episode numbers | — | — | ● |
| ratings | ● | ● (show-level / content-rating projection when mapped) | ● **episode** vote only |
| artwork refs | poster / backdrop / … | show art | **still only** |
| ids | tmdb movie (+imdb/tvdb) | tmdb show (+ext) | tmdb episode + **`tmdb_show` (required parent)** |
| collection id / name | ● (ADR-0026 §6) | — | — |
| genres | ● | ● | **— look up tv** |
| cast | ● | ● | **— look up tv** |
| show overview, content rating | — | ● | **— look up tv** |
| runtime | ● | show-level `episode_run_time` | episode runtime if present in season payload; else omit |

Filled episode card = episode row + one tv get by `tmdb_show`.

**Browse sort and `year`.** `media_items.year` is scan-derived; browse sort
still uses scan fields (client snapshot under drain — ADR-0026). Canonical
`year` / `air_date` **never** feed browse sort. Two years with different
provenance sit in adjacent tables on purpose; do not "unify" them into the
sort path by accident.

#### 1.3 Unmatched items

No provider entity → **no canonical row**. Display falls back to scan
fields on `media_items` (title / year / season / episode). Same fallback
browse already uses.

#### 1.4 Collections (ADR-0026 §6)

Nullable `collection_id` / `collection_name` on the **movie** canonical
row, written in the same transaction as that row's projection (with the
raw movie payload upsert).

On reassignment, **follow the new binding** — do not clear collection
columns on the old shared movie row (other files may still point at it).
Clear-match drops the provider binding → no canonical → no collection
line. ADR-0028's "clear collection" obligation is satisfied by binding
change, not by mutating a shared entity.

#### 1.5 Ratings

One field on the canonical row among the others (structured / JSON column
matching other multi-value projected fields). Not a separate ratings
table. TV / episode mapper gaps wait on the re-project slice.

#### 1.6 Re-project upsert and delete

Idempotent upsert on `(provider, entity_kind, provider_id)`.

| Source | Upsert | Delete-absent |
|---|---|---|
| **search hit (sparse, ADR-0026 §8.3)** | movie / tv row: title, year, plot, vote ratings, art paths, ids only — **no** cast, genre names, cert, collection, episode rows | no |
| movie / tv **detail** payload | full projection including cast, genres, collection, **content cert** (ADR-0026 §8.4); overwrites sparse fields | no |
| season payload → episodes | each episode id in the payload | **yes, season-scoped** |

Sparse search write and detail write share the same canonical PK. Empty
`Vec` cast/genres on a sparse row means “not filled yet,” not “inherit from
parent.” Detail overwrite is the only path that fills them.

**Season-scoped delete:** for episode canonical rows with `tmdb_show = S`
and `season = N`, delete rows whose `provider_id` is not in that season
payload's episode id set; then upsert the present ones. ADR-0025 treats
provider renumbers as normal; without this, dead episode ids answer forever.

**Bindings for removed episode ids are deleted in the same transaction**
as those canonical rows. Leaving dangling join rows that resolve to
nothing would make `COUNT(*)` over bindings silently overstate matched
coverage. After delete, affected files have no provider binding (see §2.2)
until rematch or manual fix. This ADR does not invent a watch-state
migrator for renumbers (ADR-0025 / ADR-0028 own key change).

### 2. File↔item join; path keys derived

#### 2.1 Join table

`media_items` stays the file + enrichment-status home (`metadata_status`
per file, ADR-0026 §8). No `item_key` column on `media_items` — that
shape cannot express one file → two keys (ADR-0025 §2 multi-episode
file).

```
media_item_links (
  media_item_id  INTEGER NOT NULL REFERENCES media_items(id) ON DELETE CASCADE,
  item_key       TEXT NOT NULL,
  manually_matched INTEGER NOT NULL DEFAULT 0,
  PRIMARY KEY (media_item_id, item_key)
)
-- INDEX (item_key)  -- versions, artwork, future watch
```

Name is illustrative; migration picks the final identifier.

Writers insert **provider** (and NFO-upgrade) keys at match time.
`manually_matched` is defined **here** (Rule 4.9: this ADR owns the join
shape). ADR-0028 owns the flag's semantics (assign sets; automatic match
must not overwrite a flagged binding; clear-match clears the flag).

**Grain is per-binding.** One file with two `item_key`s has two flags.
Assign / clear operate on the binding(s) they write or remove.

#### 2.2 Path keys are derived, not stored

`library_id` and `path` already live on `media_items`. A stored
`path:{library_id}:{relpath}` goes stale on rename and needs a rewrite
path this ADR should not own.

**Join rows are provider (and NFO-upgrade) keys only.** Empty join for a
file ⇒ unmatched-shaped. ADR-0025 §4 ("every indexed file that can carry
watch state has an `item_key`") is satisfied by: provider binding if
present, else **derive** the path key at use (watch write, API, migrator).
The durable unmatched watch record is the watch row's key at event time —
same rename fragility ADR-0025 already accepted.

#### 2.3 Resolve `item_key` → canonical

Parse the ADR-0025 grammar → `(provider, entity_kind, provider_id)` →
canonical PK.

| `item_key` | Canonical |
|---|---|
| `tmdb:movie:{id}` | `(tmdb, movie, id)` |
| `tmdb:episode:{id}` | `(tmdb, episode, id)` |
| `tvdb:…` (NFO-only until TMDB upgrade) | same table, that provider; upgrade is ADR-0025 §5 |
| derived `path:…` | no canonical row |

Show-level fields for an episode file: episode row's `tmdb_show` → tv
row. No show watch `item_key` in ADR-0025; this ADR does not invent one.

#### 2.4 Clear-match and the missing flag

Assign on an unmatched file creates the binding with `manually_matched`
set — fine under ADR-0028 §4 (below-floor assign).

Clear-match **removes** the binding, which removes the flag. Nothing then
records that a human already rejected a match for that file. Automatic
matching will re-evaluate and may write back the same wrong id. That
consequence is **inherited from ADR-0028** ("clear-match clears the flag"),
settled before this join existed; it is not softened here. Fix-flow /
product follow-up may want a longer-lived rejection signal; that is outside
this ADR. Do not "fix" it by leaving a join row with a null `item_key`.

#### 2.5 Shows browse read path and two show handles

**After season pass + episode links:** the shows-library poster path is

`media_items` → `media_item_links` → episode canonical → `tmdb_show` → tv
canonical

Four indexed hops. Cost it; do not pretend it is one get. Denormalising
for product browse is a later choice, not required by this shape.

**Two show handles — keep distinct (Rule 4.11):**

| Handle | What | Role |
|---|---|---|
| Soft key (`clean_show_title` → yearless `query_key`) | Filename-derived | Queue grouping and **Visible v1** browse unit (ADR-0026). **Provisional.** Not a watch `item_key`. |
| `tmdb_show` | Provider id on episode → tv row | Durable show metadata once episode links exist. Not a watch `item_key`. |

Do not merge them by intuition. Soft key can split one TMDB show;
`tmdb_show` can unite filenames the soft key split. Unification is future
series-identity work (ADR-0026 already deferred durable series identity).

> **Closed 2026-08-06 by [ADR-0039](0039-show-entity-and-series-key.md).**
> ADR-0033 made the show *folder* the durable identity unit; ADR-0039 names the
> reference `series_key` and makes it the browse unit. The soft key stays where
> ADR-0033 item 3 put it — in the matcher, matching-only — and is no longer a
> browse unit. `tmdb_show` keeps this section's role unchanged as the entity
> edge, and ADR-0039 item 6 states why the entity edge and the folder binding
> are deliberately not the same edge. Neither is a watch `item_key`; that part
> of this section is unchanged.

Until the season pass exists, Visible **stays** on the soft key: tv
canonical rows from show detail may exist but are unreachable from
`media_items` without episode bindings.

### 3. Season enqueue is a hard dependency

Episode ids live in season payloads (ADR-0025). ADR-0026 already states
that v1 drain resolves movie and show (episode-group) search+detail only
and that season detail is not yet enqueued.

**This ADR depends on that pass** for episode canonical rows and
`tmdb:episode:{id}` bindings. Until it lands:

- Join holds no provider keys for TV episode files (derived path key only).
- No episode canonical rows.
- `tv` rows from show detail are orphaned from the file graph.

**Partial first-run number (do not quote as complete):** dogfood movie+show
drain ≈ **2,500 groups / 22.1 min wall** = ~1,864 movies + ~716 shows and
**no seasons**. With ~1,912 seasons, first-run request count and wall grow
by roughly **+75%** once season detail is enqueued. Gate 3 publishes
requests per 1,000 items — cite the movie+show figure only with this
caveat attached.

Tables from this ADR may ship before season enqueue; episode projection
and provider bindings may not. Drain wires the season→episode bind path
behind `MetadataSource::fetch_season`; stubs return `None` and leave
files unbound. A green unit suite therefore does **not** prove episode
bindings — only a live season enqueue (or a test double that returns
season payloads) does.

## Out of scope

Named so the next reader does not treat them as forgotten:

- Watch state and playback events (Block 2) — this ADR makes the key and
  binding they hang off; it does not build those tables.
- Artwork (ADR-0027).
- Manual fix API (ADR-0028) — consumes these shapes; does not define them.
- Re-project mechanism, mapper fixes, TV ratings gap — wait on this shape.
- Cleaner-version stamp and `series_library_year` — independent; can
  proceed after #31 without this ADR. `year_from_show_folder` walks two
  parents and is path-form sensitive when the library root *is* the show
  folder (absolute keeps `Show (YYYY)/…`; relpath `Season N/…` loses it).
  Normal `library/Show/Season/file` is identical under both forms. Any
  `series_library_year` / collision-pin year repro must record whether
  paths were absolute or relative so a later media_items.path → relpath
  change (ADR-0030) is not a third untracked variable.
- Fan-out, series watch identity, Visible migration off the soft key.

## Alternatives considered

**Canonical columns on `media_items`.** Rejected: duplicates show-shaped
fields onto every episode file; fights ADR-0025 many-to-many; breaks
ADR-0026 §4's shared-entity size argument.

**Canonical keyed only on `item_key` (no entity table).** Rejected: no
show watch key in ADR-0025; show-level fields and collections need a show
or movie entity; path-keyed unmatched items are not provider entities.

**`item_key` column on `media_items` plus a join for the rare multi-key
file.** Rejected: Rule 4.11 — two write paths for the same binding.

**Store path keys in the join for unmatched files.** Rejected: duplicates
`(library_id, path)`; stales on rename. Derivation satisfies ADR-0025 §4.

**Episode fields read from season JSON at query time (no episode
rows).** Rejected: §1.1 — hostile read path; breaks one-hop resolve from
`tmdb:episode:{id}`.

**Leave dangling join rows when season-scoped delete removes episode
canonical rows.** Rejected: coverage counts overstate matches unless every
query joins through canonical. Deleting bindings with the canonical rows
is the single rule.

**Put `manually_matched` on `media_items` or on the canonical entity.**
Rejected: flag is per logical binding (ADR-0028); shared entity flag would
affect every file pointing at that movie/episode; file flag cannot express
two bindings.

## Consequences

- Implementing slice: migration (number after 010), canonical upsert from
  existing mappers (kind-sparse), join writes on match, season-scoped
  re-project delete including bindings, then season enqueue before claiming
  TV identity complete.
- ADR-0026 §4's re-project promise and §6's collection landing zone are
  defined here; writers stop passing the canonical no-op once the table
  exists.
- ADR-0025 §2's join is this table; scanner/metadata set provider bindings
  at match time; unmatched files store nothing in the join.
- ADR-0028 assign/clear/retry target `media_item_links` and movie
  canonical collection columns as specified above; clear-match's
  re-match vulnerability is acknowledged, not redesigned.
- Gate 3 request budgeting must not treat 2,500 groups / 22.1 min as the
  full first-run cost.
- Kids / allowlist / artwork continue to key on `item_key` once bindings
  exist; artwork ADR remains 0027.
