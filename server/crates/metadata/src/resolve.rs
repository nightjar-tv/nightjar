//! Resolve NFO first, then TMDB (ADR-0026 resolution path).

use rusqlite::Connection;

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
    /// Cleaned title for provider search when NFO is absent.
    pub title: Option<String>,
    pub year: Option<i32>,
    /// Series premiere year from the library (earliest episode year, else
    /// show-folder `(YYYY)`). Used to pin multi exact-title TV hits.
    pub library_year: Option<i32>,
    /// Distinct episode files / seasons under the show (TV collision pin).
    pub library_episode_count: Option<u32>,
    pub library_season_count: Option<u32>,
    /// ADR-0032 reference episode for title pin (usable after-token only).
    pub ref_season: Option<i32>,
    pub ref_episode: Option<i32>,
    pub ref_episode_title: Option<String>,
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
}

impl std::fmt::Display for UnresolvedReason {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::NoMatch => write!(f, "no match"),
            Self::NfoInvalid { detail } => write!(f, "invalid nfo: {detail}"),
            Self::BelowThreshold { confidence, method } => {
                write!(f, "below threshold: {confidence:.2} [{method}]")
            }
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
    },
    Unresolved {
        reason: UnresolvedReason,
    },
}

#[derive(Debug)]
pub enum ResolveError {
    /// Provider-level failure (network, auth, …). NFO parse problems are
    /// [`UnresolvedReason::NfoInvalid`], not this.
    Provider(String),
}

impl std::fmt::Display for ResolveError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Provider(e) => write!(f, "provider error: {e}"),
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
        /// Entity-keyed raw body for ADR-0026 §4 persistence (`None` for stubs).
        raw: Option<RawProviderPayload>,
    },
    BelowThreshold {
        confidence: f64,
        method: &'static str,
    },
    Miss,
}

/// One metadata backend (TMDB today; keep the trait thin — Rule 4.7).
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
        let Some(xml) = input.nfo_xml.as_deref() else {
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
        match NfoSource.attempt(input) {
            NfoAttempt::Parsed(metadata) => {
                return Ok(ResolveOutcome::Resolved {
                    metadata,
                    source: MetadataOrigin::Nfo,
                    match_method: None,
                });
            }
            NfoAttempt::Invalid(err) => {
                return Ok(ResolveOutcome::Unresolved {
                    reason: UnresolvedReason::NfoInvalid {
                        detail: err.to_string(),
                    },
                });
            }
            NfoAttempt::Absent => {}
        }

        let cache_kind = match input.kind.unwrap_or(MetadataKind::Movie) {
            MetadataKind::Movie => CacheKind::Movie,
            MetadataKind::Episode | MetadataKind::Show => CacheKind::Tv,
        };
        let qk = input
            .title
            .as_deref()
            .filter(|t| !t.is_empty())
            .map(|t| query_key(t, input.year));

        if let (Some(conn), Some(qk)) = (conn, &qk) {
            let now = now_rfc3339();
            if let Ok(Some(entry)) =
                negative_cache::should_skip(conn, PROVIDER_TMDB, cache_kind, qk, &now)
            {
                return Ok(match entry.reason {
                    NegativeReason::BelowThreshold => ResolveOutcome::Unresolved {
                        reason: UnresolvedReason::BelowThreshold {
                            confidence: entry.confidence.unwrap_or(0.0),
                            method: "negative_cache".into(),
                        },
                    },
                    NegativeReason::NoResults | NegativeReason::ApiError => {
                        ResolveOutcome::Unresolved {
                            reason: UnresolvedReason::NoMatch,
                        }
                    }
                });
            }
        }

        match self.tmdb.resolve(input) {
            Ok(ProviderResult::Hit {
                metadata,
                method,
                raw,
            }) => {
                if let (Some(conn), Some(raw)) = (conn, raw.as_ref()) {
                    if let Some(ref qk) = qk {
                        let _ = negative_cache::clear(conn, PROVIDER_TMDB, cache_kind, qk);
                    }
                    canonical::persist_mapped_hit(conn, PROVIDER_TMDB, raw, &metadata)
                        .map_err(ResolveError::Provider)?;
                }
                Ok(ResolveOutcome::Resolved {
                    metadata,
                    source: MetadataOrigin::Tmdb,
                    match_method: Some(method.to_string()),
                })
            }
            Ok(ProviderResult::BelowThreshold { confidence, method }) => {
                if let (Some(conn), Some(qk)) = (conn, &qk) {
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
                Ok(ResolveOutcome::Unresolved {
                    reason: UnresolvedReason::BelowThreshold {
                        confidence,
                        method: method.to_string(),
                    },
                })
            }
            Ok(ProviderResult::Miss) => {
                if let (Some(conn), Some(qk)) = (conn, &qk) {
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
                Ok(ResolveOutcome::Unresolved {
                    reason: UnresolvedReason::NoMatch,
                })
            }
            Err(e) => {
                // api_error: not cached — transient failures must not park a day.
                Err(e)
            }
        }
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
    use rusqlite::Connection;
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
            } => {
                assert_eq!(source, MetadataOrigin::Nfo);
                assert_eq!(metadata.title, "Fight Club");
                assert_eq!(match_method, None);
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

    #[test]
    fn malformed_nfo_is_unresolved_reason_not_tmdb_fallback() {
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
}
