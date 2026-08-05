# ADR-0030: Library-relative media paths and library repoint

- Status: accepted
- Date: 2026-08-03
- Amended: 2026-08-04 (poll holdoff after deferred_remove > 0;
  repoint single-walk reuse + WalkCache rekey)
- Depends on: ADR-0025 §4 (path-key grammar); ADR-0014 (reachability /
  `delete_missing`); ADR-0015 (async library jobs); ADR-0029 (join stores
  provider keys only — path keys derived)
- Supersedes: none — completes ADR-0025 §4 *storage* (relpath was paper-only
  there); does not replace ADR-0029
- Gate: Gate 3 — remount / Docker bind-path moves must not wipe probe,
  subtitle extract, or metadata bindings
- Related: ADR-0013 (`{DATA}/subs/{itemId}/`); OpenAPI `MediaItem.path`;
  ADR-0003 (no auth in v0 — Phase 3)

## Context

ADR-0025 §4 chose `path:{library_id}:{relpath}` as the remount-stable handle
for unmatched watch state: `/Volumes/media/...` and `/mnt/user/media/...`
are the same library after a remount. `libraries.path` is that root.
`media_items.path` and `media_item_sidecars.path` are absolute today, so the
identity design and the file table disagree.

No prefix-strip exists in the product. Relpath is paper-only in §4 — there
is no derivation-bug class to migrate, only storage to change.
`media_item_links` (ADR-0029) stores provider keys only; derived path keys
do not live in the join, so this migration does not touch that table.

Docker makes remount ordinary: host vs container bind paths, and moves
between Unraid / Synology / TrueNAS. Today there is no `PATCH /libraries`
— changing the root means delete and re-add → new `library_id` → lose
everything. Relpath without a repoint operation does not pay.

`year_from_show_folder` walks two parents; the only layout whose answer
changes under relpath is one-show-per-library-root (pinned in tests;
ADR-0029 out of scope). Normal `library/Show/Season/file` is identical
under both forms. This ADR does not fix `series_library_year`.

**Numbering.** Highest ADR on this branch is 0029; on `main`, 0028.
**0027 remains artwork.** This document is **0030**. Completes ADR-0025 §4
storage. No migration number reserved here — the implementing slice takes
the next free after 011 (or whatever tip `#31` / 0029 landed).

## Decision

### 1. Relative storage on `media_items` and `media_item_sidecars`

Both tables store paths **relative to `libraries.path`** for that row's
`library_id`. Uniqueness stays `(library_id, path)`.

**Canonical form (one form — two forms mean duplicate rows):**

| Rule | Form |
|---|---|
| Separator | `/` always |
| Leading slash | **None** — `Season 1/ep.mkv`, not `/Season 1/ep.mkv` |
| `.` / `..` segments | **Forbidden** after normalisation |
| Empty path | **Forbidden** (a media file is never the library root itself) |
| Library root `libraries.path` | Absolute POSIX path; **no trailing slash** (strip on write). Root `/` alone is rejected for a media library |
| Unicode | Store the path as returned by the walk after normalisation; no NFC/NFD forced in v1 |
| Windows | **Out of scope for v1.** Drive letters, drive-relative forms (`C:foo`), and UNC (`\\server\share`) are not valid library roots or relpaths. `require_relpath` / `require_library_root` reject them on every host so a future Windows build cannot store them as "relative." |

**Normalisation on every write (scan upsert, sidecar replace, migration):**

