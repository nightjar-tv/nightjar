# Publish authorization — 2026-09-12, R4 held-encoder bound

Recorded under [`docs/GIT_RULES.md`](../docs/GIT_RULES.md) §7 before the push it
covers. Authorization is session-scoped and does not carry beyond this
delivery.

**Date:** 2026-09-12.
**Repo:** `nightjar` only.
**Granted by:** the maintainer, in session — “continue”, after the proposed next
step explicitly named preparing the independently verified R4 held-encoder-cap
pull request.

## Scope

| allowed | not allowed |
|---|---|
| commit the verified R4 change and its ADR amendment, push this branch, and open one pull request | merge |

No rebase, force-push, tags, push to `main`, branch deletion, or post-open pull
request mutation is authorized.

## Delivery

Branch `test/r4-playback-resource-bounds` caps each HLS session at two retained
encoders across all rungs, reaping the oldest before another supersession is
retained. It preserves the current producer, latest seek, mapped bytes and
existing retryable waiter behavior. ADR-0050 records the measured Apple Silicon
scope and the resulting lifecycle decision.

The unchanged baseline reached 15 FFmpeg children and about 1.39 GiB aggregate
child RSS across three authorized sessions. The candidate repeated the workload
at nine children and about 872 MiB, kept all seeks applied, allowed another
viewer to progress and left zero descendants after teardown. This does not
qualify N150/QSV hardware.

**Merge is not authorized.** The pull request does not yet exist, and §7
requires merge authority for a named pull request.
