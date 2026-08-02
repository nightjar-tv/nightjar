//! Metadata resolution: NFO first, then TMDB (ADR-0025 / ADR-0026).
//!
//! Slice 2: confidence matcher (floor 0.80), TMDB search/detail client,
//! detail → canonical map. Measure harness: `cargo run -p nightjar-metadata
//! --bin metadata-match-measure`.

mod clean;
mod match_score;
mod model;
mod nfo;
mod resolve;
mod tmdb;

pub use clean::{clean_movie_title, clean_show_title, year_from_path};
pub use match_score::{
    AUTO_MATCH_FLOOR, MatchCandidate, SearchHit, SearchKind, meets_auto_match_floor, norm_key,
    score_search,
};
pub use model::{
    ArtworkKind, ArtworkRef, CanonicalMetadata, CastMember, CollectionRef, MetadataKind,
    ProviderIds, Rating, item_key_for_metadata,
};
pub use nfo::{NfoError, parse_nfo};
pub use resolve::{
    MetadataOrigin, MetadataSource, NfoSource, ProviderResult, ResolveError, ResolveInput,
    ResolveOutcome, Resolver, UnresolvedReason, resolve,
};
pub use tmdb::{
    RawProviderPayload, TmdbClient, TmdbCredentials, TmdbResolve, TmdbStub, map_movie_detail,
    map_tv_detail,
};
