# Publish authorization — 2026-09-02, second grant

Recorded under [`docs/GIT_RULES.md`](../docs/GIT_RULES.md) §7, before the push it
covers. The first grant of the day covers a different branch and was merged with
it; this is separate because §7 authorization is per branch and never standing.

**Date:** 2026-09-02.
**Repo:** `nightjar` only.
**Granted by:** the maintainer, in session, for this session only.

## Scope

| # | allowed | not allowed |
|---|---|---|
| 1 | push `loop/parser-board-overnight`, open one pull request | **merge** |

**Merge is not authorized.** §7's 2026-08-17 amendment permits it only when named
by pull request number, in session, and recorded before it runs. No number exists
yet. **Rebase is outside the carve-out** and none was done — this branch is based
on `f527198` and `main` has moved to `24b0cea`, which is a clean base for a merge
commit because the two touch different files.

## What the branch is

Fifteen commits: five parser changes in `nightjar-core`, nine notes, and one
deletion of a duplicate harness. Nothing else in `server/` is touched.

**Read [`notes/loop-overnight/09-rebaselined.md`](loop-overnight/09-rebaselined.md)
first.** Notes 00 to 08 were measured through `parse_filename`; the harness calls
`stored_parse` as of #198, and the branch reads **`608/734` → `617/734`** rather
than 598 → 607.

## Gates

Green at the tip when the branch was written, and **not re-run since**, because
nothing in `server/` has changed since — the four commits after `9196cf3` touch
`notes/` only, apart from `89e6cd6`, which was gated.

    cargo fmt --all --check          0
    cargo clippy --all-targets -D    0
    cargo test --workspace           0

`#[test]` attributes on full paths: **844 → 856**, all twelve in
`core/src/filename.rs`. No test was deleted; one assertion changed what it
records, from a known gap to the fix, and that is stated where it happened.

**A fresh worktree cannot run these** until gitignored `web/build/` is copied in —
the API crate embeds it and both `clippy` and `test` exit 101 without it. That is
environment, not a defect, and it is recorded in `00-baseline.md`.
