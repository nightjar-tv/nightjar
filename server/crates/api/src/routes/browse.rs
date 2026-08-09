//! Library browse units and series detail (ADR-0039 item 4, ADR-0035 item 8).

use crate::error::{ApiError, ApiResult, blocking};
use crate::state::AppState;
use axum::{
    Json,
    extract::{Path, Query, State},
};
use nightjar_metadata::{
    BrowseUnit, LibraryUnits, SeriesDetail, SeriesEpisode, SeriesSeason, UnitCounts, get_series,
    list_library_units,
};
use serde::{Deserialize, Serialize};

#[derive(Serialize)]
#[serde(rename_all = "camelCase")]
pub struct LibraryUnitDto {
    pub series_key: String,
    pub kind: &'static str,
    pub identity: &'static str,
    pub title: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub year: Option<i32>,
    pub item_count: i64,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub item_id: Option<i64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub poster_url: Option<String>,
}

#[derive(Serialize)]
#[serde(rename_all = "camelCase")]
pub struct UnitCountsDto {
    pub units: i64,
    pub bound: i64,
    pub entity_only: i64,
    pub unidentified: i64,
    pub items: i64,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub show_entities_without_binding: Option<i64>,
}

#[derive(Serialize)]
#[serde(rename_all = "camelCase")]
pub struct LibraryUnitsDto {
    pub library_id: i64,
    pub library_kind: String,
    pub unit_kind: &'static str,
    pub units: Vec<LibraryUnitDto>,
    pub counts: UnitCountsDto,
}

#[derive(Serialize)]
#[serde(rename_all = "camelCase")]
pub struct SeriesEpisodeDto {
    pub item_id: i64,
    pub item_key: String,
    pub title: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub season: Option<i32>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub episode: Option<i32>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub file_season: Option<i32>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub file_episode: Option<i32>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub air_date: Option<String>,
    pub canonical_numbering: bool,
    pub path: String,
    pub probe_status: String,
    pub metadata_status: String,
}

#[derive(Serialize)]
#[serde(rename_all = "camelCase")]
pub struct SeriesSeasonDto {
    pub season: i32,
    pub episodes: Vec<SeriesEpisodeDto>,
}

#[derive(Serialize)]
#[serde(rename_all = "camelCase")]
pub struct SeriesDetailDto {
    pub series_key: String,
    pub kind: &'static str,
    pub identity: &'static str,
    pub title: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub year: Option<i32>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub plot: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub poster_url: Option<String>,
    pub item_count: i64,
    pub seasons: Vec<SeriesSeasonDto>,
    pub unnumbered: Vec<SeriesEpisodeDto>,
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct SeriesQuery {
    pub series_key: String,
}

pub async fn list_units(
    State(state): State<AppState>,
    Path(library_id): Path<i64>,
) -> ApiResult<Json<LibraryUnitsDto>> {
    // A whole-library read, the same shape as `list_items` and behind the same
    // store mutex.
    blocking(move || {
        let library = state
            .db
            .get_library(library_id)
            .map_err(ApiError::internal)?
            .ok_or_else(|| ApiError::not_found(format!("library {library_id} not found")))?;
        let listed = state
            .db
            .with_conn(|c| list_library_units(c, library_id))
            .map_err(ApiError::internal)?;
        Ok(Json(units_to_dto(library_id, library.kind, listed)))
    })
    .await
}

pub async fn get(
    State(state): State<AppState>,
    Query(query): Query<SeriesQuery>,
) -> ApiResult<Json<SeriesDetailDto>> {
    blocking(move || {
        if query.series_key.trim().is_empty() {
            return Err(ApiError::bad_request("seriesKey is required"));
        }
        let detail = state
            .db
            .with_conn(|c| get_series(c, &query.series_key))
            .map_err(ApiError::bad_request)?
            .ok_or_else(|| {
                ApiError::not_found(format!("no series under key {}", query.series_key))
            })?;
        Ok(Json(series_to_dto(detail)))
    })
    .await
}

fn units_to_dto(library_id: i64, library_kind: String, listed: LibraryUnits) -> LibraryUnitsDto {
    LibraryUnitsDto {
        library_id,
        library_kind,
        unit_kind: listed.unit_kind.as_str(),
        units: listed.units.into_iter().map(unit_to_dto).collect(),
        counts: counts_to_dto(listed.counts),
    }
}

fn unit_to_dto(unit: BrowseUnit) -> LibraryUnitDto {
    LibraryUnitDto {
        poster_url: unit.poster_key.as_deref().map(poster_url),
        series_key: unit.series_key,
        kind: unit.kind.as_str(),
        identity: unit.identity.as_str(),
        title: unit.title,
        year: unit.year,
        item_count: unit.item_count,
        item_id: unit.item_id,
    }
}

fn counts_to_dto(counts: UnitCounts) -> UnitCountsDto {
    UnitCountsDto {
        units: counts.units,
        bound: counts.bound,
        entity_only: counts.entity_only,
        unidentified: counts.unidentified,
        items: counts.items,
        show_entities_without_binding: counts.show_entities_without_binding,
    }
}

fn series_to_dto(detail: SeriesDetail) -> SeriesDetailDto {
    SeriesDetailDto {
        poster_url: detail.poster_key.as_deref().map(poster_url),
        series_key: detail.series_key,
        kind: detail.kind.as_str(),
        identity: detail.identity.as_str(),
        title: detail.title,
        year: detail.year,
        plot: detail.plot,
        item_count: detail.item_count,
        seasons: detail.seasons.into_iter().map(season_to_dto).collect(),
        unnumbered: detail.unnumbered.into_iter().map(episode_to_dto).collect(),
    }
}

fn season_to_dto(season: SeriesSeason) -> SeriesSeasonDto {
    SeriesSeasonDto {
        season: season.season,
        episodes: season.episodes.into_iter().map(episode_to_dto).collect(),
    }
}

fn episode_to_dto(episode: SeriesEpisode) -> SeriesEpisodeDto {
    SeriesEpisodeDto {
        canonical_numbering: episode.canonical_numbering(),
        item_id: episode.item_id,
        item_key: episode.item_key,
        title: episode.title,
        season: episode.season,
        episode: episode.episode,
        file_season: episode.file_season,
        file_episode: episode.file_episode,
        air_date: episode.air_date,
        path: episode.path,
        probe_status: episode.probe_status,
        metadata_status: episode.metadata_status,
    }
}

/// Artwork path for a provider key (ADR-0027). Only provider keys reach here;
/// a path key carries slashes and could not be a path segment anyway.
fn poster_url(key: &str) -> String {
    format!("/api/v0/artwork/{key}/poster")
}
