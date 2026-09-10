//! Repository-native NFO identity regression tests (`cfg(test)` only).
//!
//! Registered from `lib.rs` as
//! `#[cfg(test)] #[path = "nfo_identity_harness.rs"] mod nfo_identity;`. It is
//! unavailable to the non-test build path.
//!
//! These are ordinary, non-ignored Rust tests. They exercise the real resolver
//! (`Resolver<&TmdbClient>` through `queue::drain_pending`,
//! `fix::retry_unmatched`, `fix::search_candidates` and `fix::assign`) against
//! the canonical `nfo_identity` fixture corpus. Provider responses come from
//! the deterministic in-process transport in `tmdb::test_fixture`; a request
//! with no registered route fails closed and can never reach the live network.
//!
//! Canonical XML and provider bytes are read-only. Every test owns a private
//! temporary SQLite database and library tree, so the tests run in parallel and
//! in any order.

use std::path::{Path, PathBuf};
use std::sync::atomic::AtomicU64;

use rusqlite::Connection;
use serde_json::Value;

use crate::fix::{AssignRequest, NoopArtwork, assign, retry_unmatched, search_candidates};
use crate::item_links::{link_keys_for_item, path_item_key};
use crate::model::MetadataKind;
use crate::nfo::parse_nfo;
use crate::queue::{DrainOptions, drain_pending};
use crate::resolve::{MetadataOrigin, ResolveInput, ResolveOutcome, Resolver};
use crate::tmdb::test_fixture::{self, LogEntry, RouteOutcome};
use crate::tmdb::{TmdbClient, TmdbCredentials, TmdbKeySource};

/// The five canonical columns read back for the NFO-03 merge assertion.
type CanonicalMergeFields = (
    Option<String>,
    Option<String>,
    Option<String>,
    Option<String>,
    Option<i64>,
);

// --- canonical fixture access ----------------------------------------------

fn fixture_root() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR"))
        .join("tests")
        .join("fixtures")
        .join("nfo_identity")
}

fn fixture_bytes(rel: &str) -> Vec<u8> {
    std::fs::read(fixture_root().join(rel)).unwrap_or_else(|e| panic!("read fixture {rel}: {e}"))
}

fn fixture_str(rel: &str) -> String {
    String::from_utf8_lossy(&fixture_bytes(rel)).into_owned()
}

fn load_manifest() -> Value {
    serde_json::from_slice(&fixture_bytes("manifest.json")).expect("manifest.json is valid JSON")
}

/// Register every manifest route for `case_id` under the exact request the
/// real `TmdbClient` builds. Search routes declare `query` plus `language` in
/// the manifest while the client sends only `query`; strip `language` for
/// those. The show path also folds the group title with `clean_show_title`
/// before searching. Both translations keep the route table exact: an
/// unregistered request shape still fails closed.
fn register_case_routes(case_id: &str) {
    let manifest = load_manifest();
    let routes = manifest
        .get("routes")
        .and_then(|r| r.as_array())
        .expect("manifest.routes array");
    for route in routes {
        if route.get("case_id").and_then(|c| c.as_str()) != Some(case_id) {
            continue;
        }
        let method = route
            .get("method")
            .and_then(|m| m.as_str())
            .unwrap_or("GET");
        let path = route
            .get("path")
            .and_then(|p| p.as_str())
            .expect("route path");
        let mut query: Vec<(String, String)> = route
            .get("query")
            .and_then(|q| q.as_object())
            .map(|obj| {
                obj.iter()
                    .filter_map(|(k, v)| v.as_str().map(|s| (k.clone(), s.to_string())))
                    .collect()
            })
            .unwrap_or_default();
        if path.starts_with("/search/") {
            query.retain(|(k, _)| k == "query");
            // The show path folds the group title with the product's own
            // `clean_show_title` before it searches, so the manifest's
            // author-declared title (e.g. "Gravelmere") is not the bytes on
            // the wire. Fold it the same way so the registered route matches
            // the exact request the client builds. Matching stays exact: an
            // unregistered request still fails closed.
            if path == "/search/tv" {
                for (_, v) in query.iter_mut() {
                    *v = crate::clean_show_title(v).0;
                }
            }
        }
        let body = fixture_bytes(
            route
                .get("response")
                .and_then(|r| r.as_str())
                .expect("route response"),
        );
        test_fixture::register_route(method, path, &query, RouteOutcome::Hit(body));
    }
}

