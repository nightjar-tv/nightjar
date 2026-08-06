# ADR-0026: Metadata pipeline

- Status: accepted
- Date: 2026-08-02
- Amended: 2026-08-02 — TV multi-exact collision pin; title-fold corpus
  discipline; full-library match rates after pin + fold
- Amended: 2026-08-02 — raw payload store measured at 317 MiB
  (`SUM(LENGTH(payload))`); ship uncompressed
- Amended: 2026-08-03 — API rate limiter is not shared with artwork;
  metadata queue is a query over item `metadata_status`, not a jobs table
- Amended: 2026-08-03 — TV collision ladder step 4
  (`exact_title_episode_title`); see ADR-0032
- Amended: 2026-08-04 — pin order counts-before-year; empty-shell refuse;
  TV long-title prefix hit; season detail 404 soft-skip
- Amended: 2026-08-04 — two-tier status (`matched`); adult first-screen
  search-terminal; queue fairness (search vs enrich); sparse search write;
  enrich id short-circuit; cert projection on detail for kids fail-closed
- Amended: 2026-08-04 — provisional non-watch `tmdb:show:{id}` link for
  enrich id only (§8.4)
- Amended: 2026-08-04 — NFO display authority; complete movie NFO skips
  TMDB (ready, zero HTTP); incomplete NFO merges over detail via
  `merge_prefer_left` (§8.9)
- Amended: 2026-08-05 — widen `unmatched` (§8.1) to cover a series whose
  identity TMDB confirms but whose episode identity it cannot supply;
  enrich has exactly one exit per item (§8.4); browse grouping keys on
  links, not `metadata_status` (Consequences)
- Amended: 2026-08-05 — rewrite §8.10 as the series identity cascade that
  landed (RC3-RC5), replacing the abandoned-branch cascade text; see
  `notes/block1-drift-autopsy-2026-08-05.md` D7
- Amended: 2026-08-06 — §8.4 item 3's certification projection superseded
  in place by ADR-0037 item 8; the projection it froze was never
  implemented and the storage it named cannot hold a board label
- Depends on: ADR-0025 (item identity / season-append episode ids)
- Gate: Gate 3 — auto-match ≥95% correct; every mismatch fixable in-UI in
  under 30 seconds; API requests per 1,000 items published for first run and
  rescan
- Related: Continuity settled set (direct-to-TMDB, FTS rejected, application
  key); strategy note
  (`nightjar-meta/notes/design/metadata-artwork-strategy.md`); two-tier
  design (`nightjar-meta/notes/design/metadata-two-tier-grid-strategy.md`);
  grid measure (`notes/grid-fast-vs-full-metadata-2026-08-04.md`); match
  spike (`nightjar-meta/notes/fts-vs-search-match-quality-2026-08-02.md`);
  Phase 3 Block 1 (`nightjar-meta/docs/PHASE_3_REVISED.md`)

## Context

Block 1 needs a metadata pipeline ADR before writers (Rule 4.9). Continuity
already settled the provider posture: direct to TMDB, no Nightjar-hosted
cache, no daily-export FTS matching index, shipped application key with a
user-key escape hatch, first-run budget of roughly one search plus one
detail per unmatched item. The strategy note carries NFO-first priority,
canonical model, queue priority, state machine, `append_to_response`,
change lists, and long refresh windows. Artwork is a separate ADR.

What this document still has to decide:

1. Match confidence and the threshold below which an item stays unmatched
2. Negative-result cache shape (backoff + manual retry)
3. Raw provider payload persistence and size at dogfood scale
4. Where the shipped application key lives and how a user key overrides it
5. Collections storage on the movie detail write (so the fix flow can clear it)

Wrong matches are worse than unmatched: they orphan watch state onto the
wrong `item_key` (ADR-0025) and feed the kids evaluator a wrong
certification. The spike measured search top-1 at 98.6% combined on a
280-item stratified dogfood sample; that is sample-bounded evidence, not
the Gate 3 criterion met.

## Decision

### 1. Resolution path (cited, not re-argued)

NFO first, then TMDB **search** (fast tier) then **detail + seasons** (enrich
tier) — §8. An NFO that already carries a TMDB id skips search and may go
straight to detail. Matching uses TMDB search directly. Queue priority,
`append_to_response`, change lists, and long refresh windows remain as in
`metadata-artwork-strategy.md` and Phase 3 Block 1; status machine and
first-screen criteria are owned by §8 of this ADR.
**API request-rate limiting** (this ADR §7) applies only to
`api.themoviedb.org`. Artwork uses a separate connection cap on
`image.tmdb.org`, decided in the artwork ADR — not one shared limiter over
both hosts. Episode ids come from season append per ADR-0025. Artwork
acquisition, image pipeline, and lazy download are out of scope here.

### 2. Match confidence and threshold

Confidence is a server-side score of the search result list against the
cleaned filename title and year. It is not TMDB's popularity field. v1 uses
the discrete method classes from the spike matcher:

| Method | Score | Meaning |
|---|---:|---|
| `exact_title_year` (unique) | 0.98 | Normalised title hit and year match; one candidate |
| `exact_title` (unique) | 0.90 | Title hit; no year on the file, one candidate |
| `exact_title_empty_shell` | 0.72 | Sole title hit is a TMDB shell (`number_of_seasons`/`episodes` 0) — never auto-match |
| `exact_title_year` (multi) | 0.80 | Title+year hit; more than one candidate |
| `exact_title` (multi) | 0.72 | Title hit; more than one candidate; **superseded for TV** by the collision pin below when a discriminator fires |
| `exact_title_collision_unpinned` | 0.72 | Multi exact-title; no discriminator selected exactly one candidate |
| `exact_title_episode_count` | 0.90 | Multi exact-title; library episode count uniquely matched (soft) a candidate's `number_of_episodes` |
| `exact_title_season_count` | 0.90 | Multi exact-title; library season count uniquely matched `number_of_seasons` |
| `exact_title_library_year` | 0.90 | Multi exact-title; library premiere year uniquely matched `first_air_date` year |
| `exact_title_episode_title` | 0.90 | Multi exact-title; reference episode name uniquely matched one candidate (ADR-0032; TV only) |
| `exact_title_year_nearest` | 0.70 | Title hit; year present but no exact year row |
| `top1_rank` | 0.45–0.65 | No exact title hit; took search ranking |

