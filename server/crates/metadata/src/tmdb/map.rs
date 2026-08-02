//! Map TMDB detail JSON → [`CanonicalMetadata`] (ADR-0026 append sets).

use serde_json::Value;

use crate::model::{
    ArtworkKind, ArtworkRef, CanonicalMetadata, CastMember, CollectionRef, MetadataKind,
    ProviderIds, Rating,
};
use crate::resolve::ResolveError;

/// One entity-keyed raw body for persistence (ADR-0026 §4).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RawProviderPayload {
    pub entity_kind: String,
    pub provider_id: String,
    pub payload: String,
}

pub fn map_movie_detail(data: &Value) -> Result<CanonicalMetadata, ResolveError> {
    let title = data
        .get("title")
        .and_then(|v| v.as_str())
        .ok_or_else(|| ResolveError::Provider("movie detail missing title".into()))?
        .to_string();
    let id = data
        .get("id")
        .and_then(|v| v.as_i64())
        .ok_or_else(|| ResolveError::Provider("movie detail missing id".into()))?;
    let year = data
        .get("release_date")
        .and_then(|v| v.as_str())
        .and_then(|d| d.get(..4)?.parse().ok());
    Ok(CanonicalMetadata {
        kind: MetadataKind::Movie,
        title,
        original_title: text(data, "original_title"),
        year,
        air_date: None,
        plot: text(data, "overview"),
        genres: genres(data),
        runtime_minutes: data
            .get("runtime")
            .and_then(|v| v.as_i64())
            .map(|n| n as i32),
        cast: cast_from_credits(data),
        ratings: movie_ratings(data),
        ids: ProviderIds {
            tmdb: Some(id),
            tmdb_show: None,
            imdb: data
                .pointer("/external_ids/imdb_id")
                .and_then(|v| v.as_str())
                .map(str::to_string),
            tvdb: None,
        },
        artwork: artwork_from_images(data),
        collection: data.get("belongs_to_collection").and_then(|c| {
            if c.is_null() {
                return None;
            }
            Some(CollectionRef {
                id: c.get("id").and_then(|v| v.as_i64()),
                name: c.get("name").and_then(|v| v.as_str()).map(str::to_string),
            })
        }),
        season: None,
        episode: None,
    })
}

pub fn map_tv_detail(data: &Value) -> Result<CanonicalMetadata, ResolveError> {
    let title = data
        .get("name")
        .and_then(|v| v.as_str())
        .ok_or_else(|| ResolveError::Provider("tv detail missing name".into()))?
        .to_string();
    let id = data
        .get("id")
        .and_then(|v| v.as_i64())
        .ok_or_else(|| ResolveError::Provider("tv detail missing id".into()))?;
    let year = data
        .get("first_air_date")
        .and_then(|v| v.as_str())
        .and_then(|d| d.get(..4)?.parse().ok());
    Ok(CanonicalMetadata {
        kind: MetadataKind::Show,
        title,
        original_title: text(data, "original_name"),
        year,
        air_date: None,
        plot: text(data, "overview"),
        genres: genres(data),
        runtime_minutes: data
            .get("episode_run_time")
            .and_then(|v| v.as_array())
            .and_then(|a| a.first())
            .and_then(|v| v.as_i64())
            .map(|n| n as i32),
        cast: cast_from_aggregate(data).unwrap_or_else(|| cast_from_credits(data)),
        ratings: Vec::new(),
        ids: ProviderIds {
            tmdb: Some(id),
            tmdb_show: Some(id),
            imdb: data
                .pointer("/external_ids/imdb_id")
                .and_then(|v| v.as_str())
                .map(str::to_string),
            tvdb: data
                .pointer("/external_ids/tvdb_id")
                .and_then(|v| v.as_i64()),
        },
        artwork: artwork_from_images(data),
        collection: None,
        season: None,
        episode: None,
    })
}

/// Episode rows from a season detail payload (ADR-0029 §1.1 / §1.2).
/// Kind-sparse: no genres/cast (inherit via `tmdb_show` → tv row).
pub fn map_episodes_from_season(
    show_id: i64,
    data: &Value,
) -> Result<Vec<CanonicalMetadata>, ResolveError> {
    let arr = data
        .get("episodes")
        .and_then(|v| v.as_array())
        .ok_or_else(|| ResolveError::Provider("season detail missing episodes".into()))?;
    let mut out = Vec::with_capacity(arr.len());
    for ep in arr {
        let id = ep
            .get("id")
            .and_then(|v| v.as_i64())
            .ok_or_else(|| ResolveError::Provider("episode missing id".into()))?;
        let title = ep
            .get("name")
            .and_then(|v| v.as_str())
            .unwrap_or("")
            .to_string();
        let air_date = text(ep, "air_date");
        let year = air_date.as_deref().and_then(|d| d.get(..4)?.parse().ok());
        let mut artwork = Vec::new();
        if let Some(path) = ep.get("still_path").and_then(|v| v.as_str())
            && !path.is_empty()
        {
            artwork.push(ArtworkRef {
                kind: ArtworkKind::Other,
                path: path.to_string(),
            });
        }
        let ratings = movie_ratings(ep); // vote_average / vote_count on episode object
        out.push(CanonicalMetadata {
            kind: MetadataKind::Episode,
            title,
            original_title: None,
            year,
            air_date,
            plot: text(ep, "overview"),
            genres: Vec::new(),
            runtime_minutes: ep.get("runtime").and_then(|v| v.as_i64()).map(|n| n as i32),
            cast: Vec::new(),
            ratings,
            ids: ProviderIds {
                tmdb: Some(id),
                tmdb_show: Some(show_id),
                imdb: None,
                tvdb: None,
            },
            artwork,
            collection: None,
            season: ep
                .get("season_number")
                .and_then(|v| v.as_i64())
                .map(|n| n as i32),
            episode: ep
                .get("episode_number")
                .and_then(|v| v.as_i64())
                .map(|n| n as i32),
        });
    }
    Ok(out)
}

