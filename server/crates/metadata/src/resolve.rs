//! Resolve NFO first, then TMDB (ADR-0026 resolution path).

use rusqlite::{Connection, OptionalExtension, params};

use crate::canonical;
use crate::model::{CanonicalMetadata, MetadataKind};
use crate::negative_cache::{
    self, CacheKind, NegativeReason, PROVIDER_TMDB, now_rfc3339, query_key,
};
use crate::nfo::{NfoError, parse_nfo};
use crate::tmdb::RawProviderPayload;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum MetadataOrigin {
    Nfo,
    Tmdb,
}

#[derive(Debug, Clone, Default)]
pub struct ResolveInput {
    /// Raw NFO XML when a sidecar (or equivalent) is present.
    pub nfo_xml: Option<String>,
    /// Raw `tvshow.nfo` XML at the show root (Kodi/Jellyfin layout). Series
    /// identity only: show-level tmdb/imdb/tvdb ids, never episode fields —
    /// the episode NFO stays the display authority for the item beside it.
    pub tvshow_nfo_xml: Option<String>,
    /// NFO imdb id (`tt…`) carried to the provider for TMDB `/find`
    /// (strategy note §2 A2). Set from `tvshow.nfo`; episode NFO ids are
    /// episode-level and are never a show lookup (A4 is separate work).
    pub nfo_imdb_id: Option<String>,
    /// NFO tvdb id carried to the provider for TMDB `/find` (A3).
    pub nfo_tvdb_id: Option<i64>,
    /// Cleaned title for provider search when NFO is absent.
    pub title: Option<String>,
    pub year: Option<i32>,
    /// Series premiere year from the library (earliest episode year, else
    /// show-folder `(YYYY)`). Used to pin multi exact-title TV hits.
    pub library_year: Option<i32>,
    /// Distinct episode files / seasons under the show (TV collision pin).
    pub library_episode_count: Option<u32>,
    pub library_season_count: Option<u32>,
    /// `(season_number, file_count)` the folder holds — the per-season shape,
    /// distinct from the two counts above.
    pub folder_season_counts: Vec<(i32, u32)>,
    /// The season numbers the folder asserts, season 0 excluded. Coverage
    /// evidence for the year pin; the count above stays the pin signal.
    pub library_seasons: Vec<i32>,
    /// ADR-0032 reference episode for title pin (usable after-token only).
    pub ref_season: Option<i32>,
    pub ref_episode: Option<i32>,
    pub ref_episode_title: Option<String>,
    /// Every `(season, episode, title)` the folder's filenames supply, not just
    /// the ADR-0032 reference above.
    ///
    /// The reference episode is chosen by the **folder's** numbering, so on a
    /// folder holding several installments it can name a season the stored
    /// entity does not have — and a confirmation keyed on it then returns
    /// nothing rather than disagreeing. Measured: `Monster (2022)` agrees with
    /// its stored entity on **9 of 10** testable files, and on none of them if
    /// only the reference is consulted.
    pub folder_episode_titles: Vec<(i32, i32, String)>,
    /// Enrich by provider id (ADR-0026 §8.3): detail-only, never searches.
    /// When set, the negative cache is skipped and the provider fetches the
    /// detail payload for this id directly (no query key, no floor gate).
    pub tmdb_id: Option<i64>,
    /// Stored folder series identity (ADR-0033): the show id for this group's
    /// folder, read from the `series` table by the queue. Skips the title
    /// search when the already-persisted detail payload passes the folder
    /// name/year cross-check; disagreement falls through to search. Local
    /// read only — never a provider re-fetch.
    pub series_show_id: Option<i64>,
    /// Search target; episodes search as TV (ADR-0026).
    pub kind: Option<MetadataKind>,
}

/// Why an item stayed unmatched. Surfaced for the fix flow (ADR-0028); not a
/// log-only hard failure. A present-but-corrupt NFO must not fall through to
/// TMDB ("local data always wins").
#[derive(Debug, Clone, PartialEq)]
pub enum UnresolvedReason {
    /// No usable NFO and the provider returned nothing useful.
    NoMatch,
    /// NFO bytes were present but could not be parsed. Item stays unmatched
    /// with this reason until the user fixes the file or clears/retries.
    NfoInvalid { detail: String },
    /// Best search hit scored below the auto-match floor (ADR-0026 §2).
    /// Path `item_key` / fragile watch state until manual fix or better input.
    BelowThreshold { confidence: f64, method: String },
    /// The provider entity the match picked has zero episodes. A folder with
    /// files cannot bind to an entity with nothing to bind to; the entity is
    /// not a candidate (ADR-0026, amended).
    NoEpisodes,
}

impl UnresolvedReason {
    /// The token persisted in `media_items.metadata_unmatched_reason`.
    ///
    /// Named from the variant that produces it rather than from ADR-0043 §2's
    /// list, which was written before any writer existed: of its ten tokens
    /// three had producers, five named bind-time causes nothing computes, one
    /// was struck for naming a retired mechanism, and this set's `no_episodes`
    /// was missing from it entirely. §2's `no_show_candidate` is `no_match`
    /// here, after the variant.
    pub fn token(&self) -> &'static str {
        match self {
            Self::NoMatch => "no_match",
            Self::NfoInvalid { .. } => "nfo_invalid",
            Self::BelowThreshold { .. } => "below_threshold",
            Self::NoEpisodes => "no_episodes",
        }
    }
}

impl std::fmt::Display for UnresolvedReason {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::NoMatch => write!(f, "no match"),
            Self::NfoInvalid { detail } => write!(f, "invalid nfo: {detail}"),
            Self::BelowThreshold { confidence, method } => {
                write!(f, "below threshold: {confidence:.2} [{method}]")
            }
            Self::NoEpisodes => write!(f, "picked entity has no episodes"),
        }
    }
}

#[derive(Debug, Clone, PartialEq)]
pub enum ResolveOutcome {
    Resolved {
        metadata: Box<CanonicalMetadata>,
        source: MetadataOrigin,
        /// Scorer method / discriminator name (TMDB path). `None` for NFO.
        match_method: Option<String>,
        /// Whether episode titles agreed with the chosen candidate. `None`
        /// when nothing was compared — an NFO or id route never compares.
        confirmed: Option<bool>,
    },
    Unresolved {
        reason: UnresolvedReason,
    },
}

#[derive(Debug)]
pub enum ResolveError {
    /// Provider-level failure (network, auth, …). NFO parse problems are
    /// [`UnresolvedReason::NfoInvalid`], not this. Transient: the next drain
    /// pass retries it.
    Provider(String),
    /// A provider detail endpoint returned HTTP 404 for a **stored id**
    /// (ADR-0026 §8.4). The id itself is bad, not the network call, so this
    /// is terminal (`unmatched`), never a bare next-pass repeat of the call.
    NotFound(String),
}

impl std::fmt::Display for ResolveError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Provider(e) => write!(f, "provider error: {e}"),
            Self::NotFound(e) => write!(f, "not found: {e}"),
        }
    }
}

impl std::error::Error for ResolveError {}

/// Provider search/detail outcome. Kept to Hit / Below / Miss so the trait
/// stays thin (Rule 4.7) while still surfacing the floor gate for the fix flow.
#[derive(Debug, Clone, PartialEq)]
pub enum ProviderResult {
    Hit {
        metadata: Box<CanonicalMetadata>,
        /// Scorer method string (which table row / discriminator fired).
        method: &'static str,
        /// Whether episode titles agreed with the candidate that was chosen.
        /// `None` when nothing was compared. Separate from `method`, which
        /// answers *how was this entity chosen* — confirmation sometimes
        /// chooses and sometimes only agrees, and one field cannot say both.
        confirmed: Option<bool>,
        /// Entity-keyed raw body for ADR-0026 §4 persistence (`None` for stubs).
        raw: Option<RawProviderPayload>,
    },
    /// The picked provider entity has zero episodes; it is not a candidate
    /// (ADR-0026, amended). Terminal `unmatched` with a recorded reason —
    /// not a find miss, not a provider error.
    EmptyShell,
    BelowThreshold {
        confidence: f64,
        method: &'static str,
    },
    Miss,
    /// A `/find` id lookup (NFO external id) produced no accepted hit: the
    /// find call 404'd, or the found id's detail 404'd. Not terminal — the
    /// resolver clears the external id and falls through to a title search,
    /// gated on the negative cache.
    FindMiss,
}

/// One metadata backend (TMDB today; keep the trait thin — Rule 4.7).
/// Do the folder's filenames agree with the stored entity's episode titles?
///
/// **Agreement only, and no refutation path.** [`compare_episode_title`] cannot
/// return `Disagree` on a filename-derived title — a disagreement needs a
/// corroborating air date and filenames carry none — so absence of agreement is
/// never evidence against the id. A folder whose titles are generic, or whose
/// seasons were never fetched, yields nothing and the discard stands. That is
/// the intended outcome, not a gap: `Love on the Spectrum U.S` agrees on 0 of
/// 27 files and is the one stored id the name check is right to discard.
///
/// **Local reads only.** The entity's episode rows were persisted when the
/// folder bound, so this adds no provider request and a rescan issues the same
/// count as before (ADR-0033 item 1, Gate 3).
fn stored_id_confirmed_by_episode_titles(
    conn: &Connection,
    show_id: i64,
    folder_episodes: &[(i32, i32, String)],
    show_soft_key: &str,
) -> bool {
    if folder_episodes.is_empty() {
        return false;
    }
    let Ok(mut stmt) = conn.prepare(
        "SELECT title FROM metadata_canonical
         WHERE provider = ?1 AND entity_kind = 'episode'
           AND tmdb_show = ?2 AND season = ?3 AND episode = ?4",
    ) else {
        return false;
    };
    for (season, episode, file_title) in folder_episodes {
        let provider_title: Option<String> = stmt
            .query_row(params![PROVIDER_TMDB, show_id, season, episode], |r| {
                r.get(0)
            })
            .optional()
            .ok()
            .flatten();
        let Some(provider_title) = provider_title else {
            continue;
        };
        if crate::match_score::compare_episode_title(
            file_title.as_str(),
            &provider_title,
            show_soft_key,
            None,
            None,
        ) == crate::match_score::EpisodeTitleVerdict::Agree
        {
            return true;
        }
    }
    false
}

