# ADR-0028: Manual metadata fix

- Status: accepted
- Date: 2026-08-02
- Amended: 2026-08-13 — §7, the wrong-bind class this flow owns, and why it
  has no automatic fix
- Depends on: ADR-0025 (§5 migrator); ADR-0026 (confidence floor, negative-
  result cache, payloads, collections storage §6)
- Gate: Gate 3 — every mismatch fixable in-UI in under 30 seconds
- Related: Phase 3 Block 1; kids
  scoping prefers fix over allowlist for wrong matches

## Context

Auto-match at confidence ≥ 0.80 (ADR-0026) will leave unmatched items and
will occasionally need human correction when a high-confidence hit is still
wrong. Gate 3 requires that correction in under thirty seconds. ADR-0025
already locked the migrator and merge rule for `item_key` changes; this ADR
owns the API that calls it, what durable state a manual assign leaves behind,
and the side effects (collection clear, artwork invalidate, negative-cache
bust).

UI chrome is Block 3. This document locks the server contract.

## Decision

### 1. Four operations

Additive `/api/v0` routes (exact paths frozen in OpenAPI when implemented):

1. **Search candidates** for a media file or logical item (title/year query
   against TMDB search; returns candidate ids and display fields).
2. **Assign** a provider id (movie id, or TV series id; episodes follow
   season+episode under that series after series assign, using season append
   per ADR-0025).
3. **Clear match.** Drop the provider key, return to the path `item_key`,
   clear collection fields, clear the manual-matched flag (§3).
4. **Retry unmatched.** Bust the negative-result cache row for that
   `query_key` (ADR-0026 §3) and re-queue automatic matching.

### 2. Assign is one operation

Assign, in one server-side path:

1. Write the provider `item_key` and project canonical metadata from detail
   payloads (fetch if missing; persist raw payload per ADR-0026 §4).
2. Run the **one migrator** from ADR-0025 §5 (watch state and, when present,
   playback events).
3. Clear stale `belongs_to_collection` id/name (ADR-0026 §6); rewrite from
   the new detail when present.
4. Invalidate artwork keyed under the old `item_key` and enqueue artwork for
   the new key when the artwork pipeline exists.

Shipping assign without the migrator is a bug. Soft-deleting watch rows
instead of migrating is rejected. Clients do not rematch locally.

### 3. Manual-matched flag

Assign sets a durable `manually_matched` flag on the logical item (or the
media-row binding that owns match state: one column, one meaning). The flag
survives refresh, rescan, and change-list updates. While it is set, automatic
matching must not re-evaluate the item against the 0.80 floor or overwrite
the provider id. Manual outranks every automatic detector and is never
overwritten by a later pass (same rule the media-segments design reached for
human overrides).

Clear-match clears the flag. Retry unmatched does not set it; only assign
does.

### 4. Force below the confidence floor

Assign is an explicit user choice. The 0.80 floor does not apply. The raw
provider payload is still persisted. A human may pick a candidate the
automatic matcher would have left unmatched.

### 5. Artwork keying handoff

Artwork will key on `item_key` + kind, so every file sharing a logical item
shares artwork. That is deliberate: one item, one poster. Tradeoff: the 4K
and 1080p copies of a film cannot carry different posters. Recorded here so
it is not reported as a bug when ADR-0027 lands. Key change on assign
invalidates the old key's artwork and enqueues the new.

### 6. Auth

Pre-accounts: same local-trust as the rest of `/api/v0`. When accounts and
profiles land (Block 2), these operations are admin-only. Do not invent a
second permission model here.

### 7. The wrong-bind class this flow owns

A folder can be bound to the wrong provider entity with high confidence and
stay that way through every improvement to candidate selection. **Measured on
the dogfood library, three folders are in this state**, and the fix flow owns
them because no automatic correction exists:

| folder | bound | correct entity | files |
|---|---|---|---:|
| Battlestar Galactica (2003) | 71365 (the 2003 miniseries) | 1972 (the 2004 series) | 72 |
| What If…! | 66565 | 91363 `What If...?` | 26 |
| The Haunting | 231907 | 72844 `The Haunting of Hill House` | 10 |

