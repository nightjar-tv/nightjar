#!/usr/bin/env bash
# The dogfood parse probe, over the **database's** 25,043 paths.
#
#   dogfood_probe.sh <tree> <out.tsv>
#
# The strict replay pair reads `capture-media-mac.jsonl`, which holds 25,004
# paths. The database holds 25,043, and the 610-path gap has hidden a
# regression once. This reads the database — read-only, `mode=ro`, and the
# paths are extracted once into a file so nothing reopens it.
set -euo pipefail
unset CARGO_TARGET_DIR
TREE=${1:?tree}
DEST=${2:?out.tsv}
PATHS=${PATHS:-$HOME/nightjar-wt-loop5-scratch/dogfood-paths.tsv}
[ -s "$PATHS" ] || { echo "no path list at $PATHS"; exit 1; }
WORK=$(mktemp -d)
trap 'rm -rf "$WORK"' EXIT
mkdir -p "$WORK/crate/src"
cp "$(dirname "$0")/dogfood_probe.rs" "$WORK/crate/src/main.rs"
cat > "$WORK/crate/Cargo.toml" <<TOML
[package]
name = "dogfood_probe"
version = "0.0.1"
edition = "2024"
publish = false
[dependencies]
nightjar-core = { path = "$TREE/server/crates/core" }
nightjar-scanner = { path = "$TREE/server/crates/scanner" }
[workspace]
TOML
(cd "$WORK/crate" && cargo build --release -q 2>&1 | tail -20)
[ -x "$WORK/crate/target/release/dogfood_probe" ] || {
  echo "probe did not build — refusing to report a count"; exit 1; }
"$WORK/crate/target/release/dogfood_probe" < "$PATHS" > "$DEST"
echo "rows: $(wc -l < "$DEST")  paths: $(( $(wc -l < "$DEST") / 2 ))"