pub trait MetadataSource {
    fn resolve(&self, input: &ResolveInput) -> Result<ProviderResult, ResolveError>;

    /// Season detail for episode-id projection (ADR-0029). Default: unsupported
    /// (stubs / measures that only exercise search+show detail).
    fn fetch_season(
        &self,
        _show_id: i64,
        _season_number: i32,
    ) -> Result<Option<RawProviderPayload>, ResolveError> {
        Ok(None)
    }

    /// Entities other than `exclude_show_id` that might hold a folder's
    /// unplaced files, each with its **own** seasons already fetched
    /// (ADR-0046 item 4).
    ///
    /// Default: unsupported, matching [`Self::fetch_season`], so every stub
    /// and measure keeps compiling and a source that cannot search simply
    /// finds no second entity rather than failing the bind.
    ///
    /// **This is a title search and not a provider-graph walk.** There is no
    /// related-series edge to follow: `belongs_to_collection` is null on 697
    /// of 697 TV entities — it is movie-only — and recommendations are
    /// asymmetric and partial. Given 4454, nothing in its payload points at
    /// 74321.
    fn second_entity_candidates(
        &self,
        _title: &str,
        _exclude_show_id: i64,
    ) -> Result<Vec<SecondEntityCandidate>, ResolveError> {
        Ok(Vec::new())
    }
}

/// A provider entity a folder might additionally bind, with the shape needed
/// to test it against the folder's unplaced files.
#[derive(Debug, Clone)]
pub struct SecondEntityCandidate {
    pub tmdb_show_id: i64,
    pub shape: crate::match_score::CandidateShape,
}

/// Parses `input.nfo_xml` when present. Not a [`MetadataSource`]: corrupt NFO
/// must become [`UnresolvedReason::NfoInvalid`] in the resolver, not a trait
/// `Miss` that would look like "try TMDB next".
#[derive(Debug, Default, Clone, Copy)]
pub struct NfoSource;

enum NfoAttempt {
    Absent,
    Parsed(Box<CanonicalMetadata>),
    Invalid(NfoError),
}

impl NfoSource {
    fn attempt(self, input: &ResolveInput) -> NfoAttempt {
        self.attempt_xml(input.nfo_xml.as_deref())
    }

    fn attempt_xml(self, xml: Option<&str>) -> NfoAttempt {
        let Some(xml) = xml else {
            return NfoAttempt::Absent;
        };
        if xml.trim().is_empty() {
            return NfoAttempt::Absent;
        }
        match parse_nfo(xml) {
            Ok(meta) => NfoAttempt::Parsed(Box::new(meta)),
            Err(e) => NfoAttempt::Invalid(e),
        }
    }
}

/// Does the parsed NFO supply a TMDB id the search tier can actually store?
/// Movies need a TMDB movie id; TV needs a TMDB **show** id. An episode
/// `uniqueid` is an *episode* id (never a `tmdb:show:`), and TVDB/IMDB alone
/// has no TMDB API path — both must fall through to TMDB search instead of
/// landing a terminal `matched` with nothing enrichable.
/// Report a search failure, unless a malformed NFO already explained it.
///
/// A corrupt sidecar is carried past the NFO stage rather than ending
/// resolution, so an item whose NFO failed to parse still gets the search every
/// NFO-less folder gets. If that search also fails, the parse failure is the
/// reason worth surfacing: **it names something the user can fix**, and it is
/// the cause that came first. `below_threshold` on a folder whose NFO is
/// corrupt describes the symptom.
fn unresolved_or_nfo_invalid(
    nfo_invalid_detail: &Option<String>,
    reason: UnresolvedReason,
) -> ResolveOutcome {
    match nfo_invalid_detail {
        Some(detail) => ResolveOutcome::Unresolved {
            reason: UnresolvedReason::NfoInvalid {
                detail: detail.clone(),
            },
        },
        None => ResolveOutcome::Unresolved { reason },
    }
}

fn nfo_has_usable_id(meta: &CanonicalMetadata) -> bool {
    match meta.kind {
        MetadataKind::Movie => meta.ids.tmdb.is_some(),
        MetadataKind::Show => meta.ids.tmdb.is_some() || meta.ids.tmdb_show.is_some(),
        MetadataKind::Episode => meta.ids.tmdb_show.is_some(),
    }
}

pub struct Resolver<T> {
    pub tmdb: T,
}

impl Default for Resolver<crate::tmdb::TmdbStub> {
    fn default() -> Self {
        Self {
            tmdb: crate::tmdb::TmdbStub,
        }
    }
}

impl<T: MetadataSource> Resolver<T> {
    pub fn resolve(&self, input: &ResolveInput) -> Result<ResolveOutcome, ResolveError> {
        self.resolve_inner(input, None)
    }

    /// Resolve with ADR-0026 §3 negative cache and §4/ADR-0029 persistence.
    ///
    /// Cached `no_results` / `below_threshold` entries skip the provider until
    /// `next_retry_at`. Provider errors are **not** cached. Hits upsert the
    /// raw payload and canonical projection in one transaction.
    pub fn resolve_with_store(
        &self,
        input: &ResolveInput,
        conn: &Connection,
    ) -> Result<ResolveOutcome, ResolveError> {
        self.resolve_inner(input, Some(conn))
    }

