# 09 — the terminator runs on every branch, and knows edition words

**Kept.** Corpus 435 -> 444 of 738 all-fields (58.9% -> 60.2%). Structure-only
591, unchanged. Zero corpus cases regressed. **Zero dogfood items change.**

## Mechanism

Cutting the title at the year takes away what *follows* the year and nothing
else, so junk sitting **before** it survived:

    World.Movie.Z.EXTENDED.2013.German.DL.1080p.BluRay.AVC-XANOR
      year 2013, title `World Movie Z EXTENDED`

Only the fallback arms ever called `cut_at_title_junk`. This is defect 1 of the
three the corpus notes named on 2026-08-19.

## Why the two halves land together

They do not work apart, and each measurement says so:

- the shipped junk list on the year arm, alone: **+0**. The junk before a year
  in this corpus is all edition vocabulary and the list does not hold any.
- the edition vocabulary without the year arm: **+2**. Most of those names are
  movies with a year, so the list is never reached.
- together: **+9**.

`Valana la Movie TRUEFRENCH BluRay 720p 2016` needs both. The year cut leaves
`... TRUEFRENCH BluRay 720p`; the shipped list takes it to
`Valana la Movie TRUEFRENCH`; only `truefrench` finishes it.

So this is one mechanism — *the title ends at the first release token, on every
branch* — and neither half was committed alone.

## The vocabulary, measured token by token

| token | corpus + | dogfood cut | bound today | verdict |
|---|---:|---:|---:|---|
| extended | 7 | 0 | 0 | **in** |
| truefrench | 1 | 0 | 0 | **in** |
| imax | 1 | 0 | 0 | **in** |
| german | 7 | 0 | 0 | **out** |
| complete | 1 | 1 | **1** | out — `A Complete Unknown` -> `A` |
| uncut | 0 | 1 | **1** | out — `South Park Bigger Longer and Uncut` |
| unrated | 0 | 1 | **1** | out — `The Toxic Avenger Unrated` |
| vostfr, multi, dubbed, subbed, limited, internal, vf, vfq | 0 | 0 | 0 | out — nothing to gain |

**`german` is the one to read carefully.** It earns seven cases and the corpus
holds its own refutation: `The.Good.German.2006.720p.BluRay` is a real film.
Today the year arm protects it, because the cut happens at `2006` before the
list is ever consulted — and this change removes exactly that protection. So
the word cannot come in with the terminator. It is the `Atmosphere` trap
wearing a year as a fig leaf.

Every rejected word is now a test, with the title it would have destroyed.

## Prediction vs actual

Predicted **+9**, structure unchanged, 0 dogfood items.
Actual **+9**, structure unchanged, **0** dogfood items. Landed exactly.

Three passing corpus cases have their title string change and all three were
predicted to survive, because none carries a title expectation:
`Movie Name FRENCH BluRay 720p 2016 kjhlj` expects only `year: 2016`, and the
two Windows-path cases expect season and episode. `newly failing 0` confirms it.

## The intermittent transcode test

`mapped_real_library_end_moov_mp4_copy_keeps_aac` failed again. Same as
iterations 03, 05 and 06; passed at 04 and 08. Cleared by the iteration-03
stash test. Not touched.
