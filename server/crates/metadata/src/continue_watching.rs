//! The continue-watching rollup (ADR-0035 item 8).
//!
//! One read per profile that collapses a series to one entry. The rule lives
//! here rather than in each client because three clients holding slightly
//! different ideas of "next episode" is how the rail stops being trusted
//! (ADR-0035 item 8).
//!
//! Candidates are keyed by the **effective** item identity, not by the stored
//! watch key: a row written under a `path:` key before a match and a row
//! written under the provider key afterwards name one logical item, and the
//! rail must not list it twice. Series identity is the folder-resolved
//! `series_key` (ADR-0039 item 2), so an unmatched show still groups. A movie
//! is its own series and its `series_key` is its own `item_key` (ADR-0039
//! item 2), so there is no separate movie branch in the grouping.
//!
//! Ordering inside a show comes from canonical season and episode numbers and
//! never from filenames (ADR-0035 item 8). Episodes without canonical numbers
//! group but do not order, so an unmatched show can resume and cannot advance.

use std::collections::btree_map::Entry;
use std::collections::{BTreeMap, BTreeSet, HashMap};

use rusqlite::{Connection, params, params_from_iter};

use nightjar_db::show_folder_relpath;

use crate::browse::{SeriesCache, SeriesEpisode, get_series_cached};
use crate::item_links::{
    EPISODE_KEY_PREFIX, MOVIE_KEY_PREFIX, PATH_KEY_PREFIX, effective_item_keys_for_library,
    parse_path_key, path_item_key, series_key_for_show_folder,
};
use crate::scope::{VisibilityCache, visible_item_ids_cached};
use nightjar_core::ViewerScope;

/// How many keys one batched identity query carries. Keeps the bound under
/// SQLite's parameter limit on any build while turning a per-row loop into a
/// handful of statements.
const KEY_CHUNK: usize = 400;

// Test-only statement and library-read counts, so a regression test can prove
// a request batches its identity and series reads instead of repeating them
// per row and per series. Thread-local keeps parallel tests separate.
#[cfg(test)]
thread_local! {
    static RESOLVE_QUERIES: std::cell::Cell<usize> = const { std::cell::Cell::new(0) };
    static IDENTITY_LIBRARY_READS: std::cell::Cell<usize> = const { std::cell::Cell::new(0) };
}

/// One entry on the rail, after series collapse.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ContinueWatchingEntry {
    /// Opaque series key (ADR-0039 item 2), returned as derived.
    pub series_key: String,
    /// Opaque item key of the episode or movie to resume (ADR-0025 §1).
    pub item_key: String,
    /// The media row to open.
    pub item_id: i64,
    /// Canonical title when the item has one, else the scan-derived title.
    pub title: String,
    /// `movie` | `episode`.
    pub kind: String,
    pub season: Option<i32>,
    pub episode: Option<i32>,
    /// The show's title for an episode, `None` for a movie.
    pub show_title: Option<String>,
    pub position_ms: i64,
    pub duration_ms: i64,
    pub played: bool,
    /// When this series entry was last touched. The rail sorts on this DESC.
    pub last_played_at: String,
}

