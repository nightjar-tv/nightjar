# Publish authorization — 2026-09-02, third grant

Recorded under [`docs/GIT_RULES.md`](../docs/GIT_RULES.md) §7, before the push it
covers. The two earlier grants today cover different branches; §7 authorization
is never standing, so this is its own record.

**Date:** 2026-09-02.
**Repo:** `nightjar` only.
**Granted by:** the maintainer, in session, for this session only.

## Scope

| # | allowed | not allowed |
|---|---|---|
| 1 | push `scanner/path-writers-own-binaries`, open one pull request | — |
| 2 | push `docs/adr-0054-decision-3-heading`, open one pull request | — |
| 3 | push `db/usernames-are-case-insensitive`, open one pull request | — |
| — | — | **merge, any of them** |

**Merge is not authorized.** §7's 2026-08-17 amendment permits it only when named
by pull request number, in session, and recorded before it runs. No number exists
yet, so none is named here.

## A rebase was run, and it is outside the carve-out

`scanner/path-writers-own-binaries` was rebased from `f527198` onto `2723dcf`.
**The maintainer instructed it directly this session** — *"Rebase the parked
slice onto 2723dcf"* — and the branch had never been pushed, so nothing published
was rewritten. Recorded rather than passed over, because §7 lists rebase among
the things the carve-out does not reach and a reader should not have to infer
that an instruction covered it.

The other two branches were created at `2723dcf` and have not been rebased.

## The three branches

All based on `2723dcf`. They touch disjoint files, so the merge order does not
matter.

| branch | commits | touches |
|---|---|---|
| `scanner/path-writers-own-binaries` | `04f9709`, `12199bc` | `server/crates/scanner/` |
| `docs/adr-0054-decision-3-heading` | `c142858` | `docs/adr/0054-*`, `server/crates/transcode/src/hls.rs` |
| `db/usernames-are-case-insensitive` | `dfd4332` | `server/crates/db/` |

The `transcode` change in the second is a doc comment. No behaviour moves.

## Gates

Run per branch, at `2723dcf`, with `web/build` present.

    cargo fmt --all --check                     pass
    cargo clippy --all-targets -- -D warnings   pass
    cargo test --workspace                      pass

Test counts: 853 on `scanner` and on `docs`, 858 on `db` — the five new account
and migration tests are the difference. `NIGHTJAR_TEST_REQUIRE_FFMPEG=1` was set
on every run, so nothing skipped an ffprobe guard; zero `skip:` lines.

**CI has not run on any of these yet.** `notes/OPEN-DEFECTS.md` entry 8 records
five occurrences of a billing stop skipping every required check while the pull
request still read mergeable, the most recent on #196 yesterday. **Wait for the
four checks before merging anything here**, and read their status rather than the
mergeable flag — #198 was merged on `UNSTABLE` with `web` still in flight.
