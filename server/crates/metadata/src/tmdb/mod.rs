//! TMDB HTTP client and [`MetadataSource`] (ADR-0026).

mod credentials;
mod map;

use std::sync::Arc;
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::Duration;

use serde_json::Value;

use crate::match_score::{
    CandidateShape, LibrarySeriesShape, MatchCandidate, SearchHit, SearchKind,
    meets_auto_match_floor, needs_collision_detail, norm_key, pin_episode_title,
    score_search_with_shape,
};
use crate::model::{CanonicalMetadata, MetadataKind};
use crate::rate_limit::ApiRateLimiter;
use crate::resolve::{
    MetadataSource, ProviderResult, ResolveError, ResolveInput, SecondEntityCandidate,
};

pub use credentials::{
    CredError, TmdbCredentials, TmdbKeySource, embedded_application_key, resolve_credentials,
    resolve_credentials_with,
};
pub use map::{RawProviderPayload, map_episodes_from_season, map_movie_detail, map_tv_detail};

/// Placeholder until a live client is configured. Always [`ProviderResult::Miss`].
#[derive(Debug, Default, Clone, Copy)]
pub struct TmdbStub;

impl MetadataSource for TmdbStub {
    fn resolve(&self, _input: &ResolveInput) -> Result<ProviderResult, ResolveError> {
        Ok(ProviderResult::Miss)
    }
}

/// Season numbers from a `/tv/{id}` payload's `seasons[]` array. `None` when
/// the array is absent, which must read as "not fetched", never as "no
/// seasons".
/// Cap on seasons appended to one `/tv/{id}` call. TMDB documents a limit of
/// 20 `append_to_response` items; this stays under it and bounds the response
/// for a long-running show rather than trusting the provider's ceiling.
const MAX_APPENDED_SEASONS: usize = 20;

/// Cap on search hits examined for a second entity. Each costs two requests,
/// and this path runs only when a bind left seasons unplaced. Popular
/// franchises return long prefix tails — Grand Designs 17, Doctor Who 18 — and
/// walking all of them would turn one fix into thirty-odd calls.
const MAX_SECOND_ENTITY_CANDIDATES: usize = 5;

/// `(season_number, episode_count)` from a `/tv/{id}` payload's `seasons[]`.
///
/// The array is already in every collision-tier response and
/// [`season_numbers_from_detail`] parses the numbers and **drops the counts**.
/// This reads what is already there: no request, no append.
///
/// Season 0 is excluded — a `Specials` season exists independently of whether
/// the folder files one, and counting it would let a folder with no specials
/// look short against every candidate that has them.
pub fn season_episode_counts_from_detail(data: &Value) -> Option<Vec<(i32, u32)>> {
    Some(
        data.get("seasons")?
            .as_array()?
            .iter()
            .filter_map(|s| {
                let n = s.get("season_number")?.as_i64()? as i32;
                let c = s.get("episode_count")?.as_u64()? as u32;
                (n > 0).then_some((n, c))
            })
            .collect(),
    )
}

pub fn season_numbers_from_detail(data: &Value) -> Option<Vec<i32>> {
    Some(
        data.get("seasons")?
            .as_array()?
            .iter()
            .filter_map(|s| s.get("season_number")?.as_i64())
            .map(|n| n as i32)
            .collect(),
    )
}

/// `number_of_episodes` from a `/tv/{id}` payload (fresh or stored raw).
/// `None` when the field is absent or the payload does not parse, which must
/// read as unknown, never as zero (ADR-0026, amended: unknown never excludes).
pub(crate) fn tv_payload_episode_count(payload: &str) -> Option<u32> {
    serde_json::from_str::<Value>(payload)
        .ok()
        .and_then(|v| v.get("number_of_episodes")?.as_u64())
        .map(|n| n as u32)
}

const MOVIE_APPEND: &str = "images,credits,videos,release_dates,external_ids";
const TV_APPEND: &str = "images,credits,videos,content_ratings,external_ids,aggregate_credits";
const SEASON_APPEND: &str = "images,credits,videos,external_ids";

#[derive(Debug)]
pub struct TmdbClient {
    creds: TmdbCredentials,
    agent: ureq::Agent,
    limiter: Arc<ApiRateLimiter>,
    /// Count of HTTP 429 responses (measure harness).
    pub http_429: Arc<AtomicU64>,
    /// Count of API HTTP attempts (every `get_json`, including errors).
    pub http_requests: Arc<AtomicU64>,
}

