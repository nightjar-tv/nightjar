# Item 4 — folder context: the scope, and why it is not one slice

The board says to scope this and report rather than start it inside an
iteration. It is right to, and the reason is bigger than the board states.

## The board undercounts the call sites

> `parse_filename` takes a basename by design, with **exactly two production
> call sites**.

There are **three**:

| site | what it is |
|---|---|
| `scanner/src/lib.rs:388` | the full-library walk |
| `scanner/src/lib.rs:843` | the notify/hint ingest path |
| `metadata/src/queue.rs:1696` | `EpisodeSlot::season_episodes` |

The third is production code, not a test — nothing above it is `#[cfg(test)]`.
It re-parses the basename to expand `NxMM-NN` ranges "so we do not need an
`episode_end` column", and it is inside the matcher rather than the scanner.

**That third site is not a footnote.** It is the one that made iteration 2
reach further than the parser: widening the episode-span separator changes what
`season_episodes` returns for a file, which changes which slot a file occupies.
A scoping note that inherited the board's "two" would have missed it.

## The real cost is the instruments, and they all break at once

Every harness on this project calls `parse_filename` directly:

| harness | instrument |
|---|---|
| `corpus_run.rs` | the parser corpus |
| `sweep.rs` | the parser sweep |
| `replay.rs` | the matcher oracle |
| `oracle_query.rs` | the oracle's shipped-predicate probe |
| `kindprobe.rs` | the `stored_kind` probe |

Plus **161 occurrences in `core`'s own tests** and 7 in the scanner's.

Change the signature and all five instruments stop compiling in the same
commit. That is survivable — a compile error is the *good* failure, and this
loop has already had one do its job. What is not survivable is the state after:
until every harness is updated, **there is no instrument to judge the change
by**, and the change is precisely the kind that needs one.

## So it is at least three slices, and they have an order

1. **Additive API.** Keep `parse_filename(basename)` exactly as it is and add a
   context-taking entry beside it. Nothing moves; nothing to measure; every
   instrument keeps working. This slice can land and be reviewed on its own.
2. **Move the three production call sites** to the new entry, one at a time,
   with the oracle run between each. `queue.rs:1696` is the one to do last and
   most carefully — it is the site inside the matcher.
3. **Teach the instruments to see it.** The corpus harness discards what it
   already has: `corpus_run.rs` calls `basename(input)` and throws the folder
   away, while **23 corpus cases carry a full path in `input`** and are marked
   `season-folder context (path)`. Those 23 are unearnable today *because of the
   harness*, not because of the parser. Passing the whole path scores them with
   no new corpus.

## What it would be worth

| population | rows | counted in |
|---|---:|---|
| `tv.handmade` — `01 - Closure.mkv`, needs folder title **and** the `NN - ` number | **5,840** | oracle, this loop's base |
| `tv.episodetitle` — `Episode 1.mkv`, needs the folder title | **5,844** | oracle, this loop's base |
| corpus cases whose `input` is a path | **23** | parser corpus |

Both oracle shapes read **0.0% correct** and 5,840 / 5,844 `absent` on the
corrected baseline. They are the two largest single blocks of failure the
instrument has, and neither can move without the folder. That is the case for
doing this — and the case for doing it in slices rather than in an iteration.

## The trap sitting inside it

`stored_title` already substitutes the show folder's name for an empty parse,
and `stored_kind` already reads the folder for a numbered season directory.
**Some folder context is therefore applied twice** once the parser also has it,
and the scanner's version wins because it runs after. Any slice here has to say
which layer owns which decision before it moves a call site, or the two will
disagree quietly — and `stored_title`'s history is that exactly this kind of
disagreement scored 5,644 rows `absent` for a reason that was the harness.

**Not started.** Reported, as asked.