fn text(data: &Value, key: &str) -> Option<String> {
    data.get(key)
        .and_then(|v| v.as_str())
        .map(str::trim)
        .filter(|s| !s.is_empty())
        .map(str::to_string)
}

fn genres(data: &Value) -> Vec<String> {
    data.get("genres")
        .and_then(|v| v.as_array())
        .map(|arr| {
            arr.iter()
                .filter_map(|g| g.get("name").and_then(|n| n.as_str()).map(str::to_string))
                .collect()
        })
        .unwrap_or_default()
}

fn cast_from_credits(data: &Value) -> Vec<CastMember> {
    data.pointer("/credits/cast")
        .and_then(|v| v.as_array())
        .map(|arr| {
            arr.iter()
                .take(40)
                .filter_map(|c| {
                    let name = c.get("name")?.as_str()?.to_string();
                    Some(CastMember {
                        name,
                        role: c
                            .get("character")
                            .and_then(|v| v.as_str())
                            .map(str::to_string),
                        order: c.get("order").and_then(|v| v.as_i64()).map(|n| n as i32),
                    })
                })
                .collect()
        })
        .unwrap_or_default()
}

fn cast_from_aggregate(data: &Value) -> Option<Vec<CastMember>> {
    let arr = data.pointer("/aggregate_credits/cast")?.as_array()?;
    Some(
        arr.iter()
            .take(40)
            .filter_map(|c| {
                let name = c.get("name")?.as_str()?.to_string();
                let role = c
                    .get("roles")
                    .and_then(|r| r.as_array())
                    .and_then(|a| a.first())
                    .and_then(|r| r.get("character"))
                    .and_then(|v| v.as_str())
                    .map(str::to_string);
                Some(CastMember {
                    name,
                    role,
                    order: c.get("order").and_then(|v| v.as_i64()).map(|n| n as i32),
                })
            })
            .collect(),
    )
}

fn movie_ratings(data: &Value) -> Vec<Rating> {
    let mut out = Vec::new();
    if let Some(v) = data.get("vote_average").and_then(|v| v.as_f64()) {
        out.push(Rating {
            source: "tmdb".into(),
            value: v,
            votes: data.get("vote_count").and_then(|v| v.as_i64()),
        });
    }
    out
}

fn artwork_from_images(data: &Value) -> Vec<ArtworkRef> {
    let mut out = Vec::new();
    if let Some(path) = data.get("poster_path").and_then(|v| v.as_str()) {
        out.push(ArtworkRef {
            kind: ArtworkKind::Poster,
            path: path.to_string(),
        });
    }
    if let Some(path) = data.get("backdrop_path").and_then(|v| v.as_str()) {
        out.push(ArtworkRef {
            kind: ArtworkKind::Backdrop,
            path: path.to_string(),
        });
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn maps_minimal_movie() {
        let data = serde_json::json!({
            "id": 550,
            "title": "Fight Club",
            "original_title": "Fight Club",
            "release_date": "1999-10-15",
            "overview": "plot",
            "runtime": 139,
            "genres": [{"name": "Drama"}],
            "vote_average": 8.4,
            "vote_count": 100,
            "poster_path": "/p.jpg",
            "belongs_to_collection": {"id": 1, "name": "FC"},
            "external_ids": {"imdb_id": "tt0137523"},
            "credits": {"cast": [{"name": "Brad Pitt", "character": "Tyler", "order": 0}]}
        });
        let meta = map_movie_detail(&data).unwrap();
        assert_eq!(meta.ids.tmdb, Some(550));
        assert_eq!(meta.title, "Fight Club");
        assert_eq!(meta.year, Some(1999));
        assert_eq!(
            meta.collection.as_ref().unwrap().name.as_deref(),
            Some("FC")
        );
        assert_eq!(meta.cast[0].name, "Brad Pitt");
    }

    #[test]
    fn maps_episodes_kind_sparse() {
        let data = serde_json::json!({
            "season_number": 1,
            "episodes": [{
                "id": 42,
                "name": "Pilot",
                "overview": "ep",
                "air_date": "2011-04-17",
                "season_number": 1,
                "episode_number": 1,
                "still_path": "/still.jpg",
                "vote_average": 7.5,
                "vote_count": 3
            }]
        });
        let eps = map_episodes_from_season(99, &data).unwrap();
        assert_eq!(eps.len(), 1);
        assert_eq!(eps[0].ids.tmdb, Some(42));
        assert_eq!(eps[0].ids.tmdb_show, Some(99));
        assert!(eps[0].genres.is_empty());
        assert!(eps[0].cast.is_empty());
        assert_eq!(eps[0].air_date.as_deref(), Some("2011-04-17"));
        assert_eq!(eps[0].year, Some(2011));
    }
}
