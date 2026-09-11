# Naming corpus

Four set roots live here. Each root holds cases for one purpose, and a case
belongs to exactly one root.

| Root | Purpose | State |
|------|---------|-------|
| `regression/` | Cases promoted from inline tests, to keep a fixed regression set | reserved (empty) |
| `development/` | Pinned upstream parser evidence used while developing the parser | populated |
| `heldout/` | Cases held out of tuning so a later slice can measure without fitting | reserved (empty) |
| `stress/` | Generated adversarial and size-boundary cases | populated (334 generated cases) |

`regression/` and `heldout/` carry no cases yet. Do not fabricate held-out
cases. The `stress/` set is populated by the deterministic harness in
`server/crates/core/tests/filename_stress.rs`. Promotion into `regression/`
happens only when a case is already covered by an inline test; a case is never
copied into two roots.

## What this set can establish

The `development/` set records what another parser asserts for a filename and
maps the parts of that assertion Nightjar can express. It can show that a parser
change fixes a named case, and that a change regresses a case it used to fix.

## What this set cannot establish

The upstream fixtures assert Sonarr's and Radarr's schemas. Agreement with them
is not ground truth. This set cannot measure match quality: it holds no expected
show or movie identity, no provider response, and no library context. It cannot
report an accuracy percentage, and no accuracy percentage is claimed here.

The current regression set is the inline `#[test]` cases in
`server/crates/core/src/filename.rs`. This tree does not replace them.

Before any later R3 slice tunes the parser or reports parse quality, that slice
must independently author the held-out set. This development set must not be
used to report parse quality on its own.

## Development counts

844 cases from 12 sources: 734 applicable and 110 excluded. An applicable case
asserts at least one field Nightjar produces. An excluded case asserts only
fields Nightjar does not produce, or asserts a normalisation Nightjar does not
share; it is recorded as an explicit exclusion, never as a failure.

By source:

| Source | Cases | Applicable | Excluded |
|--------|------:|-----------:|---------:|
| `radarr-EditionParserFixture.cs` | 49 | 0 | 49 |
| `radarr-ParserFixture.cs` | 98 | 70 | 28 |
| `sonarr-AbsoluteEpisodeNumberParserFixture.cs` | 162 | 162 | 0 |
| `sonarr-CrapParserFixture.cs` | 28 | 28 | 0 |
| `sonarr-DailyEpisodeParserFixture.cs` | 53 | 53 | 0 |
| `sonarr-MiniSeriesEpisodeParserFixture.cs` | 11 | 11 | 0 |
| `sonarr-MultiEpisodeParserFixture.cs` | 75 | 75 | 0 |
| `sonarr-ParserFixture.cs` | 46 | 13 | 33 |
| `sonarr-PathParserFixture.cs` | 27 | 27 | 0 |
| `sonarr-SeasonParserFixture.cs` | 56 | 56 | 0 |
| `sonarr-SingleEpisodeParserFixture.cs` | 183 | 183 | 0 |
| `sonarr-UnicodeReleaseParserFixture.cs` | 56 | 56 | 0 |

By naming category:

| Category | Cases |
|----------|------:|
| absolute numbering (anime) | 126 |
| not applicable (their schema) | 106 |
| multi-episode | 102 |
| non-English / dual title | 97 |
| SxxEyy | 88 |
| other | 73 |
| scene-style movie | 53 |
| season pack | 52 |
| must-not-parse (junk) | 44 |
| date-based | 43 |
| season-folder context (path) | 33 |
| specials / S00 | 11 |
| season extras | 5 |
| mini-series (no season number) | 4 |
| NxNN | 3 |
| site prefix / junk | 2 |
| adversarial numeric/year title | 2 |

## Regenerating

Run from `server/`:

```
python3 tools/naming_corpus/extract.py
python3 tools/naming_corpus/check.py
python3 tools/naming_corpus/check.py --self-test
```

The extractor makes no network call and writes `development/corpus.json`
deterministically. `check.py` validates the corpus against `SOURCES.json`.
