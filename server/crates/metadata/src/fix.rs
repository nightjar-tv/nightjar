//! Manual metadata fix operations (ADR-0028).
//!
//! Four ops: search candidates, assign, clear, retry. Artwork invalidate is
//! a no-op until ADR-0027 pipeline exists.

use rusqlite::{Connection, OptionalExtension, params};

use nightjar_db::show_folder_relpath;

use crate::canonical;
use crate::clean::{clean_movie_title, clean_show_title, year_from_path};
use crate::item_links::{
    clear_all_links_for_media_item, effective_item_key, link_keys_for_item, path_item_key,
    set_manually_matched, upsert_link,
};
use crate::match_score::{SearchHit, SearchKind, score_search};
use crate::migrator::{self, MigrateReport};
use crate::model::item_key_for_metadata;
use crate::negative_cache::{self, CacheKind, PROVIDER_TMDB, query_key};
use crate::queue::BindStats;
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
    /// ADR-0048 B: the candidate the shipped scorer chose, below the
    /// auto-match floor. **A suggestion the caller confirms, never a binding**
    /// — nothing in this crate assigns on it, and `assign` still requires the
    /// id the user picked.
    pub suggested: bool,
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

/// The candidate the shipped scorer chose for these hits (ADR-0048 B).
///
/// **The scorer, not a second rule.** For a movie the drain reaches
/// [`score_search`] with the same cleaned title and year and no candidate
/// shapes, so what comes back here is the entity that scored below the floor —
/// not a new opinion about which entity that should have been. Reimplementing
/// the choice is the trap `norm_key` already fell into once.
///
/// **Movies only.** ADR-0048's 88.9% was measured over `movie.noyear`'s failing
/// population, where a yearless collision has no discriminator left: a movie has
/// no seasons, so season coverage, slots explained and per-season counts all
/// have no analogue. A TV suggestion computed here would be scored *without* the
/// season shape the drain has — no `folder_seasons`, no candidate detail counts,
/// no reference episode title — and so would be a different and unmeasured
/// signal wearing the same badge.
fn suggested_candidate(
    hits: &[SearchHit],
    title: &str,
    year: Option<i32>,
    kind: SearchKind,
) -> Option<i64> {
    match kind {
        SearchKind::Movie => score_search(hits, title, year, kind).map(|c| c.tmdb_id),
        SearchKind::Tv => None,
    }
}

/// Search TMDB for assign candidates (floor does not apply).
///
/// One candidate may come back `suggested` (ADR-0048 B): the entity the shipped
/// scorer picked and the floor then declined. It changes nothing about what this
/// function returns or what `assign` will accept — it is a rank, and the user
/// still chooses.
///
/// `_year` is still accepted so the endpoint's query contract is unchanged
/// (Rule 2.3), and it is no longer used: search stopped narrowing on year at
/// the provider. Note that this year is **human-supplied**, unlike the drain's
/// folder-derived one, so whether the fix flow should narrow on it is a
/// separate decision from the one that removed the drain's filter.
pub fn search_candidates(
    client: &TmdbClient,
    item: &FixItemView,
    q: Option<&str>,
    _year: Option<i32>,
) -> Result<Vec<FixCandidate>, String> {
    let search_kind = if item.kind == "episode" {
        SearchKind::Tv
    } else {
        SearchKind::Movie
    };
    // The year the *scorer* gets, which is the year that came out of the same
    // clean as the title. A caller-supplied `q` carries no year we can trust to
    // stand for the file, so the suggestion is scored yearless there — which is
    // the yearless case ADR-0048 is about anyway.
    //
    // Note the drain reads `it.year.or(folder_year)` and this route reads
    // `year_from_path(..).or(item.year)`. The two precedences disagree; that
    // predates this change and is left alone rather than moved underneath it.
    let (title, year) = match q.map(str::trim).filter(|s| !s.is_empty()) {
        Some(given) => (given.to_string(), None),
        None if item.kind == "episode" => (clean_show_title(&item.title).0, None),
        None => clean_movie_title(&item.title, year_from_path(&item.path).or(item.year)),
    };
    // **An absent title is not a query.** `parse_filename` returns an empty
    // title when the filename carries none (`S03E09 WS PDTV XviD FUtV`), and
    // the scanner borrows the folder's name — but a file sitting directly in
    // the library root has no folder to borrow from, so the stored title can
    // still be empty. Searching on it asks the provider for everything and
    // means nothing.
    //
    // The drain's own path already refuses this: `MetadataSource::resolve`
    // filters an empty title to `Miss` before any request. This is the one
    // route that did not, because it substitutes the stored title when the
    // caller supplies no query of its own.
    if title.trim().is_empty() {
        return Ok(Vec::new());
    }
    let hits = client
        .search(search_kind, &title)
        .map_err(|e| e.to_string())?;
    Ok(candidates_from_hits(hits, &title, year, search_kind))
}

