//! Entity-keyed canonical projection store (ADR-0029 §1).

use rusqlite::{OptionalExtension, Transaction, params};
use serde_json::Value;

use crate::item_links;
use crate::match_score::SearchHit;
use crate::model::{
    ArtworkKind, ArtworkRef, CanonicalMetadata, CastMember, CollectionRef, MetadataKind,
    ProviderIds, Rating,
};
use crate::negative_cache::now_rfc3339;
use crate::raw_payload;
use crate::resolve::ResolveError;
use crate::tmdb::{RawProviderPayload, map_episodes_from_season, map_movie_detail, map_tv_detail};

/// Upsert one kind-sparse canonical row from [`CanonicalMetadata`].
pub fn upsert_canonical(
    tx: &Transaction<'_>,
    provider: &str,
    meta: &CanonicalMetadata,
) -> Result<(), String> {
    let (entity_kind, provider_id) = entity_key(meta)?;
    let (genres_json, cast_json) = match meta.kind {
        // ADR-0029 §1.2: never persist inherited show blobs on episode rows.
        MetadataKind::Episode => (None, None),
        _ => (
            Some(serde_json::to_string(&meta.genres).map_err(|e| e.to_string())?),
            Some(serde_json::to_string(&meta.cast).map_err(|e| e.to_string())?),
        ),
    };
    let ratings_json = if meta.ratings.is_empty() {
        None
    } else {
        Some(serde_json::to_string(&meta.ratings).map_err(|e| e.to_string())?)
    };
    let artwork_json = if meta.artwork.is_empty() {
        None
    } else {
        Some(serde_json::to_string(&meta.artwork).map_err(|e| e.to_string())?)
    };
    let ids_json = serde_json::to_string(&meta.ids).map_err(|e| e.to_string())?;
    let (collection_id, collection_name) = match meta.kind {
        MetadataKind::Movie => meta
            .collection
            .as_ref()
            .map(|c| (c.id, c.name.clone()))
            .unwrap_or((None, None)),
        _ => (None, None),
    };
    let (air_date, year) = match meta.kind {
        MetadataKind::Episode => (meta.air_date.clone(), meta.year),
        _ => (None, meta.year),
    };
    let (season, episode) = match meta.kind {
        MetadataKind::Episode => (meta.season, meta.episode),
        _ => (None, None),
    };
    let tmdb_show = match meta.kind {
        MetadataKind::Episode | MetadataKind::Show => meta.ids.tmdb_show,
        MetadataKind::Movie => None,
    };

    tx.execute(
        "INSERT INTO metadata_canonical (
            provider, entity_kind, provider_id, title, original_title, year, air_date,
            plot, season, episode, runtime_minutes, genres_json, cast_json, ratings_json,
            artwork_json, ids_json, collection_id, collection_name, tmdb_show, projected_at
         ) VALUES (
            ?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10, ?11, ?12, ?13, ?14, ?15, ?16, ?17, ?18, ?19, ?20
         )
         ON CONFLICT(provider, entity_kind, provider_id) DO UPDATE SET
            title = excluded.title,
            original_title = excluded.original_title,
            year = excluded.year,
            air_date = excluded.air_date,
            plot = excluded.plot,
            season = excluded.season,
            episode = excluded.episode,
            runtime_minutes = excluded.runtime_minutes,
            genres_json = excluded.genres_json,
            cast_json = excluded.cast_json,
            ratings_json = excluded.ratings_json,
            artwork_json = excluded.artwork_json,
            ids_json = excluded.ids_json,
            collection_id = excluded.collection_id,
            collection_name = excluded.collection_name,
            tmdb_show = excluded.tmdb_show,
            projected_at = excluded.projected_at",
        params![
            provider,
            entity_kind,
            provider_id,
            meta.title,
            meta.original_title,
            year,
            air_date,
            meta.plot,
            season,
            episode,
            meta.runtime_minutes,
            genres_json,
            cast_json,
            ratings_json,
            artwork_json,
            ids_json,
            collection_id,
            collection_name,
            tmdb_show,
            now_rfc3339(),
        ],
    )
    .map_err(|e| format!("upsert metadata_canonical: {e}"))?;
    Ok(())
}

