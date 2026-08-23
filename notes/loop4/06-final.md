# Loop 4 — final report

Branch `loop/board-residual` off `origin/main` at `e3208cc`. Nothing pushed,
nothing merged, `main` untouched.

## Amendment, 2026-08-23 — iteration 1 is no longer on this branch

**The resolver moved to `metadata/ipv6-interleave`.** A review read the branch
against `origin/main` and found the product diff was six files, not two, and
that the IPv4/IPv6 interleave was live at `tmdb/mod.rs:143`, wired into
`AgentBuilder::resolver`.

It was never hidden — it is board item 1 and this report lists it as kept. But
it changes the network path for **every user**, and this report's own iteration-1
entry records that **none of the four instruments can see it**: all four are
offline, and the resolver closure is only entered on a connect. A change no
instrument here can measure should not merge on a parser branch's evidence.

So the table below still reads three iterations, and this branch now carries
**two**. Iteration 1's note, `01-resolver.md`, went with the commit;
`00-phase0-oracle.md` stayed here, because it is instrument work rather than
resolver work.

`interleave_families` itself was checked and is sound — exhaustive over all
2,047 family arrangements of up to ten addresses, with the multiset, the
per-family order and the leading family all preserved. That is a reason to land
it cleanly, not a reason to land it here.

## Phase 0 — the oracle repo

### What was already done

**The brief's premise was false.** `~/nightjar-spikes/matcher-oracle-2026-08-19`
was clean. A session at 11:00–11:01 on 2026-08-23 had committed all five named
fixes — `preflight.sh`, the stale `WT` default, the strict-mode guard,
`movie.seasondir`, and `score_binding.py`'s unmanifested count — as `021da67`,
`afd6a56` and `e79133f`. Each was verified present in the tree rather than read
off a commit message. Nothing was re-committed.

### What was committed here

Three commits, all the same defect family the brief was sent to close: **a
default pointing at something that is not the thing under measurement.**

| commit | what it prevents |
|---|---|
| `36d5ad3` | `TMDB_CACHE` defaulted to the unwarmed 8,185-entry cache in `run_one.sh` and `gen_library.py`. It did not fail — it stalled `movie.seasondir` at 1,689 of 1,712 rows (98.7%) and dropped `tv.shortfolder` silently. Both now require the variable; every drain prints the cache path and its entry count. |
| `7de4938` | The README documented a run that cannot produce a correct number: it named the unwarmed cache as *the* cache, wrote `TMDB_CACHE=<the warmed cache>` and never resolved the placeholder, showed `./run.sh` when `WT` is required, and carried the 2026-08-19 counts. |
| `62245fe` | **`inventory.py` never read `TMDB_CACHE` at all** — `argv[1]` or a hard-coded path, and `run.sh` passes no argument. And `pick_entities.py` defaulted `ORACLE_QUERY` into a different checkout. |

### The one that matters

`inventory.py` decides which entities the cache can serve. Reading `argv[1]` with
no argument meant **the population was chosen from the 8,185-entry cache in every
run this instrument has ever made**, however the drain was pointed. The
generation cache and the drain cache were two different directories and no output
said so — the two-populations trap living inside the instrument rather than in a
reading of it.

Fixing it moves the instrument, not the product:

| | before the fix | after |
|---|---:|---:|
| rows | 82,806 | **90,072** |
| batches | 43 | **55** |
| entities rendered | 2,410 | **2,834** |
| correct | 66,572 | **71,434** |

### Does the documented path work from cold?

**Now, yes. Before these commits, no.** Followed exactly as written this morning
it produced a stalled, under-populated run with nothing in the output naming the
cache responsible.

**The brief's acceptance number cannot be reached, and should not be.** It asks
for 63,830 rather than 58,186 on the base. Those are two readings of a 21-shape,
81,094-row instrument. Today's is 22 shapes and 82,806 rows before the inventory
fix and 90,072 after — `movie.seasondir` alone added 1,712 rows, in the very
commit Phase 0 was sent to make. 63,830 and today's figure are not two readings
of one instrument; they are two instruments.

### And the board's baseline row is two runs

> On `origin/main` at `e3208cc`: correct 66,572, `wrong.kind` 0, `wrong.entity`
> 5, absent ~16,200.

