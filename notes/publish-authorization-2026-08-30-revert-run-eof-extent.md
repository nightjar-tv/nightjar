# Publish authorization — 2026-08-30, reverting the EOF extent

Recorded under [`docs/GIT_RULES.md`](../docs/GIT_RULES.md) §7, before the push it
covers, so `git log` carries the ordering.

**Date:** 2026-08-30.
**Repo:** `nightjar` only.
**Granted by:** the maintainer, in session, for this session only.

**Allowed:** push `transcode/revert-run-eof-extent`, open one pull request for
it.

**Not allowed:** merge. §7 authorizes a merge per pull request by name, and no
pull request was named.

**This authorization does not carry forward.**

## What is being published, and why it is a revert

**This reverts `bb6b24f` (#180), merged earlier today.** #180 changed
`apply_run_eof` to read the ended run's frontier instead of the session map's
maximum. The defect it repaired cannot reach that function — a run that dies at
a hole in the source exits non-zero and goes to `session.failed` — and the
change regressed a case no test covered.

**The question that would have caught it before the merge is which caller
reaches the branch.** It was asked one day late, and both the author and the
reviewer had passed it.

**Fourth authorization record in this repository in one day**, all sharing a
date and differing by a trailing slug. The naming needs a rule; that is noted in
[`publish-authorization-2026-08-30-run-eof-extent.md`](publish-authorization-2026-08-30-run-eof-extent.md)
and still not decided.

**CI will not run on it.** `openapi` fails before starting and every downstream
job skips — four `skipping` lines are not a pass. Gates were run locally and the
pull request body says which, including the five spawn-and-reap negative
controls, which this owes because it touches transcode.
