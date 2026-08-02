//! TMDB provider stub. No network in this slice (ADR-0026 path exists; calls later).

use crate::model::CanonicalMetadata;
use crate::resolve::{MetadataSource, ResolveError, ResolveInput};

/// Placeholder for the live TMDB client. Always unresolved until a later slice.
#[derive(Debug, Default, Clone, Copy)]
pub struct TmdbStub;

impl MetadataSource for TmdbStub {
    fn resolve(&self, _input: &ResolveInput) -> Result<Option<CanonicalMetadata>, ResolveError> {
        Ok(None)
    }
}
