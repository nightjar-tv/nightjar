# ADR-0057: No TheTVDB API integration in v1

- Status: **Accepted** (2026-09-12)
- Date: 2026-09-12
- Depends on: ADR-0025 (item identity), ADR-0026 (metadata pipeline),
  ADR-0031 §7 (left TVDB open), ADR-0033 (durable series identity),
  ADR-0039 (the show entity and `series_key`), Rule 6.1 (ADRs for
  irreversible decisions)
- Amends: ADR-0031 §7 (closes the open TVDB question)
- Related: `ENGINEERING_RULES.md` §3 (TVDB listed in v1 scope, unresolved
  and evidence-gated per ADR-0031 §7)

## Context

ADR-0031 §7 asked whether Nightjar needs TheTVDB at all and left the
question open. It named a 50-show TMDB coverage sample and did not run it.
TMDB already carries TV data, about 93% of the dogfood library is
episodes, and the parse baseline found zero absolute-numbered anime files,
the case usually cited for TVDB's alternate orderings.

The decision authority has now answered the question with a full-library
census rather than the sample. This record closes R5 and states the
product decision. It changes no schema, API, code, license, attribution
surface, or key handling.

## Decision

### 1. TMDB is the sole network metadata provider in v1

Nightjar makes no TheTVDB API calls in v1. TMDB remains the only provider
Nightjar contacts.

### 2. Existing TVDB identifiers stay, and `/find` is not TVDB use

TVDB IDs already parsed from NFO or TMDB metadata may remain canonical
external identifiers under ADR-0025, ADR-0029, and ADR-0039. A stored
TVDB ID may be sent to TMDB `/find` (ADR-0026 §8.10 item 4). That is a TMDB
request that carries an external identifier; it is not use of TheTVDB's
API.

### 3. No replacement, fallback, hybrid, rebinding, or order selector

v1 has:

- no global TVDB replacement for TMDB;
- no TVDB fallback when TMDB misses;
- no hybrid that keeps TMDB identity and takes TVDB structure;
- no automatic rebinding of an item when a provider or order changes;
- no TheTVDB alternate-order selector.

### 4. Measured basis

The decision rests on a full-library census, not the 50-show sample
ADR-0031 §7 named. Method and raw data are maintainer-private, held in
two source records: "TVDB versus TMDB across the whole TV library —
2026-08-12" and "TVDB as a supplement — the narrow test, 2026-08-12".
The aggregate results are recorded here.

- 23,260 items analysed.
- TVDB replacement resolved 17 fewer items, lost 246, and produced 255
  silent different-episode bindings.
- The hybrid gained 104 and lost 1,094, a net of 990 lost, at about 2.99
  extra TVDB calls per show.
- A narrow fallback left a residue of 41 after 107 wrong bindings, 74
  second-TMDB-entity cases, 25 episode-group cases, and 26 unbound
  folders.
- Named alternate-order examples moved 169 correct Futurama files to fix
  6, and 70 correct Rebels files to fix 1.

The measured effect is a net loss of resolved items and a population of
silent wrong-episode bindings. Silent wrong bindings are the failure
ADR-0026 and ADR-0043 exist to prevent.

### 5. Identity and watch state

A provider or order change must never silently reinterpret item identity
or migrate watch state. ADR-0025, ADR-0026, ADR-0033, and ADR-0039 keep
their current behavior. No provider experiment may rebind an existing
item as a side effect.

### 6. Terms checked 2026-09-12

The official API information is at <https://thetvdb.com/api-information>
and the Terms of Service at <https://thetvdb.com/tos>. Checked on
2026-09-12:

- The API license is limited, revocable, non-transferable, and granted
  only for the product or project named when the key was issued (ToS §2).
- Attribution with a direct link to TheTVDB.com is required to end users
  unless TheTVDB approves otherwise (API information page).
- Published revenue tiers: under US$50k per year free with attribution,
  US$50k to US$250k at US$1,000 per year, US$250k to US$1M at US$10,000
  per year, and US$1M+ or custom terms by contact.
- User-supported projects require a per-user subscription PIN (official
  support FAQ, <https://support.thetvdb.com/kb/faq.php?id=82>), and a
  subscription is personal and may not be shared (ToS §6).
- The API license does not grant rights to use or display images,
  trailers, or programming (ToS §2).

This is a product decision, not legal advice. Terms can change.

### 7. Reconsideration conditions

Reconsider TVDB only with new measured population evidence that shows
material unresolved value, and with an applicable API agreement or terms
basis. A future experiment must be explicit and opt-in, must preserve
identity, must survive provider outage without corrupting state, and must
count wrong rebindings and watch-state effects, not only resolved items.

### 8. R5 is closed by no integration

R5 is decided and closed. This ADR authorizes and requires no schema,
API, code, license, attribution UI, or key handling change.

## Alternatives considered

**Replace TMDB with TVDB.** Rejected: net loss of resolved items and 255
silent wrong-episode bindings on the census.

**Hybrid, TMDB identity with TVDB structure.** Rejected: net 990 lost and
about 2.99 extra TVDB calls per show.

**Narrow TVDB fallback for the residue.** Rejected: the 273 undescribed
files decompose into 107 pre-existing wrong bindings, 74 second-entity
cases, 25 episode-group cases, and 26 unbound folders, leaving only 41
genuine residue. A fallback does not fix those categories, and the 107
wrong bindings predate TVDB and are not its doing.

**Alternate-order selector.** Rejected: the named examples moved 169
correct files to fix 6 and 70 correct files to fix 1, and an order change
is the silent identity reinterpretation §5 forbids.

**Keep TVDB as a second source.** Rejected: the measured value does not
justify a second provider, a second license read, and a second
attribution surface.

## Consequences

- `ENGINEERING_RULES.md` §3 still lists TVDB in scope, evidence-gated by
  ADR-0031 §7. ADR-0031 §7 now points here; a constitution amendment is a
  separate act and is not part of this decision.
- No secret field, key distribution, or attribution surface for TVDB is
  needed in v1. The TMDB-only path of ADR-0031 stands.
- Stored TVDB identifiers remain valid inputs to TMDB `/find`; nothing
  about ADR-0025, ADR-0029, or ADR-0039 changes.
- Any future TVDB work must first satisfy §7's evidence and terms bar,
  then be recorded as its own ADR.