/// Resolve one profile's rail.
///
/// `limit` is applied **after** series collapse, so a show with twenty watched
/// episodes still occupies one slot. `None` means no limit.
pub fn continue_watching(
    conn: &Connection,
    profile_id: i64,
    limit: Option<usize>,
    scope: &ViewerScope,
) -> Result<Vec<ContinueWatchingEntry>, String> {
    let rows = load_watch_rows(conn, profile_id)?;
    let candidates = resolve_candidates(conn, rows)?;

    // One filter at the query layer, applied **before** the series rollup
    // (ADR-0037 item 7). An over-cap candidate must not consume the series'
    // single slot, so invisible candidates never reach the collapse. The cache
    // is reused below for the episodes a series detail offers.
    let mut visibility = VisibilityCache::new();
    let candidate_ids: Vec<i64> = candidates.iter().map(|c| c.resolved.item_id).collect();
    let visible_candidates = visible_item_ids_cached(conn, scope, &candidate_ids, &mut visibility)?;
    let candidates: Vec<Candidate> = candidates
        .into_iter()
        .filter(|c| visible_candidates.contains(&c.resolved.item_id))
        .collect();

    // Collapse duplicate files by logical item: two stored keys that resolve to
    // one effective item key are one candidate. The newest activity wins, and
    // the stored key breaks a tie so the answer does not depend on row order.
    let mut by_item: BTreeMap<String, Candidate> = BTreeMap::new();
    for candidate in candidates {
        match by_item.get_mut(&candidate.resolved.effective_key) {
            Some(existing) => {
                if candidate.is_newer_than(existing) {
                    *existing = candidate;
                }
            }
            None => {
                by_item.insert(candidate.resolved.effective_key.clone(), candidate);
            }
        }
    }

    let mut groups: BTreeMap<String, Vec<Candidate>> = BTreeMap::new();
    for candidate in by_item.into_values() {
        groups
            .entry(candidate.resolved.series_key.clone())
            .or_default()
            .push(candidate);
    }

    // One cache for the whole request: every series below reads each library
    // it touches once, instead of once per series.
    let mut cache = SeriesCache::new();
    let mut entries = Vec::new();
    for (series_key, candidates) in groups {
        if let Some(entry) = collapse_series(
            conn,
            &mut cache,
            &mut visibility,
            &series_key,
            &candidates,
            scope,
        )? {
            entries.push(entry);
        }
    }

    // Deterministic ties: the rail sorts on last_played_at DESC, and a shared
    // timestamp falls back to the series key so two reads agree.
    entries.sort_by(|a, b| {
        b.last_played_at
            .cmp(&a.last_played_at)
            .then_with(|| a.series_key.cmp(&b.series_key))
    });
    if let Some(limit) = limit {
        entries.truncate(limit);
    }
    Ok(entries)
}

struct WatchRow {
    item_key: String,
    position_ms: i64,
    duration_ms: i64,
    played: bool,
    hidden: bool,
    last_played_at: String,
}

fn load_watch_rows(conn: &Connection, profile_id: i64) -> Result<Vec<WatchRow>, String> {
    let mut stmt = conn
        .prepare(
            "SELECT item_key, position_ms, duration_ms, played, hidden, last_played_at
             FROM watch_state WHERE profile_id = ?1",
        )
        .map_err(|e| format!("prepare watch rows: {e}"))?;
    let rows = stmt
        .query_map(params![profile_id], |r| {
            Ok(WatchRow {
                item_key: r.get(0)?,
                position_ms: r.get(1)?,
                duration_ms: r.get(2)?,
                played: r.get::<_, i64>(3)? != 0,
                hidden: r.get::<_, i64>(4)? != 0,
                last_played_at: r.get(5)?,
            })
        })
        .map_err(|e| format!("query watch rows: {e}"))?;
    rows.collect::<Result<Vec<_>, _>>()
        .map_err(|e| format!("watch row: {e}"))
}

/// A stored watch row resolved to its current identity.
struct Resolved {
    effective_key: String,
    series_key: String,
    item_id: i64,
    kind: String,
    title: String,
    season: Option<i32>,
    episode: Option<i32>,
}

struct Candidate {
    resolved: Resolved,
    position_ms: i64,
    duration_ms: i64,
    played: bool,
    hidden: bool,
    last_played_at: String,
}

impl Candidate {
    /// The newer activity wins; the effective key breaks the tie. This is the
    /// explicit deterministic rule the rail relies on when two stored keys
    /// collapse onto one logical item.
    fn is_newer_than(&self, other: &Candidate) -> bool {
        match self.last_played_at.cmp(&other.last_played_at) {
            std::cmp::Ordering::Greater => true,
            std::cmp::Ordering::Less => false,
            std::cmp::Ordering::Equal => self.resolved.effective_key < other.resolved.effective_key,
        }
    }
}

