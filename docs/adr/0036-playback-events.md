# ADR-0036: Playback events (writer only in v1)

- Status: accepted
- Date: 2026-08-06
- Accepted: 2026-08-06, on the condition that `close_reason` and
  `playback_method` ship as closed enumerations rather than open strings
  (item 1)
- Depends on: ADR-0034 item 6 (durable `profileRef`) and item 7 (profile
  deletion severs identity); ADR-0025 §1 (`item_key`); ADR-0035 item 5 (the
  single progress report this writer consumes)
- Gate: Gate 3 — full v1 API frozen. This ADR is one of the pre-freeze decisions
  Gate 3 requires be recorded, and the recorded decision is that there is no read
  endpoint in v1
- Related: Block 2 plan B2-C and B2-5 (`nightjar-meta/docs/BLOCK_2_PLAN.md`);
  ADR-0035 watch state, which answers a different question with a different table

## Context

`playback_events` is an append-only log of what was actually watched. Nothing in
v1 reads it. That combination is the whole design question, and both of the
obvious justifications for building it are wrong.

It is not needed for resume. ADR-0035 resumes from a mutable row, and deriving a
resume point by replaying a log is the event sourcing Rule 4.4 exists to kill. It
is not justified by recommendations, which are post-v1 and are the exact
speculative case Rule 4.7 targets.

What is left is the honest argument. Hours watched cannot be reconstructed
retroactively. A household that installs Nightjar today and wants a year of
viewing history next year gets it only if the rows were written today, and no
migration can invent them. Jellyfin needs a plugin for playback reporting because
the data is not there by default, and a third party can only build the dashboard
we are not building if the server kept the data. That is a public-data-surface
argument, it is consistent with the product's posture, and it is the argument
this ADR is willing to defend.

The cost is shape lock, and deferring the endpoint does not defer it. An empty
table can be reshaped by a migration. A table holding eight months of real
playback cannot, because the fields that were wrong were never recorded. So the
shape is decided now, in full, and the deferral buys nothing except API surface.

## Decision

**The table ships and is written from first install. There is no read endpoint
in v1, and there is no write endpoint either: the server writes rows from the
progress reports it already receives.**

1. **One row per closed interval, never one row per position sample.** A row
   records a continuous stretch of playback: `profile_ref`, `item_key`,
   `media_item_id`, `started_at`, `ended_at`, `start_position_ms`,
   `end_position_ms`, `playback_method`, `close_reason`, and the client label
   from the login session (ADR-0034 item 5). Append-only: no update, no delete,
   no foreign key that can cascade.

   Position sampling was the alternative and it makes hours watched a guess. A
   row every ten seconds forces a later reader to decide whether two consecutive
   samples mean twenty seconds of viewing or a pause, and there is no field that
   answers it.

   **Both string-shaped columns are closed enumerations, stored as their
   `as_str` form.** An append-only table whose whole argument is that shape
   cannot be fixed later must not ship two free-text columns; a typo or a
   client-supplied variant becomes permanent data, and a later reader cannot
   distinguish a real category from a mistake.

   `playback_method` reuses `nightjar-core`'s `PlaybackMethod` without
   redefining it (Rule 4.11): `directPlay`, `remux`, `transcode`. If that enum
   ever gains a variant, this column inherits it, which is the point of not
   having a second vocabulary.

   `close_reason` is defined here and is exactly six values. `ended` means
   playback reached the end of the item. `stopped` means the viewer ended the
   session. `seek` and `audio_change` are item 3's two splits. `timeout` means no
   report arrived within the window and the server closed the interval at the
   last known position, which is what a client killed by the platform produces.
   `error` means playback failed. Adding a seventh is a migration and a decision,
   not an implementation detail.

   `timeout` and `stopped` are separate on purpose. Collapsing them would make
   "a television that was switched off at the wall" indistinguishable from "a
   viewer who chose to stop", and the first is the one that indicates a product
   problem.

2. **Hours watched is computed from positions, not wall clock.** The measure is
   the sum of `end_position_ms - start_position_ms` across rows. A pause is
   therefore invisible and correct without a pause event, and a title left
   running against a wall for three hours does not report three hours watched.
   `started_at` and `ended_at` are kept because "when did this household watch"
   is a different and equally reasonable question, not because the duration is
   derived from them.

3. **An interval closes at session end, on an audio track switch, and on a seek
   of more than 30 seconds.** Thirty is the number this ADR is required to pick,
   and it is the field that cannot be fixed later, so here is the reasoning.
   Below it sit the skip buttons, which are 10 or 15 seconds on every client we
   will ship and are frequently double-tapped. Above it sit the things that
   matter: an intro skip is 60 to 90 seconds, a chapter jump is minutes, and
   scrubbing to a remembered scene is more. At 30 seconds a nudge over-counts at
   most 30 seconds of viewing and produces no row, while an intro skip closes the
   interval and is not counted as watched.

   The measurement is against the position continuous playback would have
   reached, not against the last reported position, so a seek during a pause is
   still a seek.

   An interval also closes on `timeout` and on `error`, which are not viewer
   actions and are listed with the others in item 1 so the enumeration is
   complete. A timeout closes at the last reported position rather than at the
   moment the server noticed, because the viewer stopped watching when the
   reports stopped.

