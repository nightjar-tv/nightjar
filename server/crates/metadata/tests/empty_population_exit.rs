//! Proves `metadata-match-measure` treats an empty population as a failure:
//! the run exits non-zero and prints no report (so no fraction of `0.0`),
//! naming the exclusion list applied and the libraries found in the database.
//! Plan: 2026-09-05-an-empty-population-is-not-a-pass.

use std::process::Command;

#[test]
fn empty_population_exits_nonzero_and_prints_no_fraction() {
    let dir = tempfile::tempdir().unwrap();
    let db_path = dir.path().join("nightjar.db");
    {
        let con = rusqlite::Connection::open(&db_path).unwrap();
        con.execute_batch(
            "CREATE TABLE libraries (id INTEGER PRIMARY KEY, name TEXT NOT NULL);
             INSERT INTO libraries (id, name) VALUES (1, 'DV'), (2, 'Main');
             CREATE TABLE media_items (
                 id INTEGER PRIMARY KEY,
                 library_id INTEGER NOT NULL,
                 kind TEXT NOT NULL,
                 title TEXT NOT NULL,
                 year INTEGER,
                 path TEXT NOT NULL,
                 season INTEGER,
                 episode INTEGER
             );
             INSERT INTO media_items (id, library_id, kind, title, year, path)
             VALUES (1, 1, 'movie', 'Everything Everywhere All at Once', 2022,
                     '/srv/DV/Everything.Everywhere.All.at.Once.2022.mkv');",
        )
        .unwrap();
    }

    // Exclusion name 'DV' swallows the only library that holds items; 'Main'
    // stays in the database so the message can name what was found.
    let out = Command::new(env!("CARGO_BIN_EXE_metadata-match-measure"))
        .env("DB", &db_path)
        .env("EXCLUDE_TESTDATA", "1")
        .env("MEASURE_EXCLUDE_LIBRARY_NAMES", "DV")
        // Credentials are supplied so the only non-zero exit available is the
        // empty-population one. Without this the run exits 1 from
        // resolve_credentials instead, and the test passes whether or not the
        // behaviour under test exists at all.
        .env(
            "NIGHTJAR_TMDB_API_KEY",
            "test-key-not-used-no-request-is-made",
        )
        .output()
        .unwrap();

    assert_eq!(
        out.status.code(),
        Some(1),
        "an empty population must exit non-zero"
    );
    let stderr = String::from_utf8_lossy(&out.stderr);
    assert!(
        stderr.contains("measured nothing"),
        "stderr must explain the empty population: {stderr}"
    );
    assert!(
        stderr.contains("DV") && stderr.contains("Main"),
        "stderr must name the applied exclusion and the libraries found: {stderr}"
    );
    assert!(
        out.stdout.is_empty(),
        "an empty population must not print a report with a fraction: {}",
        String::from_utf8_lossy(&out.stdout)
    );
}