/// Resolve every watch row to its current identity in a bounded number of
/// statements: one set for the stored keys, one read of each touched library,
/// and one batch of canonical rows.
fn resolve_candidates(conn: &Connection, rows: Vec<WatchRow>) -> Result<Vec<Candidate>, String> {
    let keys: Vec<String> = rows.iter().map(|row| row.item_key.clone()).collect();
    let media = resolve_media_rows(conn, &keys)?;
    if media.is_empty() {
        return Ok(Vec::new());
    }

    let mut identity = IdentityCache::default();
    let mut resolved: Vec<(WatchRow, Resolved)> = Vec::new();
    for row in rows {
        let Some(media_row) = media.get(&row.item_key) else {
            continue;
        };
        let library_root = identity.library_root(conn, media_row.library_id)?;
        let effective_key = identity
            .effective_key(conn, media_row.library_id, media_row.item_id)?
            .unwrap_or_else(|| path_item_key(media_row.library_id, &media_row.relpath));
        let series_key = if media_row.kind == "episode" {
            let folder = show_folder_relpath(&media_row.relpath, &library_root);
            let show_id = identity.series_show(conn, media_row.library_id, &folder)?;
            series_key_for_show_folder(media_row.library_id, &folder, show_id)
        } else {
            effective_key.clone()
        };
        resolved.push((
            row,
            Resolved {
                effective_key,
                series_key,
                item_id: media_row.item_id,
                kind: media_row.kind.clone(),
                title: media_row.title.clone(),
                season: None,
                episode: None,
            },
        ));
    }

    // One read of the canonical rows the resolved identities name.
    let effective_keys: BTreeSet<String> = resolved
        .iter()
        .map(|(_, resolved)| resolved.effective_key.clone())
        .collect();
    let canonical = canonical_for_many(conn, &effective_keys)?;

    let mut candidates = Vec::with_capacity(resolved.len());
    for (row, mut resolved) in resolved {
        if let Some(canonical) = canonical.get(&resolved.effective_key) {
            resolved.title = canonical.title.clone();
            resolved.season = canonical.season;
            resolved.episode = canonical.episode;
        }
        candidates.push(Candidate {
            resolved,
            position_ms: row.position_ms,
            duration_ms: row.duration_ms,
            played: row.played,
            hidden: row.hidden,
            last_played_at: row.last_played_at,
        });
    }
    Ok(candidates)
}

/// The media row a stored watch key names now.
struct MediaRow {
    item_id: i64,
    library_id: i64,
    relpath: String,
    title: String,
    kind: String,
}

