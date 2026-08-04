//! Metadata work queue as a query over `metadata_status` (ADR-0026 §8).
//!
//! No jobs table. Pending rows are selected, grouped by search `query_key`
//! (one provider resolve per group), ordered by band then `max_id DESC`.
//! Bands derive at query time — no priority column.

use std::collections::{HashMap, HashSet};
use std::time::Instant;

use rusqlite::{Connection, OptionalExtension, params};

use nightjar_db::resolve_media_path;

use crate::canonical;
use crate::clean::{
    clean_movie_title, clean_show_title, pick_reference_episode, series_library_year,
    year_from_path,
};
use crate::item_links;
use crate::model::{ArtworkKind, CanonicalMetadata, MetadataKind, item_key_for_metadata};
use crate::negative_cache::{PROVIDER_TMDB, query_key};
use crate::resolve::MetadataSource;
use crate::resolve::{ResolveInput, ResolveOutcome, Resolver};

/// Roughly one cold first screen (ADR-0026 §8). Constant, not a setting.
pub const VISIBLE_FIRST_SCREEN_N: usize = 40;

/// Predicted `T_first_screen` for dogfood Visible union (~40 movie + ~40 show
/// groups × 1.84 HTTP/group ÷ 4.9 rps). Pass bar is 60 s.
pub const T_FIRST_SCREEN_PREDICTED_SECS: f64 = 30.0;

/// Pass bar for [`T_FIRST_SCREEN_PREDICTED_SECS`] (ADR-0026 §8).
pub const T_FIRST_SCREEN_PASS_SECS: f64 = 60.0;

/// Priority bands (ADR-0026 §8). Lower ordinal = sooner.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub enum QueueBand {
    /// Block 2 — predicate empty until watch-progress exists.
    ContinueWatching = 0,
    /// Browse-unit proxy (top-N per library kind).
    Visible = 1,
    /// Reserved undesigned — predicate empty until Block 3.
    Search = 2,
    RecentlyAdded = 3,
    Background = 4,
}

impl QueueBand {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::ContinueWatching => "continue_watching",
            Self::Visible => "visible",
            Self::Search => "search",
            Self::RecentlyAdded => "recently_added",
            Self::Background => "background",
        }
    }
}

/// Continue-watching band: empty until Block 2 watch-progress exists.
fn continue_watching_item_ids(_conn: &Connection) -> HashSet<i64> {
    HashSet::new()
}

/// Search band: reserved undesigned — always empty (no boost table).
fn search_boost_item_ids(_conn: &Connection) -> HashSet<i64> {
    HashSet::new()
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum MetadataStatus {
    Pending,
    /// Search (or NFO with TMDB id) accepted >= 0.80; sparse canonical written;
    /// enrich (detail) still pending (ADR-0026 §8.1).
    Matched,
    Ready,
    Unmatched,
}

impl MetadataStatus {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Pending => "pending",
            Self::Matched => "matched",
            Self::Ready => "ready",
            Self::Unmatched => "unmatched",
        }
    }

    pub fn parse(s: &str) -> Option<Self> {
        match s {
            "pending" => Some(Self::Pending),
            "matched" => Some(Self::Matched),
            "ready" => Some(Self::Ready),
            "unmatched" => Some(Self::Unmatched),
            _ => None,
        }
    }

    /// Terminal for the adult first screen / Visible grid (ADR-0026 §8.2):
    /// `matched` | `ready` | `unmatched`. `pending` always remains work.
    pub fn is_terminal(self) -> bool {
        matches!(self, Self::Matched | Self::Ready | Self::Unmatched)
    }
}

#[derive(Debug, Clone)]
pub struct PendingItem {
    pub id: i64,
    pub kind: String,
    pub title: String,
    pub year: Option<i32>,
    pub path: String,
    /// Library root `path` is relative to (ADR-0030).
    pub library_path: String,
    pub season: Option<i32>,
    pub episode: Option<i32>,
}

/// One browse unit in the Visible proxy (movie, or provisional show soft-key).
#[derive(Debug, Clone)]
pub struct VisibleProxyUnit {
    pub unit_key: String,
    pub library_id: i64,
    pub item_ids: Vec<i64>,
    pub is_movie: bool,
}

/// Snapshot of top-N browse units per library (ADR-0026 §8).
#[derive(Debug, Clone, Default)]
pub struct VisibleProxy {
    pub units: Vec<VisibleProxyUnit>,
}

impl VisibleProxy {
    pub fn item_id_set(&self) -> HashSet<i64> {
        let mut s = HashSet::new();
        for u in &self.units {
            s.extend(u.item_ids.iter().copied());
        }
        s
    }

    pub fn movie_unit_count(&self) -> usize {
        self.units.iter().filter(|u| u.is_movie).count()
    }

    pub fn show_unit_count(&self) -> usize {
        self.units.iter().filter(|u| !u.is_movie).count()
    }
}

#[derive(Debug, Clone)]
struct LibraryItemRow {
    id: i64,
    library_id: i64,
    library_kind: String,
    kind: String,
    title: String,
    year: Option<i32>,
    path: String,
}

/// All-items Visible proxy: movies by title; shows by provisional soft key
/// (`clean_show_title` → yearless `query_key`). Rank is a library property.
pub fn snapshot_visible_proxy(conn: &Connection) -> Result<VisibleProxy, String> {
    snapshot_visible_proxy_filtered(conn, VISIBLE_FIRST_SCREEN_N, &[])
}

pub fn snapshot_visible_proxy_n(conn: &Connection, n: usize) -> Result<VisibleProxy, String> {
    snapshot_visible_proxy_filtered(conn, n, &[])
}

/// `exclude_library_names` drops named libraries from the proxy (harnesses
/// supply machine-specific names via env — see `measure_exclude`).
pub fn snapshot_visible_proxy_filtered(
    conn: &Connection,
    n: usize,
    exclude_library_names: &[&str],
) -> Result<VisibleProxy, String> {
    let mut stmt = conn
        .prepare(
            "SELECT m.id, m.library_id, l.kind, m.kind, m.title, m.year, m.path, l.name
             FROM media_items m
             JOIN libraries l ON l.id = m.library_id
             ORDER BY m.library_id, m.id",
        )
        .map_err(|e| format!("prepare visible snapshot: {e}"))?;
    let rows = stmt
        .query_map([], |r| {
            Ok((
                LibraryItemRow {
                    id: r.get(0)?,
                    library_id: r.get(1)?,
                    library_kind: r.get(2)?,
                    kind: r.get(3)?,
                    title: r.get(4)?,
                    year: r.get(5)?,
                    path: r.get(6)?,
                },
                r.get::<_, String>(7)?,
            ))
        })
        .map_err(|e| format!("query visible snapshot: {e}"))?;

    let mut by_library: HashMap<i64, (String, Vec<LibraryItemRow>)> = HashMap::new();
    for row in rows {
        let (row, lib_name) = row.map_err(|e| format!("visible row: {e}"))?;
        if exclude_library_names.iter().any(|n| *n == lib_name) {
            continue;
        }
        by_library
            .entry(row.library_id)
            .or_insert_with(|| (row.library_kind.clone(), Vec::new()))
            .1
            .push(row);
    }

    let mut units = Vec::new();
    for (library_id, (library_kind, items)) in by_library {
        match library_kind.as_str() {
            "movies" => {
                let mut movies: Vec<&LibraryItemRow> =
                    items.iter().filter(|i| i.kind == "movie").collect();
                movies.sort_by(|a, b| {
                    a.title
                        .to_lowercase()
                        .cmp(&b.title.to_lowercase())
                        .then_with(|| a.id.cmp(&b.id))
                });
                for m in movies.into_iter().take(n) {
                    let folder_year = year_from_path(&m.path);
                    let (ct, cy) = clean_movie_title(&m.title, folder_year.or(m.year));
                    let qk = query_key(&ct, cy);
                    units.push(VisibleProxyUnit {
                        unit_key: format!("movie|{qk}"),
                        library_id,
                        item_ids: vec![m.id],
                        is_movie: true,
                    });
                }
            }
            "shows" => {
                // Prefer durable tmdb_show when episode links exist (ADR-0029 §2.5);
                // otherwise resolve soft key (ADR-0026 §8 provisional).
                let mut by_show: HashMap<String, Vec<&LibraryItemRow>> = HashMap::new();
                for it in &items {
                    if it.kind != "episode" {
                        continue;
                    }
                    let unit_key = visible_show_unit_key(conn, it)?;
                    by_show.entry(unit_key).or_default().push(it);
                }
                let mut show_units: Vec<(String, String, Vec<i64>, i64)> = by_show
                    .into_iter()
                    .map(|(unit_key, eps)| {
                        let sort_title = {
                            let (ct, _) = clean_show_title(&eps[0].title);
                            ct.to_lowercase()
                        };
                        let mut ids: Vec<i64> = eps.iter().map(|e| e.id).collect();
                        ids.sort_unstable();
                        let max_id = ids.iter().copied().max().unwrap_or(0);
                        (unit_key, sort_title, ids, max_id)
                    })
                    .collect();
                show_units.sort_by(|a, b| a.1.cmp(&b.1).then_with(|| a.3.cmp(&b.3)));
                for (unit_key, _, item_ids, _) in show_units.into_iter().take(n) {
                    units.push(VisibleProxyUnit {
                        unit_key,
                        library_id,
                        item_ids,
                        is_movie: false,
                    });
                }
            }
            _ => {}
        }
    }
    Ok(VisibleProxy { units })
}

/// Browse unit for one episode file: `tv|tmdb:{show_id}` when linked,
/// else soft-key `tv|{query_key}`.
fn visible_show_unit_key(conn: &Connection, it: &LibraryItemRow) -> Result<String, String> {
    if let Some(show_id) = tmdb_show_for_media_item(conn, it.id)? {
        return Ok(format!("tv|tmdb:{show_id}"));
    }
    let (ct, _) = clean_show_title(&it.title);
    let qk = query_key(&ct, None);
    Ok(format!("tv|{qk}"))
}

