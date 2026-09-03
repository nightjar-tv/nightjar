# Publish authorization — 2026-09-03, first grant

Recorded under [`docs/GIT_RULES.md`](../docs/GIT_RULES.md) §7, before the push it
covers. The 2026-09-02 grants named their own branches and do not survive the
session that granted them; §7 authorization is never standing.

**Date:** 2026-09-03.
**Repo:** `nightjar` only.
**Granted by:** the maintainer, in session, for this session only.

## Scope

| # | allowed | not allowed |
|---|---------|-------------|
| 1 | push `transcode/accepted-hold-is-never-404`, open one pull request | — |
| 2 | push `ci/accepted-hold-sweep`, open one pull request, push 20 empty commits to it | — |
| — | — | **merge** |

**Merge is not authorized.** No pull request number existed when the grant was
given, and §7 requires a merge to be named per pull request.

No rebase, no force-push, no push to `main`, no branch deletion.

## The sweep pushes to an open pull request, deliberately

§7 says the carve-out never extends to *"touching a PR after it is opened."*
**The sweep does exactly that, and it is named here rather than assumed.** A CI
run on a non-`main` branch only happens on the `pull_request` event, so twenty
runs need twenty commits pushed after the pull request exists. This is how #207
produced the arm this sweep is compared against — 24 commits, the pull request
opened at the first.

The measurement branch **never merges**, so nothing it carries reaches `main`.
The fix branch takes no sweep commits.

## What it is

One commit, `b6650f9`, base `a1a76dd`, no base drift. Two guards in
`asset_wait` in `server/crates/transcode/src/hls.rs`, plus the ADR-0054
decision 3 amendment.

## Gates

At `b6650f9`, with `web/build` present.

    cargo fmt --all -- --check                          pass
    cargo clippy --workspace --all-targets -D warnings  pass
    cargo test --workspace                              860 passed, 0 failed

`NIGHTJAR_TEST_REQUIRE_FFMPEG=1`, so nothing skipped an ffprobe guard. 860
before, 860 after — the change adds no test.

## The green is not evidence, and this grant says so before it is quoted

**No test in the suite reaches either line this change adds.** Four controls,
run at `b6650f9`:

| control | edit | transcode suite |
|---|---|---|
| A | `accepted_hold` forced `true` from the first look | 199 passed, 0 failed |
| B | `panic!()` at **both** 404 sites | 199 passed, 0 failed |
| C1 | `panic!()` at the `Wait` arm entry | **198 passed, 1 failed** |
| trace | `eprintln!` at the arm and both sites | arm entered **2×**, both sites **0×** |

**C1 is the control for B.** A panic that fires nothing proves nothing until the
same instrument is shown to fire, and C1 fails
`a_far_ahead_listed_want_cooks_and_the_seek_path_still_works` — so the panic
compiles in and is fatal. B's silence is therefore real: the suite executes the
`Wait` arm and stops short of both 404 sites.

Both of the trace's two `Wait` entries already carry `accepted_hold=true`, so
even the arm the suite does reach cannot tell the old code from the new.

**`held_segment_waiter_no_fill_when_pending_moves` — the test this change is
for — never enters the `Wait` arm locally at all.** Run alone under the trace it
emits nothing and passes in 2.83 s. The local suite is blind to the path on
which that test fails in CI.

So `860 passed, 0 failed` is a statement that nothing else broke. **It is not a
check on this change, in either direction**, and it must not be cited as one.

## The instrument that can see it

A CI sweep, against #207's measured 4-of-20 at `8eb7102`. Twenty runs, one empty
commit each, matching #207's cadence — the failure is load-dependent, so the
burst profile is part of the arm and a slower sweep would be a different one.

**The base differs from #207's by one commit.** #207 measured at `8eb7102`; this
branch sits on `a1a76dd` (#206), which is test-only and is the commit #207
exonerated. Named because a control's base is part of it.

## Elsewhere

`nightjar-meta` `notes/OPEN-DEFECTS.md` entry 27, which #207 was opened against.
