# Publish authorization — 2026-09-02, sixth grant

Recorded under [`docs/GIT_RULES.md`](../docs/GIT_RULES.md) §7, before the push it
covers. The fifth grant named one branch and it is merged; §7 authorization is
never standing.

**Date:** 2026-09-02.
**Repo:** `nightjar` only.
**Granted by:** the maintainer, in session, for this session only.

## Scope

| # | allowed | not allowed |
|---|---|---|
| 1 | push `transcode/rungs-derive-one-grid`, open one pull request | — |
| — | — | **merge** |

**Merge is not authorized.** No pull request number exists yet to name.

No rebase. The branch was created at `8eb7102`.

## What it is

One commit, `4465a06`, base `8eb7102`, no base drift. Two tests in
`server/crates/transcode/src/hls.rs`. **Test-only — no production code is
touched.**

## Gates

At `8eb7102`, with `web/build` present.

    cargo fmt --all --check                     pass
    cargo clippy --all-targets -- -D warnings   pass
    cargo test --workspace                      860 passed, 0 failed

`NIGHTJAR_TEST_REQUIRE_FFMPEG=1`, so nothing skipped an ffprobe guard. 858
before, 860 after.

`gate1` is not a required check and `--auto` fires before it (§3). Wait for it
explicitly, and read the four check results rather than the mergeable flag —
`OPEN-DEFECTS` entry 8 records five occurrences of a billing stop skipping every
required check while the pull request still read mergeable.

## Why a test lands before the feature it protects

S6 is not scoped. The property this pins — one IDR grid across a ladder's rungs
— is the one ADR-0051 decision 2 requires and the one that makes a ladder
switchable at all. **The grid derivation changed twice last week** (`35d9319`
#194, `9d4aff2` #193) **and both changes were single-leg.** An assertion is worth
more before the feature than after, because after, a failure looks like the new
code.

This does not build S6 (Rule 4.8). It pins a property the current code already
has.

## Elsewhere

`nightjar-meta` `6f06887` — `OPEN-DEFECTS` entry 26 (`--bench-rungs` is not a
ladder) and the S6 instrument decision.
