# Publish authorization — 2026-09-03, fourth grant

Recorded under [`docs/GIT_RULES.md`](../docs/GIT_RULES.md) §7, before the push it
covers. The first grant covered #208 and #209; the second lives in
`nightjar-meta` and covered the measurement branch for #211; the third covered
`transcode/contract-test-gets-an-honest-rate`. All are spent. §7 authorization is
never standing.

**Date:** 2026-09-03.
**Repo:** `nightjar` only.
**Granted by:** the maintainer, in session, for this session only — "approved,
proceed", in answer to a question naming this exact change.

## Scope

| # | allowed | not allowed |
|---|---------|-------------|
| 1 | push `adr/accept-0050-0051-0052`, open one pull request | — |
| — | — | **merge** |

**Merge is not authorized.** No pull request number existed when the grant was
given, and §7 requires a merge to be named per pull request.

No rebase, no force-push, no push to `main`, no branch deletion.

## What it is

**ADR-0050, ADR-0051 and ADR-0052 move from `proposed` to `accepted`.**

They have been proposed since 2026-08-23 while code and measurement accumulated
against all three:

- **ADR-0050** (a session is one throttled encoder holding a lead) — most of it
  shipped across `#158`–`#162`. `LEAD_TARGET_MS` and `LEAD_FLOOR_MS` are in
  `hls.rs:72-73`.
- **ADR-0051** (ABR ships in v1) — supersedes ADR-0008 §1, which has said so in
  its own header since 2026-08-23. The ladder itself is unbuilt; the decision is
  what is being accepted.
- **ADR-0052** (2 s IDR grid per encode leg, from source fps) — shipped in
  `#153`; migration `024` names it.

The prompt was a status audit: three `proposed` ADRs gate Gate 2 and have work
built against them, which makes "proposed" a poorer description of their state
than "accepted".

## Two corrections that ride along, both in the register

**The `Proposed` count was wrong before this change.** The paragraph named ten
entries and listed 0002, 0042, 0046, 0047, 0049, 0050, 0051, 0052, 0053 and
0054 — omitting **0055**, which was on the table. That paragraph carries its own
warning that it "was wrong about its own table for most of that time", and it was
wrong again. The list is now derived from the table rather than typed: eight
entries, 0002, 0042, 0046, 0047, 0049, 0053, 0054 and 0055.

**"Three of the ten have shipped"** counted from the same wrong figure. Rewritten
to say what it means — a proposed entry may still have shipped — without a count.

## Gates

**Documentation only. No code, no tests, no build.** Four files, all Markdown:
the three ADR headers and `docs/adr/README.md`. `git diff --cached --name-only`
lists exactly those four.

Running `cargo test --workspace` would prove nothing about this change, and is
not run. Saying so rather than reporting a pass that has no bearing.

## What is not covered

**`CLAUDE.md` is modified in this working tree and is not mine.** It is left
unstaged and uncommitted. The maintainer's edit stands.

**Accepting an ADR is not shipping it.** ADR-0051's three-rung ladder is unbuilt:
`build_master` at `hls.rs:3314` emits one hardcoded variant and `RESOLUTION`
appears zero times in the transcode crate. Acceptance records that the decision
is settled, not that the code exists.

**`OPEN-DEFECTS` entry 29 is open against ADR-0050 and ADR-0051 together.** A
hopping client accumulates one held encoder per rung visited; measured
2026-09-03, one viewer produced 22,264 s of media to serve 332 s. Accepting both
ADRs does not resolve it, and it should be read as a live question against them
rather than as settled by this push.

## Elsewhere

`nightjar-meta` `notes/gate2-status-2026-09-03.md` — what remains before Gate 2,
read from the tree, including the stale `V1_PLAN.md` lines corrected in `5b1d18f`.