impl TmdbClient {
    pub fn new(creds: TmdbCredentials) -> Self {
        Self::with_limiter(creds, ApiRateLimiter::polite_default())
    }

    pub fn with_limiter(creds: TmdbCredentials, limiter: Arc<ApiRateLimiter>) -> Self {
        let agent = ureq::AgentBuilder::new()
            .timeout_connect(Duration::from_secs(10))
            .timeout_read(Duration::from_secs(30))
            .build();
        Self {
            creds,
            agent,
            limiter,
            http_429: Arc::new(AtomicU64::new(0)),
            http_requests: Arc::new(AtomicU64::new(0)),
        }
    }

    fn get_json(&self, path: &str, query: &[(&str, &str)]) -> Result<Value, ResolveError> {
        match self.get_json_status(path, query)? {
            None => Err(ResolveError::NotFound(format!("TMDB 404: {path}"))),
            Some(v) => Ok(v),
        }
    }

    /// Like `get_json`, but HTTP 404 → `Ok(None)` (missing season/episode rows).
    fn get_json_optional(
        &self,
        path: &str,
        query: &[(&str, &str)],
    ) -> Result<Option<Value>, ResolveError> {
        self.get_json_status(path, query)
    }

    fn get_json_status(
        &self,
        path: &str,
        query: &[(&str, &str)],
    ) -> Result<Option<Value>, ResolveError> {
        let _permit = self.limiter.acquire();
        self.http_requests.fetch_add(1, Ordering::Relaxed);
        let mut url = format!("https://api.themoviedb.org/3{path}");
        let mut first = true;
        let push = |url: &mut String, first: &mut bool, k: &str, v: &str| {
            url.push(if *first { '?' } else { '&' });
            *first = false;
            url.push_str(k);
            url.push('=');
            url.push_str(&urlencoding_encode(v));
        };
        for (k, v) in query {
            push(&mut url, &mut first, k, v);
        }
        push(&mut url, &mut first, "api_key", &self.creds.api_key);

        let resp = match self.agent.get(&url).call() {
            Ok(r) => r,
            Err(ureq::Error::Status(404, _)) => return Ok(None),
            Err(e) => {
                return Err(ResolveError::Provider(scrub_tmdb_url_secret(
                    &e.to_string(),
                )));
            }
        };
        let status = resp.status();
        let body = resp
            .into_string()
            .map_err(|e| ResolveError::Provider(e.to_string()))?;
        if status == 429 {
            self.http_429.fetch_add(1, Ordering::Relaxed);
        }
        if let Some(err) = auth_rejected_error(status, &self.creds) {
            return Err(err);
        }
        if status == 404 {
            return Ok(None);
        }
        if !(200..300).contains(&status) {
            return Err(ResolveError::Provider(format!(
                "TMDB {status}: {}",
                body.chars().take(200).collect::<String>()
            )));
        }
        serde_json::from_str(&body)
            .map(Some)
            .map_err(|e| ResolveError::Provider(e.to_string()))
    }

    /// Title search, deliberately **not** narrowed by the folder's year.
    ///
    /// The year is still evidence — it pins through `library.year` and
    /// `exact_title_year` in the scorer — but it is applied here, where a
    /// stronger signal can outweigh it, rather than at the provider, where
    /// nothing can. Narrowing on `first_air_date_year` removes candidates
    /// before any Nightjar rule sees them: `Battlestar Galactica (2003)`
    /// returned one hit, the two-episode miniseries, with the four-season
    /// series absent from the result set entirely. A folder cannot be matched
    /// to a candidate that was never returned.
    pub fn search(&self, kind: SearchKind, title: &str) -> Result<Vec<SearchHit>, ResolveError> {
        let path = match kind {
            SearchKind::Movie => "/search/movie",
            SearchKind::Tv => "/search/tv",
        };
        let q: Vec<(&str, &str)> = vec![("query", title)];
        let data = self.get_json(path, &q)?;
        let results = data
            .get("results")
            .and_then(|r| r.as_array())
            .map(|arr| {
                arr.iter()
                    .take(10)
                    .filter_map(|r| serde_json::from_value::<SearchHit>(r.clone()).ok())
                    .collect()
            })
            .unwrap_or_default();
        Ok(results)
    }

