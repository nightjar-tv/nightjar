# Publish authorization — 2026-08-30, `apply_run_eof`'s extent

Recorded under [`docs/GIT_RULES.md`](../docs/GIT_RULES.md) §7, before the push it
covers, so `git log` carries the ordering.

**Date:** 2026-08-30.
**Repo:** `nightjar` only.
**Granted by:** the maintainer, in session, for this session only.

**Allowed:** push `transcode/run-eof-extent`, open one pull request for it.

**Not allowed:** merge. §7 authorizes a merge per pull request by name, and no
pull request was named.

**This authorization does not carry forward.** A later session needs its own.

## The ordering, stated exactly

The work commit precedes this note. It was committed locally in an earlier turn,
when no authorization existed and none was needed — §7's bar is push, tags and
merges, and a local commit is on none of those lists. The grant arrived in the
turn after, and this note is committed before the push.

Both commits precede the push, which is what §7 asks for. This is the same
ordering [`publish-authorization-2026-08-30.md`](publish-authorization-2026-08-30.md)
recorded and for the same reason: rebasing the note underneath the work would
produce a tidier history that claims the authorization came first, and it did
not.

## The third record in this repository, and the naming is now a question

The first said it was "a new file rather than an established convention, and it
is worth someone deciding whether it should become one." Three files now share
one date and differ only by a trailing slug. **That is past the point where the
naming needs a rule** — either one file per session with a scope list, or a
directory. Recorded here rather than decided, because deciding it is a change to
§7's shape and belongs with someone who wants to make it.

## What is being published

One commit to `nightjar-transcode`. **No migration, no API spec change**, and the
wire field it touches is already in `openapi.yaml`.

**CI will not run on it.** `nightjar-meta` `OPEN-DEFECTS.md` entry 8: billing is
stopped, `openapi` fails before starting, every downstream job skips. **Four
`skipping` lines are not a pass.** The gates were run locally and the PR body
says which.