    fn resolve_inner(
        &self,
        input: &ResolveInput,
        conn: Option<&Connection>,
    ) -> Result<ResolveOutcome, ResolveError> {
        // Series identity from tvshow.nfo outranks the episode NFO (strategy
        // note §2 A1–A3). Episode fields stay with the episode NFO, which is
        // read separately so tvshow.nfo never masks it (autopsy D5).
        let mut nfo_imdb_id: Option<String> = None;
        let mut nfo_tvdb_id: Option<i64> = None;
        // A parse failure is carried rather than returned. If search later
        // resolves the item, the malformed sidecar cost nothing and this is
        // dropped; if search also fails, it becomes the reported reason,
        // because "your NFO is corrupt" is more actionable to a user than
        // "nothing scored above the floor" and it is the cause that came
        // first.
        let mut nfo_invalid_detail: Option<String> = None;
        let tvshow_nfo = match input.kind {
            Some(MetadataKind::Episode) | Some(MetadataKind::Show) => {
                NfoSource.attempt_xml(input.tvshow_nfo_xml.as_deref())
            }
            _ => NfoAttempt::Absent,
        };
        match tvshow_nfo {
            NfoAttempt::Parsed(metadata) => {
                if nfo_has_usable_id(&metadata) {
                    return Ok(ResolveOutcome::Resolved {
                        metadata,
                        source: MetadataOrigin::Nfo,
                        // This *is* a route — the entity was chosen by the id
                        // the NFO carries — and it is the dominant one in a
                        // library written by a tool that emits TMDB ids. It
                        // reported `None` until 2026-08-15, which left the
                        // diagnostic column blank for most of the library.
                        match_method: Some("nfo_tvshow_tmdb_id".to_string()),
                        confirmed: None,
                    });
                }
                // Show identified but no TMDB id: carry imdb/tvdb for `/find`.
                nfo_imdb_id = metadata.ids.imdb.clone();
                nfo_tvdb_id = metadata.ids.tvdb;
            }
            NfoAttempt::Invalid(err) => {
                // A malformed NFO ends the *NFO candidate cascade* — it never
                // falls through to a different NFO file, which is the autopsy
                // D5 contract documented at `nfo_sidecar_xml`. It does not end
                // *resolution*. Search is still available and is exactly what a
                // folder with no NFO at all would get, so a corrupt sidecar
                // leaves the item no worse off than an absent one.
                //
                // Measured 2026-08-17: 890 of 1,146 unmatched files in a
                // library whose `tvshow.nfo` carried no TMDB id were this, 465
                // of them in one folder, because a template artifact repeats
                // across every episode. All had matchable filenames.
                nfo_invalid_detail.get_or_insert_with(|| err.to_string());
            }
            NfoAttempt::Absent => {}
        }

        match NfoSource.attempt(input) {
            NfoAttempt::Parsed(metadata) => {
                // NFO that cannot supply a usable TMDB id must fall through to
                // TMDB search: never land a terminal `matched` with nothing
                // enrichable stored (e.g. episode-only id, TVDB/IMDB only).
                if nfo_has_usable_id(&metadata) {
                    return Ok(ResolveOutcome::Resolved {
                        metadata,
                        source: MetadataOrigin::Nfo,
                        // The item's own sidecar, distinct from the show root's
                        // `tvshow.nfo` above. Two tokens rather than one,
                        // because a single `nfo` token would merge two routes
                        // and no query could separate them afterwards.
                        match_method: Some("nfo_item_tmdb_id".to_string()),
                        confirmed: None,
                    });
                }
                // Episode NFO external ids are episode-level (strategy note
                // §2 A4 is separate work); never a show lookup.
            }
            NfoAttempt::Invalid(err) => {
                nfo_invalid_detail.get_or_insert_with(|| err.to_string());
            }
            NfoAttempt::Absent => {}
        }

        let cache_kind = match input.kind.unwrap_or(MetadataKind::Movie) {
            MetadataKind::Movie => CacheKind::Movie,
            MetadataKind::Episode | MetadataKind::Show => CacheKind::Tv,
        };
        // Id resolve has no query key: the negative cache keys on search
        // queries only, so an id-driven enrich must never consult or skip on
        // a stale title miss (ADR-0026 §8.3).
        let qk = if input.tmdb_id.is_some() {
            None
        } else {
            input
                .title
                .as_deref()
                .filter(|t| !t.is_empty())
                .map(|t| query_key(t, input.year))
        };

        // ADR-0033 Q4: a folder with stored series identity caches under its
        // series id, not its title+year query key — a fold-colliding sibling
        // writes the same query for a different show, and one folder's miss
        // must never suppress the other's fall-through search. Title+year
        // keys remain for folders with no identity yet. An id-driven enrich
        // (tmdb_id set) never consults the cache, so it gets no key either.
        let series_key = input
            .series_show_id
            .filter(|_| input.tmdb_id.is_none())
            .map(negative_cache::series_cache_key);

        // ADR-0033 §8: a folder with stored series identity skips the title
        // search but must pass a folder-level name/year cross-check against
        // the already-persisted detail payload before binding — a local read,
        // never a provider re-fetch. Disagreement (or a missing stored
        // detail) clears the id and falls through to search; a wrong stored
        // id never wins. The cross-check reuses the one TV title-match
        // predicate (`find_hit_reject_reason`), so a reused id follows the
        // same name/year gate a `/find` external id does.
        if let Some(series_show_id) = input.series_show_id
            && input.tmdb_id.is_none()
            && let Some(conn) = conn
            && let Some(meta) =
                canonical::get_canonical(conn, PROVIDER_TMDB, "tv", &series_show_id.to_string())
                    .map_err(ResolveError::Provider)?
        {
            // ADR-0026 (amended): a stored detail reporting zero episodes is
            // not a candidate, so the stored id is discarded exactly like a
            // name/year disagreement. A missing payload or a missing field is
            // unknown and never rejects.
            let stored_has_no_episodes = crate::raw_payload::get_raw_payload(
                conn,
                PROVIDER_TMDB,
                "tv",
                &series_show_id.to_string(),
            )
            .map_err(ResolveError::Provider)?
            .as_deref()
            .is_some_and(|p| crate::tmdb::tv_payload_episode_count(p) == Some(0));
            let name_reject = crate::match_score::find_hit_reject_reason(
                &meta,
                crate::match_score::SearchKind::Tv,
                input.title.as_deref().unwrap_or_default(),
                input.year,
            );
            // Issue 121. The cross-check's name predicate is right about one
            // binding in eleven on the measured library, and the ten it is
            // wrong about include `Monster (2022)`, whose stored entity agrees
            // on 9 of 10 episode titles. Discarding a confirmed id is what
            // sends the folder into a re-search it does not need, and the
            // re-search is where it acquires a wrong entity.
            //
            // Episode titles are per-file evidence and cost nothing here: the
            // stored entity's episodes were persisted when the folder bound,
            // so this is a local read. That asymmetry is why this works where
            // rebinding a *claimant* did not — the claimant's episode list is
            // never stored, the incumbent's always is.
            let confirmed = name_reject.is_some()
                && stored_id_confirmed_by_episode_titles(
                    conn,
                    series_show_id,
                    &input.folder_episode_titles,
                    input.title.as_deref().unwrap_or_default(),
                );
            if let Some(reason) = name_reject.filter(|_| !confirmed) {
                eprintln!(
                    "  discard stored series id {series_show_id} — {reason}; falling through to search"
                );
            } else if stored_has_no_episodes {
                eprintln!(
                    "  discard stored series id {series_show_id} — stored detail has 0 episodes; falling through to search"
                );
            } else {
                // One hit path (Rule 4.11): the same persist and
                // negative-cache clear a fresh provider match performs, then
                // the same `Resolved` shape the queue applies links and
                // poster-warm from. A rescan of an unchanged library issues
                // zero requests (Gate 3 "no search requests" survives).
                if let Some(qk) = &qk {
                    negative_cache::clear(conn, PROVIDER_TMDB, cache_kind, qk)
                        .map_err(ResolveError::Provider)?;
                }
                // The series-id row too: a miss recorded under this folder's
                // identity must not suppress the next cross-check-failure
                // search after a successful re-bind.
                negative_cache::clear(
                    conn,
                    PROVIDER_TMDB,
                    cache_kind,
                    &negative_cache::series_cache_key(series_show_id),
                )
                .map_err(ResolveError::Provider)?;
                let tx = conn.unchecked_transaction().map_err(|e| {
                    ResolveError::Provider(format!("begin series-row persist tx: {e}"))
                })?;
                canonical::upsert_canonical(&tx, PROVIDER_TMDB, &meta)
                    .map_err(ResolveError::Provider)?;
                tx.commit().map_err(|e| {
                    ResolveError::Provider(format!("commit series-row persist: {e}"))
                })?;
                return Ok(ResolveOutcome::Resolved {
                    metadata: Box::new(meta),
                    source: MetadataOrigin::Tmdb,
                    // This route reaches here either by the name cross-check
                    // passing or by episode titles rescuing an id it rejected.
                    // Only the second compared titles.
                    confirmed: confirmed.then_some(true),
                    // Two routes reach this return and they are not the same
                    // event. One is the cross-check passing. The other is the
                    // cross-check *rejecting* on the folder name and episode
                    // titles rescuing the id — which is precisely the "how
                    // often does the name gate reject something that should
                    // have been kept" question the diagnostic column exists to
                    // answer. A shared token would make the column blind to the
                    // thing it is for.
                    match_method: Some(
                        if confirmed {
                            "series_row_title_confirmed"
                        } else {
                            "series_row"
                        }
                        .to_string(),
                    ),
                });
            }
        }

        let mut attempt = ResolveInput {
            nfo_imdb_id: nfo_imdb_id.or(input.nfo_imdb_id.clone()),
            nfo_tvdb_id: nfo_tvdb_id.or(input.nfo_tvdb_id),
            series_show_id: None,
            ..input.clone()
        };
        // The key the fall-through search consults and records under: the
        // folder's series id once identity is stored (ADR-0033 Q4), else the
        // title+year query key.
        let cache_key: Option<&str> = series_key.as_deref().or(qk.as_deref());
        // Two attempts at most: `/find` first (an id lookup a stale title
        // miss must not suppress, autopsy D4), then — only when the find
        // produced no accepted hit — a plain title search. The search attempt
        // is negative-cache gated: a live `below_threshold` / `no_results`
        // row suppresses it even after a find miss, so a rescan before
        // `next_retry_at` does not re-run find+search (ADR-0026 §3).
        let now = now_rfc3339();
        for _round in 0..2 {
            if attempt.nfo_imdb_id.is_none()
                && attempt.nfo_tvdb_id.is_none()
                && let (Some(conn), Some(qk)) = (conn, cache_key)
                && let Ok(Some(entry)) =
                    negative_cache::should_skip(conn, PROVIDER_TMDB, cache_kind, qk, &now)
            {
                return Ok(unresolved_or_nfo_invalid(
                    &nfo_invalid_detail,
                    match entry.reason {
                        NegativeReason::BelowThreshold => UnresolvedReason::BelowThreshold {
                            confidence: entry.confidence.unwrap_or(0.0),
                            method: "negative_cache".into(),
                        },
                        NegativeReason::NoResults | NegativeReason::ApiError => {
                            UnresolvedReason::NoMatch
                        }
                    },
                ));
            }
            let result = self.tmdb.resolve(&attempt)?;
            let (metadata, method, confirmed, raw) = match result {
                ProviderResult::Hit {
                    metadata,
                    method,
                    confirmed,
                    raw,
                } => (metadata, method, confirmed, raw),
                ProviderResult::EmptyShell => {
                    return Ok(unresolved_or_nfo_invalid(
                        &nfo_invalid_detail,
                        UnresolvedReason::NoEpisodes,
                    ));
                }
                ProviderResult::BelowThreshold { confidence, method } => {
                    if let (Some(conn), Some(qk)) = (conn, cache_key) {
                        let _ = negative_cache::record_miss(
                            conn,
                            PROVIDER_TMDB,
                            cache_kind,
                            qk,
                            NegativeReason::BelowThreshold,
                            Some(confidence),
                            &now_rfc3339(),
                        );
                    }
                    return Ok(unresolved_or_nfo_invalid(
                        &nfo_invalid_detail,
                        UnresolvedReason::BelowThreshold {
                            confidence,
                            method: method.to_string(),
                        },
                    ));
                }
                ProviderResult::Miss => {
                    if let (Some(conn), Some(qk)) = (conn, cache_key) {
                        let _ = negative_cache::record_miss(
                            conn,
                            PROVIDER_TMDB,
                            cache_kind,
                            qk,
                            NegativeReason::NoResults,
                            None,
                            &now_rfc3339(),
                        );
                    }
                    return Ok(unresolved_or_nfo_invalid(
                        &nfo_invalid_detail,
                        UnresolvedReason::NoMatch,
                    ));
                }
                ProviderResult::FindMiss => {
                    // The `/find` id lookup produced no accepted hit (find
                    // 404, or the found id's detail 404): clear the external
                    // id so the next attempt is a plain title search — a
                    // wrong external id must fail into search, not win.
                    attempt.nfo_imdb_id = None;
                    attempt.nfo_tvdb_id = None;
                    continue;
                }
            };
            if (attempt.nfo_imdb_id.is_some() || attempt.nfo_tvdb_id.is_some())
                && matches!(method, "nfo_imdb_find" | "nfo_tvdb_find")
                && let Some(reason) = crate::match_score::find_hit_reject_reason(
                    &metadata,
                    crate::match_score::SearchKind::Tv,
                    attempt.title.as_deref().unwrap_or_default(),
                    attempt.year,
                )
            {
                eprintln!(
                    "  discard /find hit {} — {reason}; falling through to search",
                    attempt.title.as_deref().unwrap_or("?")
                );
                attempt.nfo_imdb_id = None;
                attempt.nfo_tvdb_id = None;
                continue;
            }
            if let (Some(conn), Some(raw)) = (conn, raw.as_ref()) {
                if let Some(ref qk) = qk {
                    let _ = negative_cache::clear(conn, PROVIDER_TMDB, cache_kind, qk);
                }
                if let Some(ref key) = series_key {
                    // The stored identity's row is dead once a fresh match
                    // lands: the queue re-keys the folder's series row to the
                    // hit, and its old series-id miss must not linger.
                    let _ = negative_cache::clear(conn, PROVIDER_TMDB, cache_kind, key);
                }
                canonical::persist_mapped_hit(conn, PROVIDER_TMDB, raw, &metadata)
                    .map_err(ResolveError::Provider)?;
            }
            return Ok(ResolveOutcome::Resolved {
                metadata,
                source: MetadataOrigin::Tmdb,
                match_method: Some(method.to_string()),
                confirmed,
            });
        }
        unreachable!("a /find outcome (discard or miss) is followed by at most one search attempt")
    }
}