**Auto-match only when confidence ≥ 0.80.** Below that the item stays
unmatched and keeps its path `item_key`. Do not write a provider key for a
low-confidence hit.

#### TV multi-exact collision pin (one rule)

When normalised title matches more than one `/search/tv` hit and query year
did not already produce an `exact_title_year` row, do **not** lower the floor.
Apply discriminators in order; the **first that selects exactly one
candidate** lifts to 0.90 under the method name above. If a discriminator
selects zero or two-or-more candidates, try the next. If none pin uniquely,
stay at 0.72 as `exact_title_collision_unpinned`.

| Order | Library signal | Candidate field | Match |
|---|---|---|---|
| 1 | Episode file count under the show | `/tv/{id}` `number_of_episodes` | Soft (±15% or ±5) |
| 2 | Distinct season numbers present | `/tv/{id}` `number_of_seasons` | Exact |
| 3 | Premiere year (earliest episode `year`, else show-folder `(YYYY)`) | `first_air_date` year | Exact year |
| 4 | Reference episode title (ADR-0032) | `/tv/{id}/season/{s}/episode/{e}` `name` | Folded title unique match |

Counts before year so a folder year that uniquely matches a **miniseries**
cannot beat a multi-season library shape (dogfood: Battlestar Galactica
`(2003)` folder vs 2004 series). Empty shells (`number_of_seasons` 0 /
`number_of_episodes` 0) never pin. Multi-exact with library counts always
fetches `/tv/{id}` detail before scoring so count pins have data.

TV title hit: exact fold **or** candidate name is the query as a prefix
followed by more words (e.g. cleaned folder "The Continental" vs TMDB
"The Continental: From the World of John Wick").

Step 4 is **TV multi-exact only**, capped at 5 tied candidates, and
declines when the local reference title is on the ADR-0032 rejection list.
Explicit provider ID skips search entirely (ADR-0032 precondition) — it is
not a ladder step. Detail shapes for (2) and (3) are fetched only when (1)
did not pin — tens of ambiguous shows, not per file. Wrong series match
remains worse than none: ambiguous residue goes to the fix flow
(ADR-0028), not a looser floor.

The discrete mid-table weights (0.72 vs 0.55, etc.) are the spike's carried
forward classes; the floor and the collision pin are what dogfood measured.
Do not retune those middle weights without a new hand-scored sample.

Match outcomes carry the method string (resolved `match_method`, or
`BelowThreshold { method }`) so logs and the fix flow name which table row
or discriminator fired — same debuggability bar as track-selection reasons.

0.80 is calibrated against the 280-item stratified dogfood sample in
`notes/fts-vs-search-match-quality-2026-08-02.md`: it is the lowest
threshold at which that sample's search path had 100% precision on both
movies and episodes. 100% precision here means no observed wrongs in 280
scored rows, not a proof of no wrongs. Changing the floor requires
re-calibration on a larger hand-scored sample (or the full dogfood Gate 3
measure), not taste.

| Threshold | Movies cov / prec | Episodes cov / prec |
|---|---|---|
| 0.00 | 98.9% / 100% | 100% / 98% (2 wrongs) |
| 0.80 | 97.2% / 100% | 88% / 100% |
| 0.85 | 85.6% / 100% | 88% / 100% |

The two episode wrongs were both `One Piece` at 0.72 (`exact_title` multi:
live-action id over the anime series). Lowering the floor to clear Gate 3
coverage would re-admit that class. Coverage climbs by improving inputs and
by the collision pin above, not by accepting multi-hit title matches without
a unique discriminator.

Full-library measure (2026-08-02, testdata library excluded): movies
**95.8%**, episodes **94.9%**, combined **95.0%** at floor 0.80; below-floor
/ fragile path-key fraction **~4.5%**. Residue is genuinely ambiguous
(incomplete libraries, double-count collisions); fix flow owns it.

Gate 3 still wants a hand-scored wrong-rate check at this threshold; the
coverage numbers above are auto-match rate, not proof of zero wrongs.

### 3. Negative-result cache

A durable table, not an in-memory map. Without it the same unmatchable
names re-search on every rescan.

| Column | Role |
|---|---|
| `provider` | `tmdb` |
| `kind` | `movie` \| `tv` (search target; episodes search as `tv`) |
| `query_key` | Normalised title + year (or sentinel for yearless). The search input, not the file path. |
| `reason` | `no_results` \| `below_threshold` \| `api_error` |
| `confidence` | Best score seen when `below_threshold`; null otherwise |
| `attempt_count` | Increments on each failed try |
| `attempted_at` | Last attempt |
| `next_retry_at` | Backoff deadline |

Primary key: `(provider, kind, query_key)`.

Backoff: 1 day, then 7 days, then 30 days, then 90 days, capped at 90.
`api_error` uses the same schedule (rate-limit and transient failures
should not hot-loop). A rescan before `next_retry_at` skips the network.

**Manual retry** deletes the row (or sets `next_retry_at` to now) and
ignores the cache for that lookup. The fix-match UI and any "retry
metadata" admin action go through that path. Path changes that alter
`query_key` naturally miss the old row; that is intended.

**Cleaner / `query_key` version.** A path rename orphaning one key is
different from a title-cleaner change, which re-keys the whole library at
once. Old rows become unhittable sediment — invisible to live lookups,
indistinguishable from live rows in a dump. Do not treat historical
negative-cache CSVs as a live bug list without reproducing each row
against the current cleaner. The table needs either a **cleaner-version
stamp** on each row (misses with a mismatched stamp are ignored and
rewritten) or an explicit **sweep on cleaner change**. Pick the shape
when the stamp/sweep lands; until then, provenance-check before acting.

### 4. Raw provider payload persistence

