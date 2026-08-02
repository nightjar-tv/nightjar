//! Metadata work queue as a query over `metadata_status` (ADR-0026 §8).
//!
//! No jobs table. Pending rows are selected, grouped by search `query_key`
//! (one provider resolve per group), ordered by recently-added then
//! everything else. Continue-watching / visible / search bands are reserved
//! for Block 2/3.

use std::collections::HashMap;

use rusqlite::{Connection, params};

use crate::clean::{clean_movie_title, clean_show_title, series_library_year, year_from_path};
use crate::model::MetadataKind;
use crate::negative_cache::query_key;
use crate::resolve::MetadataSource;
use crate::resolve::{ResolveInput, ResolveOutcome, Resolver, UnresolvedReason};

/// Priority bands (ADR-0026 §8 / strategy note). Lower ordinal = sooner.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub enum QueueBand {
    /// Block 2 — not wired until continue-watching exists.
    ContinueWatching = 0,
    /// Block 3 — not wired until browse surfaces exist.
    Visible = 1,
    /// Block 3 — not wired until search exists.
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

/// Today every pending item is [`QueueBand::RecentlyAdded`]; ordering within
/// the band is `id DESC` (insertion ≈ recently added). Wire higher bands
/// here when Block 2/3 enqueue sources exist — do not invent them now.
pub fn queue_band_for_item(_item_id: i64) -> QueueBand {
    QueueBand::RecentlyAdded
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
}

#[derive(Debug, Clone)]
pub struct PendingItem {
    pub id: i64,
    pub kind: String,
    pub title: String,
    pub year: Option<i32>,
    pub path: String,
    pub season: Option<i32>,
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
    /// Max media_items.id in the group — recently-added sort key.
    max_id: i64,
    band: QueueBand,
}

#[derive(Debug, Default)]
pub struct DrainStats {
    pub groups: usize,
    pub items_ready: usize,
    pub items_unmatched: usize,
    pub provider_resolves: usize,
    pub http_429: u64,
}

/// Load pending items and fold into resolve groups (newest groups first).
fn pending_query_groups(conn: &Connection) -> Result<Vec<QueryGroup>, String> {
    let mut stmt = conn
        .prepare(
            "SELECT id, kind, title, year, path, season
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
            })
        })
        .map_err(|e| format!("query pending: {e}"))?;

    let mut items = Vec::new();
    for row in rows {
        items.push(row.map_err(|e| format!("pending row: {e}"))?);
    }

    // Episode shape for collision pin: group raw episodes by cleaned show title.
    let mut ep_by_show: HashMap<String, Vec<&PendingItem>> = HashMap::new();
    for it in &items {
        if it.kind == "episode" {
            let (ct, _) = clean_show_title(&it.title);
            ep_by_show.entry(ct).or_default().push(it);
        }
    }

    let mut groups: HashMap<String, QueryGroup> = HashMap::new();
    for it in &items {
        let band = queue_band_for_item(it.id);
        match it.kind.as_str() {
            "movie" => {
                let folder_year = year_from_path(&it.path);
                let (ct, cy) = clean_movie_title(&it.title, folder_year.or(it.year));
                let qk = query_key(&ct, cy);
                let g = groups
                    .entry(format!("movie|{qk}"))
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
                let g = groups
                    .entry(format!("tv|{qk}"))
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

/// Drain pending groups through the resolver (store + neg-cache + limiter).
pub fn drain_pending<T: MetadataSource>(
    conn: &Connection,
    resolver: &Resolver<T>,
    http_429: &std::sync::atomic::AtomicU64,
) -> Result<DrainStats, String> {
    let groups = pending_query_groups(conn)?;
    let mut stats = DrainStats {
        groups: groups.len(),
        ..DrainStats::default()
    };
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
        let outcome = resolver
            .resolve_with_store(&input, conn)
            .map_err(|e| e.to_string())?;
        let status = match outcome {
            ResolveOutcome::Resolved { .. } => MetadataStatus::Ready,
            ResolveOutcome::Unresolved {
                reason: UnresolvedReason::NfoInvalid { .. },
            } => MetadataStatus::Unmatched,
            ResolveOutcome::Unresolved { .. } => MetadataStatus::Unmatched,
        };
        set_metadata_status(conn, &g.item_ids, status)?;
        match status {
            MetadataStatus::Ready => stats.items_ready += g.item_ids.len(),
            MetadataStatus::Unmatched => stats.items_unmatched += g.item_ids.len(),
            MetadataStatus::Pending => {}
        }
    }
    stats.http_429 = http_429.load(std::sync::atomic::Ordering::Relaxed);
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

    fn seeded() -> Connection {
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
        let c = seeded();
        let groups = pending_query_groups(&c).unwrap();
        assert_eq!(groups.len(), 2);
        assert!(groups[0].max_id > groups[1].max_id);
        assert_eq!(groups[0].band, QueueBand::RecentlyAdded);
    }

    #[test]
    fn drain_marks_stub_misses_unmatched_and_second_pass_is_empty() {
        let c = seeded();
        let resolver = Resolver { tmdb: TmdbStub };
        let http_429 = AtomicU64::new(0);
        let s1 = drain_pending(&c, &resolver, &http_429).unwrap();
        assert_eq!(s1.items_unmatched, 2);
        let pending: i64 = c
            .query_row(
                "SELECT COUNT(*) FROM media_items WHERE metadata_status = 'pending'",
                [],
                |r| r.get(0),
            )
            .unwrap();
        assert_eq!(pending, 0);
        let s2 = drain_pending(&c, &resolver, &http_429).unwrap();
        assert_eq!(s2.groups, 0);
        let _ = ApiRateLimiter::polite_default();
    }

    #[test]
    fn reserved_bands_sort_before_recently_added() {
        assert!(QueueBand::ContinueWatching < QueueBand::RecentlyAdded);
        assert!(QueueBand::Visible < QueueBand::RecentlyAdded);
        assert!(QueueBand::Search < QueueBand::RecentlyAdded);
        assert!(QueueBand::RecentlyAdded < QueueBand::Background);
    }
}