`correct 66,572` comes from `scored-newtrt` — the arm **with** the file-kind fix.
`wrong.entity 5` comes from `scored-newbase` — the arm **without** it, which
reads 64,862 correct. No single run reads both. At `e3208cc` the fix is merged,
so `wrong.entity` is **6**, and this loop measured 6 on every arm.

### The harness failed to compile, and that was the good outcome

The 2026-08-21 amendment warned that `replay.rs` re-derives `kind` from
`parse_filename` and would therefore show *zero movement for a change that moved
everything*. Instead `stored_kind` gained a `parsed_year` parameter and the
harness **stopped compiling**. Patched to pass `p.year`, matching both production
call sites exactly. A signature change is a far better failure than a silent
zero.

## Iterations

**Three run, three kept, none reverted.**

| # | item | change | verdict |
|---|---|---|---|
| 1 | board item 1 | interleave IP families in the TMDB resolver | **kept** |
| 2 | board item 3 | the separator between two episode tokens may be padded | **kept** |
| 3 | board item 3 | `Ep` is the same episode marker with two letters | **kept** |

Items 2 and 5 closed without code — a decision and a report, which is what the
board asked for. Item 4 scoped and not started, as instructed.

## Per instrument, before and after

| instrument | base | after | reading |
|---|---|---|---|
| **matcher oracle** | 90,072 rows, correct **71,434**, `wrong.kind` 0, `wrong.entity` 6, absent 18,153, stalled 479 (0.5%), noise floor 0, `requests=0` | correct **71,434**, all counters identical | **0 rows changed** |
| **parser corpus** | 532 / 738 = **72.1%** | **535 / 738 = 72.5%** | +3 gained, **0 lost** |
| **parser sweep** | 74,624 names | 74,624 | 0 regressions, 0 gains |
| **dogfood strict pair** (capture, 25,004) | `groups=3217 ready=24953 unmatched=51 errors=0 requests=0` | **identical on every counter** | 0 changed |
| `cargo test --workspace` | 762 pass, 1 flaky transcode failure under load | **762 pass, 0 failed** | clean |

Both oracle arms ran the same generated library, checksum `aa9657b`, with
`requests=0` on all 55 batches.

### The one failure, and why it was not a regression

`nightjar-transcode`'s `hls::tests::mapped_real_library_end_moov_mp4_copy_keeps_aac`
failed in the first workspace run. Four checks, because the README records a full
volume turning 1 transcode failure into 20 and reading exactly like a change:

1. `nightjar-transcode` does not depend on `nightjar-metadata` — not in its
   `Cargo.toml`, no reference anywhere in the crate. The change could not reach
   it.
2. Base tree, same suite: 158 passed, 0 failed.
3. Same tree, same binary, three consecutive runs: **FAILED, ok, ok**.
4. Final workspace run with all three commits and 5.8 GB free: **762 passed, 0
   failed**, transcode 158/158.

Flaky under parallel execution. Disk was checked first and was never the cause
here — but it was checked first, because it has been the cause before.

## Every zero, and what kind of zero it is

**Iteration 1 — the resolver.** Not run on any of the four measurement
instruments, and no zero reported from them. All four are offline; the resolver
closure is only entered on a connect, and `requests=0` is the oracle's pass
condition. **Insensitive by construction**, every one. Reporting a clean run from
them would be the exact error this loop exists to prevent. The only sensitive
instrument is the test suite, and the evidence is a `#[ignore]`d DNS check
showing the real answer reordered `666666664444 -> 646464646666`.

**Iterations 2 and 3 — the parser.**

- **Oracle: genuinely sensitive, genuinely zero.** Not a population gap —
  **2,877 of the 90,072 basenames carry a padded dash directly after an episode
  token**, every one of them `Revival - S01E01 - Episode 1.mkv`. These are
  precisely the input the new guard must refuse, and the instrument supplied
  2,877 of them unasked. The marker rule consumes the `e` of `episode`, requires
  a digit, finds `i`, and declines — 2,877 times, zero rows moved, while the
  corpus case it must accept was accepted. That is the guard read for what it
  *permits*, at a scale no unit test reaches.
- **Sweep: narrow by population.** It sees parser changes; none of its 74,624
  generated names pads a dash between two episode tokens.