fn tmdb_show_for_media_item(conn: &Connection, media_item_id: i64) -> Result<Option<i64>, String> {
    let key: Option<String> = conn
        .query_row(
            "SELECT item_key FROM media_item_links
             WHERE media_item_id = ?1 AND item_key LIKE 'tmdb:episode:%'
             ORDER BY manually_matched DESC, item_key
             LIMIT 1",
            params![media_item_id],
            |r| r.get(0),
        )
        .optional()
        .map_err(|e| format!("episode link for visible: {e}"))?;
    let Some(key) = key else {
        return Ok(None);
    };
    let Some(ep_id) = key.strip_prefix("tmdb:episode:") else {
        return Ok(None);
    };
    let show: Option<i64> = conn
        .query_row(
            "SELECT tmdb_show FROM metadata_canonical
             WHERE provider = 'tmdb' AND entity_kind = 'episode' AND provider_id = ?1",
            params![ep_id],
            |r| r.get(0),
        )
        .optional()
        .map_err(|e| format!("tmdb_show for visible: {e}"))?;
    Ok(show)
}

fn band_for_item(
    item_id: i64,
    visible: &HashSet<i64>,
    continue_watching: &HashSet<i64>,
    search: &HashSet<i64>,
) -> QueueBand {
    if continue_watching.contains(&item_id) {
        QueueBand::ContinueWatching
    } else if visible.contains(&item_id) {
        QueueBand::Visible
    } else if search.contains(&item_id) {
        QueueBand::Search
    } else {
        QueueBand::RecentlyAdded
    }
}

/// Band for a pending item given a Visible snapshot (CW/Search empty today).
pub fn queue_band_for_item(item_id: i64, visible: &VisibleProxy) -> QueueBand {
    band_for_item(
        item_id,
        &visible.item_id_set(),
        &HashSet::new(),
        &HashSet::new(),
    )
}

#[derive(Debug, Clone)]
struct QueryGroup {
    resolve_kind: MetadataKind,
    /// Reference media file path for the group (sidecar NFO lookup, ADR-0026).
    path: String,
    /// Library root `path` is relative to (ADR-0030).
    library_path: String,
    title: String,
    year: Option<i32>,
    library_year: Option<i32>,
    library_episode_count: Option<u32>,
    library_season_count: Option<u32>,
    ref_season: Option<i32>,
    ref_episode: Option<i32>,
    ref_episode_title: Option<String>,
    item_ids: Vec<i64>,
    max_id: i64,
    band: QueueBand,
    unit_key: String,
}

#[derive(Debug, Default)]
pub struct DrainStats {
    pub groups: usize,
    pub movie_groups: usize,
    pub show_groups: usize,
    /// Search tier landed `matched` (not yet enriched).
    pub items_matched: usize,
    pub items_ready: usize,
    pub items_unmatched: usize,
    pub items_left_pending: usize,
    pub provider_resolves: usize,
    pub provider_errors: usize,
    pub http_429: u64,
    pub http_requests: u64,
    /// Set when draining with a Visible proxy (first-screen measure).
    pub visible_proxy_size: usize,
    pub proxy_movie_units: usize,
    pub proxy_show_units: usize,
    pub unmatched_in_proxy: usize,
    pub ready_in_proxy: usize,
    pub ready_missing_poster: usize,
    pub t_first_screen_secs: Option<f64>,
    pub predicted_secs: f64,
    pub gate_pass: bool,
    pub stopped_early: bool,
    /// Season detail fetches that returned a payload (live client or double).
    pub seasons_fetched: usize,
    /// Episode canonical rows projected from season payloads this drain.
    pub episodes_projected: usize,
    /// Media files that received at least one provider link this drain.
    pub files_linked: usize,
    /// Season fetches skipped (`fetch_season` → `None`, stub sources).
    pub seasons_skipped: usize,
    /// Bind failures (logged; items may still be marked ready).
    pub bind_errors: usize,
}

/// Counters from one [`bind_resolved_items`] call.
#[derive(Debug, Default, Clone, Copy)]
pub struct BindStats {
    pub seasons_fetched: usize,
    pub episodes_projected: usize,
    pub files_linked: usize,
    pub seasons_skipped: usize,
}

fn has_poster(meta: &CanonicalMetadata) -> bool {
    meta.artwork
        .iter()
        .any(|a| a.kind == ArtworkKind::Poster && !a.path.is_empty())
}

fn statuses_for_ids(conn: &Connection, ids: &[i64]) -> Result<Vec<MetadataStatus>, String> {
    if ids.is_empty() {
        return Ok(Vec::new());
    }
    let mut out = Vec::with_capacity(ids.len());
    let mut stmt = conn
        .prepare("SELECT metadata_status FROM media_items WHERE id = ?1")
        .map_err(|e| format!("prepare status read: {e}"))?;
    for id in ids {
        let s: String = stmt
            .query_row(params![id], |r| r.get(0))
            .map_err(|e| format!("status for {id}: {e}"))?;
        out.push(MetadataStatus::parse(&s).ok_or_else(|| format!("bad metadata_status {s}"))?);
    }
    Ok(out)
}

/// Proxy progress: terminal when every item is matched|ready|unmatched
/// (ADR-0026 §8.2 adult first screen). `ready_units` counts units that are
/// matched or ready (poster-bearing subset); `unmatched_units` are pure holes.
pub fn proxy_terminal_progress(
    conn: &Connection,
    proxy: &VisibleProxy,
    unit_has_poster: &HashMap<String, bool>,
) -> Result<(bool, usize, usize, usize), String> {
    let mut unmatched_units = 0usize;
    let mut ready_units = 0usize;
    let mut ready_missing_poster = 0usize;
    for u in &proxy.units {
        let statuses = statuses_for_ids(conn, &u.item_ids)?;
        if statuses.contains(&MetadataStatus::Pending) {
            return Ok((false, unmatched_units, ready_units, ready_missing_poster));
        }
        let any_matched_or_ready = statuses
            .iter()
            .any(|s| matches!(s, MetadataStatus::Matched | MetadataStatus::Ready));
        if any_matched_or_ready {
            ready_units += 1;
            let poster = unit_has_poster.get(&u.unit_key).copied().unwrap_or(false);
            if !poster {
                ready_missing_poster += 1;
            }
        } else {
            unmatched_units += 1;
        }
    }
    Ok((true, unmatched_units, ready_units, ready_missing_poster))
}

/// Load items at `status` and fold into resolve groups (band, then newest first).
fn status_query_groups(
    conn: &Connection,
    visible: &VisibleProxy,
    status: MetadataStatus,
) -> Result<Vec<QueryGroup>, String> {
    let visible_ids = visible.item_id_set();
    let cw = continue_watching_item_ids(conn);
    let search = search_boost_item_ids(conn);

    let mut stmt = conn
        .prepare(
            "SELECT m.id, m.kind, m.title, m.year, m.path, m.season, m.episode, l.path
             FROM media_items m
             JOIN libraries l ON l.id = m.library_id
             WHERE m.metadata_status = ?1
             ORDER BY m.id DESC",
        )
        .map_err(|e| format!("prepare status groups: {e}"))?;
    let rows = stmt
        .query_map(params![status.as_str()], |r| {
            Ok(PendingItem {
                id: r.get(0)?,
                kind: r.get(1)?,
                title: r.get(2)?,
                year: r.get(3)?,
                path: r.get(4)?,
                library_path: r.get(7)?,
                season: r.get(5)?,
                episode: r.get(6)?,
            })
        })
        .map_err(|e| format!("query status groups: {e}"))?;

    let mut items = Vec::new();
    for row in rows {
        items.push(row.map_err(|e| format!("status group row: {e}"))?);
    }

    let mut ep_by_show: HashMap<String, Vec<&PendingItem>> = HashMap::new();
    for it in &items {
        if it.kind == "episode" {
            let (ct, _) = clean_show_title(&it.title);
            ep_by_show.entry(ct).or_default().push(it);
        }
    }

    let mut groups: HashMap<String, QueryGroup> = HashMap::new();
    for it in &items {
        let band = band_for_item(it.id, &visible_ids, &cw, &search);
        match it.kind.as_str() {
            "movie" => {
                let folder_year = year_from_path(&it.path);
                let (ct, cy) = clean_movie_title(&it.title, folder_year.or(it.year));
                let qk = query_key(&ct, cy);
                let unit_key = format!("movie|{qk}");
                let g = groups
                    .entry(unit_key.clone())
                    .or_insert_with(|| QueryGroup {
                        resolve_kind: MetadataKind::Movie,
                        path: it.path.clone(),
                        library_path: it.library_path.clone(),
                        title: ct,
                        year: cy,
                        library_year: None,
                        library_episode_count: None,
                        library_season_count: None,
                        ref_season: None,
                        ref_episode: None,
                        ref_episode_title: None,
                        item_ids: Vec::new(),
                        max_id: it.id,
                        band,
                        unit_key,
                    });
                g.item_ids.push(it.id);
                g.max_id = g.max_id.max(it.id);
                g.band = g.band.min(band);
            }
            "episode" => {
                let (ct, _) = clean_show_title(&it.title);
                let siblings = ep_by_show.get(&ct).map(|v| v.as_slice()).unwrap_or(&[]);
                let years = siblings.iter().map(|s| s.year);
                let path0 = siblings
                    .first()
                    .map(|s| s.path.as_str())
                    .unwrap_or(it.path.as_str());
                let library_path0 = siblings
                    .first()
                    .map(|s| s.library_path.as_str())
                    .unwrap_or(it.library_path.as_str());
                let library_year = series_library_year(years, path0);
                let seasons: std::collections::HashSet<i32> =
                    siblings.iter().filter_map(|s| s.season).collect();
                let ref_eps: Vec<(i32, i32, &str)> = siblings
                    .iter()
                    .filter_map(|s| {
                        let season = s.season?;
                        let episode = s.episode?;
                        let base = std::path::Path::new(&s.path)
                            .file_name()
                            .and_then(|n| n.to_str())
                            .unwrap_or(s.path.as_str());
                        Some((season, episode, base))
                    })
                    .collect();
                let pref = pick_reference_episode(&ref_eps, &ct);
                let qk = query_key(&ct, None);
                let unit_key = format!("tv|{qk}");
                let g = groups
                    .entry(unit_key.clone())
                    .or_insert_with(|| QueryGroup {
                        resolve_kind: MetadataKind::Episode,
                        path: path0.to_string(),
                        library_path: library_path0.to_string(),
                        title: ct.clone(),
                        year: None,
                        library_year,
                        library_episode_count: Some(siblings.len() as u32),
                        library_season_count: (!seasons.is_empty()).then_some(seasons.len() as u32),
                        ref_season: pref.as_ref().map(|p| p.0),
                        ref_episode: pref.as_ref().map(|p| p.1),
                        ref_episode_title: pref.map(|p| p.2),
                        item_ids: Vec::new(),
                        max_id: it.id,
                        band,
                        unit_key,
                    });
                g.item_ids.push(it.id);
                g.max_id = g.max_id.max(it.id);
                g.band = g.band.min(band);
            }
            _ => {}
        }
    }

    let mut out: Vec<QueryGroup> = groups.into_values().collect();
    out.sort_by(|a, b| a.band.cmp(&b.band).then_with(|| b.max_id.cmp(&a.max_id)));
    Ok(out)
}