fn entity_key(meta: &CanonicalMetadata) -> Result<(&'static str, String), String> {
    let id = meta
        .ids
        .tmdb
        .ok_or_else(|| "canonical upsert requires tmdb id".to_string())?;
    let kind = match meta.kind {
        MetadataKind::Movie => "movie",
        MetadataKind::Show => "tv",
        MetadataKind::Episode => "episode",
    };
    Ok((kind, id.to_string()))
}

/// Sparse fast-tier projection from a search hit (ADR-0026 §8.1): title,
/// original title, year, overview, vote rating, poster/backdrop path refs and
/// tmdb ids only. Cast/genres stay empty and no cert / collection / runtime /
/// episode fields are set — the detail fetch fills those on the path to
/// `ready`. Movies use `title`/`release_date`; TV shows use `name`/
/// `first_air_date` (the same split `score_search` applies).
pub fn canonical_from_search_hit(hit: &SearchHit, kind: MetadataKind) -> CanonicalMetadata {
    let (title, original_title, air_date) = match kind {
        MetadataKind::Movie => (
            hit.title.clone().unwrap_or_default(),
            hit.original_title.clone(),
            hit.release_date.clone(),
        ),
        MetadataKind::Show | MetadataKind::Episode => (
            hit.name.clone().unwrap_or_default(),
            hit.original_name.clone(),
            hit.first_air_date.clone(),
        ),
    };
    let year = air_date
        .as_deref()
        .and_then(|d| d.get(..4))
        .and_then(|y| y.parse().ok());
    let mut artwork = Vec::new();
    if let Some(path) = &hit.poster_path {
        artwork.push(ArtworkRef {
            kind: ArtworkKind::Poster,
            path: path.clone(),
        });
    }
    if let Some(path) = &hit.backdrop_path {
        artwork.push(ArtworkRef {
            kind: ArtworkKind::Backdrop,
            path: path.clone(),
        });
    }
    let ratings = match hit.vote_average {
        Some(value) => vec![Rating {
            source: "tmdb".into(),
            value,
            votes: hit.vote_count,
        }],
        None => Vec::new(),
    };
    CanonicalMetadata {
        kind,
        title,
        original_title,
        year,
        air_date: None,
        plot: hit.overview.clone(),
        genres: Vec::new(),
        runtime_minutes: None,
        cast: Vec::new(),
        ratings,
        ids: ProviderIds {
            tmdb: Some(hit.id),
            tmdb_show: (kind == MetadataKind::Show).then_some(hit.id),
            imdb: None,
            tvdb: None,
        },
        artwork,
        collection: None,
        season: None,
        episode: None,
    }
}

/// Persist raw payload + canonical projection in one transaction.
pub fn persist_mapped_hit(
    conn: &rusqlite::Connection,
    provider: &str,
    raw: &RawProviderPayload,
    meta: &CanonicalMetadata,
) -> Result<(), String> {
    raw_payload::persist_hit_with_canonical(conn, provider, raw, |tx| {
        upsert_canonical(tx, provider, meta)
    })
}

/// Project episode rows from a season payload; season-scoped delete of absent
/// episodes **and** their join bindings (ADR-0029 §1.6).
pub fn persist_season_projection(
    conn: &rusqlite::Connection,
    provider: &str,
    show_id: i64,
    raw: &RawProviderPayload,
) -> Result<Vec<CanonicalMetadata>, String> {
    let data: Value =
        serde_json::from_str(&raw.payload).map_err(|e| format!("season JSON: {e}"))?;
    let episodes = map_episodes_from_season(show_id, &data).map_err(|e| e.to_string())?;
    let season_number = data
        .get("season_number")
        .and_then(|v| v.as_i64())
        .map(|n| n as i32)
        .ok_or_else(|| "season payload missing season_number".to_string())?;

    let present: std::collections::HashSet<String> = episodes
        .iter()
        .filter_map(|e| e.ids.tmdb.map(|id| id.to_string()))
        .collect();

    let tx = conn
        .unchecked_transaction()
        .map_err(|e| format!("begin season project tx: {e}"))?;

    let stale_keys = stale_episode_item_keys(&tx, provider, show_id, season_number, &present)?;
    item_links::delete_links_for_item_keys(&tx, &stale_keys)?;
    delete_absent_episode_rows(&tx, provider, show_id, season_number, &present)?;

    for ep in &episodes {
        upsert_canonical(&tx, provider, ep)?;
    }
    raw_payload::upsert_raw_payload(&tx, provider, raw, &now_rfc3339())?;
    tx.commit()
        .map_err(|e| format!("commit season project: {e}"))?;
    Ok(episodes)
}

