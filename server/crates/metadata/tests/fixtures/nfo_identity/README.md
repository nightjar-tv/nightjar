# nfo_identity fixture corpus

Eight synthetic NFO identity cases for the metadata identity and
resolution stage. Every title, name, plot, id, date, and payload in this
corpus is original synthetic content. The corpus is GPL-3.0-only.

## Authoring scope

Author-owned artifacts only:

- `xml/<case-id>/<phase>.nfo` - NFO input bytes
- `provider/<case-id>/<response>.json` - offline provider responses
- `manifest.json` - evidence, claim topology, and route inventory
- `README.md` - this file
- `PROVENANCE.md` - origin and hash record

The historical author stage withheld `expected.json` and every pass/fail
assertion; a separate adjudicator owns all verdicts. The checked-in
`expected.json` is that later independent adjudication artifact. The
author-owned manifest never predeclares a winner, a bound identity, or a
final state.

Nothing else in the product tree was created or changed. No tests, source
files, or generated reports were written.

## Route model

Provider requests follow TMDB v3 under `https://api.themoviedb.org/3`:

- `GET /find/{external_id}?external_source=imdb_id|tvdb_id`
- `GET /movie/{id}` with append_to_response
- `GET /tv/{id}` with append_to_response
- `GET /search/movie?query=...`
- `GET /search/tv?query=...`

Route classes in the manifest:

- `identity` - response for a request the identity stage may issue
- `content` - response for enrichment after identity binds
- `observability` - response so an adjudicator can inspect a claim

A request with no mapped response file simulates a provider miss. A case
that needs a real search ships a real search response; no case fakes a
search with a detail response.

## The eight cases

| Case | Kind | What it demonstrates | Identity route inputs |
|------|------|----------------------|----------------------|
| NFO-01 | movie | Complete: title plus usable TMDB plus plot/genre/cast. Zero provider requests. | none |
| NFO-02 | movie | Partial: title plus usable TMDB, no meaningful content. | none (identity); content detail shipped |
| NFO-03 | movie | Title/content only, no usable provider id. Real search route. | `/search/movie` + content detail |
| NFO-04 | movie | Conflicting TMDB/IMDb/TVDB claims; the usable TMDB id short-circuits identity (ADR-0026) and binds tmdb:movie:9200041. | observability details for all three entities |
| NFO-05 | show-root | `tvshow.nfo` with IMDb only, title/year match. `/find` plus detail cross-check can bind. | `/find/tt9300005`, `/tv/9300005` |
| NFO-06 | show-root | `tvshow.nfo` with IMDb only, title/year mismatch. Rejection/fallback/abstention routes. | `/find/tt9300006`, `/tv/9300006`, `/search/tv` (zero results) |
| NFO-07 | movie | Explicit zero-byte `absent.nfo`. Absent body is not identity evidence. | `/search/movie` + content detail |
| NFO-08 | movie | Malformed, corrected-body, and manual-assign phases. | `/search/movie` (miss), `/movie/9200008` |

## File inventory

### XML inputs (10 files, 7543 bytes total)

```
xml/NFO-01/complete.nfo
xml/NFO-02/partial.nfo
xml/NFO-03/content-only.nfo
xml/NFO-04/conflict.nfo
xml/NFO-05/tvshow.nfo
xml/NFO-06/tvshow.nfo
xml/NFO-07/absent.nfo            (zero bytes)
xml/NFO-08/initial-malformed.nfo
xml/NFO-08/corrected-body.nfo
xml/NFO-08/manual-assign.nfo     (byte-identical to corrected-body.nfo)
```

### Provider inputs (15 files, 21407 bytes total)

```
provider/NFO-02/movie_detail_9200002.json
provider/NFO-03/search_movie_keeper_of_the_lantern.json
provider/NFO-03/movie_detail_9200003.json
provider/NFO-04/movie_detail_9200041.json
provider/NFO-04/movie_detail_9200042.json
provider/NFO-04/movie_detail_9200043.json
provider/NFO-05/find_imdb_tt9300005.json
provider/NFO-05/tv_detail_9300005.json
provider/NFO-06/find_imdb_tt9300006.json
provider/NFO-06/tv_detail_9300006.json
provider/NFO-06/search_tv_gravelmere.json
provider/NFO-07/search_movie_kestrel_road.json
provider/NFO-07/movie_detail_9200007.json
provider/NFO-08/search_movie_secondhand_orbit_miss.json
provider/NFO-08/movie_detail_9200008.json
```

## Route input inventory

The `routes` array in `manifest.json` maps every response file to the
request it answers: method, path, query, class, sha256, and byte count.
The `route_inputs` array of each case lists those route ids in the order
the identity stage may walk them. For NFO-05 and NFO-06 the `/find`
responses are real external-id lookups that return `tv_results`; the tv
details serve the find/detail cross-check. For NFO-03, NFO-07, and
NFO-08 the search responses are real search responses with results.

## Synthetic universe

`manifest.json` records a `synthetic_universe` table that links every
fictional id to its synthetic entity. NFO-04's conflict is readable only
against this table: the tmdb claim names The Cobalt Divide, the imdb
claim names Split Tides, and the tvdb claim names Lavender Cut. Movie
TVDB identifiers are synthetic-universe attributes only; the shipped
`/movie` details do not expose a tvdb_id.

## Budgets

| Budget | Limit | Actual |
|--------|-------|--------|
| `.nfo` files | 10 | 10 |
| XML bytes | 128 KiB | 7543 |
| provider JSON files | 24 | 15 |
| provider bytes | 512 KiB | 21407 |
| manifest | 64 KiB | 27044 |
| README | 64 KiB | see bytes |
| PROVENANCE | 64 KiB | see bytes |
| root total | 1 MiB | far below |

Each `.nfo` is under 16 KiB. Each provider JSON is under 64 KiB.

## Reading the manifest

- `fixture.withholding` - author-stage withholding; the later adjudicator artifact is expected.json
- `hashing_rule` - hashes cover XML and provider bytes only
- `route_semantics` - what each route class means
- `cases` - evidence, location context, route inputs, notes per case
- `cases[].media_path` - the repository-native, library-relative media
  path the harness seeds for that case. Every case declares one; the
  NFO-08 phases repeat the same Secondhand Orbit path.
- `budgets` - allowed limits and measured actuals

## License

All fixture content is original synthetic material authored for this
corpus and is licensed GPL-3.0-only.
