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
| 3 | merge **#208**, named by the maintainer | — |
| 4 | close **#209** without merging, named by the maintainer | — |

> **Amended 2026-09-03, after both arms were in.** Grants 1 and 2 stood alone
> and this section read: *"**Merge is not authorized.** No pull request number
> existed when the grant was given, and §7 requires a merge to be named per pull
> request."* That was true when written. The maintainer then named #208 and
> #209 in session, which is what §7 asks for — a merge authorized per pull
> request, by number, recorded before it runs. The withdrawn sentence is kept
> rather than replaced, because the ordering is the point of this file.

**Grant 3 covers one merge of one pull request.** It does not carry to any
other, and it does not survive this session.

## What grant 3 is being given on

**Base arm 4 of 20** (#207 at `8eb7102`), **fix arm 0 of 20** (#209 at
`b6650f9`). Fisher exact one-sided **p = 0.053**, which does not clear 0.05.

**The merge does not rest on the sweep alone, and it is not resting on a
threshold that was crossed.** `SEGMENT_POLL` is 100 ms and all four base-arm
failures elapsed 101-105 ms, so each returned on the second loop iteration, one
poll after the hold was accepted — exactly the state grant 3's change guards.
**The mechanism is established independently of the statistics**, and the sweep
corroborates it.

**A 21st clean observation exists and is deliberately not counted.** #208's own
branch run is a real clean run at the identical tree, and folding it in would
put *n* = 21 and *p* = 0.048. Banking it after seeing p = 0.053 would be
choosing the analysis to clear the threshold, so the reported figure stays
p = 0.053 at *n* = 20.

## The merged tree is not byte-identical to the swept tree

The sweep measured tree `7d0f6b0`, at `e4176be`. The merge carries one further
commit, **this file's amendment — notes only, no code**. Stated because "the
sweep measured what merged" is a claim worth keeping exact.

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
