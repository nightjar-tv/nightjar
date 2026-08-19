# 19 — two amendments for the loop brief

Both were found by running the pair the brief asks for. Neither is a change of
policy; both make an existing instruction say what it meant.

The brief exists as a prompt rather than a file, so these are written here for
whoever holds it. Amendment 2 also belongs in
`nightjar-meta/notes/replay-harness-2026-08-18.md`, which is the document that
led me the wrong way — that repo was outside this loop's worktree limit, so it
is flagged rather than edited.

---

## Amendment 1 — the dogfood regression check is a fresh control drain, never stored status

**Current wording.** *"Dogfood: a strict replay control pair … zero
currently-correct bindings break. Not 'few'. Zero."*

**What went wrong.** When the replay harness looked unavailable, I substituted
a parse-level instrument: reproduce the search input `drain_pending` builds,
diff it across the change, and count how many affected items are "bound today".
**"Bound today" was read from `metadata_status` in the live
`nightjar-data-v9/nightjar.db`.**

That is not the same population. The live status records **what past drains
did**; a fresh control drain does the work again from the capture and binds
things the live database has as `unmatched`.

**Measured.** Both `Top Gear - The Perfect Road Trip` files are `unmatched` in
the live database. In a fresh control drain, `- 2 - 2014` is **`ready`**, bound
to `tmdb:movie:301235`. Every iteration in this loop reported "0 items bound
today at risk"; that claim was true of the live database and weaker than its
wording, and only the pair could show the gap.

**The amendment.**

> "Bound today" means bound by a **fresh control drain**, never the stored
> `metadata_status` in the live database. The live status is a record of past
> drains, not a prediction of the next one.
>
> When the pair cannot be run, a parse-level substitute may **bound which items
> move**. It cannot say which of those a drain would have bound. Say so in those
> words; do not write "N bound items at risk" from stored status.

---

## Amendment 2 — warm the new keys, and do not trust the miss lines to count them

**What went wrong.** A change that makes a folder assert a new season or
episode sends the drain to fetch something it has never needed. Nothing is
recorded for it, strict mode refuses, and the run reports errors — which reads
like a regression and is not one. Here, iteration 02's season-9 assertion cost
one `/tv/326/season/9` fetch and stalled 30 Red Dwarf items at `matched`. With
the key warmed, all 30 return to `ready` and the arms are identical.

**And the miss lines undercount.** Strict aborts a path at its **first** miss,
so it reports the misses it *reaches*, not the misses a completed run needs.

**Measured.** The strict run printed **2** distinct missing keys. Warming cost
**3** requests and added **3** entries — 8,182 to 8,185 — because fetching the
first let the drain reach a third behind it.

**The amendment.**

> A parse change that alters season or episode assertions **will** miss the
> cache. Plan for it rather than discovering it:
>
> 1. run the pair strict, and read the `cache miss in strict mode` lines;
> 2. run the **treatment arm alone with strict off** to warm the new path;
> 3. re-run the pair strict — now `requests=0` on both arms means something.
>
> **Count the cache directory before and after.** The miss lines are a lower
> bound, not a cost.

---

## What does not need amending

The brief's own traps caught almost everything else in this loop, including the
ones I walked into: *absence is not evidence*, *a fix that removes a route is
not a fix for the mechanism*, *use the shipped predicates*, *measure the
population before pass conditions*.

The one addition those deserve is a sentence, not a rule: **the instrument is
often your own shell command.** Four false negatives in this session were a
missing `timeout` binary, a glob that would not expand, a repo-tree search
standing in for a machine search, and a low-level method read standing in for a
path. A probe's scope is a claim about the probe, not about the world.
