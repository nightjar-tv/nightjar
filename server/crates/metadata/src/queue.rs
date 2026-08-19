//! Metadata work queue as a query over `metadata_status` (ADR-0026 §8).
//!
//! No jobs table. Pending rows are selected, grouped by search `query_key`
//! (one provider resolve per group), ordered by band then `max_id DESC`.
//! Bands derive at query time — no priority column.

use std::collections::{HashMap, HashSet};
use std::time::Instant;

use rusqlite::{Connection, OptionalExtension, params};

use nightjar_db::{resolve_media_path, show_folder_relpath};

use crate::canonical;
use crate::clean::{
    clean_movie_title, clean_show_title, pick_reference_episode, series_library_year,
    usable_episode_titles, year_from_path,
};
use crate::item_links;
use crate::match_score::{SeasonRangeMapping, UnplacedFile, map_unplaced_to_candidate_seasons};
use crate::model::{ArtworkKind, CanonicalMetadata, MetadataKind, item_key_for_metadata};
use crate::negative_cache::{PROVIDER_TMDB, query_key};
use crate::resolve::MetadataSource;
use crate::resolve::{MetadataOrigin, ResolveError, ResolveInput, ResolveOutcome, Resolver};

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
    pub library_id: i64,
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
    /// Library root `path` is relative to (ADR-0030).
    library_path: String,
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
            "SELECT m.id, m.library_id, l.kind, m.kind, m.title, m.year, m.path, l.path, l.name
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
                    library_path: r.get(7)?,
                },
                r.get::<_, String>(8)?,
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
                    // ADR-0026: the **filename** year outranks the folder's.
                    // Both are the library's own labelling, and where they
                    // disagree the filename is the one a renamer wrote from
                    // provider metadata while the folder is what a human typed
                    // once. Measured 2026-08-19 over 1,777 movies: 1,756 agree
                    // and the precedence decides nothing; 19 disagree, and in
                    // every one the filename year is the provider's. Two of
                    // those 19 were *bound to the wrong film* under the old
                    // order — `Peter Pan (2003)` holding the 77-minute 1953
                    // film, `Split (2016)` holding the 117-minute 2017 one —
                    // which no count reported, because a wrong bind is a
                    // success everywhere this project looks.
                    //
                    // Inert unless the library writes the year in both places:
                    // with one of them absent, `or` yields the same value
                    // either way round.
                    let folder_year = year_from_path(&m.path);
                    let (ct, cy) = clean_movie_title(&m.title, m.year.or(folder_year));
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

/// Browse unit for one episode file: `tv|tmdb:{show_id}` when linked or when
/// the folder has stored series identity (ADR-0033), else soft-key `tv|{query_key}`.
fn visible_show_unit_key(conn: &Connection, it: &LibraryItemRow) -> Result<String, String> {
    if let Some(show_id) = tmdb_show_for_media_item(conn, it.id)? {
        return Ok(format!("tv|tmdb:{show_id}"));
    }
    // Folder-scoped identity: a folder with a series row keys with its bound
    // siblings instead of folding to a shared soft key (two fold-colliding
    // folders never merge into one card, ADR-0033 Q2/Q3).
    let folder = show_folder_relpath(&it.path, &it.library_path);
    if let Some(show_id) = series_show_id_for_folder(conn, it.library_id, &folder)? {
        return Ok(format!("tv|tmdb:{show_id}"));
    }
    let (ct, _) = clean_show_title(&it.title);
    let qk = query_key(&ct, None);
    Ok(format!("tv|{qk}"))
}

/// ADR-0033: stored series identity for a show folder, or `None` when the
/// folder has no row yet (it will search fresh and write one on a match).
/// Shared with the manual-retry path (fix.rs) so both consumers resolve the
/// folder's identity through the same row lookup.
pub(crate) fn series_show_id_for_folder(
    conn: &Connection,
    library_id: i64,
    show_folder: &str,
) -> Result<Option<i64>, String> {
    conn.query_row(
        "SELECT tmdb_show_id FROM series WHERE library_id = ?1 AND relpath = ?2",
        params![library_id, show_folder],
        |r| r.get(0),
    )
    .optional()
    .map_err(|e| format!("series row lookup: {e}"))
}

/// ADR-0033: upsert the folder-keyed series row from a fresh TV match. A
/// re-match updates the row (the folder's identity follows its last accepted
/// match); nothing here runs inside a repair path.
fn upsert_series_row(conn: &Connection, g: &QueryGroup, show_id: i64) -> Result<(), String> {
    conn.execute(
        "INSERT INTO series (library_id, relpath, tmdb_show_id)
         VALUES (?1, ?2, ?3)
         ON CONFLICT(library_id, relpath) DO UPDATE SET tmdb_show_id = excluded.tmdb_show_id",
        params![g.library_id, g.show_folder, show_id],
    )
    .map_err(|e| format!("upsert series row: {e}"))?;
    Ok(())
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
    if let Some(key) = key
        && let Some(ep_id) = key.strip_prefix("tmdb:episode:")
    {
        let show: Option<i64> = conn
            .query_row(
                "SELECT tmdb_show FROM metadata_canonical
                 WHERE provider = 'tmdb' AND entity_kind = 'episode' AND provider_id = ?1",
                params![ep_id],
                |r| r.get(0),
            )
            .optional()
            .map_err(|e| format!("tmdb_show for visible: {e}"))?;
        if let Some(show) = show {
            return Ok(Some(show));
        }
    }
    // Unbound episode (widened-`unmatched` keeps `tmdb:show:{id}`, ADR-0026
    // §8.1): read the show link directly so the file keys with its bound
    // siblings instead of falling back to a soft key.
    let show_key: Option<String> = conn
        .query_row(
            "SELECT item_key FROM media_item_links
             WHERE media_item_id = ?1 AND item_key LIKE 'tmdb:show:%'
             ORDER BY manually_matched DESC, item_key
             LIMIT 1",
            params![media_item_id],
            |r| r.get(0),
        )
        .optional()
        .map_err(|e| format!("show link for visible: {e}"))?;
    if let Some(rest) = show_key.and_then(|k| k.strip_prefix("tmdb:show:").map(str::to_string))
        && let Ok(n) = rest.parse::<i64>()
    {
        return Ok(Some(n));
    }
    Ok(None)
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
    library_id: i64,
    /// ADR-0033 folder scope for TV groups: relpath of the show folder the
    /// group's episodes live under. `""` when the library root is itself the
    /// show folder. Movies never use it.
    show_folder: String,
    title: String,
    year: Option<i32>,
    library_year: Option<i32>,
    library_episode_count: Option<u32>,
    library_season_count: Option<u32>,
    /// `(season_number, file_count)` the folder holds, season 0 excluded.
    folder_season_counts: Vec<(i32, u32)>,
    library_seasons: Vec<i32>,
    ref_season: Option<i32>,
    ref_episode: Option<i32>,
    ref_episode_title: Option<String>,
    /// Every usable `(season, episode, title)` the folder supplies. The
    /// reference above is one of these; the stored-id confirmation needs all
    /// of them (issue 121).
    folder_episode_titles: Vec<(i32, i32, String)>,
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
    ///
    /// This is also the count of [`canonical::persist_season_projection`]
    /// attempts — it increments immediately before each call — so it is the
    /// denominator for [`Self::bind_errors`]. The 2026-08-07 overnight run read
    /// 2,056 here against 1,915 distinct stored season payloads; the excess is
    /// re-fetches collapsing onto the payload table's
    /// `(provider, entity_kind, provider_id)` primary key, not double counting.
    ///
    /// **It undercounts attempts exactly when they fail.** A bind that errors
    /// propagates out of [`bind_resolved_items`] with `?`, so the whole
    /// [`BindStats`] for that call is discarded — including seasons it had
    /// already fetched and projected before the failure. At `bind_errors == 0`
    /// the undercount is nil; on a run that errors it is material, and it
    /// biases the denominator downward precisely when you are trying to
    /// measure a failure rate.
    pub seasons_fetched: usize,
    /// Episode canonical rows projected from season payloads this drain.
    pub episodes_projected: usize,
    /// Media files that received at least one provider link this drain.
    pub files_linked: usize,
    /// Season fetches skipped (`fetch_season` → `None`, stub sources).
    pub seasons_skipped: usize,
    /// Bind failures (logged; items may still be marked ready).
    pub bind_errors: usize,
    /// Libraries skipped because their root is not reachable (ADR-0014).
    ///
    /// **A drain over missing media is indistinguishable from a drain over a
    /// library with no NFOs** — both report `errors = 0` and a plausible
    /// status distribution. Measured 2026-08-17: the same database drained with
    /// its media absent gave 1,054 unmatched against 155, with every NFO route
    /// silently replaced by title search. These two counters are what make that
    /// run identifiable after the fact instead of looking like a bad matching
    /// day.
    pub libraries_unreachable: usize,
    /// Items left `pending` because their library was skipped.
    pub items_in_unreachable: usize,
}

/// Why a file in a folder whose entity resolved received no episode link.
///
/// **Three tokens, each with a measured instance behind it.** ADR-0043 §2
/// predicted five bind-time reasons before any writer existed; classifying all
/// 117 items that reach `unmatched` with no computed cause found instances of
/// three and none of the other two. `no_scanned_number` and
/// `episode_not_projected` are deliberately absent rather than shipped empty —
/// a token with no instance is a guess, and §2's guesses were wrong twice.
///
/// **The variant order is load-bearing.** It is the order the bind asks the
/// questions in — season present? episode present? slot free? — and a file
/// covering an episode range can fail differently on each number it covers.
/// The derived `Ord` picks the furthest-along answer, which is the most
/// specific true thing about the file.
///
/// A file that reaches the end of the ordering records **nothing**. That
/// residue is the empty bucket that keeps the taxonomy honest: a null after
/// this means *a cause these three do not name*, which is a different claim
/// from today's null and is the signal a fourth cause has appeared.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub enum UnplacedCause {
    /// The folder's season is absent from the bound entity entirely — the
    /// season fetch returned nothing. Same fact `BindStats::seasons_skipped`
    /// counts; this is a second reader of it, not a second detection.
    SeasonOutOfRange,
    /// The season exists and was projected; this episode number is beyond
    /// what it holds.
    EpisodeOutOfRange,
    /// The slot exists and another file in the same folder already holds it.
    /// Records the cause and decides nothing — which file wins is ADR-0025's
    /// question, and `by_se` keeps the last as it always has.
    DuplicateSlot,
}

impl UnplacedCause {
    /// The stored token. Closed set, like `metadata_negative_cache.reason`.
    pub fn token(self) -> &'static str {
        match self {
            Self::SeasonOutOfRange => "season_out_of_range",
            Self::EpisodeOutOfRange => "episode_out_of_range",
            Self::DuplicateSlot => "duplicate_slot",
        }
    }
}