// --- test-owned state -------------------------------------------------------

/// A private temporary root for one test. The name is unique per test, so
/// parallel runs never share a database or a library tree.
fn test_root(name: &str) -> PathBuf {
    let dir = std::env::temp_dir().join(format!("nightjar-nfo-{}-{name}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).expect("create test root");
    dir
}

fn open_db(root: &Path) -> Connection {
    let conn = Connection::open(root.join("state.db")).expect("open db");
    nightjar_db::migrate(&conn).expect("migrate");
    conn
}

fn write_sidecar(lib_dir: &Path, folder_rel: &str, media_rel: &str, bytes: &[u8]) {
    let folder = lib_dir.join(folder_rel);
    std::fs::create_dir_all(&folder).unwrap_or_else(|e| panic!("create media folder: {e}"));
    let stem = Path::new(media_rel)
        .file_stem()
        .and_then(|s| s.to_str())
        .unwrap_or("movie");
    std::fs::write(folder.join(format!("{stem}.nfo")), bytes)
        .unwrap_or_else(|e| panic!("write sidecar: {e}"));
}

fn seed_movie(
    conn: &Connection,
    lib_dir: &Path,
    title: &str,
    year: Option<i32>,
    folder_rel: &str,
    media_rel: &str,
    nfo_rel: Option<&str>,
) -> i64 {
    if let Some(rel) = nfo_rel {
        write_sidecar(lib_dir, folder_rel, media_rel, &fixture_bytes(rel));
    }
    let lib_path = lib_dir.to_string_lossy().replace('\\', "/");
    conn.execute(
        "INSERT INTO libraries (name, path, kind) VALUES ('L', ?1, 'movies')",
        rusqlite::params![lib_path],
    )
    .unwrap();
    conn.execute(
        "INSERT INTO media_items (library_id, path, mtime_ms, size_bytes, title, kind, year, season, episode)
         VALUES (1, ?1, 1, 1, ?2, 'movie', ?3, NULL, NULL)",
        rusqlite::params![media_rel, title, year],
    )
    .unwrap();
    conn.query_row(
        "SELECT id FROM media_items WHERE path = ?1",
        rusqlite::params![media_rel],
        |r| r.get(0),
    )
    .unwrap()
}

fn seed_show(
    conn: &Connection,
    lib_dir: &Path,
    folder_rel: &str,
    show_title: &str,
    tvshow_rel: &str,
) -> i64 {
    let show_dir = lib_dir.join(folder_rel);
    std::fs::create_dir_all(&show_dir).unwrap_or_else(|e| panic!("create show dir: {e}"));
    std::fs::write(show_dir.join("tvshow.nfo"), fixture_bytes(tvshow_rel))
        .unwrap_or_else(|e| panic!("write tvshow.nfo: {e}"));
    let episode_rel = format!("{folder_rel}/{show_title}.S01E01.mkv");
    let lib_path = lib_dir.to_string_lossy().replace('\\', "/");
    conn.execute(
        "INSERT INTO libraries (name, path, kind) VALUES ('L', ?1, 'shows')",
        rusqlite::params![lib_path],
    )
    .unwrap();
    conn.execute(
        "INSERT INTO media_items (library_id, path, mtime_ms, size_bytes, title, kind, year, season, episode)
         VALUES (1, ?1, 1, 1, ?2, 'episode', NULL, NULL, NULL)",
        rusqlite::params![episode_rel, show_title],
    )
    .unwrap();
    conn.query_row(
        "SELECT id FROM media_items WHERE path = ?1",
        rusqlite::params![episode_rel],
        |r| r.get(0),
    )
    .unwrap()
}

fn fixture_client() -> TmdbClient {
    TmdbClient::new(TmdbCredentials {
        api_key: "nfo-fixture-key".into(),
        source: TmdbKeySource::Env,
    })
}

fn run_drain(conn: &Connection, client: &TmdbClient) -> crate::queue::DrainStats {
    let resolver = Resolver { tmdb: client };
    drain_pending(
        conn,
        &resolver,
        &AtomicU64::new(0),
        &AtomicU64::new(0),
        DrainOptions::default(),
    )
    .expect("drain must not error")
}

fn status(conn: &Connection, item_id: i64) -> Option<String> {
    conn.query_row(
        "SELECT metadata_status FROM media_items WHERE id = ?1",
        rusqlite::params![item_id],
        |r| r.get(0),
    )
    .ok()
}

fn match_method(conn: &Connection, item_id: i64) -> Option<String> {
    conn.query_row(
        "SELECT metadata_match_method FROM media_items WHERE id = ?1",
        rusqlite::params![item_id],
        |r| r.get(0),
    )
    .ok()
}

fn unmatched_reason(conn: &Connection, item_id: i64) -> Option<String> {
    conn.query_row(
        "SELECT metadata_unmatched_reason FROM media_items WHERE id = ?1",
        rusqlite::params![item_id],
        |r| r.get(0),
    )
    .ok()
}

fn links(conn: &Connection, item_id: i64) -> Vec<String> {
    link_keys_for_item(conn, item_id).unwrap_or_default()
}

fn log_paths(log: &[LogEntry]) -> Vec<String> {
    log.iter().map(|e| e.path.clone()).collect()
}

fn query_pairs(entry: &LogEntry) -> Vec<(String, String)> {
    entry.query.clone()
}

// --- product scenarios ------------------------------------------------------

#[test]
fn nfo_01_complete_movie_binds_without_provider() {
    test_fixture::reset();
    register_case_routes("NFO-01");
    let root = test_root("nfo_01");
    let conn = open_db(&root);
    let lib = root.join("L");
    std::fs::create_dir_all(&lib).unwrap();
    let item = seed_movie(
        &conn,
        &lib,
        "The Meridian Job",
        Some(2017),
        "The Meridian Job (2017)",
        "The Meridian Job (2017)/The Meridian Job.mkv",
        Some("xml/NFO-01/complete.nfo"),
    );

    let stats = run_drain(&conn, &fixture_client());

    assert_eq!(stats.provider_resolves, 1);
    assert_eq!(
        test_fixture::request_log().len(),
        0,
        "a complete movie NFO makes zero provider requests"
    );
    assert_eq!(status(&conn, item).as_deref(), Some("ready"));
    assert_eq!(match_method(&conn, item).as_deref(), Some("nfo_complete"));
    assert_eq!(links(&conn, item), vec!["tmdb:movie:9200001".to_string()]);

    let xml = fixture_str("xml/NFO-01/complete.nfo");
    let meta = parse_nfo(xml.trim()).expect("complete.nfo parses");
    assert_eq!(meta.ids.tmdb, Some(9200001));
    assert_eq!(meta.ids.imdb.as_deref(), Some("tt9200001"));
    assert_eq!(meta.ids.tvdb, Some(8200001));
    assert!(meta.is_nfo_complete());

    let _ = std::fs::remove_dir_all(&root);
}

#[test]
fn nfo_02_partial_movie_enriches_content_without_search() {
    test_fixture::reset();
    register_case_routes("NFO-02");
    let root = test_root("nfo_02");
    let conn = open_db(&root);
    let lib = root.join("L");
    std::fs::create_dir_all(&lib).unwrap();
    let item = seed_movie(
        &conn,
        &lib,
        "Salt Harbor",
        Some(2015),
        "Salt Harbor (2015)",
        "Salt Harbor (2015)/Salt Harbor.mkv",
        Some("xml/NFO-02/partial.nfo"),
    );

    let _ = run_drain(&conn, &fixture_client());
    let log = test_fixture::request_log();

    assert_eq!(
        log_paths(&log),
        vec!["/movie/9200002".to_string()],
        "the NFO id short-circuits search; only content detail is fetched"
    );
    assert_eq!(
        query_pairs(&log[0]),
        vec![
            (
                "append_to_response".to_string(),
                "images,credits,videos,release_dates,external_ids".to_string()
            ),
            ("language".to_string(), "en-US".to_string()),
        ]
    );
    assert_eq!(status(&conn, item).as_deref(), Some("ready"));
    assert_eq!(links(&conn, item), vec!["tmdb:movie:9200002".to_string()]);

    let _ = std::fs::remove_dir_all(&root);
}

#[test]
fn nfo_03_content_only_searches_then_preserves_nfo_fields() {
    test_fixture::reset();
    register_case_routes("NFO-03");
    let root = test_root("nfo_03");
    let conn = open_db(&root);
    let lib = root.join("L");
    std::fs::create_dir_all(&lib).unwrap();
    let item = seed_movie(
        &conn,
        &lib,
        "Keeper of the Lantern",
        Some(2011),
        "Keeper of the Lantern (2011)",
        "Keeper of the Lantern (2011)/Keeper of the Lantern.mkv",
        Some("xml/NFO-03/content-only.nfo"),
    );

    let _ = run_drain(&conn, &fixture_client());
    let log = test_fixture::request_log();

    assert_eq!(
        log_paths(&log),
        vec![
            "/search/movie".to_string(),
            "/movie/9200003".to_string(),
            "/movie/9200003".to_string(),
        ],
        "no usable id forces a real search; search-tier and enrich each fetch detail"
    );
    assert_eq!(
        query_pairs(&log[0]),
        vec![("query".to_string(), "Keeper of the Lantern".to_string())],
        "the search query is built from the NFO title"
    );
    assert_eq!(status(&conn, item).as_deref(), Some("ready"));
    assert_eq!(links(&conn, item), vec!["tmdb:movie:9200003".to_string()]);

    let xml = fixture_str("xml/NFO-03/content-only.nfo");
    let nfo = parse_nfo(xml.trim()).expect("content-only.nfo parses");
    let (canon_plot, canon_genres_json, canon_cast_json, canon_title, canon_year):
        CanonicalMergeFields = conn
        .query_row(
            "SELECT plot, genres_json, cast_json, title, year
             FROM metadata_canonical
             WHERE provider = 'tmdb' AND entity_kind = 'movie' AND provider_id = '9200003'",
            [],
            |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?, r.get(3)?, r.get(4)?)),
        )
        .expect("stored canonical row for 9200003");

    assert_eq!(
        canon_plot, nfo.plot,
        "the NFO plot survives the search/detail merge"
    );
    assert_eq!(canon_title.as_deref(), Some("Keeper of the Lantern"));
    assert_eq!(canon_year, Some(2011));
    let canon_genres: Vec<String> =
        serde_json::from_str(&canon_genres_json.unwrap()).expect("genres_json is an array");
    for genre in &nfo.genres {
        assert!(
            canon_genres.contains(genre),
            "NFO genre {genre:?} survives the merge; stored {canon_genres:?}"
        );
    }
    let canon_cast: Vec<Value> =
        serde_json::from_str(&canon_cast_json.unwrap()).expect("cast_json is an array");
    for member in &nfo.cast {
        assert!(
            canon_cast
                .iter()
                .any(|c| c.get("name").and_then(|n| n.as_str()) == Some(member.name.as_str())),
            "NFO cast member {:?} survives the merge",
            member.name
        );
    }

    let _ = std::fs::remove_dir_all(&root);
}

