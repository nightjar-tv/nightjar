# ADR-0043: Season fit as a candidate constraint, and a recorded unmatched reason

- Status: **§1 (season fit) superseded before shipping — see Amendment
  2026-08-12. §2 (unmatched reason) accepted and stands.**
- Date: 2026-08-12
- Depends on: ADR-0025 (item identity / path keys); ADR-0026 (§2 scoring
  floor, §8.4 terminal statuses); ADR-0028 (manual fix flow); ADR-0029
  (season detail payloads); ADR-0033 (durable series identity)
- Gate: Gate 3 — metadata auto-match ≥95% correct, every mismatch fixable
  in-UI in under 30 seconds
- Related: the Block 1 leave bar (unmatched is an output,
  never a target); the TVDB vs TMDB library spike note, 2026-08-12

## Context

326 items rest at `unmatched` on the dogfood library. 275 of them carry a
`tmdb:show:` link and a parsed season and episode, and **206 sit in a folder
bound to a show whose season list cannot contain that folder's episodes** —
`Battlestar Galactica (2003)` asserts seasons 1–4 across 70 files and is
bound to TMDB 71365, which holds one season of two episodes (the
miniseries). `What If…!` asserts three seasons and is bound to a four-episode
show named `What If`. `Monster (2022)` is bound to
`DAHMER - Monster: The Jeffrey Dahmer Story`.

These are wrong matches resting in the forgiving bucket. `BLOCK1_LEAVE_BAR.md`
is explicit that unmatched degrades gracefully — it has three built consumers
— while a wrong match "has none and corrupts watch state and certifications."
The count understates the harm by exactly this population.

The matcher already held the evidence to reject them. A folder asserting
seasons 1 through 4 cannot be a one-season show, and TMDB's `/tv/{id}`
response — already fetched and already persisted whole in
`metadata_raw_payloads` — carries the `seasons[]` array that says so. No
additional provider call is needed to know this at bind time.

Separately, the drain emits one line for every one of these causes:

```
enrich unmatched <show> — N file(s) without episode identity
```

Every distinction in the diagnosis above came from TMDB, not from Nightjar.
A fix flow keyed on the drain's output is keyed on one undifferentiated
bucket, and re-diagnosing needs the provider again. The drain already knows
which cause fired at the point it fails; it discards that knowledge.

## Decision

### 1. Season fit is a constraint on show candidates, not a scoring signal

**A show candidate whose season list cannot cover the folder's asserted
seasons is not a candidate.** The test is a subset test over the folder's
parsed season numbers, and it is evaluated in two places against one shared
predicate (Rule 4.11):

- **Before scoring**, filtering the search candidate set, so no downstream
  branch of the scorer ever sees a candidate that cannot hold the folder.
- **Before binding a resolved show**, because the ADR-0033 stored-series-id
  path resolves with no scoring at all, and that is precisely how all 11
  affected folders stay bound today.

Rules that make it a constraint and not a tunable:

- The fit test is over the **folder's** parsed seasons, not one file's. A
  folder asserting S1–S4 requires all of 1, 2, 3, 4.
- **Season 0 is excluded.** A `Specials` folder exists independently of
  whether a provider models season 0, and requiring it would reject correct
  candidates.
- **An unknown season list never rejects.** A candidate whose structure has
  not been fetched is not evidence against it.
- **No season-offset rule, ever.** A −5 delta lands all 52 Will & Grace
  items "in range" and would silently put the 2017 revival on seasons 4–6.
  That is a wrong match manufactured to clear a count.
- If **no** candidate fits, the group stays unmatched with reason
  `no_season_fit`. Binding the best of a bad set is the failure mode this
  decision exists to remove.

This adds a gate. It relaxes none, and it cannot raise coverage — it only
ever removes candidates, which `BLOCK1_LEAVE_BAR.md` requires of any slice
that touches the scoring path.

### 2. The unmatched reason is a column on `media_items`

`media_items.metadata_unmatched_reason TEXT NULL`, written in the same
statement that sets `metadata_status`, cleared to `NULL` on any non-unmatched
status.

**Shape (Rule 4.9), decided here before the writer exists.** A closed set of
lower-snake-case tokens, one cause per token, never a free-text sentence and
never two causes collapsed into one label:

| reason | meaning |
|---|---|
| `no_show_candidate` | nothing survived candidate selection |
| `no_season_fit` | candidates existed; none covered the folder's seasons |
| `season_out_of_range` | the scanned season does not exist on the bound show |
| `episode_out_of_range` | the season exists; the scanned episode is beyond its episode count |
| `episode_not_projected` | the season exists and holds the number, but no episode row was projected for it |
| `duplicate_slot` | another file in the folder already holds this season/episode |
| `no_scanned_number` | the file has no parsed season/episode to resolve |
| `nfo_invalid` | NFO bytes present but unparseable |
| `below_threshold` | best hit scored under the ADR-0026 floor |
| `stored_id_404` | the stored provider id no longer resolves |

Adding a token is additive and cheap; **merging or reusing one is not**, so a
new cause gets a new token rather than the nearest existing one.

**Why a column and not a structured log field.** The reason must survive
`docker rm` and be queryable to size a fix-flow backlog; log persistence has
cost this project the same diagnosis three times. It is a plain nullable
`TEXT` on a table the drain already writes per item in the same statement, so
it adds no write and no round trip. It is deliberately *not* a foreign key to
a reason table and *not* an enum-constrained column: the token set will grow
as the matcher distinguishes more causes, and a `CHECK` constraint would make
each addition a migration.