/// Resolve stored watch keys to media rows in batched statements.
///
/// A `path:` key carries its library, so a chunk resolves with one query.
/// Provider keys resolve through `media_item_links`, first row by media id,
/// which is the single-key rule. A key the identity layer cannot resolve is
/// simply absent from the result, the same as the per-key lookup returning
/// nothing.
fn resolve_media_rows(
    conn: &Connection,
    keys: &[String],
) -> Result<HashMap<String, MediaRow>, String> {
    let mut out: HashMap<String, MediaRow> = HashMap::new();

    let mut path_lookup: HashMap<(i64, String), String> = HashMap::new();
    for key in keys {
        if let Ok((library_id, relpath)) = parse_path_key(key) {
            path_lookup.insert((library_id, relpath.to_string()), key.clone());
        }
    }
    let path_pairs: Vec<(i64, String)> = path_lookup.keys().cloned().collect();
    for chunk in path_pairs.chunks(KEY_CHUNK) {
        #[cfg(test)]
        RESOLVE_QUERIES.with(|count| count.set(count.get() + 1));
        let clauses: Vec<&str> = chunk
            .iter()
            .map(|_| "(library_id = ? AND path = ?)")
            .collect();
        let sql = format!(
            "SELECT id, library_id, path, title, kind FROM media_items WHERE {}",
            clauses.join(" OR ")
        );
        let mut values: Vec<rusqlite::types::Value> = Vec::with_capacity(chunk.len() * 2);
        for (library_id, relpath) in chunk {
            values.push((*library_id).into());
            values.push(relpath.clone().into());
        }
        let mut stmt = conn
            .prepare(&sql)
            .map_err(|e| format!("prepare path media rows: {e}"))?;
        let rows = stmt
            .query_map(params_from_iter(values.iter()), |r| {
                Ok((
                    r.get::<_, i64>(0)?,
                    r.get::<_, i64>(1)?,
                    r.get::<_, String>(2)?,
                    r.get::<_, String>(3)?,
                    r.get::<_, String>(4)?,
                ))
            })
            .map_err(|e| format!("query path media rows: {e}"))?;
        for row in rows {
            let (item_id, library_id, relpath, title, kind) =
                row.map_err(|e| format!("path media row: {e}"))?;
            if let Some(key) = path_lookup.get(&(library_id, relpath.clone())) {
                out.insert(
                    key.clone(),
                    MediaRow {
                        item_id,
                        library_id,
                        relpath,
                        title,
                        kind,
                    },
                );
            }
        }
    }

    let provider_keys: Vec<&String> = keys
        .iter()
        .filter(|key| !key.starts_with(PATH_KEY_PREFIX))
        .collect();
    for chunk in provider_keys.chunks(KEY_CHUNK) {
        #[cfg(test)]
        RESOLVE_QUERIES.with(|count| count.set(count.get() + 1));
        let placeholders = vec!["?"; chunk.len()].join(",");
        let sql = format!(
            "SELECT l.item_key, m.id, m.library_id, m.path, m.title, m.kind
             FROM media_item_links l
             JOIN media_items m ON m.id = l.media_item_id
             WHERE l.item_key IN ({placeholders})
             ORDER BY l.item_key, m.id"
        );
        let mut stmt = conn
            .prepare(&sql)
            .map_err(|e| format!("prepare provider media rows: {e}"))?;
        let rows = stmt
            .query_map(
                params_from_iter(chunk.iter().map(|key| key.as_str())),
                |r| {
                    Ok((
                        r.get::<_, String>(0)?,
                        r.get::<_, i64>(1)?,
                        r.get::<_, i64>(2)?,
                        r.get::<_, String>(3)?,
                        r.get::<_, String>(4)?,
                        r.get::<_, String>(5)?,
                    ))
                },
            )
            .map_err(|e| format!("query provider media rows: {e}"))?;
        for row in rows {
            let (key, item_id, library_id, relpath, title, kind) =
                row.map_err(|e| format!("provider media row: {e}"))?;
            out.entry(key).or_insert(MediaRow {
                item_id,
                library_id,
                relpath,
                title,
                kind,
            });
        }
    }

    Ok(out)
}

/// Request-local library identity reads, one per library per request.
#[derive(Default)]
struct IdentityCache {
    library_roots: BTreeMap<i64, String>,
    effective_keys: BTreeMap<i64, HashMap<i64, String>>,
    series_shows: BTreeMap<i64, HashMap<String, i64>>,
}

impl IdentityCache {
    fn library_root(&mut self, conn: &Connection, library_id: i64) -> Result<String, String> {
        if let Entry::Vacant(entry) = self.library_roots.entry(library_id) {
            let root: String = conn
                .query_row(
                    "SELECT path FROM libraries WHERE id = ?1",
                    params![library_id],
                    |r| r.get(0),
                )
                .map_err(|e| format!("library root {library_id}: {e}"))?;
            entry.insert(root);
        }
        Ok(self
            .library_roots
            .get(&library_id)
            .expect("just inserted")
            .clone())
    }

    fn effective_key(
        &mut self,
        conn: &Connection,
        library_id: i64,
        item_id: i64,
    ) -> Result<Option<String>, String> {
        if let Entry::Vacant(entry) = self.effective_keys.entry(library_id) {
            #[cfg(test)]
            IDENTITY_LIBRARY_READS.with(|count| count.set(count.get() + 1));
            entry.insert(effective_item_keys_for_library(conn, library_id)?);
        }
        Ok(self
            .effective_keys
            .get(&library_id)
            .and_then(|keys| keys.get(&item_id).cloned()))
    }

    fn series_show(
        &mut self,
        conn: &Connection,
        library_id: i64,
        folder: &str,
    ) -> Result<Option<i64>, String> {
        if let Entry::Vacant(entry) = self.series_shows.entry(library_id) {
            #[cfg(test)]
            IDENTITY_LIBRARY_READS.with(|count| count.set(count.get() + 1));
            entry.insert(crate::browse::series_bindings(conn, library_id)?);
        }
        Ok(self
            .series_shows
            .get(&library_id)
            .and_then(|shows| shows.get(folder).copied()))
    }
}

