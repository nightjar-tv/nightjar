# Publish authorization — 2026-09-02, fourth grant

Recorded under [`docs/GIT_RULES.md`](../docs/GIT_RULES.md) §7, before the push it
covers. The third grant named three branches and all three are merged; §7
authorization is never standing, so this is its own record.

**Date:** 2026-09-02.
**Repo:** `nightjar` only.
**Granted by:** the maintainer, in session, for this session only.

## Scope

| # | allowed | not allowed |
|---|---|---|
| 1 | push `docs/citations-must-resolve-for-the-reader`, open one pull request | — |
| — | — | **merge** |

**Merge is not authorized.** §7's 2026-08-17 amendment permits it only when named
by pull request number, in session, and recorded before it runs. No number exists
yet.

No rebase. The branch was created at `4d370a3`.

## What it is

One commit, one file: §4 of the git rules gains *Citations must resolve for the
reader*. **No code changes.**

## Gates

At `4d370a3`, with `web/build` present.

    cargo fmt --all --check                     pass
    cargo clippy --all-targets -- -D warnings   pass
    cargo test --workspace                      858 passed, 0 failed

`NIGHTJAR_TEST_REQUIRE_FFMPEG=1`, so nothing skipped an ffprobe guard. 858 is the
count after #203 landed its five account and migration tests.

`gate1` is not a required check and `--auto` fires before it (§3). Wait for it
explicitly.

## The rule lands before the sweep, deliberately

**This repository does not comply with the rule this pull request adds** — 47
files break it at `4d370a3`. That is the intended order: a sweep with no rule to
be measured against is a matter of taste, and the categories want different
treatments rather than one pass.

**It is a backlog and not a leak.** This repository is private today.
