# ADR-0031: TMDB API key distribution and attribution

- Status: accepted
- Date: 2026-08-03
- Accepted: 2026-08-03
- Depends on: ADR-0026 (§5 sketched application key + override; this ADR
  owns the terms, injection, attribution, and rotation record)
- Gate: Gate 3 — metadata matching requires a working key path before
  real matcher traffic
- Related: ADR-0027 (artwork — reserved, unwritten; artwork is still
  TMDB-derived under §1's 6-month ceiling — the separate ADR decides
  image pipeline shape, not ToU scope); `nightjar-meta/docs/COPY_DECK.md`
  (needs a matching attribution entry — not written yet)
- Numbering: next free after 0030. **0027 remains artwork.** Do not reuse.

## Context

Metadata matching calls `api.themoviedb.org`. A key must exist for every
install that auto-matches, or Gate 3 metadata fails the "works by default"
bar (Continuity standing review heuristic labeled Rule 4.12 — **not** in
`ENGINEERING_RULES.md` today; numbering gap after 4.9 / missing 4.10–4.12
is Rule 6.3 gate-read-through work, not this ADR).

TMDB's API terms license the API as non-transferable and
non-sublicensable. Nightjar is GPL-3.0 and redistributes binaries and
Docker images. Putting our key in those artifacts is outside those terms.
ADR-0026 §5 already named the ship-with-key shape; this ADR records the
terms risk, what build-time injection does and does not buy, attribution,
rotation, and the override path that must work with no built-in key.

TVDB is in v1 scope (constitution §3) but is **not** decided here.

## Decision

### 1. Terms position (no softening)

TMDB licenses API access as **non-transferable and non-sublicensable**.
Embedding Nightjar's application key in a redistributed GPL-3.0 binary or
container image is outside those terms.

**Risk:** TMDB may revoke the key at any time. Revocation breaks live
metadata (search, match, refresh) for **every** install still using that
key, in the same moment. There is no gradual degradation and no per-user
isolation. Cached canonical rows and raw payloads already on disk may
keep serving **browse only temporarily** — at most until the §1.C
**6-month** cache ceiling, and only while the license relationship has
not been terminated. Under termination (§1.D / §6), purge is required and
that comfort does not apply. With no working key, nothing can be
re-validated. The override path (§4) is therefore not a convenience: it
is what keeps an install able to re-fetch and stay compliant after
revocation.

We record this knowingly. It is not an oversight.

**Commercial vs free use (verified 2026-08-03, live API Terms of Use
last updated 2023-10-20, §2):** commercial use means deriving revenue from
the API or its content. Nightjar is free and GPL-3.0, so **free
non-commercial use applies**. That is the ground this ADR stands on —
verified against the live terms (FAQ commercial definition is consistent;
§2 is the binding wording).

**Prior art (verified 2026-08-03; source not named):** the dominant
open-source implementation of this pattern holds its TMDB key as a public
constant in its repository — not build-injected, present in git history,
and with no runtime override, so revocation would require a release. Our
§3 (CI injection, not in git) and §4 (runtime override) are deliberately
stronger on both points. That key has been public for years without
revocation: record as evidence the **practical** risk is low, **not** as
permission to violate the terms.

**Cache ceiling (verified 2026-08-03, live ToU §1.C):** do not cache
anything obtained from TMDB for longer than **6 months**. Hard ceiling,
not a judgement call. The embedded application key makes **Nightjar** the
licensee, so retention on operator disks happens under our key
relationship. This is a **design input** to the Block 1 refresh strategy:
that strategy must ensure no TMDB-derived record (canonical metadata,
raw payloads, artwork once ADR-0027 lands) goes longer than 6 months
without being re-fetched or re-validated. Cadence, staggering, and
conditional image re-validation are **not** decided here — only the
constraint they must meet. Do not reopen ADR-0029 for this.

**ML / AI training (live ToU §1.C):** prohibition on using TMDB content for
ML/AI training. No v1 impact; relevant to any post-v1 recommendations
design.

### 2. Why we do it anyway (deliberate license exception)

Core behaviour should work by default; settings are for genuine
preferences and hardware escape hatches, not for required signup before
the product works. (Continuity calls this Rule 4.12; it is a review
heuristic until a constitution amendment lands it — see Context.)

Auto-match is Gate 3 core behaviour. Requiring every operator to register
a TMDB account and paste a key before the library enriches would break
that bar. Shipping an application key is a **deliberate exception** to
TMDB's license position in service of that default. The override path
(§4) is the compliance and recovery hatch (revocation, rejected key,
source builds) — not optional polish.

### 3. Build-time injection — what it does and does not achieve