Persist the raw JSON returned by TMDB for each fetched entity, keyed by
provider entity, not by media file.

| Column | Role |
|---|---|
| `provider` | `tmdb` |
| `entity_kind` | `movie` \| `tv` \| `season` |
| `provider_id` | TMDB id; for seasons, store as `{show_id}:{season_number}` |
| `fetched_at` | |
| `payload` | Exact response body (TEXT / BLOB). One row per entity |

`append_to_response` sets for v1:

- Movie: `images,credits,videos,release_dates,external_ids`
- TV show: `images,credits,videos,content_ratings,external_ids,aggregate_credits`
- Season: `images,credits,videos,external_ids` (episode ids live here per
  ADR-0025)

Canonical metadata is projected from these rows. Changing the mapping code
must not require a 24k-item re-fetch.

**Size at dogfood scale (measured 2026-08-02).** Library:
24,940 items (1,864 movies + 23,076 episodes) → about 716 show folders and
1,912 season folders. Sampled live TMDB responses (10 movies, 5 shows, 5
season-1 payloads) with the append sets above:

| Entity | Median bytes | Mean bytes |
|---|---:|---:|
| Movie detail | 100,524 | 98,191 |
| TV show detail | 178,725 | 200,329 |
| Season detail | 54,325 | 79,275 |

Projected store if every movie, show, and season is fetched once and keyed
by entity: **~420 MB median / ~480 MB mean**. Storing one payload per media
file at the movie median would be ~2.5 GB and is rejected: episodes share
show and season rows.

~0.5 GB beside the library database is acceptable on the hardware class we
dogfood. No compression requirement in v1; add it later if measured growth
demands it without changing the key shape.

### 5. Application key and user override

Ship an application API key in the binary. TMDB licences are
non-transferable and may be terminated; rotation is a release rebuild, not
a settings toggle.

| Source | Where | Role |
|---|---|---|
| Application key | Compiled into the server binary at release build (`env!("NIGHTJAR_TMDB_APP_KEY")` or equivalent). Dev builds may omit it. | Default for every install |
| User key | `{NIGHTJAR_DATA_DIR}/secrets` file, mode `0600`, field `tmdb_api_key`. Settings UI writes this file; it is not a SQLite column. | Escape hatch when the app key is revoked, rate-limited for that operator, or unsuitable |

Resolution order: non-empty user key in the secrets file, else non-empty
`NIGHTJAR_TMDB_API_KEY` environment variable (operator injection of the
same override slot), else the embedded application key. If none are
present, metadata resolution fails with a clear operator-facing reason.

The secrets file is the v1 home for third-party credentials. OpenSubtitles
credentials later use this file rather than inventing a second store or
putting tokens in SQLite. The application key
is not copied into the secrets file.

**On-disk encoding:** line-oriented `name=value`, `#` comments and blank
lines ignored; the first `=` on a line separates name from value (values
may contain `=`); when the same name appears more than once, the last
assignment wins (an empty value clears that name). Mode `0600`. Settings
UI writers and later provider fields inherit this shape — do not invent a
second encoding.

### 6. Collections storage only

On a successful movie detail write, persist `belongs_to_collection.id` and
`belongs_to_collection.name` (both nullable) in the same transaction as the
canonical metadata row. No browse surface, no rail, no setting — the
item-page line is Block 3. Storing the fields now costs a nullable column;
adding them after first-run enrichment would spend the TMDB budget again.
ADR-0028 clears these fields when a manual reassignment orphans the old
collection linkage.

### 7. API request-rate limiter (metadata only)

`api.themoviedb.org` is rate-limited by request rate per IP (CDN-enforced,
roughly 50 requests/second; the API key is not considered).
`image.tmdb.org` has **no** request-rate limit; TMDB caps **simultaneous
connections** there (about 20). Those are different ceilings on different
hosts.

v1 therefore uses an **API request-rate limiter** for metadata only. The
artwork connection cap is decided in the artwork ADR. One limiter over both
would throttle image downloads against a budget that does not apply to them
and leave the connection cap unenforced where it does. Uplink/disk
contention during first run is admission control if it ever proves real
(post-v1); metadata HTTP does not share that path with transcodes.

Politeness budget (constants, not settings): about **10 requests/second**,
well inside the ~50/s ceiling. A full-library search pass of ~2,500 unique
queries in ~12.5 minutes ran at roughly 3/s without trouble.

Ceiling / 429 probes must use a **personal or other non-application** TMDB
key. The Continuity open question is whether TMDB accepts an embedded
application key at scale; walking that key into deliberate rate-limit
rejections answers it the wrong way. The ceiling is an IP/host property,
not a key property — a personal key measures it.

### 8. Metadata queue is a query, not a jobs table

Enrichment state lives on the item row as `metadata_status`. The pipeline
is **two-tier**: a fast **search** tier that can paint an adult grid, and
a slow **enrich** tier (detail + seasons + cert projection) that finishes
identity and kids-safe fields without re-searching.

#### 8.1 Status values

| Value | Meaning | Playable? | Provider `item_key`? |
|---|---|---|---|
| `pending` | Needs search (or NFO resolve) | Yes if file probed | No (path key) |
| `matched` | Search (or NFO with TMDB id) accepted ≥ 0.80; **sparse** canonical + art path refs written | Yes | **Yes** for movies (`tmdb:movie:{id}`); TV show identity for browse/group only — episodes stay path-keyed until season bind |
| `ready` | Detail applied; TV seasons bound where TMDB has them; cert projected when present; enrichment complete for v1 | Yes | Yes (episode keys when bound) |
| `unmatched` | Search (or NFO resolve) finished without a provider match (below floor, no results, invalid NFO); **or** enrich finished and TMDB cannot supply episode identity for this file (season absent from TMDB, a TMDB renumber, a special outside the season model, absolute numbering) | Yes | Path key only |