/// Canonical title and numbering for an effective key, when the key names a
/// tmdb entity. A path key or a `tvdb:` key has no canonical row to read.
struct CanonicalItem {
    title: String,
    season: Option<i32>,
    episode: Option<i32>,
}

/// Canonical rows for a whole batch of effective keys, one statement per kind.
fn canonical_for_many(
    conn: &Connection,
    keys: &BTreeSet<String>,
) -> Result<HashMap<String, CanonicalItem>, String> {
    let mut out = HashMap::new();

    let episode_ids: Vec<&str> = keys
        .iter()
        .filter_map(|key| key.strip_prefix(EPISODE_KEY_PREFIX))
        .collect();
    for chunk in episode_ids.chunks(KEY_CHUNK) {
        let placeholders = vec!["?"; chunk.len()].join(",");
        let sql = format!(
            "SELECT provider_id, title, season, episode FROM metadata_canonical
             WHERE provider = 'tmdb' AND entity_kind = 'episode'
               AND provider_id IN ({placeholders})"
        );
        let mut stmt = conn
            .prepare(&sql)
            .map_err(|e| format!("prepare canonical episodes: {e}"))?;
        let rows = stmt
            .query_map(params_from_iter(chunk.iter().copied()), |r| {
                Ok((
                    r.get::<_, String>(0)?,
                    r.get::<_, String>(1)?,
                    r.get::<_, Option<i32>>(2)?,
                    r.get::<_, Option<i32>>(3)?,
                ))
            })
            .map_err(|e| format!("query canonical episodes: {e}"))?;
        for row in rows {
            let (id, title, season, episode) =
                row.map_err(|e| format!("canonical episode row: {e}"))?;
            out.insert(
                format!("{EPISODE_KEY_PREFIX}{id}"),
                CanonicalItem {
                    title,
                    season,
                    episode,
                },
            );
        }
    }

    let movie_ids: Vec<&str> = keys
        .iter()
        .filter_map(|key| key.strip_prefix(MOVIE_KEY_PREFIX))
        .collect();
    for chunk in movie_ids.chunks(KEY_CHUNK) {
        let placeholders = vec!["?"; chunk.len()].join(",");
        let sql = format!(
            "SELECT provider_id, title FROM metadata_canonical
             WHERE provider = 'tmdb' AND entity_kind = 'movie'
               AND provider_id IN ({placeholders})"
        );
        let mut stmt = conn
            .prepare(&sql)
            .map_err(|e| format!("prepare canonical movies: {e}"))?;
        let rows = stmt
            .query_map(params_from_iter(chunk.iter().copied()), |r| {
                Ok((r.get::<_, String>(0)?, r.get::<_, String>(1)?))
            })
            .map_err(|e| format!("query canonical movies: {e}"))?;
        for row in rows {
            let (id, title) = row.map_err(|e| format!("canonical movie row: {e}"))?;
            out.insert(
                format!("{MOVIE_KEY_PREFIX}{id}"),
                CanonicalItem {
                    title,
                    season: None,
                    episode: None,
                },
            );
        }
    }

    Ok(out)
}

