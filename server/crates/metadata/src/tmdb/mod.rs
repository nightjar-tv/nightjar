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
use crate::resolve::{MetadataSource, ProviderResult, ResolveError, ResolveInput};

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
            None => Err(ResolveError::Provider(format!("TMDB 404: {path}"))),
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

    pub fn search(
        &self,
        kind: SearchKind,
        title: &str,
        year: Option<i32>,
    ) -> Result<Vec<SearchHit>, ResolveError> {
        let path = match kind {
            SearchKind::Movie => "/search/movie",
            SearchKind::Tv => "/search/tv",
        };
        let year_s = year.map(|y| y.to_string());
        let mut q: Vec<(&str, &str)> = vec![("query", title)];
        if let Some(ref y) = year_s {
            match kind {
                SearchKind::Movie => q.push(("year", y.as_str())),
                SearchKind::Tv => q.push(("first_air_date_year", y.as_str())),
            }
        }
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
        let results = self.search(kind, title, year)?;
        if !needs_collision_detail(&results, title, year, kind, library.clone()) {
            return Ok(score_search_with_shape(
                &results, title, year, kind, library, None,
            ));
        }
        let nk = norm_key(title);
        let exact: Vec<&SearchHit> = results
            .iter()
            .filter(|r| crate::match_score::title_hit(r, &nk, kind))
            .take(8)
            .collect();
        let mut shapes = Vec::with_capacity(exact.len());
        for hit in &exact {
            shapes.push(self.tv_candidate_shape(hit)?);
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
        if let Some(ref c) = scored
            && meets_auto_match_floor(c.confidence)
        {
            return Ok(scored);
        }
        // ADR-0032 step 4: episode-title pin on still-unpinned TV multi-exact.
        let Some(ref_title) = library.ref_episode_title.as_deref() else {
            return Ok(scored);
        };
        let (Some(ref_season), Some(ref_episode)) = (library.ref_season, library.ref_episode)
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
        let mut names: Vec<Option<String>> = Vec::with_capacity(exact_refs.len());
        for hit in &exact_refs {
            names.push(self.tv_episode_name(hit.id, ref_season, ref_episode)?);
        }
        if let Some((hit, method)) = pin_episode_title(&exact_refs, &names, ref_title) {
            return Ok(Some(MatchCandidate {
                tmdb_id: hit.id,
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

    fn tv_candidate_shape(&self, hit: &SearchHit) -> Result<CandidateShape, ResolveError> {
        let year = hit
            .first_air_date
            .as_deref()
            .and_then(|d| d.get(..4)?.parse().ok());
        let data = self.get_json(&format!("/tv/{}", hit.id), &[("language", "en-US")])?;
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
        })
    }

    fn tv_episode_name(
        &self,
        show_id: i64,
        season: i32,
        episode: i32,
    ) -> Result<Option<String>, ResolveError> {
        // Missing episode on a tied candidate is a decline signal for that
        // row (ADR-0032), not a resolve failure for the show group.
        let Some(data) = self.get_json_optional(
            &format!("/tv/{show_id}/season/{season}/episode/{episode}"),
            &[("language", "en-US")],
        )?
        else {
            return Ok(None);
        };
        Ok(data
            .get("name")
            .and_then(|v| v.as_str())
            .map(|s| s.to_string()))
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
            SearchKind::Tv => self.tv_detail(candidate.tmdb_id)?,
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
    BelowThreshold {
        candidate: MatchCandidate,
    },
    NoResults,
}

impl MetadataSource for TmdbClient {
    fn resolve(&self, input: &ResolveInput) -> Result<ProviderResult, ResolveError> {
        let Some(title) = input.title.as_deref().filter(|t| !t.is_empty()) else {
            return Ok(ProviderResult::Miss);
        };
        let kind = input.kind.unwrap_or(MetadataKind::Movie);
        match self.resolve_title_with_series_shape(
            kind,
            title,
            input.year,
            LibrarySeriesShape {
                year: input.library_year,
                episode_count: input.library_episode_count,
                season_count: input.library_season_count,
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
                raw: Some(raw),
            }),
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
