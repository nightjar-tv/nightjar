# The parser loop — summary

Fifteen iterations on `nightjar-core`'s `parse_filename`, measured against the
reduced Sonarr/Radarr corpus. Fourteen kept, one reverted.

Branch `loop/parser-corpus` in the worktree `~/Documents/GitHub/nightjar-wt-loop`.
**Nothing is pushed.** The branch does not exist on the remote.

**Correction 2026-08-19:** the "no replay harness on this machine" conclusion in
the first version of this note was wrong. See the corrected section below.

## The number

| | before | after |
|---|---:|---:|
| **all-fields** | **245/738 — 33.2%** | **499/738 — 67.6%** |
| **structure-only** | **530/738 — 71.8%** | **612/738 — 82.9%** |

Baseline measured at `a04e7c2` on a tree without any change, reproducing the
33.2% already in circulation. Every iteration measured a fresh before and after.
**Zero corpus cases regressed at any point** — `newly failing 0` on every kept
iteration, verified positionally after the comparator defect found at
iteration 03.

## Per iteration

| # | change | all-fields | structure |
|---|---|---:|---:|
| — | baseline `a04e7c2` | 245 (33.2%) | 530 (71.8%) |
| 01 | the title ends at a spaced dash-number | 337 **(+92)** | 530 |
| 02 | a season token with no episode is a season | 390 (+53) | 576 |
| 03 | a bracket group is atomic | 402 (+12) | 576 |
| 04 | a bare episode marker ends the title | 413 (+11) | 576 |
| 05 | only a real extension is stripped | 423 (+10) | 585 |
| 06 | the episode marker may be spelled out | 430 (+7) | 585 |
| 07 | **the year boundary — reverted** | 430 (+0) | 585 |
| 08 | a four-digit season, marked spelling only | 435 (+5) | 591 |
| 09 | the terminator runs on every branch | 444 (+9) | 591 |
| 10 | a bracket run selects its title group | 463 **(+19)** | 591 |
| 11 | a leading site prefix is not the title | 468 (+5) | 591 |
| 12 | a dash is not the only separator | 472 (+4) | 596 |
| 13 | the season and episode may be separated | 488 **(+16)** | 612 |
| 14 | a title ends at a bracket it never opened | 494 (+6) | 612 |
| 15 | the episode marker may follow the number | 499 (+5) | 612 |

## Dogfood

**Zero bindings broken, and zero items bound today are even at risk.**

Across all fifteen iterations, exactly **three** of the 25,043 dogfood items
changed parse at all:

- two Top Gear specials (iteration 01), `movie` / `unmatched`, title only;
- one Red Dwarf season-9 special (iteration 02), `movie` / `unmatched`, which
  gains a correct season 9 and becomes an episode.

Iterations 03 to 15 changed **nothing** in the library — not one item, not one
group key, not one search input.

The Red Dwarf item is the only one with a blast radius: its kind change moves
it into the `Red Dwarf (1988)` show folder, so 67 bound items see that folder's
counts move. Every predicate their binding rests on was traced and holds; see
notes/loop/02. **That was read, not run** — see the limits below.

## What could not be measured — CORRECTED 2026-08-19

**The first version of this note said the replay harness is not on this machine.
That was wrong, and it was wrong in a specific way worth recording: I searched
the repository tree and its git history, and concluded from that about the
machine.**

The timeline of an earlier session on this project contains the same mistake —
it records `out/cases.json` as "not on disk in either checkout or on the N150",
and that file has been sitting in `~/nightjar-spikes/parser-corpus-2026-08-13/`
the whole time. It is the corpus this loop was measured against. So this failure
mode has now happened at least twice here.

### What is actually on the Mac

- **`replay.rs`** — four identical copies (sha `e93f0a7a…`) under
  `~/.claude/jobs/13c074c6/tmp/{conf,conf2src,wgsrc,cache}/server/crates/api/src/bin/`.
- **`~/.claude/jobs/13c074c6/tmp/conf/`** — a complete tree with the harness
  already applied: `measure_cache_key`, `measure_cache_write`,
  `NIGHTJAR_TMDB_CACHE` and `NIGHTJAR_TMDB_CACHE_STRICT` all in
  `crates/metadata/src/tmdb/mod.rs`.
- **`apply_harness.py`** — lifts those spans into any other tree.
- **`capture_media.py`**, **`capture_nfo.py`**, and `replay_add_reparse.py` in
  `nightjar-meta/notes/scripts/`.

### The three captures