/// Collapse one series' candidates to one rail entry, or `None` when the
/// series has nothing left to show.
fn collapse_series(
    conn: &Connection,
    cache: &mut SeriesCache,
    visibility: &mut VisibilityCache,
    series_key: &str,
    candidates: &[Candidate],
    scope: &ViewerScope,
) -> Result<Option<ContinueWatchingEntry>, String> {
    // `hidden` removes an item from the rail without marking it played
    // (ADR-0035 item 9), so hidden candidates never appear and never order.
    let visible: Vec<&Candidate> = candidates.iter().filter(|c| !c.hidden).collect();
    if visible.is_empty() {
        return Ok(None);
    }
    // A show is the only thing the rollup orders; everything else is a series
    // of one. `unknown` scan kinds therefore take the movie branch rather than
    // being handed to the episode walk with no canonical rows.
    let is_movie = candidates.iter().all(|c| c.resolved.kind != "episode");

    if is_movie {
        // A movie at the played threshold drops off the rail (ADR-0035 item 2),
        // exactly like a finished episode. A movie series holds one logical
        // item, so a played movie leaves nothing to show.
        let chosen = visible
            .iter()
            .copied()
            .filter(|c| !c.played)
            .max_by(|a, b| {
                a.last_played_at
                    .cmp(&b.last_played_at)
                    .then_with(|| b.resolved.effective_key.cmp(&a.resolved.effective_key))
            });
        return Ok(chosen.map(|chosen| entry_from(chosen, None)));
    }

    // A show: the newest in-progress episode if there is one, otherwise the
    // next unwatched episode after the highest completed ordinal.
    let in_progress = visible
        .iter()
        .copied()
        .filter(|c| !c.played)
        .max_by(|a, b| {
            a.last_played_at
                .cmp(&b.last_played_at)
                .then_with(|| b.resolved.effective_key.cmp(&a.resolved.effective_key))
        });
    let Some(detail) = get_series_cached(conn, series_key, cache, visibility, scope)? else {
        return Ok(None);
    };
    // Visibility before the rollup: the shared scoped-series helper applies the
    // same viewer scope and the same request-local visibility cache the
    // candidates above used, so the next-episode walk never offers an episode
    // the viewer cannot see and the filter is not written a second time here
    // (Rule 4.11).

    let show_title = Some(detail.title.clone());
    // One numbering scheme for the whole collapse: the folder's, which is what
    // `get_series` reports and what browse shows (ADR-0046 item 3a). A
    // renumbered binding translates its canonical season there, so the walk and
    // `highest_completed` compare like with like instead of canonical ordinals
    // against folder ordinals. A single-entity folder is unchanged, because its
    // folder numbering is its canonical numbering.
    let folder_numbers: BTreeMap<&str, (Option<i32>, Option<i32>)> = detail
        .seasons
        .iter()
        .flat_map(|season| season.episodes.iter())
        .chain(detail.unnumbered.iter())
        .map(|episode| (episode.item_key.as_str(), (episode.season, episode.episode)))
        .collect();
    // The series' most recent non-hidden activity is the rail's sort key
    // (ADR-0035 item 8; second amendment item 3), for a next-episode entry and
    // for an in-progress one whose chosen episode is not the newest row.
    let group_last = visible
        .iter()
        .map(|c| c.last_played_at.as_str())
        .max()
        .expect("visible is non-empty")
        .to_string();

    if let Some(chosen) = in_progress {
        let mut entry = entry_from(chosen, show_title);
        entry.last_played_at = group_last;
        if let Some((season, episode)) = folder_numbers.get(chosen.resolved.effective_key.as_str())
        {
            entry.season = *season;
            entry.episode = *episode;
        }
        return Ok(Some(entry));
    }

    let played: BTreeSet<&str> = candidates
        .iter()
        .filter(|c| c.played)
        .map(|c| c.resolved.effective_key.as_str())
        .collect();
    let hidden: BTreeSet<&str> = candidates
        .iter()
        .filter(|c| c.hidden)
        .map(|c| c.resolved.effective_key.as_str())
        .collect();
    let highest_completed = candidates
        .iter()
        .filter(|c| c.played)
        .filter_map(|c| {
            folder_numbers
                .get(c.resolved.effective_key.as_str())
                .copied()
        })
        .filter_map(|(season, episode)| match (season, episode) {
            (Some(season), Some(episode)) => Some((season, episode)),
            _ => None,
        })
        .max();

    let next = first_next_episode(&detail, highest_completed, &played, &hidden);
    Ok(next.map(|episode| ContinueWatchingEntry {
        series_key: series_key.to_string(),
        item_key: episode.item_key.clone(),
        item_id: episode.item_id,
        title: episode.title.clone(),
        kind: "episode".to_string(),
        season: episode.season,
        episode: episode.episode,
        show_title,
        position_ms: 0,
        duration_ms: 0,
        played: false,
        last_played_at: group_last,
    }))
}

