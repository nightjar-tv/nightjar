# ADR-0033: Durable series identity

- Status: accepted
- Date: 2026-08-05
- Amended: 2026-08-13 — item 8: what the zero-request property costs, and why
  the cross-check refutes rather than corrects
- Depends on: ADR-0025 (item identity / path keys §4); ADR-0026 (§3
  negative cache, §8.1/§8.4 two-tier, §8.6 Visible proxy, §8.10 cascade);
  ADR-0028 (manual fix); ADR-0029 (detail payloads); ADR-0030 (library-
  relative paths)
- Gate: Gate 3 — rescan of an unchanged library generates no search
  requests; every mismatch fixable in-UI in under 30 seconds
- Related: Block 1 recovery slice RC7; decision sheet
  the ADR-0033 decision sheet, 2026-08-05

## Context

Series identity is scoped to the resolve group, which keys on the
library-global folded soft title: `clean_show_title` folds `Shameless (US)`
and `Shameless (UK)` to one key and group formation keys on the yearless
folded title (`queue.rs:600-604`). Two folders that fold to the same key
land in **one** group and share identity, so the second folder can inherit
the first's id with no check — the D2 wrong-match class. The abandoned
`metadata/s1-s2-status-and-series-cascade` branch papered over this with an
in-memory cache seeded off per-file links; that is per-drain re-derivation
and a second identity author (Rule 4.11). This ADR decides the durable,
folder-scoped shape before any code (Rules 4.9, 6.1).

## Decision

**A show folder is the unit of durable series identity, persisted as a
folder-keyed series row; two folders never merge by fold collision.**

1. **Persisted folder-keyed row (Q1).** A stored `series` table keyed on the
   show folder. Identity is read from the stored shape, not re-derived from
   per-file links on each drain; the link-derived substitute is rejected
   (plan Decision 5).
2. **Folder scope and merge rule (Q2).** The show folder is the highest
   directory under the library root that contains episodes or season
   directories. `Season N/` and `Specials/` inherit the show folder's series
   row. Sibling folders merge **only by an explicit recorded rule**, never by
   fold collision. This is the D2 regression test RC8 must pass: two
   fold-colliding folders resolve to different series rows.
3. **Identity key is the folder path (Q3).** Key on `(library_id, relpath)`,
   matching the ADR-0025 §4 / ADR-0030 path shape. The title fold stays in
   the matcher and is matching-only; it never merges two folders into one
   series row. A folder rename re-derives the series row under a new key.
   That is acceptable only because Q6 keeps the row non-watch: watch state
   lives on the per-file `item_key`, so a rename orphans no watch state the
   way it would for a watch key.
4. **Negative cache keys on the series id once known (Q4).** A folder with
   identity caches under its series id; title+year keys remain for folders
   with no identity yet. `cleaner_version` keeps its existing job
   (invalidating title-fold re-keyings), unchanged.
5. **Migration 016 retro-derives existing rows (Q5).** One-shot migration
   derives series rows for the ~23k already-`ready` episodes. Derivation is
   a path walk, not a re-match: those rows already carry `tmdb:episode:`
   links, so folder grouping needs no provider call. New-content-only (B)
   would leave the two-tier state this recovery exists to end.
6. **Series row is a grouping handle, not a watch key (Q6).** It never
   becomes a watch `item_key`; `tmdb:show:` stays non-watch
   (`item_links::is_watch_item_key`). ADR-0025 is unchanged.
7. **Match state stays per-file (Q7).** Storage keeps per-file `item_key`,
   `metadata_status`, and `manually_matched`, so match state has one write
   path and `manually_matched` keeps one meaning (Rule 4.11). ADR-0028's
   existing series assign becomes an **API-level fan-out**: the assign
   endpoint targets the series id and the server applies the existing
   per-file assign path to every file under the folder. This keeps the
   30-second Gate 3 fix without a second identity author.
8. **Inherited identity is cross-checked, not blindly trusted (Q8).** A
   known series id skips the title search but must pass a folder-level
   name/year cross-check before binding. The cross-check reads the
   **already-persisted detail payload** (ADR-0029) — a local read, not a
   provider re-fetch. A rescan of an unchanged library therefore issues zero
   requests and the Gate 3 "no search requests" property survives; the cost
   is paid only when the stored name/year disagrees with the folder, which is
   exactly the `Shameless (UK)/(US)` case. "Cross-check" here means "compare
   against the stored payload," never "re-fetch."

   **What the zero-request property costs, recorded 2026-08-13.** The
   cross-check can only ever ask questions about the *stored* entity, and that
   bounds it to refuting a binding, never to correcting one. An episode list is
   persisted only when a folder binds to that entity, so the entity a wrong
   binding should have chosen has nothing stored — measured across 723 folders,
   of 942 non-bound title-hit candidates only 48 have a persisted episode list
   and **every one of the 48 is an entity some other folder is bound to**. A
   correction therefore cannot be computed here without fetching, and fetching
   is what this item exists to avoid.

   Worse, the half that *is* computable does not carry the meaning it appears
   to. "The stored episode list matches none of the folder's titles" is
   produced identically by a wrong stored id and by filenames that went stale
   after the provider corrected its metadata — and by an entity that carries no
   episode identity at all (ADR-0026 §2). Acting on it would break correct
   bindings; the measured instance is `Vanished (2026)`, whose stored id is
   right and whose filenames were stale.

   **So this item's zero-request property is not a limitation to be engineered
   around; it is the reason the wrong-bind class belongs to the operator fix
   flow (ADR-0028 §7) rather than to the resolver.** The 2026-08-13
   zero-episode discard is the shape of correction that *is* available here: a
   property of the stored payload alone, needing no comparison with any other
   entity.

## Consequences

- A folder rename re-derives the series row and drops folder-level state
  that outlives the path — safe only because Q6 keeps the row non-watch.
- Migration 016 runs once and must be idempotent at the SQL level; nothing
  re-derives series rows inside `drain_pending` (plan Decision 6).
- The fix API gains a fan-out path while the per-file write path stays
  single (Rule 4.11); OpenAPI changes when the fan-out ships (Rule 4.9).
- Search suppression for identified folders is bounded by the cross-check: a
  folder whose stored detail disagrees falls through to search rather than
  binding a wrong id (plan Decision 3).
- **The cross-check refutes; it does not correct.** Everything it can read is a
  property of the stored entity, so it can discard an id and fall through to
  search, and it can never establish that a *different* entity is the right one.
  Item 8's 2026-08-13 note records why, and ADR-0028 §7 owns what is left over.
- Group formation moved from the folded title to the folder scope, and the
  Visible proxy (§8.6) and the negative cache re-keyed with it in RC8.
- Accepted 2026-08-05 with the human sign-off on all eight questions
  recorded in the ADR-0033 decision sheet, 2026-08-05
  before RC8 started; every answer was option A. RC8 implemented this shape
  (migration 016 + folder-scoped group formation + one hit path) and deleted
  anything resembling the branch's `series_cache`; the identifier `series_cache`
  is absent from the shipped tree.