fn stale_episode_item_keys(
    tx: &Transaction<'_>,
    provider: &str,
    show_id: i64,
    season: i32,
    present: &std::collections::HashSet<String>,
) -> Result<Vec<String>, String> {
    let mut stmt = tx
        .prepare(
            "SELECT provider_id FROM metadata_canonical
             WHERE provider = ?1 AND entity_kind = 'episode'
               AND tmdb_show = ?2 AND season = ?3",
        )
        .map_err(|e| format!("prepare stale episodes: {e}"))?;
    let rows = stmt
        .query_map(params![provider, show_id, season], |r| {
            r.get::<_, String>(0)
        })
        .map_err(|e| format!("query stale episodes: {e}"))?;
    let mut keys = Vec::new();
    for row in rows {
        let id = row.map_err(|e| format!("stale episode row: {e}"))?;
        if !present.contains(&id) {
            keys.push(format!("tmdb:episode:{id}"));
        }
    }
    Ok(keys)
}

fn delete_absent_episode_rows(
    tx: &Transaction<'_>,
    provider: &str,
    show_id: i64,
    season: i32,
    present: &std::collections::HashSet<String>,
) -> Result<(), String> {
    let mut stmt = tx
        .prepare(
            "SELECT provider_id FROM metadata_canonical
             WHERE provider = ?1 AND entity_kind = 'episode'
               AND tmdb_show = ?2 AND season = ?3",
        )
        .map_err(|e| format!("prepare delete-absent: {e}"))?;
    let rows = stmt
        .query_map(params![provider, show_id, season], |r| {
            r.get::<_, String>(0)
        })
        .map_err(|e| format!("query delete-absent: {e}"))?;
    let mut to_delete = Vec::new();
    for row in rows {
        let id = row.map_err(|e| format!("delete-absent row: {e}"))?;
        if !present.contains(&id) {
            to_delete.push(id);
        }
    }
    for id in to_delete {
        tx.execute(
            "DELETE FROM metadata_canonical
             WHERE provider = ?1 AND entity_kind = 'episode' AND provider_id = ?2",
            params![provider, id],
        )
        .map_err(|e| format!("delete episode canonical {id}: {e}"))?;
    }
    Ok(())
}