#[test]
fn nfo_04_conflicting_claims_short_circuit_tmdb() {
    test_fixture::reset();
    register_case_routes("NFO-04");
    let xml = fixture_str("xml/NFO-04/conflict.nfo");
    let parsed = parse_nfo(xml.trim()).expect("conflict.nfo parses");
    // The fixture names three distinct works; every claim must be retained.
    assert_eq!(parsed.ids.tmdb, Some(9200041));
    assert_eq!(parsed.ids.imdb.as_deref(), Some("tt9200042"));
    assert_eq!(parsed.ids.tvdb, Some(8200043));

    let root = test_root("nfo_04");
    let conn = open_db(&root);
    let lib = root.join("L");
    std::fs::create_dir_all(&lib).unwrap();
    let item = seed_movie(
        &conn,
        &lib,
        "The Cobalt Divide",
        Some(2018),
        "The Cobalt Divide (2018)",
        "The Cobalt Divide (2018)/The Cobalt Divide.mkv",
        Some("xml/NFO-04/conflict.nfo"),
    );

    let client = fixture_client();
    // The resolver reports the route that chose the entity, before the queue
    // rewrites it to the persisted `nfo_complete` token.
    let resolver = Resolver { tmdb: &client };
    let outcome = resolver
        .resolve(&ResolveInput {
            nfo_xml: Some(xml.clone()),
            title: Some("The Cobalt Divide".into()),
            year: Some(2018),
            kind: Some(MetadataKind::Movie),
            ..Default::default()
        })
        .expect("resolve must not error");
    match outcome {
        ResolveOutcome::Resolved {
            source,
            match_method,
            metadata,
            ..
        } => {
            assert_eq!(source, MetadataOrigin::Nfo);
            assert_eq!(match_method.as_deref(), Some("nfo_item_tmdb_id"));
            assert_eq!(metadata.ids.tmdb, Some(9200041));
        }
        other => panic!("expected NFO resolve, got {other:?}"),
    }

    let _ = run_drain(&conn, &fixture_client());

    assert_eq!(
        test_fixture::request_log().len(),
        0,
        "a usable TMDB claim short-circuits every provider request"
    );
    assert_eq!(status(&conn, item).as_deref(), Some("ready"));
    assert_eq!(match_method(&conn, item).as_deref(), Some("nfo_complete"));
    assert_eq!(
        links(&conn, item),
        vec!["tmdb:movie:9200041".to_string()],
        "the movie binds on its usable TMDB claim"
    );

    let _ = std::fs::remove_dir_all(&root);
}

