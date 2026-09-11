# Publish authorization — 2026-09-11, R2 fixture rights

Recorded under [`docs/GIT_RULES.md`](../docs/GIT_RULES.md) §7 before the push it
covers. Authorization is session-scoped and does not carry beyond this delivery.

**Date:** 2026-09-11.
**Repo:** `nightjar` only.
**Granted by:** the maintainer, in session — “yws”, in response to the named
request to commit, push and open the verified fixture-rights remediation PR.

## Scope

| allowed | not allowed |
|---|---|
| create a short-lived branch, commit the independently verified R2 fixture-rights remediation and this authorization record, push it, and open one pull request | merge |

No rebase, force-push, tags, push to `main`, branch deletion, or post-open pull
request mutation is authorized.

## Delivery

Branch `testdata/r2-fixture-rights` removes the two tracked Dolby Vision P8.1
pair files and their external derivation paths, records the missing pair as an
explicit non-counted coverage gap, and replaces four metadata fixtures with
original synthetic GPL-3.0-only equivalents plus item-level provenance. Only
dependent test assertions change; product behavior and thresholds do not.

The work was prepared and independently verified in an isolated checkout based
on merged pull request #262 at `77c08c9`.

**Merge is not authorized.** The pull request does not yet exist, and §7
requires merge authority for a named pull request.
