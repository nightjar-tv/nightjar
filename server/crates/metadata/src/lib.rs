//! Metadata resolution: NFO first, then TMDB (ADR-0025 / ADR-0026).
//!
//! Slice 1: canonical model, NFO parse, resolver skeleton, TMDB stub.
//! No network, queue, or matcher.

mod model;
mod nfo;
mod resolve;
mod tmdb;

pub use model::{
    ArtworkKind, ArtworkRef, CanonicalMetadata, CastMember, CollectionRef, MetadataKind,
    ProviderIds, Rating, item_key_for_metadata,
};
pub use nfo::{NfoError, parse_nfo};
pub use resolve::{
    MetadataOrigin, MetadataSource, NfoSource, ResolveError, ResolveInput, ResolveOutcome,
    Resolver, resolve,
};
pub use tmdb::TmdbStub;