#[test]
fn nfo_05_show_find_cross_check_binds() {
    test_fixture::reset();
    register_case_routes("NFO-05");
    let root = test_root("nfo_05");
    let conn = open_db(&root);
    let lib = root.join("S");
    std::fs::create_dir_all(&lib).unwrap();
    let item = seed_show(
        &conn,
        &lib,
        "Cinder Street (2019)",
        "Cinder Street",
        "xml/NFO-05/tvshow.nfo",
    );

    let _ = run_drain(&conn, &fixture_client());
    let log = test_fixture::request_log();

    assert_eq!(
        log_paths(&log),
        vec![
            "/find/tt9300005".to_string(),
            "/tv/9300005".to_string(),
            "/tv/9300005".to_string(),
        ],
        "find, cross-check detail, then enrich detail by the stored id"
    );
    assert_eq!(
        query_pairs(&log[0]),
        vec![("external_source".to_string(), "imdb_id".to_string())]
    );
    assert!(
        log.iter().all(|e| !e.path.starts_with("/search/")),
        "a consistent find/detail cross-check never falls through to search"
    );
    assert_eq!(links(&conn, item), vec!["tmdb:show:9300005".to_string()]);
    assert_ne!(links(&conn, item), vec!["tmdb:show:9300006".to_string()]);

    let _ = std::fs::remove_dir_all(&root);
}