1. Interpret the filesystem path against the library root.
2. Require the file is **under** the root (see below).
3. Strip the root prefix; replace `\` with `/`; strip a single leading `/`
   if present; reject if the result is empty or contains a `..` segment.

**Not under the root** (symlink escape, bind-mount overlap that places the
inode outside the configured root, path that fails prefix check):

- **Do not index** that path. Do not store `../…`, absolute leftovers, or a
  second root.
- **Visible, not log-only** (ADR-0014 lesson): the scan job and the library
  record surface an integer `skipped_outside_root` (name illustrative) for
  that pass / rolling count. `nightjar doctor` already leads with library
  reachability — it reports this count next to it. Symlinked media trees
  are common on Unraid and Synology; "files vanished" must be diagnosable
  without reading server logs.

Sidecars use the **same** rules in the **same** migration. A sidecar
beside a video becomes e.g. `Show/Season 1/ep.en.srt` under the same
library root.

**One column, in place (Rule 4.11 / 4.5).** There is no second path column.
Migration `UPDATE`s `media_items.path` / `media_item_sidecars.path` to the
relpath when the strip succeeds; the absolute bytes are not retained on that
row. Rows that cannot strip keep their existing absolute string in the **same**
column and increment `paths_unresolved` (§5) until repair. That mixed
population is transitional failure state, not a dual-representation design.

**On-disk open path** — one helper, every call site (Rule 4.11). Resolve with
a single function: absolute stored values (unresolved leftovers) used as-is;
else `join(libraries.path, stored)`. No scattered `if` at probe / extract /
playback / doctor — one path through that helper.

**Absolute vs relative discrimination (Rule 4.9).** No marker column. The
on-disk form is discriminated by `std::path::Path::is_absolute(stored)` on
the server host (`is_absolute_stored` / `resolve_media_path`). That is the
locked shape rule for the transitional mixed population and afterwards:

- Legal relpaths are definitionally **not** absolute under that predicate.
  **Enforcement is at the write boundary:** `require_relpath` runs in
  `upsert_items_indexed`, `replace_item_sidecars`, and inside `to_relpath`
  before any new relpath is returned — not by trusting call sites. Migration
  may still leave absolute leftovers; those bypass `require_relpath` on
  purpose until repair strips them.
- Library root `/` is rejected via `require_library_root`.
- When `paths_unresolved` is zero, every row is relpath by construction; the
  predicate still holds as the writer invariant.

A separate `path_form` column was rejected: it would duplicate a property
already constrained by the relpath grammar, and the mixed state is meant to
drain to zero via repair, not become a permanent dual enum.

### 2. Case sensitivity is identity

SQLite `UNIQUE (library_id, path)` uses **BINARY** collation (SQLite's
default for `TEXT`) — case-sensitive, byte-identical. That is the stored
identity. Do not switch the unique index to `NOCASE`: NFC/case-fold rules
differ across filesystems, and `NOCASE` is ASCII-only in SQLite.

**Rescan on a case-folding filesystem** (SMB, case-insensitive APFS): the
walk may return a path that differs only by case from an existing row.
Treating that as a new path under BINARY uniqueness deletes the old row
via `delete_missing` and inserts a new `media_items.id` — the remount-
class wipe this ADR exists to stop.

**Match rule:** when matching a walked path to an existing row in that
library, compare with **Unicode case-fold** of each path segment,
`/`-joined. If fold-equal to exactly one existing row, treat as that row.
If fold-equal to two+ rows (corrupt DB from a prior case-sensitive host),
refuse the upsert for that path and surface a count — operator cleans
duplicates; do not pick silently.

**Spelling is sticky — no case-only rewrite.** On fold-equal match, do
**not** `UPDATE media_items.path` (or sidecar paths) to the walked spelling.
First-indexed spelling wins. `delete_missing` / keep-set membership is
fold-aware so a folding walk does not drop the sticky-spelling row.

**Stored spelling may disagree with `ls` on folding hosts.** That is
deliberate: creation-time (or migration-time) spelling is not refreshed.
The filesystem folds on open, so playback/probe still work. String
compare of DB path to walk/`ls` output is not a bug signal — use fold
equality or open, not byte equality, when diagnosing.

**Why not rewrite (and why that is safe):** a fold-aware dry-run (§3) can
match ~100% while every walked spelling differs from storage (case-
sensitive host → case-folding mount). Rewriting would be a mass `UPDATE`
of the identity column on an ordinary scan — no migration transaction,
no `COUNT(*)` guard, and it would change derived
`path:{library_id}:{relpath}` strings for unmatched items (future watch
state) without the ADR-0025 migrator. Sticky spelling avoids that class
entirely and keeps `path:{library_id}:{relpath}` stable across a
case-folding remount — the point of ADR-0025 §4. A true case rename on a
case-sensitive host is a different directory entry and follows normal
remove/add.

Case-sensitive hosts (typical Linux ext4): fold-equal implies byte-equal
for the usual ASCII media layout; behaviour matches today's unique key.

### 3. `PATCH /libraries/{libraryId}` — async, with wipe guard

Additive OpenAPI operation (v0): update `name` and/or `path` (root). Does
not change `library_id`. A path change is a **repoint**.

**Async (ADR-0015 §2).** A cold walk after remount is minutes (dogfood TV
serial ~686–729 s; parallel ~150 s). `POST /libraries` already returns
`201`/`202` with a job id so the HTTP request does not block on that.
`PATCH` that changes `path` must do the same: accept the patch, return
**202** with a job id, run dry-run + commit decision + enqueue scan on the
job. Name-only PATCH (no path change) may stay synchronous `200`.

The dry-run is a full walk of the candidate root; the successful path then
enqueues a normal scan — **two walks**. That cost is paid on the job, not
the request. Document in the operation description that a path PATCH
schedules work of scan-class duration.

**Guard (named, not left to implementers):** the job walks the candidate
root (same walk code as scan; reachability must be positive — ADR-0014).
Compute the set of canonical relpaths the walk would store. Against
current `media_items` for that `library_id` (fold-aware match per §2):

| Condition | Result |
|---|---|
| Library has ≥1 media row and dry-run matches **0** existing relpaths | **Refuse** — reachable-but-wrong / empty mount |
| Matched existing rows / current row count **< 0.90** | **Refuse** — more than 10% of the library would fall off the new root |
| Otherwise | Commit `libraries.path`, then enqueue a normal scan |

Refusal leaves `libraries.path` unchanged. Job (or response body on the
job resource) reports `current`, `walked`, `matched`, `would_remove`, and
a stable reason string
(`repoint_empty_match` | `repoint_below_retain_threshold`).

**Why not rely on ADR-0014 §2 alone:** §2 skips `delete_missing` when
reachability is in *doubt*. A typo root that is reachable and walks empty
*successfully* is not doubt — today that wipe still fires. The retain
threshold is the repoint-specific guard; ordinary scans keep §2.

**Retain fraction 0.90** is a **default judgement**, not a measured floor.
It was **not** run against the ~24 800-item dogfood library before acceptance;
it was picked to catch wrong roots while allowing small tree churn. A dry-run
at **0.89** (or any `matched/current < 0.90`) **refuses** — it does not
repoint and drop 11%. Job error prefix: `repoint_below_retain_threshold`.
**Revisit trigger:** dogfood remount evidence that real keep-relpath remounts
routinely land under 0.90 without being a wrong root, or that 0.90 still
admits destructive mis-points. Not a setting.

**Small libraries:** with fewer than 10 items, losing a single row is
already >10%, so the threshold is effectively **all-or-nothing** (any
unmatched existing row fails the retain check once `would_remove /
current > 0.10`). Empty-match refusal when `current ≥ 1` remains absolute.
That is accepted for v1 — tiny libraries are cheap to fix by correcting
the path; they are not a reason to loosen the guard for 24k-row libraries.

**No force-repoint in v1.** If the tree genuinely changed by more than
10% of relpaths and the operator still wants the same `library_id`, v1
provides **no** escape hatch (no `?force=`, no confirm token). The only
path is delete-and-re-add, which creates a new `library_id` and loses
probe, extracts, and bindings — said plainly (Rule 4.8). A deliberate
force flag is a later ADR if support demand appears.

**Auth (Phase 3 marker).** `PATCH` mutates the library root. Phase 1–2 have
no auth (ADR-0003 §3); the route is public like every other write today.
**Phase 3: admin-only.** Leaving it public after accounts land would let any
authenticated household member repoint (or refuse-loop) libraries. Same
class as ADR-0009's capability readout — call out now so the security pass
cannot miss it.

**First index after repoint defers `delete_missing`.** The refuse bar (≥0.90
match) stops the catastrophic wrong-root case. It does **not** stop a quiet
~9% miss: retain passes, then a normal scan would `delete_missing` the
unmatched rows under a different mechanism. That wipe still destroys
`media_items.id` (probe, extracts, sidecars, `media_item_links`) and would
orphan future `watch_state` rows that key on path `item_key` until re-match
— today watch_state is not shipped, but the extract/binding loss is already
real. Therefore: on a job with `kind=repoint`, the continuing index
**reports** unmatched count as `deferred_remove` and **does not** call
`delete_missing`. The next ordinary scan (`kind=scan`) deletes as usual.
Operators (and later UI/doctor) can see the delta before the second pass.
This makes the 0.90 default less load-bearing for quiet partial trees.
`deferred_remove` is a **per-job counter** on the repoint job row (not a
sticky library flag); unmatched rows remain until the next `kind=scan`
runs `delete_missing` — that scan is the only clear.

**Poll holdoff after deferred_remove > 0 (Gate 3 dogfood).** When the
repoint index records `deferred_remove > 0`, the process arms an in-memory
holdoff (default **1 hour**): **poll** skips starting a full walk for that
library so automatic discovery cannot apply the deferred deletes before
review. **Manual `POST .../scan` is still allowed** (operator explicit).
Holdoff clears when a successful ordinary scan completes, or when the
duration expires (process restart also drops the holdoff — one delete scan
may run; acceptable for v1). A second repoint before that scan defers
again and records a new `deferred_remove` on the new job; still no delete
until a `kind=scan`.

**One cold walk for retain + commit index.** The dry-run readdir that
computes retain also supplies the file list for the post-commit index in
the same epoch. A second full readdir is not required when that list is
reused. The dry-run populates `WalkCache` under the **new** absolute root
so the next poll is warm (old absolute keys for the previous root are
replaced, not left stale).

### 4. What a successful repoint preserves; API `path`

**Preserved** (because `media_items.id` and stored relpaths do not change):

- All `media_items` columns (probe, subtitle_status, content_id,
  usable_extent, map status, metadata_status, …)
- `keyframe_map_entries` and related map identity columns
- `{NIGHTJAR_DATA_DIR}/subs/{itemId}/` extracts (keyed on id)
- `media_item_sidecars` rows (relpath form; still under the same logical
  tree after remount if the tree moved with the root)
- `media_item_links` and `metadata_canonical` / raw payloads (ADR-0029)
- Derived `path:{library_id}:{relpath}` item keys (relpath spelling sticky)

**Changes:** `libraries.path` only (and library `name` if patched).

**OpenAPI `MediaItem.path`:** remains a required string and remains
**absolute in the response** — reconstructed as
`libraries.path` *(at response time)* + `/` + stored relpath. Wire meaning
is unchanged: absolute filesystem path of the media file (Rule 2.3 for v0
clients that already consume absolute paths). It is **not a stable
identifier** across repoint; clients that need stability use `id` or
(later) `item_key`. Document that in the schema description. Request bodies
that accept paths (if any) are out of scope for v0 create-library (still
takes the root only).

### 5. Migration shape (constraints only)

Design constraints for the implementing slice — not a reserved version
number:

- `UPDATE` in place; **no** table rebuild. Assert `COUNT(*)` before == after
  for `media_items` and `media_item_sidecars` inside the migration
  transaction (ADR-0014 §5).
- Normalise `libraries.path` trailing slashes in the same transaction.
- Does not rewrite `media_item_links`.

**Non-stripping rows — do not abort the migration.** Anyone who already
moved a root and re-added may have absolute paths that do not prefix-
match the current `libraries.path`. Aborting leaves a server that will
not start, while the fix (`PATCH`) is delivered by the same upgrade —
circular.

Instead:

1. Strip every row that strips cleanly under §1.
2. Leave non-stripping rows' `path` values unchanged (still absolute) and
   count them per library.
3. Persist that count on the library (column or equivalent) so it is
   **visible**: API library resource + `nightjar doctor`. Server starts.
4. Scan must not treat an unresolved absolute row as a missing relative
   path (`delete_missing` / keep-set aware of unresolved rows).
5. After the operator corrects `libraries.path` via PATCH (or edits the
   root to the prefix those paths share), a **repair** pass (part of the
   implementing slice — may ride the post-repoint scan) strips remaining
   rows that now match. Until the unresolved count is zero, doctor keeps
   failing that check.

Silent wrong relpaths are rejected; silent migration abort is also
rejected.

## Out of scope

- Fixing `series_library_year` / `year_from_show_folder` for
  one-show-per-library-root (noted in ADR-0029; pin path form in repros).
- DELETE library API; watch-state migrator; artwork.
- NFC normalisation beyond what the walk returns.
- Changing ADR-0014 empty-walk-under-doubt rules for ordinary scans.
- Force-repoint / confirm token for >10% tree replacement (v1: absent).
- Season enqueue / episode bind proof (ADR-0029).

## Alternatives considered

**Keep absolute paths; strip at `path_item_key` derivation time.**
Rejected: strip is the bug class §4 already refused for watch keys;
repoint still churns `(library_id, path)` uniqueness and deletes rows.

**Store relpath but skip PATCH (repoint later).** Rejected: relpath does
not pay until repoint exists; delete-and-re-add remains the only move.

**`NOCASE` unique index.** Rejected: ASCII-only, wrong for non-ASCII
titles; hides duplicates that a case-sensitive host would keep.

**Rewrite stored spelling to the walked form on fold-equal match.**
Rejected: unbounded mass `UPDATE` of the identity column on ordinary
scan after a case-sensitive→folding remount; orphans derived path keys;
conflicts with ADR-0014 §5's care around 24k-row identity churn. Sticky
spelling (§2) is the chosen escape.

**Bound spelling rewrites (refuse if >10% would change) instead of sticky.**
Rejected as unnecessary once sticky spelling is chosen; a bound still
allows large rewrites below the cap and still moves path keys.

**Refuse repoint only when dry-run walk is empty.** Rejected: a wrong
mount that lists *some other* tree can match <90% and still destroy most
of the library; retain fraction covers that.

**Soft-delete / quarantine rows that miss the new root instead of
refusing.** Rejected for v1: silent quarantine is data loss wearing a
different costume; refuse and let the operator fix the path.

**Synchronous PATCH that dry-runs in the request.** Rejected: two cold
walks are scan-class duration (ADR-0015 §2); the request would time out
on a real NAS. Async job, same as create-library.

**Abort migration if any path will not strip.** Rejected: circular with
PATCH; leaves no working binary on the machines that already moved roots.

**Relative `MediaItem.path` in the API.** Rejected for v0: breaks any
client displaying or opening paths; absolute-at-response-time is enough
if documented as unstable across repoint.

## Consequences

- Scanner upsert and sidecar association write canonical relpaths;
  fold-aware keep-set; sticky spelling (DB may disagree with `ls`).
- One absolute-path helper for mixed absolute/relpath rows — used
  everywhere a file is opened or shown as filesystem path.
- Implementing slice: migration with visible unresolved-path counts +
  repair; async path PATCH + dry-run retain guard; OpenAPI description on
  `MediaItem.path`; `skipped_outside_root` on scan/library API.
- **User-visible (ASS pattern):** `skipped_outside_root`,
  `paths_unresolved`, and repoint `deferred_remove` are on the library /
  scan-job JSON. There is no web UI surface yet — the server knows; the
  household user does not unless a client shows the counters or
  `nightjar doctor` (Phase 4 plan) does. Failed repoint drops **zero**
  items (refuse). Successful repoint's first index also drops **zero**;
  unmatched count is `deferred_remove` until the next ordinary scan.
- ADR-0025 §4 storage and grammar finally agree; derived path keys use
  the stored (sticky) relpath column.
- Migration dry-run numbers: `notes/migration-012-dogfood-2026-08-03.md`
  (24940/8583, zero leftovers). Gate 3 remount / Docker path-move
  verification against the live dogfood library remains outstanding.
- v1 has no force-repoint: operators whose tree genuinely changed by more
  than 10% of relpaths must delete-and-re-add (new `library_id`, lose
  derived state) or wait for a later ADR.