| capture | on the Mac? | note |
|---|---|---|
| media (JSONL) | **regenerable, verified** | ran `capture_media.py` against a read-only copy of the dogfood db: **25,043 items, 9.8 MB, in seconds** |
| NFO | **regenerable** | `/Volumes/media` is mounted over SMB and the NFOs are all there — **28,132** of them, counted properly (see the correction below). The script hardcodes `/mnt/media/...`, a Linux path, which is direct evidence it was written on the N150; it needs `/Volumes/media/{Movies,TV Shows}` |
| TMDB response cache | **not here** | searched at unbounded depth with a verified-working tool: the only flat JSON cache directory over 1,000 entries anywhere under `~` is the 2,975-entry **TVDB** spike cache. There is no 7,967-entry TMDB cache on this machine |

### So the blocker is narrower than stated

**Only the response cache is missing, and it is the one piece that cannot be
rebuilt offline.** Warming it needs live provider calls — forbidden in this
loop, and a decision for a human. Three ways forward, in cost order: copy the
cache from the N150 if it is there; warm it on the Mac with one live run (~10,400
requests cold, per `nightjar-meta/notes/replay-harness-2026-08-18.md`); or run
the measurement on the N150, where the harness was built.

Everything else — writing the parser, scanner and queue changes, and unit-testing
them — can be done here today.

### A second false negative, in this very audit

While checking for the NFO capture I ran `timeout 540 find /Volumes/media/Movies
-iname "*.nfo" | wc -l` and reported **0 NFOs**. **`timeout` is not a command on
macOS.** The pipeline never ran, `wc -l` counted an empty stream, and I read the
zero as absence — three times, including once after writing "if this is 0 again
the walk still did not finish". A glob contradicted it: `ls Movies/*/movie.nfo`
returns 1,777.

A zero can mean "nothing is there" or "the code never ran", and they are the same
number. Check which before believing either — including when the instrument is
your own shell command.

### The cache was copied from the N150 — the harness is now complete on the Mac

Done 2026-08-19, **zero provider requests**: a file copy, not a warm run.

The N150 is `nightjar@192.168.1.183`, reachable from the Mac over Tailscale as
`nightjar-dev.lonk-halfbeak.ts.net`. Its layout is the one
`nightjar-meta/scripts/rc9_deploy_dogfood.sh` documents: everything lives in
`~/gate2/`.

Copied to `~/nightjar-wt-loop-scratch/replay/`:

| artefact | size | check |
|---|---:|---|
| `tmdb-cache/` | 383 MB, **8,182 entries** | file-list md5 `c5515e99…` matches the N150 exactly; `__404__` sentinels survived |
| `capture-media.jsonl` | 9.7 MB | 25,004 items |
| `capture-media-v2.jsonl` | 10 MB | same plus a `year` column |
| `capture-nfo.jsonl` | 145 MB | see the gap below |

**The cache is complete for a full-library run.** The N150's logs hold several
strict runs against it:

    DONE groups=4958 ready=24923 unmatched=81 pending=0 errors=0 requests=0 secs=55.9
    DONE groups=4977 ready=24942 unmatched=62 pending=0 errors=0 requests=0 secs=54.7

`requests=0` on the full 25,004-item library in under a minute. That is the
instrument working offline, and it is the cache that was copied.

Nothing on the N150 was modified — the session read and streamed a tar to
stdout, nothing else.

### The NFO figures I first published were wrong — corrected

I reported "1,784 movie NFOs, 847 show NFOs". Those were **depth-one globs**
presented as if they characterised the library. The movie figure was roughly
right by accident, because a movie sits one folder deep. The TV figure counted
only show-level `tvshow.nfo` files and missed every episode NFO — a 30×
undercount.

Counted properly, with `find` and no `timeout` prefix:

| root | Mac (SMB) | N150 (local) |
|---|---:|---:|
| Movies | 1,783 | 1,783 |
| TV Shows | 26,349 | 26,349 |
| **total** | **28,132** | **28,132** |

That matches the 28,115 in `nightjar-meta/notes/replay-harness-2026-08-18.md`
to within the files added since the capture. **Every count is identical on both
hosts** — independent confirmation that the Mac's SMB mount and the N150's
`/mnt/media` are the same library, which is what makes the rewritten capture a
faithful one rather than a plausible one.

### Four false negatives in one session, all the same shape

Worth recording as a pattern rather than four separate corrections. Each was a
bounded or broken probe reported as a property of the world:

1. **"The replay harness is not on this machine."** I searched the repository
   tree and its git history, then concluded about the machine. It was in
   `~/.claude/jobs/13c074c6/tmp/`.
2. **"0 NFOs."** `timeout 540 find ... | wc -l`, three times. **`timeout` is not
   a command on macOS**; the pipeline never ran and `wc -l` counted an empty
   stream. `find` works fine on the SMB mount without it — 82 NFOs under Red
   Dwarf alone.