#[test]
fn nfo_06_show_find_mismatch_falls_through_to_search() {
    test_fixture::reset();
    register_case_routes("NFO-06");
    let root = test_root("nfo_06");
    let conn = open_db(&root);
    let lib = root.join("S");
    std::fs::create_dir_all(&lib).unwrap();
    let item = seed_show(
        &conn,
        &lib,
        "Gravelmere (2021)",
        "Gravelmere",
        "xml/NFO-06/tvshow.nfo",
    );

    let _ = run_drain(&conn, &fixture_client());
    let log = test_fixture::request_log();

    assert_eq!(
        log_paths(&log),
        vec![
            "/find/tt9300006".to_string(),
            "/tv/9300006".to_string(),
            "/search/tv".to_string(),
        ],
        "a mismatched find falls through to the title search"
    );
    assert_eq!(
        query_pairs(&log[2]),
        vec![("query".to_string(), "gravelmere".to_string())],
        "the show group title is folded to lowercase before the search"
    );
    assert_eq!(status(&conn, item).as_deref(), Some("unmatched"));
    assert!(
        links(&conn, item).is_empty(),
        "no link may bind the mismatched entity 9300006; got {:?}",
        links(&conn, item)
    );

    let _ = std::fs::remove_dir_all(&root);
}

#[test]
fn nfo_07_absent_body_is_not_identity_evidence() {
    test_fixture::reset();
    register_case_routes("NFO-07");
    let empty = fixture_str("xml/NFO-07/absent.nfo");
    assert!(
        parse_nfo(empty.trim()).is_err(),
        "an absent body carries no identity evidence"
    );

    let client = fixture_client();
    let resolver = Resolver { tmdb: &client };
    let outcome = resolver
        .resolve(&ResolveInput {
            nfo_xml: Some(empty),
            title: Some("Kestrel Road".into()),
            year: Some(2009),
            kind: Some(MetadataKind::Movie),
            ..Default::default()
        })
        .expect("resolve must not error");
    match outcome {
        ResolveOutcome::Resolved {
            source, metadata, ..
        } => {
            assert_eq!(
                source,
                MetadataOrigin::Tmdb,
                "the folder title, not the absent NFO, drives identity"
            );
            assert_eq!(metadata.ids.tmdb, Some(9200007));
        }
        other => panic!("expected a title-search resolve, got {other:?}"),
    }
    assert_eq!(
        log_paths(&test_fixture::request_log()),
        vec!["/search/movie".to_string(), "/movie/9200007".to_string()]
    );
}

