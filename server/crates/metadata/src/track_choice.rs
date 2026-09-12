//! Track-choice writes (ADR-0038 item 4 and its 2026-09-12 amendment).
//!
//! The one writer. Series-key resolution, the server clock and the upsert
//! happen in one transaction, so a refused key leaves no row and a failed
//! upsert leaves no partial state — the same shape as `watch_state`.

use crate::item_links::series_key_resolves;
use nightjar_db::{
    SubtitleChoiceRow, TrackChoiceRow, TrackDescription, load_track_choice, now_iso,
    upsert_track_choice, write_tx,
};
use rusqlite::Connection;

#[derive(Debug, PartialEq, Eq)]
pub enum TrackChoiceError {
    /// The key names no series through the effective identity layer.
    UnresolvedSeriesKey,
    Db(String),
}

/// Replace a profile's choice for one series. The body is a full replacement,
/// so a cleared field is written as null rather than left behind.
pub fn write_track_choice(
    conn: &Connection,
    profile_id: i64,
    series_key: &str,
    audio: Option<&TrackDescription>,
    subtitle: &SubtitleChoiceRow,
) -> Result<TrackChoiceRow, TrackChoiceError> {
    // `write_tx` takes the write lock before the first read, so the key check
    // cannot race the upsert against a concurrent bind (see `nightjar_db`).
    let tx = write_tx(conn).map_err(TrackChoiceError::Db)?;
    if !series_key_resolves(&tx, series_key).map_err(TrackChoiceError::Db)? {
        return Err(TrackChoiceError::UnresolvedSeriesKey);
    }
    let now = now_iso(&tx).map_err(TrackChoiceError::Db)?;
    upsert_track_choice(&tx, profile_id, series_key, audio, subtitle, &now)
        .map_err(TrackChoiceError::Db)?;
    let row = load_track_choice(&tx, profile_id, series_key)
        .map_err(TrackChoiceError::Db)?
        .ok_or_else(|| TrackChoiceError::Db("track choice vanished after upsert".to_string()))?;
    tx.commit()
        .map_err(|e| TrackChoiceError::Db(e.to_string()))?;
    Ok(row)
}

/// Read one profile's choice for a resolved series key.
///
/// `Ok(None)` is a real series with no stored choice, which the API reports
/// distinctly from a key that names nothing.
pub fn read_track_choice(
    conn: &Connection,
    profile_id: i64,
    series_key: &str,
) -> Result<Option<TrackChoiceRow>, TrackChoiceError> {
    if !series_key_resolves(conn, series_key).map_err(TrackChoiceError::Db)? {
        return Err(TrackChoiceError::UnresolvedSeriesKey);
    }
    load_track_choice(conn, profile_id, series_key).map_err(TrackChoiceError::Db)
}

#[cfg(test)]
mod tests {
    use super::*;
    use nightjar_db::{NewLibrary, UpsertItem};

    fn fixture() -> nightjar_db::Db {
        let db = nightjar_db::Db::open(std::path::Path::new(":memory:")).unwrap();
        db.with_conn(|conn| {
            conn.execute_batch(
                "INSERT INTO accounts (id, username, password_hash, role)
                     VALUES (1, 'a', 'x', 'owner');
                 INSERT INTO profiles (id, account_id, profile_ref, name)
                     VALUES (1, 1, 'aa', 'P');",
            )
            .map_err(|e| e.to_string())?;
            Ok(())
        })
        .unwrap();
        let library = db
            .create_library(&NewLibrary {
                name: "shows".to_string(),
                path: "/media/shows".to_string(),
                kind: "shows".to_string(),
            })
            .unwrap();
        db.upsert_items_indexed(
            library.id,
            &[UpsertItem {
                path: "Alpha/Season 1/Alpha.S01E01.mkv".to_string(),
                mtime_ms: 0,
                size_bytes: 1,
                title: "Alpha".to_string(),
                kind: "episode".to_string(),
                year: None,
                season: Some(1),
                episode: Some(1),
                content_id: None,
            }],
        )
        .unwrap();
        // ADR-0039 item 3: every show folder gets a series row, here unmatched.
        db.with_conn(|conn| {
            conn.execute(
                "INSERT INTO series (library_id, relpath, tmdb_show_id) VALUES (?1, 'Alpha', NULL)",
                [library.id],
            )
            .map_err(|e| e.to_string())?;
            Ok(())
        })
        .unwrap();
        db
    }

    fn desc(kind: &str) -> TrackDescription {
        TrackDescription {
            language: Some("ja".to_string()),
            kind: kind.to_string(),
            sdh: false,
            forced: false,
        }
    }

