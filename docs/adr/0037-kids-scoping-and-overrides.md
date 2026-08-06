# ADR-0037: Kids scoping and parent overrides

- Status: proposed
- Date: 2026-08-06
- Blocked on: the series ADR, which owns the episode-to-series edge item 5's
  certification inheritance reads; and the three-role model, which item 11
  qualifies against. Cannot be accepted before both are on disk
- Supersedes: ADR-0026 §8.4 item 3 (certification projection; see item 8)
- Depends on: ADR-0034 (account and profile scope, the cap and simple-interface
  columns, PIN deferred to B2-7); ADR-0026 status vocabulary; ADR-0029 §1.5
  (canonical projection); ADR-0033 (series identity, which overrides are scoped
  to); ADR-0028 (manual metadata fix, which is the repair for a wrong
  certification); ADR-0031 (TMDB key and attribution)
- Gate: Gate 3 — kids scoping and parent overrides are unreachable from a profile
  session including an adult-flagged one; kids denial verified on every
  item-returning endpoint
- Related: Block 2 plan B2-D, B2-6, B2-7 (`nightjar-meta/docs/BLOCK_2_PLAN.md`);
  ADR-0035 watch state (the rail is an item-returning surface and is not exempt)

## Context

Every competitor gets the ladder roughly right and the leak surface wrong.
Jellyfin's parental controls leak through suggestions on the Shows, Movies and
Collection tabs. Plex has a report of a managed user's rating profile being
bypassed after a television client lost its home-screen settings and re-prompted
for library pins. The failure is never the certification comparison. It is a
surface that returns items without asking whose session it is.

So the decision that matters here is not which ladder. It is that one filter
sits at the query layer, no endpoint is exempt, and the exemption is prevented
by types rather than by a reviewer noticing. Search, continue watching, recently
added, the collections line, the post-play card, and any later recommendation
row are all item-returning surfaces and all of them are leaks if the filter is
opt-in.

ADR-0034 put the cap and the simple-interface flag on the profile as two columns
and deliberately decided nothing about what they mean. This ADR decides what
they mean.

## Decision

**A profile's classification cap is server-side scoping applied at the query
layer to every endpoint that returns items, the evaluator fails closed, and only
an account holder can override it.**

1. **Two flags, and they stay two.** The classification cap is scoping and
   applies regardless of client. The simple interface is a presentation flag read
   by clients and enforced nowhere. A fourteen-year-old wants the cap without the
   large posters, and a five-year-old wants both. Fusing them means splitting them
   again inside a year, and the split is free today because ADR-0034 already
   shipped two columns.

2. **One classification region for the server, chosen at setup and locked.** It
   lives as a single row in `server_settings`, written during setup.

   The region belongs to the server, not to a household, and the noun matters
   because accounts on one server need not share a house. `server_settings` says
   what it is: an installation-wide setting that happens to be a classification
   board. A table called `household` would read as a claim about who lives where,
   which the server does not know and does not model.

   Only that region's board counts. An AU `PG` does not admit a title on a `DE`
   server, because the boards are not translations of each other and a mapping
   between them would be a claim we cannot support.

   Locked means no route changes it. Changing the region rescopes every item in
   the library at once and can silently expose titles to a child, so it is a
   `nightjar` subcommand on the server, the same escape-hatch class as ADR-0034
   item 12's password reset.

3. **The certification ladder ships as a snapshot compiled into the binary.**
   Region ladders are a data file embedded with `include_str!`, so the setup
   dropdown and the evaluator never call TMDB and first run works with no network
   (Rule 1.2, Rule 4.12). A background refresh may write a newer copy into the
   data directory, which is preferred when it parses and covers the server
   region; on any failure the shipped copy stands and the server logs which copy
   it is using.

   The snapshot is an on-disk shape, so it is decided here under Rule 4.9. It
   maps, per region, an ordered list of raw labels and each label's named tier.
   The ordering is the ladder; the tier names are item 4.

4. **Caps are named tiers with the raw label shown next to them.** The tier set
   is closed and is `little_kid`, `big_kid`, `teen`, `adult`. "Little kid" is
   usable by a parent standing at a television and `12` is precise for the parent
   who knows their board, so both are shown and the cap is stored as the tier. A
   region whose ladder has more rungs than four maps several labels onto one
   tier; that mapping lives in the snapshot, not in code.

5. **The evaluator fails closed, and it is a pure function.** A title is visible
   to a capped profile only if it has not been blocked, and then only if it is
   `ready` (ADR-0026), has a non-empty certification for the server region, and
   that certification is at or under the cap, or an account holder has allowed
   it. Unmatched denies. Missing certification denies. An unknown label denies. A
   provider failure denies.

   The function lives in `nightjar-core` next to `decide_playback` and the
   ADR-0024 ranker, takes the facts and returns visible or denied with a reason,
   and never performs I/O. Every denial carries its reason, which is both what
   item 11's counts aggregate and what the tests assert.

   **An episode's certification is its series' certification.** TMDB's
   `content_ratings` is a series-level endpoint, so an episode entity carries no
   board label of its own and never will. Read literally, the paragraph above
   would deny every episode in the library for missing certification, which means
   a capped profile sees no television at all. The evaluator therefore resolves an
   episode's certification through its series' canonical row, and the series ADR
   owns that inheritance because it owns the episode-to-series edge. That is why
   this ADR cannot be accepted before the series ADR lands.