**Migration:** extend the `media_items.metadata_status` CHECK to include
`matched` (next free migration after 013, expected **014**). Existing
`ready` rows stay `ready` (already fully enriched under the pre-two-tier
single-pass drain). Do not rewrite historical `ready` to `matched`. The
2026-08-05 widening of `unmatched` adds no new CHECK value and needs no
migration: the definition covers a second cause, not a second column
value.

**Playable ≠ metadata status.** Playback uses file path + probe; the
stream path must not wait on `matched` or `ready`.

**Widened `unmatched` (2026-08-05).** A series can be identified while an
individual episode cannot: TMDB has the show but not the season, or
renumbered the season after the file was named, or the file is a special
outside TMDB's season model, or the file uses absolute numbering TMDB
does not expose per-season. Today §8.1 only says "search (or NFO resolve)
finished without a provider match"; that sentence now also admits "enrich
finished without episode identity." Both are the same user-visible
outcome — path-keyed, terminal, kids-denied (§8.2), fix-flow eligible
(ADR-0028) — so this is one status widened, not a second status for one
concept (Rule 4.11). A fourth status was considered and rejected: browse
grouping does not read `metadata_status` at all (see Consequences), so a
new value would change no client-visible behaviour and would only add a
column value nothing reads.

A `tmdb:show:{id}` link survives on these rows even though the item is
`unmatched`. The link is what keeps "never searched" (`pending`, no link)
and "TMDB has no episode for this" (`unmatched`, show link present, no
episode link) distinguishable as a query over links rather than as a
column value. It is also what the RC3 grouping fallback reads to keep
the file's card grouped with its bound siblings: `tmdb_show_for_media_item`
(`queue.rs:337`) reads a `tmdb:episode:` link and, failing that, the
`tmdb:show:{id}` link directly (§8.4, Consequences), and
`visible_show_unit_key` (`queue.rs:289`) resolves a bound item through its
links first, falling back to the folder's `series` row when no link exists
(RC8).