**It is diagnostic, never control flow.** Nothing reads the column to decide
what to do next; it records why a decision already taken came out as it did.
A consumer that branches on it would make the token set an API and freeze it.

## Consequences

- Migration `021_metadata_unmatched_reason.sql` adds one nullable column. No
  backfill: existing `unmatched` rows carry `NULL` until the next drain
  re-derives them, and `NULL` reads as "recorded before this ADR", which is
  honest and distinguishes itself from every real token.

  > **Note added 2026-08-14, at commit. The number 021 is contested and is not
  > reserved by this record.** B2-0's `021_series_binding_without_entity.sql`
  > claims it too, and ADR-0046 item 2 refers to it under that name. Migrations
  > on `main` stop at 020 and neither of the two is built, so nothing has
  > settled the ordering and this record does not settle it either. **The
  > number is taken at implement time**, which is the same language ADR-0046
  > uses for its own. The name is kept as written because the section around it
  > is what was decided; only the claim on the digits is withdrawn.
- The 206 mis-bound items lose a wrong binding. They do not automatically
  gain a right one — for Battlestar the correct show (TMDB 1972) has never
  been fetched into this installation, so the folder rests at
  `no_season_fit` until a drain with provider access re-searches it. The
  unmatched count therefore **rises**, and the published 1.30% is superseded.
  That is the intended direction: `BLOCK1_LEAVE_BAR.md` makes unmatched an
  output, and moving a wrong match into the bucket with three built consumers
  is the point of the change.
- Will & Grace is the known cost, and the shortfall is **structural, not
  stale**. TMDB 4454 is the 1998 series: eight seasons, `last_air_date`
  2006-05-18. TMDB models the 2017 revival as a *separate show id*, so a
  refetch of 4454 returns seasons 1–8 however many times it is called. The
  folder spans two TMDB entities. There is no "we never fetched it" branch
  here to recover, and season fit therefore rejects 4454 permanently — taking
  the 194 files on seasons 1–8 that bind correctly today with it.
- `remoteIds` reconciliation (spike, 14 shows / 382 items) stays unbuilt; the
  number of surviving disputes after this change is the input to that
  decision, not an output of this one.

---

## Amendment 2026-08-12 — §1 is the wrong mechanism, measured

Season fit was adopted on the reasoning that a folder asserting S1–S4 cannot
be a one-season show. That inference is sound and useless: it is equally true
of a folder bound to the **wrong** show and a folder bound to the **right**
show whose provider structure stops short. Nothing in a season list separates
them, and the cost of guessing is 338 correct bindings.

**Episode titles do separate them, and they are already in the filenames.**
Measured with `metadata-title-match-measure` over the 552 files in the 11
folders season fit rejects, using the shipped extractor
(`after_token_episode_title`) and the shipped comparator (`norm_key`):

| | files |
|---|---:|
| title matches the episode at the scanned season/episode | **309** |
| title matches a different episode of the same show | 19 |
| season fetched, no episode carries this title | 48 |
| untestable — season never fetched | 160 |
| generic title (`Episode 7`), no identity | 14 |
| no title in the filename | 2 |

The 48 apparent refutations split on inspection — all 48 read by hand,
because the spike established that a punctuation-normalising comparator
cannot tell a format difference from a real one:

- **22 are genuine**, and they fall in exactly two folders. Battlestar 13
  (`33` against `Part 1`; eleven numbers absent entirely) and What If…! 9
  (`What If… Captain Carter Were The First Avenger` against
  `The Swedish Nobility`).
- **26 are title-format or alias differences**, and they fall only in folders
  whose entity is right: Farscape 19 (`Look at the Princess (1) - A Kiss Is
  But a Kiss` against `Look at the Princess - A Kiss is But a Kiss (1)` — the
  part number moved), Will & Grace 4, Jujutsu Kaisen 2, Monster 1.

**The discrimination is total.** Every folder with at least one confirming
title is a right-entity folder; the only two folders with zero confirmations
and a genuine refutation are the only two whose bindings were independently
verified wrong. Season fit scores zero of 552; title agreement recovers the
same 6 wrong bindings while preserving all 338 correct ones.

**Decision.** §1's mechanism is superseded. A show binding is refuted by
**episode-title disagreement across the folder**, not by season count. Season
fit is not retained as a cheap pre-filter: on this corpus it produced 338
false rejections and no true rejection that title agreement did not also make.

Stated precisely, because the corpus does not settle everything: three of the
eleven folders (The Traitors, The Continental, Bleach — 20 files) yield no
title verdict at all, because their titles are generic or their seasons were
never fetched. Season fit rejects them and titles are silent. All 20 are
already unmatched, so nothing turns on it either way, and it is not evidence
that season fit adds value — only that neither mechanism speaks there. Titles that carry no identity (`Episode 7`, the
show's own name) are not evidence in either direction — the same rule ADR-0032
already applies to the collision pin.

Not decided here, and needing their own record before code: how many
confirmations make a folder's binding safe, what to do with the 160 files
whose season was never fetched, and whether the comparator tolerates the
part-number transposition that accounts for most of the 26. §2 and the
`no_season_fit` reason code are unaffected and stand — the reason a candidate
was refused is worth recording whatever refuses it.