    /// Look up a TMDB show id by external id via `GET /3/find/{external_id}`
    /// (NFO imdb/tvdb ids, strategy note §2 A2/A3). HTTP 404 (a stale or
    /// malformed id) is a soft miss → `None` via `get_json_optional`, so the
    /// resolve falls through to title search instead of erroring the group.
    /// The caller owns the name+year cross-check on the fetched detail.
    pub fn find_tv_by_external_id(
        &self,
        external_source: &str,
        external_id: &str,
    ) -> Result<Option<i64>, ResolveError> {
        let path = format!("/find/{external_id}");
        let Some(data) = self.get_json_optional(&path, &[("external_source", external_source)])?
        else {
            return Ok(None);
        };
        let id = data
            .get("tv_results")
            .and_then(|r| r.as_array())
            .and_then(|arr| arr.first())
            .and_then(|hit| hit.get("id"))
            .and_then(|v| v.as_i64());
        Ok(id)
    }

    pub fn match_search(
        &self,
        kind: SearchKind,
        title: &str,
        year: Option<i32>,
    ) -> Result<Option<MatchCandidate>, ResolveError> {
        self.match_search_with_library_year(kind, title, year, None)
    }

    pub fn match_search_with_library_year(
        &self,
        kind: SearchKind,
        title: &str,
        year: Option<i32>,
        library_year: Option<i32>,
    ) -> Result<Option<MatchCandidate>, ResolveError> {
        self.match_search_with_series_shape(
            kind,
            title,
            year,
            LibrarySeriesShape {
                year: library_year,
                ..Default::default()
            },
        )
    }

    /// Search + collision pin. Fetches `/tv/{id}` shapes only when multi-exact
    /// survives the year discriminator; episode-title pin (ADR-0032) when
    /// counts still leave the tie and a usable reference episode is present.
    pub fn match_search_with_series_shape(
        &self,
        kind: SearchKind,
        title: &str,
        year: Option<i32>,
        library: LibrarySeriesShape,
    ) -> Result<Option<MatchCandidate>, ResolveError> {
        let results = self.search(kind, title)?;
        if !needs_collision_detail(&results, title, year, kind, library.clone()) {
            // A search that returned **no** title-exact hit has nothing for the
            // ladder to choose among, and `needs_collision_detail` — a test
            // about collisions — declines to fetch, so there is no evidence
            // either. That is the one case where the absence is the problem
            // rather than an economy: measured library-wide, 428 of the 432
            // folders that get no detail are sole *exact* hits already clearing
            // the floor, and only **4** have no exact hit at all.
            //
            // One detail call for those four. The seasons ride along on it as
            // everywhere else.
            let nk_probe = norm_key(title);
            let has_exact = results
                .iter()
                .any(|r| crate::match_score::title_hit(r, &nk_probe, kind));
            let sole = if kind == SearchKind::Tv
                && !has_exact
                && library.ref_episode_title.is_some()
                && let Some(top1) = results.first()
            {
                self.tv_candidate_shape(top1, library.ref_season).ok()
            } else {
                None
            };
            return Ok(crate::match_score::score_search_with_shape_and_sole(
                &results,
                title,
                year,
                kind,
                library,
                None,
                sole.as_ref(),
            ));
        }
        let nk = norm_key(title);
        // Deliberately the *unnarrowed* title-hit set: ADR-0047's exact-fold
        // precedence runs inside `find_best`, after the empty-shell exclusion,
        // and it needs the extensions still present to choose between them.
        let exact: Vec<&SearchHit> = results
            .iter()
            .filter(|r| crate::match_score::title_hit(r, &nk, kind))
            .take(8)
            .collect();
        let mut shapes = Vec::with_capacity(exact.len());
        for hit in &exact {
            shapes.push(self.tv_candidate_shape(hit, library.ref_season)?);
        }
        let shaped_results: Vec<SearchHit> = exact.iter().map(|h| (*h).clone()).collect();
        let scored = score_search_with_shape(
            &shaped_results,
            title,
            year,
            kind,
            library.clone(),
            Some(&shapes),
        );
        if scored.is_none() {
            // Every title-hit was an empty shell: none is a candidate, and
            // the episode-title pin must not resurrect one (ADR-0026,
            // amended).
            return Ok(scored);
        }
        if let Some(ref c) = scored
            && meets_auto_match_floor(c.confidence)
        {
            return Ok(scored);
        }
        // ADR-0032 step 4: episode-title pin on still-unpinned TV multi-exact.
        let Some(ref_title) = library.ref_episode_title.as_deref() else {
            return Ok(scored);
        };
        let (Some(_ref_season), Some(ref_episode)) = (library.ref_season, library.ref_episode)
        else {
            return Ok(scored);
        };
        if kind != SearchKind::Tv || exact.is_empty() {
            return Ok(scored);
        }
        let exact_owned: Vec<SearchHit> = exact.iter().map(|h| (*h).clone()).collect();
        let exact_refs: Vec<&SearchHit> = exact_owned.iter().collect();
        if exact_refs.len() > crate::match_score::EPISODE_TITLE_TIE_CAP {
            return Ok(scored);
        }
        // The reference season rode along on the `/tv/{id}` call above, so the
        // names are already here: no call per candidate.
        let names: Vec<Option<String>> = shapes
            .iter()
            .map(|sh| {
                sh.reference_season_episodes.as_deref().and_then(|eps| {
                    eps.iter()
                        .find(|(n, _)| *n == ref_episode)
                        .map(|(_, nm)| nm.clone())
                })
            })
            .collect();
        if let Some((hit, method)) = pin_episode_title(&exact_refs, &names, ref_title, title) {
            return Ok(Some(MatchCandidate {
                tmdb_id: hit.id,
                // ADR-0032's episode-title pin **selects** — the route is
                // `exact_title_episode_title` — and the titles agreed by
                // construction, so the flag is true rather than unknown.
                confirmed_by_episode_title: Some(true),
                confidence: 0.90,
                method,
                result_title: hit.name.clone().or_else(|| hit.original_name.clone()),
                result_year: hit
                    .first_air_date
                    .as_deref()
                    .and_then(|d| d.get(..4)?.parse().ok()),
                n_results: results.len(),
            }));
        }
        Ok(scored)
    }