Release CI injects the application key from **CI secrets** into the build
as `NIGHTJAR_TMDB_APP_KEY` (or equivalent `env!` / build-env slot). The
key is **not** in the git repository and **must not** appear in git
history.

| Achieves | Does not achieve |
|---|---|
| Keeps the key out of the repo and cloneable history | Hiding the key from anyone with the published binary or image |
| Rotation = cut a new release with a new secret | Stopping extraction from the artifact (strings, debugger, image layers) |

Dev and contributor builds may omit the embedded key entirely. Metadata
then depends on the override path only.

### 4. Override path (also source builds and revocation recovery)

| Source | Where | Role |
|---|---|---|
| User / operator key | `{NIGHTJAR_DATA_DIR}/secrets`, mode `0600`, field `tmdb_api_key` (TMDB **v3 api_key** string). Settings UI writes this file; not a SQLite column. Encoding: ADR-0026 §5 (line-oriented `name=value`). | Highest-priority override |
| Operator env | `NIGHTJAR_TMDB_API_KEY` (non-empty v3 api_key) | Override for containers / systemd without writing the file |
| Application key | Compiled into release builds from CI secrets (v3 api_key) | Default only when no override is set |

**Credential shape:** v1 stores and accepts a TMDB **v3 `api_key` only**.
Bearer read-access tokens are **not** a supported form in v1. Later
providers inherit that pattern: one clear secret field per provider, not
a mix of token kinds in the same slot.

**Precedence:** non-empty secrets-file key, else non-empty
`NIGHTJAR_TMDB_API_KEY`, else embedded application key. Env beats
embedded. Secrets-file beats env.

**No key present:** metadata resolution fails with a clear
operator-facing reason (no silent skip that looks like "no matches").

**Secrets file unreadable:** the file lives under `NIGHTJAR_DATA_DIR` at
mode `0600`. If a bind-mounted volume (or uid mismatch) makes it
unreadable by the runtime user, fail with a **named error** that the
secrets file could not be read — not a silent fallthrough that looks like
"no key configured."

**Override present but rejected by TMDB (401/403 or equivalent):** named
refuse with an operator-facing reason that the configured key was
rejected. **Do not** fall back to the embedded key. Silent fallback would
make an operator believe they are on their own key when they are not
(same honesty class as P5 / missing-zscale named refuse). Clearing or
fixing the override is how they return to the embedded key.

**Embedded key revoked or rejected by TMDB:** the same operator-facing
named refuse (key rejected / unavailable). No silent degradation that
looks like empty search results. Recovery is override (§4) or a release
that embeds a live key (§6).

This path **must work before any built-in key exists**: contributor and
dev builds, and recovery after revocation. The application key is never
copied into the secrets file. The secrets file remains the v1 home for
third-party credentials (ADR-0026 §5); later providers (e.g.
OpenSubtitles) add fields there rather than a second store.

**Implementation status (2026-08-03):** §4 credential resolution is in
`nightjar-metadata` (`tmdb/credentials.rs`): secrets-file →
`NIGHTJAR_TMDB_API_KEY` → empty embedded slot (`option_env!
("NIGHTJAR_TMDB_APP_KEY")`, not injected yet). v3 api_key only; bearer
rejected at the boundary; named refuse on no key, unreadable secrets,
and TMDB 401/403 (override does not fall back to embedded). Legacy
`TMDB_API_KEY` / bearer env / `~/.config/nightjar/tmdb_secret` removed.
CI injection of the application key and Settings UI that writes the
secrets file remain later work.

### 5. Attribution

TMDB requires **source attribution** and the **TMDB logo** for **all**
use (live ToU §1.B and §3), not only free non-commercial use. Verified
2026-08-03 against TMDB's FAQ and logos & attribution page: attribution
belongs in an **About or Credits** section, and that is the specified
location, not a minimum to exceed with per-item chrome.

| Surface | Content |
|---|---|
| Settings → About (or equivalent always-reachable About / Credits) | Exact notice string below, plus the approved TMDB logo under the constraints below |

**Notice (exact, live ToU §3; "application" chosen from the
website/program/service/application/product options):**
`This application uses TMDB and the TMDB APIs but is not endorsed, certified, or otherwise approved by TMDB.`

**Naming:** refer to the source only as **TMDB** or **The Movie Database**.
No other name is acceptable.

**Link target:** links back point to `https://www.themoviedb.org`.

**Logo constraints:** approved TMDB logo only; unmodified in colour,
aspect ratio, and rotation; white, black, or the approved brand colours —
dark blue `#0d253f`, light blue `#01b4e4`, light green `#90cea1` (all
approved logo assets are blue variants); less prominent than the Nightjar
mark; must not imply endorsement.

**Implementation:** vendor the approved SVG into the repo; do not hotlink
TMDB's CDN asset. The About page must render identically offline.