pub fn get_canonical(
    conn: &rusqlite::Connection,
    provider: &str,
    entity_kind: &str,
    provider_id: &str,
) -> Result<Option<CanonicalMetadata>, String> {
    conn.query_row(
        "SELECT entity_kind, title, original_title, year, air_date, plot, season, episode,
                runtime_minutes, genres_json, cast_json, ratings_json, artwork_json,
                ids_json, collection_id, collection_name, tmdb_show
         FROM metadata_canonical
         WHERE provider = ?1 AND entity_kind = ?2 AND provider_id = ?3",
        params![provider, entity_kind, provider_id],
        |r| {
            let kind_s: String = r.get(0)?;
            let kind = match kind_s.as_str() {
                "movie" => MetadataKind::Movie,
                "tv" => MetadataKind::Show,
                "episode" => MetadataKind::Episode,
                other => {
                    return Err(rusqlite::Error::FromSqlConversionFailure(
                        0,
                        rusqlite::types::Type::Text,
                        format!("bad entity_kind {other}").into(),
                    ));
                }
            };
            let genres: Vec<String> = r
                .get::<_, Option<String>>(9)?
                .as_deref()
                .map(serde_json::from_str)
                .transpose()
                .map_err(|e| {
                    rusqlite::Error::FromSqlConversionFailure(
                        9,
                        rusqlite::types::Type::Text,
                        Box::new(e),
                    )
                })?
                .unwrap_or_default();
            let cast: Vec<CastMember> = r
                .get::<_, Option<String>>(10)?
                .as_deref()
                .map(serde_json::from_str)
                .transpose()
                .map_err(|e| {
                    rusqlite::Error::FromSqlConversionFailure(
                        10,
                        rusqlite::types::Type::Text,
                        Box::new(e),
                    )
                })?
                .unwrap_or_default();
            let ratings: Vec<Rating> = r
                .get::<_, Option<String>>(11)?
                .as_deref()
                .map(serde_json::from_str)
                .transpose()
                .map_err(|e| {
                    rusqlite::Error::FromSqlConversionFailure(
                        11,
                        rusqlite::types::Type::Text,
                        Box::new(e),
                    )
                })?
                .unwrap_or_default();
            let artwork: Vec<ArtworkRef> = r
                .get::<_, Option<String>>(12)?
                .as_deref()
                .map(serde_json::from_str)
                .transpose()
                .map_err(|e| {
                    rusqlite::Error::FromSqlConversionFailure(
                        12,
                        rusqlite::types::Type::Text,
                        Box::new(e),
                    )
                })?
                .unwrap_or_default();
            let ids: ProviderIds = serde_json::from_str(&r.get::<_, String>(13)?).map_err(|e| {
                rusqlite::Error::FromSqlConversionFailure(
                    13,
                    rusqlite::types::Type::Text,
                    Box::new(e),
                )
            })?;
            let collection_id: Option<i64> = r.get(14)?;
            let collection_name: Option<String> = r.get(15)?;
            let collection = match (collection_id, collection_name) {
                (None, None) => None,
                (id, name) => Some(CollectionRef { id, name }),
            };
            Ok(CanonicalMetadata {
                kind,
                title: r.get(1)?,
                original_title: r.get(2)?,
                year: r.get(3)?,
                air_date: r.get(4)?,
                plot: r.get(5)?,
                genres,
                runtime_minutes: r.get(8)?,
                cast,
                ratings,
                ids,
                artwork,
                collection,
                season: r.get(6)?,
                episode: r.get(7)?,
            })
        },
    )
    .optional()
    .map_err(|e| format!("get metadata_canonical: {e}"))
}

/// Re-project movie/tv from a stored raw payload (no network).
pub fn reproject_from_payload(
    conn: &rusqlite::Connection,
    provider: &str,
    entity_kind: &str,
    provider_id: &str,
) -> Result<CanonicalMetadata, String> {
    let body = raw_payload::get_raw_payload(conn, provider, entity_kind, provider_id)?
        .ok_or_else(|| format!("no payload for {provider}/{entity_kind}/{provider_id}"))?;
    let data: Value = serde_json::from_str(&body).map_err(|e| format!("payload JSON: {e}"))?;
    let meta = match entity_kind {
        "movie" => map_movie_detail(&data).map_err(|e: ResolveError| e.to_string())?,
        "tv" => map_tv_detail(&data).map_err(|e: ResolveError| e.to_string())?,
        other => return Err(format!("reproject unsupported entity_kind {other}")),
    };
    let tx = conn
        .unchecked_transaction()
        .map_err(|e| format!("begin reproject: {e}"))?;
    upsert_canonical(&tx, provider, &meta)?;
    tx.commit().map_err(|e| format!("commit reproject: {e}"))?;
    Ok(meta)
}

/// Merge two [`CanonicalMetadata`] records, preferring non-empty/non-default
/// fields from `nfo` (left); fall back to `tmdb` (right) for empty fields.
///
/// Kind: nfo wins. Ids: per-field nfo wins when set. Arrays (cast, genres,
/// ratings, artwork): nfo wins when non-empty.
pub fn merge_prefer_left(nfo: &CanonicalMetadata, tmdb: &CanonicalMetadata) -> CanonicalMetadata {
    CanonicalMetadata {
        kind: nfo.kind,
        title: if nfo.title.is_empty() {
            tmdb.title.clone()
        } else {
            nfo.title.clone()
        },
        original_title: pick_str(&nfo.original_title, &tmdb.original_title),
        year: nfo.year.or(tmdb.year),
        air_date: pick_str(&nfo.air_date, &tmdb.air_date),
        plot: pick_str(&nfo.plot, &tmdb.plot),
        genres: if nfo.genres.is_empty() {
            tmdb.genres.clone()
        } else {
            nfo.genres.clone()
        },
        runtime_minutes: nfo.runtime_minutes.or(tmdb.runtime_minutes),
        cast: if nfo.cast.is_empty() {
            tmdb.cast.clone()
        } else {
            nfo.cast.clone()
        },
        ratings: if nfo.ratings.is_empty() {
            tmdb.ratings.clone()
        } else {
            nfo.ratings.clone()
        },
        ids: ProviderIds {
            tmdb: nfo.ids.tmdb.or(tmdb.ids.tmdb),
            tmdb_show: nfo.ids.tmdb_show.or(tmdb.ids.tmdb_show),
            imdb: nfo.ids.imdb.clone().or_else(|| tmdb.ids.imdb.clone()),
            tvdb: nfo.ids.tvdb.or(tmdb.ids.tvdb),
        },
        artwork: if nfo.artwork.is_empty() {
            tmdb.artwork.clone()
        } else {
            nfo.artwork.clone()
        },
        collection: nfo.collection.clone().or_else(|| tmdb.collection.clone()),
        season: nfo.season.or(tmdb.season),
        episode: nfo.episode.or(tmdb.episode),
    }
}

