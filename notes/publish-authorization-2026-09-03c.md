# Publish authorization — 2026-09-03, third grant

Recorded under [`docs/GIT_RULES.md`](../docs/GIT_RULES.md) §7, before the push it
covers. The first grant covered #208 and #209 and is spent; the second lives in
`nightjar-meta` and covered the measurement branch for #211. §7 authorization is
never standing.

**Date:** 2026-09-03.
**Repo:** `nightjar` only.
**Granted by:** the maintainer, in session, for this session only.

## Scope

| # | allowed | not allowed |
|---|---------|-------------|
| 1 | push `transcode/contract-test-gets-an-honest-rate`, open one pull request | — |
| — | — | **merge** |

**Merge is not authorized.** No pull request number existed when the grant was
given, and §7 requires a merge to be named per pull request.

No rebase, no force-push, no push to `main`, no branch deletion.

## What it is

The regime decision from `OPEN-DEFECTS` entry 27, taken by the maintainer:
**the contract test gets a real frame rate, and `NoHonestGrid` gets its own
tests under its own names.**

`held_segment_waiter_no_fill_when_pending_moves` protects a contract that came
from a full-title session — no immediate 204 on supersede, which wedged Safari
on doubles. It was built with `VideoEncodePlan::default()`, whose
`source_frame_rate` is `None`, so the session had **no honest grid** and
`want_is_listed` was false for every want. **It was asserting a full-title
contract from inside the other regime.** That is how the flake was first misfiled
as entry 13: same error string, different path.

## Gates

At this branch, base `1f0bad9`, `web/build` present,
`NIGHTJAR_TEST_REQUIRE_FFMPEG=1`:

    cargo fmt --all -- --check                          pass
    cargo clippy --workspace --all-targets -D warnings  pass
    cargo test --workspace                              863 passed, 0 failed

860 before, 863 after — three tests added, none removed.

## Every new assertion was shown to go red

| control | edit | result |
|---|---|---|
| 1a | `miss_refusal` always `NotFound` (pre-#208) | **FAILED** |
| 1b | `miss_refusal` always `NotReady` (first look loses its 404) | **FAILED** |
| 2 | the `NoHonestGrid` test given an honest rate | **FAILED** |
| 3 | the contract test put back on `VideoEncodePlan::default()` | **FAILED** |

**Control 3 is the one that matters.** It proves the contract test now refuses to
run in the wrong regime, so the misfiling that started entry 27 cannot recur
silently.

**1a and 1b together** show the rule is pinned in both directions: it catches the
old behaviour and it catches over-correcting into never answering 404.

## A named function, because the honest test needed one

`miss_refusal(accepted_hold)` is new, and both refusal sites in `asset_wait`'s
`Wait` arm now return through it. **This is a refactor inside a test change and
it is named here rather than slipped in.**

The reason is measurement, not taste. **Reaching either site end to end needs a
seek to land inside one `SEGMENT_POLL` of a held request, measured at 2 runs in
20.** An integration test named for that rule would have looked like coverage and
been it one run in ten — the exact failure this session has recorded four times
already. The rule gets one deterministic unit test instead, and the end-to-end
test is named for what it guarantees on every run rather than for the branch it
sometimes reaches.

## What is not covered, stated rather than implied

**No test reaches the two refusal sites deterministically**, and none of these
three changes that. `a_held_want_is_never_answered_404` reaches the behind-window
site about 2 runs in 20; the rest release through `no_fill_release_for_new_land`.
CI remains the only instrument that exercises those lines under load, which is
what #209 and #211 were for.

## Elsewhere

`nightjar-meta` `notes/OPEN-DEFECTS.md` entry 27 — the regime question this
answers, and the pooled base arms behind its closure.
