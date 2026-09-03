# Publish authorization — 2026-09-04

Recorded under [`docs/GIT_RULES.md`](../docs/GIT_RULES.md) §7 before the push it
covers. §7 authorization is never standing; the 2026-09-03 grants are spent.

**Date:** 2026-09-04.
**Repo:** `nightjar`.
**Granted by:** the maintainer, in session — "you have authority", in answer to a
message naming this branch and asking whether to push it.

## Scope

| allowed | not allowed |
|---|---|
| branch, commit, push, open pull requests for this session | **merge** |

**Merge is not authorized.** §7 requires a merge to be named per pull request,
and no number existed when the grant was given.

No rebase, no force-push, no push to `main`, no branch deletion.

## What it covers first

`adr/0051-ladder-mechanics` — an amendment to ADR-0051 deciding the four
mechanics S6 cannot be built without: a rung as a path segment, one segment map
per rung, audio muxed per rung in v1 with its cost quantified, and a requirement
that the session cap stop counting sessions before a ladder is enabled by
default.

The amendment exists because a recon of the tree before scoping S6 found four
places the current shape cannot express a ladder at all. Deciding them inside the
slice would have buried them in an implementation diff.

**It is an amendment rather than a new ADR** because it decides mechanics for a
decision ADR-0051 already took, and because `authority.rs`'s route test states in
its own comment that changing the cookie-accepted route count "is an ADR
amendment" — this is that amendment, naming the routes it adds.

## Not covered

**`CLAUDE.md` is modified in this working tree by the maintainer** and is left
unstaged, as it has been all session.

**No product code changes here.** The amendment decides; S6 builds. Nothing in
this branch touches `hls.rs`, the routes, or the segment map.

## Elsewhere

`nightjar-meta` `notes/ops-authorization-2026-09-04-n150-main.md` — the separate
grant for deploying `origin/main` to the bench host, which is a different action
under a different rule.
