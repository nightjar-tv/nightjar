# Publish authorization — 2026-09-11, C1 API policy restoration

Recorded under [`docs/GIT_RULES.md`](../docs/GIT_RULES.md) §7 before the push it
covers. Authorization is session-scoped and does not carry beyond this delivery.

**Date:** 2026-09-11.
**Repo:** `nightjar` only.
**Granted by:** the maintainer, in session — “proceede”, after the proposed next
step explicitly included a separate small pull request restoring the already
approved C1 API policy.

## Scope

| allowed | not allowed |
|---|---|
| create a short-lived branch, commit the reviewed C1 documentation restoration, push it, and open one pull request | merge |

No rebase, force-push, tags, push to `main`, branch deletion, or post-open pull
request mutation is authorized.

## Delivery

Branch `docs/c1-api-policy-restoration` restores the approved official-client
API evolution policy and aligns the constitution, contributor rule, release
rule, ADR-0003, and ADR register. It preserves the product-direction changes
merged by pull request #261 and adds no runtime, service, licensing, platform,
or release decision.

The work was prepared in an isolated checkout based on `cb1b8fc` so the
maintainer's dirty product checkout remained untouched.

**Merge is not authorized.** The pull request does not yet exist, and §7
requires merge authority for a named pull request.
