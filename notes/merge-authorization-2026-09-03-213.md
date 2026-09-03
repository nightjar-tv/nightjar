# Merge authorization — #213

Recorded under [`docs/GIT_RULES.md`](../docs/GIT_RULES.md) §7 before the merge it
covers. §7 authorizes a merge **per pull request, by name**; a session-scoped
push grant does not carry one.

**Date:** 2026-09-03.
**Pull request:** #213 — `docs: ADR-0050, 0051 and 0052 are accepted`.
**Granted by:** the maintainer, in session — "merge it".

## What merges

Documentation only. Four Markdown files: three ADR headers move from `proposed`
to `accepted`, and `docs/adr/README.md` records it. No code, no migration, no
test.

## Gates

`openapi` pass, `web` pass, `server` pending at the time of the grant. The branch
reads `MERGEABLE / UNSTABLE` — unstable because `server` had not finished, not
because anything failed.

**No gate on this branch can fail on its contents**, because the branch contains
no code. That is the reason to merge without waiting, and it is stated rather
than left as an assumption. If `server` goes red it will be red on `main` for a
reason this branch did not introduce.

## What it does not settle

**ADR-0051's ladder is unbuilt.** `build_master` at `hls.rs:3314` emits one
hardcoded variant; `RESOLUTION` appears zero times in the transcode crate.

**`OPEN-DEFECTS` entry 29 stays open against ADR-0050 and ADR-0051 together.**
One hopping viewer produced 22,264 s of media to serve 332 s on the N150.
Accepting both ADRs does not answer it, and it is the next decision.

## Elsewhere

`notes/publish-authorization-2026-09-03d.md` — the push grant this merge follows.
