# Publish authorization — 2026-08-31

Recorded under [`docs/GIT_RULES.md`](../docs/GIT_RULES.md) §7, before the
pushes it covers.

**Date:** 2026-08-31.
**Repo:** `nightjar` only.
**Granted by:** the maintainer, in session, for this session only.

## Scope

| # | allowed | not allowed |
|---|---|---|
| 1 | push `transcode/s3-full-title-listing`, open one pull request | merge |
| 2 | merge **#182**, named by the maintainer | — |
| 3 | push `docs/adr-s3-measured-corrections`, open one pull request | merge |
| 4 | merge **#183**, named by the maintainer | — |
| 5 | push `api/session-asset-cache-headers`, open one pull request | merge |

**This authorization does not carry forward.** A later session needs its own.

## This file was committed empty, and is restored here

**It landed as 0 bytes in `fb174b6` (#183).** A `git commit -F -` chained
before this heredoc took the heredoc as its commit message, so the commit
carried the note's text as its subject **and `cat` wrote nothing to the file**.

**The wrong subject was caught and amended; the empty file was not.** The check
run at the time was `git log`, which showed the corrected message and says
nothing about content. **Reading the commit's diff is what would have caught
it**, and that is the lesson: after amending a message, look at what the commit
contains, not at what it is called.

So the record for grants 1 to 4 existed only as a commit subject and a pull
request body until now. **It is restored rather than backfilled quietly**,
because a §7 record that appeared to exist and did not is worth more as a
recorded failure than as a tidy file.

## One file per session, from here

**Decided 2026-08-31.** This replaces the one-file-per-branch shape, which
collided four times in two days:

- `publish-authorization-2026-08-30.md`
- `publish-authorization-2026-08-30-adr-0050.md`
- `publish-authorization-2026-08-30-run-eof-extent.md`
- `publish-authorization-2026-08-31-s3-listing.md`

Each recorded one grant and had to invent a slug to avoid the last. **A session
is the unit §7 already uses** — *"authorization is never standing; it does not
survive the session that granted it"* — so the file is one per session, named
for the date, with a scope table that grows as grants arrive.

**Two sessions on one day get `-b`, `-c`.** Rare, and cheaper than a slug that
has to describe the work.

**§7 does not change.** It says the note records the date and the scope and
precedes the action; it never said one file per grant. This is a convention for
where the notes live, decided rather than accreted a fifth time.

**The earlier four stay as they are.** Renaming records that were correct when
written, for tidiness, is not worth it.

## Grant 5 — the first row added to an existing file

**The convention above, used.** It works because the row lands on the branch it
authorizes, before that branch is pushed and before any pull request exists, so
neither §7 exclusion is touched.

**A merge grant still cannot go here**, for the reason recorded in
`nightjar-meta`: adding it would mean pushing to `main` or editing an open pull
request's branch. Grants 2 and 4 above are recorded after the fact, from the
meta-repo notes written before each merge. **That gap is unchanged and this
convention does not close it.**
