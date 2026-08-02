# ADR-0026: Metadata pipeline

- Status: accepted
- Date: 2026-08-02
- Amended: 2026-08-02 — TV multi-exact collision pin; title-fold corpus
  discipline; full-library match rates after pin + fold
- Amended: 2026-08-02 — raw payload store measured at 317 MiB
  (`SUM(LENGTH(payload))`); ship uncompressed
- Amended: 2026-08-03 — API rate limiter is not shared with artwork;
  metadata queue is a query over item `metadata_status`, not a jobs table
- Depends on: ADR-0025 (item identity / season-append episode ids)
- Gate: Gate 3 — auto-match ≥95% correct; every mismatch fixable in-UI in
  under 30 seconds; API requests per 1,000 items published for first run and
  rescan
- Related: Continuity settled set (direct-to-TMDB, FTS rejected, application
  key); strategy note
  (`nightjar-meta/notes/design/metadata-artwork-strategy.md`); match spike
  (`nightjar-meta/notes/fts-vs-search-match-quality-2026-08-02.md`); Phase 3
  Block 1 (`nightjar-meta/docs/PHASE_3_REVISED.md`)

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

NFO first, then TMDB search + detail. An NFO that already carries a TMDB id
skips search. Matching uses TMDB search directly. Pipeline shape, queue
priority, state machine, `append_to_response`, change lists, and long refresh
windows are as in `metadata-artwork-strategy.md` and Phase 3 Block 1.
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
| `exact_title_year` (multi) | 0.80 | Title+year hit; more than one candidate |
| `exact_title` (multi) | 0.72 | Title hit; more than one candidate; **superseded for TV** by the collision pin below when a discriminator fires |
| `exact_title_collision_unpinned` | 0.72 | Multi exact-title; no discriminator selected exactly one candidate |
| `exact_title_library_year` | 0.90 | Multi exact-title; library premiere year uniquely matched `first_air_date` year |
| `exact_title_episode_count` | 0.90 | Multi exact-title; library episode count uniquely matched (soft) a candidate's `number_of_episodes` |
| `exact_title_season_count` | 0.90 | Multi exact-title; library season count uniquely matched `number_of_seasons` |
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
| 1 | Premiere year (earliest episode `year`, else show-folder `(YYYY)`) | `first_air_date` year (search hit) | Exact year |
| 2 | Episode file count under the show | `/tv/{id}` `number_of_episodes` | Soft (±15% or ±5) |
| 3 | Distinct season numbers present | `/tv/{id}` `number_of_seasons` | Exact |

Detail shapes for (2) and (3) are fetched only when (1) did not pin — tens of
ambiguous shows, not per file. Wrong series match remains worse than none:
ambiguous residue goes to the fix flow (ADR-0028), not a looser floor.

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

Enrichment state lives on the item row as `metadata_status`:

| Value | Meaning |
|---|---|
| `pending` | Not yet resolved (default for new/scanned items) |
| `ready` | Metadata written (NFO or TMDB hit + payload) |
| `unmatched` | Resolve finished without a provider match (below floor, no results, invalid NFO) |

The work queue is `SELECT` over `metadata_status = 'pending'`, skipping
network when the negative-result cache (§3) says so. **No separate jobs
table** — that would be a second structure tracking the same fact (Rule
4.11). Resume-after-restart is automatic: still-`pending` rows are selected
again; `ready` / `unmatched` are not.

This is deliberately a **second scheduling concept** beside the scan pool
(`WorkKind::Probe | Extract | Map` with its own priority ordering). Metadata
is HTTP-bound and gated by the API rate limiter (§7); scan work is
disk/CPU-bound. Do not merge them from intuition — shared structure would
couple unrelated backpressure.

#### Band ordering (derive at SELECT — no priority column)

Bands are predicates in the queue query, not a denormalised
`queue_band` / `priority` column on `media_items` (Rule 4.9 / 4.11).
Group fold is unchanged: one resolve per search `query_key`; a group's
band is `min(member bands)` — so one episode in a higher band promotes
the **entire show group** (intentional once Visible is show-unit).

| Band | How the SELECT knows |
|---|---|
| Continue watching | Join on watch-progress when Block 2 exists; empty until then |
| Visible | Server browse-unit proxy (below); expressible today |
| Search | **Reserved, undesigned.** No boost table or expiry schema here (Rule 4.7); Block 3 designs the predicate with the use case |
| Recently added / background | Remaining `pending`, `id DESC` |