/// Pick `a` when it is `Some` and non-empty; otherwise fall back to `b`.
fn pick_str(a: &Option<String>, b: &Option<String>) -> Option<String> {
    match a {
        Some(s) if !s.is_empty() => Some(s.clone()),
        _ => b.clone(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::item_links::{self, upsert_link};
    use crate::model::item_key_for_metadata;
    use crate::negative_cache::PROVIDER_TMDB;
    use nightjar_db::migrate;
    use rusqlite::Connection;

    fn mem() -> Connection {
        let c = Connection::open_in_memory().unwrap();
        migrate(&c).unwrap();
        c
    }

    #[test]
    fn movie_upsert_kind_sparse_and_collection() {
        let c = mem();
        let meta = map_movie_detail(&serde_json::json!({
            "id": 550,
            "title": "Fight Club",
            "release_date": "1999-10-15",
            "overview": "plot",
            "genres": [{"name": "Drama"}],
            "vote_average": 8.4,
            "vote_count": 10,
            "belongs_to_collection": {"id": 1, "name": "FC"},
            "credits": {"cast": [{"name": "Brad Pitt", "character": "Tyler", "order": 0}]}
        }))
        .unwrap();
        let raw = RawProviderPayload {
            entity_kind: "movie".into(),
            provider_id: "550".into(),
            payload: "{}".into(),
        };
        persist_mapped_hit(&c, PROVIDER_TMDB, &raw, &meta).unwrap();
        let got = get_canonical(&c, PROVIDER_TMDB, "movie", "550")
            .unwrap()
            .unwrap();
        assert_eq!(got.title, "Fight Club");
        assert_eq!(got.collection.as_ref().unwrap().id, Some(1));
        assert!(!got.cast.is_empty());
        assert_eq!(got.air_date, None);
    }

    #[test]
    fn season_project_deletes_absent_episode_and_binding() {
        let c = mem();
        c.execute_batch(
            "INSERT INTO libraries (name, path, kind) VALUES ('S', '/S', 'shows');
             INSERT INTO media_items (library_id, path, mtime_ms, size_bytes, title, kind, season, episode)
             VALUES (1, 'e1.mkv', 1, 1, 'Show', 'episode', 1, 1),
                    (1, 'e2.mkv', 1, 1, 'Show', 'episode', 1, 2);",
        )
        .unwrap();

        let season1 = serde_json::json!({
            "season_number": 1,
            "episodes": [
                {
                    "id": 100,
                    "name": "Pilot",
                    "overview": "a",
                    "air_date": "2011-04-17",
                    "season_number": 1,
                    "episode_number": 1,
                    "still_path": "/s.jpg",
                    "vote_average": 8.0,
                    "vote_count": 5
                },
                {
                    "id": 101,
                    "name": "Two",
                    "overview": "b",
                    "air_date": "2011-04-24",
                    "season_number": 1,
                    "episode_number": 2
                }
            ]
        });
        let raw = RawProviderPayload {
            entity_kind: "season".into(),
            provider_id: "99:1".into(),
            payload: season1.to_string(),
        };
        let eps = persist_season_projection(&c, PROVIDER_TMDB, 99, &raw).unwrap();
        assert_eq!(eps.len(), 2);

        let tx = c.unchecked_transaction().unwrap();
        upsert_link(&tx, 1, "tmdb:episode:100", false).unwrap();
        upsert_link(&tx, 2, "tmdb:episode:101", false).unwrap();
        tx.commit().unwrap();

        // Re-project with episode 101 gone (renumber).
        let season_shrunk = serde_json::json!({
            "season_number": 1,
            "episodes": [{
                "id": 100,
                "name": "Pilot",
                "air_date": "2011-04-17",
                "season_number": 1,
                "episode_number": 1
            }]
        });
        let raw2 = RawProviderPayload {
            entity_kind: "season".into(),
            provider_id: "99:1".into(),
            payload: season_shrunk.to_string(),
        };
        persist_season_projection(&c, PROVIDER_TMDB, 99, &raw2).unwrap();

        assert!(
            get_canonical(&c, PROVIDER_TMDB, "episode", "101")
                .unwrap()
                .is_none()
        );
        assert!(
            get_canonical(&c, PROVIDER_TMDB, "episode", "100")
                .unwrap()
                .is_some()
        );
        assert!(item_links::link_keys_for_item(&c, 2).unwrap().is_empty());
        assert_eq!(
            item_links::link_keys_for_item(&c, 1).unwrap(),
            vec!["tmdb:episode:100".to_string()]
        );

        let ep = get_canonical(&c, PROVIDER_TMDB, "episode", "100")
            .unwrap()
            .unwrap();
        assert!(ep.genres.is_empty());
        assert!(ep.cast.is_empty());
        assert_eq!(ep.air_date.as_deref(), Some("2011-04-17"));
        assert_eq!(
            item_key_for_metadata(&ep).as_deref(),
            Some("tmdb:episode:100")
        );
    }

    fn movie_hit() -> SearchHit {
        SearchHit {
            id: 550,
            title: Some("Fight Club".into()),
            name: None,
            original_title: Some("Fight Club".into()),
            original_name: None,
            release_date: Some("1999-10-15".into()),
            first_air_date: None,
            poster_path: Some("/p.jpg".into()),
            backdrop_path: Some("/b.jpg".into()),
            overview: Some("A man and soap.".into()),
            vote_average: Some(8.4),
            vote_count: Some(100),
        }
    }

    #[test]
    fn sparse_movie_from_search_hit() {
        let meta = canonical_from_search_hit(&movie_hit(), MetadataKind::Movie);
        assert_eq!(meta.kind, MetadataKind::Movie);
        assert_eq!(meta.title, "Fight Club");
        assert_eq!(meta.original_title.as_deref(), Some("Fight Club"));
        assert_eq!(meta.year, Some(1999));
        assert_eq!(meta.air_date, None);
        assert_eq!(meta.plot.as_deref(), Some("A man and soap."));
        assert!(meta.genres.is_empty());
        assert!(meta.cast.is_empty());
        assert_eq!(meta.runtime_minutes, None);
        assert_eq!(meta.ids.tmdb, Some(550));
        assert_eq!(meta.ids.tmdb_show, None);
        assert_eq!(meta.ids.imdb, None);
        assert_eq!(meta.ratings.len(), 1);
        assert_eq!(meta.ratings[0].source, "tmdb");
        assert_eq!(meta.ratings[0].value, 8.4);
        assert_eq!(meta.ratings[0].votes, Some(100));
        assert_eq!(meta.artwork.len(), 2);
        assert_eq!(meta.artwork[0].kind, ArtworkKind::Poster);
        assert_eq!(meta.artwork[0].path, "/p.jpg");
        assert_eq!(meta.artwork[1].kind, ArtworkKind::Backdrop);
        assert_eq!(meta.artwork[1].path, "/b.jpg");
        assert_eq!(meta.collection, None);
        assert_eq!(meta.season, None);
        assert_eq!(meta.episode, None);
    }

    #[test]
    fn sparse_show_from_search_hit() {
        let mut hit = movie_hit();
        hit.id = 37854;
        hit.title = None;
        hit.name = Some("One Piece".into());
        hit.original_title = None;
        hit.original_name = Some("ワンピース".into());
        hit.release_date = None;
        hit.first_air_date = Some("1999-10-20".into());
        let meta = canonical_from_search_hit(&hit, MetadataKind::Show);
        assert_eq!(meta.kind, MetadataKind::Show);
        assert_eq!(meta.title, "One Piece");
        assert_eq!(meta.original_title.as_deref(), Some("ワンピース"));
        assert_eq!(meta.year, Some(1999));
        assert_eq!(meta.ids.tmdb, Some(37854));
        assert_eq!(meta.ids.tmdb_show, Some(37854));
    }

    #[test]
    fn sparse_drops_absent_rating_and_art() {
        let mut hit = movie_hit();
        hit.overview = None;
        hit.vote_average = None;
        hit.vote_count = None;
        hit.poster_path = None;
        hit.backdrop_path = None;
        let meta = canonical_from_search_hit(&hit, MetadataKind::Movie);
        assert_eq!(meta.plot, None);
        assert!(meta.ratings.is_empty());
        assert!(meta.artwork.is_empty());
    }

    // --- merge_prefer_left tests ---

    fn empty_meta(kind: MetadataKind) -> CanonicalMetadata {
        CanonicalMetadata {
            kind,
            title: String::new(),
            original_title: None,
            year: None,
            air_date: None,
            plot: None,
            genres: vec![],
            runtime_minutes: None,
            cast: vec![],
            ratings: vec![],
            ids: ProviderIds::default(),
            artwork: vec![],
            collection: None,
            season: None,
            episode: None,
        }
    }

    #[test]
    fn merge_prefer_left_nfo_wins_non_empty() {
        let mut nfo = empty_meta(MetadataKind::Movie);
        nfo.title = "NFO Title".into();
        nfo.original_title = Some("NFO Original".into());
        nfo.year = Some(2020);
        nfo.plot = Some("NFO plot".into());
        nfo.genres = vec!["Action".into()];
        nfo.runtime_minutes = Some(120);
        nfo.cast = vec![CastMember { name: "Actor".into(), role: Some("Role".into()), order: Some(0) }];
        nfo.ratings = vec![Rating { source: "imdb".into(), value: 8.0, votes: Some(100) }];
        nfo.ids = ProviderIds { tmdb: Some(1), tmdb_show: None, imdb: Some("tt123".into()), tvdb: None };
        nfo.artwork = vec![ArtworkRef { kind: ArtworkKind::Poster, path: "/p.jpg".into() }];
        nfo.collection = Some(CollectionRef { id: Some(10), name: Some("Col".into()) });

        let mut tmdb = empty_meta(MetadataKind::Movie);
        tmdb.title = "TMDB Title".into();
        tmdb.original_title = Some("TMDB Original".into());
        tmdb.year = Some(2021);
        tmdb.plot = Some("TMDB plot".into());
        tmdb.genres = vec!["Drama".into()];
        tmdb.runtime_minutes = Some(130);
        tmdb.cast = vec![CastMember { name: "TMDB Actor".into(), role: Some("R".into()), order: Some(1) }];
        tmdb.ratings = vec![Rating { source: "tmdb".into(), value: 7.5, votes: Some(200) }];
        tmdb.ids = ProviderIds { tmdb: Some(2), tvdb: Some(100), ..Default::default() };
        tmdb.artwork = vec![ArtworkRef { kind: ArtworkKind::Backdrop, path: "/b.jpg".into() }];
        tmdb.collection = Some(CollectionRef { id: Some(20), name: Some("Other".into()) });
        tmdb.season = Some(1);
        tmdb.episode = Some(2);

        let m = merge_prefer_left(&nfo, &tmdb);
        assert_eq!(m.title, "NFO Title");
        assert_eq!(m.original_title.as_deref(), Some("NFO Original"));
        assert_eq!(m.year, Some(2020));
        assert_eq!(m.plot.as_deref(), Some("NFO plot"));
        assert_eq!(m.genres, vec!["Action"]);
        assert_eq!(m.runtime_minutes, Some(120));
        assert_eq!(m.cast[0].name, "Actor");
        assert_eq!(m.ratings[0].source, "imdb");
        assert_eq!(m.ids.tmdb, Some(1));
        assert_eq!(m.ids.imdb.as_deref(), Some("tt123"));
        assert_eq!(m.ids.tvdb, Some(100)); // nfo None → tmdb
        assert_eq!(m.artwork[0].kind, ArtworkKind::Poster);
        assert_eq!(m.collection.as_ref().unwrap().id, Some(10));
        assert_eq!(m.season, Some(1)); // nfo None → tmdb
        assert_eq!(m.episode, Some(2));
    }

    #[test]
    fn merge_prefer_left_nfo_empty_falls_back_to_tmdb() {
        let nfo = empty_meta(MetadataKind::Movie);
        let mut tmdb = empty_meta(MetadataKind::Movie);
        tmdb.title = "TMDB Title".into();
        tmdb.original_title = Some("TMDB Orig".into());
        tmdb.year = Some(2021);
        tmdb.air_date = Some("2021-01-01".into());
        tmdb.plot = Some("TMDB plot".into());
        tmdb.genres = vec!["Drama".into()];
        tmdb.runtime_minutes = Some(130);
        tmdb.cast = vec![CastMember { name: "A".into(), role: None, order: None }];
        tmdb.ratings = vec![Rating { source: "tmdb".into(), value: 7.0, votes: None }];
        tmdb.ids = ProviderIds { tmdb: Some(99), imdb: Some("tt999".into()), tvdb: Some(88), ..Default::default() };
        tmdb.artwork = vec![ArtworkRef { kind: ArtworkKind::Poster, path: "/t.jpg".into() }];
        tmdb.collection = Some(CollectionRef { id: Some(1), name: Some("C".into()) });
        tmdb.season = Some(1);
        tmdb.episode = Some(3);

        let m = merge_prefer_left(&nfo, &tmdb);
        assert_eq!(m.title, "TMDB Title");
        assert_eq!(m.original_title.as_deref(), Some("TMDB Orig"));
        assert_eq!(m.year, Some(2021));
        assert_eq!(m.air_date.as_deref(), Some("2021-01-01"));
        assert_eq!(m.plot.as_deref(), Some("TMDB plot"));
        assert_eq!(m.genres, vec!["Drama"]);
        assert_eq!(m.runtime_minutes, Some(130));
        assert_eq!(m.cast.len(), 1);
        assert_eq!(m.ratings.len(), 1);
        assert_eq!(m.ids.tmdb, Some(99));
        assert_eq!(m.ids.tvdb, Some(88));
        assert_eq!(m.artwork.len(), 1);
        assert_eq!(m.collection.as_ref().unwrap().id, Some(1));
        assert_eq!(m.season, Some(1));
        assert_eq!(m.episode, Some(3));
    }

    #[test]
    fn merge_prefer_left_ids_per_field() {
        // nfo has tvdb/imdb, tmdb has tmdb — each side supplies what it has;
        // nfo tmdb overrides tmdb tmdb when set.
        let mut nfo = empty_meta(MetadataKind::Movie);
        nfo.title = "NFO".into();
        nfo.ids = ProviderIds { tmdb: Some(1), imdb: Some("tt000".into()), tvdb: Some(42), ..Default::default() };
        let mut tmdb = empty_meta(MetadataKind::Movie);
        tmdb.ids = ProviderIds { tmdb: Some(2), tvdb: Some(99), ..Default::default() };
        let m = merge_prefer_left(&nfo, &tmdb);
        assert_eq!(m.ids.tmdb, Some(1)); // nfo wins
        assert_eq!(m.ids.imdb.as_deref(), Some("tt000")); // nfo only
        assert_eq!(m.ids.tvdb, Some(42)); // nfo wins

        // nfo has no tmdb — falls back
        let mut nfo2 = empty_meta(MetadataKind::Movie);
        nfo2.title = "NFO2".into();
        nfo2.ids.tvdb = Some(42);
        let mut tmdb2 = empty_meta(MetadataKind::Movie);
        tmdb2.ids.tmdb = Some(99);
        let m2 = merge_prefer_left(&nfo2, &tmdb2);
        assert_eq!(m2.ids.tmdb, Some(99)); // from tmdb
        assert_eq!(m2.ids.tvdb, Some(42)); // from nfo
    }

    #[test]
    fn merge_prefer_left_empty_nfo_string_falls_back() {
        let mut nfo = empty_meta(MetadataKind::Movie);
        nfo.plot = Some("NFO plot".into()); // set, wins
        let mut tmdb = empty_meta(MetadataKind::Movie);
        tmdb.title = "TMDB Title".into(); // nfo title empty → tmdb
        let m = merge_prefer_left(&nfo, &tmdb);
        assert_eq!(m.title, "TMDB Title");
        assert_eq!(m.plot.as_deref(), Some("NFO plot"));
    }
}
