# ADR index

The register. Every accepted, proposed, and superseded ADR, in number
order, with what supersedes what made visible here rather than only
inline. A filename alone does not tell you an ADR is dead in whole or in
part — this table exists so you do not have to open all forty-one to find
out.

Status here is the ADR's own `Status:` line, condensed. Where an ADR is
only *partially* superseded (one section, one item, one decision inside
a longer record), the table says so and the rest of that ADR still
stands — read the ADR itself for which parts, don't infer it from this
row. Full amendment history stays inside each ADR; this index tracks
supersession, not every amendment.

Newcomer reading order lives in [`../../CONTRIBUTING.md`](../../CONTRIBUTING.md),
not here — this page is the reference, not the tour.

| # | Title | Status | Supersedes | Superseded by |
|---|---|---|---|---|
| [0001](0001-client-platforms.md) | Client platform strategy | Superseded | — | [0021](0021-client-architecture.md) (whole) |
| [0002](0002-builtin-https-acme.md) | Built-in HTTPS / ACME | **Proposed** | — | — |
| [0003](0003-phase1-schema-api.md) | Phase 1 library schema and API shape | Partially superseded | — | §5 by [0006](0006-phase2-remux-decision.md) |
| [0004](0004-async-scan-jobs.md) | Async scan jobs and Gate 1 index-pass criterion | Accepted | — | — |
| [0005](0005-gate1-close-pi-carry.md) | Close Gate 1 with Pi hardware scan carried forward | Accepted | — | — |
| [0006](0006-phase2-remux-decision.md) | Phase 2 playback decision engine and remux delivery | Partially superseded | [0003](0003-phase1-schema-api.md) §5 | [0011](0011-remux-session-convergence.md) (delivery / remux job / cache / `remuxState`; decision-engine shape stands) |
| [0007](0007-hls-transcode-sessions.md) | HLS software-transcode sessions | Accepted | — | — |
| [0008](0008-abr-post-v1.md) | Adaptive bitrate is post-v1 | Accepted | — | — |
| [0009](0009-hw-accel-detection.md) | Hardware encode detection by verification | Accepted (amended 2026-08-03) | — | — |
| [0010](0010-text-subs-webvtt.md) | Text subtitle tracks as WebVTT sidecars | Partially superseded | — | §7 by [0013](0013-subtitle-extraction-at-scan.md) |
| [0011](0011-remux-session-convergence.md) | Remux converges onto the session model | Accepted | [0006](0006-phase2-remux-decision.md) (delivery decisions) | — |
| [0012](0012-audio-downmix-and-track-selection.md) | Audio downmix and multi-track selection | Accepted | — | — |
| [0013](0013-subtitle-extraction-at-scan.md) | Subtitle extraction at scan time | Partially superseded | [0010](0010-text-subs-webvtt.md) §7 | §1–§2 by [0041](0041-subtitle-classification-and-client-gated-extraction.md) (§3–§12 stand) |
| [0014](0014-library-availability.md) | Library availability and failure classification | Accepted | — | — |
| [0015](0015-library-discovery-scheduling.md) | Library discovery scheduling | Accepted (amended 2026-08-03, 2026-08-04) | — | — |
| [0016](0016-rejected-playlist-sole-authority.md) | Reject playlist-as-sole-authority seek rewrite | Accepted | — | — |
| [0017](0017-desktop-safari-hlsjs.md) | Desktop Safari attaches with hls.js | Accepted | — | — |
| [0018](0018-subtitle-burn-in.md) | Image and ASS subtitle burn-in | Partially superseded | — | §5 by [0019](0019-ass-burn-extract-at-scan.md) |
| [0019](0019-ass-burn-extract-at-scan.md) | ASS burn-in extract at scan time | Accepted | [0018](0018-subtitle-burn-in.md) §5 | — |
| [0020](0020-copy-mode-segment-boundaries.md) | Producer-owned segment boundaries (time-keyed) | Accepted | — | — |
| [0021](0021-client-architecture.md) | Client architecture (Flutter UI, per-platform engines) | Accepted (Apple engine closed 2026-08-06) | [0001](0001-client-platforms.md); prior Rule 2.4 wording | — |
| [0022](0022-capability-profiles.md) | Client capability profiles (bitrate, resolution, HDR) | Accepted | — | — |
| [0023](0023-cluster-map-byte-offset-start.md) | Keyframe map and byte-offset session start | Accepted (amended 2026-08-06) | — | — |
| [0024](0024-ranked-track-selection.md) | Ranked track selection | Accepted | — | — |
| [0025](0025-item-identity.md) | Item identity | Accepted | — | — |
| [0026](0026-metadata-pipeline.md) | Metadata pipeline | Partially superseded (amended 11×) | — | §8.4 item 3 by [0037](0037-kids-scoping-and-overrides.md) (in place) |
| [0027](0027-artwork-pipeline.md) | Artwork pipeline | Accepted (amended) | — | — |
| [0028](0028-manual-metadata-fix.md) | Manual metadata fix | Accepted | — | — |
| [0029](0029-canonical-metadata-and-item-links.md) | Canonical metadata store and file↔item links | Accepted (amended) | — | — |
| [0030](0030-library-relative-paths.md) | Library-relative media paths and library repoint | Accepted (amended 2026-08-04) | — | — |
| [0031](0031-tmdb-api-key-and-attribution.md) | TMDB API key distribution and attribution | Accepted | — | — |
| [0032](0032-multi-exact-collision-order.md) | Multi-exact collision resolution order | Accepted (amended 2×) | — | — |
| [0033](0033-series-identity.md) | Durable series identity | Accepted | — | — |
| [0034](0034-accounts-and-profiles.md) | Accounts and profiles | Partially superseded | — | item 2 by [0040](0040-account-roles.md) |
| [0035](0035-watch-state-and-continue-watching.md) | Watch state and continue-watching | Accepted | — | — |
| [0036](0036-playback-events.md) | Playback events (writer only in v1) | Accepted, condition unverified — item 1 | — | — |
| [0037](0037-kids-scoping-and-overrides.md) | Kids scoping and parent overrides | Accepted | [0026](0026-metadata-pipeline.md) §8.4 item 3 | — |
| [0038](0038-track-selection-persistence.md) | Track selection persistence | Accepted | — | — |
| [0039](0039-show-entity-and-series-key.md) | The show entity and `series_key` | Accepted | — | — |
| [0040](0040-account-roles.md) | Account roles | Accepted | — | — |
| [0041](0041-subtitle-classification-and-client-gated-extraction.md) | Subtitle classification and client-gated extraction | Accepted | [0013](0013-subtitle-extraction-at-scan.md) §1–§2 | — |
| [0042](0042-derived-artifact-versioning-and-reconciliation.md) | Derived artifact versioning and library reconciliation | **Proposed** | — | — |