    #[test]
    fn writes_and_reads_an_unmatched_folder_key() {
        let db = fixture();
        let row = db
            .with_conn(|conn| {
                Ok(write_track_choice(
                    conn,
                    1,
                    "folder:1:Alpha",
                    Some(&desc("main")),
                    &SubtitleChoiceRow::Off,
                ))
            })
            .unwrap()
            .unwrap();
        assert_eq!(row.series_key, "folder:1:Alpha");
        assert_eq!(row.audio, Some(desc("main")));
        assert_eq!(row.subtitle, SubtitleChoiceRow::Off);

        let read = db
            .with_conn(|conn| Ok(read_track_choice(conn, 1, "folder:1:Alpha")))
            .unwrap()
            .unwrap();
        assert_eq!(read.unwrap().subtitle, SubtitleChoiceRow::Off);
    }

    /// An unresolved key is a 404 and writes nothing.
    #[test]
    fn an_unresolved_key_is_refused_and_writes_nothing() {
        let db = fixture();
        let result = db
            .with_conn(|conn| {
                Ok(write_track_choice(
                    conn,
                    1,
                    "tmdb:show:999",
                    None,
                    &SubtitleChoiceRow::Unset,
                ))
            })
            .unwrap();
        assert_eq!(result, Err(TrackChoiceError::UnresolvedSeriesKey));
        let count = db
            .with_conn(|conn| {
                conn.query_row("SELECT COUNT(*) FROM profile_track_choice", [], |r| {
                    r.get::<_, i64>(0)
                })
                .map_err(|e| e.to_string())
            })
            .unwrap();
        assert_eq!(count, 0);
    }
}

#[cfg(test)]
mod scoping_tests {
    use super::*;
    use crate::item_links::series_key_for_item;
    use nightjar_db::{NewLibrary, UpsertItem};

    /// A shows library with two episodes in one folder, and a movies library
    /// with two movies.
    fn fixture() -> (nightjar_db::Db, i64, i64) {
        let db = nightjar_db::Db::open(std::path::Path::new(":memory:")).unwrap();
        db.with_conn(|conn| {
            conn.execute_batch(
                "INSERT INTO accounts (id, username, password_hash, role)
                     VALUES (1, 'a', 'x', 'owner');
                 INSERT INTO profiles (id, account_id, profile_ref, name)
                     VALUES (1, 1, 'aa', 'P');",
            )
            .map_err(|e| e.to_string())?;
            Ok(())
        })
        .unwrap();
        let shows = db
            .create_library(&NewLibrary {
                name: "shows".to_string(),
                path: "/media/shows".to_string(),
                kind: "shows".to_string(),
            })
            .unwrap();
        db.upsert_items_indexed(
            shows.id,
            &[
                UpsertItem {
                    path: "Alpha/Season 1/Alpha.S01E01.mkv".to_string(),
                    mtime_ms: 0,
                    size_bytes: 1,
                    title: "Alpha".to_string(),
                    kind: "episode".to_string(),
                    year: None,
                    season: Some(1),
                    episode: Some(1),
                    content_id: None,
                },
                UpsertItem {
                    path: "Alpha/Season 1/Alpha.S01E02.mkv".to_string(),
                    mtime_ms: 0,
                    size_bytes: 1,
                    title: "Alpha".to_string(),
                    kind: "episode".to_string(),
                    year: None,
                    season: Some(1),
                    episode: Some(2),
                    content_id: None,
                },
            ],
        )
        .unwrap();
        db.with_conn(|conn| {
            conn.execute(
                "INSERT INTO series (library_id, relpath, tmdb_show_id)
                 VALUES (?1, 'Alpha', NULL)",
                [shows.id],
            )
            .map_err(|e| e.to_string())?;
            Ok(())
        })
        .unwrap();
        let movies = db
            .create_library(&NewLibrary {
                name: "movies".to_string(),
                path: "/media/movies".to_string(),
                kind: "movies".to_string(),
            })
            .unwrap();
        db.upsert_items_indexed(
            movies.id,
            &[
                UpsertItem {
                    path: "One.mkv".to_string(),
                    mtime_ms: 0,
                    size_bytes: 1,
                    title: "One".to_string(),
                    kind: "movie".to_string(),
                    year: None,
                    season: None,
                    episode: None,
                    content_id: None,
                },
                UpsertItem {
                    path: "Two.mkv".to_string(),
                    mtime_ms: 0,
                    size_bytes: 1,
                    title: "Two".to_string(),
                    kind: "movie".to_string(),
                    year: None,
                    season: None,
                    episode: None,
                    content_id: None,
                },
            ],
        )
        .unwrap();
        (db, shows.id, movies.id)
    }

