//! Manual metadata fix operations (ADR-0028).
//!
//! Four ops: search candidates, assign, clear, retry. Artwork invalidate is
//! a no-op until ADR-0027 pipeline exists.

use rusqlite::{Connection, OptionalExtension, params};

use crate::canonical;
use crate::clean::{clean_movie_title, clean_show_title, year_from_path};
use crate::item_links::{
    clear_all_links_for_media_item, effective_item_key, link_keys_for_item, path_item_key,
    set_manually_matched, upsert_link,
};
use crate::match_score::SearchKind;
use crate::migrator::{self, MigrateReport};
use crate::model::item_key_for_metadata;
use crate::negative_cache::{self, CacheKind, PROVIDER_TMDB, query_key};
use crate::queue::{self, MetadataStatus};
use crate::resolve::{MetadataSource, Resolver};
use crate::tmdb::TmdbClient;

/// Artwork side-effect hook (ADR-0028 §5). Product fills this when 0027 lands.
pub trait ArtworkInvalidate: Send + Sync {
    fn on_item_key_change(&self, old_keys: &[String], new_key: Option<&str>);
}

/// No-op until artwork cache exists.
#[derive(Debug, Default, Clone, Copy)]
pub struct NoopArtwork;

impl ArtworkInvalidate for NoopArtwork {
    fn on_item_key_change(&self, _old_keys: &[String], _new_key: Option<&str>) {}
}

#[derive(Debug, Clone)]
pub struct FixCandidate {
    pub provider: String,
    pub kind: String,
    pub id: i64,
    pub title: String,
    pub year: Option<i32>,
}

#[derive(Debug, Clone)]
pub struct AssignRequest {
    pub media_item_id: i64,
    /// `movie` or `tv` (series id; episodes bind via S/E).
    pub kind: String,
    pub tmdb_id: i64,
}

#[derive(Debug, Clone)]
pub struct FixItemView {
    pub id: i64,
    pub library_id: i64,
    pub path: String,
    pub title: String,
    pub kind: String,
    pub year: Option<i32>,
    pub season: Option<i32>,
    pub episode: Option<i32>,
}

fn load_item(conn: &Connection, id: i64) -> Result<FixItemView, String> {
    conn.query_row(
        "SELECT id, library_id, path, title, kind, year, season, episode
         FROM media_items WHERE id = ?1",
        params![id],
        |r| {
            Ok(FixItemView {
                id: r.get(0)?,
                library_id: r.get(1)?,
                path: r.get(2)?,
                title: r.get(3)?,
                kind: r.get(4)?,
                year: r.get(5)?,
                season: r.get(6)?,
                episode: r.get(7)?,
            })
        },
    )
    .optional()
    .map_err(|e| format!("load item: {e}"))?
    .ok_or_else(|| format!("item {id} not found"))
}

/// Search TMDB for assign candidates (floor does not apply).
pub fn search_candidates(
    client: &TmdbClient,
    item: &FixItemView,
    q: Option<&str>,
    year: Option<i32>,
) -> Result<Vec<FixCandidate>, String> {
    let title = q
        .map(str::trim)
        .filter(|s| !s.is_empty())
        .map(str::to_string)
        .unwrap_or_else(|| {
            if item.kind == "episode" {
                clean_show_title(&item.title).0
            } else {
                clean_movie_title(&item.title, year_from_path(&item.path).or(item.year)).0
            }
        });
    let yr = year.or_else(|| {
        if item.kind == "movie" {
            year_from_path(&item.path).or(item.year)
        } else {
            None
        }
    });
    let search_kind = if item.kind == "episode" {
        SearchKind::Tv
    } else {
        SearchKind::Movie
    };
    let hits = client
        .search(search_kind, &title, yr)
        .map_err(|e| e.to_string())?;
    Ok(hits
        .into_iter()
        .map(|h| {
            let (title, year) = match search_kind {
                SearchKind::Movie => (
                    h.title.or(h.original_title).unwrap_or_default(),
                    h.release_date
                        .as_deref()
                        .and_then(|d| d.get(..4)?.parse().ok()),
                ),
                SearchKind::Tv => (
                    h.name.or(h.original_name).unwrap_or_default(),
                    h.first_air_date
                        .as_deref()
                        .and_then(|d| d.get(..4)?.parse().ok()),
                ),
            };
            FixCandidate {
                provider: "tmdb".into(),
                kind: match search_kind {
                    SearchKind::Movie => "movie".into(),
                    SearchKind::Tv => "tv".into(),
                },
                id: h.id,
                title,
                year,
            }
        })
        .collect())
}

#[derive(Debug)]
pub struct AssignResult {
    pub item_key: String,
    pub migrate: MigrateReport,
}