/// The search response, as the fix flow sees it.
///
/// Split from [`search_candidates`] so the suggestion can be tested through the
/// list it lands on rather than only where it is computed: `search_candidates`
/// takes a concrete [`TmdbClient`], so nothing can call it without a provider,
/// and a flag that is right in one function and dropped in the next is exactly
/// the kind of gap this codebase keeps finding.
fn candidates_from_hits(
    hits: Vec<SearchHit>,
    query: &str,
    year: Option<i32>,
    search_kind: SearchKind,
) -> Vec<FixCandidate> {
    let suggested = suggested_candidate(&hits, query, year, search_kind);
    hits.into_iter()
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
                suggested: suggested == Some(h.id),
                id: h.id,
                title,
                year,
            }
        })
        .collect()
}

#[derive(Debug)]
pub struct AssignResult {
    pub item_key: String,
    pub migrate: MigrateReport,
    /// Season/link counters from this assign's bind, so a manual fix's provider
    /// work is visible the way the drain's is. Default for a movie assign,
    /// which does not bind seasons.
    pub bind: BindStats,
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
    let mut bind = BindStats::default();
    let (new_key, meta) = match kind {
        "movie" => {
            let (meta, raw) = client
                .movie_detail(req.tmdb_id)
                .map_err(|e| e.to_string())?;
            canonical::persist_mapped_hit(conn, PROVIDER_TMDB, &raw, &meta)
                .map_err(|e| e.to_string())?;
            let key = item_key_for_metadata(&meta)
                .ok_or_else(|| "movie detail missing tmdb id".to_string())?;
            let tx = conn
                .unchecked_transaction()
                .map_err(|e| format!("begin assign tx: {e}"))?;
            clear_all_links_for_media_item(&tx, item.id)?;
            upsert_link(&tx, item.id, &key, true)?;
            // ADR-0039 item 7, movie direction: a movie's series key is its
            // item key, so this bind changes both and runs both migrators over
            // the same value in the same transaction. The tables the series
            // migrator rewrites do not exist yet, so this no-ops today.
            migrator::migrate_series_keys(&tx, &old_effective, &key)?;
            tx.commit().map_err(|e| format!("commit assign: {e}"))?;
            (key, meta)
        }
        "tv" => {
            let (meta, raw) = client.tv_detail(req.tmdb_id).map_err(|e| e.to_string())?;
            canonical::persist_mapped_hit(conn, PROVIDER_TMDB, &raw, &meta)
                .map_err(|e| e.to_string())?;
            // Bind season→episode for this file (and multi-ep ranges).
            // Counters are carried out on AssignResult rather than dropped: a
            // manual assign fetches seasons and persists projections exactly as
            // the drain does, and discarding them here left that work in no
            // counter anywhere.
            bind = queue::bind_resolved_items(conn, resolver, &[item.id], &meta)?;
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
        bind,
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
    // ADR-0033 Q4: an identified folder caches misses under `series:{id}`, not
    // the title+year key. Clear that row too, or the next drain re-suppresses
    // the fall-through search and the retry silently no-ops (ADR-0026 §3).
    if kind == CacheKind::Tv
        && let Some(show_id) = series_show_id_for_item(conn, &item)?
    {
        negative_cache::clear(
            conn,
            PROVIDER_TMDB,
            kind,
            &negative_cache::series_cache_key(show_id),
        )?;
    }
    queue::set_metadata_status(conn, &[item.id], MetadataStatus::Pending)?;
    Ok(())
}

/// ADR-0033 Q4: the stored series id for the item's show folder, when one
/// exists. Same folder-key derivation (`show_folder_relpath`) and row lookup
/// (`queue::series_show_id_for_folder`) the drain uses, so the manual-retry
/// delete hits the exact row the next resolve consults.
fn series_show_id_for_item(conn: &Connection, item: &FixItemView) -> Result<Option<i64>, String> {
    let library_path: String = conn
        .query_row(
            "SELECT path FROM libraries WHERE id = ?1",
            params![item.library_id],
            |r| r.get(0),
        )
        .optional()
        .map_err(|e| format!("load library {}: {e}", item.library_id))?
        .ok_or_else(|| format!("library {} not found", item.library_id))?;
    let folder = show_folder_relpath(&item.path, &library_path);
    queue::series_show_id_for_folder(conn, item.library_id, &folder)
}

/// Load item fields for API handlers.
pub fn get_fix_item(conn: &Connection, id: i64) -> Result<FixItemView, String> {
    load_item(conn, id)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::queue::{DrainOptions, drain_pending};
    use crate::resolve::{ResolveError, ResolveInput, Resolver};
    use nightjar_db::migrate;
    use rusqlite::Connection;
    use std::cell::Cell;
    use std::sync::atomic::AtomicU64;

    struct CountingMiss {
        calls: Cell<usize>,
    }

    fn movie_hit(id: i64, title: &str, year: i32) -> SearchHit {
        SearchHit {
            id,
            title: Some(title.into()),
            name: None,
            original_title: None,
            original_name: None,
            release_date: Some(format!("{year}-01-01")),
            first_air_date: None,
            poster_path: None,
            backdrop_path: None,
            overview: None,
            vote_average: None,
            vote_count: None,
        }
    }

    /// **The suggestion is the scorer's pick, and it is below the floor.**
    ///
    /// `CODA` is the shape ADR-0048 measured: several real distinct films
    /// sharing a title, no year anywhere in the path to separate them. The
    /// matcher scores this `exact_title_collision_unpinned` at 0.72 and the
    /// 0.90 floor declines it, which is why the file is in the fix flow at all.
    /// What is asserted here is only that the id offered is the same id that
    /// scored — not that it is the right film, which is what the user confirms.
    #[test]
    fn a_yearless_movie_collision_suggests_the_candidate_the_scorer_chose() {
        let hits = vec![
            movie_hit(776503, "CODA", 2021),
            movie_hit(431478, "CODA", 2019),
            movie_hit(1129593, "CODA", 2015),
        ];
        let scored = score_search(&hits, "CODA", None, SearchKind::Movie).unwrap();
        assert!(
            !crate::match_score::meets_auto_match_floor(scored.confidence),
            "the population this serves is the one the floor declined; if this              clears the floor the fixture is no longer that population"
        );
        assert_eq!(
            suggested_candidate(&hits, "CODA", None, SearchKind::Movie),
            Some(scored.tmdb_id),
            "the suggestion must be the shipped scorer's own pick, not a second rule"
        );

        // Through the list the route actually returns, so the flag is checked
        // where a caller reads it and not only where it is decided.
        let out = candidates_from_hits(hits, "CODA", None, SearchKind::Movie);
        assert_eq!(out.len(), 3, "every hit is still offered");
        let flagged: Vec<i64> = out.iter().filter(|c| c.suggested).map(|c| c.id).collect();
        assert_eq!(
            flagged,
            vec![scored.tmdb_id],
            "exactly one candidate carries the suggestion, and it is the scored one"
        );
    }

    /// A TV search gets no suggestion, and the reason is evidence rather than
    /// taste: this route has no `folder_seasons`, no candidate detail counts and
    /// no reference episode title, so scoring here would answer a different
    /// question from the one the drain answers. ADR-0048's 88.9% is a movie
    /// number and does not carry.
    #[test]
    fn a_tv_search_carries_no_suggestion() {
        let hits = vec![SearchHit {
            id: 1,
            title: None,
            name: Some("Ghosts".into()),
            original_title: None,
            original_name: None,
            release_date: None,
            first_air_date: Some("2021-10-07".into()),
            poster_path: None,
            backdrop_path: None,
            overview: None,
            vote_average: None,
            vote_count: None,
        }];
        assert!(
            score_search(&hits, "Ghosts", None, SearchKind::Tv).is_some(),
            "the scorer does answer for TV — the abstention below is this              route's decision, not an empty result"
        );
        assert_eq!(
            suggested_candidate(&hits, "Ghosts", None, SearchKind::Tv),
            None
        );
        let out = candidates_from_hits(hits, "Ghosts", None, SearchKind::Tv);
        assert_eq!(out.len(), 1, "the candidate is still offered");
        assert!(
            !out.iter().any(|c| c.suggested),
            "no TV candidate is marked, so a client cannot lead with one"
        );
    }

    impl MetadataSource for CountingMiss {
        fn resolve(
            &self,
            _input: &ResolveInput,
        ) -> Result<crate::resolve::ProviderResult, ResolveError> {
            self.calls.set(self.calls.get() + 1);
            Ok(crate::resolve::ProviderResult::Miss)
        }
    }

    /// **An absent title is not a query.** The drain's own path filters an
    /// empty title to a miss before any request; this route did not, because
    /// it substitutes the *stored* title when the caller supplies no query.
    ///
    /// A file directly in the library root is where that bites: the parser
    /// finds no title in `S01E04.mkv` and the scanner has no folder to borrow
    /// one from, so the stored title is empty.
    ///
    /// The client here carries a bogus key and is never reached — a real
    /// search would error, so `Ok([])` is what proves the guard fired.
    #[test]
    fn a_fix_search_on_an_absent_title_asks_the_provider_nothing() {
        let client = TmdbClient::new(crate::tmdb::TmdbCredentials {
            api_key: "not-a-key".into(),
            source: crate::tmdb::TmdbKeySource::Env,
        });
        for kind in ["episode", "movie"] {
            let item = FixItemView {
                id: 1,
                library_id: 1,
                path: "S01E04.mkv".into(),
                title: String::new(),
                kind: kind.into(),
                year: None,
                season: Some(1),
                episode: Some(4),
            };
            let out = search_candidates(&client, &item, None, None).expect("no request, no error");
            assert!(out.is_empty(), "{kind} searched on an empty title");
            // An empty query string from the caller is the same absence.
            let out = search_candidates(&client, &item, Some("   "), None).expect("no request");
            assert!(out.is_empty(), "{kind} searched on a blank query");
        }
    }

    /// RC8 (verify round 1 issue): a manual retry on an identified folder must
    /// clear the folder's series-keyed negative-cache row as well as the
    /// title+year row. Without it, the next drain re-suppresses the
    /// fall-through search and the retry silently no-ops (ADR-0026 §3,
    /// ADR-0033 Q4).
    #[test]
    fn retry_clears_series_keyed_miss_and_next_drain_searches() {
        let c = Connection::open_in_memory().unwrap();
        migrate(&c).unwrap();
        // Identified folder: the series row exists, but its stored detail
        // (year 2011) disagrees with the folder year (2002), so the next drain
        // discards the id and falls through to search. Both a live series-keyed
        // miss and a live title+year miss are seeded; the retry must clear both.
        c.execute_batch(
            "INSERT INTO libraries (name, path, kind) VALUES ('S', '/tmp/S', 'shows');
             INSERT INTO media_items (library_id, path, mtime_ms, size_bytes, title, kind, season, episode)
             VALUES (1, 'Alpha (2002)/Season 1/Alpha.S01E01.mkv', 1, 1, 'Alpha', 'episode', 1, 1);
             INSERT INTO series (library_id, relpath, tmdb_show_id)
             VALUES (1, 'Alpha (2002)', 55);
             INSERT INTO metadata_canonical (
               provider, entity_kind, provider_id, title, year, ids_json, tmdb_show, projected_at
             ) VALUES
               ('tmdb', 'tv', '55', 'Alpha', 2011, '{\"tmdb\":55,\"tmdb_show\":55}', 55,
                '2026-01-01T00:00:00Z');
             INSERT INTO metadata_negative_cache
               (provider, kind, query_key, reason, confidence, attempt_count,
                attempted_at, next_retry_at, cleaner_version)
             VALUES
               ('tmdb', 'tv', 'series:55', 'no_results', NULL, 3,
                '2026-01-01T00:00:00Z', '2999-01-01T00:00:00Z', 1),
               ('tmdb', 'tv', 'alpha|-', 'no_results', NULL, 3,
                '2026-01-01T00:00:00Z', '2999-01-01T00:00:00Z', 1);",
        )
        .unwrap();

        retry_unmatched(&c, 1).unwrap();

        let n: i64 = c
            .query_row("SELECT COUNT(*) FROM metadata_negative_cache", [], |r| {
                r.get(0)
            })
            .unwrap();
        assert_eq!(
            n, 0,
            "the manual retry must clear both the title+year row and the series-keyed row"
        );

        let src = CountingMiss {
            calls: Cell::new(0),
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
        assert_eq!(s.items_unmatched, 1);
        assert_eq!(
            resolver.tmdb.calls.get(),
            1,
            "after a retry, the identified folder's fall-through search must reach the provider"
        );
    }
}
