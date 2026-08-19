# Parser corpus at HEAD — 2026-08-19

Re-ran the reduced Sonarr/Radarr corpus against `main` at `7bd799c`. The saved
results in the spike directory were from 2026-08-13, before #145, #146 and #147.

- Corpus: `~/nightjar-spikes/parser-corpus-2026-08-13/`, 844 cases, 738 applicable.
- Harness: the spike's own `corpus_run.rs`, unchanged, built against HEAD.
- Fresh output: `out/results-2026-08-19-head.json` in that directory.

**245 of 738 pass — 33.2%.** The 33% figure in circulation is current, not stale.
The 2026-08-13 baseline was 186. The three parser commits since moved 59 cases.

## Title is the failure axis, and it is not in the per-form ordering

Failures counted by which field is wrong:

| field wrong | fails |
|---|---:|
| title | 446 |
| season | 166 |
| episode | 103 |
| episodes | 34 |
| year | 20 |

**285 of the 493 failures are title-only** — season, episode and year already
correct, title wrong. If nothing but title extraction improved, the corpus goes
from 33.2% to 71.8%. Title fails span 11 of the 16 forms, so ordering the work
by form splits one mechanism across every row of the table.

## What the title runs past

Classifying the 285 title-only fails by the text the title wrongly absorbed:

| class | cases | example |
|---|---:|---|
| `- NN` episode after the title | 97 | `... - 12 [Baha]` → title keeps `- 12` |
| release word not in the junk vocabulary | 81 | `German`, `EXTENDED`, `TRUEFRENCH`, `Imax`, `v2` |
| leading junk (group tag, site prefix, CJK dual title) | 56 | `[Jumonji-Giri]_[F-B]_...`, `www.Torrenting.com - ` |
| episode token | 29 | `Ep04`, `E1135`, `1x03` |
| bracket group absorbed | 20 | `[05]`, `(01-25)`, `(BDRip 1920x1080 ...)` |
| season token, any language | 2 | `Stagione 3`, `Temporada 1`, `Se 3 afl` |

The bracket class shows a specific defect: the title stops **inside** the
bracket, at the first junk token, not at the bracket. `[BD 1080p FLAC]` yields
`... [BD`. A bracket group is atomic — if it contains a junk token the title
ends at the opening bracket.

The first, fourth and fifth classes are structural — position and punctuation,
no new vocabulary, so no new way to eat a real title. The release-word class is
vocabulary and carries the `strip_junk`/`Atmosphere` risk; it needs its own
guard and should not ride along with the structural work.

## Absolute numbering is measurable after all

`METADATA_REMAINING` records absolute numbering as unmeasurable — 126 cases whose
expectations were reduced away, so a correct fix scores zero. That is true of the
absolute *number* and false of the case.

All 126 carry a `title` expectation. **117 of them fail on title alone.** Six
pass. The absolute number is unscorable; the title for the same 126 files is
fully scorable and is where every one of the failures is. The largest bucket in
the corpus is measurable today, on the axis that already dominates.

## Smaller corrections

- `parse_filename` is called on a basename at **two** sites, `scanner/src/lib.rs:233`
  and `:688`. A signature change touches both.
- `Specials` appears 15 times, not 3, and code does read path components:
  `db/src/paths.rs:116` has `Season N/` and `Specials/` inherit the show folder.
  Season 0 is then **deliberately excluded** at `metadata/src/tmdb/mod.rs:59` and
  `queue.rs:798`. The problem is a decision to reverse, not a folder nobody reads.
- `take(10)` confirmed at `metadata/src/tmdb/mod.rs:223`.
- The absent-as-`0` extraction artefact is fixed for the Absolute fixtures only
  (`extract.py:144`). The residue is 5 cases where the corpus stores an
  already-normalised title (`seriestitle`) and the harness compares it against a
  spaced one. Worth 0.7 points. It does not change any conclusion here.

## Three named defects behind the bracket class

Found while writing the step 0 plan, after the table above.

1. **The year branch never runs the terminator.** `parse_filename:147` returns
   `clean_title(&stem[..i])` at the year cut. Only the fallback arm calls
   `cut_at_title_junk`. Any name where a year-like token is found first skips
   the junk cut entirely.
2. **`find_bare_year` (`filename.rs:415`) reads a resolution as a year.** The
   boundary check is digit-only, so in `1920x1080` the `x` satisfies it and
   `1920` is a year. Seven corpus cases parse a resolution width as the year,
   and through defect 1 the title is then cut at the resolution token.
3. **The terminator stops inside a bracket.** `[BD 1080p FLAC]` cuts at `1080p`
   and leaves `… [BD`. A bracket group is atomic.

## The structural half is 148, not 146

Adding the season-token class (`Season 01-07`), which is punctuation and
position like the other three: 97 + 29 + 20 + 2 = **148**. Predicted corpus
after step 0: **393/738, 53.3%**.

Plan: `nightjar-meta/docs/plans/2026-08-19-the-title-ends-at-the-first-structural-terminator.md`.