6. **Precedence has exactly one order: blocked, then allowed, then the ladder.**
   No weighting and no tie-breaks. A blocked title is invisible even if it is
   rated `G` and even if it was previously allowed. An allowed title is visible
   even if its certification is missing, which is the whole point of allow.

7. **One filter at the query layer, enforced by the type system.** The shared
   item-returning query function takes a viewer scope parameter that has no
   `Default` and is not `Option`. A new route cannot compile without deciding
   what scope it runs under. The two scopes are ADR-0034's: account scope browses
   unrestricted because the manual fix flow has to see the item it is fixing, and
   profile scope carries the cap.

   A test that enumerates item-returning routes and asserts each denies is a
   backstop and is not the guarantee. An enumeration only covers what someone
   remembered to add to it, which is the vacuous-test class the Block 1 autopsy
   caught. The guarantee is that the code does not build.

8. **Certification storage: a new column, and B2-6 builds the projection before
   it can read one.** `metadata_canonical` gains a `certifications_json` column
   holding region to label pairs. This ADR decides that shape under Rule 4.9.

   **ADR-0026 §8.4 item 3 is superseded in place, not merely redirected.** It
   froze a certification projection into the ADR-0029 §1.5 ratings path and reads
   as a shipped decision. It is neither correct nor shipped. The storage it named
   cannot hold a board label: `Rating` is `{source: String, value: f64, votes:
   Option<i64>}` and `ratings_json` is a `Vec<Rating>`, so `PG-13` has nowhere to
   go. And nothing in `server/crates/metadata` parses `release_dates` or
   `content_ratings`, so there is no projection to correct. Leaving that clause
   readable as frozen-and-shipped is the hygiene debt the Block 2 plan already
   flags for ADR-0001 and ADR-0002, so §8.4 carries a supersession marker
   pointing here (Rule 6.1).

   Widening `Rating` into a sum type was the alternative and is rejected. A
   rating is a numeric score with a source and a vote count; a certification is a
   board label with a region. Different provenance, different consumers, and the
   evaluator reads one and never the other. Widening would force every existing
   `Rating` consumer to match on a variant it does not care about, to save one
   column.

   **The scope consequence, stated because the plan implies otherwise: B2-6 does
   not read a projection that exists. It writes the projection and then reads
   it.** That is parser work over stored payloads, a migration, a back-fill over
   the existing library, and the evaluator, in one slice. Sized as "add a filter
   to a query" it will be under-scoped.

   No third full-library TMDB pass is required, which is the part of §8.4's
   intent that survives. `MOVIE_APPEND` and `TV_APPEND` already request
   `release_dates` and `content_ratings`, and `metadata_raw_payloads` stores the
   raw body keyed by provider entity, so the labels are on disk and unparsed.

   **Confirm coverage before B2-6 dispatches, and report three numbers, not
   one.** Movies, series, and episodes are counted separately, because
   `content_ratings` is series-level and episodes will show zero by construction
   (item 5). A single blended coverage figure would hide that and read as a
   catastrophic gap. B2-M measure 1 publishes the real numbers with their method;
   the AU 93 / DE 96 / BR 87 figures came from filename search against a probe
   chain and the note that produced them says to ignore them.

9. **Two overrides, both account-only, both PIN-confirmed, both series-scoped for
   shows.** "Allow in kids mode" admits a title the evaluator denied and is the
   only thing that can. "Remove from kids mode" hides a title the evaluator
   allowed, which is the case fail-closed cannot reach: a correctly certified `PG`
   that a parent does not want in the house. Both record the acting account and
   the time.

   **The scope column ships in v1** and defaults to server-wide, so one entry
   applies to every capped profile unless it says otherwise. Server-wide is the
   right default for most installs and the wrong one for two adults with
   different parenting standards, and the earlier plan to add the column later
   "without moving the rows" is a worse deal than it sounds: a nullable column
   added afterwards means every reader written in between assumed one meaning,
   and the migration is cheap while the assumption is not. Rule 4.9 wants the
   shape now. Whether any surface lets a user set it to something other than
   server-wide in v1 is a Block 3 question and does not change the column.

   A profile can never perform either action, including an adult-flagged one, and
   the actions do not render inside a profile session. Widening from profile scope
   back to account scope re-authenticates per ADR-0034 item 3. The PIN confirms
   the account holder is present and is not a second factor layered on an already
   authorised profile. B2-7 designs the PIN and this ADR does not bind its
   routing, matching ADR-0034 item 3.

10. **Allow is not the fix for a wrong match.** If a title carries the wrong
    certification because it matched the wrong TMDB record, the repair is the
    ADR-0028 fix flow, which re-derives the certification for the region.
    Allowlisting a mismatched title leaves the wrong synopsis, artwork and runtime
    in place and removes the title from the counts that would have surfaced the
    fault. The UI offers the fix flow first and the override second.