- **Core tests: sensitive and unchanged.** 111 pass on both arms.
- **Dogfood strict pair: sensitive for iteration 2, narrow for iteration 3.**
  **1,632 of the 25,004 captured basenames** carry a padded dash after an
  episode token — `30 Rock - 2x10 - Episode 210 - Bluray-1080p.mkv` is a real
  file, and it is the adversarial case written by a real library rather than by
  a generator. Zero moved. No captured name spells a marker `Ep<digit>`, so
  iteration 3 is unmeasured here.

**And the gap the oracle left is covered by the dogfood pair.** The oracle holds
**no bare-dash range at all** (`S15E06-08`, 0 of 90,072), so the exemption
iteration 2 preserves is invisible to it. The real library has **33**, and they
are unchanged across the pair. Two instruments, two populations, and the
exemption is only measured because both were run.

**Which population was measured, said plainly:** the **capture, 25,004 files**.
Not the database's 25,043. The 610 paths in one and not the other are where the
Futurama regression hid, and no count here is quoted across that gap.

## What I would do next, and why

1. **Item 4, slice 1 — the additive parser API.** `tv.handmade` (5,840 rows) and
   `tv.episodetitle` (5,844) both read **0.0% correct** and are the two largest
   blocks of failure the oracle has. Neither can move without the parent
   directory. Add a context-taking entry *beside* `parse_filename` and change
   nothing else: no instrument breaks, nothing to measure, and it is reviewable
   alone. Only then move the three production call sites one at a time.
2. **Teach the corpus harness to pass the path it already has.** `corpus_run.rs`
   calls `basename(input)` and discards the folder, while **23 corpus cases carry
   a full path** and are marked `season-folder context (path)`. Those 23 are
   unearnable because of the harness, not the parser. This is nearly free and it
   is the instrument item 4 will need to judge itself by.
3. **Build `tv.scene.collide` before ADR-0049 is accepted.** The record says the
   deciding shape does not exist. Its **entities do** — 80 folded TV pairs,
   already picked and already tagged `collision.title`. Spec is in note 05.
4. **Re-price ADR-0049's option A.** It quotes 1,295 unmatched; the corrected
   instrument reads **1,712**.

## What I could not measure, named

- **Item 1 in production.** No instrument here opens a socket. The dogfood pair
  and the oracle both report `requests=0`, which is their pass condition, not a
  reading of this change. The fix is
  argued from ureq's source, a DNS answer and a unit test — not from a drain.
  And it turns *never* into *slow*, not into *fast*: ~5s per call is still spent
  on the first dead IPv6 address, which at 1,758 calls is about 2.6 hours of
  warming. Real Happy Eyeballs needs a parallel race `ureq` 2.x cannot do.
- **ADR-0048's suggestion (item 2).** The ADR says so itself: a below-floor
  suggestion still scores `absent`, so `correct%` cannot validate it. What it
  changes is the cost of a fix flow the oracle does not model.
- **Options B, C and D of ADR-0049 (item 5).** No shape puts two different shows
  in scene folders. Not built here: it changes the population, and it needs
  warming, which is live and human-run.
- **Runtime**, still excluded. The harness generates `duration_ms` from the
  correct entity's own runtime, so anything reading a duration marks its own
  homework.
- **The glued date cluster (board item 3, "3 cases").** Not earnable as
  described. All three also assert a `year` — 2014 from `140722` — because
  `extract.py` drops Sonarr's `airdate`, `month` and `day` as "theirs not ours"
  and keeps `year`. Fixing the title alone moves the corpus by **zero**, and
  asserting an air-date year as the work's year would feed the matcher a wrong
  pin. The oracle generates no daily-dated name, so **it could not see the harm
  either**. Left alone deliberately.
- **How common scene-named folders are in the wild.** `populations.py` answers
  zero, and the one dogfood library is Sonarr-named. That means *not this
  library*, never *not anywhere*.

## Not claimed

Nothing here says the matcher or the parser works. The corpus reads 72.5% of 738
applicable cases, against a measured ceiling of 91.2% that this loop **took from
the board and did not re-derive**. The oracle reads 71,434 correct of
90,072 rows with 18,153 absent, on generated English-only names with no NFOs,
mostly season 1, one drain from empty, and no rescan, manual match or fix flow
anywhere in it. Two of its shapes read 0.0%.
