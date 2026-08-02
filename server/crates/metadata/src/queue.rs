//! Metadata work queue as a query over `metadata_status` (ADR-0026 §8).
//!
//! No jobs table. Pending rows are selected, grouped by search `query_key`
//! (one provider resolve per group), ordered by band then `max_id DESC`.
//! Bands derive at query time — no priority column.

use std::collections::{HashMap, HashSet};
use std::time::Instant;

use rusqlite::{Connection, params};

use crate::canonical;
use crate::clean::{clean_movie_title, clean_show_title, series_library_year, year_from_path};
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
    Ready,
    Unmatched,
}

impl MetadataStatus {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Pending => "pending",
            Self::Ready => "ready",
            Self::Unmatched => "unmatched",
        }
    }

    pub fn parse(s: &str) -> Option<Self> {
        match s {
            "pending" => Some(Self::Pending),
            "ready" => Some(Self::Ready),
            "unmatched" => Some(Self::Unmatched),
            _ => None,
        }
    }

    pub fn is_terminal(self) -> bool {
        matches!(self, Self::Ready | Self::Unmatched)
    }
}

#[derive(Debug, Clone)]
pub struct PendingItem {
    pub id: i64,
    pub kind: String,
    pub title: String,
    pub year: Option<i32>,
    pub path: String,
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
                // Provisional: distinct show = resolve soft key (ADR-0026 §8).
                let mut by_show: HashMap<String, Vec<&LibraryItemRow>> = HashMap::new();
                for it in &items {
                    if it.kind != "episode" {
                        continue;
                    }
                    let (ct, _) = clean_show_title(&it.title);
                    let qk = query_key(&ct, None);
                    by_show.entry(format!("tv|{qk}")).or_default().push(it);
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
    title: String,
    year: Option<i32>,
    library_year: Option<i32>,
    library_episode_count: Option<u32>,
    library_season_count: Option<u32>,
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

/// Proxy progress: terminal when every item is ready|unmatched.
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
        let any_ready = statuses.contains(&MetadataStatus::Ready);
        if any_ready {
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

/// Load pending items and fold into resolve groups (band, then newest first).
fn pending_query_groups(
    conn: &Connection,
    visible: &VisibleProxy,
) -> Result<Vec<QueryGroup>, String> {
    let visible_ids = visible.item_id_set();
    let cw = continue_watching_item_ids(conn);
    let search = search_boost_item_ids(conn);

    let mut stmt = conn
        .prepare(
            "SELECT id, kind, title, year, path, season, episode
             FROM media_items
             WHERE metadata_status = 'pending'
             ORDER BY id DESC",
        )
        .map_err(|e| format!("prepare pending: {e}"))?;
    let rows = stmt
        .query_map([], |r| {
            Ok(PendingItem {
                id: r.get(0)?,
                kind: r.get(1)?,
                title: r.get(2)?,
                year: r.get(3)?,
                path: r.get(4)?,
                season: r.get(5)?,
                episode: r.get(6)?,
            })
        })
        .map_err(|e| format!("query pending: {e}"))?;

    let mut items = Vec::new();
    for row in rows {
        items.push(row.map_err(|e| format!("pending row: {e}"))?);
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
                        title: ct,
                        year: cy,
                        library_year: None,
                        library_episode_count: None,
                        library_season_count: None,
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
                let library_year = series_library_year(years, path0);
                let seasons: std::collections::HashSet<i32> =
                    siblings.iter().filter_map(|s| s.season).collect();
                let qk = query_key(&ct, None);
                let unit_key = format!("tv|{qk}");
                let g = groups
                    .entry(unit_key.clone())
                    .or_insert_with(|| QueryGroup {
                        resolve_kind: MetadataKind::Episode,
                        title: ct.clone(),
                        year: None,
                        library_year,
                        library_episode_count: Some(siblings.len() as u32),
                        library_season_count: (!seasons.is_empty()).then_some(seasons.len() as u32),
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

/// Write provider bindings (and season→episode projection when the source
/// supports `fetch_season`). TV files without a season fetch stay unbound
/// (derived path key — ADR-0029 §2 / §3). Stub sources skip seasons; that
/// path is wired but unproven until live season enqueue runs.
fn bind_resolved_items<T: MetadataSource>(
    conn: &Connection,
    resolver: &Resolver<T>,
    item_ids: &[i64],
    metadata: &CanonicalMetadata,
) -> Result<(), String> {
    match metadata.kind {
        MetadataKind::Movie => {
            let Some(key) = item_key_for_metadata(metadata) else {
                return Ok(());
            };
            let tx = conn
                .unchecked_transaction()
                .map_err(|e| format!("begin movie bind tx: {e}"))?;
            for id in item_ids {
                item_links::replace_auto_link(&tx, *id, &key)?;
            }
            tx.commit().map_err(|e| format!("commit movie bind: {e}"))?;
            Ok(())
        }
        MetadataKind::Show | MetadataKind::Episode => {
            let Some(show_id) = metadata.ids.tmdb.or(metadata.ids.tmdb_show) else {
                return Ok(());
            };
            let rows = episode_slots(conn, item_ids)?;
            let seasons: std::collections::HashSet<i32> =
                rows.iter().filter_map(|r| r.season).collect();
            if seasons.is_empty() {
                return Ok(());
            }
            let mut by_se: std::collections::HashMap<(i32, i32), i64> =
                std::collections::HashMap::new();
            for row in &rows {
                if let (Some(s), Some(e)) = (row.season, row.episode) {
                    by_se.insert((s, e), row.id);
                }
            }
            for sn in seasons {
                let Some(raw) = resolver
                    .tmdb
                    .fetch_season(show_id, sn)
                    .map_err(|e| e.to_string())?
                else {
                    // Stub / no season support — leave files unbound.
                    continue;
                };
                let eps = canonical::persist_season_projection(conn, PROVIDER_TMDB, show_id, &raw)?;
                let tx = conn
                    .unchecked_transaction()
                    .map_err(|e| format!("begin episode bind tx: {e}"))?;
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
                    item_links::replace_auto_link(&tx, *media_id, &key)?;
                }
                tx.commit()
                    .map_err(|e| format!("commit episode bind: {e}"))?;
            }
            Ok(())
        }
    }
}

struct EpisodeSlot {
    id: i64,
    season: Option<i32>,
    episode: Option<i32>,
}

fn episode_slots(conn: &Connection, ids: &[i64]) -> Result<Vec<EpisodeSlot>, String> {
    let mut out = Vec::with_capacity(ids.len());
    let mut stmt = conn
        .prepare("SELECT id, season, episode FROM media_items WHERE id = ?1")
        .map_err(|e| format!("prepare episode slots: {e}"))?;
    for id in ids {
        let row = stmt
            .query_row(params![id], |r| {
                Ok(EpisodeSlot {
                    id: r.get(0)?,
                    season: r.get(1)?,
                    episode: r.get(2)?,
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

/// Options for [`drain_pending`].
#[derive(Debug, Clone, Default)]
pub struct DrainOptions {
    /// Cap groups (short probes). Ignored when [`Self::stop_when_visible_terminal`].
    pub max_groups: Option<usize>,
    /// Snapshot Visible once; stop when every proxy unit is terminal.
    pub stop_when_visible_terminal: bool,
    /// Library names omitted from the Visible snapshot (measure excludes).
    pub exclude_library_names: Vec<String>,
}

/// Drain pending groups through the resolver (store + neg-cache + limiter).
///
/// Provider/`api_error` failures leave the group's rows **pending** and are
/// not written to the negative-result cache — a blip must not park the
/// library for a day. Genuine misses become `unmatched` (and may cache).
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
    let mut groups = pending_query_groups(conn, &proxy)?;
    if !opts.stop_when_visible_terminal
        && let Some(n) = opts.max_groups
    {
        groups.truncate(n);
    }

    let mut stats = DrainStats {
        groups: groups.len(),
        movie_groups: groups
            .iter()
            .filter(|g| g.resolve_kind == MetadataKind::Movie)
            .count(),
        show_groups: groups
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
    // Units already terminal before drain (ready with unknown poster → fail open false).
    for u in &proxy.units {
        let statuses = statuses_for_ids(conn, &u.item_ids)?;
        if statuses.iter().all(|s| s.is_terminal()) && statuses.contains(&MetadataStatus::Ready) {
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

    let mut resolved_groups = 0usize;
    for (i, g) in groups.iter().enumerate() {
        if (i + 1) % 50 == 0 || i + 1 == groups.len() {
            eprintln!("  queue {}/{} …", i + 1, groups.len());
        }
        let input = ResolveInput {
            title: Some(g.title.clone()),
            year: g.year,
            library_year: g.library_year,
            library_episode_count: g.library_episode_count,
            library_season_count: g.library_season_count,
            kind: Some(g.resolve_kind),
            ..Default::default()
        };
        stats.provider_resolves += 1;
        match resolver.resolve_with_store(&input, conn) {
            Ok(ResolveOutcome::Resolved { metadata, .. }) => {
                if let Err(e) = bind_resolved_items(conn, resolver, &g.item_ids, &metadata) {
                    eprintln!("  bind/season ({}): {e}", g.title);
                }
                set_metadata_status(conn, &g.item_ids, MetadataStatus::Ready)?;
                stats.items_ready += g.item_ids.len();
                let poster = has_poster(&metadata);
                unit_has_poster
                    .entry(g.unit_key.clone())
                    .and_modify(|p| *p = *p || poster)
                    .or_insert(poster);
            }
            Ok(ResolveOutcome::Unresolved { .. }) => {
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
                stats.movie_groups = groups[..resolved_groups]
                    .iter()
                    .filter(|g| g.resolve_kind == MetadataKind::Movie)
                    .count();
                stats.show_groups = groups[..resolved_groups]
                    .iter()
                    .filter(|g| g.resolve_kind == MetadataKind::Episode)
                    .count();
                break;
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
        let alpha = groups.iter().find(|g| g.title == "Alpha").unwrap();
        let zulu = groups.iter().find(|g| g.title == "Zulu").unwrap();
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
}
