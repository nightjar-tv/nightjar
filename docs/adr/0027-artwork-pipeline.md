# ADR-0027: Artwork pipeline

- Status: accepted
- Date: 2026-08-04
- Depends on: ADR-0026 (§7 CDN cap separate from API limiter; §8 first-screen
  poster reference only); ADR-0028 (§5 keying `item_key` + kind); ADR-0029
  (artwork_json on canonical)
- Gate: Gate 3 — artwork served from Nightjar cache; disk measure for ~24.8k
  items at the chosen thumbnail set
- Related: strategy note
  (`nightjar-meta/notes/design/metadata-artwork-strategy.md`); Phase 3 Block 1

## Context

Canonical rows already store TMDB relative paths and NFO local thumbs. Clients
must never hit `image.tmdb.org`. Assign must invalidate under the old
`item_key` and enqueue under the new. Thumbnail pixel widths must not be
invented without a size measure; dogfood validates the first cut.

## Decision

### 1. Identity and disk layout

Artwork keys on **`item_key` + kind** (poster, backdrop, still). Versions of
one movie share one poster (ADR-0028 §5).

On disk under `{NIGHTJAR_DATA_DIR}/artwork/`:

```text
artwork/{safe_item_key}/{kind}.orig
artwork/{safe_item_key}/{kind}.w{width}.jpg   # derived; optional until measured
```

`safe_item_key` replaces path-unsafe characters (`:`, `/`) with `_`. No
nested hierarchy beyond that.

### 2. Source priority

1. Local NFO / filesystem art (path on disk) → derive only; leave original.
2. Remote TMDB path from canonical `artwork_json` → download original + derive.
3. Placeholder (client-side / empty 404) when neither exists.

### 3. Thumbnail set (first cut; re-measure on dogfood)

| Role | Width |
|---|---:|
| Card / rail | 342 |
| Detail hero (backdrop) | 780 |

Formats: JPEG for derived; original kept as downloaded (often JPEG). Palette
and blurhash are **deferred** until a measure shows card paint needs them;
brand still wants palette later (strategy note). Incomplete, not provisional
(Rule 4.8): serve works without palette.

### 4. `image.tmdb.org` connection cap

Separate from the metadata API rate limiter (ADR-0026 §7). v1: **8**
simultaneous downloads (under TMDB’s ~20 connection guidance). Constant, not
a setting (Rule 4.12).

Base URL: `https://image.tmdb.org/t/p/original{path}`.

### 5. Lazy acquisition

Download on first serve miss for Visible/ready items, or when assign enqueues
a new key. Background drain may warm posters for ready Visible units later;
not required to ship serve.

### 6. Serve API

`GET /api/v0/artwork/{itemKey}/{kind}`  
Optional query `w=` (342 or 780). Default: original if present else 342.

Clients use opaque `item_key` from future metadata responses; today fix flow
returns `itemKey` after assign.

### 7. Invalidate on assign/clear

Delete `{safe_item_key}/` tree for old keys; enqueue download for new key when
canonical has a poster path.

## Consequences

- Disk measure after dogfood full library: sum of `artwork/` vs ADR projection.
- Raising thumb widths is an ADR amend with a before/after byte measure.
- Palette/blur remain named follow-ups, not fake columns.
- Fix API `ArtworkInvalidate` becomes the real cache clearer.

## Alternatives considered

**Shared limiter with metadata API.** Rejected (ADR-0026 §7).

**Per-version posters.** Rejected (ADR-0028 §5).

**Clients fetch TMDB CDN.** Rejected: self-hosted posture; offline after first
fill; rate concentration.