    /// `/tv/{id}` for a collision candidate, with the folder's reference season
    /// appended to the **same** request (`append_to_response=season/{n}`), so
    /// episode-title evidence costs no additional call.
    fn tv_candidate_shape(
        &self,
        hit: &SearchHit,
        ref_season: Option<i32>,
    ) -> Result<CandidateShape, ResolveError> {
        let year = hit
            .first_air_date
            .as_deref()
            .and_then(|d| d.get(..4)?.parse().ok());
        let append = ref_season.map(|n| format!("season/{n}"));
        let mut q: Vec<(&str, &str)> = vec![("language", "en-US")];
        if let Some(ref a) = append {
            q.push(("append_to_response", a.as_str()));
        }
        let data = self.get_json(&format!("/tv/{}", hit.id), &q)?;
        Ok(CandidateShape {
            year,
            episode_count: data
                .get("number_of_episodes")
                .and_then(|v| v.as_u64())
                .map(|n| n as u32),
            season_count: data
                .get("number_of_seasons")
                .and_then(|v| v.as_u64())
                .map(|n| n as u32),
            season_numbers: season_numbers_from_detail(&data),
            season_episode_counts: season_episode_counts_from_detail(&data),
            reference_season_episodes: ref_season
                .and_then(|n| data.get(format!("season/{n}")))
                .and_then(|v| v.get("episodes"))
                .and_then(|e| e.as_array())
                .map(|arr| {
                    arr.iter()
                        .filter_map(|e| {
                            Some((
                                e.get("episode_number")?.as_i64()? as i32,
                                e.get("name")?.as_str()?.to_string(),
                            ))
                        })
                        .collect()
                }),
            candidate_season_episodes: None,
        })
    }

