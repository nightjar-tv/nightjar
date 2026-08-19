# 17 — the strict pair on the N150

Run 2026-08-19 on `nightjar-dev`. **Both arms `requests=0`** — genuinely
offline. Binaries verified distinct before running:

    4e6c0f45…  replay-ctl   (a04e7c2, the loop's baseline)
    9de07b5c…  replay-trt   (HEAD, the loop plus the absent-title slice)

Both arms: `NIGHTJAR_TMDB_CACHE_STRICT=1`, `NIGHTJAR_REPARSE=1`, the same
capture, the same cache, run sequentially. Separate target directories, so the
byte-identical trap could not apply.

## The numbers

|  | control | treatment |
|---|---:|---:|
| ready | 24,953 | 24,923 |
| matched | 0 | **30** |
| unmatched | 51 | 49 |
| pending | 0 | 2 |
| errors | 0 | **2** |
| requests | **0** | **0** |

## The two errors are two cache misses, and both are mine

Exactly **two distinct** keys missing, zero in the control:

    /search/movie   query=Top Gear The Perfect Road Trip
    /tv/326/season/9  append_to_response=…&language=en-US

The first is iteration 01: `Top Gear - The Perfect Road Trip - 1` became
`Top Gear - The Perfect Road Trip`, a title the cache was never warmed for. Two
items, `unmatched` in the control, `pending` here.

The second is iteration 02: the `S09 Back to Earth` special now asserts season
9, so the drain went to fetch a season **it had never fetched before**.

## What the season-9 miss did, exactly

The error stalled the Red Dwarf folder's episode enrichment. 30 items went
`ready` -> `matched`, across seasons 2, 3, 8, 9, 11 and 12.

**No binding moved.** Checked, not assumed:

- the folder's series row is `Red Dwarf (1988) -> 326` in **both** arms;
- `REBOUND` (ready in both, different key) is **0**;
- the 29 items that differ went from `tmdb:episode:1224257`-style links to
  `tmdb:show:326` — the right show, without the per-episode link that the
  errored season fetch would have written.

`matched` means bound and awaiting enrichment. Nothing bound to a wrong entity.

And the change did what iteration 02 predicted: the S09 special itself went
`unmatched` -> `matched`.

## Where my iteration-02 analysis was right, and where it was incomplete

Iteration 02 said "read, not run" and traced every predicate the *binding*
rests on: `candidate_covers_folder_seasons` still holds because TMDB 326
declares season 9, `slots_explained` is monotone, the count pin gets stronger,
and the folder has a stored series row so it does not re-search.

**All of that was correct** — the replay confirms it, with an identical series
row and zero rebinds.

**What it missed** is that asserting a new season triggers a new *season fetch*
for episode-level enrichment. That is a different mechanism from binding, and
reading the binding predicates could never have found it. This is exactly why
the note said "read, not run" and flagged it for the replay, and the replay
found it on the first run.

## What this run cannot decide

It cannot distinguish **"the change is fine and the cache is cold"** from
**"the change breaks episode linking"**.

The evidence favours the first, strongly:

- the error is literally `cache miss in strict mode`, and `requests=0` says the
  run never went online to find out;
- the folder binding is unchanged and correct;
- `nightjar-meta/notes/replay-harness-2026-08-18.md` documents this exact
  pattern — "raising confidence made folders match and so request seasons never
  fetched before. Strict mode refused instead of quietly going online. Warming
  the new path cost 72 requests."

But it is **not proven**, and the difference matters. Settling it costs exactly
**two requests** — warm those two keys and re-run the pair.

**No requests were made.** The loop forbids live provider calls and this run's
authorisation was to measure, not to warm. That is a decision for a human.

## Verdict

**Not a pass.** The slice is not verified end to end. What is established:

- the parser work moves nothing in the library that was bound (0 rebinds,
  identical folder binding);
- the two new queries it does create are both accounted for and both are
  consequences of changes that were measured and predicted;
- one of them stalls a folder's enrichment under a cold cache.

Artefacts left on the N150 under `~/gate2/loop/` — both trees, both binaries,
both logs, both databases, the run script and the comparison script.