fn pending_query_groups(
    conn: &Connection,
    visible: &VisibleProxy,
) -> Result<Vec<QueryGroup>, String> {
    status_query_groups(conn, visible, MetadataStatus::Pending)
}

fn matched_query_groups(
    conn: &Connection,
    visible: &VisibleProxy,
) -> Result<Vec<QueryGroup>, String> {
    status_query_groups(conn, visible, MetadataStatus::Matched)
}

/// Provider id stored at search tier for enrich short-circuit (movie watch key
/// or provisional `tmdb:show:{id}` for TV).
fn tmdb_id_from_links(
    conn: &Connection,
    item_ids: &[i64],
) -> Result<Option<(i64, MetadataKind)>, String> {
    for id in item_ids {
        for key in item_links::link_keys_for_item(conn, *id)? {
            if let Some(rest) = key.strip_prefix("tmdb:movie:")
                && let Ok(n) = rest.parse::<i64>()
            {
                return Ok(Some((n, MetadataKind::Movie)));
            }
            if let Some(rest) = key.strip_prefix("tmdb:show:")
                && let Ok(n) = rest.parse::<i64>()
            {
                return Ok(Some((n, MetadataKind::Show)));
            }
        }
    }
    Ok(None)
}

/// Search tier: write identity + sparse/full projected canonical, **no** season
/// bind. Movies get `tmdb:movie:` links; TV gets provisional `tmdb:show:` for
/// enrich id recovery (not a watch key — see [`item_links::is_watch_item_key`]).
fn apply_search_hit(
    conn: &Connection,
    item_ids: &[i64],
    metadata: &CanonicalMetadata,
) -> Result<(), String> {
    let tx = conn
        .unchecked_transaction()
        .map_err(|e| format!("begin search-hit tx: {e}"))?;
    match metadata.kind {
        MetadataKind::Movie => {
            if let Some(key) = item_key_for_metadata(metadata) {
                for id in item_ids {
                    item_links::replace_auto_link(&tx, *id, &key)?;
                }
            }
        }
        MetadataKind::Show | MetadataKind::Episode => {
            if let Some(show_id) = metadata.ids.tmdb.or(metadata.ids.tmdb_show) {
                let key = format!("tmdb:show:{show_id}");
                for id in item_ids {
                    item_links::replace_auto_link(&tx, *id, &key)?;
                }
            }
        }
    }
    tx.commit().map_err(|e| format!("commit search-hit: {e}"))?;
    set_metadata_status(conn, item_ids, MetadataStatus::Matched)?;
    Ok(())
}

/// Write provider bindings (and season→episode projection when the source
/// supports `fetch_season`). TV files without a season fetch stay unbound
/// (derived path key — ADR-0029 §2 / §3). Stub sources skip seasons; live
/// `TmdbClient` fetches season detail inside this bind (ADR-0029 §3).
///
/// Public for manual assign (ADR-0028) and product drain.
pub fn bind_resolved_items<T: MetadataSource>(
    conn: &Connection,
    resolver: &Resolver<T>,
    item_ids: &[i64],
    metadata: &CanonicalMetadata,
) -> Result<BindStats, String> {
    let mut stats = BindStats::default();
    match metadata.kind {
        MetadataKind::Movie => {
            let Some(key) = item_key_for_metadata(metadata) else {
                return Ok(stats);
            };
            let tx = conn
                .unchecked_transaction()
                .map_err(|e| format!("begin movie bind tx: {e}"))?;
            for id in item_ids {
                item_links::replace_auto_link(&tx, *id, &key)?;
                stats.files_linked += 1;
            }
            tx.commit().map_err(|e| format!("commit movie bind: {e}"))?;
            Ok(stats)
        }
        MetadataKind::Show | MetadataKind::Episode => {
            let Some(show_id) = metadata.ids.tmdb.or(metadata.ids.tmdb_show) else {
                return Ok(stats);
            };
            let rows = episode_slots(conn, item_ids)?;
            let seasons: std::collections::HashSet<i32> =
                rows.iter().filter_map(|r| r.season).collect();
            if seasons.is_empty() {
                return Ok(stats);
            }
            // One file may cover several episode numbers (ADR-0025 §2 range).
            let mut by_se: std::collections::HashMap<(i32, i32), i64> =
                std::collections::HashMap::new();
            for row in &rows {
                for (s, e) in row.season_episodes() {
                    by_se.insert((s, e), row.id);
                }
            }
            for sn in seasons {
                let Some(raw) = resolver
                    .tmdb
                    .fetch_season(show_id, sn)
                    .map_err(|e| e.to_string())?
                else {
                    // Stub, or TMDB 404 for this season number — skip and try others.
                    stats.seasons_skipped += 1;
                    continue;
                };
                stats.seasons_fetched += 1;
                let eps = canonical::persist_season_projection(conn, PROVIDER_TMDB, show_id, &raw)?;
                stats.episodes_projected += eps.len();
                let mut keys_by_media: std::collections::HashMap<i64, Vec<String>> =
                    std::collections::HashMap::new();
                for ep in &eps {
                    let (Some(s), Some(e)) = (ep.season, ep.episode) else {
                        continue;
                    };
                    let Some(media_id) = by_se.get(&(s, e)) else {
                        continue;
                    };
                    let Some(key) = item_key_for_metadata(ep) else {
                        continue;
                    };
                    keys_by_media.entry(*media_id).or_default().push(key);
                }
                let tx = conn
                    .unchecked_transaction()
                    .map_err(|e| format!("begin episode bind tx: {e}"))?;
                for (media_id, keys) in &keys_by_media {
                    item_links::replace_auto_links(&tx, *media_id, keys)?;
                    stats.files_linked += 1;
                }
                tx.commit()
                    .map_err(|e| format!("commit episode bind: {e}"))?;
            }
            Ok(stats)
        }
    }
}

struct EpisodeSlot {
    id: i64,
    season: Option<i32>,
    episode: Option<i32>,
    path: String,
}

impl EpisodeSlot {
    /// Season/episode pairs this file covers. Re-parses the basename for
    /// `NxMM-NN` ranges so we do not need an `episode_end` column.
    fn season_episodes(&self) -> Vec<(i32, i32)> {
        let Some(season) = self.season else {
            return Vec::new();
        };
        let Some(start) = self.episode else {
            return Vec::new();
        };
        let base = std::path::Path::new(&self.path)
            .file_name()
            .and_then(|n| n.to_str())
            .unwrap_or(self.path.as_str());
        let parsed = nightjar_core::parse_filename(base);
        if parsed.season == Some(season) && parsed.episode == Some(start) {
            return parsed
                .episode_numbers()
                .into_iter()
                .map(|e| (season, e))
                .collect();
        }
        vec![(season, start)]
    }
}

fn episode_slots(conn: &Connection, ids: &[i64]) -> Result<Vec<EpisodeSlot>, String> {
    let mut out = Vec::with_capacity(ids.len());
    let mut stmt = conn
        .prepare("SELECT id, season, episode, path FROM media_items WHERE id = ?1")
        .map_err(|e| format!("prepare episode slots: {e}"))?;
    for id in ids {
        let row = stmt
            .query_row(params![id], |r| {
                Ok(EpisodeSlot {
                    id: r.get(0)?,
                    season: r.get(1)?,
                    episode: r.get(2)?,
                    path: r.get(3)?,
                })
            })
            .map_err(|e| format!("episode slot {id}: {e}"))?;
        out.push(row);
    }
    Ok(out)
}

pub fn set_metadata_status(
    conn: &Connection,
    ids: &[i64],
    status: MetadataStatus,
) -> Result<(), String> {
    if ids.is_empty() {
        return Ok(());
    }
    let tx = conn
        .unchecked_transaction()
        .map_err(|e| format!("begin status tx: {e}"))?;
    {
        let mut stmt = tx
            .prepare("UPDATE media_items SET metadata_status = ?1 WHERE id = ?2")
            .map_err(|e| format!("prepare status update: {e}"))?;
        for id in ids {
            stmt.execute(params![status.as_str(), id])
                .map_err(|e| format!("update status {id}: {e}"))?;
        }
    }
    tx.commit().map_err(|e| format!("commit status: {e}"))?;
    Ok(())
}

/// Poster warm hook fired on the search-tier → `matched` transition
/// (ADR-0026 §8). No artwork store exists yet (ADR-0027 pending), so the
/// default is a no-op; product wires a real implementation when the artwork
/// cache pipeline lands. Never blocks the drain.
pub trait PosterWarm: Send + Sync {
    /// `item_ids` all landed `matched` with the same `metadata`.
    fn on_matched(&self, item_ids: &[i64], metadata: &CanonicalMetadata);
}