    /// `/tv/{id}` with the candidate's **own** seasons appended, for the
    /// unplaced-file search (ADR-0046 item 4).
    ///
    /// [`Self::tv_candidate_shape`] appends the season number the *folder*
    /// uses, which is right for the collision tier — same show, same
    /// numbering — and **silent** on a renumbered split. Will & Grace's
    /// unplaced files are folder seasons 9-11 and TMDB 74321 has seasons 1-3,
    /// so `season/9` is simply absent from the response and the shipped
    /// confirmation returns `None`. Not wrong; silent. That is the one shape
    /// the second-entity search is about, so it needs the candidate's own
    /// numbering instead.
    ///
    /// **Costs two requests**, because the season numbers are not knowable
    /// until the detail arrives. That is affordable precisely because this
    /// runs only when a bind left seasons unplaced — never on a rescan of a
    /// folder that already has its bindings, which is what the stored
    /// folder->entity record exists to guarantee (ADR-0033 item 1).
    fn tv_candidate_own_seasons(
        &self,
        id: i64,
    ) -> Result<Option<Vec<crate::match_score::CandidateEpisode>>, ResolveError> {
        let detail = self.get_json(&format!("/tv/{id}"), &[("language", "en-US")])?;
        let Some(seasons) = season_numbers_from_detail(&detail) else {
            return Ok(None);
        };
        // Season 0 is excluded for the same reason the coverage predicate
        // excludes it: a `Specials` folder exists independently of whether a
        // provider models season 0, so it is not evidence about identity.
        let wanted: Vec<i32> = seasons
            .into_iter()
            .filter(|n| *n > 0)
            .take(MAX_APPENDED_SEASONS)
            .collect();
        if wanted.is_empty() {
            return Ok(None);
        }
        let append = wanted
            .iter()
            .map(|n| format!("season/{n}"))
            .collect::<Vec<_>>()
            .join(",");
        let data = self.get_json(
            &format!("/tv/{id}"),
            &[("language", "en-US"), ("append_to_response", &append)],
        )?;
        let mut out = Vec::new();
        for n in wanted {
            let Some(eps) = data
                .get(format!("season/{n}"))
                .and_then(|v| v.get("episodes"))
                .and_then(|e| e.as_array())
            else {
                continue;
            };
            for e in eps {
                let (Some(num), Some(name)) = (
                    e.get("episode_number").and_then(|v| v.as_i64()),
                    e.get("name").and_then(|v| v.as_str()),
                ) else {
                    continue;
                };
                out.push((n, num as i32, name.to_string()));
            }
        }
        if out.is_empty() {
            Ok(None)
        } else {
            Ok(Some(out))
        }
    }

    pub fn movie_detail(
        &self,
        id: i64,
    ) -> Result<(CanonicalMetadata, RawProviderPayload), ResolveError> {
        let id_s = id.to_string();
        let data = self.get_json(
            &format!("/movie/{id}"),
            &[("append_to_response", MOVIE_APPEND), ("language", "en-US")],
        )?;
        let raw = RawProviderPayload {
            entity_kind: "movie".into(),
            provider_id: id_s,
            payload: data.to_string(),
        };
        let meta = map_movie_detail(&data)?;
        Ok((meta, raw))
    }

    pub fn tv_detail(
        &self,
        id: i64,
    ) -> Result<(CanonicalMetadata, RawProviderPayload), ResolveError> {
        let id_s = id.to_string();
        let data = self.get_json(
            &format!("/tv/{id}"),
            &[("append_to_response", TV_APPEND), ("language", "en-US")],
        )?;
        let raw = RawProviderPayload {
            entity_kind: "tv".into(),
            provider_id: id_s,
            payload: data.to_string(),
        };
        let meta = map_tv_detail(&data)?;
        Ok((meta, raw))
    }

    /// Season detail keyed `{show_id}:{season_number}` (ADR-0026 §4).
    /// HTTP 404 → `Ok(None)` so bind can skip a missing season and continue
    /// with other seasons (library S2+ vs TMDB shape lag).
    pub fn season_detail(
        &self,
        show_id: i64,
        season_number: i32,
    ) -> Result<Option<RawProviderPayload>, ResolveError> {
        let path = format!("/tv/{show_id}/season/{season_number}");
        let Some(data) = self.get_json_optional(
            &path,
            &[("append_to_response", SEASON_APPEND), ("language", "en-US")],
        )?
        else {
            return Ok(None);
        };
        Ok(Some(RawProviderPayload {
            entity_kind: "season".into(),
            provider_id: format!("{show_id}:{season_number}"),
            payload: data.to_string(),
        }))
    }

