//! Canonical metadata model (strategy note + ADR-0025 / ADR-0026 ids).

use nightjar_core::MediaKind;
use serde::{Deserialize, Serialize};

/// What the NFO / provider record describes.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum MetadataKind {
    Movie,
    Episode,
    Show,
}

impl MetadataKind {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Movie => "movie",
            Self::Episode => "episode",
            Self::Show => "show",
        }
    }
}

/// External ids. TMDB movie/episode id is the identity source for
/// `item_key` (ADR-0025). NFO-only tvdb/imdb may key until a TMDB id exists.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct ProviderIds {
    pub tmdb: Option<i64>,
    pub tmdb_show: Option<i64>,
    pub imdb: Option<String>,
    pub tvdb: Option<i64>,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct CastMember {
    pub name: String,
    pub role: Option<String>,
    pub order: Option<i32>,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Rating {
    pub source: String,
    pub value: f64,
    pub votes: Option<i64>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum ArtworkKind {
    Poster,
    Backdrop,
    Banner,
    Logo,
    Other,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ArtworkRef {
    pub kind: ArtworkKind,
    /// Local path or remote URL as written in the NFO / provider payload.
    pub path: String,
}

/// Movie collection fields stored on detail write (ADR-0026 §6).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct CollectionRef {
    pub id: Option<i64>,
    pub name: Option<String>,
}

/// One canonical record regardless of source. Nothing outside the resolver
/// layer should care whether this came from NFO or TMDB.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct CanonicalMetadata {
    pub kind: MetadataKind,
    pub title: String,
    pub original_title: Option<String>,
    pub year: Option<i32>,
    pub plot: Option<String>,
    pub genres: Vec<String>,
    pub runtime_minutes: Option<i32>,
    pub cast: Vec<CastMember>,
    pub ratings: Vec<Rating>,
    pub ids: ProviderIds,
    pub artwork: Vec<ArtworkRef>,
    pub collection: Option<CollectionRef>,
    pub season: Option<i32>,
    pub episode: Option<i32>,
}

impl CanonicalMetadata {
    /// Map to the scanner/media kind where possible.
    pub fn media_kind(&self) -> MediaKind {
        match self.kind {
            MetadataKind::Movie => MediaKind::Movie,
            MetadataKind::Episode => MediaKind::Episode,
            MetadataKind::Show => MediaKind::Unknown,
        }
    }
}

/// Build an ADR-0025 `item_key` when a provider id is present.
/// Returns `None` when only a path key would apply (caller supplies that).
pub fn item_key_for_metadata(meta: &CanonicalMetadata) -> Option<String> {
    if let Some(id) = meta.ids.tmdb {
        return Some(match meta.kind {
            MetadataKind::Movie => format!("tmdb:movie:{id}"),
            MetadataKind::Episode => format!("tmdb:episode:{id}"),
            // Show-level NFO is not an episode watch key; series identity is
            // not a watch-state key in ADR-0025. Leave unset until episode id.
            MetadataKind::Show => return None,
        });
    }
    if let Some(id) = meta.ids.tvdb {
        return Some(format!("tvdb:{id}"));
    }
    None
}
