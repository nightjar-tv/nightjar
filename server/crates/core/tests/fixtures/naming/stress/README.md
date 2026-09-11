# stress/

Generated adversarial and size-boundary cases for the `nightjar-core` filename
parser. This is a bounded robustness and convention check, not an accuracy
benchmark.

Ownership: the core crate. The cases are generated in memory, never copied from
`development/`. The generated bulk output is not committed; the run rebuilds it
from a fixed seed.

## Reproducing

Run from `server/`:

```
cargo test -p nightjar-core --test filename_stress
```

Add `-- --nocapture` to see the local observation line:

```
cargo test -p nightjar-core --test filename_stress -- --nocapture
```

The run reads no network, no private path, and no sealed held-out set.

## Generator

`tests/filename_stress.rs` holds the generator. `stress/manifest.json` records
the values the test cross-checks.

| Field | Value |
|-------|-------|
| generator version | 2 |
| seed | `0x6e696768746a6172` |
| generated cases | 334 |
| maximum input bytes (this run) | 4096 |
| local case budget (this run) | 1024 |
| local byte budget (this run) | 1048576 (1 MiB) |
| generated input bytes (this run) | 16312 |

The budgets and byte totals above are local to this run and this checkout. They
are chosen ceilings for the generated population, not parser limits or defaults
for another library; a real basename can exceed the input bound.

The fingerprint `0x7d9347a33fa80b78` (FNV-1a 64) covers the generated inputs. It
binds the population to the seed and the family counts. A changed seed or count
fails the test until the manifest is regenerated.

## Families

Each row is one family. Variations of one template stay in one family and are
not independent evidence.

| Family | Cases | Convention asserted |
|--------|------:|---------------------|
| `numeric_title_year` | 30 | Numeric title, parenthesised year, movie, no episode |
| `scene_film` | 30 | Dotted or parenthesised film title, year, movie, no episode |
| `unicode_title` | 24 | Non-ASCII title kept intact, year, movie |
| `missing_year` | 24 | Title and movie kind with no year, no invented year |
| `sxxeyy_nxnn` | 40 | `SxxEyy` and `NxNN`: season, episode, not absolute |
| `season_folder` | 24 | Folder title fills an empty title; a basename season wins; an absolute episode takes no folder season |
| `seasonless_date_absolute` | 36 | `Eyy`, `Epyy` and `yyyymmdd` markers: absolute episode, no season; a leading date alone is a year with an empty title |
| `multi_episode` | 32 | `SxxEyy-Ezz` and `NxNN-zz` ranges: start and inclusive end |
| `season_zero` | 20 | `S00`: season zero specials keep their episode |
| `extras_editions` | 28 | Edition, `Proper`/`Repack` and extras (`Extras`, `Deleted.Scenes`) tokens: title, year and episode unchanged; a season-extra keeps its season and invents no episode |
| `misleading_tokens` | 30 | `720p`, `1080p`, `x264`, `H.264` and `S01E00` invent no episode or year |
| `malformed_size` | 16 | Empty, punctuation-only, malformed-separator and 4096-byte names do not panic; the input stays bounded |

## What this set can establish

It shows the parser keeps the listed conventions over a generated population,
and that it stays bounded on adverse inputs. It is a regression net: a parser
change that breaks a convention fails a named family.

## What this set cannot establish

The names are invented, so this is not household or external-library evidence,
and it is not a match-quality measure. It holds no expected show or movie
identity, no provider response and no library context. It cannot report an
accuracy percentage, and no accuracy percentage is claimed here. The elapsed
time printed by a local run is one machine's observation, not a performance
claim; the portable controls are the fixed case and byte caps.

Before any later R3 slice tunes the parser or reports parse quality, that slice
must use the held-out set. This generated set must not be used to report parse
quality on its own.