11. **Observability, so an adult can find allow candidates without guessing.**
    `GET /api/v0/system/kids-scope` returns counts of titles denied as unmatched,
    denied for no certification in the server region, denied as over cap, and
    denied for an unknown label, plus the overridden titles listed separately in
    each direction. An override is a decision somebody made and has to be
    findable later rather than invisible.

    **"Admin-only" no longer names one thing, so the route is qualified by
    role.** Under the three-role model an owner and a manager see the counts for
    every profile on the server; a member sees only their own profiles. The
    library-wide denial counts do not vary by role, because they are facts about
    the library rather than about a viewer. What varies is which profiles' caps
    the response reports against, and a member reading another account's
    parenting decisions is not something this route should make easy.

12. **A kid profile's history is readable from account scope on the owning
    account, and is not rendered in any profile session.** Visibility follows the
    current cap with no retroactive access, so clearing a cap ends the view from
    that moment rather than making "my kid turned 18" a support question.

    **This is scope separation, not confidentiality, and the ADR says so rather
    than implying a guarantee.** An earlier draft read as though a teenager's
    history were private from the adult profile sitting next to it. It is not.
    The same person holds the account password, and ADR-0034 item 3 lets them
    widen from profile scope to account scope by re-entering it. Under the
    three-role model an owner or manager can do the same for accounts they
    administer. What the rule actually buys is that history never appears
    incidentally in a profile session, so a child cannot read a sibling's history
    by picking their tile and an adult cannot read it by accident. Anyone who
    wants it and holds the credential gets it.

    The server administrator can read the SQLite file regardless. The profile UI
    says both of these plainly, because a privacy promise the architecture does
    not keep is worse than no promise.

## Alternatives considered

**One flag combining cap and presentation.** Simpler schema and it matches how
most products ship the feature. Rejected in item 1 for the fourteen-year-old
case, and rejected more strongly because unfusing them later means migrating
profiles whose single flag meant two things.

**Filter in the route handlers, with a review checklist.** The obvious
implementation and the one every leaked competitor shipped. Rejected in item 7:
the failure mode is a route added later by someone who did not read the
checklist, and a type is the only mechanism that catches that on the day it
happens.

**Fail open on missing certification, so an unmatched library is usable.**
Genuinely tempting, because a household with a 7% unmatched rate hands a child a
mostly empty library. Rejected: fail-open means one unmatched adult title is
visible to a five-year-old, and the correct response to an empty kids library is
to fix matching, which the counts in item 11 make actionable.

**Multiple server regions, one per profile.** Would serve a mixed household
and looks more flexible. Rejected: a per-profile region multiplies the
projection by region count and produces cross-board comparisons the boards do
not support. One region, locked, and the override is the escape hatch.

**Call TMDB at evaluation time for a missing certification.** Rejected: it puts a
network call on the item query path, fails closed anyway when the network is
down, and turns a kids listing into a rate-limit consumer. The projection is
computed at enrich time and read locally.

**Time limits, bedtimes, per-day quotas.** Rejected for Phase 3 explicitly and
deferred out of the phase, not merely unbuilt. They need a scheduler and a
notion of enforcement during playback that nothing else in the product has.

## Consequences

**Good**

- The leak class competitors ship is prevented by construction rather than by
  discipline, and a Block 3 route inherits the filter or does not compile.
- Certification arrives with no additional TMDB traffic, because the data is
  already stored and only unparsed.
- Fail-closed means every gap in matching is visible as a denial with a reason
  instead of as an accidental exposure.
- The override model is one table with room to gain per-account scope later
  without moving rows.

**Bad (accepted)**

- A capped profile sees a smaller library than the household owns, bounded by the
  unmatched rate and by regional certification coverage, and both are measured
  rather than assumed. On a library where coverage is poor, the kids experience is
  poor, and the fix is matching work rather than a threshold.
- Overrides default to server-wide, which is wrong for two adults with different
  parenting standards. The scope column ships in v1 so the fix is a surface
  rather than a migration, but no v1 surface sets it.
- Region is locked behind a subcommand, so an installation that picks wrong at
  setup needs shell access to change it. That is deliberate and it is a support
  cost.
- The evaluator adds a join to every item query. Its cost at 24,800 items is
  unmeasured, and B2-6 owns measuring it before the filter lands on the rail.
- **B2-6 is a larger slice than the plan implied.** It builds the certification
  projection, migrates the column, back-fills the library, and only then filters
  (item 8). Sizing it as "add a filter" under-scopes it.
- ADR-0026 §8.4 item 3 is superseded in place, so a reader of that ADR now finds
  a marker rather than a decision that was never shipped.

**Still open**

- Kids interface surfaces, poster treatments, the gape mark, and PIN entry are
  Block 3.
- Gate 3 requires kids denial verified on the collections line and the post-play
  card, which are Block 3 routes. B2-6 cannot test endpoints that do not exist, so
  the real Gate 3 re-run is recorded as an open residual at the Block 3 gate
  rather than claimed here.