/// Assign a TMDB id below the auto floor is allowed (ADR-0028 §4).
pub fn assign<T: MetadataSource, A: ArtworkInvalidate>(
    conn: &Connection,
    resolver: &Resolver<T>,
    client: &TmdbClient,
    artwork: &A,
    req: &AssignRequest,
) -> Result<AssignResult, String> {
    let item = load_item(conn, req.media_item_id)?;
    let old_keys = link_keys_for_item(conn, item.id)?;
    let old_effective = effective_item_key(conn, item.id, item.library_id, &item.path)?;

    let kind = req.kind.as_str();
    let (new_key, meta) = match kind {
        "movie" => {
            let (meta, raw) = client.movie_detail(req.tmdb_id).map_err(|e| e.to_string())?;
            canonical::persist_mapped_hit(conn, PROVIDER_TMDB, &raw, &meta)
                .map_err(|e| e.to_string())?;
            let key = item_key_for_metadata(&meta)
                .ok_or_else(|| "movie detail missing tmdb id".to_string())?;
            let tx = conn
                .unchecked_transaction()
                .map_err(|e| format!("begin assign tx: {e}"))?;
            clear_all_links_for_media_item(&tx, item.id)?;
            upsert_link(&tx, item.id, &key, true)?;
            tx.commit().map_err(|e| format!("commit assign: {e}"))?;
            (key, meta)
        }
        "tv" => {
            let (meta, raw) = client.tv_detail(req.tmdb_id).map_err(|e| e.to_string())?;
            canonical::persist_mapped_hit(conn, PROVIDER_TMDB, &raw, &meta)
                .map_err(|e| e.to_string())?;
            // Bind season→episode for this file (and multi-ep ranges).
            let _bind = queue::bind_resolved_items(conn, resolver, &[item.id], &meta)?;
            let tx = conn
                .unchecked_transaction()
                .map_err(|e| format!("begin assign tv tx: {e}"))?;
            set_manually_matched(&tx, item.id)?;
            // If still unbound (missing S/E), keep show assignment as path? No —
            // leave unlinked but ready so retry can re-bind after parse fix.
            tx.commit().map_err(|e| format!("commit assign tv: {e}"))?;
            let keys = link_keys_for_item(conn, item.id)?;
            let key = keys.first().cloned().unwrap_or_else(|| {
                // No episode id yet: still force ready with path key semantics
                // until season bind can attach.
                path_item_key(item.library_id, &item.path)
            });
            (key, meta)
        }
        other => return Err(format!("unsupported assign kind {other}")),
    };

    let mut old_for_migrate = old_keys;
    if !old_for_migrate.contains(&old_effective) {
        old_for_migrate.push(old_effective.clone());
    }
    let migrate = migrator::migrate_item_keys(conn, &old_for_migrate, &new_key)?;
    artwork.on_item_key_change(&old_for_migrate, Some(&new_key));
    queue::set_metadata_status(conn, &[item.id], MetadataStatus::Ready)?;
    let _ = meta;
    Ok(AssignResult {
        item_key: new_key,
        migrate,
    })
}

#[derive(Debug)]
pub struct ClearResult {
    pub item_key: String,
    pub migrate: MigrateReport,
}

pub fn clear_match<A: ArtworkInvalidate>(
    conn: &Connection,
    artwork: &A,
    media_item_id: i64,
) -> Result<ClearResult, String> {
    let item = load_item(conn, media_item_id)?;
    let old_keys = link_keys_for_item(conn, item.id)?;
    let path_key = path_item_key(item.library_id, &item.path);
    let tx = conn
        .unchecked_transaction()
        .map_err(|e| format!("begin clear: {e}"))?;
    clear_all_links_for_media_item(&tx, item.id)?;
    tx.commit().map_err(|e| format!("commit clear: {e}"))?;
    let migrate = migrator::migrate_item_keys(conn, &old_keys, &path_key)?;
    artwork.on_item_key_change(&old_keys, Some(&path_key));
    queue::set_metadata_status(conn, &[item.id], MetadataStatus::Pending)?;
    Ok(ClearResult {
        item_key: path_key,
        migrate,
    })
}

pub fn retry_unmatched(conn: &Connection, media_item_id: i64) -> Result<(), String> {
    let item = load_item(conn, media_item_id)?;
    let (title, year, kind) = if item.kind == "episode" {
        let (ct, _) = clean_show_title(&item.title);
        (ct, None, CacheKind::Tv)
    } else {
        let (ct, cy) = clean_movie_title(&item.title, year_from_path(&item.path).or(item.year));
        (ct, cy, CacheKind::Movie)
    };
    let qk = query_key(&title, year);
    negative_cache::clear(conn, PROVIDER_TMDB, kind, &qk)?;
    queue::set_metadata_status(conn, &[item.id], MetadataStatus::Pending)?;
    Ok(())
}

/// Load item fields for API handlers.
pub fn get_fix_item(conn: &Connection, id: i64) -> Result<FixItemView, String> {
    load_item(conn, id)
}