**Re-entry.** An item does not leave this widened `unmatched` on the next
drain pass by itself; a drain that only repeats the same failed lookup
does not converge (Gate 3 "rescan of an unchanged library generates no
search requests"). What moves it out, exactly one of:

- A manual fix (ADR-0028) assigns identity directly.
- The underlying file changes (a rename, a re-mux, a new NFO) and gets a
  new file parse.
- A `cleaner_version` bump (§3) invalidates the row's negative-result
  cache entry and the next drain re-attempts it under the new cleaner.
- A bounded refresh window re-checks TMDB for season data that did not
  exist yet at first enrich (TMDB adding a season after air is the
  ordinary case this covers); the window is a scheduled sweep, not a
  per-drain retry.

#### 8.2 Terminal surfaces

| Surface | Terminal statuses | Notes |
|---|---|---|
| Adult first screen / Visible grid | `matched` \| `unmatched` \| `ready` | Poster path required only for the `matched` / `ready` subset |
| Kids visibility | **`ready` only** (unmatched and missing cert deny) | Cert is **not** taken from search; fail-closed (Phase 3 Block 2) |
| Episode provider watch key | After season bind on the path to `ready` | Until then path key (ADR-0025) |
| Fix API assign | Prefer **`ready`** when assign fetches detail (+ bind for TV) in one shot | One server path (Rule 4.11); may briefly land `matched` only if enrich is deferred — prefer go to `ready` when detail is already in hand |

#### 8.3 Fast tier (search → `matched` \| `unmatched`)

Work set: `metadata_status = 'pending'`.

On auto-match ≥ 0.80 (or NFO that already carries a TMDB id and is accepted
into the same identity path):

1. Upsert **sparse** `metadata_canonical` from the search hit (ADR-0029
   entity row): title, original_title, year, plot/overview, vote ratings,
   artwork poster/backdrop **paths**, provider ids (`tmdb` movie or
   `tmdb_show` for TV).
2. **Do not** invent cast, genre **names**, content certification,
   collection, or episode canonical rows at this step. Empty cast/genre
   on the sparse row is correct; detail overwrite fills them later
   (ADR-0029 re-project upsert).
3. Set file(s) in the resolve group to `matched`.
4. Movies: write `media_item_links` → `tmdb:movie:{id}` (automatic, not
   `manually_matched`).
5. TV: **do not** write `tmdb:episode:{id}` until season bind. Browse may
   use Visible unit / soft-key evolution (`tv|tmdb:{show_id}` when linked
   elsewhere); that is not a watch key (ADR-0025).
6. Artwork: when a poster (and optionally backdrop) path is present,
   enqueue or allow first-serve download under ADR-0027 for Visible units
   so the adult grid can paint without waiting for enrich.

Below floor / miss / invalid NFO: set `unmatched`; **no** provider link;
UI may show filename. Do **not** adopt top-1 below the floor for watch
state. Display-only poster without identity is out of this slice.

#### 8.4 Slow tier (enrich → `ready`)

Work set: `metadata_status = 'matched'`.

1. **Id short-circuit only.** Load the stored TMDB id (link — movie
   `tmdb:movie:{id}`, or provisional non-watch `tmdb:show:{id}` written at
   search tier for enrich id only — or sparse canonical). Call
   `movie_detail` / `tv_detail` by id. **Never re-search.**
2. Map cast, genre names (const id→name map is fine, ~1 KB), collection on
   movies (§6), and full artwork refs from detail.
3. **Content certification projection: SUPERSEDED by ADR-0037 item 8
   (2026-08-06). Not implemented; do not read this clause as shipped.**

   What it said: on detail write, project TMDB certification into the
   existing canonical ratings / content-rating projection path (ADR-0029
   §1.2 / §1.5), not a second cert table.

   Why it is superseded: that path cannot hold a certification. The
   shipped `Rating` type is `{source: String, value: f64, votes:
   Option<i64>}` and `ratings_json` is a `Vec<Rating>`; a board label such
   as `PG-13` has nowhere to go. Nothing in `server/crates/metadata`
   parses `release_dates` or `content_ratings` today, so no projection
   exists to correct. ADR-0037 decides the storage shape instead
   (`certifications_json` on `metadata_canonical`) and owns building the
   projection.

   What survives unchanged: the intent, and the reason it was frozen
   here. `MOVIE_APPEND` and `TV_APPEND` already request `release_dates`
   and `content_ratings`, and `metadata_raw_payloads` stores the raw
   body, so the labels are on disk and a third full-library TMDB pass is
   not required for kids. Null after a successful detail fetch stays
   allowed; kids fail-closed treats missing or unknown as deny.
4. TV: season bind via existing `bind_resolved_items` (ADR-0029 §3).
   **HTTP 404 on a season remains a soft skip** (continue other seasons);
   missing seasons leave those files without an episode link from this
   pass.
5. **One exit per item, no non-terminal leftover on a successful pass.**
   A successful enrich pass — the season fetch(es) it needed either
   succeeded or soft-skipped, and the item was not blocked by a
   provider error — sets exactly one of:
   - `ready`, for an item that received a `tmdb:episode:{id}` link and
     its canonical episode row from the bind.
   - `unmatched` (§8.1, widened 2026-08-05), for an item in a group whose
     season fetch(es) completed (or soft-skipped) but that did not
     receive an    episode link — TMDB has the series but not this episode's
     identity. The series' `tmdb:show:{id}` link is kept.
   An item left `matched` after enrich means a **provider error**
   interrupted the pass (timeout, 5xx) — that is a retry, not a terminal
   outcome, and the next drain pass must attempt it again. A detail 404
   on a stored id is different: the id itself is bad, not the network
   call, so it is not this retry class; RC3 makes that class terminal or
   a negative-cached retry with backoff, never a bare next-pass repeat of
   the same call. `matched` must never be the resting state
   for an item enrich actually finished; that ambiguity (was this item
   not tried, or did it fail?) is what left the pre-2026-08-05 drain
   looping. Distinguishing the two is a code requirement, not a naming
   preference (RC3, this ADR's implement slice).

NFO-with-id may skip search and go straight to detail: the NFO resolve
returns `MetadataOrigin::Nfo` (`resolve.rs:279-284`) and the queue
persists it without a provider search (`queue.rs:1245-1250`; a complete
movie NFO is `ready` with zero HTTP, §8.9). Manual assign that already
holds detail should end `ready` when bind completes.

#### 8.5 Queue classes and fairness (v1 constant, not a setting)

**No separate jobs table** (Rule 4.11). Two SELECT classes over item
status:

1. Search work: `metadata_status = 'pending'`
2. Enrich work: `metadata_status = 'matched'`

Negative-result cache (§3) still applies to search only.

**Order (product drain loop; derive at SELECT):**

1. Search groups that intersect Visible  
2. Enrich groups that intersect Visible  
3. Search background  
4. Enrich background  

Bands still derive at SELECT (no denormalised priority column). Continue
watching is empty until Block 2; Search boost remains reserved undesigned
until Block 3. Group fold is unchanged: one resolve per search
`query_key`; a group's band is `min(member bands)`.

Resume-after-restart is automatic: still-`pending` and still-`matched`
rows are selected again; `ready` / `unmatched` are not.

This remains a **second scheduling concept** beside the scan pool
(`WorkKind::Probe | Extract | Map`). Metadata is HTTP-bound and gated by
the API rate limiter (§7); scan work is disk/CPU-bound. Do not merge them.

#### 8.6 Band predicates and Visible proxy

| Band | How the SELECT knows |
|---|---|
| Continue watching | Join on watch-progress when Block 2 exists; empty until then |
| Visible | Server browse-unit proxy (below); expressible today |
| Search | **Reserved, undesigned.** No boost table or expiry schema here (Rule 4.7); Block 3 designs the predicate with the use case |
| Recently added / background | Remaining search or enrich work, `id DESC` |

**Visible proxy (not client scroll hints):** first paint of the default
library grid, keyed by library kind — **movies** for a movies library,
**shows** (distinct series, not episodes) for a shows library. Rank is
over **all** browse units in the library (not pending-only), so
already-terminal neighbours do not steal slots from units still on the
first screen. **N ≈ 40** means roughly one cold first screen — a
constant, not a setting; the number may move with Block 3
poster-card layout without amending this ADR. Try the proxy before any
chatty client→server visibility hint.

**Show browse unit:** when episode links exist, prefer
`tv|tmdb:{show_id}` (ADR-0029 / shipped product); else the soft key
`clean_show_title` → yearless `query_key`. Soft keys are **not** watch
`item_key`s (ADR-0025).

Episode-sorted item lists are the wrong unit for TV: top-N episodes
collapse to one or two `query_key` groups and measure nothing.

**Reordering under drain:** a view already rendered must not re-sort
under the user while a drain is in flight — that is a **client**
snapshot concern. Do not ban writers from updating `media_items.title`
to "solve" it; permanently sorting on filename while cards show canonical
titles is worse than a one-time re-sort.

#### 8.7 First-screen success criterion (adult)

Full-library wall is an ordering problem wearing a throughput costume.
Fan-out stays dead unless this gate fails for the right reason.

**Adult `T_first_screen`:** wall seconds from drain start on a cold
pending dogfood library until every browse unit in the Visible proxy set
is **search-terminal**
(`metadata_status IN ('matched','unmatched','ready')`), with a poster
**path** present for the `matched`/`ready` subset (CDN **bytes** optional
for the pass bar — report both “path known” and “bytes cached”). Report
`proxy_matched` / `proxy_ready` / `proxy_unmatched` beside the time.
Image download remains ADR-0027.

**Kids first screen** is not this metric: only units that are cert-ready
for the profile (`ready` with cert policy; fail-closed) count. Block 2
owns the evaluator.

**Prediction (search tier):** dogfood Visible ~80 units; measured
search-only + CDN model ≈ **~28 s** path-known
(`notes/grid-fast-vs-full-metadata-2026-08-04.md`). Full enrich
(detail + seasons) is ~5–14× slower depending on TV share and is **not**
on the adult first-screen critical path once two-tier is live.

**Pass:** adult `T_first_screen` ≤ 60 s. ~30 s confirms the search-tier
model; ~55 s means the proxy path costs ~1.8× plain search drain —
investigate before fan-out. Pre-two-tier measures that gate on
`ready`|`unmatched` alone remain valid **labeled** historical numbers;
do not mix them with search-terminal claims.

#### 8.8 Drain execution shape

Product drain (own SQLite connection; scan never waits):

1. Select and run **search** work for the fairness order (§8.5).  
2. Select and run **enrich** work for the fairness order.  
3. Idle when neither class has rows.

Within a group, search then detail remain serial where both run in one
process path; after the split, search and enrich are separate selections.
v1 walks groups **serially** (one resolve at a time). While that holds, a
concurrency knob does nothing — Rule 4.11: engage it with group-level
fan-out or remove it; do not ship a dead tunable. Fan-out's only axis is
across groups.

Season fetch is part of **enrich** for live `TmdbClient`, not a separate
jobs table. First-run **request count** for leave measures must still
publish search + detail + seasons (and rescan with no search on
unchanged); wall for **adult first screen** uses the search-terminal
definition above.

Prefix probes (`QUEUE_MAX_GROUPS`) are not representative of full-library
cost when the first N groups skew movie-heavy (show detail payloads are
larger — §4). Record movie/show group split on every probe; do not
extrapolate wall time from a prefix.

#### 8.9 NFO display authority; skip TMDB when complete

NFO fields are the display authority for the item they sit beside. A
movie NFO is **complete** when it has a non-empty title **and** a TMDB id
**and** meaningful content (plot, genres, or cast). TV never claims
completeness — shows still fetch TMDB detail and seasons, so an NFO never
starves a show of season data.

- **Complete movie NFO:** search tier persists the NFO canonical row,
  writes the `tmdb:movie:{id}` link, and sets `ready` directly — **zero
  TMDB HTTP calls** for the item.
- **Incomplete NFO:** TMDB search / detail runs as usual; enrich reloads
  the sidecar and persists `merge_prefer_left(nfo, tmdb)` — every
  non-empty NFO field wins (ids per field; arrays only when non-empty),
  TMDB gap-fills the empties. Episode NFOs (`episodedetails.nfo`) never
  merge into show-level detail (kind-mismatch guard).

A movie NFO with a TMDB id but incomplete content still skips the
search-tier TMDB search (existing id short-circuit); only the enrich
detail call is skipped when the NFO is complete.

#### 8.10 Series identity cascade (as landed, RC3–RC8)

This section records the series-identity order the search tier runs today,
per resolve group (folder-scoped for TV since RC8), in one pass. It
describes shipped behaviour: every claim maps to a `file:line` in
`server/crates/metadata/`. It is the rewrite of the cascade the abandoned
`metadata/s1-s2-status-and-series-cascade` branch described but never
shipped; that branch's text is superseded by this one and nothing is
cherry-picked from it.

For a TV group the search tier resolves series identity in this order,
and the first step that yields identity wins:

1. **`tvshow.nfo` at the show root.** `search_one_group` loads the show
   root NFO once per group via `show_root_nfo_xml` (`queue.rs:1600`),
   walking up from the episode path and bounded by the **library path**,
   not by a hop count. A `tvshow.nfo` carrying a usable TMDB show id
   resolves the group with zero provider calls
   (`resolve.rs:253-258`, `MetadataOrigin::Nfo`). A `tvshow.nfo` carrying
   only imdb/tvdb ids carries them forward to `/find`
   (`resolve.rs:261-262`). A corrupt `tvshow.nfo` is `NfoInvalid`:
   terminal `unmatched`, fix-flow eligible — the same single concept as a
   corrupt per-file NFO (`resolve.rs:289-294`).
2. **Episode NFOs stay readable.** `nfo_sidecar_xml` (`queue.rs:1578`)
   returns the per-file `.nfo` / `episodedetails.nfo` and never
   `tvshow.nfo`, so the episode NFO is not masked by the show NFO. A
   per-file NFO that itself carries a usable TMDB id resolves the group
   directly with zero provider calls (`resolve.rs:279-284`,
   `MetadataOrigin::Nfo`). Episode NFO external ids are episode-level and
   are never sent as a show lookup (`resolve.rs:286-287`); sending them
   can only miss `tv_results`.
3. **Stored folder series row (ADR-0033, landed RC8).** A folder whose
   `series` row (migration 016) holds a show id binds with zero provider
   calls when the already-persisted detail payload passes the folder
   name/year cross-check (`resolve.rs:335-382`, `match_method` "series_row"):
   a local read against the ADR-0029 payload, never a re-fetch, so a
   rescan of an unchanged library issues zero requests for identified
   folders (Gate 3). The cross-check reuses `find_hit_reject_reason`
   (`match_score.rs:167`) — the same gate a `/find` hit must pass, one TV
   title-match predicate (Rule 4.11). Disagreement, or a missing stored
   detail, clears the id and falls through to search; a wrong stored id
   never wins (ADR-0033 §8). The queue reads the row via
   `series_show_id_for_folder` (`queue.rs:309`) and writes it on a fresh
   TV match via `upsert_series_row` (`queue.rs:326`). Group formation is
    folder-scoped: `status_query_groups` (`queue.rs:542`) keys TV groups on
    `(library_id, show_folder)` instead of the folded title, so two
    fold-colliding folders are separate groups and the D2 wrong-match
    mechanism is gone by construction. Browse grouping follows the same
    identity: `visible_show_unit_key` (`queue.rs:289`) resolves a bound item
    through its links via `tmdb_show_for_media_item` (`queue.rs:337`) first,
    then the folder's `series` row for an unbound item, then the soft key.
4. **NFO imdb/tvdb via `/find`, with a name-and-year cross-check.**
   `TmdbClient::resolve` tries `/find` (`external_source=imdb_id`, then
   `tvdb_id`) before a title search when the attempt carries an external
   id (`tmdb/mod.rs:473`, `:500-504`). `find_tv_by_external_id`
   (`tmdb/mod.rs:178`) treats an HTTP 404 as a soft miss (`None`) via
   `get_json_optional` (`tmdb/mod.rs:184`). A `/find`-returned show id
   must pass the cross-check: `find_hit_reject_reason` (`match_score.rs:167`)
   reuses the matcher's own `name_matches_query` / `norm_key` predicate
   (one TV title-match concept, Rule 4.11) against the group's cleaned
   folder title and `(YYYY)`. A hit that disagrees is discarded and the
   resolver falls through to a plain title search with the external id
   cleared (`resolve.rs:479-490`) — a wrong external id fails into search,
   it never wins, and it is never written as a link. A `/find` 404 or a
   find-derived detail 404 becomes `FindMiss` (`resolve.rs:143`), which
   clears the external id and falls through to search (`resolve.rs:472-478`);
   a genuine provider error (timeout, 5xx, 429) propagates as
   `ResolveError::Provider` and stays retryable. The search tier never
   surfaces the stored-id `NotFound` terminal from RC3's enrich path
   (below); there a 404 is terminal, here it is a find miss.
5. **Folder year plus title search at the 0.80 floor.** The TV search
   year is `g.year.or(g.library_year)` (`queue.rs:1242`) — the folder's
   `(YYYY)`, mapped to `first_air_date_year` in the provider search
   (`tmdb/mod.rs:156`). Title search runs at the 0.80 floor (§2); the year
   change also re-keys the negative-result cache (`top gear|-` →
   `top gear|2002`), which is the intended targeted re-search (§3).
6. **Ids before the negative cache.** An id lookup is never suppressed by
   a stale title miss: the negative-cache `should_skip` gate applies in
   the attempt loop only when the attempt carries no NFO external id
   (`resolve.rs:405-414`), so a `/find` for a tvshow.nfo imdb/tvdb id
   always runs. After a find miss the fall-through search **is**
   cache-gated, so a live `below_threshold` / `no_results` row suppresses
   it and a rescan before `next_retry_at` does not re-run find+search
   (§3). A successful write clears the stale row.

Enrich picks up after identity: episode `ready` is gated on episode
identity, a series whose episode identity TMDB cannot supply is terminal
`unmatched` with the `tmdb:show:{id}` link kept, and `matched` after a
successful enrich means a provider error (retry), not a resting state —
all per §8.1 / §8.4 as amended 2026-08-05. A stored-show-id detail 404 is
terminal `unmatched`, not a retryable provider error
(`resolve.rs:109`; the enrich `Err(NotFound)` arm in `enrich_one_group`,
`queue.rs:1532`): that is the drain-fixpoint property, and it is why a
drain of an unchanged library reaches `groups == 0` (Gate 3 "rescan
generates no search requests"). On the browse side, `visible_show_unit_key`
(`queue.rs:289`) keys on the show id from `tmdb_show_for_media_item`
(`queue.rs:337`) — which reads a `tmdb:episode:` link and, failing that,
the `tmdb:show:{id}` link directly — and, only when no link exists, falls
back to the folder's `series` row, so a widened-`unmatched` episode with
only a show link keys with its bound siblings instead of a soft key.

**Known miss clusters (unchanged, stated plainly):** specials and `S00`
files parse as `kind='movie'` and search as standalone movies; and
absolute episode numbering for anime is unsupported. Folder-scoped series
identity, in contrast, **is shipped** (ADR-0033, RC8): group formation
keys on the show folder (`status_query_groups`, `queue.rs:542`), so two
fold-colliding folders (`Shameless (US)` / `Shameless (UK)`) are separate
groups and no longer resolve together or share an id — the D2 wrong-match
mechanism the autopsy named is gone by construction, and the regression
test `fold_colliding_folders_resolve_to_different_series_ids`
(`queue.rs` tests) fails on any tree that reintroduces fold-keyed
grouping. No in-memory or link-derived substitute exists; the branch's
`series_cache` is absent from the tree (the identifier has no hits), per Decision 5.

## Alternatives considered

**Daily-export FTS matching index.** Rejected in Continuity; spike evidence
in `notes/fts-vs-search-match-quality-2026-08-02.md`. Search is primary.

**Nightjar-hosted metadata cache / proxy.** Rejected in Continuity: TMDB
rate-limits by IP, not by key.

**Auto-match at confidence 0 (always take top-1).** Rejected: admits the
`exact_title` multi-hit wrong class (One Piece on the spike sample) and
feeds kids / watch state a wrong id.

**Threshold 0.85 (unique exact title only).** Rejected: drops movie
coverage from 97.2% to 85.6% on the sample by discarding correct
`exact_title_year` multi-hit rows scored 0.80. 0.80 already clears the
known wrong class.

**Negative cache keyed on file path.** Rejected: a rename that improves
the cleaned title would keep skipping search. Key on the search input.

**Raw payload per media_items row.** Rejected: ~2.5 GB projection vs
~0.5 GB entity-keyed; episodes would duplicate show/season JSON.

**User TMDB key in SQLite.** Rejected: the DB is what operators copy and
back up casually; third-party credentials need a tighter file. Same
decision OpenSubtitles will inherit.

**Application key only in environment / secrets, not embedded.** Rejected:
core behaviour must work by default. The embedded key is the
default; the user key is the escape hatch.

**Separate metadata jobs table.** Rejected (Rule 4.11): item
`metadata_status` plus the negative-result cache already express work and
backoff; a jobs table would duplicate that.

**One rate limiter shared by metadata API and artwork CDN.** Rejected:
different hosts, different ceiling types (request rate vs connection cap);
see §7.

**Denormalised `queue_band` / priority column on `media_items`.** Rejected:
bands are SELECT predicates over durable facts (status, browse-unit rank,
future watch-progress). A priority column is state someone must keep
correct for no query benefit (Rule 4.11).

**Client scroll/visibility hints for the Visible band.** Rejected for v1:
chatty API; try first-N browse-unit proxy first. Revisit only if
`T_first_screen` fails the prediction for the right reason.

**Search boost table (`item_id` + expiry) in this settle.** Rejected as
premature (Rule 4.7): the Search band is reserved undesigned until Block 3
has the use case.

**Ban writers from updating `media_items.title` to freeze browse sort.**
Rejected: permanently diverges filename sort from canonical card titles.
Reordering-under-drain is a client snapshot concern (§8).

**Gate on `metadata_status = 'ready'` alone for first screen.** Rejected
for **adult** paint: unmatched is terminal, and requiring full detail +
seasons before the grid paints makes first screen track full-library
enrich cost (measured seasons dominate). Adult first screen is
**search-terminal** (`matched` \| `unmatched` \| `ready`); posters on the
matched/ready subset. Kids still require `ready` + cert (fail-closed).

**Single-pass search+detail before any grid paint (pre-two-tier only).**
Superseded for adult first screen by §8 once implementers land `matched`.
Historical dogfood drains that wrote `ready` in one pass remain valid
evidence for leave measures labeled pre-two-tier.

**Staging table for sparse “matched” metadata.** Rejected (Rule 4.11): one
canonical row per entity; sparse search write then detail overwrite
(ADR-0029 upsert). No second matched table.

**Re-search on enrich.** Rejected: wastes budget and risks identity churn;
enrich is id short-circuit only (§8.4).

**Content cert only at kids Block 2 (third library pass).** Rejected for
projection: store cert on enrich now so kids can fail-closed without
re-fetching 24.8k details. Ladder/region files remain Block 2.

**TVDB season fallback.** Rejected (Continuity); soft season 404 + path
keys only.

## Consequences

- Matcher code owns the confidence function, the 0.80 constant, and the TV
  collision pin (§2); tests assert method → score, pin uniqueness, and
  threshold gating on fixture result lists. The constant is
  sample-calibrated; raising or lowering it is a re-calibration against a
  larger hand-scored set, recorded in an ADR amend. Do not retune the
  mid-table discrete weights without that evidence.
- Unmatched (below threshold, no results, or NFO-less miss) keeps the
  path `item_key` from ADR-0025 until a manual fix or a successful retry.
  Path keys lose history on rename and do not survive library
  remove-and-re-add, so the below-floor rate is also the fraction of the
  library with fragile watch state. Full-library measure (testdata
  excluded, after collision pin + title fold): about **4.5%** below floor.
  The calibration sample had projected ~11%; ADR-0025 Consequences should
  cite the measured figure when amended.
- Gate 3 measurement must report auto-match rate and wrong rate **at
  threshold 0.80** on the dogfood library, not top-1 with no floor.
- Filename cleaner folds (`and`↔`&`, apostrophes, colons, diacritics) share
  one `norm_key` path. New folds need a corpus fixture row
  (`fold_corpus.json`) before the rule — same discipline as a playback bug.
  Cleaner changes re-key the negative-result cache (§3); ship a version
  stamp or sweep so old rows do not accumulate as unhittable sediment.
  Historical cache dumps are not a live bug list without current-cleaner
  repro.
- **Raw payload store size (measured 2026-08-02, dogfood, testdata
  excluded):** `SUM(LENGTH(payload))` = **317 MiB** across 4,193 entity
  rows (1,721 movie / 682 tv / 1,790 season). The §4 projection of
  ~420 MB median was high but same order; ship **uncompressed** UTF-8
  JSON so the SQLite file stays inspectable. Do not confuse this column
  sum with the whole database file size (library rows + payloads). Gzip
  remains a pure implementation option if the figure becomes a user
  complaint; pruning `credits` from `append_to_response` is the better
  design lever if bandwidth/storage need cutting (credits alone were
  ~70% of movie payload bytes in a sample).
- Artwork ADR owns the `image.tmdb.org` connection cap; it must not reuse
  the metadata API request-rate limiter.
- Queue workers select `pending` (search) and `matched` (enrich);
  writing `matched` / `ready` / `unmatched` is what makes progress durable
  across restarts. Fairness order is §8.5 (Visible search before Visible
  enrich, then background).
- Adult `T_first_screen` is search-terminal (§8.7). Publish leave measures
  with that definition after two-tier lands; keep pre-two-tier ready-gate
  numbers only if labeled.
- Sparse search write must not invent cast/genre names/cert/collection/
  episode rows; detail overwrite fills them. Cert projection on enrich is
  required for kids fail-closed without a third pass (§8.4).
- Genre id→name may be a const map at detail map time. Kids classification
  ladder remains Block 2.
- Migration 014 (or next free after 013) extends the status CHECK for
  `matched` before any writer of that value (Rule 4.9 / 6.1).
- Artwork may warm at `matched` for Visible posters (ADR-0027); serve path
  already keys on `item_key`.
- Release engineering must be able to rotate `NIGHTJAR_TMDB_APP_KEY` and
  ship a new binary; document that beside the secrets-file override.
- Ask TMDB whether embedding an application key in a self-hosted binary at
  scale is acceptable (Continuity follow-up); this ADR does not depend on
  a favourable answer, but a public reply beats forum inference.
- Manual-fix (ADR-0028) clears collection id/name on reassignment; writers
  must populate §6 on the initial detail write or the clear is a no-op forever.
- **Browse grouping keys on links, not on `metadata_status`.**
  `visible_show_unit_key` (`server/crates/metadata/src/queue.rs:289`) calls
  `tmdb_show_for_media_item` (`queue.rs:337`) for a show id; that function
  reads only `item_links`, never `metadata_status`. The API applies no
  status filter either: `list_items` (`server/crates/db/src/store.rs:410`)
  selects every row for the library and `to_dto`
  (`server/crates/api/src/routes/items.rs:490`) passes `metadata_status`
  through as a field, not a filter. Widening `unmatched` (§8.1) therefore
  changes no client-visible grouping and implies no OpenAPI change: what
  the user sees is controlled by which link a row carries, which this
  amendment requires to survive (`tmdb:show:{id}` kept on a widened-
  `unmatched` row), not by which status string the row has. A grouping
  fallback so an episode with only a show link keys the same as its bound
  siblings is separate code work (RC3), not this amendment.
