# Publish authorization — 2026-09-02, fifth grant

Recorded under [`docs/GIT_RULES.md`](../docs/GIT_RULES.md) §7, before the push it
covers. The fourth grant named one branch and it is merged; §7 authorization is
never standing.

**Date:** 2026-09-02.
**Repo:** `nightjar` only.
**Granted by:** the maintainer, in session, for this session only.

## Scope

| # | allowed | not allowed |
|---|---|---|
| 1 | push `docs/citations-sweep`, open one pull request | — |
| — | — | **merge** |

**Merge is not authorized.** No pull request number exists yet to name.

No rebase. The branch was created at `973469d`.

## What it is

Two commits, base `973469d`, no base drift.

`cc189ea` — the citation sweep §4 asked for: fifteen files, source comments
keeping their reason and losing their pointer, ADRs quoting the measurement and
marking the location maintainer-private.

`747bbd8` — 42 working-note files leave `notes/` for the maintainer repository,
where they landed first as `4db148e`. `docs/adr/0049`'s citation of a deleted
script is corrected, and §4 gains two things: authorization records as a second
exception, and the post-move grep check.

**Comment-only in every source file. No behaviour changes.**

## Gates

At `973469d`, with `web/build` present.

    cargo fmt --all --check                     pass
    cargo clippy --all-targets -- -D warnings   pass
    cargo test --workspace                      858 passed, 0 failed
    npm run check (web)                         168 files, 0 errors

`NIGHTJAR_TEST_REQUIRE_FFMPEG=1`, so nothing skipped an ffprobe guard. Re-run
after the §4 amendment rather than carried over from before it.

`gate1` is not a required check and `--auto` fires before it (§3). Wait for it
explicitly, and read the four check results rather than the mergeable flag.

## Elsewhere

`nightjar-meta` `4db148e` (the notes, landed before the deletion here) and
`e711f3c` (the blind spot the move exposed).