A fourth, `The Continental (2023)`, was in the class and is **already fixed**:
its stored entity reports zero episodes, which ADR-0026's 2026-08-13 exclusion
discards, so it re-binds on the next rescan with no operator action.

**Why there is no automatic fix, and both halves are structural.**

*The evidence is not available where the decision is made.* The stored-id bind
path may read persisted payloads only (ADR-0033 §8). An episode list is
persisted **only when a folder binds to that entity** — so a wrong bind means
the correct entity was never bound and its list was never stored. Measured
across 723 bound folders: of 942 non-bound title-hit candidates, 48 have a
persisted episode list, and every one of those 48 is an entity some *other*
folder is bound to. For all four folders above, the correct entity has nothing
stored at all. The evidence a rebind would need is unavailable by construction,
not by oversight.

*And the locally available half cannot tell a wrong id from a stale filename.*
Both produce identical evidence: a stored episode list that none of the
folder's titles matches. `Vanished (2026)` is the proof — its stored id 270857
is **correct**, its filenames were stale, and a predicate built on the local
half fires on it and would move it to 287231, breaking a correct binding. A
stale-filename detector was measured as the prerequisite that would separate
the two and does not: it sees a *renumbered* filename (the title still exists
on the show at another number) and cannot see a *renamed* one (the title exists
nowhere on the show), and the renamed kind is exactly the failing case.

**So the class is surfaced as low confidence and corrected by an operator
through the four operations above.** That is what this flow is for.

Two facts recorded with it, because they bound what any future automatic
attempt could achieve:

- **Two of the three would not auto-bind even with the evidence.** On the
  fall-through search their correct entities score 0.72
  (`exact_title_collision_unpinned`) — below the 0.80 floor. Only Battlestar
  clears it.
- **`series_key` cannot survive a rebind.** It is `tmdb:show:{id}`, derived
  from the show id (`item_links::series_key_for_show_folder`), so any rebind
  restates the folder's identity to every consumer keyed on it, including
  existing `media_item_links` rows and any bookmarked browse-unit URL. An
  earlier plan asserted the key should not change because one folder is one
  show to the user; that is right about the user and wrong about the key.

## Alternatives considered

**Assign without running the migrator.** Rejected: orphans watch state onto
the wrong item; contradicts ADR-0025 and Gate 3 survival.

**Soft-delete watch rows on reassignment.** Rejected: loses resume the Gate
exists to keep.

**Per-client rematch.** Rejected: Rule 2.1; match state is server-side.

**Re-run automatic match after assign without a manual flag.** Rejected:
refresh or rescan would unmatch a human decision against the 0.80 floor.

**Defer collection clear until Block 3 UI.** Rejected: stale collection
linkage after reassignment is a data bug, not a presentation bug.

## Consequences

- Writers implement the four operations behind `/api/v0`; OpenAPI is the
  source of truth for path and body shapes.
- The `manually_matched` column (or equivalent) is schema under Rule 4.9;
  ship it with the fix API, not as a later patch after the first wrong
  rematch.
- **Auth blast radius.** Pre-accounts local-trust is honest for now, but
  assign rewrites watch state for every profile that held the old or new
  `item_key`. That is a larger blast radius than anything else currently on
  `/api/v0`. Block 2 accounts work must treat this endpoint as the first
  that leaves the open surface. Admin-only is not optional polish.
- Kids allowlisting a mismatched title is the wrong repair; the fix flow
  is preferred (Phase 3 kids prose).
- **§7's class is a standing operator obligation, not a backlog item.** It does
  not shrink as matching improves, because the failure is in what the bind site
  can see rather than in how well it chooses. Any future proposal to automate
  it must first answer the `Vanished (2026)` case, and a fetch at the bind site
  does not: it buys the missing claimant evidence and leaves the wrong-id /
  stale-filename ambiguity exactly where it was.
- ADR-0027 (artwork) consumes the invalidate/enqueue obligation; it does
  not redefine artwork identity. Number 0027 is reserved for that ADR;
  write it immediately before the artwork slice once real payloads exist to
  measure thumbnail disk against.
- Gate 3 needs an observed fix under thirty seconds on a real mismatch, not
  only a unit test of the migrator.
