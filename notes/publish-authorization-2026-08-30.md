# Publish authorization — 2026-08-30

Recorded under [`docs/GIT_RULES.md`](../docs/GIT_RULES.md) §7, before the push
it covers, so `git log` carries the ordering.

**Date:** 2026-08-30.
**Repo:** `nightjar` only.
**Granted by:** the maintainer, in session, for this session only.

**Allowed:** branch, commit, push `docs/adr-register-matches-the-tree`, open one
pull request for it.

**Not allowed:** merge. Stated in the grant rather than assumed — §7 authorizes
a merge per pull request by name, and no pull request was named. The
maintainer's reason is recorded because it bears on the review: a documentation
change correcting records that were wrong needs its own read, and this chain has
already produced three rewrites of one section each written from the previous
text.

Also not allowed, and not requested: any push to `nightjar-meta`. The session
handoff note for that repo is written to
`nightjar-meta/notes/agent-session-handoff-2026-08-30-adr-register.md` and left
**uncommitted** for the maintainer, the same way the 2026-08-07 handoff was when
product publish was authorized and meta publish was not.

**This authorization does not carry forward.** A later session needs its own.

## Why this file is here rather than only in the meta handoff

§7 says the note is committed before the push so that "a push with no
authorization commit before it is a violation on the face of the history,
visible to someone who was not in the room." The history a reader of *this*
repository has is this repository's. Prior sessions recorded the grant only in
the private handoff, which satisfies the sentence about the session handoff and
not the one about `git log`.

This is the first authorization record committed here, so it is a new file
rather than an established convention, and it is worth someone deciding whether
it should become one. It is recorded rather than assumed either way.

## The ordering, stated exactly

This commit sits **after** `docs: reconcile the ADR register with the tree it
describes`, not before it. The work was committed locally in an earlier turn,
when no authorization existed and none was needed — §7's bar is push, tags and
merges, and a local commit is on none of those lists. The grant arrived in the
turn after.

So the true order is: work committed, authorization granted and recorded, push.
Both commits precede the push, which is what §7 asks for. Rebasing this note
underneath the work commit would have produced a tidier history that claimed
the authorization came first, and it did not.