    /// Search + floor gate + detail. Returns metadata when confidence ≥
    /// [`crate::match_score::AUTO_MATCH_FLOOR`] (ADR-0026 §2).
    pub fn resolve_title(
        &self,
        kind: MetadataKind,
        title: &str,
        year: Option<i32>,
    ) -> Result<TmdbResolve, ResolveError> {
        self.resolve_title_with_library_year(kind, title, year, None)
    }

    pub fn resolve_title_with_library_year(
        &self,
        kind: MetadataKind,
        title: &str,
        year: Option<i32>,
        library_year: Option<i32>,
    ) -> Result<TmdbResolve, ResolveError> {
        self.resolve_title_with_series_shape(
            kind,
            title,
            year,
            LibrarySeriesShape {
                year: library_year,
                ..Default::default()
            },
        )
    }

    pub fn resolve_title_with_series_shape(
        &self,
        kind: MetadataKind,
        title: &str,
        year: Option<i32>,
        library: LibrarySeriesShape,
    ) -> Result<TmdbResolve, ResolveError> {
        let search_kind = match kind {
            MetadataKind::Movie => SearchKind::Movie,
            MetadataKind::Episode | MetadataKind::Show => SearchKind::Tv,
        };
        let Some(candidate) =
            self.match_search_with_series_shape(search_kind, title, year, library)?
        else {
            return Ok(TmdbResolve::NoResults);
        };
        if !meets_auto_match_floor(candidate.confidence) {
            return Ok(TmdbResolve::BelowThreshold { candidate });
        }
        let (metadata, raw) = match search_kind {
            SearchKind::Movie => self.movie_detail(candidate.tmdb_id)?,
            SearchKind::Tv => {
                let (metadata, raw) = self.tv_detail(candidate.tmdb_id)?;
                if tv_payload_episode_count(&raw.payload) == Some(0) {
                    // A winner with zero episodes is not a candidate
                    // (ADR-0026, amended): refuse the bind. A sole title-hit
                    // never fetches shapes at score time, so the winner
                    // detail is where the shell is first seen; this is the
                    // zero-extra-request bind-time check.
                    return Ok(TmdbResolve::EmptyShell);
                }
                (metadata, raw)
            }
        };
        Ok(TmdbResolve::Matched {
            metadata: Box::new(metadata),
            raw,
            candidate,
        })
    }
}

#[derive(Debug)]
pub enum TmdbResolve {
    Matched {
        metadata: Box<CanonicalMetadata>,
        raw: RawProviderPayload,
        candidate: MatchCandidate,
    },
    /// The picked entity has zero episodes, so it is not a candidate
    /// (ADR-0026, amended). Not a provider error and not a find miss: the
    /// resolver records it as terminal `unmatched` with a reason.
    EmptyShell,
    BelowThreshold {
        candidate: MatchCandidate,
    },
    NoResults,
}

