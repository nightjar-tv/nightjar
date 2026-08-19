#!/usr/bin/env bash
# Run the reduced Sonarr/Radarr parser corpus against one tree.
#
#   run_corpus.sh <tree> [out.json]
#
# The corpus spike ships `corpus_run.rs` and the cases but no runner, so this
# builds the same throwaway wrapper crate the parser sweep uses: a crate whose
# only dependencies are the tree's own `nightjar-core` and `nightjar-metadata`.
# Nothing here is product code and nothing is left behind.
#
# `corpus_run.rs` calls `nightjar_core::parse_filename` and
# `nightjar_metadata::clean_show_title` and nothing else, so it is parse-level
# by construction and cannot see a change in the matcher. That is a reason to
# report its number, not a reason to skip it — it is the instrument that catches
# parse collateral.
set -euo pipefail
TREE=${1:?tree}
OUTJSON=${2:-/dev/stdout}
SPIKE=$HOME/nightjar-spikes/parser-corpus-2026-08-13
WORK=$(mktemp -d)
trap 'rm -rf "$WORK"' EXIT

mkdir -p "$WORK/crate/src"
cp "$SPIKE/corpus_run.rs" "$WORK/crate/src/main.rs"
cat > "$WORK/crate/Cargo.toml" <<EOF
[package]
name = "corpus_run"
version = "0.0.1"
edition = "2024"
publish = false
[dependencies]
nightjar-core = { path = "$TREE/server/crates/core" }
nightjar-metadata = { path = "$TREE/server/crates/metadata" }
serde_json = "1"
[workspace]
EOF
(cd "$WORK/crate" && cargo build --release -q 2>&1 | tail -20)
CASES="$SPIKE/out/cases.json" OUT="$WORK/results.json" \
  "$WORK/crate/target/release/corpus_run"
python3 - "$WORK/results.json" "$OUTJSON" <<'PY'
import json, sys, collections
rs = json.load(open(sys.argv[1]))
c = collections.Counter(r["verdict"] for r in rs)
applicable = sum(v for k, v in c.items() if k != "not_applicable")
rate = 100.0 * c["pass"] / applicable if applicable else float("nan")
print("corpus: %d cases, %d applicable, pass %d, fail %d  =  %.1f%%"
      % (len(rs), applicable, c["pass"], c["fail"], rate))
# The soft-key rate as well as the exact one: the soft key is what matching
# consumes, and reporting only one of the two has hidden a difference before.
soft = sum(1 for r in rs if r.get("soft_pass"))
if any("soft_pass" in r for r in rs):
    print("        soft-key pass %d = %.1f%%" % (soft, 100.0 * soft / applicable))
json.dump({"counts": dict(c), "applicable": applicable, "rate": rate},
          open(sys.argv[2], "w") if sys.argv[2] != "/dev/stdout" else sys.stdout)
PY