// --- NFO-08: malformed, corrected, manual-assign ----------------------------

const NFO08_MEDIA: &str = "Secondhand Orbit (2013)/Secondhand Orbit.mkv";
const NFO08_FOLDER: &str = "Secondhand Orbit (2013)";

fn create_watch_tables(conn: &Connection) {
    conn.execute_batch(
        "CREATE TABLE IF NOT EXISTS watch_state (
            profile_id INTEGER NOT NULL,
            item_key TEXT NOT NULL,
            position_ms INTEGER NOT NULL,
            duration_ms INTEGER,
            played INTEGER NOT NULL DEFAULT 0,
            last_played_at TEXT NOT NULL,
            PRIMARY KEY (profile_id, item_key)
         );
         CREATE TABLE IF NOT EXISTS playback_events (
            profile_id INTEGER NOT NULL,
            item_key TEXT NOT NULL,
            played_at TEXT NOT NULL,
            event TEXT NOT NULL,
            position_ms INTEGER NOT NULL,
            PRIMARY KEY (profile_id, item_key, played_at)
         );",
    )
    .expect("create watch tables");
}

fn seed_watch_rows(conn: &Connection, old_key: &str) {
    conn.execute(
        "INSERT INTO watch_state (profile_id, item_key, position_ms, duration_ms, played, last_played_at)
         VALUES (1, ?1, 3412000, 6100000, 1, '2026-01-05T21:14:00Z')",
        rusqlite::params![old_key],
    )
    .expect("seed watch_state");
    conn.execute(
        "INSERT INTO playback_events (profile_id, item_key, played_at, event, position_ms)
         VALUES (1, ?1, '2026-01-05T21:00:00Z', 'resume', 20000),
                (1, ?1, '2026-01-05T21:14:00Z', 'pause', 3412000)",
        rusqlite::params![old_key],
    )
    .expect("seed playback_events");
}

