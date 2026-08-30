# Publish authorization — 2026-08-31, the S3 full-title listing

Recorded under [`docs/GIT_RULES.md`](../docs/GIT_RULES.md) §7, before the push
it covers.

**Date:** 2026-08-31.
**Repo:** `nightjar` only.
**Granted by:** the maintainer, in session, for this session only.

**Allowed:** push `transcode/s3-full-title-listing`, open one pull request
for it.

**Not allowed:** merge. §7 authorizes a merge per pull request by name, and no
pull request was named.

**This authorization does not carry forward.**

## What is being published

**Seven commits to `nightjar-transcode`.** No migration. No API spec change —
the playlist body changes, the routes and DTOs do not.

**Two of them correct code landed earlier in the same slice**, which is stated
in their own messages rather than folded away: the segment cadence is
frame-quantised on every leg, not only the ones that discard
`-force_key_frames`; and copy's producer key needs snapping onto the point the
walk listed.

**CI will not run on it.** `nightjar-meta` `OPEN-DEFECTS.md` entry 8. **Four
`skipping` lines are not a pass.** Local gates are the bar and the pull request
body says which, including the five spawn-and-reap controls and an on-box
verification against a real copy session.

## The fourth authorization record, and the naming is still undecided

Three files share 2026-08-30 and this one starts 08-31. The question raised in
[`publish-authorization-2026-08-30-run-eof-extent.md`](publish-authorization-2026-08-30-run-eof-extent.md)
— one file per session with a scope list, or a directory — **is still open**,
and a date-based name has now collided four times.
