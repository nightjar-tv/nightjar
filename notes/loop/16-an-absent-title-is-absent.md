# 16 — an absent title is absent (the three-part slice)

**Kept.** Corpus 499 -> 523 of 738 all-fields (67.6% -> 70.9%). Structure-only
612, unchanged. Zero corpus cases regressed. **Zero dogfood items change** —
group keys and search inputs both unmoved.

The slice iteration 13 identified and declined to do half of.

## The three parts

**A — `nightjar-core`.** When the season/episode token starts the name there is
no series title in the filename at all — `S03E09 WS PDTV XviD FUtV`, `1x04`,
`01x04 - Halloween, Part 1`. The parser substituted the whole stem, producing a
"title" of pure release junk. It now returns `""`.

**B — `nightjar-scanner`.** `title_from_folder` borrows the folder's name when
the parsed title is empty. **The scanner is the layer that has the folder**;
`parse_filename` only ever sees a basename. An episode takes its show folder
via the same `show_folder_relpath` the queue groups by, so the two cannot
disagree, and `Season 1/` and `Specials/` walk up to it. A movie takes its
containing folder — which is where the year lives, and `clean_movie_title`
reads it downstream. Applied at **both** upsert sites, `lib.rs:233` and `:688`.

**C — `nightjar-metadata`.** An absent title is not a query.

## Part C was smaller than iteration 13 claimed, and that note was wrong

Iteration 13 said `TmdbClient::search` builds `("query", title)` with no empty
check and that "the only empty-title guard in the crate is inside a test
double". **That is wrong.** The guard is one level up, at the
`MetadataSource::resolve` entry:

```rust
let Some(title) = input.title.as_deref().filter(|t| !t.is_empty()) else {
    return Ok(ProviderResult::Miss);
};
```

The drain's own path has never been able to issue an empty query. I read the
low-level method and concluded about the path — the same bounded-search shape as
the harness conclusion, one layer down instead of one machine over.

Checking every caller of `.search(` found three. Two are guarded upstream. The
third, `fix.rs`, is a real hole: when the caller supplies no query it
substitutes the **stored** title, and that can be empty for a file sitting
directly in the library root, where the parser finds no title and the scanner
has no folder to borrow one from. That one route is now guarded, and the guard
is where Part C lives.

## Why the dogfood cannot justify any of this

**Zero of the 25,043 items get an empty title**, and zero titles changed. A
renamer-written library always puts the show name first, so the shape this slice
exists for does not occur in it.

That is the point. The library is one shape among thousands and the easiest one;
the corpus is where the other shapes live, and it says 24 cases. Parts B and C
are justified by the code path, not by the library — B because the parser cannot
see a folder, C because one route reached a provider with nothing to ask for.

## What is still not measured

The corpus is parse-level, so it scores Part A alone. **Parts B and C have unit
tests and no end-to-end measurement.** The strict replay pair is what would
measure them, and it should run on the N150: the capture's `/mnt/media` paths
match there, the media is local so the NFO route is real without a rewrite, and
a strict run is ~55 seconds.

A Mac-usable capture now exists anyway —
`~/nightjar-wt-loop-scratch/replay/capture-media-mac.jsonl`, with the library
roots repointed at `/Volumes/media` and **verified against the filesystem**: 40
sampled rows, every media file resolves. The rewriting script refuses to write
an output whose paths do not resolve, because the failure it exists to prevent
is silent.