impl MetadataSource for TmdbClient {
    fn resolve(&self, input: &ResolveInput) -> Result<ProviderResult, ResolveError> {
        let kind = input.kind.unwrap_or(MetadataKind::Movie);
        // Enrich by id (ADR-0026 §8.3): detail only, no search, no floor gate.
        // Shares the same detail+raw mapping the fix `assign` path uses.
        if let Some(id) = input.tmdb_id {
            let (metadata, raw) = match kind {
                MetadataKind::Movie => self.movie_detail(id)?,
                MetadataKind::Show | MetadataKind::Episode => self.tv_detail(id)?,
            };
            return Ok(ProviderResult::Hit {
                metadata: Box::new(metadata),
                method: "tmdb_id",
                // A stored id resolves without comparing titles.
                confirmed: None,
                // Resolving a stored id compares no titles.
                raw: Some(raw),
            });
        }
        // NFO external id → TMDB `/find` (strategy note §2 A2/A3): an asserted
        // id outranks inference. A miss — the find call 404s, or the found
        // id's detail 404s — is reported to the resolver as `FindMiss`, which
        // falls through to a title search gated on the negative cache instead
        // of erroring the group into a repeat of the same find+404. The
        // name+year cross-check on a find hit lives in the resolver.
        if (kind == MetadataKind::Episode || kind == MetadataKind::Show)
            && (input.nfo_imdb_id.is_some() || input.nfo_tvdb_id.is_some())
        {
            let find_hit: Option<(i64, &'static str)> = if let Some(ref imdb_id) = input.nfo_imdb_id
                && let Some(tmdb_id) = self.find_tv_by_external_id("imdb_id", imdb_id)?
            {
                Some((tmdb_id, "nfo_imdb_find"))
            } else if let Some(tvdb_id) = input.nfo_tvdb_id {
                let tvdb_s = tvdb_id.to_string();
                if let Some(tmdb_id) = self.find_tv_by_external_id("tvdb_id", &tvdb_s)? {
                    Some((tmdb_id, "nfo_tvdb_find"))
                } else {
                    None
                }
            } else {
                None
            };
            if let Some((tmdb_id, method)) = find_hit {
                match self.tv_detail(tmdb_id) {
                    Ok((metadata, raw)) => {
                        return Ok(ProviderResult::Hit {
                            metadata: Box::new(metadata),
                            method,
                            // `/find` resolves by external id, not by title.
                            confirmed: None,
                            // `/find` resolves by external id, not by title.
                            raw: Some(raw),
                        });
                    }
                    // A find-listed id whose detail 404s (stale or wrong
                    // external id) is a soft miss, not a resolve error: the
                    // item must fail into search, not sit pending behind a
                    // bare repeat of the same find+404.
                    Err(ResolveError::NotFound(_)) => {}
                    Err(e) => return Err(e),
                }
            }
            return Ok(ProviderResult::FindMiss);
        }
        let Some(title) = input.title.as_deref().filter(|t| !t.is_empty()) else {
            return Ok(ProviderResult::Miss);
        };
        match self.resolve_title_with_series_shape(
            kind,
            title,
            input.year,
            LibrarySeriesShape {
                year: input.library_year,
                episode_count: input.library_episode_count,
                season_count: input.library_season_count,
                folder_season_counts: input.folder_season_counts.clone(),
                folder_episode_titles: input.folder_episode_titles.clone(),
                folder_seasons: input.library_seasons.clone(),
                ref_season: input.ref_season,
                ref_episode: input.ref_episode,
                ref_episode_title: input.ref_episode_title.clone(),
            },
        )? {
            TmdbResolve::Matched {
                metadata,
                candidate,
                raw,
            } => Ok(ProviderResult::Hit {
                metadata,
                method: candidate.method,
                confirmed: candidate.confirmed_by_episode_title,
                raw: Some(raw),
            }),
            TmdbResolve::EmptyShell => Ok(ProviderResult::EmptyShell),
            TmdbResolve::BelowThreshold { candidate } => Ok(ProviderResult::BelowThreshold {
                confidence: candidate.confidence,
                method: candidate.method,
            }),
            TmdbResolve::NoResults => Ok(ProviderResult::Miss),
        }
    }

    fn fetch_season(
        &self,
        show_id: i64,
        season_number: i32,
    ) -> Result<Option<RawProviderPayload>, ResolveError> {
        self.season_detail(show_id, season_number)
    }

    fn second_entity_candidates(
        &self,
        title: &str,
        exclude_show_id: i64,
    ) -> Result<Vec<SecondEntityCandidate>, ResolveError> {
        let hits = self.search(SearchKind::Tv, title)?;
        let mut out = Vec::new();
        for hit in hits.iter().take(MAX_SECOND_ENTITY_CANDIDATES) {
            if hit.id == exclude_show_id {
                continue;
            }
            let Some(episodes) = self.tv_candidate_own_seasons(hit.id)? else {
                // No episodes reachable. The empty-shell exclusion (#111)
                // already says an entity with no episodes is never a
                // candidate; there is nothing here for a file to bind to.
                continue;
            };
            out.push(SecondEntityCandidate {
                tmdb_show_id: hit.id,
                shape: CandidateShape {
                    candidate_season_episodes: Some(episodes),
                    ..Default::default()
                },
            });
        }
        Ok(out)
    }
}

impl MetadataSource for &TmdbClient {
    fn resolve(&self, input: &ResolveInput) -> Result<ProviderResult, ResolveError> {
        (*self).resolve(input)
    }

    fn fetch_season(
        &self,
        show_id: i64,
        season_number: i32,
    ) -> Result<Option<RawProviderPayload>, ResolveError> {
        (*self).fetch_season(show_id, season_number)
    }