3. **"`TmdbClient::search` has no empty-title check."** I read the low-level
   method and concluded about the path. The guard is one level up at
   `MetadataSource::resolve`, and the drain has never been able to issue an
   empty query. Same shape as (1), one layer down instead of one machine over.
4. **"TV depth2: 0."** A multi-level glob that failed to expand across ~847
   shows. A single-directory glob on the same path returns 7.

The brief's rule covers all four: *a zero can mean "nothing is there" or "the
code never ran", and they are the same number.* The addition this session
supplies is that **the instrument is often your own shell command**, and that a
probe's scope is a claim about the probe, not about the world.

### Two gaps to close before a strict pair runs on the Mac

**1. The capture carries the N150's paths.** Every row says
`library_path: /mnt/media/Movies`. The Mac mounts the same media over SMB at
`/Volumes/media/Movies`. The drain reads NFOs from the real path at resolve
time, so on the Mac every NFO route would miss silently and the run would
measure a different library while reporting success — the exact failure
`nightjar-meta/notes/replay-harness-2026-08-18.md` warns about. Either rewrite
`library_path` in the capture, or set `NIGHTJAR_MEASURE_NO_NFO=1` on **both**
arms.

**2. `capture-nfo.jsonl` exists and nothing consumes it.** All ten `replay.rs`
copies on the N150 were checked: none mentions NFOs at all. On the N150 that
never mattered, because the media is local and the real paths resolve. So the
"three parts" in the harness note are really two parts plus a capture that was
made and never wired in. Wiring it in is the clean fix for any host without the
media at the captured path.

### Recipe on the Mac

    J=~/.claude/jobs/13c074c6/tmp          # replay.rs, apply_harness.py, reference tree
    R=~/nightjar-wt-loop-scratch/replay    # cache + captures, copied from the N150

    S=~/Documents/GitHub/nightjar-meta/notes/scripts

    git archive <commit> server | tar x -C /tmp/tree
    python3 $J/apply_harness.py $J/conf/server /tmp/tree/server   # TMDB cache + strict + NFO switch
    mkdir -p /tmp/tree/server/crates/api/src/bin                  # the repo has no such directory
    cp $J/conf/server/crates/api/src/bin/replay.rs /tmp/tree/server/crates/api/src/bin/
    python3 $S/replay_add_year.py    /tmp/tree                    # MUST come first
    python3 $S/replay_add_reparse.py /tmp/tree
    cargo build --release --bin replay -p nightjar-api

**`replay_add_year.py` is not optional and must run first.** The base
`replay.rs` inserts no `year` column at all, and `replay_add_reparse.py` patches
a span that only exists after the year patch — run alone it fails with
"expected one match". The first version of this recipe omitted it.

    NIGHTJAR_DATA_DIR=/tmp/data-ctl \
    NIGHTJAR_CAPTURE=$R/capture-media-v2.jsonl \
    NIGHTJAR_TMDB_CACHE=$R/tmdb-cache \
    NIGHTJAR_TMDB_CACHE_STRICT=1 \
    NIGHTJAR_REPARSE=1 \
    ./target/release/replay

**`NIGHTJAR_REPARSE=1` is not optional for a parser change.** Without it the
replay inserts the *captured* parse and the change under test never runs. Both
arms must set it, because re-parsing with today's parser does not reproduce the
scanner that made the capture — the baseline moves, and that is only sound if
both arms move together.

### Warm the new keys before a pair means anything

**A change that makes a folder assert a new season or episode will miss the
cache, and strict mode will refuse it.** That is not a defect in the change and
not a defect in the harness — it is the harness doing its job. The drain goes
to fetch something it has never needed before, and there is nothing recorded
for it.

It is predictable, so it should be planned for rather than discovered:

1. Run the pair strict. Read the `cache miss in strict mode` lines.
2. Run the **treatment arm alone with strict off** to warm the new path.
   Record the cache entry count before and after, so the cost is measured
   rather than claimed.
3. Re-run the pair strict. Now `requests=0` on both arms means something.

**Strict mode under-reports the warm cost.** It aborts a path at its first
miss, so it can only report the misses it *reaches*. Measured here: the strict
run showed **2** distinct missing keys; warming actually cost **3** requests and
added **3** entries (8,182 -> 8,185), because fetching the first key let the
drain reach a second one behind it. Budget for a cascade, and count the cache
directory rather than trusting the miss lines.

### Where to measure

**The N150 is still the cheapest place**: its paths already match the capture,
its strict runs take ~55 seconds, and the media is local so the NFO route is
real without any rewrite.

