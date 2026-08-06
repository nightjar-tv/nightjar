# ADR-0035: Watch state and continue-watching

- Status: proposed
- Date: 2026-08-06
- Blocked on: the series ADR, whose folder-scoped series rows change what item 8
  can compute for unmatched episodes; and the three-role model, which item 7's
  ownership rule reads. Cannot be accepted before both are on disk
- Depends on: ADR-0034 (profiles, `profileRef`, deletion cascade); ADR-0025 §1
  (`item_key`), §3 (lifecycle), §4 (path keys), §5 (key-change merge rule);
  ADR-0033 (series identity, extended by the series ADR); ADR-0007 / ADR-0011
  (playback sessions, the progress reporter)
- Gate: Gate 3 — resume works across devices and survives server restarts;
  watch history survives rename, re-encode, and library remove-and-re-add;
  full v1 API frozen
- Related: Block 2 plan B2-B and B2-3 / B2-4
  (`nightjar-meta/docs/BLOCK_2_PLAN.md`); ADR-0036 playback events, which is a
  different table for a different question

## Context

ADR-0025 decided what a watch row points at and left what it holds to this
record. Everything since has been written against a table that does not exist:
the §5 migrator is real merge code that no-ops because there is nothing to
merge, ADR-0028's assign endpoint promises to carry watch state across a
provider-id change, and ADR-0034 gave the row an owner by deciding that a
profile is the only thing that can write one.

Two forces set the shape. Resume is the feature a household notices when it
breaks, so the row optimises for surviving identity changes rather than for
recording history faithfully. And the continue-watching rail is a rollup over
episodes that three clients would otherwise each compute differently, so the
collapse rule has to live on the server or it will drift the first week two
clients ship.

## Decision

**One mutable row per (profile, `item_key`), and one server-computed rollup
over it. There is no series-level state row and no play count.**

1. **The row.** `watch_state` holds `profile_id`, `item_key`, `position_ms`,
   `duration_ms`, `played`, `hidden`, `first_played_at`, `last_played_at`.
   Primary key `(profile_id, item_key)`, with an index on
   `(profile_id, last_played_at DESC)` because every read is the rail asking for
   a profile's recent items in order. `profile_id` cascades on profile deletion
   per ADR-0034 item 7.

   `duration_ms` is a snapshot taken from the file that was playing, not a
   foreign key to the current file. It is there because the thresholds below and
   the ADR-0025 §5 merge rule are both ratios, and a re-encode that changes
   duration must not retroactively move an old row across a threshold.

2. **Two thresholds, both constants, neither a setting** (Rule 4.12). Below 2%
   of duration no resume point is kept, so a title opened by accident does not
   land on the rail. At 90% the item is marked played and drops off the rail.
   Both are `const` in `nightjar-core` with table tests at the boundary, and the
   test asserts the behaviour at 1.9%, 2.1%, 89.9% and 90.1% rather than the
   values themselves.

   A setting here would be the exact failure Rule 4.12 names. The honest reason
   somebody wants one is that 90% is wrong for a title with eight minutes of
   credits, and the fix for that is reading the credits offset when we have one,
   not asking the user for a percentage.

3. **The server clock stamps every write, and concurrent writes are
   last-write-wins.** Highest-position-wins was considered and rejected: it
   breaks starting a title over on the television while the phone still has the
   old session open, which is the case a household hits and a rule optimising
   for "never lose progress" gets wrong. Client-supplied timestamps are ignored
   rather than trusted, because a television with a bad clock would otherwise
   pin the rail order permanently.

4. **No `play_count`.** State answers where you are, not how many times you have
   been there. A rewatch overwrites; the history question belongs to ADR-0036,
   which records intervals and can answer it properly.

5. **Progress is reported to one route and written by one writer.** The client
   reports position every 10 seconds of playback, and on pause, on seek
   completion, on track change, and at session end. The server writes the watch
   row and, in the same call, hands the report to the ADR-0036 interval writer,
   so there is one progress path and not two (Rule 4.11).

   Ten seconds is a constant with a reason rather than a preference: it is the
   worst-case position loss when a client dies without a final report, and ten
   seconds of a rewatched scene is below what a viewer notices on resume. It is
   not a setting for the same reason as item 2.

6. **Routes are profile-scoped in the path, and the item key is a query
   parameter.** `PUT /api/v0/profiles/{profileRef}/watch-state?itemKey=…` writes,
   `GET /api/v0/profiles/{profileRef}/watch-state?itemKey=…` reads one row, and
   `GET /api/v0/profiles/{profileRef}/continue-watching` reads the rail.

   The ref is in the path rather than inferred from the session because an
   account holder reading a child's history is a real case (ADR-0037 item 12) and
   it needs a route that names whose history it is. `profileRef`, never
   `profileId`, per ADR-0034 item 6.

   **The key is not in a path segment, and that is a correctness decision rather
   than a style one.** An unmatched item's key is `path:{library_id}:{relpath}`
   (ADR-0025 §4), which contains slashes and spaces. `%2F` inside a path segment
   is rejected or silently normalised by default in both nginx and Axum's path
   extractor, so a path-segment key would break for exactly the below-floor
   fraction of the library these ADRs take care to support, and it would break
   after the route shape is frozen. In a query parameter, percent-encoding is
   ordinary and no proxy rewrites it. Base64url in the path was the alternative
   and is rejected because it gives one key two wire representations, raw in
   bodies and encoded in URLs, which is the fork Rule 4.11 asks us not to create
   for a formatting problem.

