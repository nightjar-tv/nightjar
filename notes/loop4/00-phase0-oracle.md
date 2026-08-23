# Phase 0 — the oracle repo

## The premise was already false

The brief says `~/nightjar-spikes/matcher-oracle-2026-08-19` is dirty with five
uncommitted fixes. **It was clean.** A session at 11:00–11:01 on 2026-08-23 —
about an hour before this one — committed all five:

| fix | commit |
|---|---|
| `preflight.sh`, the stale `WT` default, the strict-mode guard | `021da67` |
| `movie.seasondir`, `score_binding.py`'s unmanifested count | `afd6a56` |
| per-arm output ignored so `git status` means something | `e79133f` |

Each was verified present in the tree rather than taken from the commit
message. Nothing was re-committed.

## What was still wrong — the same defect, one directory along

`TMDB_CACHE` defaulted to `~/nightjar-wt-loop-scratch/replay/tmdb-cache`, in
**both** `run_one.sh` and `gen_library.py`. That is the unwarmed 8,185-entry
cache the instrument was first built against. Two shapes have been warmed since
and neither warming went into it.

**So the default did not fail.** It read a real directory that cannot serve the
current shape set:

  * `movie.seasondir` stalls **1,689 of 1,712 rows — 98.7%**. The shape added
    last session to catch a film filed under a numbered season measures 23 rows.
  * `gen_library.py` drops `tv.shortfolder` outright: the shape verifies its
    truncated heads against the cache, and an unverifiable head is discarded.
    A missing shape and a smaller shape look identical in a row total.

This is precisely the stale-`WT` defect in a second variable, and it survived
the session that fixed the first one. Both now refuse to run without the
variable, and **every drain prints the cache path and its entry count** — three
caches exist on this machine and a run's output never named the one it read.

    8,185   ~/nightjar-wt-loop-scratch/replay/tmdb-cache      run_one.sh's old default
   20,350   ~/nightjar-wt-matcher-scratch/tmdb-cache-warm
   22,122   ~/nightjar-wt-matcher-scratch/tmdb-cache-kind     the one the current shapes need

Committed as `36d5ad3`. The `run_all.sh` check sits **above** `rm -rf "$DEST"`:
the first cut sat below it, so an unset variable destroyed the previous drain
and then refused to produce a new one.

## The README documented a run that produces the wrong number

Committed as `7de4938`. Followed exactly, it could not produce a correct
measurement:

  * the "what it needs" table named the **unwarmed** cache as *the* cache;
  * the amendment that warned about `TMDB_CACHE` wrote `TMDB_CACHE=<the warmed
    cache>` and never resolved that placeholder to a path anywhere in the file;
  * the shown invocation was `./run.sh`, which cannot run — `WT` is required and
    rightly has no default;
  * the counts were the 2026-08-19 ones, 34 libraries and 17 shapes, against 43
    batches over 22 shapes today;
  * 4.2 GB of free disk was a warning inside an amendment rather than a listed
    prerequisite.

## The acceptance number in the brief is stale

The brief asks for **63,830 rather than 58,186** on the base. Neither is
reachable now, and chasing 63,830 would have been chasing a number from a
different instrument.

  * `58,186` — the pre-`stored_title` harness. A real defect, fixed.
  * `63,830` — the corrected harness at **21 shapes / 81,094 rows**.
  * The instrument today is **22 shapes / 82,806 rows**: `movie.seasondir`
    added 1,712 of them, in the commit Phase 0 was sent to make.

`63,830` and today's figure are not two readings of one instrument. They are two
instruments.

## And the board's baseline row is two runs, not one

> On `origin/main` at `e3208cc`: correct 66,572, `wrong.kind` 0, `wrong.entity`
> 5, absent ~16,200.

Summed from the recorded per-arm tables in the oracle repo:

| | `scored-newbase` | `scored-newtrt` |
|---|---:|---:|
| correct | 64,862 | **66,572** |
| `wrong.entity` | **5** | 6 |
| absent | 16,250 | 16,228 |
| stalled | 1,689 | 0 |

`newbase` and `newtrt` are the same commit, `e28f0be`, differing by the
file-level kind fix as an uncommitted diff. **The board takes `correct` from the
fixed arm and `wrong.entity` from the unfixed one.** No single run reads
66,572 with `wrong.entity` 5.

That fix is on `main`: `stored_kind` at `e3208cc` carries the `parsed_year`
guard. So the row to expect at base is `newtrt`'s — `wrong.entity` **6**.

## The harness did not compile against main, and that is the good outcome

The 2026-08-21 amendment warned that `replay.rs` re-derives `kind` from
`parse_filename`, and that a scanner-level kind rule would therefore show **zero
movement for a change that moved everything**.

It did not play out that way. `stored_kind` gained a `parsed_year` parameter, so
the harness **failed to compile** instead of quietly reporting a zero. Patched
to pass `p.year`, matching both production call sites
(`scanner/src/lib.rs:405,860`) exactly. `kindprobe.rs` needed the same.

A signature change is a much better failure than a silent one, and it is worth
preferring deliberately: it is the difference between an instrument that lies
and an instrument that stops.
