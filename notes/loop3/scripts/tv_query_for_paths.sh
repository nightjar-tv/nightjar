#!/usr/bin/env bash
# The TV search query each path would carry **if its kind were episode**.
#
#   tv_query_for_paths.sh <tree>          # paths on stdin, one per line
#
# Item 3's rule — a file inside a numbered season directory is not a film —
# moves `tv.episodetitle`'s files from a movie search to a TV one. Whether the
# oracle can judge that rule depends on whether the replay cache already holds
# those TV searches, and that question needs the query the *shipped* cleaner
# builds, not the raw basename.
#
# `oracle_query` cannot answer it: it picks the kind itself from
# `parse_filename`, and for these names the parser says movie, so it prints
# `clean_movie_title`. This asks the other question directly.
#
# Same throwaway-wrapper pattern as `run_corpus.sh`: a crate whose only
# dependencies are the tree's own `nightjar-core` and `nightjar-metadata`, built
# in a temp dir and left nowhere.
set -euo pipefail
# An inherited CARGO_TARGET_DIR sends the binary somewhere this script does not
# look, and the failure reads as "the wrapper did not build".
unset CARGO_TARGET_DIR
TREE=${1:?tree}
WORK=$(mktemp -d)
trap 'rm -rf "$WORK"' EXIT

mkdir -p "$WORK/crate/src"
cat > "$WORK/crate/src/main.rs" <<'RS'
//! stdin: one relpath per line. stdout: `relpath \t tv-query`.
use nightjar_core::parse_filename;
use nightjar_metadata::clean_show_title;
use std::io::{BufRead, Write};

fn main() {
    let stdin = std::io::stdin();
    let stdout = std::io::stdout();
    let mut w = std::io::BufWriter::new(stdout.lock());
    for line in stdin.lock().lines() {
        let rel = line.unwrap();
        if rel.is_empty() {
            continue;
        }
        let base = rel.rsplit('/').next().unwrap_or(&rel);
        // `status_query_groups` for an episode: the parsed title, cleaned as a
        // show title, with no year.
        let (q, _) = clean_show_title(&parse_filename(base).title);
        writeln!(w, "{rel}\t{q}").unwrap();
    }
}
RS
cat > "$WORK/crate/Cargo.toml" <<TOML
[package]
name = "tv_query"
version = "0.0.1"
edition = "2024"
publish = false
[dependencies]
nightjar-core = { path = "$TREE/server/crates/core" }
nightjar-metadata = { path = "$TREE/server/crates/metadata" }
[workspace]
TOML
(cd "$WORK/crate" && cargo build --release -q 2>&1 | tail -20)
[ -x "$WORK/crate/target/release/tv_query" ] || {
  echo "wrapper did not build — refusing to print a query"; exit 1; }
"$WORK/crate/target/release/tv_query"
