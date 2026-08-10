# ADR-0027: Artwork pipeline

- Status: accepted; amended 2026-08-10 (§1 kind set, §2 local art is cached,
  §6 per-item advertisement, §8 language scope, §9 cardinality)
- Date: 2026-08-04
- Amended: 2026-08-04 — warm posters at `metadata_status = matched`
  (ADR-0026 §8 two-tier); 2026-08-10, the sections above, plus a correction
  recorded rather than edited away (see Corrections)
- Depends on: ADR-0026 (§7 CDN cap separate from API limiter; §8 first-screen
  poster path on matched/ready); ADR-0028 (§5 keying `item_key` + kind);
  ADR-0029 (artwork_json on canonical)
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

Artwork keys on **`item_key` + kind**. Versions of one movie share one poster
(ADR-0028 §5).

**The rendered set is poster, backdrop and logo** (amended 2026-08-10; the
original text named poster, backdrop and still). Each is in the set because a
surface draws it, and a kind with no surface is not fetched:

| kind | where it renders |
|---|---|
| poster | the grid tile |
| backdrop | the item page background, darkened with metadata over it |
| logo | the title treatment drawn over that backdrop instead of typed text, and player overlays |

`still` stays a stored kind and drops out of the rendered set. It is what the
episode canonical projection writes (ADR-0029 §1.2 gives episode rows a still
and nothing else), so it keeps arriving; nothing draws it yet.

**Landscape and thumb are deferred, not overlooked.** Jellyfin's `ImageType`
carries thirteen entries (Art, Backdrop, Banner, Box, BoxRear, Chapter, Disc,
Logo, Menu, Primary, Profile, Screenshot, Thumb) and their own documentation
calls only three of those the main types. `Thumb` is distinct from `Backdrop`
and is what Emby's client leans on, because a thumb usually carries the title
baked into the image, which is the job this ADR gives to a logo composited
over a backdrop instead. Neither is added until a rail needs one. The
reasoning is recorded so a later reader knows the set was chosen rather than
missed.

On disk under `{NIGHTJAR_DATA_DIR}/artwork/`:

```text
artwork/{safe_item_key}/{kind}.orig
artwork/{safe_item_key}/{kind}.w{width}.jpg   # derived; optional until measured
```

`safe_item_key` replaces path-unsafe characters (`:`, `/`) with `_`. No
nested hierarchy beyond that.

### 2. Source priority

1. Local NFO / filesystem art (path on disk) → copy into the cache, then derive.
2. Remote TMDB path from canonical `artwork_json` → download original + derive.
3. Placeholder (client-side / empty 404) when neither exists.

**Everything is cached locally, including art already on the array** (amended
2026-08-10). Step 1 previously said local files are derived in place and the
original left where it sits, on the reasoning that a second copy of a file we
already have is waste. That was wrong on three counts:

- A poster on the array is a network read per grid tile, and a grid is forty
  tiles at once. The cache exists to make that one local read.
- Browsing survives an unreachable library today, and must keep doing so. Art
  that lives only on the array disappears exactly when the library does, which
  is the moment a placeholder is least acceptable.
- Local files are frequently full resolution, so any resize step applies to
  them for the same reason it applies to a download.

The source priority above is unchanged: local art still wins over TMDB. What
changed is that winning means being copied, not being referenced in place.

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
a setting.

Base URL: `https://image.tmdb.org/t/p/original{path}`.

### 5. Lazy acquisition

This section was believed to be working when it was not; see Corrections
before reading disk-usage figures against it.

Download on first serve miss for Visible items that already have a poster
path (`matched` or `ready`), or when assign enqueues a new key. Background
drain **should warm posters for Visible units once they reach `matched`**
(ADR-0026 §8.3) so the adult search-terminal first screen can paint without
waiting for enrich. Warming at `ready` remains fine; waiting for `ready`
before any warm is not required for adult grid paint. Palette/blurhash stay
deferred (§3).

### 6. Serve API

`GET /api/v0/artwork/{itemKey}/{kind}`  
Optional query `w=` (342 or 780). Default: original if present else 342.

Clients use opaque `item_key` from future metadata responses; today fix flow
returns `itemKey` after assign.

**The item response says which kinds a title has** (amended 2026-08-10).
`GET /api/v0/items/{id}` carries an `artwork` array of `{kind, url}` holding
only the kinds that title actually has a source for, and `ArtworkKind` on the
wire is the three-value rendered set from §1 rather than every kind the store
can hold.