    fn second_entity_candidates(
        &self,
        title: &str,
        exclude_show_id: i64,
    ) -> Result<Vec<SecondEntityCandidate>, ResolveError> {
        (*self).second_entity_candidates(title, exclude_show_id)
    }
}

/// Named refuse when TMDB rejects the active key (ADR-0031 §4).
/// Does not consult embedded as a fallback — the active source already won
/// precedence at resolve time.
fn auth_rejected_error(status: u16, creds: &TmdbCredentials) -> Option<ResolveError> {
    if status == 401 || status == 403 {
        Some(ResolveError::Provider(creds.rejected_reason()))
    } else {
        None
    }
}

fn urlencoding_encode(s: &str) -> String {
    let mut out = String::with_capacity(s.len() * 2);
    for b in s.bytes() {
        match b {
            b'A'..=b'Z' | b'a'..=b'z' | b'0'..=b'9' | b'-' | b'_' | b'.' | b'~' => {
                out.push(b as char);
            }
            b' ' => out.push_str("%20"),
            _ => out.push_str(&format!("%{b:02X}")),
        }
    }
    out
}

/// ureq Status errors embed the request URL; strip the query api_key.
fn scrub_tmdb_url_secret(msg: &str) -> String {
    let mut out = String::with_capacity(msg.len());
    let bytes = msg.as_bytes();
    let mut i = 0;
    while i < bytes.len() {
        if bytes[i..].starts_with(b"api_key=") {
            out.push_str("api_key=REDACTED");
            i += "api_key=".len();
            while i < bytes.len() && bytes[i].is_ascii_alphanumeric() {
                i += 1;
            }
            continue;
        }
        out.push(bytes[i] as char);
        i += 1;
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::match_score::AUTO_MATCH_FLOOR;

    #[test]
    fn resolve_title_floor_is_adr_value() {
        assert!((AUTO_MATCH_FLOOR - 0.80).abs() < f64::EPSILON);
    }

    #[test]
    fn scrub_strips_api_key_from_ureq_status_text() {
        let raw = "https://api.themoviedb.org/3/tv/1/season/1/episode/1?api_key=abc123secret: status code 404";
        assert_eq!(
            scrub_tmdb_url_secret(raw),
            "https://api.themoviedb.org/3/tv/1/season/1/episode/1?api_key=REDACTED: status code 404"
        );
    }

    #[test]
    fn stub_is_always_miss() {
        assert_eq!(
            TmdbStub.resolve(&ResolveInput::default()).unwrap(),
            ProviderResult::Miss
        );
    }

    #[test]
    fn tv_payload_episode_count_is_unknown_when_absent() {
        assert_eq!(
            tv_payload_episode_count(r#"{"id":1,"name":"Test Show"}"#),
            None,
            "a missing field is unknown, never zero"
        );
        assert_eq!(
            tv_payload_episode_count(r#"{"id":1,"number_of_episodes":0}"#),
            Some(0)
        );
        assert_eq!(
            tv_payload_episode_count(r#"{"id":1,"number_of_episodes":5}"#),
            Some(5)
        );
        assert_eq!(
            tv_payload_episode_count("not json"),
            None,
            "an unparseable payload is unknown, never a reject"
        );
    }

    #[test]
    fn auth_reject_override_does_not_mention_fallback_to_embedded() {
        for status in [401u16, 403] {
            for source in [TmdbKeySource::SecretsFile, TmdbKeySource::Env] {
                let creds = TmdbCredentials {
                    api_key: "bad".into(),
                    source,
                };
                let err = auth_rejected_error(status, &creds).expect("refuse");
                let msg = err.to_string();
                assert!(
                    msg.contains("not falling back to embedded"),
                    "status={status} source={source:?}: {msg}"
                );
            }
        }
    }

    #[test]
    fn auth_reject_embedded_is_named() {
        let creds = TmdbCredentials {
            api_key: "bad".into(),
            source: TmdbKeySource::Embedded,
        };
        let err = auth_rejected_error(401, &creds).expect("refuse");
        assert!(
            err.to_string()
                .contains("embedded application key rejected"),
            "{}",
            err
        );
    }

    #[test]
    fn auth_reject_ignores_non_auth_status() {
        let creds = TmdbCredentials {
            api_key: "x".into(),
            source: TmdbKeySource::Env,
        };
        assert!(auth_rejected_error(404, &creds).is_none());
        assert!(auth_rejected_error(429, &creds).is_none());
        assert!(auth_rejected_error(200, &creds).is_none());
    }
}