/// Default: nothing to warm until an artwork store exists.
#[derive(Debug, Default, Clone, Copy)]
pub struct NoopPosterWarm;

impl PosterWarm for NoopPosterWarm {
    fn on_matched(&self, _item_ids: &[i64], _metadata: &CanonicalMetadata) {}
}

/// Named drain call-site helper: warms posters for a freshly `matched`
/// group when a hook is wired in; no-ops when the store is missing.
pub fn warm_poster_for_matched(
    warm: Option<&dyn PosterWarm>,
    item_ids: &[i64],
    metadata: &CanonicalMetadata,
) {
    if let Some(w) = warm {
        w.on_matched(item_ids, metadata);
    }
}

/// Options for [`drain_pending`].
#[derive(Default)]
pub struct DrainOptions {
    /// Cap groups (short probes). Ignored when [`Self::stop_when_visible_terminal`].
    pub max_groups: Option<usize>,
    /// Snapshot Visible once; stop when every proxy unit is terminal.
    pub stop_when_visible_terminal: bool,
    /// Library names omitted from the Visible snapshot (measure excludes).
    pub exclude_library_names: Vec<String>,
    /// Poster warm hook for the `matched` transition (see [`warm_poster_for_matched`]).
    pub poster_warm: Option<Box<dyn PosterWarm>>,
}

/// Drain two-tier work (ADR-0026 §8):
/// 1. **Search** `pending` → `matched` | `unmatched` (no season bind).
/// 2. **Enrich** `matched` → `ready` by stored id (no re-search) + season bind.
///
/// Fairness: Visible-banded groups sort first within each class (existing
/// band order). Search class runs before enrich class in one call so a
/// single product-drain tick paints the grid before full enrich.
///
/// Provider/`api_error` failures leave rows **pending** (search) or
/// **matched** (enrich) and are not negative-cached.
///
/// Sidecar NFO (Kodi layout) feeds the search tier only: same-stem `.nfo`
/// beside the media file, or `<dir>/episodedetails.nfo` for episode groups.
/// Enrich stays TMDB-id-driven so a sparse NFO never blocks season detail.
pub fn drain_pending<T: MetadataSource>(
    conn: &Connection,
    resolver: &Resolver<T>,
    http_429: &std::sync::atomic::AtomicU64,
    http_requests: &std::sync::atomic::AtomicU64,
    opts: DrainOptions,
) -> Result<DrainStats, String> {
    let exclude: Vec<&str> = opts
        .exclude_library_names
        .iter()
        .map(String::as_str)
        .collect();
    let proxy = snapshot_visible_proxy_filtered(conn, VISIBLE_FIRST_SCREEN_N, &exclude)?;
    let mut search_groups = pending_query_groups(conn, &proxy)?;
    if !opts.stop_when_visible_terminal
        && let Some(n) = opts.max_groups
    {
        search_groups.truncate(n);
    }

    let mut stats = DrainStats {
        groups: search_groups.len(),
        movie_groups: search_groups
            .iter()
            .filter(|g| g.resolve_kind == MetadataKind::Movie)
            .count(),
        show_groups: search_groups
            .iter()
            .filter(|g| g.resolve_kind == MetadataKind::Episode)
            .count(),
        visible_proxy_size: proxy.units.len(),
        proxy_movie_units: proxy.movie_unit_count(),
        proxy_show_units: proxy.show_unit_count(),
        predicted_secs: T_FIRST_SCREEN_PREDICTED_SECS * (proxy.units.len() as f64 / 80.0),
        ..DrainStats::default()
    };

    let mut unit_has_poster: HashMap<String, bool> = HashMap::new();
    // Units already search-terminal (matched/ready) — poster unknown → fail open false.
    for u in &proxy.units {
        let statuses = statuses_for_ids(conn, &u.item_ids)?;
        if statuses.iter().all(|s| s.is_terminal())
            && statuses
                .iter()
                .any(|s| matches!(s, MetadataStatus::Matched | MetadataStatus::Ready))
        {
            unit_has_poster.entry(u.unit_key.clone()).or_insert(false);
        }
    }

    let t0 = Instant::now();
    if opts.stop_when_visible_terminal {
        let (term, unmatched, ready, missing) =
            proxy_terminal_progress(conn, &proxy, &unit_has_poster)?;
        if term {
            stats.unmatched_in_proxy = unmatched;
            stats.ready_in_proxy = ready;
            stats.ready_missing_poster = missing;
            stats.t_first_screen_secs = Some(0.0);
            stats.gate_pass = missing == 0;
            stats.stopped_early = true;
            stats.groups = 0;
            stats.movie_groups = 0;
            stats.show_groups = 0;
            stats.http_429 = http_429.load(std::sync::atomic::Ordering::Relaxed);
            stats.http_requests = http_requests.load(std::sync::atomic::Ordering::Relaxed);
            return Ok(stats);
        }
    }

    // --- Phase A: search pending → matched | unmatched ---
    let mut resolved_groups = 0usize;
    let mut stopped_on_visible = false;
    for (i, g) in search_groups.iter().enumerate() {
        if (i + 1) % 50 == 0 || i + 1 == search_groups.len() {
            eprintln!("  search {}/{} …", i + 1, search_groups.len());
        }
        let input = ResolveInput {
            nfo_xml: nfo_sidecar_xml(
                &resolve_media_path(&g.library_path, &g.path),
                g.resolve_kind,
            ),
            title: Some(g.title.clone()),
            year: g.year,
            library_year: g.library_year,
            library_episode_count: g.library_episode_count,
            library_season_count: g.library_season_count,
            ref_season: g.ref_season,
            ref_episode: g.ref_episode,
            ref_episode_title: g.ref_episode_title.clone(),
            kind: Some(g.resolve_kind),
            ..Default::default()
        };
        stats.provider_resolves += 1;
        match resolver.resolve_with_store(&input, conn) {
            Ok(ResolveOutcome::Resolved {
                metadata,
                match_method,
                ..
            }) => {
                let tmdb_id = metadata.ids.tmdb.or(metadata.ids.tmdb_show);
                eprintln!(
                    "  match {} → tmdb:{:?} method={} (search tier)",
                    g.title,
                    tmdb_id,
                    match_method.as_deref().unwrap_or("?")
                );
                apply_search_hit(conn, &g.item_ids, &metadata)?;
                warm_poster_for_matched(opts.poster_warm.as_deref(), &g.item_ids, &metadata);
                stats.items_matched += g.item_ids.len();
                let poster = has_poster(&metadata);
                unit_has_poster
                    .entry(g.unit_key.clone())
                    .and_modify(|p| *p = *p || poster)
                    .or_insert(poster);
            }
            Ok(ResolveOutcome::Unresolved { reason, .. }) => {
                eprintln!("  unmatched {} reason={reason:?}", g.title);
                set_metadata_status(conn, &g.item_ids, MetadataStatus::Unmatched)?;
                stats.items_unmatched += g.item_ids.len();
            }
            Err(e) => {
                eprintln!("  provider error (left pending): {} — {e}", g.title);
                stats.provider_errors += 1;
                stats.items_left_pending += g.item_ids.len();
            }
        }
        resolved_groups += 1;

        if opts.stop_when_visible_terminal {
            let (term, unmatched, ready, missing) =
                proxy_terminal_progress(conn, &proxy, &unit_has_poster)?;
            if term {
                stats.unmatched_in_proxy = unmatched;
                stats.ready_in_proxy = ready;
                stats.ready_missing_poster = missing;
                stats.t_first_screen_secs = Some(t0.elapsed().as_secs_f64());
                stats.gate_pass = missing == 0
                    && stats.t_first_screen_secs.unwrap_or(f64::MAX) <= T_FIRST_SCREEN_PASS_SECS;
                stats.stopped_early = true;
                stats.groups = resolved_groups;
                stats.movie_groups = search_groups[..resolved_groups]
                    .iter()
                    .filter(|g| g.resolve_kind == MetadataKind::Movie)
                    .count();
                stats.show_groups = search_groups[..resolved_groups]
                    .iter()
                    .filter(|g| g.resolve_kind == MetadataKind::Episode)
                    .count();
                stopped_on_visible = true;
                break;
            }
        }
    }

    // --- Phase B: enrich matched → ready (id only, + season bind) ---
    // Skip when first-screen measure stopped after search-terminal (next
    // product-drain tick picks up matched work).
    if !stopped_on_visible {
        let mut enrich_groups = matched_query_groups(conn, &proxy)?;
        if !opts.stop_when_visible_terminal
            && let Some(n) = opts.max_groups
        {
            // Cap total work: enrich at most remaining budget after search.
            let used = resolved_groups.min(n);
            let rest = n.saturating_sub(used);
            enrich_groups.truncate(rest);
        }
        stats.groups += enrich_groups.len();
        stats.movie_groups += enrich_groups
            .iter()
            .filter(|g| g.resolve_kind == MetadataKind::Movie)
            .count();
        stats.show_groups += enrich_groups
            .iter()
            .filter(|g| g.resolve_kind == MetadataKind::Episode)
            .count();

        for (i, g) in enrich_groups.iter().enumerate() {
            if (i + 1) % 50 == 0 || i + 1 == enrich_groups.len() {
                eprintln!("  enrich {}/{} …", i + 1, enrich_groups.len());
            }
            let Some((tmdb_id, id_kind)) = tmdb_id_from_links(conn, &g.item_ids)? else {
                eprintln!(
                    "  enrich skip {} — no stored tmdb id (left matched)",
                    g.title
                );
                continue;
            };
            let kind = match g.resolve_kind {
                MetadataKind::Movie => MetadataKind::Movie,
                MetadataKind::Episode | MetadataKind::Show => id_kind,
            };
            let input = ResolveInput {
                tmdb_id: Some(tmdb_id),
                kind: Some(kind),
                title: Some(g.title.clone()),
                year: g.year,
                library_year: g.library_year,
                library_episode_count: g.library_episode_count,
                library_season_count: g.library_season_count,
                ref_season: g.ref_season,
                ref_episode: g.ref_episode,
                ref_episode_title: g.ref_episode_title.clone(),
                ..Default::default()
            };
            stats.provider_resolves += 1;
            match resolver.resolve_with_store(&input, conn) {
                Ok(ResolveOutcome::Resolved { metadata, .. }) => {
                    eprintln!("  enrich {} → tmdb:{tmdb_id} (detail+bind)", g.title);
                    match bind_resolved_items(conn, resolver, &g.item_ids, &metadata) {
                        Ok(b) => {
                            stats.seasons_fetched += b.seasons_fetched;
                            stats.episodes_projected += b.episodes_projected;
                            stats.files_linked += b.files_linked;
                            stats.seasons_skipped += b.seasons_skipped;
                            if b.seasons_skipped > 0 {
                                eprintln!(
                                    "  bind {} seasons_fetched={} skipped={} linked={}",
                                    g.title, b.seasons_fetched, b.seasons_skipped, b.files_linked
                                );
                            }
                        }
                        Err(e) => {
                            eprintln!("  bind/season ({}): {e}", g.title);
                            stats.bind_errors += 1;
                        }
                    }
                    set_metadata_status(conn, &g.item_ids, MetadataStatus::Ready)?;
                    stats.items_ready += g.item_ids.len();
                    let poster = has_poster(&metadata);
                    unit_has_poster
                        .entry(g.unit_key.clone())
                        .and_modify(|p| *p = *p || poster)
                        .or_insert(poster);
                }
                Ok(ResolveOutcome::Unresolved { reason, .. }) => {
                    // Id was stored at match; unexpected miss — leave matched for retry.
                    eprintln!(
                        "  enrich unresolved {} reason={reason:?} (left matched)",
                        g.title
                    );
                    stats.provider_errors += 1;
                }
                Err(e) => {
                    eprintln!("  enrich provider error (left matched): {} — {e}", g.title);
                    stats.provider_errors += 1;
                }
            }
        }
    }

    if opts.stop_when_visible_terminal && stats.t_first_screen_secs.is_none() {
        let (term, unmatched, ready, missing) =
            proxy_terminal_progress(conn, &proxy, &unit_has_poster)?;
        stats.unmatched_in_proxy = unmatched;
        stats.ready_in_proxy = ready;
        stats.ready_missing_poster = missing;
        if term {
            stats.t_first_screen_secs = Some(t0.elapsed().as_secs_f64());
            stats.gate_pass = missing == 0
                && stats.t_first_screen_secs.unwrap_or(f64::MAX) <= T_FIRST_SCREEN_PASS_SECS;
        } else {
            stats.gate_pass = false;
        }
    }

    stats.http_429 = http_429.load(std::sync::atomic::Ordering::Relaxed);
    stats.http_requests = http_requests.load(std::sync::atomic::Ordering::Relaxed);
    Ok(stats)
}