fn watch_row_count(conn: &Connection, key: &str) -> i64 {
    conn.query_row(
        "SELECT COUNT(*) FROM watch_state WHERE item_key = ?1",
        rusqlite::params![key],
        |r| r.get(0),
    )
    .unwrap_or(0)
}

fn event_row_count(conn: &Connection, key: &str) -> i64 {
    conn.query_row(
        "SELECT COUNT(*) FROM playback_events WHERE item_key = ?1",
        rusqlite::params![key],
        |r| r.get(0),
    )
    .unwrap_or(0)
}

/// Seed the NFO-08 movie with its malformed sidecar and a non-empty watch
/// history under the path key. Returns the item id and the old key.
fn seed_nfo08(conn: &Connection, lib: &Path) -> (i64, String) {
    let item = seed_movie(
        conn,
        lib,
        "Secondhand Orbit",
        Some(2013),
        NFO08_FOLDER,
        NFO08_MEDIA,
        Some("xml/NFO-08/initial-malformed.nfo"),
    );
    let old_key = path_item_key(1, NFO08_MEDIA);
    create_watch_tables(conn);
    seed_watch_rows(conn, &old_key);
    (item, old_key)
}

#[test]
fn nfo_08_initial_malformed_is_unmatched() {
    test_fixture::reset();
    register_case_routes("NFO-08");
    let root = test_root("nfo_08_initial");
    let conn = open_db(&root);
    let lib = root.join("L");
    std::fs::create_dir_all(&lib).unwrap();
    let (item, old_key) = seed_nfo08(&conn, &lib);

    let _ = run_drain(&conn, &fixture_client());

    assert_eq!(status(&conn, item).as_deref(), Some("unmatched"));
    assert_eq!(
        unmatched_reason(&conn, item).as_deref(),
        Some("nfo_invalid"),
        "a malformed body is carried as the reason when the fallback search also misses"
    );
    assert!(
        links(&conn, item).is_empty(),
        "the malformed body binds nothing"
    );
    // The watch history is untouched: no binding exists yet.
    assert_eq!(watch_row_count(&conn, &old_key), 1);
    assert_eq!(event_row_count(&conn, &old_key), 2);

    let _ = std::fs::remove_dir_all(&root);
}

#[test]
fn nfo_08_corrected_body_auto_retries_to_ready() {
    test_fixture::reset();
    register_case_routes("NFO-08");
    let root = test_root("nfo_08_corrected");
    let conn = open_db(&root);
    let lib = root.join("L");
    std::fs::create_dir_all(&lib).unwrap();
    let (item, old_key) = seed_nfo08(&conn, &lib);
    // Phase 1: the malformed body leaves the item unmatched.
    let _ = run_drain(&conn, &fixture_client());
    assert_eq!(status(&conn, item).as_deref(), Some("unmatched"));

    // Phase 2: the corrected body is retried, then the drain binds it.
    write_sidecar(
        &lib,
        NFO08_FOLDER,
        NFO08_MEDIA,
        &fixture_bytes("xml/NFO-08/corrected-body.nfo"),
    );
    retry_unmatched(&conn, item).expect("retry_unmatched must not error");
    let _ = run_drain(&conn, &fixture_client());

    assert_eq!(status(&conn, item).as_deref(), Some("ready"));
    assert_eq!(match_method(&conn, item).as_deref(), Some("nfo_complete"));
    assert_eq!(links(&conn, item), vec!["tmdb:movie:9200008".to_string()]);
    let manually: i64 = conn
        .query_row(
            "SELECT manually_matched FROM media_item_links WHERE media_item_id = ?1",
            rusqlite::params![item],
            |r| r.get(0),
        )
        .expect("link row");
    assert_eq!(manually, 0, "automatic retry is never manually_matched");
    let _ = old_key;

    let _ = std::fs::remove_dir_all(&root);
}

