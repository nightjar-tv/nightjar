# Publish authorization — 2026-09-02

Recorded under [`docs/GIT_RULES.md`](../docs/GIT_RULES.md) §7, before the push it
covers.

**Date:** 2026-09-02.
**Repo:** `nightjar` only. `nightjar-meta` publishes under its own rule —
`AGENT_PIPELINE.md` §Publishing, records straight to `main` — not under this one.
**Granted by:** the maintainer, in session, for this session only.

## Scope

| # | allowed | not allowed |
|---|---|---|
| 1 | push `corpus/entry-point-is-stored-parse`, open one pull request | **merge** |

**Merge is not authorized.** §7's 2026-08-17 amendment permits it on the same
terms as push — named, in-session, recorded before it runs — and no pull request
number has been named. This authorization does not carry forward; a later session
needs its own.

## What the branch is

One change to `notes/loop3/scripts/corpus_results.sh`: the throwaway crate it
writes now takes a `nightjar-scanner` path dependency, and passes `ROOT` through.

**Because the corpus harness changed entry point.** `corpus_run.rs` — which lives
in `nightjar-spikes`, not here — moved from `nightjar_core::parse_filename` to
`nightjar_scanner::stored_parse`. The product stopped calling the first at
`2af44de`, *scanner: one layer decides what a file is* (#172), which introduced
`stored_parse` and wired `parse_with_parent` behind it. Both scanner indexing
paths call it (`scanner/src/lib.rs:501`, `:951`).

Without the dependency the wrapper does not build, so this is not optional.

## Gates

**No cargo gate applies and none was run.** The change is a shell script under
`notes/`; nothing in `server/` is touched, so `cargo fmt`, `clippy` and `test`
cannot see it. A fresh worktree cannot run them anyway — `web/build/` is
gitignored and the API crate embeds it, so both exit 101 until it is copied in.

**The gate for this change is running it**, and it was run: the committed script
against a `f527198` tree gives

    pass 608  fail 126  not_applicable 110  total 844
    applicable pass rate: 82.8%

byte-identical to the same measurement taken with a hand-built crate, and
`classify.py --diff` still aligns against a results file produced by the old
entry point (`+10 / −0`, exit 0) because the harness records its input unchanged.