/// Best-effort sidecar NFO for a group's reference media path (Kodi layout):
/// `foo.mkv` → `foo.nfo` beside the file; episode groups also try
/// `<dir>/episodedetails.nfo`. Read/IO failures are silent `None` — the
/// resolver decides on NFO content (corrupt NFO → `NfoInvalid`, not fallthrough).
fn nfo_sidecar_xml(path: &std::path::Path, kind: MetadataKind) -> Option<String> {
    let mut candidates = vec![path.with_extension("nfo")];
    if kind == MetadataKind::Episode
        && let Some(dir) = path.parent()
    {
        candidates.push(dir.join("episodedetails.nfo"));
    }
    for c in candidates {
        if c.is_file()
            && let Ok(xml) = std::fs::read_to_string(&c)
        {
            return Some(xml);
        }
    }
    None
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::rate_limit::ApiRateLimiter;
    use crate::resolve::Resolver;
    use crate::tmdb::TmdbStub;
    use nightjar_db::migrate;
    use rusqlite::Connection;
    use std::sync::atomic::AtomicU64;

    fn seeded_movies() -> Connection {
        let c = Connection::open_in_memory().unwrap();
        migrate(&c).unwrap();
        c.execute_batch(
            "INSERT INTO libraries (name, path, kind) VALUES ('L', '/tmp/L', 'movies');
             INSERT INTO media_items (library_id, path, mtime_ms, size_bytes, title, kind)
             VALUES
               (1, '/tmp/L/old.mkv', 1, 1, 'Old Film', 'movie'),
               (1, '/tmp/L/new.mkv', 1, 1, 'New Film', 'movie');",
        )
        .unwrap();
        c
    }

    #[test]
    fn pending_groups_newest_id_first() {
        let c = seeded_movies();
        let proxy = snapshot_visible_proxy(&c).unwrap();
        let groups = pending_query_groups(&c, &proxy).unwrap();
        assert_eq!(groups.len(), 2);
        assert!(groups[0].max_id > groups[1].max_id);
    }

    #[test]
    fn drain_marks_stub_misses_unmatched_and_second_pass_is_empty() {
        let c = seeded_movies();
        let resolver = Resolver { tmdb: TmdbStub };
        let http_429 = AtomicU64::new(0);
        let http_requests = AtomicU64::new(0);
        let s1 = drain_pending(
            &c,
            &resolver,
            &http_429,
            &http_requests,
            DrainOptions::default(),
        )
        .unwrap();
        assert_eq!(s1.items_unmatched, 2);
        let pending: i64 = c
            .query_row(
                "SELECT COUNT(*) FROM media_items WHERE metadata_status = 'pending'",
                [],
                |r| r.get(0),
            )
            .unwrap();
        assert_eq!(pending, 0);
        let s2 = drain_pending(
            &c,
            &resolver,
            &http_429,
            &http_requests,
            DrainOptions::default(),
        )
        .unwrap();
        assert_eq!(s2.groups, 0);
        let _ = ApiRateLimiter::polite_default();
    }

    #[test]
    fn provider_error_leaves_rows_pending() {
        struct Boom;
        impl MetadataSource for Boom {
            fn resolve(
                &self,
                _: &ResolveInput,
            ) -> Result<crate::resolve::ProviderResult, crate::resolve::ResolveError> {
                Err(crate::resolve::ResolveError::Provider("timeout".into()))
            }
        }
        let c = seeded_movies();
        let resolver = Resolver { tmdb: Boom };
        let s = drain_pending(
            &c,
            &resolver,
            &AtomicU64::new(0),
            &AtomicU64::new(0),
            DrainOptions::default(),
        )
        .unwrap();
        assert_eq!(s.provider_errors, 2);
        assert_eq!(s.items_left_pending, 2);
        let pending: i64 = c
            .query_row(
                "SELECT COUNT(*) FROM media_items WHERE metadata_status = 'pending'",
                [],
                |r| r.get(0),
            )
            .unwrap();
        assert_eq!(pending, 2);
        let neg: i64 = c
            .query_row("SELECT COUNT(*) FROM metadata_negative_cache", [], |r| {
                r.get(0)
            })
            .unwrap();
        assert_eq!(neg, 0, "api_error must not write negative cache");
    }

    #[test]
    fn reserved_bands_sort_before_recently_added() {
        assert!(QueueBand::ContinueWatching < QueueBand::RecentlyAdded);
        assert!(QueueBand::Visible < QueueBand::RecentlyAdded);
        assert!(QueueBand::Search < QueueBand::RecentlyAdded);
        assert!(QueueBand::RecentlyAdded < QueueBand::Background);
    }

    #[test]
    fn exclude_list_drops_named_libs_from_visible_proxy() {
        let c = Connection::open_in_memory().unwrap();
        migrate(&c).unwrap();
        c.execute_batch(
            "INSERT INTO libraries (name, path, kind) VALUES
               ('Movies', '/m', 'movies'),
               ('DV2', '/dv2', 'movies');
             INSERT INTO media_items (library_id, path, mtime_ms, size_bytes, title, kind)
             VALUES
               (1, '/m/Alpha.mkv', 1, 1, 'Alpha', 'movie'),
               (2, '/dv2/Pattern.mkv', 1, 1, 'Patterns Of Nature', 'movie');",
        )
        .unwrap();
        let with = snapshot_visible_proxy_filtered(&c, 40, &[]).unwrap();
        assert_eq!(with.movie_unit_count(), 2);
        let excl = snapshot_visible_proxy_filtered(&c, 40, &["DV2"]).unwrap();
        assert_eq!(excl.movie_unit_count(), 1);
        assert_eq!(excl.units[0].item_ids, vec![1]);
    }

    #[test]
    fn visible_rank_is_over_all_items_not_pending_only() {
        let c = Connection::open_in_memory().unwrap();
        migrate(&c).unwrap();
        c.execute_batch(
            "INSERT INTO libraries (name, path, kind) VALUES ('M', '/tmp/M', 'movies');
             INSERT INTO media_items (library_id, path, mtime_ms, size_bytes, title, kind, metadata_status)
             VALUES
               (1, '/tmp/M/a.mkv', 1, 1, 'Alpha', 'movie', 'ready'),
               (1, '/tmp/M/b.mkv', 1, 1, 'Bravo', 'movie', 'pending'),
               (1, '/tmp/M/c.mkv', 1, 1, 'Charlie', 'movie', 'pending');",
        )
        .unwrap();
        // N=1 → only Alpha (ready) is Visible; Bravo must NOT become Visible.
        let proxy = snapshot_visible_proxy_n(&c, 1).unwrap();
        assert_eq!(proxy.units.len(), 1);
        assert_eq!(proxy.units[0].item_ids, vec![1]);
        let groups = pending_query_groups(&c, &proxy).unwrap();
        let bravo = groups.iter().find(|g| g.title == "Bravo").unwrap();
        let charlie = groups.iter().find(|g| g.title == "Charlie").unwrap();
        assert_eq!(bravo.band, QueueBand::RecentlyAdded);
        assert_eq!(charlie.band, QueueBand::RecentlyAdded);
    }

    #[test]
    fn shows_proxy_returns_distinct_shows_not_one_show_episodes() {
        let c = Connection::open_in_memory().unwrap();
        migrate(&c).unwrap();
        c.execute_batch(
            "INSERT INTO libraries (name, path, kind) VALUES ('S', '/tmp/S', 'shows');
             INSERT INTO media_items (library_id, path, mtime_ms, size_bytes, title, kind, season, episode)
             VALUES
               (1, '/tmp/S/Alpha/S01E01.mkv', 1, 1, 'Alpha', 'episode', 1, 1),
               (1, '/tmp/S/Alpha/S01E02.mkv', 1, 1, 'Alpha', 'episode', 1, 2),
               (1, '/tmp/S/Bravo/S01E01.mkv', 1, 1, 'Bravo', 'episode', 1, 1),
               (1, '/tmp/S/Charlie/S01E01.mkv', 1, 1, 'Charlie', 'episode', 1, 1);",
        )
        .unwrap();
        let proxy = snapshot_visible_proxy_n(&c, 2).unwrap();
        assert_eq!(proxy.units.len(), 2);
        assert!(proxy.units.iter().all(|u| !u.is_movie));
        let keys: HashSet<_> = proxy.units.iter().map(|u| u.unit_key.as_str()).collect();
        assert_eq!(keys.len(), 2, "two distinct show units");
        // Alpha has 2 episodes — if we wrongly ranked episodes, N=2 could be Alpha×2.
        let alpha = proxy.units.iter().find(|u| u.unit_key.contains("alpha"));
        if let Some(a) = alpha {
            assert_eq!(a.item_ids.len(), 2);
        }
    }

    #[test]
    fn one_visible_episode_promotes_whole_show_group() {
        let c = Connection::open_in_memory().unwrap();
        migrate(&c).unwrap();
        // N=1 → only Alpha is Visible (title sort). Both Alpha episodes pending.
        c.execute_batch(
            "INSERT INTO libraries (name, path, kind) VALUES ('S', '/tmp/S', 'shows');
             INSERT INTO media_items (library_id, path, mtime_ms, size_bytes, title, kind, season, episode)
             VALUES
               (1, '/tmp/S/Alpha/S01E01.mkv', 1, 1, 'Alpha', 'episode', 1, 1),
               (1, '/tmp/S/Alpha/S01E02.mkv', 1, 1, 'Alpha', 'episode', 1, 2),
               (1, '/tmp/S/Zulu/S01E01.mkv', 1, 1, 'Zulu', 'episode', 1, 1);",
        )
        .unwrap();
        let proxy = snapshot_visible_proxy_n(&c, 1).unwrap();
        assert_eq!(proxy.units.len(), 1);
        let groups = pending_query_groups(&c, &proxy).unwrap();
        let alpha = groups.iter().find(|g| g.title == "alpha").unwrap();
        let zulu = groups.iter().find(|g| g.title == "zulu").unwrap();
        assert_eq!(alpha.band, QueueBand::Visible);
        assert_eq!(alpha.item_ids.len(), 2);
        assert_eq!(zulu.band, QueueBand::RecentlyAdded);
        assert!(alpha.band < zulu.band);
    }

    #[test]
    fn empty_cw_and_search_do_not_change_ordering() {
        let c = seeded_movies();
        let proxy = snapshot_visible_proxy(&c).unwrap();
        assert!(continue_watching_item_ids(&c).is_empty());
        assert!(search_boost_item_ids(&c).is_empty());
        let groups = pending_query_groups(&c, &proxy).unwrap();
        // With N=40 and 2 movies, both are Visible — none CW/Search.
        assert!(groups.iter().all(|g| g.band == QueueBand::Visible));
        assert!(!groups.iter().any(|g| g.band == QueueBand::ContinueWatching));
        assert!(!groups.iter().any(|g| g.band == QueueBand::Search));
    }

    #[test]
    fn terminal_gate_accepts_unmatched_without_poster() {
        let c = seeded_movies();
        let resolver = Resolver { tmdb: TmdbStub };
        let s = drain_pending(
            &c,
            &resolver,
            &AtomicU64::new(0),
            &AtomicU64::new(0),
            DrainOptions {
                stop_when_visible_terminal: true,
                ..DrainOptions::default()
            },
        )
        .unwrap();
        assert!(s.stopped_early);
        assert!(s.t_first_screen_secs.is_some());
        assert_eq!(s.unmatched_in_proxy, s.visible_proxy_size);
        assert_eq!(s.ready_in_proxy, 0);
        assert_eq!(s.ready_missing_poster, 0);
        assert!(s.gate_pass, "unmatched-only proxy must pass poster check");
    }

    /// Season-returning double: prove bind path writes episode links (ADR-0029 §3).
    struct SeasonBindSource;

    impl MetadataSource for SeasonBindSource {
        fn resolve(
            &self,
            input: &crate::resolve::ResolveInput,
        ) -> Result<crate::resolve::ProviderResult, crate::resolve::ResolveError> {
            let title = input.title.as_deref().unwrap_or("");
            if title.is_empty() {
                return Ok(crate::resolve::ProviderResult::Miss);
            }
            let meta = CanonicalMetadata {
                kind: MetadataKind::Show,
                title: title.to_string(),
                original_title: None,
                year: Some(2020),
                air_date: None,
                plot: None,
                genres: Vec::new(),
                runtime_minutes: None,
                cast: Vec::new(),
                ratings: Vec::new(),
                ids: crate::model::ProviderIds {
                    tmdb: Some(99),
                    tmdb_show: Some(99),
                    imdb: None,
                    tvdb: None,
                },
                artwork: vec![crate::model::ArtworkRef {
                    kind: ArtworkKind::Poster,
                    path: "/p.jpg".into(),
                }],
                collection: None,
                season: None,
                episode: None,
            };
            let raw = crate::tmdb::RawProviderPayload {
                entity_kind: "tv".into(),
                provider_id: "99".into(),
                payload: r#"{"id":99,"name":"Alpha","first_air_date":"2020-01-01"}"#.into(),
            };
            Ok(crate::resolve::ProviderResult::Hit {
                metadata: Box::new(meta),
                method: "exact_title_year",
                raw: Some(raw),
            })
        }

        fn fetch_season(
            &self,
            show_id: i64,
            season_number: i32,
        ) -> Result<Option<crate::tmdb::RawProviderPayload>, crate::resolve::ResolveError> {
            assert_eq!(show_id, 99);
            assert_eq!(season_number, 1);
            let payload = r#"{
                "season_number": 1,
                "episodes": [
                  {"id": 1001, "name": "Pilot", "season_number": 1, "episode_number": 1, "air_date": "2020-01-01"},
                  {"id": 1002, "name": "Next", "season_number": 1, "episode_number": 2, "air_date": "2020-01-08"}
                ]
            }"#;
            Ok(Some(crate::tmdb::RawProviderPayload {
                entity_kind: "season".into(),
                provider_id: format!("{show_id}:{season_number}"),
                payload: payload.into(),
            }))
        }
    }

    /// S1 present on TMDB, S5 missing (404 → None): bind S1, skip S5, no hard error.
    struct PartialSeasonSource;

    impl MetadataSource for PartialSeasonSource {
        fn resolve(
            &self,
            input: &crate::resolve::ResolveInput,
        ) -> Result<crate::resolve::ProviderResult, crate::resolve::ResolveError> {
            let title = input.title.as_deref().unwrap_or("Beta");
            let meta = CanonicalMetadata {
                kind: MetadataKind::Show,
                title: title.to_string(),
                original_title: None,
                year: Some(1999),
                air_date: None,
                plot: None,
                genres: Vec::new(),
                runtime_minutes: None,
                cast: Vec::new(),
                ratings: Vec::new(),
                ids: crate::model::ProviderIds {
                    tmdb: Some(77),
                    tmdb_show: Some(77),
                    imdb: None,
                    tvdb: None,
                },
                artwork: Vec::new(),
                collection: None,
                season: None,
                episode: None,
            };
            Ok(crate::resolve::ProviderResult::Hit {
                metadata: Box::new(meta),
                method: "exact_title",
                raw: Some(crate::tmdb::RawProviderPayload {
                    entity_kind: "tv".into(),
                    provider_id: "77".into(),
                    payload: r#"{"id":77,"name":"Beta"}"#.into(),
                }),
            })
        }

        fn fetch_season(
            &self,
            show_id: i64,
            season_number: i32,
        ) -> Result<Option<crate::tmdb::RawProviderPayload>, crate::resolve::ResolveError> {
            assert_eq!(show_id, 77);
            if season_number == 5 {
                // TMDB has no this season — product must soft-skip.
                return Ok(None);
            }
            if season_number != 1 {
                return Ok(None);
            }
            let payload = r#"{
                "season_number": 1,
                "episodes": [
                  {"id": 2001, "name": "One", "season_number": 1, "episode_number": 1, "air_date": "1999-01-01"},
                  {"id": 2002, "name": "Two", "season_number": 1, "episode_number": 2, "air_date": "1999-01-08"}
                ]
            }"#;
            Ok(Some(crate::tmdb::RawProviderPayload {
                entity_kind: "season".into(),
                provider_id: format!("{show_id}:{season_number}"),
                payload: payload.into(),
            }))
        }
    }

    #[test]
    fn drain_binds_good_seasons_when_one_season_missing() {
        let c = Connection::open_in_memory().unwrap();
        migrate(&c).unwrap();
        c.execute_batch(
            "INSERT INTO libraries (name, path, kind) VALUES ('S', '/tmp/S', 'shows');
             INSERT INTO media_items (library_id, path, mtime_ms, size_bytes, title, kind, season, episode)
             VALUES
               (1, 'Beta/Season 01/Beta.S01E01.mkv', 1, 1, 'Beta', 'episode', 1, 1),
               (1, 'Beta/Season 01/Beta.S01E02.mkv', 1, 1, 'Beta', 'episode', 1, 2),
               (1, 'Beta/Season 05/Beta.S05E01.mkv', 1, 1, 'Beta', 'episode', 5, 1);",
        )
        .unwrap();
        let s = drain_pending(
            &c,
            &Resolver {
                tmdb: PartialSeasonSource,
            },
            &AtomicU64::new(0),
            &AtomicU64::new(0),
            DrainOptions::default(),
        )
        .unwrap();
        assert_eq!(s.items_ready, 3);
        assert_eq!(s.seasons_fetched, 1);
        assert_eq!(s.seasons_skipped, 1);
        assert_eq!(s.files_linked, 2);
        assert_eq!(s.bind_errors, 0);
        assert_eq!(s.episodes_projected, 2);

        // S01 files get episode keys; S05 keeps provisional tmdb:show for enrich id.
        let linked: i64 = c
            .query_row("SELECT COUNT(*) FROM media_item_links", [], |r| r.get(0))
            .unwrap();
        assert_eq!(linked, 3);
        let s5_ep: i64 = c
            .query_row(
                "SELECT COUNT(*) FROM media_item_links l
                 JOIN media_items m ON m.id = l.media_item_id
                 WHERE m.season = 5 AND l.item_key LIKE 'tmdb:episode:%'",
                [],
                |r| r.get(0),
            )
            .unwrap();
        assert_eq!(
            s5_ep, 0,
            "missing season stays without episode keys, not a hard error"
        );
    }

    #[test]
    fn drain_binds_episode_keys_when_source_returns_season() {
        let c = Connection::open_in_memory().unwrap();
        migrate(&c).unwrap();
        c.execute_batch(
            "INSERT INTO libraries (name, path, kind) VALUES ('S', '/tmp/S', 'shows');
             INSERT INTO media_items (library_id, path, mtime_ms, size_bytes, title, kind, season, episode)
             VALUES
               (1, 'Alpha/Season 01/Alpha.S01E01.mkv', 1, 1, 'Alpha', 'episode', 1, 1),
               (1, 'Alpha/Season 01/Alpha.S01E02.mkv', 1, 1, 'Alpha', 'episode', 1, 2);",
        )
        .unwrap();
        let resolver = Resolver {
            tmdb: SeasonBindSource,
        };
        let s = drain_pending(
            &c,
            &resolver,
            &AtomicU64::new(0),
            &AtomicU64::new(0),
            DrainOptions::default(),
        )
        .unwrap();
        assert_eq!(s.items_ready, 2);
        assert_eq!(s.seasons_fetched, 1);
        assert_eq!(s.episodes_projected, 2);
        assert_eq!(s.files_linked, 2);
        assert_eq!(s.seasons_skipped, 0);
        assert_eq!(s.bind_errors, 0);

        let keys: Vec<String> = c
            .prepare("SELECT item_key FROM media_item_links ORDER BY item_key")
            .unwrap()
            .query_map([], |r| r.get(0))
            .unwrap()
            .map(|r| r.unwrap())
            .collect();
        assert_eq!(
            keys,
            vec![
                "tmdb:episode:1001".to_string(),
                "tmdb:episode:1002".to_string()
            ]
        );

        let ep_canon: i64 = c
            .query_row(
                "SELECT COUNT(*) FROM metadata_canonical WHERE entity_kind = 'episode'",
                [],
                |r| r.get(0),
            )
            .unwrap();
        assert_eq!(ep_canon, 2);
    }

    #[test]
    fn visible_uses_tmdb_show_when_episode_linked() {
        let c = Connection::open_in_memory().unwrap();
        migrate(&c).unwrap();
        c.execute_batch(
            "INSERT INTO libraries (name, path, kind) VALUES ('S', '/tmp/S', 'shows');
             INSERT INTO media_items (library_id, path, mtime_ms, size_bytes, title, kind, season, episode)
             VALUES
               (1, 'Alpha/S01E01.mkv', 1, 1, 'Alpha', 'episode', 1, 1),
               (1, 'Alpha Alias/S01E01.mkv', 1, 1, 'Alpha Alias', 'episode', 1, 1);
             INSERT INTO metadata_canonical (
               provider, entity_kind, provider_id, title, ids_json, tmdb_show, projected_at
             ) VALUES
               ('tmdb', 'episode', '1001', 'Pilot', '{\"tmdb\":1001}', 55, '2026-01-01T00:00:00Z'),
               ('tmdb', 'episode', '1002', 'Pilot', '{\"tmdb\":1002}', 55, '2026-01-01T00:00:00Z');
             INSERT INTO media_item_links (media_item_id, item_key, manually_matched)
             VALUES (1, 'tmdb:episode:1001', 0), (2, 'tmdb:episode:1002', 0);",
        )
        .unwrap();
        let proxy = snapshot_visible_proxy_n(&c, 10).unwrap();
        assert_eq!(
            proxy.units.len(),
            1,
            "two soft keys collapse under one tmdb_show"
        );
        assert_eq!(proxy.units[0].unit_key, "tv|tmdb:55");
        assert_eq!(proxy.units[0].item_ids.len(), 2);
    }

    #[test]
    fn stub_source_skips_season_and_leaves_tv_unlinked() {
        let c = Connection::open_in_memory().unwrap();
        migrate(&c).unwrap();
        // Stub always misses — use SeasonBindSource without seasons... use a hit
        // source that returns None from fetch_season (trait default).
        struct HitNoSeason;
        impl MetadataSource for HitNoSeason {
            fn resolve(
                &self,
                _input: &crate::resolve::ResolveInput,
            ) -> Result<crate::resolve::ProviderResult, crate::resolve::ResolveError> {
                let meta = CanonicalMetadata {
                    kind: MetadataKind::Show,
                    title: "Alpha".into(),
                    original_title: None,
                    year: Some(2020),
                    air_date: None,
                    plot: None,
                    genres: Vec::new(),
                    runtime_minutes: None,
                    cast: Vec::new(),
                    ratings: Vec::new(),
                    ids: crate::model::ProviderIds {
                        tmdb: Some(1),
                        tmdb_show: Some(1),
                        imdb: None,
                        tvdb: None,
                    },
                    artwork: Vec::new(),
                    collection: None,
                    season: None,
                    episode: None,
                };
                Ok(crate::resolve::ProviderResult::Hit {
                    metadata: Box::new(meta),
                    method: "exact_title",
                    raw: Some(crate::tmdb::RawProviderPayload {
                        entity_kind: "tv".into(),
                        provider_id: "1".into(),
                        payload: r#"{"id":1,"name":"Alpha"}"#.into(),
                    }),
                })
            }
        }
        c.execute_batch(
            "INSERT INTO libraries (name, path, kind) VALUES ('S', '/tmp/S', 'shows');
             INSERT INTO media_items (library_id, path, mtime_ms, size_bytes, title, kind, season, episode)
             VALUES (1, 'Alpha/S01E01.mkv', 1, 1, 'Alpha', 'episode', 1, 1);",
        )
        .unwrap();
        let s = drain_pending(
            &c,
            &Resolver { tmdb: HitNoSeason },
            &AtomicU64::new(0),
            &AtomicU64::new(0),
            DrainOptions::default(),
        )
        .unwrap();
        assert_eq!(s.items_ready, 1);
        assert_eq!(s.seasons_skipped, 1);
        assert_eq!(s.files_linked, 0);
        // Provisional show handle for enrich id; not a watch key (path still effective).
        let n: i64 = c
            .query_row("SELECT COUNT(*) FROM media_item_links", [], |r| r.get(0))
            .unwrap();
        assert_eq!(n, 1);
        let key: String = c
            .query_row("SELECT item_key FROM media_item_links", [], |r| r.get(0))
            .unwrap();
        assert_eq!(key, "tmdb:show:1");
    }

    #[test]
    fn drain_search_lands_matched_then_enrich_ready() {
        use crate::resolve::ProviderResult;
        use std::sync::atomic::{AtomicUsize, Ordering};
        static SEARCHES: AtomicUsize = AtomicUsize::new(0);
        struct CountSource;
        impl MetadataSource for CountSource {
            fn resolve(
                &self,
                input: &ResolveInput,
            ) -> Result<ProviderResult, crate::resolve::ResolveError> {
                if input.tmdb_id.is_none() {
                    SEARCHES.fetch_add(1, Ordering::SeqCst);
                }
                let meta = CanonicalMetadata {
                    kind: MetadataKind::Movie,
                    title: "Hit".into(),
                    original_title: None,
                    year: Some(2020),
                    air_date: None,
                    plot: None,
                    genres: Vec::new(),
                    runtime_minutes: None,
                    cast: Vec::new(),
                    ratings: Vec::new(),
                    ids: crate::model::ProviderIds {
                        tmdb: Some(42),
                        tmdb_show: None,
                        imdb: None,
                        tvdb: None,
                    },
                    artwork: Vec::new(),
                    collection: None,
                    season: None,
                    episode: None,
                };
                Ok(ProviderResult::Hit {
                    metadata: Box::new(meta),
                    method: "exact_title",
                    raw: Some(crate::tmdb::RawProviderPayload {
                        entity_kind: "movie".into(),
                        provider_id: "42".into(),
                        payload: r#"{"id":42,"title":"Hit"}"#.into(),
                    }),
                })
            }
        }
        SEARCHES.store(0, Ordering::SeqCst);
        let c = Connection::open_in_memory().unwrap();
        migrate(&c).unwrap();
        c.execute_batch(
            "INSERT INTO libraries (name, path, kind) VALUES ('L', '/tmp/L', 'movies');
             INSERT INTO media_items (library_id, path, mtime_ms, size_bytes, title, kind)
             VALUES (1, '/tmp/L/hit.mkv', 1, 1, 'Hit', 'movie');",
        )
        .unwrap();
        let s = drain_pending(
            &c,
            &Resolver { tmdb: CountSource },
            &AtomicU64::new(0),
            &AtomicU64::new(0),
            DrainOptions::default(),
        )
        .unwrap();
        assert_eq!(s.items_matched, 1);
        assert_eq!(s.items_ready, 1);
        // One search resolve + one id enrich (no second search).
        assert_eq!(SEARCHES.load(Ordering::SeqCst), 1);
        let status: String = c
            .query_row("SELECT metadata_status FROM media_items", [], |r| r.get(0))
            .unwrap();
        assert_eq!(status, "ready");
    }

    #[test]
    fn drain_nfo_sidecar_lands_matched_without_search_then_enrich_ready() {
        use crate::resolve::ProviderResult;
        use std::sync::atomic::{AtomicUsize, Ordering};
        static NFO_TMDB_CALLS: AtomicUsize = AtomicUsize::new(0);
        struct NfoHit;
        impl MetadataSource for NfoHit {
            fn resolve(
                &self,
                input: &ResolveInput,
            ) -> Result<ProviderResult, crate::resolve::ResolveError> {
                // Search tier must never reach the provider: NFO resolves it.
                // Enrich must be id-driven and never see the NFO again.
                assert!(input.nfo_xml.is_none(), "enrich must not see NFO");
                assert!(input.tmdb_id.is_some(), "enrich must be id-driven");
                NFO_TMDB_CALLS.fetch_add(1, Ordering::SeqCst);
                let meta = CanonicalMetadata {
                    kind: MetadataKind::Movie,
                    title: "Hit".into(),
                    original_title: None,
                    year: Some(2020),
                    air_date: None,
                    plot: None,
                    genres: Vec::new(),
                    runtime_minutes: None,
                    cast: Vec::new(),
                    ratings: Vec::new(),
                    ids: crate::model::ProviderIds {
                        tmdb: Some(42),
                        tmdb_show: None,
                        imdb: None,
                        tvdb: None,
                    },
                    artwork: Vec::new(),
                    collection: None,
                    season: None,
                    episode: None,
                };
                Ok(ProviderResult::Hit {
                    metadata: Box::new(meta),
                    method: "exact_title",
                    raw: Some(crate::tmdb::RawProviderPayload {
                        entity_kind: "movie".into(),
                        provider_id: "42".into(),
                        payload: r#"{"id":42,"title":"Hit"}"#.into(),
                    }),
                })
            }
        }
        NFO_TMDB_CALLS.store(0, Ordering::SeqCst);
        // Real sidecar NFO beside a (not-required-to-exist) media file.
        let dir =
            std::env::temp_dir().join(format!("nightjar-nfo-sidecar-test-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        let media = dir.join("hit.mkv");
        std::fs::write(
            dir.join("hit.nfo"),
            r#"<?xml version="1.0" encoding="utf-8"?>
<movie><title>Hit</title><year>2020</year>
<uniqueid type="tmdb">42</uniqueid></movie>"#,
        )
        .unwrap();
        let c = Connection::open_in_memory().unwrap();
        migrate(&c).unwrap();
        c.execute_batch(&format!(
            "INSERT INTO libraries (name, path, kind) VALUES ('L', '{}', 'movies');
             INSERT INTO media_items (library_id, path, mtime_ms, size_bytes, title, kind)
             VALUES (1, '{}', 1, 1, 'Hit', 'movie');",
            dir.to_str().unwrap(),
            media.to_str().unwrap()
        ))
        .unwrap();
        let s = drain_pending(
            &c,
            &Resolver { tmdb: NfoHit },
            &AtomicU64::new(0),
            &AtomicU64::new(0),
            DrainOptions::default(),
        )
        .unwrap();
        // NFO matched at search tier with no provider call; enrich by id only.
        assert_eq!(NFO_TMDB_CALLS.load(Ordering::SeqCst), 1);
        assert_eq!(s.items_matched, 1);
        assert_eq!(s.items_ready, 1);
        let status: String = c
            .query_row("SELECT metadata_status FROM media_items", [], |r| r.get(0))
            .unwrap();
        assert_eq!(status, "ready");
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn drain_nfo_sidecar_resolves_library_relative_path() {
        use crate::resolve::ProviderResult;
        use std::sync::atomic::{AtomicUsize, Ordering};
        static REL_NFO_TMDB_CALLS: AtomicUsize = AtomicUsize::new(0);
        struct RelNfoHit;
        impl MetadataSource for RelNfoHit {
            fn resolve(
                &self,
                input: &ResolveInput,
            ) -> Result<ProviderResult, crate::resolve::ResolveError> {
                // Search tier must never reach the provider with a bare
                // (no-tmdb-id) query: the sidecar NFO resolves it from a
                // library-relative path. Enrich is id-driven and NFO-free.
                assert!(input.nfo_xml.is_none(), "enrich must not see NFO");
                assert!(input.tmdb_id.is_some(), "search tier reached provider");
                REL_NFO_TMDB_CALLS.fetch_add(1, Ordering::SeqCst);
                let meta = CanonicalMetadata {
                    kind: MetadataKind::Movie,
                    title: "Film".into(),
                    original_title: None,
                    year: Some(2021),
                    air_date: None,
                    plot: None,
                    genres: Vec::new(),
                    runtime_minutes: None,
                    cast: Vec::new(),
                    ratings: Vec::new(),
                    ids: crate::model::ProviderIds {
                        tmdb: Some(42),
                        tmdb_show: None,
                        imdb: None,
                        tvdb: None,
                    },
                    artwork: Vec::new(),
                    collection: None,
                    season: None,
                    episode: None,
                };
                Ok(ProviderResult::Hit {
                    metadata: Box::new(meta),
                    method: "exact_title",
                    raw: Some(crate::tmdb::RawProviderPayload {
                        entity_kind: "movie".into(),
                        provider_id: "42".into(),
                        payload: r#"{"id":42,"title":"Film"}"#.into(),
                    }),
                })
            }
        }
        REL_NFO_TMDB_CALLS.store(0, Ordering::SeqCst);
        // ADR-0030: media_items.path is *library-relative*. The library root
        // (not the CWD) is where the sidecar NFO lives.
        let root =
            std::env::temp_dir().join(format!("nightjar-nfo-relpath-test-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&root);
        std::fs::create_dir_all(&root).unwrap();
        std::fs::write(
            root.join("film.nfo"),
            r#"<?xml version="1.0" encoding="utf-8"?>
<movie><title>Film</title><year>2021</year>
<uniqueid type="tmdb">42</uniqueid></movie>"#,
        )
        .unwrap();
        let c = Connection::open_in_memory().unwrap();
        migrate(&c).unwrap();
        c.execute_batch(&format!(
            "INSERT INTO libraries (name, path, kind) VALUES ('L', '{}', 'movies');
             INSERT INTO media_items (library_id, path, mtime_ms, size_bytes, title, kind)
             VALUES (1, 'film.mkv', 1, 1, 'Film', 'movie');",
            root.to_str().unwrap()
        ))
        .unwrap();
        let s = drain_pending(
            &c,
            &Resolver { tmdb: RelNfoHit },
            &AtomicU64::new(0),
            &AtomicU64::new(0),
            DrainOptions::default(),
        )
        .unwrap();
        // NFO at <root>/film.nfo matched at search tier with no provider call;
        // enrich by id only (one provider call total).
        assert_eq!(REL_NFO_TMDB_CALLS.load(Ordering::SeqCst), 1);
        assert_eq!(s.items_matched, 1);
        assert_eq!(s.items_ready, 1);
        let status: String = c
            .query_row("SELECT metadata_status FROM media_items", [], |r| r.get(0))
            .unwrap();
        assert_eq!(status, "ready");
        let _ = std::fs::remove_dir_all(&root);
    }

    #[test]
    fn drain_calls_poster_warm_once_on_matched() {
        use crate::resolve::ProviderResult;
        use std::sync::atomic::{AtomicUsize, Ordering};
        static WARMED: AtomicUsize = AtomicUsize::new(0);
        struct Warm;
        impl PosterWarm for Warm {
            fn on_matched(&self, item_ids: &[i64], metadata: &CanonicalMetadata) {
                assert_eq!(metadata.title, "Hit");
                assert_eq!(item_ids.len(), 1);
                WARMED.fetch_add(1, Ordering::SeqCst);
            }
        }
        struct HitSource;
        impl MetadataSource for HitSource {
            fn resolve(
                &self,
                _input: &ResolveInput,
            ) -> Result<ProviderResult, crate::resolve::ResolveError> {
                let meta = CanonicalMetadata {
                    kind: MetadataKind::Movie,
                    title: "Hit".into(),
                    original_title: None,
                    year: Some(2020),
                    air_date: None,
                    plot: None,
                    genres: Vec::new(),
                    runtime_minutes: None,
                    cast: Vec::new(),
                    ratings: Vec::new(),
                    ids: crate::model::ProviderIds {
                        tmdb: Some(42),
                        tmdb_show: None,
                        imdb: None,
                        tvdb: None,
                    },
                    artwork: Vec::new(),
                    collection: None,
                    season: None,
                    episode: None,
                };
                Ok(ProviderResult::Hit {
                    metadata: Box::new(meta),
                    method: "exact_title",
                    raw: Some(crate::tmdb::RawProviderPayload {
                        entity_kind: "movie".into(),
                        provider_id: "42".into(),
                        payload: r#"{"id":42,"title":"Hit"}"#.into(),
                    }),
                })
            }
        }
        WARMED.store(0, Ordering::SeqCst);
        let c = seeded_movies();
        let opts = DrainOptions {
            poster_warm: Some(Box::new(Warm)),
            ..DrainOptions::default()
        };
        let s = drain_pending(
            &c,
            &Resolver { tmdb: HitSource },
            &AtomicU64::new(0),
            &AtomicU64::new(0),
            opts,
        )
        .unwrap();
        assert_eq!(s.items_matched, 2);
        // Once per matched group (two groups in `seeded_movies`), never on enrich.
        assert_eq!(WARMED.load(Ordering::SeqCst), 2);
    }
}