7. **Which `profileRef` a caller may address is a rule, not an assumption.** A
   profile-scope session may address only its own ref. From account scope, the
   answer is the three-role model: an owner or a manager may address any profile
   on the server, and a member may address the profiles under their own account
   and no others. Anything else is the named forbidden error rather than a 404,
   because a 404 leaks whether a ref exists and the refs are guessable only if we
   make them so.

   This is written down because the earlier draft said a profile may address its
   own ref and left account scope implied. Implied access rules are how a member
   ends up reading another household member's viewing history.

8. **The rollup is computed server-side and collapses a series to one entry.**
   For a series, the entry is the in-progress episode with the most recent
   `last_played_at` if there is one, otherwise the next unwatched episode after
   the highest completed episode in series order. Series identity comes from the
   series entity and episode order from the canonical season and episode numbers,
   not from filenames. Movies are one entry each. The rail sorts on
   `last_played_at DESC`.

   Web, TV and Flutter must not each reimplement this. It is stated here in the
   ADR rather than left to the endpoint's implementation because the rule is the
   thing that drifts, and three clients each holding a slightly different idea of
   "next episode" is how a household stops trusting the rail.

   **Unmatched episodes group but do not order.** An unmatched show folder now
   gets a `folder:`-keyed series row, so its episodes do belong to a series and
   the rail collapses them rather than listing each file. What they lack is
   canonical season and episode numbers, so there is no series order to walk:
   the rollup can show an in-progress episode, and it cannot compute a next
   unwatched one. Finishing an unmatched episode therefore ends that series'
   presence on the rail until something is played again, rather than advancing.

   That is narrower than "unmatched episodes do not roll up", which is what an
   earlier draft said and which stopped being true when the series entity gained
   folder-scoped identity. The behaviour to document for users is missing
   next-episode, not missing grouping.

9. **`hidden` removes an item from the rail without marking it played.** It is a
   column here and an affordance in Block 3. Marking played and hiding are
   different user intents and collapsing them means "I am not finishing this"
   also claims "I watched this", which then feeds item 8's next-episode
   calculation the wrong answer.

10. **The ADR-0025 §5 merge rule is unchanged, and this ADR does not get to
   reorder it.** On any `item_key` change, the surviving row is chosen by
   position ratio first, then `played`, then newer `last_played_at`. Played-first
   would let a low-position "marked played" row beat an 80% resume under
   rematch, which is backwards for the Gate 3 survival claim.

   The interaction with item 2 is pinned by a table test: an item crossing the
   90% threshold under one key, merged against a mid-watch row under another,
   resolves per §5 and not per the threshold. That test exists because this is
   precisely the place a later reader is tempted to make the rules agree.

11. **Pre-freeze decision, recorded: `item_key` is opaque on the wire,
    permanently.** ADR-0025 §1 documented a grammar for debugging and said it was
    not a parse contract. This ADR is where that becomes a frozen API statement,
    because item 6 puts `item_key` in a URL at all, and a client that reaches
    production parsing `tmdb:episode:{id}` out of one turns a server
    implementation detail into a compatibility promise. Clients pass the string
    back exactly as received, percent-encoded as any query value is. The server
    may change the grammar in any version.

## Alternatives considered

**A series-level state row alongside the episode rows.** Would make the rail a
single-table read and remove the rollup query from the hot path. Rejected: two
places then hold "where you are in this show", they disagree the first time an
episode row is merged by ADR-0025 §5, and the repair is a reconciler nobody
wants to own. The rollup cost is measured before B2-4 ships (B2-M measure 2) so
this stays a measured decision rather than an assumed one.

**Highest-position-wins for concurrent device writes.** A reasonable pick, and
it is what a "never lose progress" instinct chooses. Rejected for the start-over
case in item 3: the user who restarts a title on the television has expressed a
newer intent than the phone that is merely still open.

**Client-computed continue-watching.** Cheaper server-side and it is what the
web client could do today from data it already has. Rejected under Rule 2.1 and
because three clients will drift; the rail is exactly the surface where drift is
visible to the household.

**`play_count` on the row, since it is one integer.** Rejected under Rule 4.7:
no v1 feature reads it, and ADR-0036 answers the question it pretends to answer
without guessing.

**Keep watch state on the file rather than the logical item.** Rejected by
ADR-0025 before this ADR existed; restated only because a reader arriving at the
write path will see `media_items` and wonder.

## Consequences

**Good**

- The Gate 3 survival test becomes runnable for the first time: rename,
  re-encode to a different resolution, and library remove-and-re-add, with
  resume still attaching for a provider-keyed title.
- The ADR-0025 §5 migrator stops being dead code and gets its first real test.
- One rollup implementation, on the server, in the crate that already owns
  policy.
- ADR-0028's assign endpoint can honour its watch-state promise, which it
  currently cannot.

**Bad (accepted)**

- The rollup is a query over episodes and canonical rows at every rail load. At
  24,800 items its cost is unmeasured, and B2-M measure 2 exists because the
  answer is a design input to B2-4 rather than a tuning task afterwards.
- Unmatched series group on the rail but never advance to a next episode
  (item 8), so finishing one drops the series off until something is played
  again. That is the
  below-floor fraction ADR-0025 already priced, showing up in a second place.
- Last-write-wins loses a position when two devices genuinely race. The loss is
  bounded by the 10 second report cadence and is not recoverable from the row.
- A profile's history is destroyed by profile deletion with no undo, per
  ADR-0034 item 7. That is the decision, not a gap.

**Open, and deliberately not decided here**

- Unwatch affordances, "mark unwatched from here", watched treatments, and the
  rail's visual design are Block 3.
- Whether the rail hides a series whose next episode is not yet downloaded is a
  Block 3 presentation question and does not change this table.