## Reading this table

- **Partially superseded** means part of the ADR is dead and part still
  governs. Open the ADR for which part — most carry an inline marker at
  the superseded section (e.g. ADR-0034 item 2, ADR-0037 §8.4 item 3).
  ADR-0010 §7 and ADR-0018 §5 were missing that inline marker even
  though the superseding ADR names them — fixed in the same pass as
  this index (one blockquote each, not a rewrite of either section),
  since a reader who opens ADR-0010 directly and never sees this table
  still needs the warning.
- **`—` in Supersedes/Superseded by** means exactly what it says: no
  relationship recorded either direction, not "not checked."
- ADR-0002 is the one `Proposed` entry. See the note in `CONTRIBUTING.md`
  and below — it is not sign-off debt, it is an open decision every ADR
  that touches it (ADR-0034) correctly defers to rather than assumes.

## ADR-0001 and ADR-0002

Both were flagged in `nightjar-meta/docs/SLICE_CLOSEOUT.md`'s "ADR
hygiene" checklist as an illustration: *"Leaving 0001/0002 as `proposed`
while later ADRs assume them is how sign-off debt hides."* As of this
index:

- **ADR-0001 already carries its correct status** — `superseded by
  ADR-0021`, dated 2026-07-31. It is not `proposed`. The
  `SLICE_CLOSEOUT.md` illustration was stale on this half by about a
  week; corrected there in the companion `nightjar-meta` commit rather
  than left to keep citing a status that changed.
- **ADR-0002 is genuinely `proposed` and correctly so.** Its own
  Decision section says "Pending sign-off. Decide before Phase 3 HTTPS
  work." Nothing in `server/` assumes embedded ACME (checked: zero
  matches for ACME/Let's Encrypt in `server/`), and the one place another
  ADR touches it (ADR-0034, TLS as a bootstrap-window mitigation) states
  outright that "ADR-0002... is still `proposed`" and defers rather than
  assumes. This is an honestly open decision, not hidden debt — it
  should stay `proposed` until Phase 3 HTTPS work actually decides it,
  which is a decision for whoever owns that phase, not something this
  pass flips.

## ADR-0036's accept condition

ADR-0036 was accepted 2026-08-06 "on the condition that `close_reason`
and `playback_method` ship as closed enumerations rather than open
strings" (item 1) — the kind of condition that is easy to lose track of
once the accept lands. Checked before trusting the index's own "Accepted"
label: there is no `playback_events` table in any migration
(`server/crates/db/migrations/`, latest is 018, unrelated), no writer
module for it anywhere under `server/crates/` (`grep -rln "event"` across
the workspace returns nothing), and the only in-tree references to
`playback_events` are two defensive `table_exists` / conditional-rewrite
guards in `server/crates/metadata/src/migrator.rs` — future-proofing for
a table that does not exist yet, not evidence the ADR shipped.

So the condition is neither met nor violated: **nothing has been built
yet for it to apply to.** The design decision (accepted) and the
implementation status (not started) are different claims, the same
distinction ADR-0023 draws between "mechanism proven" and "population
covered." Whoever dispatches the Block 2 B2-C / B2-5 slice this ADR
names as its target needs to actually ship `close_reason` and
`playback_method` as closed enums, not open strings, or the accept
condition is violated retroactively the moment it does land — this is
the check to run then, not a thing this index can close now.
