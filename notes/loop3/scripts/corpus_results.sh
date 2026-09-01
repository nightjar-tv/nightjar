#!/usr/bin/env bash
# The parser corpus, with the **per-case** results kept.
#
#   corpus_results.sh <tree> <results.json>
#
# `run_corpus.sh` prints a rate and throws the rows away in its temp dir, so a
# failure cannot be looked at. This is the same throwaway wrapper crate, keeping
# what the wrapper wrote.
#
# The corpus is parse-level by construction — `corpus_run.rs` calls
# `stored_parse` and `clean_show_title` and nothing else — so it cannot see a
# change in the matcher. That is a reason to report its number, not to skip it.
#
# **It does see the scanner, and did not until 2026-09-02.** `corpus_run.rs`
# passed a basename, which stopped modelling the product at `2af44de` (#172);
# it now calls `nightjar_scanner::stored_parse`, so this crate links the
# scanner. `ROOT` is passed through as the library root.
set -euo pipefail
# An inherited CARGO_TARGET_DIR sends the binary where this script does not look.
unset CARGO_TARGET_DIR
TREE=${1:?tree}
DEST=${2:?results.json}
SPIKE=$HOME/nightjar-spikes/parser-corpus-2026-08-13
WORK=$(mktemp -d)
trap 'rm -rf "$WORK"' EXIT

mkdir -p "$WORK/crate/src"
cp "$SPIKE/corpus_run.rs" "$WORK/crate/src/main.rs"
cat > "$WORK/crate/Cargo.toml" <<TOML
[package]
name = "corpus_run"
version = "0.0.1"
edition = "2024"
publish = false
[dependencies]
nightjar-core = { path = "$TREE/server/crates/core" }
nightjar-metadata = { path = "$TREE/server/crates/metadata" }
nightjar-scanner = { path = "$TREE/server/crates/scanner" }
serde_json = "1"
[workspace]
TOML
(cd "$WORK/crate" && cargo build --release -q 2>&1 | tail -20)
[ -x "$WORK/crate/target/release/corpus_run" ] || {
  echo "wrapper did not build — refusing to report a rate"; exit 1; }
CASES="$SPIKE/out/cases.json" OUT="$DEST" ROOT="${ROOT:-/media}" "$WORK/crate/target/release/corpus_run"