The Mac is now viable for **writing and unit-testing** the three-part slice, and
for a strict pair once gap 1 is closed. It was never blocked on the harness —
only on one directory, and that directory is here now.

### What the corpus still cannot say

The corpus is **parse-level**. No provider, no search, no candidates. It says
nothing about scoring or binding, and a green corpus is not a working pipeline.

The substitute built for this loop and used every iteration — `parse_all` and
`group_keys`, reproducing the search input `drain_pending` builds from the
shipped predicates — **bounds** which items could change outcome. It does not run
the matcher. It is weaker than a control pair and must not be read as one. That
part of the original note stands.

## The ceiling, measured

239 failures remain. Counted from the case file, not estimated:

| cause | cases | can a parser change earn it? |
|---|---:|---|
| addressable in the parser | 174 | yes |
| wants an empty title | 29 | the parser can; **the caller cannot** |
| the season is in a path component | 22 | no — the runner passes the basename, by design |
| the corpus stores a squashed title | 9 | no — a harness artefact |
| the corpus stores absent as `0` | 5 | no — `None` never equals `Some(0)` |

**The ceiling for a parse-level, parser-only change is 673/738 = 91.2%.**

## What I would do next, and why in this order

**1. The empty title — 29 cases, and it is a three-part slice, not a one-liner.**
The largest single class left. `S03E09 WS PDTV XviD FUtV` has no series title in
the filename; the folder has it. The parser change is one line and it is
**blocked on the caller**, verified by reading it: `scanner/src/lib.rs:233` and
`:688` store `parsed.title` verbatim with no folder fallback, and
`TmdbClient::search` builds `("query", title)` with no empty check — the only
empty-title guard in the crate is inside a test double. Doing the parser half
alone trades 29 corpus cases for an empty provider query. The slice is parser +
scanner folder fallback + queue guard, and it needs the replay pair.

**2. The four-digit season in the bare spelling, then the year boundary — 13
year corrections that currently cost two titles.** Iteration 07 is reverted and
notes/loop/07 has the argument: the bogus year from `1920x1080` is acting as an
accidental title terminator, and two titles go from right to wrong when it is
removed. Both are the bare `2016x231` form. Give that form a real terminator
first and the year fix costs nothing. It needs a discriminator between
`2016x231` and `1920x804` that a filename may not carry — start there.

**3. Season-number normalisation in the soft key — a handful of anime cases.**
Several want `Anime-Series Title S2` and get `Anime-Series Title S02`. That is
`clean_show_title` in `nightjar-metadata`, not the parser, and it is
matcher-visible, so it wants the replay pair.

**Not worth doing, and each has the measurement that says so:**

- **A trailing standalone number.** 8 corpus gains against 420 broken bound
  library titles at its loosest guard and 4 at its tightest — `Apollo 13`
  becomes `Apollo`. notes/loop/06.
- **A glued dash-number.** 213 bound items broken; `Stargate SG-1` becomes
  `Stargate SG`. notes/loop/01.
- **A separator run between episode tokens.** One corpus case against four
  bound items; `9-1-1 - 2x02 - 7.1` becomes six episodes. notes/loop/13.
- **`german` in the junk vocabulary.** Seven corpus gains, and the corpus holds
  its own refutation: `The.Good.German.2006` is a real film. notes/loop/09.

## Method notes worth keeping

- **The population check paid for itself four times**, each time before a line
  of code was written. Every rejection above came from querying the library
  first.
- **A prediction that misses is a finding.** Iteration 03 predicted +18 and got
  +1: the rule fixed one route into the mechanism and seventeen cases came
  through two others. Extending it to all routes gave +12.
- **My own instrument had a defect.** `analyse.py` compared results through a
  dict keyed on the input string, and the corpus holds duplicate inputs, so it
  invented both a gain and a loss. Fixed to compare positionally; iterations 01
  and 02 were re-verified.
- **One shipped test changed**, at iteration 10, because the new behaviour is
  right rather than to make a change pass. The old expectation is recorded in
  the test's own comment. No test was weakened or deleted.

## One red test, unrelated

`nightjar-transcode`'s `hls::tests::mapped_real_library_end_moov_mp4_copy_keeps_aac`
fails intermittently with `video starts at 63.646s, land was 58.975s`. It
**passed and failed on the same tree in two consecutive runs** during the final
gate, and stashing the only source change at iteration 03 reproduced the failure
on the unchanged tree. It asserts a start time against a real media file. Not
touched, not weakened.

Everything else is green: 100 core, 256 metadata, 89, 65, 43, 15, 3 and 2 across
the other crates. Clippy clean at `-D warnings`. `cargo fmt --check` clean.
