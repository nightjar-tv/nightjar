//! B2-4 measurement-only probe for the continue-watching rollup.
//!
//! Not product code and not shipped: this example exists to time
//! `nightjar_metadata::continue_watching` against an already-migrated
//! derivative database. It opens the database read-only and never seeds,
//! migrates or writes.
//!
//! Usage:
//!   cargo run -p nightjar-metadata --example b2_4_measure -- <db_path> <profile_id> [limit]
//!
//! It times one first execution, then 30 warm executions, and prints JSON with
//! every duration in microseconds plus p50/p95/max and the ordered winners.

use std::path::PathBuf;
use std::time::Instant;

use nightjar_core::ViewerScope;
use nightjar_metadata::continue_watching;
use rusqlite::{Connection, OpenFlags};
use serde::Serialize;

const WARM_RUNS: usize = 30;

#[derive(Debug, Serialize)]
struct Winner {
    series_key: String,
    item_key: String,
    last_played_at: String,
}

#[derive(Debug, Serialize)]
struct Report {
    db_path: String,
    profile_id: i64,
    limit: Option<usize>,
    sqlite_version: Option<String>,
    warm_runs: usize,
    first_execution_us: u64,
    warm_durations_us: Vec<u64>,
    p50_us: u64,
    p95_us: u64,
    max_us: u64,
    output_count: usize,
    output_count_stable: bool,
    winners: Vec<Winner>,
    note: String,
}

fn main() {
    let (db_path, profile_id, limit) = match parse_args() {
        Ok(parsed) => parsed,
        Err(message) => {
            eprintln!("{message}");
            std::process::exit(2);
        }
    };

    let conn = Connection::open_with_flags(&db_path, OpenFlags::SQLITE_OPEN_READ_ONLY)
        .unwrap_or_else(|e| {
            eprintln!("open {}: {e}", db_path.display());
            std::process::exit(1);
        });

    let sqlite_version: Option<String> = conn
        .query_row("SELECT sqlite_version()", [], |r| r.get(0))
        .ok();

    let start = Instant::now();
    let first = match continue_watching(&conn, profile_id, limit, &ViewerScope::Account) {
        Ok(entries) => entries,
        Err(e) => {
            eprintln!("continue_watching failed: {e}");
            std::process::exit(1);
        }
    };
    let first_execution_us = start.elapsed().as_micros() as u64;

    let output_count = first.len();
    let mut output_count_stable = true;
    let mut warm_durations_us = Vec::with_capacity(WARM_RUNS);
    for _ in 0..WARM_RUNS {
        let start = Instant::now();
        match continue_watching(&conn, profile_id, limit, &ViewerScope::Account) {
            Ok(entries) => {
                warm_durations_us.push(start.elapsed().as_micros() as u64);
                if entries.len() != output_count {
                    output_count_stable = false;
                }
            }
            Err(e) => {
                eprintln!("continue_watching failed on a warm run: {e}");
                std::process::exit(1);
            }
        }
    }

    let mut sorted = warm_durations_us.clone();
    sorted.sort_unstable();

    let winners: Vec<Winner> = first
        .iter()
        .map(|entry| Winner {
            series_key: entry.series_key.clone(),
            item_key: entry.item_key.clone(),
            last_played_at: entry.last_played_at.clone(),
        })
        .collect();

    let report = Report {
        db_path: db_path.display().to_string(),
        profile_id,
        limit,
        sqlite_version,
        warm_runs: WARM_RUNS,
        first_execution_us,
        warm_durations_us,
        p50_us: percentile(&sorted, 50.0),
        p95_us: percentile(&sorted, 95.0),
        max_us: sorted.last().copied().unwrap_or(0),
        output_count,
        output_count_stable,
        winners,
        note: "Measurement only. Read-only open; the database was not seeded or mutated. \
               Durations are wall-clock microseconds. p50/p95 use nearest-rank over the 30 \
               warm runs. No pass threshold is implied."
            .to_string(),
    };

    println!("{}", serde_json::to_string_pretty(&report).unwrap());
}

fn percentile(sorted: &[u64], p: f64) -> u64 {
    if sorted.is_empty() {
        return 0;
    }
    let rank = (p / 100.0 * sorted.len() as f64).ceil() as usize;
    let index = rank.saturating_sub(1).min(sorted.len() - 1);
    sorted[index]
}

fn parse_args() -> Result<(PathBuf, i64, Option<usize>), String> {
    let mut args = std::env::args().skip(1);
    let db = args.next().ok_or_else(usage)?;
    let profile_id = args
        .next()
        .ok_or_else(usage)?
        .parse::<i64>()
        .map_err(|_| usage())?;
    let limit = match args.next() {
        Some(raw) => Some(raw.parse::<usize>().map_err(|_| usage())?),
        None => None,
    };
    if args.next().is_some() {
        return Err(usage());
    }
    Ok((PathBuf::from(db), profile_id, limit))
}

fn usage() -> String {
    "usage: b2_4_measure <db_path> <profile_id> [limit]".to_string()
}