/// Convenience: default NFO + TMDB stub resolver.
pub fn resolve(input: &ResolveInput) -> Result<ResolveOutcome, ResolveError> {
    Resolver::default().resolve(input)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::tmdb::TmdbStub;
    use nightjar_db::migrate;
    use rusqlite::{Connection, params};
    use std::cell::Cell;

    fn fixture(name: &str) -> String {
        let path = format!("{}/tests/fixtures/{name}", env!("CARGO_MANIFEST_DIR"));
        std::fs::read_to_string(&path).unwrap_or_else(|e| panic!("read {path}: {e}"))
    }

    #[test]
    fn prefers_nfo_over_tmdb_stub() {
        let outcome = resolve(&ResolveInput {
            nfo_xml: Some(fixture("movie.nfo")),
            ..Default::default()
        })
        .unwrap();
        match outcome {
            ResolveOutcome::Resolved {
                source,
                metadata,
                match_method,
                confirmed: _,
            } => {
                assert_eq!(source, MetadataOrigin::Nfo);
                assert_eq!(metadata.title, "Fight Club");
                assert_eq!(match_method.as_deref(), Some("nfo_item_tmdb_id"));
            }
            ResolveOutcome::Unresolved { .. } => panic!("expected NFO resolve"),
        }
    }

    #[test]
    fn unresolved_without_nfo_when_tmdb_is_stub() {
        let outcome = Resolver { tmdb: TmdbStub }
            .resolve(&ResolveInput {
                nfo_xml: None,
                ..Default::default()
            })
            .unwrap();
        assert_eq!(
            outcome,
            ResolveOutcome::Unresolved {
                reason: UnresolvedReason::NoMatch
            }
        );
    }

    /// A corrupt sidecar must not cost the item its search.
    ///
    /// Replaces `malformed_nfo_is_unresolved_reason_not_tmdb_fallback`, which
    /// asserted the opposite contract. That test kept passing after the change
    /// **for the wrong reason**: `TmdbStub` always misses, so the search it
    /// was asserting could not happen produced nothing either way. A provider
    /// that can actually hit is what makes the difference observable.
    #[test]
    fn malformed_nfo_falls_through_to_search_and_can_still_resolve() {
        let src = CountingHit {
            search_calls: Cell::new(0),
        };
        let outcome = Resolver { tmdb: src }
            .resolve(&ResolveInput {
                nfo_xml: Some(fixture("malformed.nfo")),
                title: Some("Fight Club".into()),
                year: Some(1999),
                kind: Some(MetadataKind::Movie),
                ..Default::default()
            })
            .unwrap();
        match outcome {
            ResolveOutcome::Resolved { source, .. } => {
                assert_eq!(source, MetadataOrigin::Tmdb, "search must have run");
            }
            other => panic!("expected the search result, got {other:?}"),
        }
    }

    /// And when search fails too, the parse failure is the reason reported —
    /// not `no_match`. It names something the user can fix, and it came first.
    #[test]
    fn malformed_nfo_is_the_reported_reason_when_search_also_fails() {
        let outcome = resolve(&ResolveInput {
            nfo_xml: Some(fixture("malformed.nfo")),
            title: Some("Fight Club".into()),
            year: Some(1999),
            kind: Some(MetadataKind::Movie),
            ..Default::default()
        })
        .unwrap();
        match outcome {
            ResolveOutcome::Unresolved {
                reason: UnresolvedReason::NfoInvalid { detail },
            } => {
                assert!(!detail.is_empty());
            }
            other => panic!("expected NfoInvalid, got {other:?}"),
        }
    }

    /// Always-miss provider that counts how many times it was asked.
    struct CountingMiss {
        calls: Cell<usize>,
    }

    impl MetadataSource for CountingMiss {
        fn resolve(&self, _input: &ResolveInput) -> Result<ProviderResult, ResolveError> {
            self.calls.set(self.calls.get() + 1);
            Ok(ProviderResult::Miss)
        }
    }

    #[test]
    fn second_resolve_issues_zero_provider_requests_for_cached_misses() {
        let conn = Connection::open_in_memory().unwrap();
        migrate(&conn).unwrap();

        let resolver = Resolver {
            tmdb: CountingMiss {
                calls: Cell::new(0),
            },
        };

        // Fixture set: unmatchable filenames (no NFO, titles that miss).
        let fixtures = [
            ResolveInput {
                title: Some("ZzNightjarUnmatchableAlpha2099".into()),
                year: Some(2099),
                kind: Some(MetadataKind::Movie),
                ..Default::default()
            },
            ResolveInput {
                title: Some("ZzNightjarUnmatchableBeta".into()),
                year: None,
                kind: Some(MetadataKind::Movie),
                ..Default::default()
            },
            ResolveInput {
                title: Some("ZzNightjarUnmatchableShow".into()),
                year: None,
                kind: Some(MetadataKind::Episode),
                ..Default::default()
            },
        ];

        for input in &fixtures {
            let out = resolver.resolve_with_store(input, &conn).unwrap();
            assert!(matches!(
                out,
                ResolveOutcome::Unresolved {
                    reason: UnresolvedReason::NoMatch
                }
            ));
        }
        let after_first = resolver.tmdb.calls.get();
        assert_eq!(after_first, fixtures.len());

        for input in &fixtures {
            let out = resolver.resolve_with_store(input, &conn).unwrap();
            assert!(matches!(
                out,
                ResolveOutcome::Unresolved {
                    reason: UnresolvedReason::NoMatch
                }
            ));
        }
        assert_eq!(
            resolver.tmdb.calls.get(),
            after_first,
            "second run must issue zero provider requests for cached misses"
        );
    }

    /// Id-only provider: counts search-style resolve calls (no `tmdb_id`) and
    /// serves a hit for id resolves — enrichment must never re-search.
    struct IdOnlyCounting {
        search_calls: Cell<usize>,
    }

    impl MetadataSource for IdOnlyCounting {
        fn resolve(&self, input: &ResolveInput) -> Result<ProviderResult, ResolveError> {
            let Some(id) = input.tmdb_id else {
                self.search_calls.set(self.search_calls.get() + 1);
                return Ok(ProviderResult::Miss);
            };
            Ok(ProviderResult::Hit {
                metadata: Box::new(CanonicalMetadata {
                    kind: MetadataKind::Movie,
                    title: "Fight Club".into(),
                    original_title: None,
                    year: Some(1999),
                    air_date: None,
                    plot: None,
                    genres: Vec::new(),
                    runtime_minutes: None,
                    cast: Vec::new(),
                    ratings: Vec::new(),
                    ids: crate::model::ProviderIds {
                        tmdb: Some(id),
                        tmdb_show: None,
                        imdb: None,
                        tvdb: None,
                    },
                    artwork: Vec::new(),
                    collection: None,
                    season: None,
                    episode: None,
                }),
                method: "tmdb_id",
                confirmed: None,
                raw: None,
            })
        }
    }

    /// Always-hit provider that counts search-tier calls (no `tmdb_id`).
    struct CountingHit {
        search_calls: Cell<usize>,
    }

    impl CountingHit {
        fn hit_meta(id: i64) -> CanonicalMetadata {
            CanonicalMetadata {
                kind: MetadataKind::Movie,
                title: "Fight Club".into(),
                original_title: None,
                year: Some(1999),
                air_date: None,
                plot: None,
                genres: Vec::new(),
                runtime_minutes: None,
                cast: Vec::new(),
                ratings: Vec::new(),
                ids: crate::model::ProviderIds {
                    tmdb: Some(id),
                    tmdb_show: None,
                    imdb: None,
                    tvdb: None,
                },
                artwork: Vec::new(),
                collection: None,
                season: None,
                episode: None,
            }
        }
    }

    impl MetadataSource for CountingHit {
        fn resolve(&self, input: &ResolveInput) -> Result<ProviderResult, ResolveError> {
            let id = match input.tmdb_id {
                Some(id) => id,
                None => {
                    self.search_calls.set(self.search_calls.get() + 1);
                    42
                }
            };
            Ok(ProviderResult::Hit {
                metadata: Box::new(Self::hit_meta(id)),
                method: "test",
                confirmed: None,
                raw: None,
            })
        }
    }

    #[test]
    fn nfo_without_usable_id_falls_through_to_tmdb_search() {
        // TVDB-only movie NFO: no TMDB id, so TMDB search must run — the NFO
        // alone must not land a terminal Resolved (nothing enrichable stored).
        let src = CountingHit {
            search_calls: Cell::new(0),
        };
        let outcome = Resolver { tmdb: src }
            .resolve(&ResolveInput {
                nfo_xml: Some(
                    r#"<movie><title>Fight Club</title><year>1999</year>
                       <uniqueid type="tvdb">361</uniqueid></movie>"#
                        .into(),
                ),
                title: Some("Fight Club".into()),
                year: Some(1999),
                kind: Some(MetadataKind::Movie),
                ..Default::default()
            })
            .unwrap();
        match outcome {
            ResolveOutcome::Resolved {
                metadata,
                source,
                match_method,
                confirmed: _,
            } => {
                assert_eq!(source, MetadataOrigin::Tmdb);
                assert_eq!(metadata.ids.tmdb, Some(42));
                assert!(match_method.is_some());
            }
            other => panic!("expected TMDB-resolved fall-through, got {other:?}"),
        }
    }

    #[test]
    fn episode_nfo_with_only_episode_id_falls_through_to_tmdb_search() {
        // episodedetails.nfo `uniqueid` is an *episode* id — not a usable show
        // id, so the group must fall through to a TV search, not resolve.
        let src = CountingHit {
            search_calls: Cell::new(0),
        };
        let outcome = Resolver { tmdb: src }
            .resolve(&ResolveInput {
                nfo_xml: Some(
                    r#"<episodedetails><title>Pilot</title><showtitle>Breaking Bad</showtitle>
                       <season>1</season><episode>1</episode>
                       <uniqueid type="tmdb">62085</uniqueid></episodedetails>"#
                        .into(),
                ),
                title: Some("Breaking Bad".into()),
                year: None,
                kind: Some(MetadataKind::Episode),
                ..Default::default()
            })
            .unwrap();
        assert!(
            matches!(
                outcome,
                ResolveOutcome::Resolved {
                    source: MetadataOrigin::Tmdb,
                    ..
                }
            ),
            "episode-id-only NFO must fall through to TMDB search, got {outcome:?}"
        );
    }

    #[test]
    fn nfo_with_usable_tmdb_id_never_calls_provider() {
        let resolver = Resolver {
            tmdb: CountingHit {
                search_calls: Cell::new(0),
            },
        };
        let outcome = resolver
            .resolve(&ResolveInput {
                nfo_xml: Some(fixture("movie.nfo")),
                title: Some("Fight Club".into()),
                year: Some(1999),
                kind: Some(MetadataKind::Movie),
                ..Default::default()
            })
            .unwrap();
        match outcome {
            ResolveOutcome::Resolved {
                metadata,
                source,
                match_method,
                confirmed: _,
            } => {
                assert_eq!(source, MetadataOrigin::Nfo);
                assert_eq!(metadata.ids.tmdb, Some(550));
                // Was `None` until 2026-08-15. The NFO's own id *is* how the
                // entity was chosen, and reporting nothing left the diagnostic
                // column blank for most of a Sonarr-managed library — 22,096 of
                // 25,212 ready items on the measured run. The assertion moves
                // with the decision rather than pinning the gap in place.
                assert_eq!(match_method.as_deref(), Some("nfo_item_tmdb_id"));
            }
            other => panic!("expected NFO resolve, got {other:?}"),
        }
        assert_eq!(
            resolver.tmdb.search_calls.get(),
            0,
            "usable NFO id must short-circuit the provider"
        );
    }

    #[test]
    fn resolve_by_tmdb_id_never_searches() {
        let resolver = Resolver {
            tmdb: IdOnlyCounting {
                search_calls: Cell::new(0),
            },
        };
        let out = resolver
            .resolve(&ResolveInput {
                // Even with a search title present, the id short-circuits.
                tmdb_id: Some(550),
                title: Some("Fight Club".into()),
                year: Some(1999),
                kind: Some(MetadataKind::Movie),
                ..Default::default()
            })
            .unwrap();
        match out {
            ResolveOutcome::Resolved {
                metadata,
                source,
                match_method,
                confirmed: _,
            } => {
                assert_eq!(metadata.ids.tmdb, Some(550));
                assert_eq!(metadata.title, "Fight Club");
                assert_eq!(source, MetadataOrigin::Tmdb);
                assert_eq!(match_method.as_deref(), Some("tmdb_id"));
            }
            other => panic!("expected resolved, got {other:?}"),
        }
        assert_eq!(
            resolver.tmdb.search_calls.get(),
            0,
            "id resolve must never issue a search"
        );
    }

    /// Provider that emulates the `/find` protocol: with an NFO external id it
    /// returns the *wrong* show (name and year disagree with the folder); with
    /// the id cleared it searches and hits the right show.
    struct WrongFindThenSearch {
        calls: Cell<usize>,
    }

    fn show_hit(id: i64, title: &str, year: Option<i32>, method: &'static str) -> ProviderResult {
        ProviderResult::Hit {
            metadata: Box::new(CanonicalMetadata {
                kind: MetadataKind::Show,
                title: title.into(),
                original_title: None,
                year,
                air_date: None,
                plot: None,
                genres: Vec::new(),
                runtime_minutes: None,
                cast: Vec::new(),
                ratings: Vec::new(),
                ids: crate::model::ProviderIds {
                    tmdb: Some(id),
                    tmdb_show: Some(id),
                    imdb: None,
                    tvdb: None,
                },
                artwork: Vec::new(),
                collection: None,
                season: None,
                episode: None,
            }),
            method,
            confirmed: None,
            raw: Some(crate::tmdb::RawProviderPayload {
                entity_kind: "tv".into(),
                provider_id: id.to_string(),
                payload: format!(r#"{{"id":{id},"name":"{title}"}}"#),
            }),
        }
    }

    impl MetadataSource for WrongFindThenSearch {
        fn resolve(&self, input: &ResolveInput) -> Result<ProviderResult, ResolveError> {
            self.calls.set(self.calls.get() + 1);
            if input.nfo_tvdb_id.is_some() {
                return Ok(show_hit(999, "Wrong Show", Some(1999), "nfo_tvdb_find"));
            }
            Ok(show_hit(55, "Alpha", Some(2002), "exact_title"))
        }
    }

    /// RC5: `/find` returns a show whose name and year do not match the
    /// folder — the resolve discards the id, falls through to search, and the
    /// wrong id is never written (autopsy D4).
    #[test]
    fn find_hit_wrong_name_year_falls_through_to_search_and_never_writes_wrong_id() {
        let src = WrongFindThenSearch {
            calls: Cell::new(0),
        };
        let resolver = Resolver { tmdb: src };
        let conn = Connection::open_in_memory().unwrap();
        migrate(&conn).unwrap();
        let input = ResolveInput {
            tvshow_nfo_xml: Some(
                r#"<tvshow><title>Alpha</title>
                   <uniqueid type="tvdb">73762</uniqueid></tvshow>"#
                    .into(),
            ),
            title: Some("Alpha".into()),
            year: Some(2002),
            kind: Some(MetadataKind::Episode),
            ..Default::default()
        };
        let outcome = resolver.resolve_with_store(&input, &conn).unwrap();
        match outcome {
            ResolveOutcome::Resolved {
                metadata,
                source,
                match_method,
                confirmed: _,
            } => {
                assert_eq!(source, MetadataOrigin::Tmdb);
                assert_eq!(metadata.ids.tmdb, Some(55), "search result wins");
                assert_eq!(match_method.as_deref(), Some("exact_title"));
            }
            other => panic!("expected search resolve, got {other:?}"),
        }
        assert_eq!(
            resolver.tmdb.calls.get(),
            2,
            "the discarded /find hit is followed by exactly one search"
        );
        let wrong: i64 = conn
            .query_row(
                "SELECT COUNT(*) FROM metadata_canonical
                 WHERE provider = 'tmdb' AND provider_id = '999'",
                [],
                |r| r.get(0),
            )
            .unwrap();
        assert_eq!(wrong, 0, "the wrong /find id must never be written");
        let right: i64 = conn
            .query_row(
                "SELECT COUNT(*) FROM metadata_canonical
                 WHERE provider = 'tmdb' AND provider_id = '55'",
                [],
                |r| r.get(0),
            )
            .unwrap();
        assert_eq!(right, 1, "the search hit is persisted");
    }

    /// Provider that resolves a matching `/find` hit in one call (no search).
    struct FindOkSource {
        calls: Cell<usize>,
    }

    impl MetadataSource for FindOkSource {
        fn resolve(&self, input: &ResolveInput) -> Result<ProviderResult, ResolveError> {
            self.calls.set(self.calls.get() + 1);
            let id = if input.nfo_tvdb_id.is_some() { 45 } else { 77 };
            let method = if input.nfo_tvdb_id.is_some() {
                "nfo_tvdb_find"
            } else {
                "exact_title"
            };
            Ok(show_hit(id, "Top Gear", Some(2002), method))
        }
    }

    /// RC5: a stale `below_threshold` neg-cache row for the group's title
    /// does not prevent the `/find` call — the id attempt runs ahead of the
    /// cache check (autopsy D4).
    #[test]
    fn stale_below_threshold_row_does_not_suppress_find() {
        fn seed_stale_row(conn: &Connection) {
            let key = query_key("Top Gear", Some(2002));
            conn.execute(
                "INSERT INTO metadata_negative_cache
                   (provider, kind, query_key, reason, confidence, attempt_count,
                    attempted_at, next_retry_at, cleaner_version)
                 VALUES ('tmdb', 'tv', ?1, 'below_threshold', 0.72, 3,
                         '2026-01-01T00:00:00Z', '2999-01-01T00:00:00Z', ?2)",
                params![key, negative_cache::CLEANER_VERSION],
            )
            .unwrap();
        }

        let conn = Connection::open_in_memory().unwrap();
        migrate(&conn).unwrap();
        seed_stale_row(&conn);

        let src = FindOkSource {
            calls: Cell::new(0),
        };
        let resolver = Resolver { tmdb: src };
        let input = ResolveInput {
            tvshow_nfo_xml: Some(
                r#"<tvshow><title>Top Gear</title>
                   <uniqueid type="tvdb">7940</uniqueid></tvshow>"#
                    .into(),
            ),
            title: Some("Top Gear".into()),
            year: Some(2002),
            kind: Some(MetadataKind::Episode),
            ..Default::default()
        };
        let outcome = resolver.resolve_with_store(&input, &conn).unwrap();
        match outcome {
            ResolveOutcome::Resolved { match_method, .. } => {
                assert_eq!(match_method.as_deref(), Some("nfo_tvdb_find"));
            }
            other => panic!("the cached miss must not suppress /find, got {other:?}"),
        }
        assert_eq!(resolver.tmdb.calls.get(), 1, "one find call, no cache skip");

        // Control on a fresh DB: without the external id the same row *is*
        // live and suppresses the provider — proving the row is real and that
        // only the id lookup bypassed it.
        let conn = Connection::open_in_memory().unwrap();
        migrate(&conn).unwrap();
        seed_stale_row(&conn);
        let control = Resolver {
            tmdb: FindOkSource {
                calls: Cell::new(0),
            },
        };
        let suppressed = control
            .resolve_with_store(
                &ResolveInput {
                    title: Some("Top Gear".into()),
                    year: Some(2002),
                    kind: Some(MetadataKind::Episode),
                    ..Default::default()
                },
                &conn,
            )
            .unwrap();
        assert!(
            matches!(
                suppressed,
                ResolveOutcome::Unresolved {
                    reason: UnresolvedReason::BelowThreshold { .. }
                }
            ),
            "the seeded row must suppress a plain search: {suppressed:?}"
        );
        assert_eq!(
            control.tmdb.calls.get(),
            0,
            "cache hit, zero provider calls"
        );
    }

    /// A `/find` hit that agrees with the folder is accepted in one call —
    /// the cross-check must not discard a correct external id.
    #[test]
    fn find_hit_matching_show_is_accepted_without_search() {
        let src = FindOkSource {
            calls: Cell::new(0),
        };
        let resolver = Resolver { tmdb: src };
        let outcome = resolver
            .resolve(&ResolveInput {
                tvshow_nfo_xml: Some(
                    r#"<tvshow><title>Top Gear</title>
                       <uniqueid type="tvdb">7940</uniqueid></tvshow>"#
                        .into(),
                ),
                title: Some("Top Gear".into()),
                year: Some(2002),
                kind: Some(MetadataKind::Episode),
                ..Default::default()
            })
            .unwrap();
        match outcome {
            ResolveOutcome::Resolved {
                metadata,
                match_method,
                ..
            } => {
                assert_eq!(metadata.ids.tmdb, Some(45));
                assert_eq!(match_method.as_deref(), Some("nfo_tvdb_find"));
            }
            other => panic!("expected find resolve, got {other:?}"),
        }
        assert_eq!(resolver.tmdb.calls.get(), 1);
    }

    /// Provider that mimics a `/find` soft miss: with an NFO external id the
    /// id attempt reports [`ProviderResult::FindMiss`] (the real provider's
    /// conversion for a find 404 or a find-derived id whose detail 404s); a
    /// plain title search would hit the right show.
    struct FindMissThenSearchHit {
        calls: Cell<usize>,
    }

    impl MetadataSource for FindMissThenSearchHit {
        fn resolve(&self, input: &ResolveInput) -> Result<ProviderResult, ResolveError> {
            self.calls.set(self.calls.get() + 1);
            if input.nfo_tvdb_id.is_some() {
                return Ok(ProviderResult::FindMiss);
            }
            Ok(show_hit(55, "Top Gear", Some(2002), "exact_title"))
        }
    }

    /// RC5 fix (verify issue 1): a live `below_threshold` neg-cache row must
    /// not prevent the `/find` call, but it must suppress the fall-through
    /// title search after the find misses — a rescan before `next_retry_at`
    /// re-runs the find, not find+search (ADR-0026 §3).
    #[test]
    fn live_negative_cache_row_suppresses_search_after_find_miss() {
        fn seed_live_row(conn: &Connection) {
            let key = query_key("Top Gear", Some(2002));
            conn.execute(
                "INSERT INTO metadata_negative_cache
                   (provider, kind, query_key, reason, confidence, attempt_count,
                    attempted_at, next_retry_at, cleaner_version)
                 VALUES ('tmdb', 'tv', ?1, 'below_threshold', 0.72, 3,
                         '2026-01-01T00:00:00Z', '2999-01-01T00:00:00Z', ?2)",
                params![key, negative_cache::CLEANER_VERSION],
            )
            .unwrap();
        }

        let conn = Connection::open_in_memory().unwrap();
        migrate(&conn).unwrap();
        seed_live_row(&conn);

        let src = FindMissThenSearchHit {
            calls: Cell::new(0),
        };
        let resolver = Resolver { tmdb: src };
        let input = ResolveInput {
            tvshow_nfo_xml: Some(
                r#"<tvshow><title>Top Gear</title>
                   <uniqueid type="tvdb">7940</uniqueid></tvshow>"#
                    .into(),
            ),
            title: Some("Top Gear".into()),
            year: Some(2002),
            kind: Some(MetadataKind::Episode),
            ..Default::default()
        };
        let outcome = resolver.resolve_with_store(&input, &conn).unwrap();
        assert!(
            matches!(
                outcome,
                ResolveOutcome::Unresolved {
                    reason: UnresolvedReason::BelowThreshold { .. }
                }
            ),
            "the live row must suppress the fall-through search: {outcome:?}"
        );
        assert_eq!(
            resolver.tmdb.calls.get(),
            1,
            "the /find ran; the search was suppressed by the live row"
        );
    }

    /// RC5 fix (verify issue 2): a find-derived id whose detail 404s is a
    /// find soft-miss, not a resolve error — the resolve falls through to
    /// title search and the search hit wins, instead of leaving the group
    /// pending behind a repeat of the same find+404 (the D1 class).
    #[test]
    fn find_detail_404_falls_through_to_search_and_wins() {
        let src = FindMissThenSearchHit {
            calls: Cell::new(0),
        };
        let resolver = Resolver { tmdb: src };
        let conn = Connection::open_in_memory().unwrap();
        migrate(&conn).unwrap();
        let input = ResolveInput {
            tvshow_nfo_xml: Some(
                r#"<tvshow><title>Top Gear</title>
                   <uniqueid type="tvdb">7940</uniqueid></tvshow>"#
                    .into(),
            ),
            title: Some("Top Gear".into()),
            year: Some(2002),
            kind: Some(MetadataKind::Episode),
            ..Default::default()
        };
        let outcome = resolver.resolve_with_store(&input, &conn).unwrap();
        match outcome {
            ResolveOutcome::Resolved {
                metadata,
                match_method,
                ..
            } => {
                assert_eq!(metadata.ids.tmdb, Some(55), "search hit wins");
                assert_eq!(match_method.as_deref(), Some("exact_title"));
            }
            other => panic!("expected search resolve after find miss, got {other:?}"),
        }
        assert_eq!(
            resolver.tmdb.calls.get(),
            2,
            "one find attempt, then exactly one search"
        );
        let right: i64 = conn
            .query_row(
                "SELECT COUNT(*) FROM metadata_canonical
                 WHERE provider = 'tmdb' AND provider_id = '55'",
                [],
                |r| r.get(0),
            )
            .unwrap();
        assert_eq!(right, 1, "the search hit is persisted");
    }

    /// RC8 (ADR-0033 §8): a stored series id that agrees with the folder
    /// binds with zero provider calls — the same hit shape a fresh match
    /// produces — and clears the stale negative-cache row (one hit path,
    /// Rule 4.11; the reference branch's cache-hit fork is gone).
    #[test]
    fn stored_series_id_matching_folder_binds_with_zero_provider_calls() {
        struct PanicProvider;
        impl MetadataSource for PanicProvider {
            fn resolve(&self, _input: &ResolveInput) -> Result<ProviderResult, ResolveError> {
                panic!("a folder with stored identity must not reach the provider");
            }
        }
        let conn = Connection::open_in_memory().unwrap();
        migrate(&conn).unwrap();
        conn.execute_batch(
            "INSERT INTO libraries (name, path, kind) VALUES ('S', '/tmp/S', 'shows');
             INSERT INTO series (library_id, relpath, tmdb_show_id) VALUES (1, 'Alpha (2002)', 55);
             INSERT INTO metadata_canonical (
               provider, entity_kind, provider_id, title, year, ids_json, tmdb_show, projected_at
             ) VALUES
               ('tmdb', 'tv', '55', 'Alpha', 2002, '{\"tmdb\":55,\"tmdb_show\":55}', 55,
                '2026-01-01T00:00:00Z');",
        )
        .unwrap();
        // A stale title miss must not survive a successful stored-id bind.
        let qk = query_key("Alpha", Some(2002));
        conn.execute(
            "INSERT INTO metadata_negative_cache
               (provider, kind, query_key, reason, confidence, attempt_count,
                attempted_at, next_retry_at, cleaner_version)
             VALUES ('tmdb', 'tv', ?1, 'below_threshold', 0.72, 2,
                     '2026-01-01T00:00:00Z', '2999-01-01T00:00:00Z', ?2)",
            params![qk, negative_cache::CLEANER_VERSION],
        )
        .unwrap();
        // A series-id miss (ADR-0033 Q4: identified folders cache under their
        // series id) must be cleared by the re-bind just the same.
        conn.execute(
            "INSERT INTO metadata_negative_cache
               (provider, kind, query_key, reason, confidence, attempt_count,
                attempted_at, next_retry_at, cleaner_version)
             VALUES ('tmdb', 'tv', ?1, 'below_threshold', 0.72, 2,
                     '2026-01-01T00:00:00Z', '2999-01-01T00:00:00Z', ?2)",
            params![
                negative_cache::series_cache_key(55),
                negative_cache::CLEANER_VERSION
            ],
        )
        .unwrap();

        let resolver = Resolver {
            tmdb: PanicProvider,
        };
        let outcome = resolver
            .resolve_with_store(
                &ResolveInput {
                    series_show_id: Some(55),
                    title: Some("Alpha".into()),
                    year: Some(2002),
                    kind: Some(MetadataKind::Episode),
                    ..Default::default()
                },
                &conn,
            )
            .unwrap();
        match outcome {
            ResolveOutcome::Resolved {
                metadata,
                match_method,
                ..
            } => {
                assert_eq!(metadata.ids.tmdb, Some(55));
                assert_eq!(match_method.as_deref(), Some("series_row"));
            }
            other => panic!("stored identity must resolve locally, got {other:?}"),
        }
        let n: i64 = conn
            .query_row("SELECT COUNT(*) FROM metadata_negative_cache", [], |r| {
                r.get(0)
            })
            .unwrap();
        assert_eq!(
            n, 0,
            "a reused id clears stale negative-cache rows (title+year and series-id)"
        );
    }

    /// RC8 (ADR-0033 §8): a stored series id whose persisted detail disagrees
    /// with the folder is discarded and the resolve falls through to search —
    /// a wrong stored id never wins and is never bound (the Shameless
    /// (UK)/(US) case: stored US detail, 2011, against a 2004 folder).
    #[test]
    fn stored_series_id_disagreeing_with_folder_falls_through_to_search() {
        struct WrongStoredThenSearch {
            calls: Cell<usize>,
        }
        impl MetadataSource for WrongStoredThenSearch {
            fn resolve(&self, input: &ResolveInput) -> Result<ProviderResult, ResolveError> {
                self.calls.set(self.calls.get() + 1);
                assert!(
                    input.series_show_id.is_none(),
                    "the stale id must be cleared before the fall-through search"
                );
                Ok(show_hit(20610, "Shameless", Some(2004), "exact_title"))
            }
        }
        let conn = Connection::open_in_memory().unwrap();
        migrate(&conn).unwrap();
        conn.execute_batch(
            "INSERT INTO libraries (name, path, kind) VALUES ('S', '/tmp/S', 'shows');
             INSERT INTO series (library_id, relpath, tmdb_show_id)
             VALUES (1, 'Shameless (UK) (2004)', 34343);
             INSERT INTO metadata_canonical (
               provider, entity_kind, provider_id, title, year, ids_json, tmdb_show, projected_at
             ) VALUES
               ('tmdb', 'tv', '34343', 'Shameless', 2011, '{\"tmdb\":34343,\"tmdb_show\":34343}', 34343,
                '2026-01-01T00:00:00Z');",
        )
        .unwrap();

        let src = WrongStoredThenSearch {
            calls: Cell::new(0),
        };
        let resolver = Resolver { tmdb: src };
        let outcome = resolver
            .resolve_with_store(
                &ResolveInput {
                    series_show_id: Some(34343),
                    title: Some("Shameless".into()),
                    year: Some(2004),
                    kind: Some(MetadataKind::Episode),
                    ..Default::default()
                },
                &conn,
            )
            .unwrap();
        match outcome {
            ResolveOutcome::Resolved {
                metadata,
                match_method,
                ..
            } => {
                assert_eq!(
                    metadata.ids.tmdb,
                    Some(20610),
                    "the fall-through search hit wins, never the stored US id"
                );
                assert_eq!(match_method.as_deref(), Some("exact_title"));
            }
            other => panic!("expected search resolve, got {other:?}"),
        }
        assert_eq!(
            resolver.tmdb.calls.get(),
            1,
            "exactly one search after the stored id was discarded"
        );
        let wrong: i64 = conn
            .query_row(
                "SELECT COUNT(*) FROM media_item_links WHERE item_key LIKE 'tmdb:show:34343%'",
                [],
                |r| r.get(0),
            )
            .unwrap();
        assert_eq!(wrong, 0, "the wrong stored id is never written as a link");
    }

    /// Q4 (verify issue 1): a folder with stored identity that fails its
    /// name/year cross-check falls through to search, and that search must
    /// not be suppressed by a live title+year miss recorded by a
    /// fold-colliding sibling (identical query, different show). The
    /// identified folder's cache rows key on its series id (ADR-0033 Q4), so
    /// the sibling's shared-key row is invisible to it.
    #[test]
    fn stored_series_id_search_ignores_fold_colliding_sibling_miss() {
        struct SiblingMissThenSearch {
            calls: Cell<usize>,
        }
        impl MetadataSource for SiblingMissThenSearch {
            fn resolve(&self, input: &ResolveInput) -> Result<ProviderResult, ResolveError> {
                assert!(
                    input.series_show_id.is_none(),
                    "the stale id must be cleared before the fall-through search"
                );
                self.calls.set(self.calls.get() + 1);
                Ok(show_hit(20610, "Shameless", Some(2004), "exact_title"))
            }
        }
        let conn = Connection::open_in_memory().unwrap();
        migrate(&conn).unwrap();
        conn.execute_batch(
            "INSERT INTO libraries (name, path, kind) VALUES ('S', '/tmp/S', 'shows');
             INSERT INTO series (library_id, relpath, tmdb_show_id)
             VALUES (1, 'Shameless (US) (2011)', 34343);
             INSERT INTO metadata_canonical (
               provider, entity_kind, provider_id, title, year, ids_json, tmdb_show, projected_at
             ) VALUES
               ('tmdb', 'tv', '34343', 'Shameless', 2011, '{\"tmdb\":34343,\"tmdb_show\":34343}', 34343,
                '2026-01-01T00:00:00Z');",
        )
        .unwrap();
        // The sibling's live miss under the *shared* title+year query — the
        // exact query this folder's fall-through search would use.
        let sibling_qk = query_key("Shameless", Some(2004));
        conn.execute(
            "INSERT INTO metadata_negative_cache
               (provider, kind, query_key, reason, confidence, attempt_count,
                attempted_at, next_retry_at, cleaner_version)
             VALUES ('tmdb', 'tv', ?1, 'below_threshold', 0.72, 2,
                     '2026-01-01T00:00:00Z', '2999-01-01T00:00:00Z', ?2)",
            params![sibling_qk, negative_cache::CLEANER_VERSION],
        )
        .unwrap();

        let src = SiblingMissThenSearch {
            calls: Cell::new(0),
        };
        let resolver = Resolver { tmdb: src };
        let outcome = resolver
            .resolve_with_store(
                &ResolveInput {
                    series_show_id: Some(34343),
                    title: Some("Shameless".into()),
                    year: Some(2004),
                    kind: Some(MetadataKind::Episode),
                    ..Default::default()
                },
                &conn,
            )
            .unwrap();
        match outcome {
            ResolveOutcome::Resolved { metadata, .. } => {
                assert_eq!(metadata.ids.tmdb, Some(20610));
            }
            other => panic!("expected the fall-through search to run, got {other:?}"),
        }
        assert_eq!(
            resolver.tmdb.calls.get(),
            1,
            "the sibling's title+year miss must not suppress the identified folder's search"
        );
    }

    /// Q4 (verify issue 1): an identified folder's failed search records its
    /// miss under the series id — never the shared title+year key — and the
    /// next resolve consults that same series-id row, so backoff works
    /// without touching (or being touched by) a fold-colliding sibling.
    #[test]
    fn stored_series_id_miss_caches_under_series_id() {
        let conn = Connection::open_in_memory().unwrap();
        migrate(&conn).unwrap();
        conn.execute_batch(
            "INSERT INTO libraries (name, path, kind) VALUES ('S', '/tmp/S', 'shows');
             INSERT INTO series (library_id, relpath, tmdb_show_id)
             VALUES (1, 'Alpha (2003)', 55);
             INSERT INTO metadata_canonical (
               provider, entity_kind, provider_id, title, year, ids_json, tmdb_show, projected_at
             ) VALUES
               ('tmdb', 'tv', '55', 'Alpha', 2002, '{\"tmdb\":55,\"tmdb_show\":55}', 55,
                '2026-01-01T00:00:00Z');",
        )
        .unwrap();
        let input = ResolveInput {
            series_show_id: Some(55),
            title: Some("Alpha".into()),
            year: Some(2003),
            kind: Some(MetadataKind::Episode),
            ..Default::default()
        };
        let src = CountingMiss {
            calls: Cell::new(0),
        };
        let resolver = Resolver { tmdb: src };

        let out = resolver.resolve_with_store(&input, &conn).unwrap();
        assert!(
            matches!(out, ResolveOutcome::Unresolved { .. }),
            "the fall-through search misses: {out:?}"
        );
        assert_eq!(resolver.tmdb.calls.get(), 1, "the search ran once");
        let series_row: i64 = conn
            .query_row(
                "SELECT COUNT(*) FROM metadata_negative_cache
                 WHERE provider = 'tmdb' AND kind = 'tv' AND query_key = ?1",
                params![negative_cache::series_cache_key(55)],
                |r| r.get(0),
            )
            .unwrap();
        assert_eq!(series_row, 1, "the miss is cached under the series id");
        let title_row: i64 = conn
            .query_row(
                "SELECT COUNT(*) FROM metadata_negative_cache
                 WHERE provider = 'tmdb' AND kind = 'tv' AND query_key = ?1",
                params![query_key("Alpha", Some(2003))],
                |r| r.get(0),
            )
            .unwrap();
        assert_eq!(
            title_row, 0,
            "an identified folder never writes the shared title+year key"
        );

        let out = resolver.resolve_with_store(&input, &conn).unwrap();
        assert!(
            matches!(out, ResolveOutcome::Unresolved { .. }),
            "the series-id backoff suppresses the re-search: {out:?}"
        );
        assert_eq!(
            resolver.tmdb.calls.get(),
            1,
            "the second resolve consults the series-id row"
        );
    }

    /// ADR-0026 (amended): a pick with zero episodes is terminal `unmatched`
    /// with the recorded reason, never a `Resolved` bind to the empty entity.
    struct EmptyShellSource;
    impl MetadataSource for EmptyShellSource {
        fn resolve(&self, _input: &ResolveInput) -> Result<ProviderResult, ResolveError> {
            Ok(ProviderResult::EmptyShell)
        }
    }

    #[test]
    fn empty_shell_winner_is_unresolved_with_recorded_reason() {
        let outcome = Resolver {
            tmdb: EmptyShellSource,
        }
        .resolve(&ResolveInput {
            title: Some("Test Show".into()),
            year: None,
            kind: Some(MetadataKind::Episode),
            ..Default::default()
        })
        .unwrap();
        assert_eq!(
            outcome,
            ResolveOutcome::Unresolved {
                reason: UnresolvedReason::NoEpisodes
            },
            "an entity with zero episodes must not be bound"
        );
    }

    /// ADR-0033 §8 + ADR-0026 (amended): a stored series id whose persisted
    /// detail reports zero episodes is not a candidate, so the id is
    /// discarded and the resolve falls through to search, exactly like a
    /// name/year disagreement. The stored shell id is never bound.
    #[test]
    fn stored_series_id_with_zero_episodes_falls_through_to_search() {
        struct EmptyStoredThenSearch {
            calls: Cell<usize>,
        }
        impl MetadataSource for EmptyStoredThenSearch {
            fn resolve(&self, input: &ResolveInput) -> Result<ProviderResult, ResolveError> {
                self.calls.set(self.calls.get() + 1);
                assert!(
                    input.series_show_id.is_none(),
                    "the stored shell id must be cleared before the fall-through search"
                );
                Ok(show_hit(20610, "Test Show", Some(2004), "exact_title"))
            }
        }
        let conn = Connection::open_in_memory().unwrap();
        migrate(&conn).unwrap();
        conn.execute_batch(
            "INSERT INTO libraries (name, path, kind) VALUES ('S', '/tmp/S', 'shows');
             INSERT INTO series (library_id, relpath, tmdb_show_id)
             VALUES (1, 'Test Show (2004)', 55);
             INSERT INTO metadata_canonical (
               provider, entity_kind, provider_id, title, year, ids_json, tmdb_show, projected_at
             ) VALUES
               ('tmdb', 'tv', '55', 'Test Show', 2004, '{\"tmdb\":55,\"tmdb_show\":55}', 55,
                '2026-01-01T00:00:00Z');
             INSERT INTO metadata_raw_payloads (provider, entity_kind, provider_id, fetched_at, payload)
             VALUES ('tmdb', 'tv', '55', '2026-01-01T00:00:00Z',
                     '{\"id\":55,\"name\":\"Test Show\",\"number_of_episodes\":0}');",
        )
        .unwrap();

        let src = EmptyStoredThenSearch {
            calls: Cell::new(0),
        };
        let resolver = Resolver { tmdb: src };
        let outcome = resolver
            .resolve_with_store(
                &ResolveInput {
                    series_show_id: Some(55),
                    title: Some("Test Show".into()),
                    year: Some(2004),
                    kind: Some(MetadataKind::Episode),
                    ..Default::default()
                },
                &conn,
            )
            .unwrap();
        match outcome {
            ResolveOutcome::Resolved {
                metadata,
                match_method,
                ..
            } => {
                assert_eq!(
                    metadata.ids.tmdb,
                    Some(20610),
                    "the fall-through search hit wins, never the stored shell id"
                );
                assert_eq!(match_method.as_deref(), Some("exact_title"));
            }
            other => panic!("expected search resolve, got {other:?}"),
        }
        assert_eq!(
            resolver.tmdb.calls.get(),
            1,
            "exactly one search after the stored shell id was discarded"
        );
        let wrong: i64 = conn
            .query_row(
                "SELECT COUNT(*) FROM media_item_links WHERE item_key LIKE 'tmdb:show:55%'",
                [],
                |r| r.get(0),
            )
            .unwrap();
        assert_eq!(wrong, 0, "the empty stored id is never written as a link");
    }

    /// Empty is the rule, not small: a stored payload with episodes still
    /// binds locally, and a missing payload stays unknown (never a reject).
    #[test]
    fn stored_series_id_with_episodes_still_binds() {
        struct PanicProvider;
        impl MetadataSource for PanicProvider {
            fn resolve(&self, _input: &ResolveInput) -> Result<ProviderResult, ResolveError> {
                panic!("a folder with stored identity must not reach the provider");
            }
        }
        let conn = Connection::open_in_memory().unwrap();
        migrate(&conn).unwrap();
        conn.execute_batch(
            "INSERT INTO libraries (name, path, kind) VALUES ('S', '/tmp/S', 'shows');
             INSERT INTO series (library_id, relpath, tmdb_show_id) VALUES (1, 'Alpha (2002)', 55);
             INSERT INTO metadata_canonical (
               provider, entity_kind, provider_id, title, year, ids_json, tmdb_show, projected_at
             ) VALUES
               ('tmdb', 'tv', '55', 'Alpha', 2002, '{\"tmdb\":55,\"tmdb_show\":55}', 55,
                '2026-01-01T00:00:00Z');
             INSERT INTO metadata_raw_payloads (provider, entity_kind, provider_id, fetched_at, payload)
             VALUES ('tmdb', 'tv', '55', '2026-01-01T00:00:00Z',
                     '{\"id\":55,\"name\":\"Alpha\",\"number_of_episodes\":4}');",
        )
        .unwrap();
        let outcome = Resolver {
            tmdb: PanicProvider,
        }
        .resolve_with_store(
            &ResolveInput {
                series_show_id: Some(55),
                title: Some("Alpha".into()),
                year: Some(2002),
                kind: Some(MetadataKind::Episode),
                ..Default::default()
            },
            &conn,
        )
        .unwrap();
        match outcome {
            ResolveOutcome::Resolved {
                metadata,
                match_method,
                ..
            } => {
                assert_eq!(metadata.ids.tmdb, Some(55));
                assert_eq!(match_method.as_deref(), Some("series_row"));
            }
            other => panic!("stored identity with episodes must resolve locally, got {other:?}"),
        }
    }

    /// Fixture for issue 121: a folder whose title the name gate rejects,
    /// bound to an entity whose episode titles the filenames carry.
    /// `Monster` against `DAHMER - Monster: The Jeffrey Dahmer Story`.
    fn name_mismatch_folder(conn: &Connection, provider_ep_title: &str) {
        conn.execute_batch(&format!(
            "INSERT INTO libraries (name, path, kind) VALUES ('S', '/tmp/S', 'shows');
             INSERT INTO series (library_id, relpath, tmdb_show_id)
                  VALUES (1, 'Monster (2022)', 113988);
             INSERT INTO metadata_canonical
               (provider, entity_kind, provider_id, title, year, ids_json, tmdb_show, projected_at)
             VALUES
               ('tmdb', 'tv', '113988', 'DAHMER - Monster: The Jeffrey Dahmer Story', 2022,
                '{{\"tmdb\":113988,\"tmdb_show\":113988}}', 113988, '2026-01-01T00:00:00Z'),
               ('tmdb', 'episode', '9001', '{provider_ep_title}', NULL, '{{}}', 113988,
                '2026-01-01T00:00:00Z');
             UPDATE metadata_canonical SET season = 1, episode = 3
               WHERE provider_id = '9001';"
        ))
        .unwrap();
    }

    fn resolve_mismatch(
        conn: &Connection,
        resolver: &Resolver<CountingMiss>,
        folder_titles: Vec<(i32, i32, String)>,
    ) -> ResolveOutcome {
        resolver
            .resolve_with_store(
                &ResolveInput {
                    series_show_id: Some(113988),
                    title: Some("Monster".into()),
                    year: None,
                    kind: Some(MetadataKind::Show),
                    folder_episode_titles: folder_titles,
                    ..Default::default()
                },
                conn,
            )
            .unwrap()
    }

    /// **Issue 121's root.** The name gate rejects `DAHMER - Monster: …` for
    /// the folder `Monster`, and that binding is correct — the filenames carry
    /// its episode titles. The stored id is kept and no search is issued, so
    /// the folder never reaches the re-search where it acquired `Monster High`.
    #[test]
    fn a_confirmed_stored_id_survives_a_name_mismatch() {
        let conn = Connection::open_in_memory().unwrap();
        migrate(&conn).unwrap();
        name_mismatch_folder(&conn, "Blame it on the Rain");
        let resolver = Resolver {
            tmdb: CountingMiss {
                calls: std::cell::Cell::new(0),
            },
        };

        let outcome = resolve_mismatch(
            &conn,
            &resolver,
            vec![(1, 3, "Blame it on the Rain".to_string())],
        );

        assert!(
            matches!(outcome, ResolveOutcome::Resolved { .. }),
            "a confirmed stored id resolves rather than falling through"
        );
        assert_eq!(
            resolver.tmdb.calls.get(),
            0,
            "no search is issued — this is the request the rescue exists to avoid"
        );
    }

    /// The rescue must not save the one binding the gate is right about.
    /// `Love on the Spectrum U.S` agrees on 0 of 27 files; with no agreement
    /// the discard stands and the search runs exactly as before.
    #[test]
    fn an_unconfirmed_stored_id_is_still_discarded() {
        let conn = Connection::open_in_memory().unwrap();
        migrate(&conn).unwrap();
        name_mismatch_folder(&conn, "Something Else Entirely");
        let resolver = Resolver {
            tmdb: CountingMiss {
                calls: std::cell::Cell::new(0),
            },
        };

        resolve_mismatch(
            &conn,
            &resolver,
            vec![(1, 3, "Blame it on the Rain".to_string())],
        );

        assert_eq!(
            resolver.tmdb.calls.get(),
            1,
            "no agreement means the discard stands and the search runs"
        );
    }

    /// **Decision 2.** The agreement is on season 1 episode 3; a confirmation
    /// keyed on the ADR-0032 reference episode alone would consult whichever
    /// single file was picked and could miss it entirely. Every testable file
    /// is compared, so a folder holding several installments still confirms.
    #[test]
    fn confirmation_reads_every_file_not_the_reference_episode() {
        let conn = Connection::open_in_memory().unwrap();
        migrate(&conn).unwrap();
        name_mismatch_folder(&conn, "Blame it on the Rain");
        let resolver = Resolver {
            tmdb: CountingMiss {
                calls: std::cell::Cell::new(0),
            },
        };

        // The first two files are on seasons the stored entity does not have —
        // the shape a multi-installment folder produces. Only the third agrees.
        let outcome = resolve_mismatch(
            &conn,
            &resolver,
            vec![
                (2, 1, "Spree".to_string()),
                (3, 1, "The Gein Family".to_string()),
                (1, 3, "Blame it on the Rain".to_string()),
            ],
        );

        assert!(matches!(outcome, ResolveOutcome::Resolved { .. }));
        assert_eq!(resolver.tmdb.calls.get(), 0, "the third file confirms");
    }

    /// A folder with no usable episode titles yields no verdict, and absence
    /// of evidence is not evidence for the id.
    #[test]
    fn no_episode_titles_means_no_rescue() {
        let conn = Connection::open_in_memory().unwrap();
        migrate(&conn).unwrap();
        name_mismatch_folder(&conn, "Blame it on the Rain");
        let resolver = Resolver {
            tmdb: CountingMiss {
                calls: std::cell::Cell::new(0),
            },
        };

        resolve_mismatch(&conn, &resolver, Vec::new());

        assert_eq!(resolver.tmdb.calls.get(), 1, "no titles, no rescue");
    }
}