/// Counters from one [`bind_resolved_items`] call.
#[derive(Debug, Default, Clone, Copy)]
pub struct BindStats {
    /// Season fetches that reached [`canonical::persist_season_projection`].
    /// Discarded wholesale if this bind later returns `Err` — see
    /// [`DrainStats::seasons_fetched`].
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
            // `l.reachable = 1` is the whole of the reachability gate: a
            // library whose root is gone contributes no groups, so nothing in
            // it is resolved from evidence that is not there (ADR-0014).
            "SELECT m.id, m.kind, m.title, m.year, m.path, m.season, m.episode,
                    l.id as library_id, l.path
             FROM media_items m
             JOIN libraries l ON l.id = m.library_id
             WHERE m.metadata_status = ?1 AND l.reachable = 1
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
                library_path: r.get(8)?,
                season: r.get(5)?,
                episode: r.get(6)?,
                library_id: r.get(7)?,
            })
        })
        .map_err(|e| format!("query status groups: {e}"))?;

    let mut items = Vec::new();
    for row in rows {
        items.push(row.map_err(|e| format!("status group row: {e}"))?);
    }

    // ADR-0033 Q2: TV groups are folder-scoped. The show folder is the highest
    // directory under the library root that contains episodes or season
    // directories; `Season N/` and `Specials/` inherit it. Two folders that
    // fold to the same matcher key (`Shameless (US)` / `Shameless (UK)`) are
    // separate groups and never share identity (the D2 wrong-match class).
    let mut ep_by_show: HashMap<(i64, String), Vec<&PendingItem>> = HashMap::new();
    for it in &items {
        if it.kind == "episode" {
            let folder = show_folder_relpath(&it.path, &it.library_path);
            let (ct, _) = clean_show_title(&it.title);
            ep_by_show
                .entry(episode_group_key(it.library_id, &folder, &ct))
                .or_default()
                .push(it);
        }
    }

    // Stored folder series identity, loaded once so group unit keys follow
    // the folder's row (`tv|tmdb:{show_id}`) instead of a soft key — a
    // fold-colliding folder with identity keys with its own card.
    let series_by_folder = load_series_rows(conn)?;

    let mut movie_groups: HashMap<String, QueryGroup> = HashMap::new();
    let mut ep_groups: HashMap<(i64, String), QueryGroup> = HashMap::new();
    for it in &items {
        let band = band_for_item(it.id, &visible_ids, &cw, &search);
        match it.kind.as_str() {
            "movie" => {
                // Same precedence as the visible-proxy path above, and for
                // the same measured reason.
                let folder_year = year_from_path(&it.path);
                let (ct, cy) = clean_movie_title(&it.title, it.year.or(folder_year));
                let qk = query_key(&ct, cy);
                let unit_key = format!("movie|{qk}");
                let g = movie_groups
                    .entry(unit_key.clone())
                    .or_insert_with(|| QueryGroup {
                        resolve_kind: MetadataKind::Movie,
                        path: it.path.clone(),
                        library_path: it.library_path.clone(),
                        library_id: it.library_id,
                        show_folder: String::new(),
                        title: ct,
                        year: cy,
                        library_year: None,
                        library_episode_count: None,
                        library_season_count: None,
                        folder_season_counts: Vec::new(),
                        library_seasons: Vec::new(),
                        ref_season: None,
                        ref_episode: None,
                        ref_episode_title: None,
                        folder_episode_titles: Vec::new(),
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
                let show_folder = show_folder_relpath(&it.path, &it.library_path);
                // Two keys, deliberately. `group_key` decides what shares a
                // group; `folder_key` reads the folder's stored series identity
                // and stays keyed on the real relpath, because that is what the
                // `series` table holds.
                let group_key = episode_group_key(it.library_id, &show_folder, &ct);
                let folder_key = (it.library_id, show_folder.clone());
                let siblings = ep_by_show
                    .get(&group_key)
                    .map(|v| v.as_slice())
                    .unwrap_or(&[]);
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
                // The group's browse unit: the folder's stored series id when
                // it has one, else the soft key — matching
                // [`visible_show_unit_key`] so poster attribution lands.
                let unit_key = match series_by_folder.get(&folder_key) {
                    Some(show_id) => format!("tv|tmdb:{show_id}"),
                    None => format!("tv|{}", query_key(&ct, None)),
                };
                let g = ep_groups.entry(group_key).or_insert_with(|| QueryGroup {
                    resolve_kind: MetadataKind::Episode,
                    path: path0.to_string(),
                    library_path: library_path0.to_string(),
                    library_id: it.library_id,
                    show_folder: show_folder.clone(),
                    title: ct.clone(),
                    year: None,
                    library_year,
                    library_episode_count: Some(siblings.len() as u32),
                    library_season_count: (!seasons.is_empty()).then_some(seasons.len() as u32),
                    folder_season_counts: {
                        // Season 0 excluded, matching the candidate side: a
                        // `Specials` folder exists independently of whether a
                        // provider models one.
                        let mut per: std::collections::HashMap<i32, u32> =
                            std::collections::HashMap::new();
                        for sib in siblings {
                            if let Some(sn) = sib.season
                                && sn > 0
                            {
                                *per.entry(sn).or_insert(0) += 1;
                            }
                        }
                        let mut v: Vec<(i32, u32)> = per.into_iter().collect();
                        v.sort_by_key(|(s, _)| *s);
                        v
                    },
                    library_seasons: {
                        // Season 0 is excluded: a `Specials` folder exists
                        // independently of whether a provider models season 0,
                        // so requiring it would count correct candidates short.
                        let mut v: Vec<i32> = seasons.iter().copied().filter(|s| *s > 0).collect();
                        v.sort_unstable();
                        v
                    },
                    ref_season: pref.as_ref().map(|p| p.0),
                    ref_episode: pref.as_ref().map(|p| p.1),
                    ref_episode_title: pref.map(|p| p.2),
                    folder_episode_titles: folder_titles_from_db(
                        conn,
                        it.library_id,
                        &show_folder,
                        &ct,
                    )
                    .unwrap_or_default(),
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

    let mut out: Vec<QueryGroup> = movie_groups
        .into_values()
        .chain(ep_groups.into_values())
        .collect();
    out.sort_by(|a, b| a.band.cmp(&b.band).then_with(|| b.max_id.cmp(&a.max_id)));
    Ok(out)
}

/// ADR-0033: every folder-keyed series row, keyed `(library_id, relpath)`.
/// The key episode files group under.
///
/// Normally the show folder. ADR-0033 Q2 makes TV groups folder-scoped so that
/// two folders folding to the same matcher key — `Shameless (US)` and
/// `Shameless (UK)` — stay separate groups and never share one identity.
///
/// **A file directly in the library root has no show folder.**
/// [`show_folder_relpath`] returns `""` for it. An empty string is not a folder
/// that happens to be shared; it is the absence of one, and grouping on it
/// inverts the protection above into its opposite: every show in the root lands
/// in one bucket, and the bucket binds all of them to whichever entity won.
/// Everything derived from the bucket pools with it too — the premiere year, the
/// episode and season counts, and the reference episode.
///
/// The matcher oracle measures that directly. Its `tv.root` shape puts 20 shows
/// in one root and scores **2,553 of 4,148 measured rows bound to the wrong
/// entity**, against 150 correct.
///
/// So when there is no folder to scope by, scope by the title the filename
/// carries. This is what the browse side already does: [`visible_show_unit_key`]
/// falls back to the soft key `tv|{query_key}` when the folder has no series
/// row, so browse splits root-level files by title while grouping merged them.
/// The two now agree.
///
/// **It needs the basename to carry a title.** A shared root of `S01E01.mkv`
/// has neither a folder nor a title, so it still groups as one — the case
/// `resolve_episode_group`'s empty-title refusal already covers by declining to
/// search at all.
fn episode_group_key(library_id: i64, show_folder: &str, cleaned_title: &str) -> (i64, String) {
    if show_folder.is_empty() {
        // `\0` cannot appear in a relpath, so a synthesised key can never
        // collide with a real folder's.
        (library_id, format!("\0{}", query_key(cleaned_title, None)))
    } else {
        (library_id, show_folder.to_string())
    }
}

fn load_series_rows(conn: &Connection) -> Result<HashMap<(i64, String), i64>, String> {
    let mut stmt = conn
        .prepare("SELECT library_id, relpath, tmdb_show_id FROM series")
        .map_err(|e| format!("prepare series rows: {e}"))?;
    let rows = stmt
        .query_map([], |r| {
            Ok((
                r.get::<_, i64>(0)?,
                r.get::<_, String>(1)?,
                r.get::<_, i64>(2)?,
            ))
        })
        .map_err(|e| format!("query series rows: {e}"))?;
    let mut out = HashMap::new();
    for row in rows {
        let (library_id, relpath, tmdb_show_id) = row.map_err(|e| format!("series row: {e}"))?;
        out.insert((library_id, relpath), tmdb_show_id);
    }
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
///
/// `matched` requires an enrichable TMDB id: an episode-kind `ids.tmdb` is an
/// *episode* id and must never be written as `tmdb:show:`, and a hit with no
/// TMDB id at all must not land `matched` (no stored id → enrich dead-end).
/// Returns `false` when nothing enrichable was stored; callers keep the item
/// un-matched instead of parking it in a terminal `matched` without a key.
/// Record whether folder episode titles agreed with the chosen candidate.
///
/// Diagnostic only — nothing reads this to decide what happens next. It exists
/// so a measurement can ask "how often did confirmation agree with the route
/// that won" without the route token having to encode both facts at once.
fn record_episode_title_confirmation(
    conn: &Connection,
    item_ids: &[i64],
    confirmed: bool,
) -> Result<(), String> {
    if item_ids.is_empty() {
        return Ok(());
    }
    let tx = conn
        .unchecked_transaction()
        .map_err(|e| format!("begin confirmation tx: {e}"))?;
    {
        let mut stmt = tx
            .prepare(
                "UPDATE media_items
                    SET metadata_confirmed_by_episode_title = ?1
                  WHERE id = ?2",
            )
            .map_err(|e| format!("prepare confirmation update: {e}"))?;
        for id in item_ids {
            stmt.execute(params![i64::from(confirmed), id])
                .map_err(|e| format!("update confirmation {id}: {e}"))?;
        }
    }
    tx.commit()
        .map_err(|e| format!("commit confirmation: {e}"))?;
    Ok(())
}

fn apply_search_hit(
    conn: &Connection,
    item_ids: &[i64],
    metadata: &CanonicalMetadata,
    match_method: Option<&str>,
    confirmed: Option<bool>,
) -> Result<bool, String> {
    let tx = conn
        .unchecked_transaction()
        .map_err(|e| format!("begin search-hit tx: {e}"))?;
    let wrote = match metadata.kind {
        MetadataKind::Movie => {
            if let Some(movie_id) = metadata.ids.tmdb {
                let key = format!("tmdb:movie:{movie_id}");
                for id in item_ids {
                    item_links::replace_auto_link(&tx, *id, &key)?;
                }
                true
            } else {
                false
            }
        }
        MetadataKind::Show => {
            if let Some(show_id) = metadata.ids.tmdb.or(metadata.ids.tmdb_show) {
                let key = format!("tmdb:show:{show_id}");
                for id in item_ids {
                    item_links::replace_auto_link(&tx, *id, &key)?;
                }
                true
            } else {
                false
            }
        }
        MetadataKind::Episode => {
            // `ids.tmdb` on an episode NFO is an *episode* id — never a show
            // key. Only a real show id (`tmdb_show`) qualifies for enrich.
            if let Some(show_id) = metadata.ids.tmdb_show {
                let key = format!("tmdb:show:{show_id}");
                for id in item_ids {
                    item_links::replace_auto_link(&tx, *id, &key)?;
                }
                true
            } else {
                false
            }
        }
    };
    tx.commit().map_err(|e| format!("commit search-hit: {e}"))?;
    if wrote {
        // The search tier is where the entity is *chosen*, so this is the
        // route worth recording. Enrich resolves by the stored id afterwards
        // and always reports `tmdb_id`; without this write the column answers
        // "how did enrich resolve" rather than "how was this entity chosen",
        // and the twelve scorer tokens have no producer at all.
        set_metadata_status_recording(
            conn,
            item_ids,
            MetadataStatus::Matched,
            match_method.map_or(Decision::Uncomputed, Decision::Token),
        )?;
        // ADR-0032 as amended: the second field. `match_method` says what
        // selected the candidate; this says whether episode titles agreed with
        // that choice, and the two answer different questions. Only written
        // when there is an answer — a `None` leaves the column NULL, which is
        // "not evaluated", not "did not agree".
        if let Some(confirmed) = confirmed {
            record_episode_title_confirmation(conn, item_ids, confirmed)?;
        }
    }
    Ok(wrote)
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
    bind_resolved_items_classified(conn, resolver, item_ids, metadata).map(|(stats, _)| stats)
}

/// [`bind_resolved_items`], and why each file that received no link went
/// unplaced (ADR-0043 §2 as amended 2026-08-16).
///
/// **The classification runs where the bind already knows.** Every fact it
/// needs — which seasons the entity returned, which episode numbers it
/// projected, which file holds which slot — is already in hand here. Deriving
/// the same answers from stored state afterwards would be a second
/// implementation of the bind, free to disagree with it.
///
/// Diagnostic only: this changes no binding, no status and no request.
pub fn bind_resolved_items_classified<T: MetadataSource>(
    conn: &Connection,
    resolver: &Resolver<T>,
    item_ids: &[i64],
    metadata: &CanonicalMetadata,
) -> Result<(BindStats, HashMap<i64, UnplacedCause>), String> {
    let mut stats = BindStats::default();
    let mut unplaced: HashMap<i64, UnplacedCause> = HashMap::new();
    match metadata.kind {
        MetadataKind::Movie => {
            let Some(key) = item_key_for_metadata(metadata) else {
                return Ok((stats, unplaced));
            };
            let tx = conn
                .unchecked_transaction()
                .map_err(|e| format!("begin movie bind tx: {e}"))?;
            for id in item_ids {
                item_links::replace_auto_link(&tx, *id, &key)?;
                stats.files_linked += 1;
            }
            tx.commit().map_err(|e| format!("commit movie bind: {e}"))?;
            Ok((stats, unplaced))
        }
        MetadataKind::Show | MetadataKind::Episode => {
            let Some(show_id) = metadata.ids.tmdb.or(metadata.ids.tmdb_show) else {
                return Ok((stats, unplaced));
            };
            let rows = episode_slots(conn, item_ids)?;
            let seasons: std::collections::HashSet<i32> =
                rows.iter().filter_map(|r| r.season).collect();
            if seasons.is_empty() {
                // Every file parsed no season number. That is §2's
                // `no_scanned_number`, which measured zero instances — so it
                // is residue and records nothing, not a token shipped empty.
                return Ok((stats, unplaced));
            }
            // One file may cover several episode numbers (ADR-0025 §2 range),
            // and one slot may hold several files — the Bluray and the WEBRip
            // of the same episode are two files of one item, which §2 decided
            // and this used to discard by keeping whichever came last.
            let mut slot_members: std::collections::HashMap<(i32, i32), Vec<&EpisodeSlot>> =
                std::collections::HashMap::new();
            for row in &rows {
                for (s, e) in row.season_episodes() {
                    slot_members.entry((s, e)).or_default().push(row);
                }
            }
            // Extras are excluded here, once, so both the link site and the
            // classification below read the same answer.
            let by_se: std::collections::HashMap<(i32, i32), Vec<i64>> = slot_members
                .iter()
                .map(|(k, members)| (*k, slot_members_that_link(members)))
                .collect();
            // What the bind learns as it goes, kept for the classification
            // below. Not a second lookup — the same values, retained.
            let mut seasons_absent: std::collections::HashSet<i32> =
                std::collections::HashSet::new();
            let mut projected: std::collections::HashSet<(i32, i32)> =
                std::collections::HashSet::new();
            let mut linked: std::collections::HashSet<i64> = std::collections::HashSet::new();
            for sn in seasons {
                let Some(raw) = resolver
                    .tmdb
                    .fetch_season(show_id, sn)
                    .map_err(|e| e.to_string())?
                else {
                    // Stub, or TMDB 404 for this season number — skip and try others.
                    stats.seasons_skipped += 1;
                    seasons_absent.insert(sn);
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
                    projected.insert((s, e));
                    let Some(media_ids) = by_se.get(&(s, e)) else {
                        continue;
                    };
                    let Some(key) = item_key_for_metadata(ep) else {
                        continue;
                    };
                    for media_id in media_ids {
                        keys_by_media
                            .entry(*media_id)
                            .or_default()
                            .push(key.clone());
                    }
                }
                let tx = conn
                    .unchecked_transaction()
                    .map_err(|e| format!("begin episode bind tx: {e}"))?;
                for (media_id, keys) in &keys_by_media {
                    item_links::replace_auto_links(&tx, *media_id, keys)?;
                    stats.files_linked += 1;
                    linked.insert(*media_id);
                }
                tx.commit()
                    .map_err(|e| format!("commit episode bind: {e}"))?;
            }
            for row in &rows {
                if linked.contains(&row.id) {
                    continue;
                }
                let mut cause: Option<UnplacedCause> = None;
                for (s, e) in row.season_episodes() {
                    let this = if seasons_absent.contains(&s) {
                        UnplacedCause::SeasonOutOfRange
                    } else if !projected.contains(&(s, e)) {
                        UnplacedCause::EpisodeOutOfRange
                    } else if !by_se.get(&(s, e)).is_some_and(|ids| ids.contains(&row.id)) {
                        UnplacedCause::DuplicateSlot
                    } else {
                        // The slot projected, this file holds it, and still no
                        // link was written. None of the three names that, so
                        // record nothing — the residue is the signal.
                        continue;
                    };
                    cause = Some(cause.map_or(this, |prev| prev.max(this)));
                }
                if let Some(cause) = cause {
                    unplaced.insert(row.id, cause);
                }
            }
            Ok((stats, unplaced))
        }
    }
}

/// Every usable `(season, episode, title)` in a show folder, read from the
/// database rather than from the pending group.
///
/// **The group is not the folder.** `ep_by_show` collects only items awaiting
/// resolution, so a folder whose earlier seasons are already `ready` presents
/// the resolver with just the seasons being retried. `Monster (2022)` retried
/// its unmatched S2 and S3 and offered no S1 file at all — while the stored
/// entity, DAHMER, holds only S1. The confirmation then had nothing to compare
/// and the correct id was discarded anyway, which is the exact defect this is
/// meant to stop.
///
/// A local read. No provider request.
fn folder_titles_from_db(
    conn: &Connection,
    library_id: i64,
    show_folder: &str,
    show_soft_key: &str,
) -> Result<Vec<(i32, i32, String)>, String> {
    let like = if show_folder.is_empty() {
        "%".to_string()
    } else {
        format!("{show_folder}/%")
    };
    let mut stmt = conn
        .prepare(
            "SELECT path, season, episode FROM media_items
             WHERE library_id = ?1 AND kind = 'episode'
               AND season IS NOT NULL AND episode IS NOT NULL
               AND path LIKE ?2",
        )
        .map_err(|e| format!("prepare folder titles: {e}"))?;
    let rows: Vec<(String, i32, i32)> = stmt
        .query_map(params![library_id, like], |r| {
            Ok((r.get(0)?, r.get(1)?, r.get(2)?))
        })
        .map_err(|e| format!("query folder titles: {e}"))?
        .collect::<Result<_, _>>()
        .map_err(|e| format!("folder title row: {e}"))?;
    let basenames: Vec<(i32, i32, &str)> = rows
        .iter()
        .map(|(p, s, e)| {
            let base = std::path::Path::new(p)
                .file_name()
                .and_then(|n| n.to_str())
                .unwrap_or(p.as_str());
            (*s, *e, base)
        })
        .collect();
    Ok(usable_episode_titles(&basenames, show_soft_key))
}

/// Unlink files bound to an entity this folder does not bind (issue 121).
///
/// A file whose canonical episode belongs to an entity absent from the
/// folder's bindings is mis-linked — it says the folder holds an episode of a
/// show the folder is not bound to. `Monster (2022)` acquired 17 such links to
/// `Monster High` before the stored-id rescue existed.
///
/// **Unlinked, not rebound.** The item returns to `unmatched`, which is the
/// honest state and the bucket with three built consumers; choosing a
/// replacement is a matching decision this repair does not make.
///
/// **Every binding counts, not just the primary.** A folder that binds a
/// second entity under ADR-0046 holds that entity's episodes legitimately —
/// Will & Grace's 52 revival files are linked to 74321 and must survive.
fn unlink_foreign_entity_links(
    conn: &Connection,
    library_id: i64,
    show_folder: &str,
    item_ids: &[i64],
) -> Result<usize, String> {
    let bound: Vec<i64> = crate::series_bindings::for_folder(conn, library_id, show_folder)?
        .into_iter()
        .map(|b| b.tmdb_show_id)
        .collect();
    if bound.is_empty() {
        // No record of what this folder binds. Unlinking on that basis would
        // be destroying links on absence of evidence, which is the failure
        // this codebase keeps refusing.
        return Ok(0);
    }
    let mut stale = Vec::new();
    for id in item_ids {
        let mut stmt = conn
            .prepare(
                "SELECT c.tmdb_show FROM media_item_links l
                 JOIN metadata_canonical c
                   ON c.provider = ?1 AND c.entity_kind = 'episode'
                  AND l.item_key = 'tmdb:episode:' || c.provider_id
                 WHERE l.media_item_id = ?2 AND l.manually_matched = 0",
            )
            .map_err(|e| format!("prepare foreign link scan: {e}"))?;
        let shows: Vec<Option<i64>> = stmt
            .query_map(params![PROVIDER_TMDB, id], |r| r.get(0))
            .map_err(|e| format!("scan links: {e}"))?
            .collect::<Result<_, _>>()
            .map_err(|e| format!("scan link row: {e}"))?;
        if !shows.is_empty() && shows.iter().all(|s| s.is_some_and(|s| !bound.contains(&s))) {
            stale.push(*id);
        }
    }
    if stale.is_empty() {
        return Ok(0);
    }
    let tx = conn
        .unchecked_transaction()
        .map_err(|e| format!("begin unlink tx: {e}"))?;
    for id in &stale {
        item_links::clear_all_links_for_media_item(&tx, *id)?;
        tx.execute(
            "UPDATE media_items SET metadata_status = 'unmatched' WHERE id = ?1",
            params![id],
        )
        .map_err(|e| format!("reset status: {e}"))?;
    }
    tx.commit().map_err(|e| format!("commit unlink: {e}"))?;
    Ok(stale.len())
}

/// Counters from one [`bind_second_entities`] call.
#[derive(Debug, Default, Clone, Copy)]
pub struct SecondEntityStats {
    pub entities_bound: usize,
    pub files_linked: usize,
    /// Files still unplaced after every candidate was tested. Reported rather
    /// than swallowed: a folder needing three entities where the search finds
    /// two is a real outcome, and Monster is that folder.
    pub still_unplaced: usize,
}

/// The folder a second-entity search runs for.
#[derive(Debug, Clone, Copy)]
struct SecondEntityScope<'a> {
    library_id: i64,
    show_folder: &'a str,
    title: &'a str,
    soft_key: &'a str,
}

/// ADR-0046 item 4: after a bind leaves folder seasons unplaced, look for a
/// second entity that explains them.
///
/// Runs only when `seasons_skipped > 0` and only when the folder has no
/// secondary binding yet, so a rescan of a resolved folder issues **zero**
/// search requests — the property ADR-0033 item 1 protects and Gate 3
/// depends on.
///
/// **The primary binding is never touched.** A primary that moves is a
/// `series_key` change, restating the folder's identity to every consumer
/// keyed on it. Battlestar Galactica is the folder that tempts it — bound to
/// the two-episode miniseries while 70 files belong to the series — and it is
/// blocked on ADR-0046 item 5 rather than fixed here.
fn bind_second_entities<T: MetadataSource>(
    conn: &Connection,
    resolver: &Resolver<T>,
    folder: SecondEntityScope<'_>,
    item_ids: &[i64],
    primary_show_id: i64,
) -> Result<SecondEntityStats, String> {
    let SecondEntityScope {
        library_id,
        show_folder,
        title,
        soft_key,
    } = folder;
    let mut stats = SecondEntityStats::default();
    if crate::series_bindings::has_secondary(conn, library_id, show_folder)? {
        return Ok(stats);
    }
    let mut unplaced: Vec<(i64, UnplacedFile)> = Vec::new();
    for slot in episode_slots(conn, item_ids)? {
        let linked: Option<i64> = conn
            .query_row(
                "SELECT 1 FROM media_item_links
                 WHERE media_item_id = ?1 AND item_key LIKE 'tmdb:episode:%' LIMIT 1",
                params![slot.id],
                |r| r.get(0),
            )
            .optional()
            .map_err(|e| format!("check episode link: {e}"))?;
        if linked.is_some() {
            continue;
        }
        let (Some(season), Some(episode)) = (slot.season, slot.episode) else {
            continue;
        };
        let base = std::path::Path::new(&slot.path)
            .file_name()
            .and_then(|n| n.to_str())
            .unwrap_or(slot.path.as_str());
        let Some(t) = crate::clean::after_token_episode_title(base, season, episode) else {
            continue;
        };
        unplaced.push((
            slot.id,
            UnplacedFile {
                folder_season: season,
                episode,
                title: t,
            },
        ));
    }
    if unplaced.is_empty() {
        return Ok(stats);
    }
    stats.still_unplaced = unplaced.len();

    let candidates = resolver
        .tmdb
        .second_entity_candidates(title, primary_show_id)
        .map_err(|e| e.to_string())?;
    let files: Vec<UnplacedFile> = unplaced.iter().map(|(_, f)| f.clone()).collect();
    for cand in candidates {
        let Some(mapping) = map_unplaced_to_candidate_seasons(&cand.shape, &files, soft_key) else {
            // No title agreed. No binding — the files stay unmatched, which is
            // the bucket with three built consumers. Binding the best of a bad
            // set is the failure this refuses.
            continue;
        };
        crate::series_bindings::record_secondary(
            conn,
            library_id,
            show_folder,
            cand.tmdb_show_id,
            mapping.folder_seasons,
            mapping.entity_seasons,
        )?;
        stats.entities_bound += 1;
        let linked = link_files_to_entity(conn, resolver, cand.tmdb_show_id, &mapping, &unplaced)?;
        stats.files_linked += linked;
        stats.still_unplaced = stats.still_unplaced.saturating_sub(linked);
    }
    Ok(stats)
}

/// Project the entity's seasons and link the folder files that map onto them.
fn link_files_to_entity<T: MetadataSource>(
    conn: &Connection,
    resolver: &Resolver<T>,
    entity_id: i64,
    mapping: &SeasonRangeMapping,
    unplaced: &[(i64, UnplacedFile)],
) -> Result<usize, String> {
    let (flo, fhi) = mapping.folder_seasons;
    let (elo, ehi) = mapping.entity_seasons;
    let mut linked = 0usize;
    for entity_season in elo..=ehi {
        let Some(raw) = resolver
            .tmdb
            .fetch_season(entity_id, entity_season)
            .map_err(|e| e.to_string())?
        else {
            continue;
        };
        let eps = canonical::persist_season_projection(conn, PROVIDER_TMDB, entity_id, &raw)?;
        // The folder's number for this entity season, derived from the two
        // stored ranges rather than from an offset.
        let folder_season = flo + (entity_season - elo);
        if folder_season > fhi {
            continue;
        }
        let tx = conn
            .unchecked_transaction()
            .map_err(|e| format!("begin second-entity bind tx: {e}"))?;
        for ep in &eps {
            let (Some(es), Some(en)) = (ep.season, ep.episode) else {
                continue;
            };
            if es != entity_season {
                continue;
            }
            let Some(key) = item_key_for_metadata(ep) else {
                continue;
            };
            for (media_id, file) in unplaced {
                if file.folder_season == folder_season && file.episode == en {
                    item_links::replace_auto_links(&tx, *media_id, std::slice::from_ref(&key))?;
                    linked += 1;
                }
            }
        }
        tx.commit()
            .map_err(|e| format!("commit second-entity bind: {e}"))?;
    }
    Ok(linked)
}

struct EpisodeSlot {
    id: i64,
    season: Option<i32>,
    episode: Option<i32>,
    path: String,
    /// Probed runtime. `None` is **no evidence** and never excludes a file —
    /// see [`slot_members_that_link`].
    duration_ms: Option<i64>,
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

/// Which files in one season/episode slot are versions of that episode.
///
/// ADR-0025 §2 already decided this: *"Multiple versions of one film (4K remux +
/// 1080p, or Bluray + WEBDL) — one `item_key`, several media files."* The bind
/// did not implement it, keeping whichever file came last and leaving the rest
/// `unmatched`.
///
/// **The exception is extras.** A folder holds
/// `American Gigg-Olo - Cat Scene (Uncensored)` beside the episode: 48 seconds
/// against 22.9 minutes. Linking it as a version puts a clip in the episode's
/// `item_key`, and ADR-0022 §5's version selection is then free to play the clip
/// when someone asked for the episode. **That is playing the wrong thing** —
/// worse than the arbitrary-but-whole-episode file the bind kept before.
///
/// **Duration, not filename tokens.** Measured over the 30 files in this state:
/// versions sit at **0.98–1.02** of the longest file in their slot and the two
/// real extras at **0.50** and **0.03**, so any threshold in 0.55–0.95
/// separates them. Tokens would have been *less* accurate, not merely less
/// robust — `Family Guy - S20E17 - All About Alana.mkv` carries no scene marker
/// and is a genuine version.
///
/// **A file with no probed duration links.** `None` is no evidence, never a
/// verdict — the same one-directional rule as `compare_episode_title` and
/// `candidate_covers_folder_seasons`. A filter written the obvious way would
/// exclude it.
fn slot_members_that_link(members: &[&EpisodeSlot]) -> Vec<i64> {
    const MIN_RATIO: f64 = 0.75;
    // The reference is the longest file present, not the one the old code
    // happened to keep and not the provider's runtime: the former was
    // arbitrary, the latter is absent for some episodes and would cost a call.
    let longest = members
        .iter()
        .filter_map(|m| m.duration_ms)
        .filter(|d| *d > 0)
        .max();
    members
        .iter()
        .filter(|m| match (longest, m.duration_ms) {
            (Some(long), Some(d)) if d > 0 => (d as f64) / (long as f64) >= MIN_RATIO,
            _ => true,
        })
        .map(|m| m.id)
        .collect()
}

fn episode_slots(conn: &Connection, ids: &[i64]) -> Result<Vec<EpisodeSlot>, String> {
    let mut out = Vec::with_capacity(ids.len());
    let mut stmt = conn
        .prepare("SELECT id, season, episode, path, duration_ms FROM media_items WHERE id = ?1")
        .map_err(|e| format!("prepare episode slots: {e}"))?;
    for id in ids {
        let row = stmt
            .query_row(params![id], |r| {
                Ok(EpisodeSlot {
                    id: r.get(0)?,
                    season: r.get(1)?,
                    episode: r.get(2)?,
                    path: r.get(3)?,
                    duration_ms: r.get(4)?,
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
    set_metadata_status_only(conn, ids, status)
}

/// Status only, both decision columns untouched.
fn set_metadata_status_only(
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

/// What to record about a status change (ADR-0043 §2).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Decision<'a> {
    /// This decision has a known cause or route. It goes to the column the
    /// status calls for; the other is cleared.
    Token(&'a str),
    /// Nothing computes a cause for this decision — the honest state for every
    /// bind-time exit today. Clears both.
    Uncomputed,
    /// The status moved but **the entity was not re-chosen**, so whatever route
    /// is already recorded still describes how it was chosen. Leaves both
    /// columns alone.
    ///
    /// This is what enrich-by-stored-id is: it resolves details for an id the
    /// search tier already picked. Overwriting with `tmdb_id` there is what
    /// made the column answer "how did enrich resolve" instead of "how was this
    /// entity chosen".
    KeepRoute,
}

/// Set the status and record **why**, in the same statement (ADR-0043 §2).
///
/// An item never carries both a failure reason and a success route.
///
/// **Diagnostic, never control flow.** Nothing reads either column to decide
/// what happens next.
pub fn set_metadata_status_recording(
    conn: &Connection,
    ids: &[i64],
    status: MetadataStatus,
    decision: Decision<'_>,
) -> Result<(), String> {
    if ids.is_empty() {
        return Ok(());
    }
    if decision == Decision::KeepRoute {
        return set_metadata_status_only(conn, ids, status);
    }
    let token = match decision {
        Decision::Token(t) => Some(t),
        _ => None,
    };
    let sql = if status == MetadataStatus::Unmatched {
        "UPDATE media_items
            SET metadata_status = ?1,
                metadata_unmatched_reason = ?2,
                metadata_match_method = NULL
          WHERE id = ?3"
    } else {
        "UPDATE media_items
            SET metadata_status = ?1,
                metadata_match_method = ?2,
                metadata_unmatched_reason = NULL
          WHERE id = ?3"
    };
    let tx = conn
        .unchecked_transaction()
        .map_err(|e| format!("begin status tx: {e}"))?;
    {
        let mut stmt = tx
            .prepare(sql)
            .map_err(|e| format!("prepare status update: {e}"))?;
        for id in ids {
            stmt.execute(params![status.as_str(), token, id])
                .map_err(|e| format!("update status {id}: {e}"))?;
        }
    }
    tx.commit().map_err(|e| format!("commit status: {e}"))?;
    Ok(())
}

/// Poster warm hook fired on the search-tier → `matched` transition
/// (ADR-0026 §8). The crate default is a no-op so the queue has no I/O
/// dependency; the product drain wires a store-backed implementation via
/// [`crate::ArtworkStore`] (ADR-0027 §5). Never blocks the drain.
pub trait PosterWarm: Send + Sync {
    /// `item_ids` all landed `matched` with the same `metadata`.
    fn on_matched(&self, item_ids: &[i64], metadata: &CanonicalMetadata);
}

/// Default: nothing to warm (product wires the store-backed hook).
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
/// Fairness order (ADR-0026 §8.5, v1 constant):
/// 1. Search Visible (and CW)  2. Enrich Visible
/// 3. Search background        4. Enrich background
///
/// Provider/`api_error` failures leave rows **pending** (search) or
/// **matched** (enrich) and are not negative-cached.
///
/// Sidecar NFO (Kodi layout) feeds the search tier only, from the ordered
/// candidate list in [`nfo_sidecar_xml`]: same-stem `.nfo` beside the media
/// file, then `movie.nfo` / `<foldername>.nfo` for movies or
/// `episodedetails.nfo` for episode groups.
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

    let mut stats = DrainStats {
        visible_proxy_size: proxy.units.len(),
        proxy_movie_units: proxy.movie_unit_count(),
        proxy_show_units: proxy.show_unit_count(),
        predicted_secs: T_FIRST_SCREEN_PREDICTED_SECS * (proxy.units.len() as f64 / 80.0),
        ..DrainStats::default()
    };

    // ADR-0014: a library whose root is unreachable is skipped, not resolved
    // from whatever evidence survives. `request_scan` already refuses with
    // "library path is not reachable"; the drain had no equivalent and would
    // read no NFO, resolve by title search instead, and report `errors = 0`.
    //
    // **Refuse rather than degrade.** Items stay `pending`, which is the
    // honest state for "not attempted" and is what the next drain retries;
    // resolving them against a library that is not there produces terminal
    // statuses derived from absent evidence.
    //
    // This reads `libraries.reachable`, the record the scanner already
    // maintains, rather than probing the path here — Rule 4.11, and a probe in
    // this path would need its own timeout for a hung network mount.
    let unreachable: Vec<i64> = {
        let mut stmt = conn
            .prepare("SELECT id FROM libraries WHERE reachable = 0")
            .map_err(|e| format!("prepare reachability: {e}"))?;
        let ids: Vec<i64> = stmt
            .query_map([], |r| r.get(0))
            .map_err(|e| format!("query reachability: {e}"))?
            .collect::<Result<_, _>>()
            .map_err(|e| format!("reachability row: {e}"))?;
        ids
    };
    if !unreachable.is_empty() {
        stats.libraries_unreachable = unreachable.len();
        for id in &unreachable {
            let n: i64 = conn
                .query_row(
                    "SELECT COUNT(*) FROM media_items
                      WHERE library_id = ?1 AND metadata_status = 'pending'",
                    params![id],
                    |r| r.get(0),
                )
                .unwrap_or(0);
            stats.items_in_unreachable += n as usize;
            let path: String = conn
                .query_row(
                    "SELECT path FROM libraries WHERE id = ?1",
                    params![id],
                    |r| r.get(0),
                )
                .unwrap_or_default();
            eprintln!(
                "  skipping library {id} — path is not reachable: {path} ({n} item(s) left pending)"
            );
        }
    }

    let mut unit_has_poster: HashMap<String, bool> = HashMap::new();
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
            stats.http_429 = http_429.load(std::sync::atomic::Ordering::Relaxed);
            stats.http_requests = http_requests.load(std::sync::atomic::Ordering::Relaxed);
            return Ok(stats);
        }
    }

    let mut budget = if opts.stop_when_visible_terminal {
        None
    } else {
        opts.max_groups
    };
    let mut stopped_on_visible = false;
    let mut groups_done = 0usize;

    let take = |budget: &mut Option<usize>| -> bool {
        match budget {
            None => true,
            Some(0) => false,
            Some(n) => {
                *n -= 1;
                true
            }
        }
    };

    let is_front = |b: QueueBand| b <= QueueBand::Visible;

    // --- 1. Search Visible ---
    let all_search = pending_query_groups(conn, &proxy)?;
    let (vis_search, bg_search): (Vec<_>, Vec<_>) =
        all_search.into_iter().partition(|g| is_front(g.band));

    for g in &vis_search {
        if !take(&mut budget) {
            break;
        }
        search_one_group(conn, resolver, g, &opts, &mut stats, &mut unit_has_poster)?;
        groups_done += 1;
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
                stopped_on_visible = true;
                break;
            }
        }
    }

    // --- 2. Enrich Visible (before any background search) ---
    if !stopped_on_visible {
        let vis_enrich: Vec<_> = matched_query_groups(conn, &proxy)?
            .into_iter()
            .filter(|g| is_front(g.band))
            .collect();
        for g in &vis_enrich {
            if !take(&mut budget) {
                break;
            }
            enrich_one_group(conn, resolver, g, &mut stats, &mut unit_has_poster)?;
            groups_done += 1;
        }
    }

    // --- 3. Search background ---
    if !stopped_on_visible {
        for g in &bg_search {
            if !take(&mut budget) {
                break;
            }
            search_one_group(conn, resolver, g, &opts, &mut stats, &mut unit_has_poster)?;
            groups_done += 1;
        }
    }

    // --- 4. Enrich background ---
    if !stopped_on_visible {
        let bg_enrich: Vec<_> = matched_query_groups(conn, &proxy)?
            .into_iter()
            .filter(|g| !is_front(g.band))
            .collect();
        for g in &bg_enrich {
            if !take(&mut budget) {
                break;
            }
            enrich_one_group(conn, resolver, g, &mut stats, &mut unit_has_poster)?;
            groups_done += 1;
        }
    }

    stats.groups = groups_done;
    // movie/show group counts approximate from work done (re-query not needed for stats).
    if stats.movie_groups == 0 && stats.show_groups == 0 {
        // leave zero if nothing processed; helpers bump items_* only
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

fn search_one_group<T: MetadataSource>(
    conn: &Connection,
    resolver: &Resolver<T>,
    g: &QueryGroup,
    opts: &DrainOptions,
    stats: &mut DrainStats,
    unit_has_poster: &mut HashMap<String, bool>,
) -> Result<(), String> {
    if g.resolve_kind == MetadataKind::Movie {
        stats.movie_groups += 1;
    } else {
        stats.show_groups += 1;
    }
    let media_path = resolve_media_path(&g.library_path, &g.path);
    // TV search year is the folder year (earliest episode year, else
    // show-folder `(YYYY)`), mapped to `first_air_date_year`. It also enters
    // the neg-cache key, so a yearless miss (`top gear|-`) re-searches under
    // the year-keyed row — the intended re-search trigger.
    let search_year = match g.resolve_kind {
        MetadataKind::Movie => g.year,
        MetadataKind::Episode | MetadataKind::Show => g.year.or(g.library_year),
    };
    let input = ResolveInput {
        nfo_xml: nfo_sidecar_xml(&media_path, g.resolve_kind, &g.library_path),
        tvshow_nfo_xml: match g.resolve_kind {
            // tvshow.nfo is series identity — TV groups only.
            MetadataKind::Movie => None,
            MetadataKind::Episode | MetadataKind::Show => {
                show_root_nfo_xml(&media_path, &g.library_path)
            }
        },
        title: Some(g.title.clone()),
        year: search_year,
        library_year: g.library_year,
        library_episode_count: g.library_episode_count,
        library_season_count: g.library_season_count,
        folder_season_counts: g.folder_season_counts.clone(),
        library_seasons: g.library_seasons.clone(),
        ref_season: g.ref_season,
        ref_episode: g.ref_episode,
        ref_episode_title: g.ref_episode_title.clone(),
        folder_episode_titles: g.folder_episode_titles.clone(),
        // ADR-0033: the folder's stored series identity, when it has one.
        // The resolver cross-checks it against the persisted detail and binds
        // with zero provider calls; disagreement falls through to search.
        series_show_id: match g.resolve_kind {
            MetadataKind::Movie => None,
            MetadataKind::Episode | MetadataKind::Show => {
                series_show_id_for_folder(conn, g.library_id, &g.show_folder)?
            }
        },
        kind: Some(g.resolve_kind),
        ..Default::default()
    };
    stats.provider_resolves += 1;
    match resolver.resolve_with_store(&input, conn) {
        Ok(ResolveOutcome::Resolved {
            metadata,
            source,
            match_method,
            confirmed,
            ..
        }) => {
            let tmdb_id = metadata.ids.tmdb.or(metadata.ids.tmdb_show);
            eprintln!(
                "  match {} → tmdb:{:?} method={} (search tier)",
                g.title,
                tmdb_id,
                match_method.as_deref().unwrap_or("?")
            );
            // Complete NFO movie: skip TMDB entirely, persist and go Ready.
            if source == MetadataOrigin::Nfo && metadata.is_nfo_complete() {
                persist_nfo_ready_and_link(conn, &g.item_ids, &metadata)?;
                warm_poster_for_matched(opts.poster_warm.as_deref(), &g.item_ids, &metadata);
                stats.items_ready += g.item_ids.len();
                let poster = has_poster(&metadata);
                unit_has_poster
                    .entry(g.unit_key.clone())
                    .and_modify(|p| *p = *p || poster)
                    .or_insert(poster);
            } else {
                match apply_search_hit(
                    conn,
                    &g.item_ids,
                    &metadata,
                    match_method.as_deref(),
                    confirmed,
                ) {
                    Ok(true) => {
                        // ADR-0033: a fresh TV match writes the folder-keyed
                        // series row, so a later group under this folder binds
                        // with zero search calls through the same hit path.
                        if g.resolve_kind != MetadataKind::Movie
                            && let Some(show_id) = tmdb_id
                        {
                            upsert_series_row(conn, g, show_id)?;
                        }
                        warm_poster_for_matched(
                            opts.poster_warm.as_deref(),
                            &g.item_ids,
                            &metadata,
                        );
                        stats.items_matched += g.item_ids.len();
                        let poster = has_poster(&metadata);
                        unit_has_poster
                            .entry(g.unit_key.clone())
                            .and_modify(|p| *p = *p || poster)
                            .or_insert(poster);
                    }
                    Ok(false) => {
                        // Resolved but nothing enrichable stored — never park
                        // the group in `matched` without a stored id.
                        eprintln!(
                            "  unmatched {} — resolved without usable tmdb id (no stored key)",
                            g.title
                        );
                        set_metadata_status_recording(
                            conn,
                            &g.item_ids,
                            MetadataStatus::Unmatched,
                            Decision::Token("no_usable_provider_id"),
                        )?;
                        stats.items_unmatched += g.item_ids.len();
                    }
                    Err(e) => return Err(e),
                }
            }
        }
        Ok(ResolveOutcome::Unresolved { reason, .. }) => {
            eprintln!("  unmatched {} reason={reason:?}", g.title);
            set_metadata_status_recording(
                conn,
                &g.item_ids,
                MetadataStatus::Unmatched,
                Decision::Token(reason.token()),
            )?;
            stats.items_unmatched += g.item_ids.len();
        }
        Err(e) => {
            eprintln!("  provider error (left pending): {} — {e}", g.title);
            stats.provider_errors += 1;
            stats.items_left_pending += g.item_ids.len();
        }
    }
    Ok(())
}

/// Persist NFO-originated canonical metadata, write item links, and set
/// status to Ready — all in one transaction. Used when a complete NFO
/// makes a TMDB detail call unnecessary.
fn persist_nfo_ready_and_link(
    conn: &Connection,
    item_ids: &[i64],
    metadata: &CanonicalMetadata,
) -> Result<(), String> {
    let tx = conn
        .unchecked_transaction()
        .map_err(|e| format!("begin nfo ready tx: {e}"))?;
    canonical::upsert_canonical(&tx, PROVIDER_TMDB, metadata)?;
    for id in item_ids {
        if let Some(key) = item_key_for_metadata(metadata) {
            item_links::replace_auto_link(&tx, *id, &key)?;
        }
    }
    {
        // A third status-write site, and it records its route like the other
        // two. It is inline rather than calling the shared writer because it
        // shares this function's transaction — the whole point of the path is
        // that identity, links and status land together.
        let mut stmt = tx
            .prepare(
                "UPDATE media_items
                    SET metadata_status = ?1,
                        metadata_match_method = 'nfo_complete',
                        metadata_unmatched_reason = NULL
                  WHERE id = ?2",
            )
            .map_err(|e| format!("prepare status update: {e}"))?;
        for id in item_ids {
            stmt.execute(params![MetadataStatus::Ready.as_str(), id])
                .map_err(|e| format!("update status {id}: {e}"))?;
        }
    }
    tx.commit().map_err(|e| format!("commit nfo ready: {e}"))?;
    Ok(())
}

fn enrich_one_group<T: MetadataSource>(
    conn: &Connection,
    resolver: &Resolver<T>,
    g: &QueryGroup,
    stats: &mut DrainStats,
    unit_has_poster: &mut HashMap<String, bool>,
) -> Result<(), String> {
    if g.resolve_kind == MetadataKind::Movie {
        stats.movie_groups += 1;
    } else {
        stats.show_groups += 1;
    }
    let Some((tmdb_id, id_kind)) = tmdb_id_from_links(conn, &g.item_ids)? else {
        eprintln!(
            "  enrich skip {} — no stored tmdb id (left matched)",
            g.title
        );
        return Ok(());
    };
    let kind = match g.resolve_kind {
        MetadataKind::Movie => MetadataKind::Movie,
        MetadataKind::Episode | MetadataKind::Show => id_kind,
    };

    // Load NFO sidecar — enrich may merge NFO data over TMDB detail.
    let nfo = nfo_sidecar_meta(
        &resolve_media_path(&g.library_path, &g.path),
        g.resolve_kind,
        &g.library_path,
    );

    // Complete NFO (movie): skip TMDB detail entirely.
    if let Some(ref nfo_meta) = nfo
        && nfo_meta.is_nfo_complete()
    {
        eprintln!("  enrich {} → nfo complete (skip tmdb:{tmdb_id})", g.title);
        persist_nfo_ready_and_link(conn, &g.item_ids, nfo_meta)?;
        stats.items_ready += g.item_ids.len();
        let poster = has_poster(nfo_meta);
        unit_has_poster
            .entry(g.unit_key.clone())
            .and_modify(|p| *p = *p || poster)
            .or_insert(poster);
        return Ok(());
    }

    let input = ResolveInput {
        tmdb_id: Some(tmdb_id),
        kind: Some(kind),
        title: Some(g.title.clone()),
        year: g.year,
        library_year: g.library_year,
        library_episode_count: g.library_episode_count,
        library_season_count: g.library_season_count,
        folder_season_counts: g.folder_season_counts.clone(),
        library_seasons: g.library_seasons.clone(),
        ref_season: g.ref_season,
        ref_episode: g.ref_episode,
        ref_episode_title: g.ref_episode_title.clone(),
        ..Default::default()
    };
    stats.provider_resolves += 1;
    match resolver.resolve_with_store(&input, conn) {
        // The enrich tier's own `match_method` is deliberately not bound: it
        // resolves details for an id the search tier already chose, and
        // recording it would overwrite the route that did the choosing.
        Ok(ResolveOutcome::Resolved { metadata, .. }) => {
            // If NFO is present but incomplete, merge NFO data over TMDB
            // (only when kinds match — episode NFOs do not merge into show
            // detail, ADR-0026 §8.2).
            let final_meta = if let Some(ref nfo_meta) = nfo {
                if nfo_meta.kind == kind {
                    let merged = canonical::merge_prefer_left(nfo_meta, &metadata);
                    // Re-persist merged canonical over TMDB-only row.
                    let tx = conn
                        .unchecked_transaction()
                        .map_err(|e| format!("begin merge persist tx: {e}"))?;
                    canonical::upsert_canonical(&tx, PROVIDER_TMDB, &merged)?;
                    tx.commit()
                        .map_err(|e| format!("commit merge persist: {e}"))?;
                    Box::new(merged)
                } else {
                    metadata
                }
            } else {
                metadata
            };

            eprintln!("  enrich {} → tmdb:{tmdb_id} (detail+bind)", g.title);
            let mut unplaced: HashMap<i64, UnplacedCause> = HashMap::new();
            let bind_complete = match bind_resolved_items_classified(
                conn,
                resolver,
                &g.item_ids,
                &final_meta,
            ) {
                Ok((b, why)) => {
                    unplaced = why;
                    stats.seasons_fetched += b.seasons_fetched;
                    stats.episodes_projected += b.episodes_projected;
                    stats.files_linked += b.files_linked;
                    stats.seasons_skipped += b.seasons_skipped;
                    if b.seasons_skipped > 0 {
                        eprintln!(
                            "  bind {} seasons_fetched={} skipped={} linked={}",
                            g.title, b.seasons_fetched, b.seasons_skipped, b.files_linked
                        );
                        // Issue 121: a file linked to an entity this folder
                        // does not bind is mis-linked. Clear it before looking
                        // for a second entity, so the unplaced set is the
                        // truth rather than yesterday's wrong answer.
                        match unlink_foreign_entity_links(
                            conn,
                            g.library_id,
                            &g.show_folder,
                            &g.item_ids,
                        ) {
                            Ok(n) if n > 0 => eprintln!(
                                "  unlink {} — {n} file(s) linked to an entity this folder does not bind",
                                g.title
                            ),
                            Ok(_) => {}
                            Err(e) => eprintln!("  unlink ({}): {e}", g.title),
                        }
                        // ADR-0046 item 4: seasons the bound entity has no
                        // season for are the unplaced set, and they are the
                        // input to the second-entity search. The count is
                        // already computed from a call the drain just made.
                        if let Some(primary) = final_meta.ids.tmdb.or(final_meta.ids.tmdb_show) {
                            match bind_second_entities(
                                conn,
                                resolver,
                                SecondEntityScope {
                                    library_id: g.library_id,
                                    show_folder: &g.show_folder,
                                    title: &g.title,
                                    soft_key: &g.title,
                                },
                                &g.item_ids,
                                primary,
                            ) {
                                Ok(se) if se.entities_bound > 0 => {
                                    eprintln!(
                                        "  second entity {} bound={} linked={} still_unplaced={}",
                                        g.title,
                                        se.entities_bound,
                                        se.files_linked,
                                        se.still_unplaced
                                    );
                                    stats.files_linked += se.files_linked;
                                }
                                Ok(_) => {}
                                Err(e) => eprintln!("  second entity ({}): {e}", g.title),
                            }
                        }
                    }
                    true
                }
                Err(e) => {
                    eprintln!("  bind/season ({}): {e}", g.title);
                    stats.bind_errors += 1;
                    false
                }
            };
            // One exit per item (ADR-0026 §8.4 step 5): an item that received
            // a `tmdb:episode:` link is `ready`; one that did not, after the
            // season fetch(es) it needed succeeded or soft-skipped, is
            // `unmatched` — TMDB has the series but not this episode. Only a
            // provider error (bind interrupted, `Ok(Unresolved)`, `Err`) keeps
            // an item `matched`, and that is a retry, never a resting state.
            match g.resolve_kind {
                MetadataKind::Movie => {
                    set_metadata_status_recording(
                        conn,
                        &g.item_ids,
                        MetadataStatus::Ready,
                        Decision::KeepRoute,
                    )?;
                    stats.items_ready += g.item_ids.len();
                }
                MetadataKind::Episode | MetadataKind::Show => {
                    let bound = episode_bound_ids(conn, &g.item_ids)?;
                    if !bound.is_empty() {
                        set_metadata_status_recording(
                            conn,
                            &bound,
                            MetadataStatus::Ready,
                            Decision::KeepRoute,
                        )?;
                        stats.items_ready += bound.len();
                    }
                    let unbound: Vec<i64> = g
                        .item_ids
                        .iter()
                        .copied()
                        .filter(|id| !bound.contains(id))
                        .collect();
                    if !unbound.is_empty() && bind_complete {
                        eprintln!(
                            "  enrich unmatched {} — {} file(s) without episode identity",
                            g.title,
                            unbound.len()
                        );
                        // Layer 2 (ADR-0043 §2, as amended 2026-08-15): the
                        // status is still a set difference, but the bind now
                        // says why each file fell out of it. Items the bind
                        // did not classify keep `Uncomputed` — after this
                        // slice that null means *a cause the three tokens do
                        // not name*, not "nothing computes this".
                        let mut residue: Vec<i64> = Vec::new();
                        let mut by_cause: HashMap<UnplacedCause, Vec<i64>> = HashMap::new();
                        for id in &unbound {
                            match unplaced.get(id) {
                                Some(cause) => by_cause.entry(*cause).or_default().push(*id),
                                None => residue.push(*id),
                            }
                        }
                        for (cause, ids) in &by_cause {
                            eprintln!("    {} — {} file(s)", cause.token(), ids.len());
                            set_metadata_status_recording(
                                conn,
                                ids,
                                MetadataStatus::Unmatched,
                                Decision::Token(cause.token()),
                            )?;
                        }
                        if !residue.is_empty() {
                            eprintln!(
                                "    unclassified — {} file(s) unplaced for a cause the bind-time tokens do not name",
                                residue.len()
                            );
                            set_metadata_status_recording(
                                conn,
                                &residue,
                                MetadataStatus::Unmatched,
                                Decision::Uncomputed,
                            )?;
                        }
                        stats.items_unmatched += unbound.len();
                    }
                    // bind_complete == false: the bind provider error is a
                    // retry; unbound items stay `matched` on purpose.
                }
            }
            let poster = has_poster(&final_meta);
            unit_has_poster
                .entry(g.unit_key.clone())
                .and_modify(|p| *p = *p || poster)
                .or_insert(poster);
        }
        Ok(ResolveOutcome::Unresolved { reason, .. }) => {
            eprintln!(
                "  enrich unresolved {} reason={reason:?} (left matched)",
                g.title
            );
            stats.provider_errors += 1;
        }
        Err(ResolveError::NotFound(e)) => {
            // A detail 404 on a stored id means the id itself is bad, not a
            // transient failure: terminal `unmatched`, never a bare next-pass
            // repeat of the same call (ADR-0026 §8.4). The link survives.
            eprintln!("  enrich unmatched {} — stored id 404: {e}", g.title);
            set_metadata_status_recording(
                conn,
                &g.item_ids,
                MetadataStatus::Unmatched,
                Decision::Token("stored_id_404"),
            )?;
            stats.items_unmatched += g.item_ids.len();
        }
        Err(ResolveError::Provider(e)) => {
            eprintln!("  enrich provider error (left matched): {} — {e}", g.title);
            stats.provider_errors += 1;
        }
    }
    Ok(())
}

/// Media ids that hold at least one `tmdb:episode:{id}` link after a bind.
fn episode_bound_ids(conn: &Connection, item_ids: &[i64]) -> Result<Vec<i64>, String> {
    if item_ids.is_empty() {
        return Ok(Vec::new());
    }
    let mut out = Vec::new();
    let mut stmt = conn
        .prepare(
            "SELECT COUNT(*) FROM media_item_links
             WHERE media_item_id = ?1 AND item_key LIKE 'tmdb:episode:%'",
        )
        .map_err(|e| format!("prepare episode_bound_ids: {e}"))?;
    for id in item_ids {
        let count: i64 = stmt
            .query_row(params![id], |r| r.get(0))
            .map_err(|e| format!("episode_bound_ids check {id}: {e}"))?;
        if count > 0 {
            out.push(*id);
        }
    }
    Ok(out)
}

/// Best-effort sidecar NFO for a group's reference media path (Kodi layout),
/// as an ordered candidate list per kind. First candidate that **reads** wins:
/// a corrupt NFO yields `NfoInvalid` from the resolver rather than falling
/// through to a different file, which is the existing contract (autopsy D5) and
/// the reason this is not "first that parses".
///
/// **That contract is about this candidate list, not about resolution.** A
/// corrupt NFO ends the *cascade over NFO files*; it does not end the resolve.
/// Search still runs — the same search a folder with no NFO would get — and the
/// parse failure is reported only if that search also fails. The distinction
/// was worth writing down because this comment was read as authorising the
/// stronger behaviour, and 890 files in one measured run were terminal
/// `unmatched` on filenames that would have matched.
///
/// - **Movie:** `<stem>.nfo`, then `<dir>/movie.nfo`, then
///   `<dir>/<foldername>.nfo`.
/// - **Episode:** `<stem>.nfo`, then `<dir>/episodedetails.nfo`.
/// - **Show:** `<stem>.nfo` only.
///
/// The movie folder candidates cover the Kodi/Emby/Jellyfin layout, which is
/// what Radarr writes and what every movie in the 2026-08-08 dogfood library
/// uses: 1,748 of 1,756 movie NFOs are `movie.nfo`, so same-stem alone found
/// eight of them and every other movie was fetched from TMDB with a complete
/// NFO sitting beside it.
///
/// Not a folder scan, deliberately: `Breaking Bad/Season 1/` holds `season.nfo`
/// beside twenty episode NFOs, so "any NFO here" would give an episode the
/// season's metadata, and movie folders hold extras and trailers with NFOs of
/// their own.
///
/// `tvshow.nfo` is deliberately **not** a candidate — it is series identity,
/// read separately by [`show_root_nfo_xml`], so it never masks the episode NFO
/// (autopsy D5). Read/IO failures are silent `None`.
fn nfo_sidecar_xml(
    path: &std::path::Path,
    kind: MetadataKind,
    library_path: &str,
) -> Option<String> {
    let mut candidates = vec![path.with_extension("nfo")];
    if let Some(dir) = path.parent() {
        match kind {
            // Folder-level names only mean "this title" when the folder *is*
            // the title. Flat `Movies/Eagle Eye (2008).mkv` would otherwise
            // take `Movies/movie.nfo`, applying one stray file's metadata to
            // every movie in the library — autopsy D5 in a new place.
            MetadataKind::Movie if !is_library_root(dir, library_path) => {
                candidates.push(dir.join("movie.nfo"));
                if let Some(name) = dir.file_name() {
                    let mut folder_named = name.to_os_string();
                    folder_named.push(".nfo");
                    candidates.push(dir.join(folder_named));
                }
            }
            MetadataKind::Episode => candidates.push(dir.join("episodedetails.nfo")),
            MetadataKind::Movie | MetadataKind::Show => {}
        }
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

/// Whether `dir` is the library root itself, so a folder-level NFO in it would
/// describe the library rather than one title. Compares canonicalised paths so
/// a symlinked or `/var` vs `/private/var` root still matches (ADR-0030 §1).
fn is_library_root(dir: &std::path::Path, library_path: &str) -> bool {
    let library = std::path::Path::new(library_path);
    if dir == library {
        return true;
    }
    match (dir.canonicalize(), library.canonicalize()) {
        (Ok(a), Ok(b)) => a == b,
        _ => false,
    }
}

/// Series identity from `tvshow.nfo` at the show root (Kodi/Jellyfin layout).
/// Walks up from the media file's parent, bounded by the library root: a
/// `tvshow.nfo` above the library must never apply one show's ids to every
/// group under it (autopsy D5). A file at the library root itself counts,
/// for a library whose root is the show folder.
fn show_root_nfo_xml(path: &std::path::Path, library_path: &str) -> Option<String> {
    let library = std::path::Path::new(library_path);
    let mut current = path.parent()?;
    loop {
        if !current.starts_with(library) {
            return None;
        }
        let candidate = current.join("tvshow.nfo");
        if candidate.is_file() {
            return std::fs::read_to_string(&candidate).ok();
        }
        if current == library {
            return None;
        }
        current = current.parent()?;
    }
}

/// Parse the sidecar NFO (if any) for a media path into [`CanonicalMetadata`].
fn nfo_sidecar_meta(
    path: &std::path::Path,
    kind: MetadataKind,
    library_path: &str,
) -> Option<CanonicalMetadata> {
    let xml = nfo_sidecar_xml(path, kind, library_path)?;
    if xml.trim().is_empty() {
        return None;
    }
    crate::nfo::parse_nfo(&xml).ok()
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::rate_limit::ApiRateLimiter;
    use crate::resolve::Resolver;
    use crate::tmdb::TmdbStub;
    use nightjar_db::migrate;
    use rusqlite::Connection;
    use std::cell::Cell;
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
                confirmed: None,
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
                confirmed: None,
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

    /// A library whose root is unreachable contributes no groups, and its
    /// items stay `pending` rather than acquiring a terminal status derived
    /// from evidence that is not there.
    ///
    /// Measured motivation: the same database drained with its media absent
    /// reported 1,054 unmatched against 155, `errors = 0` either way, because
    /// no NFO could be read and every folder fell through to title search.
    #[test]
    fn drain_skips_unreachable_libraries_and_leaves_them_pending() {
        let c = Connection::open_in_memory().unwrap();
        migrate(&c).unwrap();
        c.execute_batch(
            "INSERT INTO libraries (name, path, kind) VALUES ('S', '/tmp/S', 'shows');
             INSERT INTO libraries (name, path, kind) VALUES ('Gone', '/tmp/Gone', 'shows');
             UPDATE libraries SET reachable = 0 WHERE name = 'Gone';
             INSERT INTO media_items (library_id, path, mtime_ms, size_bytes, title, kind, season, episode)
             VALUES
               (1, 'Beta/Season 01/Beta.S01E01.mkv', 1, 1, 'Beta', 'episode', 1, 1),
               (2, 'Beta/Season 01/Beta.S01E01.mkv', 1, 1, 'Beta', 'episode', 1, 1);",
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
        assert_eq!(s.libraries_unreachable, 1);
        assert_eq!(s.items_in_unreachable, 1);

        let reachable_status: String = c
            .query_row(
                "SELECT metadata_status FROM media_items WHERE library_id = 1",
                [],
                |r| r.get(0),
            )
            .unwrap();
        assert_ne!(
            reachable_status, "pending",
            "the reachable library is drained as usual"
        );
        let gone_status: String = c
            .query_row(
                "SELECT metadata_status FROM media_items WHERE library_id = 2",
                [],
                |r| r.get(0),
            )
            .unwrap();
        assert_eq!(
            gone_status, "pending",
            "not attempted is the honest state; the next drain retries it"
        );
        let gone_links: i64 = c
            .query_row(
                "SELECT COUNT(*) FROM media_item_links l
                 JOIN media_items m ON m.id = l.media_item_id
                 WHERE m.library_id = 2",
                [],
                |r| r.get(0),
            )
            .unwrap();
        assert_eq!(
            gone_links, 0,
            "nothing is bound from a library that is not there"
        );
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
        assert_eq!(s.items_ready, 2);
        assert_eq!(s.items_unmatched, 1);
        assert_eq!(s.seasons_fetched, 1);
        assert_eq!(s.seasons_skipped, 1);
        assert_eq!(s.files_linked, 2);
        assert_eq!(s.bind_errors, 0);
        assert_eq!(s.episodes_projected, 2);

        // S01 files get episode keys → ready; S05 (season 404 soft-skip) has
        // no episode identity → terminal unmatched, keeping its show link.
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
        let s5_status: String = c
            .query_row(
                "SELECT metadata_status FROM media_items WHERE season = 5",
                [],
                |r| r.get(0),
            )
            .unwrap();
        assert_eq!(s5_status, "unmatched");
        let s5_link: String = c
            .query_row(
                "SELECT item_key FROM media_item_links l
                 JOIN media_items m ON m.id = l.media_item_id
                 WHERE m.season = 5",
                [],
                |r| r.get(0),
            )
            .unwrap();
        assert_eq!(s5_link, "tmdb:show:77");
        // ADR-0043 §2 as amended: the same fact `seasons_skipped` counted, now
        // readable per file. One signal, two consumers.
        let s5_reason: Option<String> = c
            .query_row(
                "SELECT metadata_unmatched_reason FROM media_items WHERE season = 5",
                [],
                |r| r.get(0),
            )
            .unwrap();
        assert_eq!(s5_reason.as_deref(), Some("season_out_of_range"));
    }

    /// The season exists and was projected; this number is past its end.
    /// Distinct from `season_out_of_range` — the entity is right, the season
    /// is right, and only the episode number is not there.
    #[test]
    fn unplaced_records_episode_out_of_range_when_the_season_projected() {
        let c = Connection::open_in_memory().unwrap();
        migrate(&c).unwrap();
        c.execute_batch(
            "INSERT INTO libraries (name, path, kind) VALUES ('S', '/tmp/S', 'shows');
             INSERT INTO media_items (library_id, path, mtime_ms, size_bytes, title, kind, season, episode)
             VALUES
               (1, 'Beta/Season 01/Beta.S01E01.mkv', 1, 1, 'Beta', 'episode', 1, 1),
               (1, 'Beta/Season 01/Beta.S01E09.mkv', 1, 1, 'Beta', 'episode', 1, 9);",
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
        assert_eq!(s.items_ready, 1);
        assert_eq!(s.items_unmatched, 1);
        assert_eq!(
            s.seasons_skipped, 0,
            "the season was fetched — this is not the season-absent case"
        );
        let reason: Option<String> = c
            .query_row(
                "SELECT metadata_unmatched_reason FROM media_items WHERE episode = 9",
                [],
                |r| r.get(0),
            )
            .unwrap();
        assert_eq!(reason.as_deref(), Some("episode_out_of_range"));
    }

    /// Two files, one slot, **both link** — ADR-0025 §2's "Bluray + WEBDL:
    /// one `item_key`, several media files", which the bind used to discard by
    /// keeping whichever came last.
    ///
    /// Replaces `unplaced_records_duplicate_slot_and_leaves_the_winner_alone`,
    /// which asserted the discarding behaviour. Neither file here has a probed
    /// duration, so this also pins decision 4: **absent duration links**.
    #[test]
    fn two_versions_of_one_episode_both_link() {
        let c = Connection::open_in_memory().unwrap();
        migrate(&c).unwrap();
        c.execute_batch(
            "INSERT INTO libraries (name, path, kind) VALUES ('S', '/tmp/S', 'shows');
             INSERT INTO media_items (library_id, path, mtime_ms, size_bytes, title, kind, season, episode)
             VALUES
               (1, 'Beta/Season 01/Beta.S01E01.1080p.mkv', 1, 1, 'Beta', 'episode', 1, 1),
               (1, 'Beta/Season 01/Beta.S01E01.720p.mkv', 1, 1, 'Beta', 'episode', 1, 1);",
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
        assert_eq!(s.items_ready, 2, "both versions are the episode");
        assert_eq!(s.items_unmatched, 0);

        let with_key: i64 = c
            .query_row(
                "SELECT COUNT(*) FROM media_item_links WHERE item_key LIKE 'tmdb:episode:%'",
                [],
                |r| r.get(0),
            )
            .unwrap();
        assert_eq!(with_key, 2, "both files carry the episode key");
        let distinct_keys: i64 = c
            .query_row(
                "SELECT COUNT(DISTINCT item_key) FROM media_item_links
                 WHERE item_key LIKE 'tmdb:episode:%'",
                [],
                |r| r.get(0),
            )
            .unwrap();
        assert_eq!(distinct_keys, 1, "one item, several files");
    }

    /// An extra is not a version. 48 seconds against a 22-minute episode is the
    /// measured shape (`American Gigg-Olo - Cat Scene (Uncensored)`), and
    /// linking it would let ADR-0022 §5 play the clip when someone asked for
    /// the episode.
    #[test]
    fn a_much_shorter_file_in_the_slot_does_not_link() {
        let c = Connection::open_in_memory().unwrap();
        migrate(&c).unwrap();
        c.execute_batch(
            "INSERT INTO libraries (name, path, kind) VALUES ('S', '/tmp/S', 'shows');
             INSERT INTO media_items (library_id, path, mtime_ms, size_bytes, title, kind, season, episode, duration_ms)
             VALUES
               (1, 'Beta/Season 01/Beta.S01E01.mkv', 1, 1, 'Beta', 'episode', 1, 1, 1374000),
               (1, 'Beta/Season 01/Beta.S01E01 - Cat Scene.mkv', 1, 1, 'Beta', 'episode', 1, 1, 48000);",
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
        assert_eq!(s.items_ready, 1, "the episode links");
        assert_eq!(s.items_unmatched, 1, "the clip does not");

        let clip_reason: Option<String> = c
            .query_row(
                "SELECT metadata_unmatched_reason FROM media_items
                 WHERE path LIKE '%Cat Scene%'",
                [],
                |r| r.get(0),
            )
            .unwrap();
        assert_eq!(
            clip_reason.as_deref(),
            Some("duplicate_slot"),
            "another file holds the slot, which is still what happened"
        );
        let clip_keys: i64 = c
            .query_row(
                "SELECT COUNT(*) FROM media_item_links l
                 JOIN media_items m ON m.id = l.media_item_id
                 WHERE m.path LIKE '%Cat Scene%' AND l.item_key LIKE 'tmdb:episode:%'",
                [],
                |r| r.get(0),
            )
            .unwrap();
        assert_eq!(clip_keys, 0, "the clip never enters the episode's item");
    }

    /// The residue. A file the bind cannot place for any of the three reasons
    /// records **nothing** rather than a fallback label — §2's
    /// `no_scanned_number` measured zero instances and is not a shipped token.
    ///
    /// This is the check that keeps the taxonomy honest: a null here means *a
    /// cause the three do not name*, which is what makes a future non-zero
    /// count a signal instead of noise.
    #[test]
    fn unplaced_records_nothing_when_no_token_names_the_cause() {
        let c = Connection::open_in_memory().unwrap();
        migrate(&c).unwrap();
        c.execute_batch(
            "INSERT INTO libraries (name, path, kind) VALUES ('S', '/tmp/S', 'shows');
             INSERT INTO media_items (library_id, path, mtime_ms, size_bytes, title, kind, season, episode)
             VALUES
               (1, 'Beta/Season 01/Beta.S01E01.mkv', 1, 1, 'Beta', 'episode', 1, 1),
               (1, 'Beta/Season 01/Beta.extras.mkv', 1, 1, 'Beta', 'episode', 1, NULL);",
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
        assert_eq!(s.items_ready, 1);
        assert_eq!(s.items_unmatched, 1);
        // Status and reason read together: a null reason on a row that was
        // never written would pass this test for the wrong reason.
        let (status, reason): (String, Option<String>) = c
            .query_row(
                "SELECT metadata_status, metadata_unmatched_reason FROM media_items
                 WHERE episode IS NULL",
                [],
                |r| Ok((r.get(0)?, r.get(1)?)),
            )
            .unwrap();
        assert_eq!(status, "unmatched");
        assert_eq!(
            reason, None,
            "no token names this, so none is invented — the null is the record"
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
                    confirmed: None,
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
        assert_eq!(s.items_ready, 0);
        assert_eq!(s.items_unmatched, 1);
        assert_eq!(s.seasons_skipped, 1);
        assert_eq!(s.files_linked, 0);
        // Season fetch soft-skipped and no episode identity exists: the file is
        // terminal unmatched (ADR-0026 §8.4), keeping its show link for enrich id.
        let n: i64 = c
            .query_row("SELECT COUNT(*) FROM media_item_links", [], |r| r.get(0))
            .unwrap();
        assert_eq!(n, 1);
        let key: String = c
            .query_row("SELECT item_key FROM media_item_links", [], |r| r.get(0))
            .unwrap();
        assert_eq!(key, "tmdb:show:1");
        let status: String = c
            .query_row("SELECT metadata_status FROM media_items", [], |r| r.get(0))
            .unwrap();
        assert_eq!(status, "unmatched");
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
                    confirmed: None,
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
                    confirmed: None,
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
                    confirmed: None,
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

    /// A show-sidecar NFO whose only TMDB id is an *episode* id must fall
    /// through to a TV search: the episode id is never written as a show key
    /// and `matched` requires an enrichable id.
    #[test]
    fn drain_episode_nfo_with_episode_id_only_falls_through_to_tv_search() {
        use crate::resolve::ProviderResult;
        use std::sync::atomic::{AtomicUsize, Ordering};
        static EP_NFO_SEARCHES: AtomicUsize = AtomicUsize::new(0);
        struct EpNfoSource;
        impl MetadataSource for EpNfoSource {
            fn resolve(
                &self,
                input: &crate::resolve::ResolveInput,
            ) -> Result<ProviderResult, crate::resolve::ResolveError> {
                if input.tmdb_id.is_none() {
                    EP_NFO_SEARCHES.fetch_add(1, Ordering::SeqCst);
                }
                let meta = CanonicalMetadata {
                    kind: MetadataKind::Show,
                    title: "Beta".into(),
                    original_title: None,
                    year: Some(2020),
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
                    artwork: vec![crate::model::ArtworkRef {
                        kind: ArtworkKind::Poster,
                        path: "/b.jpg".into(),
                    }],
                    collection: None,
                    season: None,
                    episode: None,
                };
                Ok(ProviderResult::Hit {
                    metadata: Box::new(meta),
                    method: "exact_title",
                    confirmed: None,
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
            ) -> Result<Option<crate::tmdb::RawProviderPayload>, crate::resolve::ResolveError>
            {
                assert_eq!(show_id, 77);
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
        EP_NFO_SEARCHES.store(0, Ordering::SeqCst);
        // episodedetails.nfo in the show folder carries only an episode TMDB id.
        let root = std::env::temp_dir().join(format!(
            "nightjar-nfo-ep-fallback-test-{}",
            std::process::id()
        ));
        let _ = std::fs::remove_dir_all(&root);
        std::fs::create_dir_all(&root).unwrap();
        std::fs::write(
            root.join("episodedetails.nfo"),
            r#"<?xml version="1.0" encoding="utf-8"?>
<episodedetails>
  <title>Pilot</title>
  <showtitle>Beta</showtitle>
  <season>1</season>
  <episode>1</episode>
  <uniqueid type="tmdb">62085</uniqueid>
</episodedetails>"#,
        )
        .unwrap();
        let c = Connection::open_in_memory().unwrap();
        migrate(&c).unwrap();
        c.execute_batch(&format!(
            "INSERT INTO libraries (name, path, kind) VALUES ('S', '{}', 'shows');
             INSERT INTO media_items (library_id, path, mtime_ms, size_bytes, title, kind, season, episode)
             VALUES
               (1, 'Beta.S01E01.mkv', 1, 1, 'Beta', 'episode', 1, 1),
               (1, 'Beta.S01E02.mkv', 1, 1, 'Beta', 'episode', 1, 2);",
            root.to_str().unwrap()
        ))
        .unwrap();
        let s = drain_pending(
            &c,
            &Resolver { tmdb: EpNfoSource },
            &AtomicU64::new(0),
            &AtomicU64::new(0),
            DrainOptions::default(),
        )
        .unwrap();
        // Fall-through: the episode-id NFO could not resolve the group, so the
        // provider search ran once (search tier), then enrich by show id.
        assert_eq!(EP_NFO_SEARCHES.load(Ordering::SeqCst), 1);
        assert_eq!(s.items_matched, 2);
        assert_eq!(s.items_ready, 2);
        let ready: i64 = c
            .query_row(
                "SELECT COUNT(*) FROM media_items WHERE metadata_status = 'ready'",
                [],
                |r| r.get(0),
            )
            .unwrap();
        assert_eq!(ready, 2);
        // Episode links come from the season bind — never `tmdb:show:{62085}`.
        let show_62085: i64 = c
            .query_row(
                "SELECT COUNT(*) FROM media_item_links WHERE item_key = 'tmdb:show:62085'",
                [],
                |r| r.get(0),
            )
            .unwrap();
        assert_eq!(show_62085, 0, "episode id must never become a show key");
        let mut stmt = c
            .prepare("SELECT item_key FROM media_item_links ORDER BY item_key")
            .unwrap();
        let ep_links: Vec<String> = stmt
            .query_map([], |r| r.get(0))
            .unwrap()
            .collect::<Result<Vec<String>, _>>()
            .unwrap();
        assert_eq!(ep_links, vec!["tmdb:episode:1001", "tmdb:episode:1002"]);
        let _ = std::fs::remove_dir_all(&root);
    }

    /// Complete movie NFO (title + tmdb id + plot) skips TMDB entirely:
    /// search tier sets Ready directly and the mock provider is never called.
    #[test]
    fn drain_complete_movie_nfo_ready_zero_tmdb_http() {
        use crate::resolve::ProviderResult;
        use std::sync::atomic::{AtomicUsize, Ordering};
        static COMPLETE_TMDB_CALLS: AtomicUsize = AtomicUsize::new(0);
        struct CompleteNfoProvider;
        impl MetadataSource for CompleteNfoProvider {
            fn resolve(
                &self,
                _input: &ResolveInput,
            ) -> Result<ProviderResult, crate::resolve::ResolveError> {
                COMPLETE_TMDB_CALLS.fetch_add(1, Ordering::SeqCst);
                unreachable!("complete NFO must never call TMDB");
            }
        }
        COMPLETE_TMDB_CALLS.store(0, Ordering::SeqCst);
        let dir =
            std::env::temp_dir().join(format!("nightjar-complete-nfo-test-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        let media = dir.join("complete.mkv");
        std::fs::write(
            dir.join("complete.nfo"),
            r#"<?xml version="1.0" encoding="utf-8"?>
<movie><title>Complete NFO</title><year>2025</year>
<plot>Local plot data</plot>
<uniqueid type="tmdb">42</uniqueid></movie>"#,
        )
        .unwrap();
        let c = Connection::open_in_memory().unwrap();
        migrate(&c).unwrap();
        c.execute_batch(&format!(
            "INSERT INTO libraries (name, path, kind) VALUES ('L', '{}', 'movies');
             INSERT INTO media_items (library_id, path, mtime_ms, size_bytes, title, kind)
             VALUES (1, '{}', 1, 1, 'Complete NFO', 'movie');",
            dir.to_str().unwrap(),
            media.to_str().unwrap()
        ))
        .unwrap();
        let s = drain_pending(
            &c,
            &Resolver {
                tmdb: CompleteNfoProvider,
            },
            &AtomicU64::new(0),
            &AtomicU64::new(0),
            DrainOptions::default(),
        )
        .unwrap();
        assert_eq!(COMPLETE_TMDB_CALLS.load(Ordering::SeqCst), 0);
        assert_eq!(s.items_matched, 0);
        assert_eq!(s.items_ready, 1);
        let status: String = c
            .query_row("SELECT metadata_status FROM media_items", [], |r| r.get(0))
            .unwrap();
        assert_eq!(status, "ready");
        // Canonical was persisted from NFO.
        let title: String = c
            .query_row(
                "SELECT title FROM metadata_canonical WHERE provider = 'tmdb'",
                [],
                |r| r.get(0),
            )
            .unwrap();
        assert_eq!(title, "Complete NFO");
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// Incomplete movie NFO (no TMDB id) with plot LOCAL → search falls
    /// through to TMDB → enrich loads NFO → merge_prefer_left preserves
    /// LOCAL plot over TMDB REMOTE plot.
    #[test]
    fn drain_nfo_plot_local_wins_merge_over_tmdb_remote() {
        use crate::resolve::ProviderResult;
        use std::sync::atomic::{AtomicUsize, Ordering};
        static MERGE_TMDB_CALLS: AtomicUsize = AtomicUsize::new(0);
        struct MergeNfoProvider;
        impl MetadataSource for MergeNfoProvider {
            fn resolve(
                &self,
                _input: &ResolveInput,
            ) -> Result<ProviderResult, crate::resolve::ResolveError> {
                MERGE_TMDB_CALLS.fetch_add(1, Ordering::SeqCst);
                // Search tier: NFO has no TMDB id, so TMDB search is called.
                // Enrich tier: id-driven, so NFO is not in input.
                let meta = CanonicalMetadata {
                    kind: MetadataKind::Movie,
                    title: "Merge NFO".into(),
                    original_title: None,
                    year: Some(2025),
                    air_date: None,
                    plot: Some("REMOTE tmdb plot".into()),
                    genres: vec![],
                    runtime_minutes: None,
                    cast: vec![],
                    ratings: vec![],
                    ids: crate::model::ProviderIds {
                        tmdb: Some(99),
                        tmdb_show: None,
                        imdb: None,
                        tvdb: None,
                    },
                    artwork: vec![],
                    collection: None,
                    season: None,
                    episode: None,
                };
                Ok(ProviderResult::Hit {
                    metadata: Box::new(meta),
                    method: "exact_title",
                    confirmed: None,
                    raw: Some(crate::tmdb::RawProviderPayload {
                        entity_kind: "movie".into(),
                        provider_id: "99".into(),
                        payload: r#"{"id":99,"title":"Merge NFO","overview":"REMOTE tmdb plot"}"#
                            .into(),
                    }),
                })
            }
        }
        MERGE_TMDB_CALLS.store(0, Ordering::SeqCst);
        let dir =
            std::env::temp_dir().join(format!("nightjar-merge-nfo-test-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        let media = dir.join("merge.mkv");
        // NFO has plot="LOCAL nfo plot" but NO tmdb uniqueid — incomplete.
        std::fs::write(
            dir.join("merge.nfo"),
            r#"<?xml version="1.0" encoding="utf-8"?>
<movie><title>Merge NFO</title><year>2025</year>
<plot>LOCAL nfo plot</plot></movie>"#,
        )
        .unwrap();
        let c = Connection::open_in_memory().unwrap();
        migrate(&c).unwrap();
        c.execute_batch(&format!(
            "INSERT INTO libraries (name, path, kind) VALUES ('L', '{}', 'movies');
             INSERT INTO media_items (library_id, path, mtime_ms, size_bytes, title, kind)
             VALUES (1, '{}', 1, 1, 'Merge NFO', 'movie');",
            dir.to_str().unwrap(),
            media.to_str().unwrap()
        ))
        .unwrap();
        let s = drain_pending(
            &c,
            &Resolver {
                tmdb: MergeNfoProvider,
            },
            &AtomicU64::new(0),
            &AtomicU64::new(0),
            DrainOptions::default(),
        )
        .unwrap();
        // Search + enrich: two TMDB calls (one search, one detail).
        assert_eq!(MERGE_TMDB_CALLS.load(Ordering::SeqCst), 2);
        assert_eq!(s.items_matched, 1);
        assert_eq!(s.items_ready, 1);
        let status: String = c
            .query_row("SELECT metadata_status FROM media_items", [], |r| r.get(0))
            .unwrap();
        assert_eq!(status, "ready");
        // Canonical plot = LOCAL from NFO (merge_prefer_left wins).
        let plot: String = c
            .query_row(
                "SELECT plot FROM metadata_canonical WHERE provider = 'tmdb'",
                [],
                |r| r.get(0),
            )
            .unwrap();
        assert_eq!(plot, "LOCAL nfo plot");
        let _ = std::fs::remove_dir_all(&dir);
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
                    confirmed: None,
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

    #[test]
    fn drain_visible_enrich_before_background_search() {
        use crate::resolve::ProviderResult;
        use std::sync::Mutex;
        static ORDER: Mutex<Vec<&'static str>> = Mutex::new(Vec::new());
        struct OrderSource;
        impl MetadataSource for OrderSource {
            fn resolve(
                &self,
                input: &ResolveInput,
            ) -> Result<ProviderResult, crate::resolve::ResolveError> {
                let mut o = ORDER.lock().unwrap();
                if input.tmdb_id.is_some() {
                    o.push("enrich");
                } else {
                    o.push("search");
                }
                let title = input.title.clone().unwrap_or_default();
                let id = if title.contains("Visible") { 1 } else { 2 };
                let meta = CanonicalMetadata {
                    kind: MetadataKind::Movie,
                    title,
                    original_title: None,
                    year: Some(2020),
                    air_date: None,
                    plot: None,
                    genres: Vec::new(),
                    runtime_minutes: None,
                    cast: Vec::new(),
                    ratings: Vec::new(),
                    ids: crate::model::ProviderIds {
                        tmdb: Some(id),
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
                    confirmed: None,
                    raw: Some(crate::tmdb::RawProviderPayload {
                        entity_kind: "movie".into(),
                        provider_id: id.to_string(),
                        payload: format!(r#"{{"id":{id},"title":"x"}}"#),
                    }),
                })
            }
        }
        ORDER.lock().unwrap().clear();
        let c = Connection::open_in_memory().unwrap();
        migrate(&c).unwrap();
        // Visible proxy ranks movies by title (first N). Seed:
        // - 40 "A Fill …" ready rows (fill the proxy)
        // - "A Visible Hit" matched + movie link (in proxy → front enrich)
        // - "Zzz Background" pending (outside proxy → background search)
        c.execute_batch(
            "INSERT INTO libraries (name, path, kind) VALUES ('L', '/tmp/L', 'movies');",
        )
        .unwrap();
        for i in 1..=40 {
            c.execute(
                "INSERT INTO media_items (library_id, path, mtime_ms, size_bytes, title, kind, metadata_status)
                 VALUES (1, ?1, 1, 1, ?2, 'movie', 'ready')",
                rusqlite::params![format!("f{i:02}.mkv"), format!("A Fill {i:02}")],
            )
            .unwrap();
        }
        c.execute(
            "INSERT INTO media_items (library_id, path, mtime_ms, size_bytes, title, kind, metadata_status)
             VALUES (1, 'vis.mkv', 1, 1, 'A Visible Hit', 'movie', 'matched')",
            [],
        )
        .unwrap();
        let vis_id = c.last_insert_rowid();
        c.execute(
            "INSERT INTO media_item_links (media_item_id, item_key, manually_matched)
             VALUES (?1, 'tmdb:movie:1', 0)",
            [vis_id],
        )
        .unwrap();
        c.execute(
            "INSERT INTO media_items (library_id, path, mtime_ms, size_bytes, title, kind, metadata_status)
             VALUES (1, 'bg.mkv', 1, 1, 'Zzz Background', 'movie', 'pending')",
            [],
        )
        .unwrap();
        // Proxy is first 40 by title: A Fill 01-40 only — "A Visible Hit" sorts after
        // "A Fill 40" and may be #41. Force Visible Hit into proxy by naming:
        // "A 00 Visible" sorts before "A Fill".
        c.execute("DELETE FROM media_items WHERE title = 'A Visible Hit'", [])
            .unwrap();
        c.execute("DELETE FROM media_item_links", []).unwrap();
        c.execute(
            "INSERT INTO media_items (library_id, path, mtime_ms, size_bytes, title, kind, metadata_status)
             VALUES (1, 'vis.mkv', 1, 1, 'A 00 Visible', 'movie', 'matched')",
            [],
        )
        .unwrap();
        let vis_id = c.last_insert_rowid();
        c.execute(
            "INSERT INTO media_item_links (media_item_id, item_key, manually_matched)
             VALUES (?1, 'tmdb:movie:1', 0)",
            [vis_id],
        )
        .unwrap();
        // Proxy: A 00 Visible + A Fill 01-39 (40 units). Zzz Background out.
        let s = drain_pending(
            &c,
            &Resolver { tmdb: OrderSource },
            &AtomicU64::new(0),
            &AtomicU64::new(0),
            DrainOptions::default(),
        )
        .unwrap();
        assert!(s.items_ready >= 1, "visible matched should enrich to ready");
        assert_eq!(
            s.items_matched, 1,
            "background pending should search to matched"
        );
        let order = ORDER.lock().unwrap().clone();
        let enrich_pos = order.iter().position(|&x| x == "enrich").expect("enrich");
        let search_pos = order.iter().position(|&x| x == "search").expect("search");
        assert!(
            enrich_pos < search_pos,
            "Visible enrich must precede background search, got {order:?}"
        );
    }

    /// Show hit with a counting `resolve`; `fetch_season` soft-skips every
    /// season (`Ok(None)`, the TMDB-404 shape). Shares one `show_id`.
    struct SoftSkipSeasonSource {
        resolve_calls: Cell<usize>,
        show_id: i64,
    }

    impl MetadataSource for SoftSkipSeasonSource {
        fn resolve(
            &self,
            input: &ResolveInput,
        ) -> Result<crate::resolve::ProviderResult, crate::resolve::ResolveError> {
            self.resolve_calls.set(self.resolve_calls.get() + 1);
            let id = input.tmdb_id.unwrap_or(self.show_id);
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
                    tmdb: Some(id),
                    tmdb_show: Some(id),
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
                confirmed: None,
                raw: Some(crate::tmdb::RawProviderPayload {
                    entity_kind: "tv".into(),
                    provider_id: id.to_string(),
                    payload: format!(r#"{{"id":{id},"name":"Alpha"}}"#),
                }),
            })
        }

        fn fetch_season(
            &self,
            _show_id: i64,
            _season_number: i32,
        ) -> Result<Option<crate::tmdb::RawProviderPayload>, crate::resolve::ResolveError> {
            // TMDB has no season for this file (special outside the model):
            // soft skip, never a hard error.
            Ok(None)
        }
    }

    /// RC3: an unbindable special (S00) soft-skips its season, lands terminal
    /// `unmatched`, and the second drain pass makes zero provider calls.
    #[test]
    fn unbindable_special_is_terminal_unmatched_and_second_drain_makes_zero_calls() {
        let c = Connection::open_in_memory().unwrap();
        migrate(&c).unwrap();
        c.execute_batch(
            "INSERT INTO libraries (name, path, kind) VALUES ('S', '/tmp/S', 'shows');
             INSERT INTO media_items (library_id, path, mtime_ms, size_bytes, title, kind, season, episode)
             VALUES (1, 'Alpha/Specials/Alpha.S00E01.mkv', 1, 1, 'Alpha', 'episode', 0, 1);",
        )
        .unwrap();
        let src = SoftSkipSeasonSource {
            resolve_calls: Cell::new(0),
            show_id: 55,
        };
        let resolver = Resolver { tmdb: src };
        let s1 = drain_pending(
            &c,
            &resolver,
            &AtomicU64::new(0),
            &AtomicU64::new(0),
            DrainOptions::default(),
        )
        .unwrap();
        assert_eq!(s1.items_matched, 1);
        assert_eq!(s1.items_ready, 0);
        assert_eq!(s1.items_unmatched, 1);
        assert_eq!(s1.seasons_skipped, 1);
        let calls_after_first = resolver.tmdb.resolve_calls.get();
        assert_eq!(calls_after_first, 2, "one search + one id enrich");

        let status: String = c
            .query_row("SELECT metadata_status FROM media_items", [], |r| r.get(0))
            .unwrap();
        assert_eq!(status, "unmatched");
        let link: String = c
            .query_row("SELECT item_key FROM media_item_links", [], |r| r.get(0))
            .unwrap();
        assert_eq!(
            link, "tmdb:show:55",
            "show link survives on widened unmatched"
        );

        let s2 = drain_pending(
            &c,
            &resolver,
            &AtomicU64::new(0),
            &AtomicU64::new(0),
            DrainOptions::default(),
        )
        .unwrap();
        assert_eq!(s2.groups, 0);
        assert_eq!(
            resolver.tmdb.resolve_calls.get(),
            calls_after_first,
            "second drain must make zero provider calls"
        );
    }

    /// RC3: a detail 404 on a stored id is a bad id, not a network hiccup —
    /// terminal `unmatched`, and the next pass never repeats the call.
    #[test]
    fn stored_show_id_404_reaches_terminal_status_and_is_not_repeated() {
        struct Detail404 {
            resolve_calls: Cell<usize>,
        }
        impl MetadataSource for Detail404 {
            fn resolve(
                &self,
                input: &ResolveInput,
            ) -> Result<crate::resolve::ProviderResult, crate::resolve::ResolveError> {
                self.resolve_calls.set(self.resolve_calls.get() + 1);
                assert!(
                    input.tmdb_id.is_some(),
                    "pre-seeded matched item must only run enrich"
                );
                Err(crate::resolve::ResolveError::NotFound(format!(
                    "TMDB 404: /tv/{}",
                    input.tmdb_id.unwrap()
                )))
            }
        }
        let c = Connection::open_in_memory().unwrap();
        migrate(&c).unwrap();
        c.execute_batch(
            "INSERT INTO libraries (name, path, kind) VALUES ('S', '/tmp/S', 'shows');
             INSERT INTO media_items (library_id, path, mtime_ms, size_bytes, title, kind, season, episode, metadata_status)
             VALUES (1, 'Alpha/S01E01.mkv', 1, 1, 'Alpha', 'episode', 1, 1, 'matched');
             INSERT INTO media_item_links (media_item_id, item_key, manually_matched)
             VALUES (1, 'tmdb:show:2682989', 0);",
        )
        .unwrap();
        let src = Detail404 {
            resolve_calls: Cell::new(0),
        };
        let resolver = Resolver { tmdb: src };
        let s1 = drain_pending(
            &c,
            &resolver,
            &AtomicU64::new(0),
            &AtomicU64::new(0),
            DrainOptions::default(),
        )
        .unwrap();
        assert_eq!(s1.items_unmatched, 1);
        assert_eq!(
            s1.provider_errors, 0,
            "404 is not a retryable provider error"
        );
        let status: String = c
            .query_row("SELECT metadata_status FROM media_items", [], |r| r.get(0))
            .unwrap();
        assert_eq!(status, "unmatched");
        let calls_after_first = resolver.tmdb.resolve_calls.get();
        assert_eq!(calls_after_first, 1);

        let s2 = drain_pending(
            &c,
            &resolver,
            &AtomicU64::new(0),
            &AtomicU64::new(0),
            DrainOptions::default(),
        )
        .unwrap();
        assert_eq!(s2.groups, 0);
        assert_eq!(
            resolver.tmdb.resolve_calls.get(),
            calls_after_first,
            "bad stored id must not be re-fetched next pass"
        );
    }

    /// RC3: a genuine provider error (timeout) is a retry — the item stays
    /// `matched` and the next pass does attempt it again.
    #[test]
    fn provider_error_leaves_matched_and_next_pass_retries() {
        struct Flaky {
            resolve_calls: Cell<usize>,
        }
        impl MetadataSource for Flaky {
            fn resolve(
                &self,
                input: &ResolveInput,
            ) -> Result<crate::resolve::ProviderResult, crate::resolve::ResolveError> {
                self.resolve_calls.set(self.resolve_calls.get() + 1);
                let id = input.tmdb_id.unwrap_or(77);
                if self.resolve_calls.get() == 1 {
                    return Err(crate::resolve::ResolveError::Provider("timeout".into()));
                }
                let meta = CanonicalMetadata {
                    kind: MetadataKind::Show,
                    title: "Beta".into(),
                    original_title: None,
                    year: Some(2020),
                    air_date: None,
                    plot: None,
                    genres: Vec::new(),
                    runtime_minutes: None,
                    cast: Vec::new(),
                    ratings: Vec::new(),
                    ids: crate::model::ProviderIds {
                        tmdb: Some(id),
                        tmdb_show: Some(id),
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
                    confirmed: None,
                    raw: Some(crate::tmdb::RawProviderPayload {
                        entity_kind: "tv".into(),
                        provider_id: id.to_string(),
                        payload: format!(r#"{{"id":{id},"name":"Beta"}}"#),
                    }),
                })
            }

            fn fetch_season(
                &self,
                show_id: i64,
                season_number: i32,
            ) -> Result<Option<crate::tmdb::RawProviderPayload>, crate::resolve::ResolveError>
            {
                assert_eq!(show_id, 77);
                assert_eq!(season_number, 1);
                let payload = r#"{
                    "season_number": 1,
                    "episodes": [
                      {"id": 2001, "name": "Pilot", "season_number": 1, "episode_number": 1, "air_date": "2020-01-01"}
                    ]
                }"#;
                Ok(Some(crate::tmdb::RawProviderPayload {
                    entity_kind: "season".into(),
                    provider_id: format!("{show_id}:{season_number}"),
                    payload: payload.into(),
                }))
            }
        }
        let c = Connection::open_in_memory().unwrap();
        migrate(&c).unwrap();
        c.execute_batch(
            "INSERT INTO libraries (name, path, kind) VALUES ('S', '/tmp/S', 'shows');
             INSERT INTO media_items (library_id, path, mtime_ms, size_bytes, title, kind, season, episode, metadata_status)
             VALUES (1, 'Beta/S01E01.mkv', 1, 1, 'Beta', 'episode', 1, 1, 'matched');
             INSERT INTO media_item_links (media_item_id, item_key, manually_matched)
             VALUES (1, 'tmdb:show:77', 0);",
        )
        .unwrap();
        let src = Flaky {
            resolve_calls: Cell::new(0),
        };
        let resolver = Resolver { tmdb: src };
        let s1 = drain_pending(
            &c,
            &resolver,
            &AtomicU64::new(0),
            &AtomicU64::new(0),
            DrainOptions::default(),
        )
        .unwrap();
        assert_eq!(s1.provider_errors, 1);
        assert_eq!(s1.items_ready, 0);
        assert_eq!(s1.items_unmatched, 0);
        let status_after_first: String = c
            .query_row("SELECT metadata_status FROM media_items", [], |r| r.get(0))
            .unwrap();
        assert_eq!(
            status_after_first, "matched",
            "timeout is a retry, not terminal"
        );

        let s2 = drain_pending(
            &c,
            &resolver,
            &AtomicU64::new(0),
            &AtomicU64::new(0),
            DrainOptions::default(),
        )
        .unwrap();
        assert_eq!(
            resolver.tmdb.resolve_calls.get(),
            2,
            "next pass must retry the item"
        );
        assert_eq!(s2.provider_errors, 0);
        assert_eq!(s2.items_ready, 1);
        let status: String = c
            .query_row("SELECT metadata_status FROM media_items", [], |r| r.get(0))
            .unwrap();
        assert_eq!(status, "ready");
        let ep_link: String = c
            .query_row("SELECT item_key FROM media_item_links", [], |r| r.get(0))
            .unwrap();
        assert_eq!(ep_link, "tmdb:episode:2001");
    }

    /// RC3: an episode holding only `tmdb:show:{id}` keys with its bound
    /// siblings (same show, same browse card). Fails on main: the show-link
    /// episode fell back to a soft key and split the show.
    #[test]
    fn episode_with_only_show_link_keys_with_bound_sibling() {
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
               ('tmdb', 'episode', '1001', 'Pilot', '{\"tmdb\":1001}', 55, '2026-01-01T00:00:00Z');
             INSERT INTO media_item_links (media_item_id, item_key, manually_matched)
             VALUES (1, 'tmdb:episode:1001', 0), (2, 'tmdb:show:55', 0);",
        )
        .unwrap();
        let proxy = snapshot_visible_proxy_n(&c, 10).unwrap();
        assert_eq!(proxy.units.len(), 1, "one show must be one browse unit");
        assert_eq!(proxy.units[0].unit_key, "tv|tmdb:55");
        assert_eq!(proxy.units[0].item_ids.len(), 2);
    }

    /// RC4: `apply_search_hit`'s Episode branch accepts only `ids.tmdb_show`,
    /// never `ids.tmdb` — an episode-only id must not be written as a show
    /// key (autopsy D3; the repair is migration 015).
    #[test]
    fn apply_search_hit_episode_branch_refuses_episode_only_id() {
        let c = Connection::open_in_memory().unwrap();
        migrate(&c).unwrap();
        c.execute_batch(
            "INSERT INTO libraries (name, path, kind) VALUES ('S', '/tmp/S', 'shows');
             INSERT INTO media_items (library_id, path, mtime_ms, size_bytes, title, kind, season, episode)
             VALUES (1, 'Alpha/S01E01.mkv', 1, 1, 'Alpha', 'episode', 1, 1);",
        )
        .unwrap();
        let meta = CanonicalMetadata {
            kind: MetadataKind::Episode,
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
                tmdb: Some(62085), // an episode id, never a show id
                tmdb_show: None,
                imdb: None,
                tvdb: None,
            },
            artwork: Vec::new(),
            collection: None,
            season: Some(1),
            episode: Some(1),
        };
        let wrote = apply_search_hit(&c, &[1], &meta, None, None).unwrap();
        assert!(
            !wrote,
            "episode-only id must not be accepted as an enrichable hit"
        );
        let links: i64 = c
            .query_row("SELECT COUNT(*) FROM media_item_links", [], |r| r.get(0))
            .unwrap();
        assert_eq!(links, 0, "no link may be written for an episode-only id");
        let status: String = c
            .query_row("SELECT metadata_status FROM media_items", [], |r| r.get(0))
            .unwrap();
        assert_eq!(status, "pending", "item must stay pending, not matched");
    }

    /// RC5: the folder year reaches the provider's search input for TV groups,
    /// asserted on `ResolveInput.year` at the mock boundary, not on
    /// `QueryGroup.library_year` (the reference branch only tested the latter).
    #[test]
    fn tv_folder_year_reaches_provider_search_input() {
        struct YearCapture {
            years: std::sync::Mutex<Vec<Option<i32>>>,
        }
        impl MetadataSource for YearCapture {
            fn resolve(
                &self,
                input: &ResolveInput,
            ) -> Result<crate::resolve::ProviderResult, crate::resolve::ResolveError> {
                self.years.lock().unwrap().push(input.year);
                Ok(crate::resolve::ProviderResult::Miss)
            }
        }
        let c = Connection::open_in_memory().unwrap();
        migrate(&c).unwrap();
        c.execute_batch(
            "INSERT INTO libraries (name, path, kind) VALUES ('S', '/tmp/L', 'shows');
             INSERT INTO media_items (library_id, path, mtime_ms, size_bytes, title, kind, season, episode)
             VALUES (1, 'Top Gear (2002)/Season 1/Top.Gear.S01E01.mkv', 1, 1, 'Top Gear', 'episode', 1, 1);",
        )
        .unwrap();
        let src = YearCapture {
            years: std::sync::Mutex::new(Vec::new()),
        };
        let resolver = Resolver { tmdb: src };
        let s = drain_pending(
            &c,
            &resolver,
            &AtomicU64::new(0),
            &AtomicU64::new(0),
            DrainOptions::default(),
        )
        .unwrap();
        assert_eq!(s.groups, 1);
        assert_eq!(
            resolver.tmdb.years.lock().unwrap().as_slice(),
            &[Some(2002)],
            "folder (YYYY) must reach ResolveInput.year for TV search"
        );
    }

    /// **The filename year outranks the folder's, and only where both exist.**
    ///
    /// Both are the library's own labelling. Where they disagree the filename is
    /// what a renamer wrote from provider metadata and the folder is what a
    /// human typed once — measured over 1,777 movies, 19 disagree and the
    /// filename is the provider's year in all 19. Two of those were bound to
    /// the *wrong film* under the old order and reported as successes.
    ///
    /// The last two cases are the reach limit: with only one of the two present
    /// the precedence decides nothing, so a library that names the year once
    /// gets neither the fix nor a regression.
    #[test]
    fn the_filename_year_outranks_the_folder_year() {
        struct YearCapture {
            years: std::sync::Mutex<Vec<Option<i32>>>,
        }
        impl MetadataSource for YearCapture {
            fn resolve(
                &self,
                input: &ResolveInput,
            ) -> Result<crate::resolve::ProviderResult, crate::resolve::ResolveError> {
                self.years.lock().unwrap().push(input.year);
                Ok(crate::resolve::ProviderResult::Miss)
            }
        }
        for (path, parsed_year, want, why) in [
            // both present and disagreeing: the filename wins
            (
                "Split (2016)/Split (2017).mkv",
                Some(2017),
                Some(2017),
                "filename wins",
            ),
            // both present and agreeing: nothing to decide
            (
                "Some Film (2019)/Some Film (2019).mkv",
                Some(2019),
                Some(2019),
                "agree",
            ),
            // folder only: no filename year to outrank, so the folder still
            // reaches the provider
            (
                "Some Film (2019)/Some Film.mkv",
                None,
                Some(2019),
                "folder only",
            ),
            // filename only: nothing changes either
            (
                "Some Film/Some Film (2019).mkv",
                Some(2019),
                Some(2019),
                "filename only",
            ),
        ] {
            let c = Connection::open_in_memory().unwrap();
            migrate(&c).unwrap();
            c.execute(
                "INSERT INTO libraries (name, path, kind) VALUES ('M', '/tmp/M', 'movies')",
                [],
            )
            .unwrap();
            c.execute(
                "INSERT INTO media_items (library_id, path, mtime_ms, size_bytes, title, kind, year)
                 VALUES (1, ?1, 1, 1, 'Some Film', 'movie', ?2)",
                params![path, parsed_year],
            )
            .unwrap();
            let resolver = Resolver {
                tmdb: YearCapture {
                    years: std::sync::Mutex::new(Vec::new()),
                },
            };
            drain_pending(
                &c,
                &resolver,
                &AtomicU64::new(0),
                &AtomicU64::new(0),
                DrainOptions::default(),
            )
            .unwrap();
            assert_eq!(
                resolver.tmdb.years.lock().unwrap().as_slice(),
                &[want],
                "{why}: {path}"
            );
        }
    }

    fn movie_nfo_xml(title: &str, tmdb: u32) -> String {
        format!(
            "<movie><title>{title}</title><uniqueid type=\"tmdb\">{tmdb}</uniqueid>\
             <plot>A plot.</plot></movie>"
        )
    }

    /// The Kodi/Emby/Jellyfin movie layout, which is what Radarr writes and
    /// what 1,748 of 1,756 movie NFOs in the dogfood library use. Same-stem
    /// alone found eight of them, so every other movie was fetched from TMDB
    /// with a complete NFO beside it.
    #[test]
    fn movie_nfo_in_the_title_folder_is_found() {
        let base = std::env::temp_dir().join(format!(
            "nightjar-movie-nfo-folder-{}-{}",
            std::process::id(),
            line!()
        ));
        let _ = std::fs::remove_dir_all(&base);
        let library = base.join("Movies");
        let folder = library.join("Eagle Eye (2008)");
        std::fs::create_dir_all(&folder).unwrap();
        let media = folder.join("Eagle Eye (2008).mkv");
        std::fs::write(folder.join("movie.nfo"), movie_nfo_xml("Eagle Eye", 9982)).unwrap();

        let meta = nfo_sidecar_meta(&media, MetadataKind::Movie, library.to_str().unwrap())
            .expect("movie.nfo beside the media file must be read");
        assert_eq!(meta.title, "Eagle Eye");
        assert_eq!(meta.ids.tmdb, Some(9982));
        assert!(meta.is_nfo_complete(), "and it must skip the TMDB fetch");
        let _ = std::fs::remove_dir_all(&base);
    }

    /// Third candidate, for layouts that name the NFO after the folder rather
    /// than the file or the literal `movie.nfo`.
    #[test]
    fn folder_named_movie_nfo_is_found() {
        let base = std::env::temp_dir().join(format!(
            "nightjar-movie-nfo-folder-{}-{}",
            std::process::id(),
            line!()
        ));
        let _ = std::fs::remove_dir_all(&base);
        let library = base.join("Movies");
        let folder = library.join("Eagle Eye (2008)");
        std::fs::create_dir_all(&folder).unwrap();
        // Media file stem differs from the folder name, so `<stem>.nfo` misses.
        let media = folder.join("Eagle Eye (2008) Bluray-1080p.mkv");
        std::fs::write(
            folder.join("Eagle Eye (2008).nfo"),
            movie_nfo_xml("Eagle Eye", 9982),
        )
        .unwrap();

        let meta = nfo_sidecar_meta(&media, MetadataKind::Movie, library.to_str().unwrap())
            .expect("<foldername>.nfo must be read when the stem does not match");
        assert_eq!(meta.ids.tmdb, Some(9982));
        let _ = std::fs::remove_dir_all(&base);
    }

    /// Candidate order: the file-specific NFO describes *this* file, the folder
    /// ones describe the title, so the specific one wins when both exist.
    #[test]
    fn same_stem_nfo_beats_movie_nfo() {
        let base = std::env::temp_dir().join(format!(
            "nightjar-movie-nfo-folder-{}-{}",
            std::process::id(),
            line!()
        ));
        let _ = std::fs::remove_dir_all(&base);
        let library = base.join("Movies");
        let folder = library.join("Eagle Eye (2008)");
        std::fs::create_dir_all(&folder).unwrap();
        let media = folder.join("Eagle Eye (2008).mkv");
        std::fs::write(
            folder.join("Eagle Eye (2008).nfo"),
            movie_nfo_xml("From The Stem", 1),
        )
        .unwrap();
        std::fs::write(folder.join("movie.nfo"), movie_nfo_xml("From movie.nfo", 2)).unwrap();

        let meta = nfo_sidecar_meta(&media, MetadataKind::Movie, library.to_str().unwrap())
            .expect("same-stem NFO must still be read");
        assert_eq!(meta.title, "From The Stem");
        assert_eq!(meta.ids.tmdb, Some(1));
        let _ = std::fs::remove_dir_all(&base);
    }

    /// Autopsy D5 applied to movies: with a flat layout the media file's parent
    /// *is* the library root, so `Movies/movie.nfo` describes no single title.
    /// Reading it would apply one stray file's metadata to every movie in the
    /// library — the same failure as a `tvshow.nfo` above the library root.
    #[test]
    fn movie_nfo_at_the_library_root_is_not_read() {
        let base = std::env::temp_dir().join(format!(
            "nightjar-movie-nfo-folder-{}-{}",
            std::process::id(),
            line!()
        ));
        let _ = std::fs::remove_dir_all(&base);
        let library = base.join("Movies");
        std::fs::create_dir_all(&library).unwrap();
        // No title folder: the movie sits directly in the library root.
        let media = library.join("Eagle Eye (2008).mkv");
        std::fs::write(library.join("movie.nfo"), movie_nfo_xml("Wrong Movie", 1)).unwrap();
        std::fs::write(library.join("Movies.nfo"), movie_nfo_xml("Also Wrong", 2)).unwrap();

        assert_eq!(
            nfo_sidecar_xml(&media, MetadataKind::Movie, library.to_str().unwrap()),
            None,
            "a folder-level NFO at the library root must not describe one title"
        );

        // The same-stem candidate is unambiguous and still applies there.
        std::fs::write(
            library.join("Eagle Eye (2008).nfo"),
            movie_nfo_xml("Eagle Eye", 9982),
        )
        .unwrap();
        let meta = nfo_sidecar_meta(&media, MetadataKind::Movie, library.to_str().unwrap())
            .expect("same-stem NFO is unambiguous even at the library root");
        assert_eq!(meta.ids.tmdb, Some(9982));
        let _ = std::fs::remove_dir_all(&base);
    }

    /// First candidate that *reads* wins, not first that parses. A corrupt
    /// `movie.nfo` surfaces as `NfoInvalid` rather than silently resolving from
    /// a stale `<foldername>.nfo` that may describe a different cut entirely.
    #[test]
    fn corrupt_movie_nfo_does_not_fall_through_to_the_next_candidate() {
        let base = std::env::temp_dir().join(format!(
            "nightjar-movie-nfo-folder-{}-{}",
            std::process::id(),
            line!()
        ));
        let _ = std::fs::remove_dir_all(&base);
        let library = base.join("Movies");
        let folder = library.join("Eagle Eye (2008)");
        std::fs::create_dir_all(&folder).unwrap();
        // Stem does not match, so `movie.nfo` is the first candidate and the
        // folder-named NFO is the one it must not fall through to.
        let media = folder.join("Eagle Eye (2008) Bluray-1080p.mkv");
        std::fs::write(folder.join("movie.nfo"), "<movie><title>truncated").unwrap();
        std::fs::write(
            folder.join("Eagle Eye (2008).nfo"),
            movie_nfo_xml("Stale Fallback", 7),
        )
        .unwrap();

        let xml = nfo_sidecar_xml(&media, MetadataKind::Movie, library.to_str().unwrap())
            .expect("the corrupt NFO is still what gets read");
        assert!(
            xml.contains("truncated"),
            "movie.nfo must win on read, not on parse"
        );
        assert!(
            crate::nfo::parse_nfo(&xml).is_err(),
            "and the resolver decides on its content"
        );
        let _ = std::fs::remove_dir_all(&base);
    }

    /// RC5: a `tvshow.nfo` one level above the library root is **not** read —
    /// the walk-up is bounded by the library path, with the file present
    /// (autopsy D5; replaces the vacuous branch test that proved only
    /// termination).
    #[test]
    fn show_root_nfo_is_bounded_by_library_root() {
        let base =
            std::env::temp_dir().join(format!("nightjar-tvshow-bound-test-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&base);
        let library = base.join("library");
        std::fs::create_dir_all(library.join("Show/Season 1")).unwrap();
        std::fs::write(
            base.join("tvshow.nfo"),
            "<tvshow><title>Not This</title></tvshow>",
        )
        .unwrap();
        let media = library.join("Show/Season 1/ep.mkv");
        assert_eq!(
            show_root_nfo_xml(&media, library.to_str().unwrap()),
            None,
            "tvshow.nfo above the library root must not be read"
        );
        std::fs::write(
            library.join("Show/tvshow.nfo"),
            "<tvshow><title>Alpha</title><uniqueid type=\"tmdb\">55</uniqueid></tvshow>",
        )
        .unwrap();
        let got = show_root_nfo_xml(&media, library.to_str().unwrap());
        assert!(
            got.as_deref().is_some_and(|x| x.contains("Alpha")),
            "tvshow.nfo at the show root inside the library must be read"
        );
        let _ = std::fs::remove_dir_all(&base);
    }

    /// RC5: an episode with both `tvshow.nfo` (show tmdb id) and
    /// `episodedetails.nfo` (episode title) resolves series identity from the
    /// former without discarding the latter — the episode NFO stays readable
    /// on its own path (autopsy D5).
    #[test]
    fn episode_uses_tvshow_nfo_identity_and_keeps_episode_nfo_readable() {
        use crate::resolve::ProviderResult;
        use std::sync::atomic::{AtomicUsize, Ordering};
        static TVSHOW_NFO_SEARCHES: AtomicUsize = AtomicUsize::new(0);
        struct TvShowNfoSource;
        impl MetadataSource for TvShowNfoSource {
            fn resolve(
                &self,
                input: &ResolveInput,
            ) -> Result<ProviderResult, crate::resolve::ResolveError> {
                if input.tmdb_id.is_none() {
                    TVSHOW_NFO_SEARCHES.fetch_add(1, Ordering::SeqCst);
                }
                let id = input.tmdb_id.unwrap_or(55);
                let meta = CanonicalMetadata {
                    kind: MetadataKind::Show,
                    title: "Alpha".into(),
                    original_title: None,
                    year: Some(2002),
                    air_date: None,
                    plot: None,
                    genres: Vec::new(),
                    runtime_minutes: None,
                    cast: Vec::new(),
                    ratings: Vec::new(),
                    ids: crate::model::ProviderIds {
                        tmdb: Some(id),
                        tmdb_show: Some(id),
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
                    method: if input.tmdb_id.is_some() {
                        "tmdb_id"
                    } else {
                        "nfo_show"
                    },
                    confirmed: None,
                    raw: Some(crate::tmdb::RawProviderPayload {
                        entity_kind: "tv".into(),
                        provider_id: id.to_string(),
                        payload: format!(r#"{{"id":{id},"name":"Alpha"}}"#),
                    }),
                })
            }

            fn fetch_season(
                &self,
                show_id: i64,
                season_number: i32,
            ) -> Result<Option<crate::tmdb::RawProviderPayload>, crate::resolve::ResolveError>
            {
                assert_eq!(show_id, 55);
                assert_eq!(season_number, 1);
                let payload = r#"{
                    "season_number": 1,
                    "episodes": [
                      {"id": 1001, "name": "Pilot", "season_number": 1, "episode_number": 1, "air_date": "2002-01-01"}
                    ]
                }"#;
                Ok(Some(crate::tmdb::RawProviderPayload {
                    entity_kind: "season".into(),
                    provider_id: format!("{show_id}:{season_number}"),
                    payload: payload.into(),
                }))
            }
        }
        TVSHOW_NFO_SEARCHES.store(0, Ordering::SeqCst);
        let base = std::env::temp_dir().join(format!(
            "nightjar-tvshow-ep-nfo-test-{}",
            std::process::id()
        ));
        let _ = std::fs::remove_dir_all(&base);
        let library = base.join("library");
        std::fs::create_dir_all(library.join("Alpha/Season 1")).unwrap();
        std::fs::write(
            library.join("Alpha/tvshow.nfo"),
            r#"<?xml version="1.0" encoding="utf-8"?>
<tvshow><title>Alpha</title><year>2002</year>
<uniqueid type="tmdb">55</uniqueid></tvshow>"#,
        )
        .unwrap();
        std::fs::write(
            library.join("Alpha/Season 1/episodedetails.nfo"),
            r#"<?xml version="1.0" encoding="utf-8"?>
<episodedetails><title>Pilot</title><season>1</season><episode>1</episode></episodedetails>"#,
        )
        .unwrap();
        let c = Connection::open_in_memory().unwrap();
        migrate(&c).unwrap();
        c.execute_batch(&format!(
            "INSERT INTO libraries (name, path, kind) VALUES ('S', '{}', 'shows');
             INSERT INTO media_items (library_id, path, mtime_ms, size_bytes, title, kind, season, episode)
             VALUES (1, 'Alpha/Season 1/Alpha.S01E01.mkv', 1, 1, 'Alpha', 'episode', 1, 1);",
            library.to_str().unwrap()
        ))
        .unwrap();
        let s = drain_pending(
            &c,
            &Resolver {
                tmdb: TvShowNfoSource,
            },
            &AtomicU64::new(0),
            &AtomicU64::new(0),
            DrainOptions::default(),
        )
        .unwrap();
        assert_eq!(
            TVSHOW_NFO_SEARCHES.load(Ordering::SeqCst),
            0,
            "series identity must come from tvshow.nfo, not a search"
        );
        assert_eq!(s.items_ready, 1);
        let status: String = c
            .query_row("SELECT metadata_status FROM media_items", [], |r| r.get(0))
            .unwrap();
        assert_eq!(status, "ready");
        // The show id could only have come from tvshow.nfo — zero searches
        // ran, and the bind landed on show 55 (the mock asserts the id).
        let tv_row: i64 = c
            .query_row(
                "SELECT COUNT(*) FROM metadata_canonical
                 WHERE provider = 'tmdb' AND entity_kind = 'tv' AND provider_id = '55'",
                [],
                |r| r.get(0),
            )
            .unwrap();
        assert_eq!(
            tv_row, 1,
            "show 55 detail persisted (identity from tvshow.nfo)"
        );
        let ep_row: i64 = c
            .query_row(
                "SELECT COUNT(*) FROM metadata_canonical
                 WHERE provider = 'tmdb' AND entity_kind = 'episode' AND provider_id = '1001'",
                [],
                |r| r.get(0),
            )
            .unwrap();
        assert_eq!(
            ep_row, 1,
            "episode canonical projected from the season bind"
        );
        let ep_link: i64 = c
            .query_row(
                "SELECT COUNT(*) FROM media_item_links WHERE item_key = 'tmdb:episode:1001'",
                [],
                |r| r.get(0),
            )
            .unwrap();
        assert_eq!(ep_link, 1, "season bind lands the episode watch key");
        let episode_nfo = nfo_sidecar_meta(
            &resolve_media_path(library.to_str().unwrap(), "Alpha/Season 1/Alpha.S01E01.mkv"),
            MetadataKind::Episode,
            library.to_str().unwrap(),
        );
        assert_eq!(
            episode_nfo.as_ref().map(|m| m.title.as_str()),
            Some("Pilot"),
            "episodedetails.nfo must stay readable beside tvshow.nfo"
        );
        let _ = std::fs::remove_dir_all(&base);
    }

    /// RC8: two folders that fold to the same matcher soft key resolve to
    /// **different** series ids and neither inherits the other's (D2
    /// regression, ADR-0033 Q2/Q3). Folder-scoped grouping means each folder
    /// does its own resolve — the second folder cannot inherit the first's id
    /// through any cache, and the browse proxy splits them into two cards.
    #[test]
    fn fold_colliding_folders_resolve_to_different_series_ids() {
        struct ShamelessFoldSource {
            searches: Cell<usize>,
        }
        impl MetadataSource for ShamelessFoldSource {
            fn resolve(
                &self,
                input: &ResolveInput,
            ) -> Result<crate::resolve::ProviderResult, crate::resolve::ResolveError> {
                if input.tmdb_id.is_none() {
                    self.searches.set(self.searches.get() + 1);
                }
                let id = input.tmdb_id.unwrap_or(match input.year {
                    // TMDB show ids for the US (2011) and UK (2004) versions.
                    Some(2011) => 34343,
                    Some(2004) => 20610,
                    _ => 99999,
                });
                let meta = CanonicalMetadata {
                    kind: MetadataKind::Show,
                    title: "Shameless".into(),
                    original_title: None,
                    year: input.year,
                    air_date: None,
                    plot: None,
                    genres: Vec::new(),
                    runtime_minutes: None,
                    cast: Vec::new(),
                    ratings: Vec::new(),
                    ids: crate::model::ProviderIds {
                        tmdb: Some(id),
                        tmdb_show: Some(id),
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
                    confirmed: None,
                    raw: Some(crate::tmdb::RawProviderPayload {
                        entity_kind: "tv".into(),
                        provider_id: id.to_string(),
                        payload: format!(r#"{{"id":{id},"name":"Shameless"}}"#),
                    }),
                })
            }
        }

        let c = Connection::open_in_memory().unwrap();
        migrate(&c).unwrap();
        c.execute_batch(
            "INSERT INTO libraries (name, path, kind) VALUES ('S', '/tmp/S', 'shows');
             INSERT INTO media_items (library_id, path, mtime_ms, size_bytes, title, kind, season, episode)
             VALUES
               (1, 'Shameless (US) (2011)/Season 1/Shameless.US.S01E01.mkv', 1, 1, 'Shameless (US)', 'episode', 1, 1),
               (1, 'Shameless (UK) (2004)/Season 1/Shameless.UK.S01E01.mkv', 1, 1, 'Shameless (UK)', 'episode', 1, 1);",
        )
        .unwrap();

        // Group formation is folder-scoped: two groups despite one soft key.
        let proxy = snapshot_visible_proxy(&c).unwrap();
        let groups = pending_query_groups(&c, &proxy).unwrap();
        assert_eq!(
            groups.len(),
            2,
            "fold-colliding folders must not share a group"
        );
        assert_ne!(groups[0].show_folder, groups[1].show_folder);

        let src = ShamelessFoldSource {
            searches: Cell::new(0),
        };
        let resolver = Resolver { tmdb: src };
        let s = drain_pending(
            &c,
            &resolver,
            &AtomicU64::new(0),
            &AtomicU64::new(0),
            DrainOptions::default(),
        )
        .unwrap();
        assert_eq!(s.items_matched, 2);
        assert_eq!(
            resolver.tmdb.searches.get(),
            2,
            "each folder must resolve for itself — the second must not inherit the first's id"
        );

        let mut stmt = c
            .prepare(
                "SELECT m.title, l.item_key FROM media_item_links l
                 JOIN media_items m ON m.id = l.media_item_id
                 ORDER BY m.title",
            )
            .unwrap();
        let links: Vec<(String, String)> = stmt
            .query_map([], |r| Ok((r.get(0)?, r.get(1)?)))
            .unwrap()
            .collect::<Result<_, _>>()
            .unwrap();
        assert_eq!(
            links,
            vec![
                ("Shameless (UK)".to_string(), "tmdb:show:20610".to_string()),
                ("Shameless (US)".to_string(), "tmdb:show:34343".to_string()),
            ],
            "the two folders must bind different shows"
        );

        // Durable rows are folder-keyed and never merge by fold collision.
        let mut stmt = c
            .prepare("SELECT relpath, tmdb_show_id FROM series ORDER BY relpath")
            .unwrap();
        let rows: Vec<(String, i64)> = stmt
            .query_map([], |r| Ok((r.get(0)?, r.get(1)?)))
            .unwrap()
            .collect::<Result<_, _>>()
            .unwrap();
        assert_eq!(
            rows,
            vec![
                ("Shameless (UK) (2004)".to_string(), 20610),
                ("Shameless (US) (2011)".to_string(), 34343),
            ]
        );
        assert_ne!(
            rows[0].1, rows[1].1,
            "no shared series identity across folders"
        );

        // Browse proxy: two cards, one per folder identity.
        let proxy = snapshot_visible_proxy(&c).unwrap();
        let keys: HashSet<_> = proxy.units.iter().map(|u| u.unit_key.as_str()).collect();
        assert_eq!(
            keys,
            HashSet::from(["tv|tmdb:34343", "tv|tmdb:20610"]),
            "the fold-colliding folders must not share a browse card"
        );
    }

    /// Show identity mock: one search per folder, a poster on the show hit,
    /// and a season payload binding S1E1..E2.
    struct AlphaFolderSource {
        searches: Cell<usize>,
        resolve_calls: Cell<usize>,
    }

    impl MetadataSource for AlphaFolderSource {
        fn resolve(
            &self,
            input: &ResolveInput,
        ) -> Result<crate::resolve::ProviderResult, crate::resolve::ResolveError> {
            self.resolve_calls.set(self.resolve_calls.get() + 1);
            if input.tmdb_id.is_none() {
                self.searches.set(self.searches.get() + 1);
            }
            let id = input.tmdb_id.unwrap_or(55);
            let meta = CanonicalMetadata {
                kind: MetadataKind::Show,
                title: "Alpha".into(),
                original_title: None,
                year: Some(2002),
                air_date: None,
                plot: None,
                genres: Vec::new(),
                runtime_minutes: None,
                cast: Vec::new(),
                ratings: Vec::new(),
                ids: crate::model::ProviderIds {
                    tmdb: Some(id),
                    tmdb_show: Some(id),
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
            Ok(crate::resolve::ProviderResult::Hit {
                metadata: Box::new(meta),
                method: if input.tmdb_id.is_some() {
                    "tmdb_id"
                } else {
                    "exact_title"
                },
                confirmed: None,
                raw: Some(crate::tmdb::RawProviderPayload {
                    entity_kind: "tv".into(),
                    provider_id: id.to_string(),
                    payload: format!(r#"{{"id":{id},"name":"Alpha"}}"#),
                }),
            })
        }

        fn fetch_season(
            &self,
            show_id: i64,
            season_number: i32,
        ) -> Result<Option<crate::tmdb::RawProviderPayload>, crate::resolve::ResolveError> {
            assert_eq!(show_id, 55);
            assert_eq!(season_number, 1);
            let payload = r#"{
                "season_number": 1,
                "episodes": [
                  {"id": 1001, "name": "One", "season_number": 1, "episode_number": 1, "air_date": "2002-01-01"},
                  {"id": 1002, "name": "Two", "season_number": 1, "episode_number": 2, "air_date": "2002-01-08"}
                ]
            }"#;
            Ok(Some(crate::tmdb::RawProviderPayload {
                entity_kind: "season".into(),
                provider_id: format!("{show_id}:{season_number}"),
                payload: payload.into(),
            }))
        }
    }

    /// RC8: a new episode added under an already-identified folder binds with
    /// zero search calls — asserted on the exact mock search count, not a
    /// range (Gate 3 "rescan generates no search requests" for identified
    /// folders, ADR-0033 §8).
    #[test]
    fn new_episode_in_identified_folder_binds_with_zero_search_calls() {
        let c = Connection::open_in_memory().unwrap();
        migrate(&c).unwrap();
        c.execute_batch(
            "INSERT INTO libraries (name, path, kind) VALUES ('S', '/tmp/S', 'shows');
             INSERT INTO media_items (library_id, path, mtime_ms, size_bytes, title, kind, season, episode)
             VALUES (1, 'Alpha (2002)/Season 1/Alpha.S01E01.mkv', 1, 1, 'Alpha', 'episode', 1, 1);",
        )
        .unwrap();
        let src = AlphaFolderSource {
            searches: Cell::new(0),
            resolve_calls: Cell::new(0),
        };
        let resolver = Resolver { tmdb: src };
        let s1 = drain_pending(
            &c,
            &resolver,
            &AtomicU64::new(0),
            &AtomicU64::new(0),
            DrainOptions::default(),
        )
        .unwrap();
        assert_eq!(s1.items_matched, 1);
        assert_eq!(s1.items_ready, 1);
        assert_eq!(resolver.tmdb.searches.get(), 1, "first pass searches once");
        let searches_after_first = resolver.tmdb.searches.get();

        // A second episode lands in the same folder; the folder already has a
        // series row from the first pass.
        c.execute(
            "INSERT INTO media_items (library_id, path, mtime_ms, size_bytes, title, kind, season, episode)
             VALUES (1, 'Alpha (2002)/Season 1/Alpha.S01E02.mkv', 1, 1, 'Alpha', 'episode', 1, 2)",
            [],
        )
        .unwrap();
        let s2 = drain_pending(
            &c,
            &resolver,
            &AtomicU64::new(0),
            &AtomicU64::new(0),
            DrainOptions::default(),
        )
        .unwrap();
        assert_eq!(s2.items_ready, 1);
        assert_eq!(
            resolver.tmdb.searches.get(),
            searches_after_first,
            "stored folder identity must bind the new episode with zero search calls"
        );
        let status: String = c
            .query_row(
                "SELECT metadata_status FROM media_items WHERE season = 1 AND episode = 2",
                [],
                |r| r.get(0),
            )
            .unwrap();
        assert_eq!(status, "ready");
    }

    /// RC8: a reused series id produces the same canonical, link, and
    /// poster-warm effects as a fresh match — one hit path (Rule 4.11), the
    /// reference branch's cache-hit fork that skipped all three is gone.
    #[test]
    fn reused_series_id_has_same_canonical_link_poster_effects_as_fresh_match() {
        use std::sync::Mutex;
        use std::sync::atomic::{AtomicUsize, Ordering};
        static WARMED: AtomicUsize = AtomicUsize::new(0);
        static WARMED_TITLES: Mutex<Vec<String>> = Mutex::new(Vec::new());
        struct RecordWarm;
        impl PosterWarm for RecordWarm {
            fn on_matched(&self, item_ids: &[i64], metadata: &CanonicalMetadata) {
                assert_eq!(item_ids.len(), 1);
                assert_eq!(
                    metadata.artwork.first().map(|a| a.path.as_str()),
                    Some("/p.jpg"),
                    "the reused-id hit must carry the same artwork a fresh match does"
                );
                WARMED.fetch_add(1, Ordering::SeqCst);
                WARMED_TITLES.lock().unwrap().push(metadata.title.clone());
            }
        }

        let c = Connection::open_in_memory().unwrap();
        migrate(&c).unwrap();
        c.execute_batch(
            "INSERT INTO libraries (name, path, kind) VALUES ('S', '/tmp/S', 'shows');
             INSERT INTO media_items (library_id, path, mtime_ms, size_bytes, title, kind, season, episode)
             VALUES (1, 'Alpha (2002)/Season 1/Alpha.S01E01.mkv', 1, 1, 'Alpha', 'episode', 1, 1);",
        )
        .unwrap();
        let src = AlphaFolderSource {
            searches: Cell::new(0),
            resolve_calls: Cell::new(0),
        };
        let resolver = Resolver { tmdb: src };
        let opts = DrainOptions {
            poster_warm: Some(Box::new(RecordWarm)),
            ..DrainOptions::default()
        };
        let s1 =
            drain_pending(&c, &resolver, &AtomicU64::new(0), &AtomicU64::new(0), opts).unwrap();
        assert_eq!(s1.items_ready, 1);
        assert_eq!(WARMED.load(Ordering::SeqCst), 1, "fresh match warms once");

        c.execute(
            "INSERT INTO media_items (library_id, path, mtime_ms, size_bytes, title, kind, season, episode)
             VALUES (1, 'Alpha (2002)/Season 1/Alpha.S01E02.mkv', 1, 1, 'Alpha', 'episode', 1, 2)",
            [],
        )
        .unwrap();
        // Stamp the tv row so the reuse pass must prove it re-persists it: the
        // stored-id bind re-upserts the canonical row with a fresh
        // `projected_at`, so the row can no longer be pass 1's leftover
        // (verify issue 2 — artwork/link/poster assertions alone cannot tell).
        c.execute(
            "UPDATE metadata_canonical SET projected_at = '2000-01-01T00:00:00Z'
             WHERE provider = 'tmdb' AND entity_kind = 'tv' AND provider_id = '55'",
            [],
        )
        .unwrap();
        let s2 = drain_pending(
            &c,
            &resolver,
            &AtomicU64::new(0),
            &AtomicU64::new(0),
            DrainOptions {
                poster_warm: Some(Box::new(RecordWarm)),
                ..DrainOptions::default()
            },
        )
        .unwrap();
        assert_eq!(s2.items_ready, 1);
        assert_eq!(
            WARMED.load(Ordering::SeqCst),
            2,
            "a reused id goes through the same poster-warm as a fresh match"
        );
        assert!(
            WARMED_TITLES.lock().unwrap().iter().all(|t| t == "Alpha"),
            "both paths must warm the same show metadata"
        );

        // Same canonical: the tv row persists across the reuse, artwork kept.
        let (title, art, projected_at): (String, Option<String>, String) = c
            .query_row(
                "SELECT title, artwork_json, projected_at FROM metadata_canonical
                 WHERE provider = 'tmdb' AND entity_kind = 'tv' AND provider_id = '55'",
                [],
                |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?)),
            )
            .unwrap();
        assert_eq!(title, "Alpha");
        assert!(
            art.as_deref().is_some_and(|a| a.contains("/p.jpg")),
            "the persisted show row keeps its artwork through a reused-id bind"
        );
        assert_ne!(
            projected_at.as_str(),
            "2000-01-01T00:00:00Z",
            "the reuse pass re-persisted the canonical row; it is not pass 1's leftover"
        );
        // Same links: both episodes end bound through the same bind path.
        let mut stmt = c
            .prepare("SELECT item_key FROM media_item_links ORDER BY item_key")
            .unwrap();
        let links: Vec<String> = stmt
            .query_map([], |r| r.get(0))
            .unwrap()
            .collect::<Result<_, _>>()
            .unwrap();
        assert_eq!(links, vec!["tmdb:episode:1001", "tmdb:episode:1002"]);
    }

    /// A source that offers one second entity numbering its seasons from 1,
    /// and counts how many times it was asked. The count is the test for the
    /// zero-request rescan property.
    struct SecondEntitySource {
        searches: std::cell::Cell<usize>,
    }

    impl SecondEntitySource {
        fn new() -> Self {
            Self {
                searches: std::cell::Cell::new(0),
            }
        }
    }

    impl MetadataSource for SecondEntitySource {
        fn resolve(
            &self,
            _input: &crate::resolve::ResolveInput,
        ) -> Result<crate::resolve::ProviderResult, crate::resolve::ResolveError> {
            Ok(crate::resolve::ProviderResult::Miss)
        }

        fn second_entity_candidates(
            &self,
            _title: &str,
            exclude: i64,
        ) -> Result<Vec<crate::resolve::SecondEntityCandidate>, crate::resolve::ResolveError>
        {
            self.searches.set(self.searches.get() + 1);
            assert_ne!(exclude, 74321, "the bound primary is never a candidate");
            Ok(vec![crate::resolve::SecondEntityCandidate {
                tmdb_show_id: 74321,
                shape: crate::match_score::CandidateShape {
                    candidate_season_episodes: Some(vec![
                        (1, 1, "Eleven Years Later".to_string()),
                        (1, 2, "Rocket Man".to_string()),
                    ]),
                    ..Default::default()
                },
            }])
        }

        fn fetch_season(
            &self,
            show_id: i64,
            season_number: i32,
        ) -> Result<Option<crate::tmdb::RawProviderPayload>, crate::resolve::ResolveError> {
            assert_eq!(show_id, 74321);
            let payload = format!(
                r#"{{"season_number": {season_number}, "episodes": [
                     {{"id": 9001, "name": "Eleven Years Later", "season_number": {season_number}, "episode_number": 1}},
                     {{"id": 9002, "name": "Rocket Man", "season_number": {season_number}, "episode_number": 2}}
                   ]}}"#
            );
            Ok(Some(crate::tmdb::RawProviderPayload {
                entity_kind: "season".into(),
                provider_id: format!("{show_id}:{season_number}"),
                payload,
            }))
        }
    }

    /// Folder numbered S9, entity numbering the same episodes S1.
    fn unplaced_folder(c: &Connection) {
        c.execute_batch(
            "INSERT INTO libraries (id, name, path, kind)
                  VALUES (1, 'Shows', '/Shows', 'shows');
             INSERT INTO series (library_id, relpath, tmdb_show_id)
                  VALUES (1, 'Will & Grace', 4454);
             INSERT INTO series_entity_bindings
                  (library_id, relpath, tmdb_show_id, is_primary)
                  VALUES (1, 'Will & Grace', 4454, 1);
             INSERT INTO media_items
                  (id, library_id, path, mtime_ms, size_bytes, title, kind, season, episode)
             VALUES
                (1, 1, 'Will & Grace/Season 9/Will & Grace - S09E01 - Eleven Years Later.mkv',
                    1, 1, 'Will & Grace', 'episode', 9, 1),
                (2, 1, 'Will & Grace/Season 9/Will & Grace - S09E02 - Rocket Man.mkv',
                    1, 1, 'Will & Grace', 'episode', 9, 2);",
        )
        .unwrap();
    }

    fn scope() -> SecondEntityScope<'static> {
        SecondEntityScope {
            library_id: 1,
            show_folder: "Will & Grace",
            title: "Will & Grace",
            soft_key: "Will & Grace",
        }
    }

    #[test]
    fn agreeing_titles_write_a_secondary_binding_and_link_the_files() {
        let c = Connection::open_in_memory().unwrap();
        migrate(&c).unwrap();
        unplaced_folder(&c);
        let resolver = Resolver {
            tmdb: SecondEntitySource::new(),
        };

        let stats = bind_second_entities(&c, &resolver, scope(), &[1, 2], 4454).unwrap();
        assert_eq!(stats.entities_bound, 1);
        assert_eq!(stats.files_linked, 2);
        assert_eq!(stats.still_unplaced, 0);

        let row: (i64, i64, i32, i32, i32, i32) = c
            .query_row(
                "SELECT tmdb_show_id, is_primary, folder_season_start, folder_season_end,
                        entity_season_start, entity_season_end
                 FROM series_entity_bindings WHERE is_primary = 0",
                [],
                |r| {
                    Ok((
                        r.get(0)?,
                        r.get(1)?,
                        r.get(2)?,
                        r.get(3)?,
                        r.get(4)?,
                        r.get(5)?,
                    ))
                },
            )
            .unwrap();
        assert_eq!(row, (74321, 0, 9, 9, 1, 1), "folder S9 maps to entity S1");

        let links: i64 = c
            .query_row(
                "SELECT COUNT(*) FROM media_item_links WHERE item_key LIKE 'tmdb:episode:%'",
                [],
                |r| r.get(0),
            )
            .unwrap();
        assert_eq!(links, 2);
    }

    /// The primary is never rewritten, whatever is found. A primary that moves
    /// is a `series_key` change, restating the folder's identity to every
    /// consumer keyed on it.
    #[test]
    fn the_primary_binding_is_never_touched() {
        let c = Connection::open_in_memory().unwrap();
        migrate(&c).unwrap();
        unplaced_folder(&c);
        let resolver = Resolver {
            tmdb: SecondEntitySource::new(),
        };
        bind_second_entities(&c, &resolver, scope(), &[1, 2], 4454).unwrap();

        let primary: i64 = c
            .query_row(
                "SELECT tmdb_show_id FROM series_entity_bindings WHERE is_primary = 1",
                [],
                |r| r.get(0),
            )
            .unwrap();
        assert_eq!(primary, 4454);
        let series: i64 = c
            .query_row("SELECT tmdb_show_id FROM series", [], |r| r.get(0))
            .unwrap();
        assert_eq!(series, 4454, "the series row is untouched");
    }

    /// **Zero search requests on a rescan of a resolved folder** — the
    /// property ADR-0033 item 1 protects and Gate 3 depends on. The stored
    /// record exists to suppress the re-search; without it identity would be
    /// re-derived from search on every drain.
    #[test]
    fn a_resolved_folder_issues_no_search_on_rescan() {
        let c = Connection::open_in_memory().unwrap();
        migrate(&c).unwrap();
        unplaced_folder(&c);
        let resolver = Resolver {
            tmdb: SecondEntitySource::new(),
        };

        bind_second_entities(&c, &resolver, scope(), &[1, 2], 4454).unwrap();
        assert_eq!(resolver.tmdb.searches.get(), 1, "first pass searches once");

        bind_second_entities(&c, &resolver, scope(), &[1, 2], 4454).unwrap();
        assert_eq!(
            resolver.tmdb.searches.get(),
            1,
            "a rescan issues no further search"
        );
    }

    /// A source that finds a candidate whose titles do not agree.
    struct DisagreeingSource;

    impl MetadataSource for DisagreeingSource {
        fn resolve(
            &self,
            _input: &crate::resolve::ResolveInput,
        ) -> Result<crate::resolve::ProviderResult, crate::resolve::ResolveError> {
            Ok(crate::resolve::ProviderResult::Miss)
        }

        fn second_entity_candidates(
            &self,
            _title: &str,
            _exclude: i64,
        ) -> Result<Vec<crate::resolve::SecondEntityCandidate>, crate::resolve::ResolveError>
        {
            Ok(vec![crate::resolve::SecondEntityCandidate {
                tmdb_show_id: 55555,
                shape: crate::match_score::CandidateShape {
                    candidate_season_episodes: Some(vec![(
                        1,
                        1,
                        "Something Else Entirely".to_string(),
                    )]),
                    ..Default::default()
                },
            }])
        }
    }

    /// No agreement, no binding. The files stay unmatched — the bucket with
    /// three built consumers — rather than binding the best of a bad set.
    #[test]
    fn no_title_agreement_writes_no_binding() {
        let c = Connection::open_in_memory().unwrap();
        migrate(&c).unwrap();
        unplaced_folder(&c);
        let resolver = Resolver {
            tmdb: DisagreeingSource,
        };

        let stats = bind_second_entities(&c, &resolver, scope(), &[1, 2], 4454).unwrap();
        assert_eq!(stats.entities_bound, 0);
        assert_eq!(stats.files_linked, 0);
        assert_eq!(
            stats.still_unplaced, 2,
            "both files stay unplaced, and are reported"
        );

        let secondaries: i64 = c
            .query_row(
                "SELECT COUNT(*) FROM series_entity_bindings WHERE is_primary = 0",
                [],
                |r| r.get(0),
            )
            .unwrap();
        assert_eq!(secondaries, 0);
    }

    /// Issue 121 cleanup. A file linked to an episode of an entity the folder
    /// does not bind is mis-linked and returns to `unmatched`.
    #[test]
    fn a_file_bound_to_an_unbound_entity_is_unlinked() {
        let c = Connection::open_in_memory().unwrap();
        migrate(&c).unwrap();
        c.execute_batch(
            "INSERT INTO libraries (id, name, path, kind) VALUES (1, 'S', '/S', 'shows');
             INSERT INTO series (library_id, relpath, tmdb_show_id)
                  VALUES (1, 'Monster (2022)', 113988);
             INSERT INTO series_entity_bindings
                  (library_id, relpath, tmdb_show_id, is_primary)
                  VALUES (1, 'Monster (2022)', 113988, 1);
             INSERT INTO media_items
                  (id, library_id, path, mtime_ms, size_bytes, title, kind, season, episode,
                   metadata_status)
             VALUES (1, 1, 'Monster (2022)/Season 2/m.2x01.mkv', 1, 1, 'M', 'episode', 2, 1,
                     'ready');
             INSERT INTO metadata_canonical
                  (provider, entity_kind, provider_id, title, ids_json, tmdb_show,
                   season, episode, projected_at)
             VALUES ('tmdb', 'episode', '7001', 'Scream Building', '{}', 41974, 1, 1, 'now');
             INSERT INTO media_item_links (media_item_id, item_key)
                  VALUES (1, 'tmdb:episode:7001');",
        )
        .unwrap();

        let n = unlink_foreign_entity_links(&c, 1, "Monster (2022)", &[1]).unwrap();
        assert_eq!(n, 1);
        let links: i64 = c
            .query_row("SELECT COUNT(*) FROM media_item_links", [], |r| r.get(0))
            .unwrap();
        assert_eq!(links, 0, "the mis-link is gone");
        let status: String = c
            .query_row(
                "SELECT metadata_status FROM media_items WHERE id = 1",
                [],
                |r| r.get(0),
            )
            .unwrap();
        assert_eq!(status, "unmatched", "unlinked, not rebound");
    }

    /// **Will & Grace's 52 must survive.** A folder that binds a second entity
    /// under ADR-0046 holds that entity's episodes legitimately, and a repair
    /// reading only the primary would delete them.
    #[test]
    fn a_file_bound_to_a_secondary_entity_is_untouched() {
        let c = Connection::open_in_memory().unwrap();
        migrate(&c).unwrap();
        c.execute_batch(
            "INSERT INTO libraries (id, name, path, kind) VALUES (1, 'S', '/S', 'shows');
             INSERT INTO series (library_id, relpath, tmdb_show_id)
                  VALUES (1, 'Will & Grace', 4454);
             INSERT INTO series_entity_bindings
                  (library_id, relpath, tmdb_show_id, is_primary,
                   folder_season_start, folder_season_end,
                   entity_season_start, entity_season_end)
             VALUES (1, 'Will & Grace', 4454, 1, NULL, NULL, NULL, NULL),
                    (1, 'Will & Grace', 74321, 0, 9, 11, 1, 3);
             INSERT INTO media_items
                  (id, library_id, path, mtime_ms, size_bytes, title, kind, season, episode,
                   metadata_status)
             VALUES (1, 1, 'Will & Grace/Season 9/w.s09e01.mkv', 1, 1, 'W', 'episode', 9, 1,
                     'ready');
             INSERT INTO metadata_canonical
                  (provider, entity_kind, provider_id, title, ids_json, tmdb_show,
                   season, episode, projected_at)
             VALUES ('tmdb', 'episode', '9001', 'Eleven Years Later', '{}', 74321, 1, 1, 'now');
             INSERT INTO media_item_links (media_item_id, item_key)
                  VALUES (1, 'tmdb:episode:9001');",
        )
        .unwrap();

        let n = unlink_foreign_entity_links(&c, 1, "Will & Grace", &[1]).unwrap();
        assert_eq!(n, 0, "a secondary binding is a binding");
        let links: i64 = c
            .query_row("SELECT COUNT(*) FROM media_item_links", [], |r| r.get(0))
            .unwrap();
        assert_eq!(links, 1);
    }

    /// No binding record means no evidence about what the folder holds, and
    /// deleting links on absence of evidence is the failure this refuses.
    #[test]
    fn no_binding_record_means_no_unlinking() {
        let c = Connection::open_in_memory().unwrap();
        migrate(&c).unwrap();
        c.execute_batch(
            "INSERT INTO libraries (id, name, path, kind) VALUES (1, 'S', '/S', 'shows');
             INSERT INTO media_items
                  (id, library_id, path, mtime_ms, size_bytes, title, kind, season, episode)
             VALUES (1, 1, 'Orphan/Season 1/o.s01e01.mkv', 1, 1, 'O', 'episode', 1, 1);
             INSERT INTO metadata_canonical
                  (provider, entity_kind, provider_id, title, ids_json, tmdb_show,
                   season, episode, projected_at)
             VALUES ('tmdb', 'episode', '5001', 'Whatever', '{}', 999, 1, 1, 'now');
             INSERT INTO media_item_links (media_item_id, item_key)
                  VALUES (1, 'tmdb:episode:5001');",
        )
        .unwrap();

        assert_eq!(
            unlink_foreign_entity_links(&c, 1, "Orphan", &[1]).unwrap(),
            0
        );
    }

    /// A manual match is the operator's decision and outranks this repair
    /// (ADR-0028).
    #[test]
    fn a_manual_link_is_never_unlinked() {
        let c = Connection::open_in_memory().unwrap();
        migrate(&c).unwrap();
        c.execute_batch(
            "INSERT INTO libraries (id, name, path, kind) VALUES (1, 'S', '/S', 'shows');
             INSERT INTO series (library_id, relpath, tmdb_show_id) VALUES (1, 'Monster (2022)', 113988);
             INSERT INTO series_entity_bindings (library_id, relpath, tmdb_show_id, is_primary)
                  VALUES (1, 'Monster (2022)', 113988, 1);
             INSERT INTO media_items
                  (id, library_id, path, mtime_ms, size_bytes, title, kind, season, episode)
             VALUES (1, 1, 'Monster (2022)/Season 2/m.2x01.mkv', 1, 1, 'M', 'episode', 2, 1);
             INSERT INTO metadata_canonical
                  (provider, entity_kind, provider_id, title, ids_json, tmdb_show,
                   season, episode, projected_at)
             VALUES ('tmdb', 'episode', '7001', 'Scream Building', '{}', 41974, 1, 1, 'now');
             INSERT INTO media_item_links (media_item_id, item_key, manually_matched)
                  VALUES (1, 'tmdb:episode:7001', 1);",
        )
        .unwrap();

        assert_eq!(
            unlink_foreign_entity_links(&c, 1, "Monster (2022)", &[1]).unwrap(),
            0
        );
    }

    /// **The defect this slice shipped green once.** `ep_by_show` collects
    /// only items awaiting resolution, so a folder whose earlier seasons are
    /// already `ready` presents the resolver with just the seasons being
    /// retried. `Monster (2022)` retried its unmatched S2 and S3 while its
    /// stored entity holds only S1, so a confirmation built from the group saw
    /// nothing to compare and the correct id was discarded anyway.
    ///
    /// Every unit test for the confirmation passed against that, because each
    /// constructed the title list directly and none exercised what feeds it.
    /// This asserts the feed.
    #[test]
    fn folder_titles_include_files_that_are_not_pending() {
        let c = Connection::open_in_memory().unwrap();
        migrate(&c).unwrap();
        c.execute_batch(
            "INSERT INTO libraries (id, name, path, kind) VALUES (1, 'S', '/S', 'shows');
             INSERT INTO media_items
                  (id, library_id, path, mtime_ms, size_bytes, title, kind, season, episode,
                   metadata_status)
             VALUES
                (1, 1, 'Monster (2022)/Season 1/Monster - S01E03 - Blame it on the Rain.mkv',
                     1, 1, 'Monster', 'episode', 1, 3, 'ready'),
                (2, 1, 'Monster (2022)/Season 2/Monster - S02E01 - Spree.mkv',
                     1, 1, 'Monster', 'episode', 2, 1, 'pending');",
        )
        .unwrap();

        let titles = folder_titles_from_db(&c, 1, "Monster (2022)", "monster").unwrap();
        assert!(
            titles.iter().any(|(s, e, _)| *s == 1 && *e == 3),
            "the already-ready S1 file must be present — it is the only one the \
             stored entity can confirm against, and reading the pending group \
             alone is what let the wrong entity through"
        );
        assert_eq!(titles.len(), 2, "both files, whatever their status");
    }

    /// A folder read must not collect a neighbouring folder's files.
    #[test]
    fn folder_titles_do_not_leak_across_folders() {
        let c = Connection::open_in_memory().unwrap();
        migrate(&c).unwrap();
        c.execute_batch(
            "INSERT INTO libraries (id, name, path, kind) VALUES (1, 'S', '/S', 'shows');
             INSERT INTO media_items
                  (id, library_id, path, mtime_ms, size_bytes, title, kind, season, episode)
             VALUES
                (1, 1, 'Monster (2022)/Season 1/Monster - S01E03 - Blame it on the Rain.mkv',
                     1, 1, 'Monster', 'episode', 1, 3),
                (2, 1, 'Monster High/Season 1/Monster High - S01E01 - Scream Building.mkv',
                     1, 1, 'Monster High', 'episode', 1, 1);",
        )
        .unwrap();

        let titles = folder_titles_from_db(&c, 1, "Monster (2022)", "monster").unwrap();
        assert_eq!(
            titles.len(),
            1,
            "a prefix-sharing sibling folder is not this folder"
        );
    }

    fn reasons(c: &Connection) -> Vec<(i64, Option<String>, Option<String>)> {
        let mut st = c
            .prepare(
                "SELECT id, metadata_unmatched_reason, metadata_match_method
                 FROM media_items ORDER BY id",
            )
            .unwrap();
        st.query_map([], |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?)))
            .unwrap()
            .collect::<Result<_, _>>()
            .unwrap()
    }

    fn one_item(c: &Connection) {
        c.execute_batch(
            "INSERT INTO libraries (id, name, path, kind) VALUES (1, 'S', '/S', 'shows');
             INSERT INTO media_items (id, library_id, path, mtime_ms, size_bytes, title, kind)
             VALUES (1, 1, 'A/a.mkv', 1, 1, 'A', 'episode');",
        )
        .unwrap();
    }

    /// A failure writes the reason column and leaves the route column NULL.
    #[test]
    fn an_unmatched_exit_records_its_reason_and_no_route() {
        let c = Connection::open_in_memory().unwrap();
        migrate(&c).unwrap();
        one_item(&c);
        set_metadata_status_recording(
            &c,
            &[1],
            MetadataStatus::Unmatched,
            Decision::Token("no_episodes"),
        )
        .unwrap();
        assert_eq!(reasons(&c), vec![(1, Some("no_episodes".into()), None)]);
    }

    /// A success writes the route column and leaves the reason column NULL.
    #[test]
    fn a_ready_exit_records_its_route_and_no_reason() {
        let c = Connection::open_in_memory().unwrap();
        migrate(&c).unwrap();
        one_item(&c);
        set_metadata_status_recording(
            &c,
            &[1],
            MetadataStatus::Ready,
            Decision::Token("series_row"),
        )
        .unwrap();
        assert_eq!(reasons(&c), vec![(1, None, Some("series_row".into()))]);
    }

    /// **Neither column ever carries a stale value from the other state.** An
    /// item that failed and then succeeded must not keep its failure reason —
    /// that is the "cleared when the status leaves the state the token
    /// describes" rule, and it is the one a reader would otherwise trust.
    #[test]
    fn a_later_decision_clears_the_earlier_one() {
        let c = Connection::open_in_memory().unwrap();
        migrate(&c).unwrap();
        one_item(&c);
        set_metadata_status_recording(
            &c,
            &[1],
            MetadataStatus::Unmatched,
            Decision::Token("no_match"),
        )
        .unwrap();
        set_metadata_status_recording(
            &c,
            &[1],
            MetadataStatus::Ready,
            Decision::Token("exact_title"),
        )
        .unwrap();
        assert_eq!(reasons(&c), vec![(1, None, Some("exact_title".into()))]);

        set_metadata_status_recording(
            &c,
            &[1],
            MetadataStatus::Unmatched,
            Decision::Token("stored_id_404"),
        )
        .unwrap();
        assert_eq!(reasons(&c), vec![(1, Some("stored_id_404".into()), None)]);
    }

    /// `None` is the honest record for a decision nothing computes a cause
    /// for — every bind-time exit today. It must not leave a stale token
    /// behind either.
    #[test]
    fn no_token_records_nothing_and_clears_what_was_there() {
        let c = Connection::open_in_memory().unwrap();
        migrate(&c).unwrap();
        one_item(&c);
        set_metadata_status_recording(
            &c,
            &[1],
            MetadataStatus::Unmatched,
            Decision::Token("no_match"),
        )
        .unwrap();
        set_metadata_status_recording(&c, &[1], MetadataStatus::Unmatched, Decision::Uncomputed)
            .unwrap();
        assert_eq!(reasons(&c), vec![(1, None, None)]);
    }

    /// The plain wrapper keeps its old behaviour and records nothing.
    #[test]
    fn set_metadata_status_records_nothing() {
        let c = Connection::open_in_memory().unwrap();
        migrate(&c).unwrap();
        one_item(&c);
        set_metadata_status(&c, &[1], MetadataStatus::Ready).unwrap();
        assert_eq!(reasons(&c), vec![(1, None, None)]);
    }

    fn movie_meta() -> CanonicalMetadata {
        CanonicalMetadata {
            kind: MetadataKind::Movie,
            title: "A".into(),
            original_title: None,
            year: Some(1999),
            air_date: None,
            plot: None,
            genres: Vec::new(),
            runtime_minutes: None,
            cast: Vec::new(),
            ratings: Vec::new(),
            ids: crate::model::ProviderIds {
                tmdb: Some(550),
                tmdb_show: None,
                imdb: None,
                tvdb: None,
            },
            artwork: Vec::new(),
            collection: None,
            season: None,
            episode: None,
        }
    }

    /// **The gap the shipped column had, asserted at the call site.**
    ///
    /// `apply_search_hit` is where the search tier commits the entity the
    /// scorer chose. It previously set `Matched` through the plain writer, so
    /// the route was computed, logged and dropped — and the twelve scorer
    /// tokens had no producer at all. Every writer test passed, because the
    /// writer was correct; what was missing was a caller.
    #[test]
    fn the_search_tier_records_the_route_that_chose_the_entity() {
        let c = Connection::open_in_memory().unwrap();
        migrate(&c).unwrap();
        one_item(&c);
        let meta = CanonicalMetadata {
            kind: MetadataKind::Movie,
            title: "A".into(),
            original_title: None,
            year: Some(1999),
            air_date: None,
            plot: None,
            genres: Vec::new(),
            runtime_minutes: None,
            cast: Vec::new(),
            ratings: Vec::new(),
            ids: crate::model::ProviderIds {
                tmdb: Some(550),
                tmdb_show: None,
                imdb: None,
                tvdb: None,
            },
            artwork: Vec::new(),
            collection: None,
            season: None,
            episode: None,
        };
        let wrote = apply_search_hit(&c, &[1], &meta, Some("exact_title_year"), None).unwrap();
        assert!(wrote);
        assert_eq!(
            reasons(&c),
            vec![(1, None, Some("exact_title_year".into()))],
            "the scorer's route reaches the column"
        );
    }

    /// **The second field has a producer.** The first version of the route
    /// column shipped with a writer and no caller; this asserts the caller for
    /// the confirmation column before it is measured, so a run reporting all
    /// NULLs means "confirmation never fired", not "nothing writes it".
    #[test]
    fn the_search_tier_records_whether_episode_titles_agreed() {
        fn confirmed_col(c: &Connection) -> Option<i64> {
            c.query_row(
                "SELECT metadata_confirmed_by_episode_title FROM media_items WHERE id = 1",
                [],
                |r| r.get(0),
            )
            .unwrap()
        }
        let meta = movie_meta();

        // Not evaluated stays NULL — the state a `NOT NULL DEFAULT 0` would
        // have destroyed.
        let c = Connection::open_in_memory().unwrap();
        migrate(&c).unwrap();
        one_item(&c);
        apply_search_hit(&c, &[1], &meta, Some("exact_title_year"), None).unwrap();
        assert_eq!(confirmed_col(&c), None, "no evidence is not disagreement");

        // Evaluated and disagreed.
        let c = Connection::open_in_memory().unwrap();
        migrate(&c).unwrap();
        one_item(&c);
        apply_search_hit(&c, &[1], &meta, Some("exact_title_year"), Some(false)).unwrap();
        assert_eq!(confirmed_col(&c), Some(0));

        // Evaluated and agreed — and the route it agreed *with* survives, which
        // is the whole reason there are two fields.
        let c = Connection::open_in_memory().unwrap();
        migrate(&c).unwrap();
        one_item(&c);
        apply_search_hit(&c, &[1], &meta, Some("exact_title_year"), Some(true)).unwrap();
        assert_eq!(confirmed_col(&c), Some(1));
        assert_eq!(
            reasons(&c),
            vec![(1, None, Some("exact_title_year".into()))],
            "confirmation records itself without taking the route's column"
        );
    }

    /// **Enrich must not overwrite it.** Resolving details for an id the search
    /// tier already picked is not a second choice of entity, and reporting
    /// `tmdb_id` there is what made the column answer "how did enrich resolve".
    #[test]
    fn enrich_by_stored_id_keeps_the_route_that_chose_the_entity() {
        let c = Connection::open_in_memory().unwrap();
        migrate(&c).unwrap();
        one_item(&c);
        set_metadata_status_recording(
            &c,
            &[1],
            MetadataStatus::Matched,
            Decision::Token("exact_title_season_coverage"),
        )
        .unwrap();
        set_metadata_status_recording(&c, &[1], MetadataStatus::Ready, Decision::KeepRoute)
            .unwrap();
        let got = reasons(&c);
        assert_eq!(
            got,
            vec![(1, None, Some("exact_title_season_coverage".into()))],
            "the chosen route survives enrich"
        );
        let status: String = c
            .query_row(
                "SELECT metadata_status FROM media_items WHERE id = 1",
                [],
                |r| r.get(0),
            )
            .unwrap();
        assert_eq!(status, "ready", "and the status still moved");
    }
}