`nightjar-meta/docs/COPY_DECK.md` needs a matching entry for that string,
naming rule, link target, and logo placement. **Not written yet — flag
for copy, not invented here.**

### 6. Rotation

| Event | Effect |
|---|---|
| CI secret rotated; new release built | New builds embed the new key. Matching works for upgraded installs with no user action. |
| Old builds after rotation, **old key still valid** | Keep working until TMDB invalidates the old key. |
| TMDB revokes the application key | Every install still using that key loses **live** metadata at once. Browse of already-cached rows is temporary only (≤6 months under §1.C) and ends under termination (§1.D). Override (§4) is how an install re-validates and stays compliant. |
| User on an old binary, key revoked, no override | Live metadata fails; cached browse is time-limited as above until they upgrade to a build with a live key **or** set an override. |

Rotation of the embedded key is a **release rebuild**, not a settings
toggle and not a server-pushed secret. We do not operate a key-distribution
service.

**Termination (verified 2026-08-03, live ToU §1.D):** on termination, cease
use of the APIs and all keys and promptly purge all TMDB content including
cached content.

- The obligation binds **Nightjar as licensee**. We cannot reach operators'
  disks; what we can do is ship the capability and document it.
- **No purge command in v1.** The provenance the refresh strategy needs —
  every stored field attributable to its source — is what would make a
  purge straightforward later. Mark **post-v1**.
- Do **not** open ADR-0029 work for this.

### 7. Not decided: TVDB

**Open before reading TVDB's terms: do we need TVDB at all?** TMDB carries
TV data; ~93% of the dogfood library is episodes; the parse baseline found
**zero** absolute-numbered anime files — the main case usually cited for
TVDB's alternate orderings. Dropping TVDB would remove a provider, a
licensing read, and this section. A **50-show TMDB coverage sample** would
answer it and **has not been run**.

If TVDB stays: its licensing and attribution model differ. Its terms have
**not** been read for this decision. Do not assume the TMDB answer (embed
+ override + attribution surfaces) applies. Open until a separate ADR or
an amendment here after the terms are read.

## Alternatives considered

**Require every operator to bring a TMDB key; no embedded key.** Rejected:
breaks works-by-default for Gate 3 metadata. Override remains mandatory
for source builds and revocation recovery.

**On override auth failure, fall back to the embedded key.** Rejected:
operator cannot tell which key is in use; same honesty class as silent
capability lies (P5 / zscale). Named refuse instead.

**Nightjar-hosted metadata proxy that holds the key server-side.**
Rejected for v1 (ADR-0026 / strategy note): rate limits are IP-keyed;
proxy concentrates load and does not remove the license problem for a
redistributed client that still needs a key relationship.

**Put the key in the git repo or a public CI log.** Rejected: forbids
rotation hygiene and treats the terms violation as a publishing strategy.

**Store the user key in SQLite.** Rejected: secrets file is the v1
credential home (ADR-0026 §5); tokens are not browse data.

## Consequences

- Release engineering owns `NIGHTJAR_TMDB_APP_KEY` in CI secrets and the
  rebuild path on rotation or revocation.
- Matcher / TMDB client implement precedence §4 (secrets → env →
  embedded), v3 api_key only, named refuse on rejected override **or**
  rejected/revoked embedded key, named error when secrets file is
  unreadable, fail closed when no key. §4 resolution replaces the
  measure-only `from_env` path; CI injection of the embedded key and
  Settings UI remain later.
- Web (and any client About / Credits surface) ships attribution §5
  before claiming TMDB-backed metadata in a release — About only, not
  per-item.
- COPY_DECK gets the exact ToU §3 notice string
  (`This application uses TMDB and the TMDB APIs but is not endorsed, certified, or otherwise approved by TMDB.`),
  logo placement, link target, and the naming rule (**TMDB** / **The Movie
  Database** only) as a **copy** constraint, not just UI chrome.
- ADR-0026 §5 remains the pipeline pointer; this ADR is the terms and
  distribution record. Do not re-decide the secrets-file shape in a third
  place.
- Block 1 refresh strategy must meet the **6-month** ToU §1.C ceiling
  (§1); cadence and staggering are decided there, not here.
- Termination purge capability is **post-v1** (§6); provenance for it
  rides the refresh strategy, not ADR-0029 reopen.
- TVDB: run or skip the 50-show TMDB coverage sample before investing in
  a TVDB terms read (§7).
- **Constitution hygiene (separate act):** Continuity's standing review
  rules 4.10–4.12 are not in `ENGINEERING_RULES.md`. Either amend the
  constitution at the next Rule 6.3 gate read-through (unanimous approval
  + commit message stating what prompted it), or stop citing them by
  number as binding. Not folded into accepting this ADR.