**Visible proxy (not client scroll hints):** first paint of the default
library grid, keyed by library kind — **movies** for a movies library,
**shows** (distinct series, not episodes) for a shows library. Rank is
over **all** browse units in the library (not pending-only), so
already-ready neighbours do not steal slots from pending units still on
the first screen. **N ≈ 40** means roughly one cold first screen — a
constant (Rule 4.12), not a setting; the number may move with Block 3
poster-card layout without amending this ADR. Try the proxy before any
chatty client→server visibility hint.

**Provisional show browse unit (v1):** there is no durable series row yet
(ADR-0025 owns movie/episode `item_key` only). Until a series handle
exists, a shows-library browse unit is the same soft key the resolve
queue already uses: `clean_show_title` → yearless `query_key`. That is
filename-derived and may split a show if episode titles clean differently;
it is **not** a watch `item_key` and must not be mistaken for one. Durable
series identity is a later schema/ADR.

Episode-sorted item lists are the wrong unit for TV: top-N episodes
collapse to one or two `query_key` groups and measure nothing.

**Reordering under drain:** a view already rendered must not re-sort
under the user while a drain is in flight — that is a **client**
snapshot concern. Do not ban writers from updating `media_items.title`
to "solve" it; permanently sorting on filename while cards show canonical
titles is worse than a one-time re-sort.

#### First-screen success criterion

Full-library wall (~22 min movie+show drain) is an ordering problem
wearing a throughput costume. Fan-out stays dead unless this gate fails.

**T_first_screen:** wall seconds from drain start on a cold pending
dogfood library until every browse unit in the Visible proxy set is
**terminal** (`metadata_status IN ('ready','unmatched')`). Poster
reference (payload `poster_path` or NFO thumb) is required only for the
**ready** subset; unmatched units are holes, not infinite waits. Report
`proxy_ready` / `proxy_unmatched` beside the time. Image bytes remain
ADR-0027.

**Prediction:** dogfood Visible union ≈ 40 movie groups + 40 show groups.
At measured serial-drain rates (~1.84 HTTP/group, ~4.9 req/s) that is
~80 × 1.84 / 4.9 ≈ **30 s**. (Movies-only would be ~15 s; do not quote
that for the union.) Show detail payloads are ~1.8× movie (§4) and may
stretch wall slightly above the request-count model. Still well inside
the pass bar.

**Pass:** `T_first_screen ≤ 60`. ~30 s confirms the model; ~55 s means
the proxy path costs ~1.8× the plain drain — investigate before fan-out.

v1 drain resolves **movie and show (episode-group) search+detail only**.
Season detail (`/tv/{id}/season/{n}`, ADR-0025 episode ids / §4 season
append) is not yet enqueued; first-run request count and wall time must
not be quoted as complete until that pass exists.

v1 `drain_pending` walks groups **serially** (one resolve at a time). While
that holds, a concurrency knob does nothing — Rule 4.11: engage it with
group-level fan-out or remove it; do not ship a dead tunable. Search→detail
is inherently serial *within* a group; fan-out's only axis is across groups.

Prefix probes (`QUEUE_MAX_GROUPS`) are not representative of full-library
cost when the first N groups skew movie-heavy (show detail payloads are
larger — §4). Record movie/show group split on every probe; do not
extrapolate wall time from a prefix.

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
core behaviour must work by default (Rule 4.12). The embedded key is the
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

**Gate on `metadata_status = 'ready'` alone for first screen.** Rejected:
unmatched is terminal; at ~4.5% below-floor a 40-unit set is unmatched
~84% of the time and the gate never closes. Use terminal status; posters
only on the ready subset.

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
- Queue workers select `metadata_status = 'pending'`; writing `ready` /
  `unmatched` is what makes progress durable across restarts.
- Release engineering must be able to rotate `NIGHTJAR_TMDB_APP_KEY` and
  ship a new binary; document that beside the secrets-file override.
- Ask TMDB whether embedding an application key in a self-hosted binary at
  scale is acceptable (Continuity follow-up); this ADR does not depend on
  a favourable answer, but a public reply beats forum inference.
- Manual-fix (ADR-0028) clears collection id/name on reassignment; writers
  must populate §6 on the initial detail write or the clear is a no-op forever.
