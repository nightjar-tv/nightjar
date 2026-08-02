//! Resolve NFO first, then TMDB (ADR-0026 resolution path).

use crate::model::CanonicalMetadata;
use crate::nfo::{NfoError, parse_nfo};

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum MetadataOrigin {
    Nfo,
    Tmdb,
}

#[derive(Debug, Clone, Default)]
pub struct ResolveInput {
    /// Raw NFO XML when a sidecar (or equivalent) is present.
    pub nfo_xml: Option<String>,
}

/// Why an item stayed unmatched. Surfaced for the fix flow (ADR-0028); not a
/// log-only hard failure. A present-but-corrupt NFO must not fall through to
/// TMDB ("local data always wins").
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum UnresolvedReason {
    /// No usable NFO and the provider returned nothing.
    NoMatch,
    /// NFO bytes were present but could not be parsed. Item stays unmatched
    /// with this reason until the user fixes the file or clears/retries.
    NfoInvalid { detail: String },
}

impl std::fmt::Display for UnresolvedReason {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::NoMatch => write!(f, "no match"),
            Self::NfoInvalid { detail } => write!(f, "invalid nfo: {detail}"),
        }
    }
}

#[derive(Debug, Clone, PartialEq)]
pub enum ResolveOutcome {
    Resolved {
        metadata: Box<CanonicalMetadata>,
        source: MetadataOrigin,
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

/// One metadata backend (TMDB today; keep the trait thin — Rule 4.7).
pub trait MetadataSource {
    fn resolve(&self, input: &ResolveInput) -> Result<Option<CanonicalMetadata>, ResolveError>;
}

/// Parses `input.nfo_xml` when present. Not a [`MetadataSource`]: corrupt NFO
/// must become [`UnresolvedReason::NfoInvalid`] in the resolver, not a trait
/// `None` that would look like "try TMDB next".
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
        match NfoSource.attempt(input) {
            NfoAttempt::Parsed(metadata) => {
                return Ok(ResolveOutcome::Resolved {
                    metadata,
                    source: MetadataOrigin::Nfo,
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
        if let Some(metadata) = self.tmdb.resolve(input)? {
            return Ok(ResolveOutcome::Resolved {
                metadata: Box::new(metadata),
                source: MetadataOrigin::Tmdb,
            });
        }
        Ok(ResolveOutcome::Unresolved {
            reason: UnresolvedReason::NoMatch,
        })
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

    fn fixture(name: &str) -> String {
        let path = format!("{}/tests/fixtures/{name}", env!("CARGO_MANIFEST_DIR"));
        std::fs::read_to_string(&path).unwrap_or_else(|e| panic!("read {path}: {e}"))
    }

    #[test]
    fn prefers_nfo_over_tmdb_stub() {
        let outcome = resolve(&ResolveInput {
            nfo_xml: Some(fixture("movie.nfo")),
        })
        .unwrap();
        match outcome {
            ResolveOutcome::Resolved { source, metadata } => {
                assert_eq!(source, MetadataOrigin::Nfo);
                assert_eq!(metadata.title, "Fight Club");
            }
            ResolveOutcome::Unresolved { .. } => panic!("expected NFO resolve"),
        }
    }

    #[test]
    fn unresolved_without_nfo_when_tmdb_is_stub() {
        let outcome = Resolver { tmdb: TmdbStub }
            .resolve(&ResolveInput { nfo_xml: None })
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
}
