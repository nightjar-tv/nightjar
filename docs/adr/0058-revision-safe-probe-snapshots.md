# ADR-0058: Revision-safe atomic probe snapshots

**Status:** Accepted (2026-09-14)

## Context

Technical media facts are cached observations of mutable files. A probe that
finishes after an in-place replacement must not publish facts for the prior
bytes, and playback must not combine scalar facts with track rows from a
different observation. The file remains authoritative; persisted probe data is
usable only when certified against the current media revision and content ID.

This decision reuses ADR-0023 `content_id`. It does not introduce another
fingerprint, history table, eager backfill, or a playback-time probe.

## Decision

### Schema and revision meanings

`media_items` gains:

- `media_revision INTEGER NOT NULL DEFAULT 1 CHECK(media_revision >= 1)`;
- `probe_revision INTEGER NOT NULL DEFAULT 0 CHECK(probe_revision >= 0)`;
- `probed_media_revision INTEGER NULL`; and
- `video_stream_index INTEGER NULL`.

Existing technical columns, `content_id`, and `probed_content_id` remain.
`probe_revision` identifies an accepted publication, not media identity. A
successful forced reprobe increments it; media replacement never resets it.

`media_item_subtitle_tracks` gains
`probe_revision INTEGER NOT NULL DEFAULT 0`; its existing fields and identity
remain. Add `media_item_audio_tracks` with:

```sql
media_item_id INTEGER NOT NULL REFERENCES media_items(id) ON DELETE CASCADE,
probe_revision INTEGER NOT NULL,
stream_index INTEGER NOT NULL,
codec TEXT NOT NULL,
language TEXT NULL,
channels INTEGER NULL,
channel_layout TEXT NULL,
title TEXT NULL,
is_default INTEGER NOT NULL CHECK(is_default IN (0,1)),
PRIMARY KEY (media_item_id, stream_index)
```

New items start at media revision 1 and probe revision 0, with both validity
stamps NULL. An accepted change to observed media bytes, source path, or
library-root binding increments `media_revision` exactly once and clears
`probed_media_revision` and `probed_content_id`; an unchanged observation does
neither. Sidecar-only changes affect neither revision and belong to ADR-0042's
follow-up work.

Legacy migration assigns revisions 1 and 0, NULL validity stamps, subtitle row
revision 0, and an empty audio inventory. It preserves current facts and
statuses and schedules no migration-time probe sweep.

### Probe capture and publication

A worker captures `{item_id, library_id, library_root, path, media_revision,
probe_revision, content_id, mtime_ms, size_bytes}` before probing. A NULL
`content_id` cannot certify a successful snapshot. Probe execution and final
filesystem validation happen outside the database transaction; changed input
invalidates the result.

Final validation applies to successful and failed probe execution. A final
stat matching the captured mtime and size proceeds to the database CAS. A
successful stat with a different tuple makes the result stale and publishes
nothing. If stat cannot access the source, the worker may publish only
`unavailable` carrying that access failure (never the earlier probe error),
subject to the same CAS. Administrative cancellation while the source remains
readable publishes nothing; cancellation is not itself a technical media fact.

Publication uses `BEGIN IMMEDIATE`. Its compare-and-swap requires every
captured field, including current library-root binding and expected
`probe_revision`, still to match. On success, one transaction:

1. writes every technical scalar, including the selected absolute video index;
2. replaces the complete audio and subtitle inventories at
   `probe_revision + 1`;
3. derives subtitle classification from that inventory; and
4. sets `probe_status='probed'`, `probed_media_revision=media_revision`, and
   `probed_content_id` to the captured nonempty identity.

Commit results are `Published { media_revision, probe_revision }`,
`FailureRecorded`, or `Stale`. A CAS mismatch rolls back without changing
facts, inventories, statuses, or timestamps and returns `Stale`; stale work is
not counted as a successful probe. Database errors remain errors.

Failed probes use the same CAS. They record `error` or `unavailable`, clear the
validity stamps, retain prior technical facts and inventories for diagnostics,
and do not increment `probe_revision`. Any partial child/scalar write failure
rolls back the whole publication.

### Reads

A coherent probe read uses one database read transaction and returns item
technical fields plus audio and subtitle rows ordered by absolute stream index,
all bearing the same positive `probe_revision`. It returns an immutable
`Ready` snapshot only when status is `probed`,
`probed_media_revision == media_revision`, and nonempty
`probed_content_id == content_id`; otherwise it returns `Unverified`.

Concurrent demand for one item and media revision is single-flight. Normal
unchanged scanning and playback add no hashes, content reads, or ffprobes, and
there is at most one ffprobe for an item/media revision. Failure suppression
for an unchanged media revision and identical playback plan is not erased by a
forced probe publication.

## Scope

This ADR establishes revision-safe storage and scanner publication. Playback
preflight/recovery, cross-session circuit breaking, sidecar artifact revisions,
clients, public APIs, and background integrity sweeps are separate slices.

## Verification

Tests must prove that held revision A cannot overwrite B, a forced child write
failure leaves no partial snapshot, concurrent same-revision demand starts one
ffprobe, successful scalar and inventory revisions agree, legacy migration is
lazy and count-preserving, and frame-rate write-back cannot cross a revision.
