//! TMDB HTTP client and [`MetadataSource`] (ADR-0026).

mod map;

use std::time::Duration;

use serde_json::Value;

use crate::match_score::{
    CandidateShape, LibrarySeriesShape, MatchCandidate, SearchHit, SearchKind,
    meets_auto_match_floor, needs_collision_detail, norm_key, score_search_with_shape,
};
use crate::model::{CanonicalMetadata, MetadataKind};
use crate::resolve::{MetadataSource, ProviderResult, ResolveError, ResolveInput};

pub use map::{RawProviderPayload, map_movie_detail, map_tv_detail};

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

#[derive(Debug, Clone)]
pub struct TmdbCredentials {
    /// v3 api_key query param and/or v4 bearer.
    pub api_key: Option<String>,
    pub bearer: Option<String>,
}

impl TmdbCredentials {
    /// ADR-0026 override slot: env `NIGHTJAR_TMDB_API_KEY`, else `TMDB_API_KEY` /
    /// `TMDB_BEARER`, else `~/.config/nightjar/tmdb_secret` (dev/dogfood).
    pub fn from_env() -> Option<Self> {
        let mut api_key = std::env::var("NIGHTJAR_TMDB_API_KEY")
            .ok()
            .filter(|s| !s.is_empty())
            .or_else(|| std::env::var("TMDB_API_KEY").ok().filter(|s| !s.is_empty()));
        let mut bearer = std::env::var("TMDB_BEARER")
            .ok()
            .or_else(|| std::env::var("TMDB_ACCESS_TOKEN").ok())
            .filter(|s| !s.is_empty());

        if api_key.is_none() && bearer.is_none() {
            let path = std::env::var_os("TMDB_SECRET_FILE")
                .map(std::path::PathBuf::from)
                .unwrap_or_else(|| {
                    dirs_next_home()
                        .map(|h| h.join(".config/nightjar/tmdb_secret"))
                        .unwrap_or_default()
                });
            if let Ok(raw) = std::fs::read_to_string(&path) {
                let t = raw.trim();
                if t.starts_with("eyJ") {
                    bearer = Some(t.to_string());
                } else if !t.is_empty() {
                    api_key = Some(t.to_string());
                }
            }
        }
        if let Some(ref k) = api_key
            && k.starts_with("eyJ")
            && bearer.is_none()
        {
            bearer = api_key.take();
        }
        if api_key.is_none() && bearer.is_none() {
            return None;
        }
        Some(Self { api_key, bearer })
    }
}

fn dirs_next_home() -> Option<std::path::PathBuf> {
    std::env::var_os("HOME").map(std::path::PathBuf::from)
}

#[derive(Debug)]
pub struct TmdbClient {
    creds: TmdbCredentials,
    agent: ureq::Agent,
    min_interval: Duration,
    last_call: std::sync::Mutex<Option<std::time::Instant>>,
}

impl TmdbClient {
    pub fn new(creds: TmdbCredentials) -> Self {
        let agent = ureq::AgentBuilder::new()
            .timeout_connect(Duration::from_secs(10))
            .timeout_read(Duration::from_secs(30))
            .build();
        Self {
            creds,
            agent,
            min_interval: Duration::from_millis(40),
            last_call: std::sync::Mutex::new(None),
        }
    }

    fn throttle(&self) {
        let mut guard = self.last_call.lock().unwrap_or_else(|e| e.into_inner());
        if let Some(prev) = *guard {
            let elapsed = prev.elapsed();
            if elapsed < self.min_interval {
                std::thread::sleep(self.min_interval - elapsed);
            }
        }
        *guard = Some(std::time::Instant::now());
    }

    fn get_json(&self, path: &str, query: &[(&str, &str)]) -> Result<Value, ResolveError> {
        self.throttle();
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
        if let Some(ref key) = self.creds.api_key {
            push(&mut url, &mut first, "api_key", key);
        }

        let mut req = self.agent.get(&url);
        if let Some(ref bearer) = self.creds.bearer {
            req = req.set("Authorization", &format!("Bearer {bearer}"));
        }
        let resp = req
            .call()
            .map_err(|e| ResolveError::Provider(e.to_string()))?;
        let status = resp.status();
        let body = resp
            .into_string()
            .map_err(|e| ResolveError::Provider(e.to_string()))?;
        if !(200..300).contains(&status) {
            return Err(ResolveError::Provider(format!(
                "TMDB {status}: {}",
                body.chars().take(200).collect::<String>()
            )));
        }
        serde_json::from_str(&body).map_err(|e| ResolveError::Provider(e.to_string()))
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
    /// survives the year discriminator (tens of shows, not per-file).
    pub fn match_search_with_series_shape(
        &self,
        kind: SearchKind,
        title: &str,
        year: Option<i32>,
        library: LibrarySeriesShape,
    ) -> Result<Option<MatchCandidate>, ResolveError> {
        let results = self.search(kind, title, year)?;
        if !needs_collision_detail(&results, title, year, kind, library) {
            return Ok(score_search_with_shape(
                &results, title, year, kind, library, None,
            ));
        }
        let nk = norm_key(title);
        let exact: Vec<&SearchHit> = results
            .iter()
            .filter(|r| {
                let (primary, original) = match kind {
                    SearchKind::Movie => (r.title.as_deref(), r.original_title.as_deref()),
                    SearchKind::Tv => (r.name.as_deref(), r.original_name.as_deref()),
                };
                primary.is_some_and(|t| norm_key(t) == nk)
                    || original.is_some_and(|t| norm_key(t) == nk)
            })
            .take(8)
            .collect();
        let mut shapes = Vec::with_capacity(exact.len());
        for hit in &exact {
            shapes.push(self.tv_candidate_shape(hit)?);
        }
        // Re-score using only the shaped exact subset as the result list so
        // shape indices align; non-exact hits already lost for pinning.
        let shaped_results: Vec<SearchHit> = exact.iter().map(|h| (*h).clone()).collect();
        Ok(score_search_with_shape(
            &shaped_results,
            title,
            year,
            kind,
            library,
            Some(&shapes),
        ))
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
            },
        )? {
            TmdbResolve::Matched {
                metadata,
                candidate,
                ..
            } => Ok(ProviderResult::Hit {
                metadata,
                method: candidate.method,
            }),
            TmdbResolve::BelowThreshold { candidate } => Ok(ProviderResult::BelowThreshold {
                confidence: candidate.confidence,
                method: candidate.method,
            }),
            TmdbResolve::NoResults => Ok(ProviderResult::Miss),
        }
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

#[cfg(test)]
mod tests {
    use super::*;
    use crate::match_score::AUTO_MATCH_FLOOR;

    #[test]
    fn resolve_title_floor_is_adr_value() {
        assert!((AUTO_MATCH_FLOOR - 0.80).abs() < f64::EPSILON);
    }

    #[test]
    fn stub_is_always_miss() {
        assert_eq!(
            TmdbStub.resolve(&ResolveInput::default()).unwrap(),
            ProviderResult::Miss
        );
    }
}
