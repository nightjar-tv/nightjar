# Publish authorization — 2026-08-30, ADR-0050 decision 5

Recorded under [`docs/GIT_RULES.md`](../docs/GIT_RULES.md) §7, before the push it
covers, so `git log` carries the ordering.

**Date:** 2026-08-30.
**Repo:** `nightjar` only.
**Granted by:** the maintainer, in session, for this session only.

**Allowed:** branch, commit, push `docs/adr-0050-superseded-encoder-reason`, open
one pull request for it.

**Not allowed:** merge. §7 authorizes a merge per pull request by name, and no
pull request was named. Stated rather than assumed.

**This authorization does not carry forward.** A later session needs its own.

## The second record in this repository

The first was [`publish-authorization-2026-08-30.md`](publish-authorization-2026-08-30.md),
which said it was "the first authorization record committed here, so it is a new
file rather than an established convention, and it is worth someone deciding
whether it should become one." **Nobody has decided.** This file follows it
because following an undecided convention is cheaper than dropping one, not
because the question is closed. Two files now share a date and differ by scope,
which is the first sign the naming needs a rule.

## What is being published, and what it is not

A **documentation** change to one decision in one ADR. It corrects a stated
reason and leaves the decision standing. **No product code changes.**

**CI will not run on it.** `nightjar-meta` `OPEN-DEFECTS.md` entry 8: billing is
stopped, the `openapi` job fails before starting, and every downstream job skips.
**Four `skipping` lines are not a pass.** A reviewer should read this as
unverified by CI, and for a prose-only change that is the whole of what CI would
have told them anyway.
