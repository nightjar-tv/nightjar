# ADR-0026: Metadata pipeline

- Status: accepted
- Date: 2026-08-02
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

Wrong matches are worse than unmatched: they orphan watch state onto the
wrong `item_key` (ADR-0025) and feed the kids evaluator a wrong
certification. The spike measured search top-1 at 98.6% combined on a
280-item stratified dogfood sample; that is sample-bounded evidence, not
the Gate 3 criterion met.

## Decision

### 1. Resolution path (cited, not re-argued)

NFO first, then TMDB search + detail. An NFO that already carries a TMDB id
skips search. Matching uses TMDB search directly. Pipeline shape, queue
priority, state machine, `append_to_response`, change lists, long refresh
windows, and the shared rate limiter across metadata and artwork download
are as in `metadata-artwork-strategy.md` and Phase 3 Block 1. Episode ids
come from season append per ADR-0025. Artwork acquisition, image pipeline,
and lazy download are out of scope here.

### 2. Match confidence and threshold

Confidence is a server-side score of the search result list against the
cleaned filename title and year. It is not TMDB's popularity field. v1 uses
the discrete method classes from the spike matcher:

| Method | Score | Meaning |
|---|---:|---|
| `exact_title_year` (unique) | 0.98 | Normalised title hit and year match; one candidate |
| `exact_title` (unique) | 0.90 | Title hit; no year on the file, one candidate |
| `exact_title_year` (multi) | 0.80 | Title+year hit; more than one candidate |
| `exact_title` (multi) | 0.72 | Title hit; more than one candidate |
| `exact_title_year_nearest` | 0.70 | Title hit; year present but no exact year row |
| `top1_rank` | 0.45–0.65 | No exact title hit; took search ranking |

**Auto-match only when confidence ≥ 0.80.** Below that the item stays
unmatched and keeps its path `item_key`. Do not write a provider key for a
low-confidence hit.

0.80 is the lowest threshold at which the spike's search path had 100%
precision on both movies and episodes. On that sample:

| Threshold | Movies cov / prec | Episodes cov / prec |
|---|---|---|
| 0.00 | 98.9% / 100% | 100% / 98% (2 wrongs) |
| 0.80 | 97.2% / 100% | 88% / 100% |
| 0.85 | 85.6% / 100% | 88% / 100% |

The two episode wrongs were both `One Piece` at 0.72 (`exact_title` multi:
live-action id over the anime series). Lowering the floor to clear Gate 3
coverage would re-admit that class. Coverage climbs by improving inputs
(filename cleaner folding `&`/`and` and apostrophes; year extraction), not
by accepting multi-hit title matches.

Gate 3 still requires a hand-scored measure of auto-match rate and wrong
rate at this threshold on the full dogfood library. The spike is the
starting point, not that measure.

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
putting tokens in SQLite (Phase 3 cross-cutting note). The application key
is not copied into the secrets file.

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

## Consequences

- Matcher code owns the confidence function and the 0.80 constant; tests
  assert method → score and threshold gating on fixture result lists.
- Unmatched (below threshold, no results, or NFO-less miss) keeps the
  path `item_key` from ADR-0025 until a manual fix or a successful retry.
- Gate 3 measurement must report auto-match rate and wrong rate **at
  threshold 0.80** on the dogfood library, not top-1 with no floor.
- Filename cleaner fold for `&`/`and` and apostrophes remains a separate
  small slice; it is the cheapest coverage recovery without moving the
  threshold.
- Artwork ADR consumes detail payloads (image paths) already stored here;
  it does not re-fetch metadata to learn poster URLs.
- Release engineering must be able to rotate `NIGHTJAR_TMDB_APP_KEY` and
  ship a new binary; document that beside the secrets-file override.
- Ask TMDB whether embedding an application key in a self-hosted binary at
  scale is acceptable (Continuity follow-up); this ADR does not depend on
  a favourable answer, but a public reply beats forum inference.