#[test]
fn nfo_08_manual_assign_records_manually_matched_and_migrates_watch() {
    test_fixture::reset();
    register_case_routes("NFO-08");
    let root = test_root("nfo_08_manual");
    let conn = open_db(&root);
    let lib = root.join("L");
    std::fs::create_dir_all(&lib).unwrap();
    let (item, old_key) = seed_nfo08(&conn, &lib);
    let _ = run_drain(&conn, &fixture_client());
    assert_eq!(status(&conn, item).as_deref(), Some("unmatched"));
    assert_eq!(watch_row_count(&conn, &old_key), 1);
    assert_eq!(event_row_count(&conn, &old_key), 2);

    let client = fixture_client();
    let fix_item = crate::fix::get_fix_item(&conn, item).expect("load fix item");
    let candidates = search_candidates(&client, &fix_item, None, None)
        .expect("search_candidates must not error");
    assert!(
        candidates.iter().all(|c| c.provider == "tmdb"),
        "fix candidates come from the tmdb provider seam"
    );
    let resolver = Resolver { tmdb: &client };
    assign(
        &conn,
        &resolver,
        &client,
        &NoopArtwork,
        &AssignRequest {
            media_item_id: item,
            kind: "movie".into(),
            tmdb_id: 9200008,
        },
    )
    .expect("assign must not error");

    assert_eq!(status(&conn, item).as_deref(), Some("ready"));
    assert_eq!(links(&conn, item), vec!["tmdb:movie:9200008".to_string()]);
    let manually: i64 = conn
        .query_row(
            "SELECT manually_matched FROM media_item_links WHERE media_item_id = ?1",
            rusqlite::params![item],
            |r| r.get(0),
        )
        .expect("link row");
    assert_eq!(manually, 1, "manual Assign records manually_matched");
    // Watch state and playback events migrate from the path key to the new
    // watch key, preserving every non-key value.
    assert_eq!(watch_row_count(&conn, &old_key), 0);
    assert_eq!(event_row_count(&conn, &old_key), 0);
    let migrated: (i64, Option<i64>, i64, String) = conn
        .query_row(
            "SELECT position_ms, duration_ms, played, last_played_at
             FROM watch_state WHERE item_key = 'tmdb:movie:9200008'",
            [],
            |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?, r.get(3)?)),
        )
        .expect("migrated watch row");
    assert_eq!(
        migrated,
        (3412000, Some(6100000), 1, "2026-01-05T21:14:00Z".into())
    );
    assert_eq!(event_row_count(&conn, "tmdb:movie:9200008"), 2);

    let _ = std::fs::remove_dir_all(&root);
}

// --- negative provider control ----------------------------------------------

#[test]
fn unregistered_fixture_route_fails_closed() {
    test_fixture::reset();
    let client = fixture_client();
    // No route is registered for this title. The transport must refuse
    // deterministically and never fall through to the live `ureq` route.
    let err = client
        .search(crate::match_score::SearchKind::Movie, "Absent Route Probe")
        .expect_err("an unregistered fixture route is a provider error");
    assert!(
        err.to_string().contains("fixture route missing"),
        "deterministic fixture refusal expected: {err}"
    );
    let log = test_fixture::request_log();
    assert_eq!(log.len(), 1, "the refused call is logged exactly once");
    assert_eq!(log[0].method, "GET");
    assert_eq!(log[0].path, "/search/movie");
    assert!(matches!(
        log[0].outcome,
        crate::tmdb::test_fixture::LogOutcome::Error(_)
    ));
}
