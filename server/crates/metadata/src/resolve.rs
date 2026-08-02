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

#[derive(Debug, Clone, PartialEq)]
pub enum ResolveOutcome {
    Resolved {
        metadata: Box<CanonicalMetadata>,
        source: MetadataOrigin,
    },
    Unresolved,
}

#[derive(Debug)]
pub enum ResolveError {
    Nfo(NfoError),
    Provider(String),
}

impl std::fmt::Display for ResolveError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Nfo(e) => write!(f, "{e}"),
            Self::Provider(e) => write!(f, "provider error: {e}"),
        }
    }
}

impl std::error::Error for ResolveError {
    fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
        match self {
            Self::Nfo(e) => Some(e),
            Self::Provider(_) => None,
        }
    }
}

impl From<NfoError> for ResolveError {
    fn from(value: NfoError) -> Self {
        Self::Nfo(value)
    }
}

/// One metadata backend. NFO and TMDB share this so the resolver stays one path.
pub trait MetadataSource {
    fn resolve(&self, input: &ResolveInput) -> Result<Option<CanonicalMetadata>, ResolveError>;
}

/// NFO-backed source: parse `input.nfo_xml` when present.
#[derive(Debug, Default, Clone, Copy)]
pub struct NfoSource;

impl MetadataSource for NfoSource {
    fn resolve(&self, input: &ResolveInput) -> Result<Option<CanonicalMetadata>, ResolveError> {
        let Some(xml) = input.nfo_xml.as_deref() else {
            return Ok(None);
        };
        if xml.trim().is_empty() {
            return Ok(None);
        }
        Ok(Some(parse_nfo(xml)?))
    }
}

pub struct Resolver<N, T> {
    pub nfo: N,
    pub tmdb: T,
}

impl Default for Resolver<NfoSource, crate::tmdb::TmdbStub> {
    fn default() -> Self {
        Self {
            nfo: NfoSource,
            tmdb: crate::tmdb::TmdbStub,
        }
    }
}

impl<N: MetadataSource, T: MetadataSource> Resolver<N, T> {
    pub fn resolve(&self, input: &ResolveInput) -> Result<ResolveOutcome, ResolveError> {
        if let Some(metadata) = self.nfo.resolve(input)? {
            return Ok(ResolveOutcome::Resolved {
                metadata: Box::new(metadata),
                source: MetadataOrigin::Nfo,
            });
        }
        if let Some(metadata) = self.tmdb.resolve(input)? {
            return Ok(ResolveOutcome::Resolved {
                metadata: Box::new(metadata),
                source: MetadataOrigin::Tmdb,
            });
        }
        Ok(ResolveOutcome::Unresolved)
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
            ResolveOutcome::Unresolved => panic!("expected NFO resolve"),
        }
    }

    #[test]
    fn unresolved_without_nfo_when_tmdb_is_stub() {
        let outcome = Resolver {
            nfo: NfoSource,
            tmdb: TmdbStub,
        }
        .resolve(&ResolveInput { nfo_xml: None })
        .unwrap();
        assert_eq!(outcome, ResolveOutcome::Unresolved);
    }

    #[test]
    fn malformed_nfo_does_not_fall_through_to_tmdb() {
        let err = resolve(&ResolveInput {
            nfo_xml: Some(fixture("malformed.nfo")),
        })
        .unwrap_err();
        assert!(matches!(err, ResolveError::Nfo(_)));
    }
}