4. **A subtitle change does not close an interval; an audio track switch does.**
   Audio changes what was watched in a way a later reader would want to see
   separately, and it is the case ADR-0024's ranker can get wrong, so a switch is
   evidence worth keeping. Subtitles do not change the audio timeline and would
   double the row count for a viewer toggling captions on and off.

5. **Profile identity is severed on deletion and the rows stay.** `profile_ref`
   is the durable opaque reference from ADR-0034 item 6, stored as a plain column
   with no foreign key. Deleting a profile destroys its name and its watch state
   and leaves these rows with a reference that resolves to nothing. That is the
   same sentence ADR-0034 item 7 and ADR-0035 use, and it is deliberate: household
   hours watched survives a profile being deleted and recreated, and the deleted
   person's name does not.

   The rowid alternative fails here specifically. SQLite reuses rowids, so a
   profile deleted and recreated would inherit the previous profile's viewing
   history under a rowid reference, which is the failure ADR-0025 refused for
   watch keys and refuses again here.

6. **The server writes these rows; no client and no route can.** The writer sits
   on the progress path ADR-0035 item 5 defines, so a report that updates the
   watch row also advances or closes the current interval. There is no
   `POST /playback/events`.

   That removes a public write surface, removes any question about clients
   forging viewing history, and means the interval rule is enforced in one place
   rather than trusted to each client. It also means the log records what the
   server served, which is the honest scope of what a server can claim to know.

7. **No read endpoint in v1, recorded as the pre-freeze decision.** Adding one
   after the freeze is additive under Rule 2.3 and costs nothing later. Gate 3
   requires the four pre-freeze decisions be recorded in ADRs, and this sentence
   is why "not built yet" satisfies that rather than dodging it. A later endpoint
   can serve history back to first install, which is the entire reason to write
   the rows now.

8. **Retention is forever, and the size is small enough to say so.** A row is on
   the order of 100 bytes. A household watching five hours a day, with intervals
   closing on the order of twenty times a day, writes roughly 2 KB a day and
   under a megabyte a year. There is no pruning job, no retention setting, and no
   rollup table (Rule 4.7 and Rule 4.12). If the number is ever wrong it will be
   wrong by an order of magnitude and still not matter.

9. **This is not an analytics pipeline and nothing leaves the server.** No
   telemetry, no upload, no aggregation service. The rows are in the same SQLite
   file as everything else and the household owns them.

## Alternatives considered

**Do not build the table until something reads it.** The Rule 4.7 answer, and it
is the strongest alternative. Rejected on the one asymmetry that Rule 4.7 does
not cover: the cost of waiting is unrecoverable data, not a later refactor. Every
other speculative abstraction can be added when the second use case arrives; this
one cannot be backfilled.

**Position samples, aggregated later.** Simpler writer, and it is what a naive
implementation produces. Rejected in item 1: it makes hours watched
unreconstructible, which is the only thing the table is for.

**Reuse `watch_state` history by keeping old rows.** Would avoid a second table.
Rejected: watch state is mutable by design and its merge rule (ADR-0025 §5)
deliberately discards losers, so it is the wrong object to hang an append-only
log off.

**Ship the read endpoint now, since the data will exist.** Rejected: it attaches
a compatibility promise to a surface with no consumer to validate it against,
which is the cost PHASE_3_REVISED named, and Rule 2.3 makes adding it later free.

**A `paused` event type, so wall clock reconstructs viewing.** Rejected by item
2: computing from positions makes pause tracking unnecessary, and a pause event
is a row we would write for every remote button press.

## Consequences

**Good**

- Hours watched is answerable for any window back to first install, whenever
  someone builds the reader.
- No public write surface and no client-supplied history.
- One progress path shared with ADR-0035, so the interval rule cannot drift from
  the resume rule.

**Bad (accepted)**

- A table with no reader in v1 is code that ships untested by use. Its tests are
  the only thing asserting it is correct, and a shape error will be found by the
  first reader in a later phase, against months of rows.
- The 30 second threshold is a judgement. If it is wrong it is wrong in the
  recorded data forever, which item 3 states plainly rather than hedging.
- Playback outside a Nightjar session is invisible. A direct-played file that a
  client fetches and never reports on produces no rows, so the log measures what
  the server observed rather than what the household watched.
- Deleted profiles leave dangling references by design, so any future reader must
  handle an unresolvable `profile_ref` as a normal case rather than as corruption.
