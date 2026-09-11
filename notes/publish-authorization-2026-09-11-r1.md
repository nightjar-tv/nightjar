# Publish authorization — 2026-09-11, R1 reconciliation

Recorded under [`docs/GIT_RULES.md`](../docs/GIT_RULES.md) §7 before the push it
covers. Authorization is session-scoped and does not carry beyond this delivery.

**Date:** 2026-09-11.
**Repo:** `nightjar` only.
**Granted by:** the maintainer, in session — “okay lets proceed with next step”,
after the next step was named as delivering the verified R1 product correction
through a dedicated branch and pull request.

## Scope

| allowed | not allowed |
|---|---|
| create a short-lived branch, commit the reviewed R1 documentation correction, push it, and open one pull request | merge |

No rebase, force-push, tags, push to `main`, branch deletion, or post-open PR
mutation is authorized.

## Delivery

Branch `docs/r1-final-reconciliation` records the approved Core/Plus boundary,
corrects obsolete authentication and permanent no-premium claims, clarifies the
single Nightjar executable versus external media-tool dependencies, and records
TVDB as unresolved and evidence-gated. It changes documentation and governing
rules only; it adds no service, provider integration, license change, platform
qualification, or release claim.

The work was prepared in an isolated checkout at merged baseline `fa43ada` so
the maintainer's dirty product checkout remained untouched. Independent prose
and constitution verification passed before publication.

**Merge is not authorized.** The pull request does not yet exist, and §7
requires merge authority for a named pull request.