This is the substantive part of the amendment. §6 gave clients a URL shape and
no way to learn which kinds exist, which has two consequences. A detail page
cannot render a backdrop even once the fetcher supports one, because it has no
way to know whether asking is worthwhile. And it cannot tell **not cached yet**
from **this title has none**, which are different states needing different
behaviour: the first is a placeholder while the fetch runs, the second is a
layout drawn without that element at all. A URL offered for artwork that does
not exist is a broken image, which is worse than an absent one.

So absence in the array is the signal, and it is load-bearing. A kind missing
from the array does not exist for that title; a kind present is fetched on
demand at first request (§5) and the first GET may be slow, but it will not
fail for want of a source.

Serving still keys on `item_key` + kind (§1), so this adds no second identity
for artwork. It adds the one thing a client could not previously derive
without guessing.

### 7. Invalidate on assign/clear

Delete `{safe_item_key}/` tree for old keys; enqueue download for new key when
canonical has a poster path.

### 8. Language is server-scoped

One language, set by the server owner in `server_settings`. Not per profile,
and the reason is not that per-profile would be harder to build.

**The owner's downloads are already in a language.** The audio tracks, the
release, the folder naming: all of it. Artwork in a different language would be
inconsistent with the content itself, not merely with a viewer's preference,
and a poster whose title text disagrees with the audio that plays under it is
worse than one nobody chose.

Subtitles are per-profile for a reason that does not carry over. Multiple
subtitle tracks exist *inside the file*, so a profile choosing among them is
choosing among things that are all present and all correct for this copy. There
is one library and it has one language.

Precedent: ADR-0037 puts the certification region on the server for the same
class of reason, and `server_settings` is where that went.

**No language dimension in the cache key** (§1). The key stays `item_key` plus
kind. Adding a third axis would multiply the cache by a factor nothing reads.

### 9. Cardinality

One image per title per kind. Three rendered kinds, one language, roughly
24,800 titles, so roughly **74,400 files**.

That is a count and not a size. What those files weigh depends on the sizing
and format decision, which is open (see below).

## Corrections

**Lazy acquisition was not working as designed, and the number that suggested
it was did not mean what it appeared to** (recorded 2026-08-10).

21,769 canonical rows carrying artwork URLs against 70 files on disk was read
as §5 behaving correctly: art is fetched when something asks, little had been
asked for, so little was on disk. It was not that. The reader filtered
canonical paths on `starts_with('/')`, which kept the 25,116 TMDB-relative
paths and discarded all **44,540 whole URLs** written by the NFO `<thumb>` and
`<fanart>` projection. No movie had fetchable artwork at all. `ArtworkRef.path`
is documented as a local path or a remote URL, so the reader was wrong and the
data was right. Fixed at the read.

The serve path carried the same bug in a second place: it supplied the
canonical source only for posters, so a backdrop or logo request could never
find one and always missed as not cached. §1's kind set is only reachable
because both halves were fixed.

This is recorded rather than edited away because the shape of the mistake is
worth seeing. A plausible mechanism was fitted to a number without checking
that the number meant what it appeared to mean, and the mechanism fitted was
the one this ADR itself describes, which is exactly what made it convincing.

## Open, and deliberately not decided here

**Sizing, format and quality are one decision.** Measured originals: movie
backdrop 742 KB at 3840x2160, tv backdrop 1.27 MB, poster 231 KB at 1000x1500.
Three kinds across 24,800 titles at original size is roughly 85 GB; at
`w500` poster and `w1280` backdrop as JPEG, roughly 10 GB; as WebP, roughly
5 to 6 GB.

The format evidence is settled even though the decision is not. WebP has been
Baseline since September 2020 at 96.09% global support, covering Samsung
Internet 4+ (the Tizen engine), Android Browser 4.2+, Safari 14+ and iOS 14+,
and Chrome 32+, which is every profile in ADR-0022. Alpha, which logos need,
arrived at Chrome 32 and Android 4.2. **AVIF is excluded**: Chromium 85 means
any television older than roughly 2022 fails.

What is not decided is dimension, format and quality together, and §3's
thumbnail set is the first cut it would replace. This is the artwork
leave-measure's actual question.

**Logo source order.** Three sources, cheapest first:

1. TMDB `images.logos` via `append_to_response=images`, a parameter the
   existing requests do not send. Cheapest, untried, coverage unknown.
2. Local files. 29 of 40 sampled shows carry `clearlogo.png`, and there are
   **zero logo rows across 26,407 canonical rows**, so this is currently the
   only source that could serve one at all.
3. fanart.tv, which is the specialist source and means a new provider and a new
   key. Jellyfin's answer is an opt-in plugin; logos are not in their base
   install and their TVDB plugin does not fetch them either.

Measuring TMDB's logo coverage comes before building local-file discovery, so
that the second is known to fill a gap rather than duplicate the first.

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