    fn episode_ids(db: &nightjar_db::Db, library_id: i64) -> Vec<i64> {
        db.with_conn(|conn| {
            let mut stmt = conn
                .prepare("SELECT id FROM media_items WHERE library_id = ?1 ORDER BY id")
                .map_err(|e| e.to_string())?;
            let rows = stmt
                .query_map([library_id], |r| r.get::<_, i64>(0))
                .map_err(|e| e.to_string())?;
            rows.collect::<Result<Vec<i64>, _>>()
                .map_err(|e| e.to_string())
        })
        .unwrap()
    }

    /// Next-episode reuse: two episodes in one folder derive the same series
    /// key, so a choice stored for episode one is found for episode two.
    #[test]
    fn two_episodes_in_one_folder_share_the_series_key() {
        let (db, shows, _) = fixture();
        let ids = episode_ids(&db, shows);
        let keys = db
            .with_conn(|conn| {
                let mut out = Vec::new();
                for id in &ids {
                    let row = conn
                        .query_row("SELECT path FROM media_items WHERE id = ?1", [id], |r| {
                            r.get::<_, String>(0)
                        })
                        .map_err(|e| e.to_string())?;
                    out.push(series_key_for_item(
                        conn,
                        *id,
                        shows,
                        &row,
                        "/media/shows",
                        "episode",
                    )?);
                }
                Ok(out)
            })
            .unwrap();
        assert_eq!(keys.len(), 2);
        assert_eq!(keys[0], keys[1], "the folder is the scope, not the episode");
        assert_eq!(keys[0], format!("folder:{shows}:Alpha"));

        let first = keys[0].clone();
        db.with_conn(|conn| {
            write_track_choice(conn, 1, &first, None, &SubtitleChoiceRow::Off)
                .map_err(|e| format!("{e:?}"))
        })
        .unwrap();
        let reused = db
            .with_conn(|conn| nightjar_db::load_track_choice(conn, 1, &keys[1]))
            .unwrap()
            .expect("episode two reads episode one's choice");
        assert_eq!(reused.subtitle, SubtitleChoiceRow::Off);
    }

    /// Movie isolation: a movie's series key is its own item key, so a choice
    /// on one movie is not found for another.
    #[test]
    fn a_choice_on_one_movie_does_not_apply_to_another() {
        let (db, _, movies) = fixture();
        let ids = episode_ids(&db, movies);
        let keys = db
            .with_conn(|conn| {
                let mut out = Vec::new();
                for id in &ids {
                    let row = conn
                        .query_row("SELECT path FROM media_items WHERE id = ?1", [id], |r| {
                            r.get::<_, String>(0)
                        })
                        .map_err(|e| e.to_string())?;
                    out.push(series_key_for_item(
                        conn,
                        *id,
                        movies,
                        &row,
                        "/media/movies",
                        "movie",
                    )?);
                }
                Ok(out)
            })
            .unwrap();
        assert_ne!(keys[0], keys[1], "a movie is a series of one");
        db.with_conn(|conn| {
            write_track_choice(conn, 1, &keys[0], None, &SubtitleChoiceRow::Off)
                .map_err(|e| format!("{e:?}"))
        })
        .unwrap();
        let other = db
            .with_conn(|conn| nightjar_db::load_track_choice(conn, 1, &keys[1]))
            .unwrap();
        assert_eq!(other, None, "the sibling movie has no override of its own");
    }

    /// ADR-0038 item 2: the table stores descriptions only. A `track_id` or
    /// `stream_index` column would be the promise item 2 forbids, so its
    /// absence is asserted rather than assumed.
    #[test]
    fn the_table_stores_no_stream_index_or_track_id() {
        let (db, _, _) = fixture();
        let columns: Vec<String> = db
            .with_conn(|conn| {
                let mut stmt = conn
                    .prepare("SELECT name FROM pragma_table_info('profile_track_choice')")
                    .map_err(|e| e.to_string())?;
                let rows = stmt
                    .query_map([], |r| r.get::<_, String>(0))
                    .map_err(|e| e.to_string())?;
                rows.collect::<Result<Vec<String>, _>>()
                    .map_err(|e| e.to_string())
            })
            .unwrap();
        assert!(!columns.is_empty(), "the instrument saw no columns");
        for forbidden in [
            "track_id",
            "stream_index",
            "audio_track_id",
            "subtitle_track_id",
        ] {
            assert!(
                !columns.iter().any(|c| c == forbidden),
                "{forbidden} must not be a column: {columns:?}"
            );
        }
    }
}