/// The first available unwatched episode after `highest_completed`, in the
/// folder's numbering that `detail.seasons` carries.
///
/// Numbers only, never filenames (ADR-0035 item 8). `detail.seasons` arrives
/// ordered by season and then episode, and the translation a renumbered binding
/// needs is already applied there (ADR-0046 item 3a), so the walk and the
/// completed ordinal compare in one scheme.
fn first_next_episode<'a>(
    detail: &'a crate::browse::SeriesDetail,
    highest_completed: Option<(i32, i32)>,
    played: &BTreeSet<&str>,
    hidden: &BTreeSet<&str>,
) -> Option<&'a SeriesEpisode> {
    for season in &detail.seasons {
        for episode in &season.episodes {
            let (Some(season_number), Some(episode_number)) = (episode.season, episode.episode)
            else {
                continue;
            };
            if let Some((high_season, high_episode)) = highest_completed
                && (season_number, episode_number) <= (high_season, high_episode)
            {
                continue;
            }
            if played.contains(episode.item_key.as_str()) {
                continue;
            }
            if hidden.contains(episode.item_key.as_str()) {
                continue;
            }
            return Some(episode);
        }
    }
    None
}

fn entry_from(candidate: &Candidate, show_title: Option<String>) -> ContinueWatchingEntry {
    ContinueWatchingEntry {
        series_key: candidate.resolved.series_key.clone(),
        item_key: candidate.resolved.effective_key.clone(),
        item_id: candidate.resolved.item_id,
        title: candidate.resolved.title.clone(),
        kind: candidate.resolved.kind.clone(),
        season: candidate.resolved.season,
        episode: candidate.resolved.episode,
        show_title,
        position_ms: candidate.position_ms,
        duration_ms: candidate.duration_ms,
        played: candidate.played,
        last_played_at: candidate.last_played_at.clone(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use nightjar_db::migrate;

    /// `shows` folders of `episodes` episodes each, every episode in progress.
    fn seed_shows(conn: &Connection, shows: usize, episodes: usize) -> i64 {
        conn.execute_batch(
            "INSERT INTO libraries (id, name, path, kind)
                  VALUES (1, 'Shows', '/Shows', 'shows');",
        )
        .unwrap();
        let (_, profile_id) =
            nightjar_db::create_account_with_profile(conn, "p", "hash", "owner", "P", "aa")
                .unwrap();
        for show in 0..shows {
            let show_id = 1000 + show as i64;
            let folder = format!("Show{show}");
            conn.execute(
                "INSERT INTO series (library_id, relpath, tmdb_show_id) VALUES (1, ?1, ?2)",
                params![folder, show_id],
            )
            .unwrap();
            conn.execute(
                "INSERT INTO metadata_canonical
                    (provider, entity_kind, provider_id, title, ids_json, projected_at)
                 VALUES ('tmdb', 'tv', ?1, ?2, '{}', 'now')",
                params![show_id.to_string(), format!("Show {show}")],
            )
            .unwrap();
            for episode in 1..=episodes {
                let item_id = (show * episodes + episode) as i64;
                let episode_id = show_id * 100 + episode as i64;
                conn.execute(
                    "INSERT INTO media_items
                        (id, library_id, path, mtime_ms, size_bytes, title, kind, season, episode)
                     VALUES (?1, 1, ?2, 1, 1, ?3, 'episode', 1, ?4)",
                    params![
                        item_id,
                        format!("{folder}/S01E{episode:02}.mkv"),
                        format!("E{episode}"),
                        episode as i32
                    ],
                )
                .unwrap();
                conn.execute(
                    "INSERT INTO media_item_links (media_item_id, item_key) VALUES (?1, ?2)",
                    params![item_id, format!("tmdb:episode:{episode_id}")],
                )
                .unwrap();
                conn.execute(
                    "INSERT INTO metadata_canonical
                        (provider, entity_kind, provider_id, title, season, episode,
                         tmdb_show, ids_json, projected_at)
                     VALUES ('tmdb', 'episode', ?1, ?2, 1, ?3, ?4, '{}', 'now')",
                    params![
                        episode_id.to_string(),
                        format!("Ep{episode}"),
                        episode as i32,
                        show_id
                    ],
                )
                .unwrap();
                conn.execute(
                    "INSERT INTO watch_state
                        (profile_id, item_key, position_ms, duration_ms, played, hidden,
                         first_played_at, last_played_at)
                     VALUES (?1, ?2, 1000, 10000, 0, 0, ?3, ?3)",
                    params![
                        profile_id,
                        format!("tmdb:episode:{episode_id}"),
                        format!("2026-09-12T10:00:{episode:02}.000Z")
                    ],
                )
                .unwrap();
            }
        }
        profile_id
    }

    fn reset_counters() {
        crate::browse::reset_library_views();
        RESOLVE_QUERIES.with(|count| count.set(0));
        IDENTITY_LIBRARY_READS.with(|count| count.set(0));
    }

    /// Bounded-work regression: one rail request resolves every stored key in
    /// one batch and reads each touched library once, however many series the
    /// profile watches.
    #[test]
    fn a_rail_request_batches_identity_and_series_reads() {
        let conn = Connection::open_in_memory().unwrap();
        migrate(&conn).unwrap();
        let profile_id = seed_shows(&conn, 6, 4);
        reset_counters();

        let entries = continue_watching(&conn, profile_id, None, &ViewerScope::Account).unwrap();
        assert_eq!(entries.len(), 6, "one entry per show");

        assert_eq!(
            crate::browse::library_views_built(),
            1,
            "the series detail reads the one library once, not once per show"
        );
        assert_eq!(
            IDENTITY_LIBRARY_READS.with(std::cell::Cell::get),
            2,
            "effective keys and series bindings are each read once for the library"
        );
        assert_eq!(
            RESOLVE_QUERIES.with(std::cell::Cell::get),
            1,
            "24 provider keys resolve in one statement, not one per key"
        );
    }

    /// The path and provider shapes each resolve in their own batch, and a
    /// duplicate pair collapses to one entry, so the batched lookup keeps the
    /// single-key rule.
    #[test]
    fn batched_lookup_keeps_the_single_key_rule() {
        let conn = Connection::open_in_memory().unwrap();
        migrate(&conn).unwrap();
        conn.execute_batch(
            "INSERT INTO libraries (id, name, path, kind)
                  VALUES (1, 'Movies', '/Movies', 'movies');",
        )
        .unwrap();
        let (_, profile_id) =
            nightjar_db::create_account_with_profile(&conn, "p", "hash", "owner", "P", "aa")
                .unwrap();
        conn.execute_batch(
            "INSERT INTO media_items (id, library_id, path, mtime_ms, size_bytes, title, kind)
                  VALUES (1, 1, 'A.mkv', 1, 1, 'A', 'movie'),
                         (2, 1, 'A-dup.mkv', 1, 1, 'A', 'movie');
             INSERT INTO media_item_links (media_item_id, item_key)
                  VALUES (1, 'tmdb:movie:550'), (2, 'tmdb:movie:550');
             INSERT INTO metadata_canonical
                  (provider, entity_kind, provider_id, title, ids_json, projected_at)
                  VALUES ('tmdb', 'movie', '550', 'Alpha', '{}', 'now');",
        )
        .unwrap();
        // The pre-match path row and the post-match provider row are one item.
        conn.execute(
            "INSERT INTO watch_state
                (profile_id, item_key, position_ms, duration_ms, played, hidden,
                 first_played_at, last_played_at)
             VALUES (?1, 'path:1:A.mkv', 9000, 10000, 0, 0, ?2, ?2),
                    (?1, 'tmdb:movie:550', 4000, 10000, 0, 0, ?3, ?3)",
            params![
                profile_id,
                "2026-09-12T09:00:00.000Z",
                "2026-09-12T10:00:00.000Z"
            ],
        )
        .unwrap();
        reset_counters();

        let entries = continue_watching(&conn, profile_id, None, &ViewerScope::Account).unwrap();
        assert_eq!(entries.len(), 1, "the duplicate collapses to one entry");
        assert_eq!(entries[0].item_key, "tmdb:movie:550");
        assert_eq!(entries[0].position_ms, 4000, "the newer row wins");
        assert_eq!(
            RESOLVE_QUERIES.with(std::cell::Cell::get),
            2,
            "one path batch plus one provider batch"
        );
    }
}
