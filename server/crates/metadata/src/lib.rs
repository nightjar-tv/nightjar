//! Metadata resolution: NFO first, then TMDB (ADR-0025 / ADR-0026).
//!
//! Slice 4: metadata queue (query over `metadata_status`) and API
//! request-rate limiter (ADR-0026 §7/§8).

mod clean;
mod match_score;
mod model;
mod negative_cache;
mod nfo;
mod queue;
mod rate_limit;
mod raw_payload;
mod resolve;
mod tmdb;

pub use clean::{
    clean_movie_title, clean_show_title, fold_title_orthography, series_library_year,
    year_from_path, year_from_show_folder,
};
pub use match_score::{
    AUTO_MATCH_FLOOR, CandidateShape, LibrarySeriesShape, MatchCandidate, SearchHit, SearchKind,
    meets_auto_match_floor, needs_collision_detail, norm_key, score_search,
    score_search_with_library_year, score_search_with_shape,
};
pub use model::{
    ArtworkKind, ArtworkRef, CanonicalMetadata, CastMember, CollectionRef, MetadataKind,
    ProviderIds, Rating, item_key_for_metadata,
};
pub use negative_cache::{
    CacheKind, NegativeEntry, NegativeReason, PROVIDER_TMDB, ReasonCounts,
    clear as clear_negative_cache, counts_by_reason, query_key, record_miss, should_skip,
};
pub use nfo::{NfoError, parse_nfo};
pub use queue::{
    DrainOptions, DrainStats, MetadataStatus, PendingItem, QueueBand, T_FIRST_SCREEN_PASS_SECS,
    T_FIRST_SCREEN_PREDICTED_SECS, VISIBLE_FIRST_SCREEN_N, VisibleProxy, VisibleProxyUnit,
    drain_pending, proxy_terminal_progress, queue_band_for_item, set_metadata_status,
    snapshot_visible_proxy, snapshot_visible_proxy_filtered, snapshot_visible_proxy_n,
};
pub use rate_limit::{ApiRateLimiter, DEFAULT_MAX_IN_FLIGHT, DEFAULT_REQUESTS_PER_SEC};
pub use raw_payload::{
    PayloadStoreStats, get_raw_payload, payload_store_stats, persist_hit_with_canonical,
    upsert_raw_payload,
};
pub use resolve::{
    MetadataOrigin, MetadataSource, NfoSource, ProviderResult, ResolveError, ResolveInput,
    ResolveOutcome, Resolver, UnresolvedReason, resolve,
};
pub use tmdb::{
    RawProviderPayload, TmdbClient, TmdbCredentials, TmdbResolve, TmdbStub, map_movie_detail,
    map_tv_detail,
};
