#!/usr/bin/env python3
"""Validate the naming corpus against its provenance.

Checks the generated corpus and the pinned sources, then reports every failure
at once. `--self-test` runs the same validator over five deliberately broken
copies and asserts that each one is rejected for its named reason.

Run from `server/`:

    python3 tools/naming_corpus/check.py
    python3 tools/naming_corpus/check.py --self-test
"""
import argparse
import copy
import hashlib
import json
import os
import sys

SERVER = os.path.dirname(os.path.dirname(os.path.dirname(os.path.abspath(__file__))))
NAMING = os.path.join(SERVER, "crates", "core", "tests", "fixtures", "naming")
DEVELOPMENT = os.path.join(NAMING, "development")
UPSTREAM = os.path.join(DEVELOPMENT, "upstream")
CORPUS = os.path.join(DEVELOPMENT, "corpus.json")
SET_ROOTS = ("regression", "development", "heldout", "stress")

SCHEMA_VERSION = 1
SET = "development"
MAX_INPUT_BYTES = 1024
MAX_CASES = 2000
CASE_FIELDS = ("id", "source", "test", "input", "expect", "dropped",
               "applicable", "reason", "category")


def sha256_file(path):
    h = hashlib.sha256()
    with open(path, "rb") as fh:
        for block in iter(lambda: fh.read(65536), b""):
            h.update(block)
    return h.hexdigest()


def load_json(path):
    with open(path, encoding="utf-8") as fh:
        return json.load(fh)


def validate(corpus_text, sources, upstream_dir, naming_dir):
    """Return a list of error strings; empty means the corpus is intact."""
    errors = []
    try:
        corpus = json.loads(corpus_text)
    except json.JSONDecodeError as exc:
        return [f"malformed JSON: {exc}"]

    if corpus.get("schema_version") != SCHEMA_VERSION:
        errors.append(f"schema_version is not {SCHEMA_VERSION}")
    if corpus.get("set") != SET:
        errors.append(f"set is not {SET!r}")
    cases = corpus.get("cases")
    if not isinstance(cases, list):
        return errors + ["cases is not a list"]

    declared = {s["file"]: s for s in sources.get("sources", [])}
    present = {f for f in os.listdir(upstream_dir) if f.endswith(".cs")}
    for f in sorted(present - set(declared)):
        errors.append(f"undeclared source: {f}")
    for f in sorted(set(declared) - present):
        errors.append(f"missing source: {f}")
    for f, entry in sorted(declared.items()):
        path = os.path.join(upstream_dir, f)
        if os.path.exists(path) and sha256_file(path) != entry["sha256"]:
            errors.append(f"hash mismatch: {f}")

    seen = set()
    applicable = 0
    categories = {}
    for case in cases:
        cid = case.get("id")
        for field in CASE_FIELDS:
            if field not in case:
                errors.append(f"case {cid!r} lacks field {field!r}")
        if cid in seen:
            errors.append(f"duplicate id: {cid}")
        seen.add(cid)
        if case.get("source") not in declared:
            errors.append(f"case {cid!r} names an undeclared source {case.get('source')!r}")
        if case.get("applicable"):
            applicable += 1
        categories[case.get("category")] = categories.get(case.get("category"), 0) + 1
        size = len(case.get("input", "").encode("utf-8"))
        if size > MAX_INPUT_BYTES:
            errors.append(f"case {cid!r} input is {size} bytes, over {MAX_INPUT_BYTES}")

    if len(cases) > MAX_CASES:
        errors.append(f"{len(cases)} cases over the {MAX_CASES} budget")
    counts = corpus.get("counts", {})
    if counts.get("total") != len(cases):
        errors.append(f"count drift: total {counts.get('total')} != {len(cases)}")
    if counts.get("applicable") != applicable:
        errors.append(f"count drift: applicable {counts.get('applicable')} != {applicable}")
    if counts.get("excluded") != len(cases) - applicable:
        errors.append(f"count drift: excluded {counts.get('excluded')} != {len(cases) - applicable}")
    if corpus.get("categories") != {k: categories[k] for k in sorted(categories)}:
        errors.append("count drift: category counts do not match the cases")
    if sorted(corpus.get("sources", [])) != sorted(declared):
        errors.append("corpus source list does not match the manifest")

    assigned = {}
    for root in SET_ROOTS:
        root_dir = os.path.join(naming_dir, root)
        if not os.path.isdir(root_dir):
            errors.append(f"set root missing: {root}")
            continue
        for name in sorted(os.listdir(root_dir)):
            if not name.endswith(".json"):
                continue
            try:
                other = load_json(os.path.join(root_dir, name))
            except json.JSONDecodeError:
                errors.append(f"set {root} holds malformed {name}")
                continue
            for case in other.get("cases", []):
                cid = case.get("id")
                if cid in assigned:
                    errors.append(f"case {cid!r} is in both {assigned[cid]} and {root}")
                assigned[cid] = root

    return errors


def self_test(corpus_text, sources, upstream_dir, naming_dir):
    """Prove the validator rejects each named mutation for its named reason."""
    base = json.loads(corpus_text)

    duplicate = copy.deepcopy(base)
    duplicate["cases"][1]["id"] = duplicate["cases"][0]["id"]

    drift = copy.deepcopy(base)
    drift["counts"]["applicable"] += 1

    oversized = copy.deepcopy(base)
    oversized["cases"][0]["input"] = "x" * (MAX_INPUT_BYTES + 1)

    missing = {**sources, "sources": sources["sources"] + [{"file": "ghost.cs", "sha256": "00"}]}

    checks = [
        ("malformed JSON", "{ not json", sources, "malformed JSON"),
        ("duplicate id", json.dumps(duplicate), sources, "duplicate id"),
        ("count drift", json.dumps(drift), sources, "count drift"),
        ("missing source", corpus_text, missing, "missing source"),
        ("oversized input", json.dumps(oversized), sources, f"over {MAX_INPUT_BYTES}"),
    ]
    failures = 0
    for label, text, mut_sources, needle in checks:
        errors = validate(text, mut_sources, upstream_dir, naming_dir)
        hit = any(needle in e for e in errors)
        print(f"  {'ok  ' if hit else 'FAIL'} {label}: {errors[0] if errors else 'accepted'}")
        if not hit:
            failures += 1
    if failures:
        print(f"self-test: {failures} control(s) did not fire")
        return 1
    print("self-test: all 5 controls rejected for their named reason")
    return 0


def main():
    ap = argparse.ArgumentParser(description=__doc__)
    ap.add_argument("--self-test", action="store_true")
    args = ap.parse_args()

    sources = load_json(os.path.join(UPSTREAM, "SOURCES.json"))
    with open(CORPUS, encoding="utf-8") as fh:
        corpus_text = fh.read()

    if args.self_test:
        return self_test(corpus_text, sources, UPSTREAM, NAMING)

    errors = validate(corpus_text, sources, UPSTREAM, NAMING)
    for error in errors:
        print(f"FAIL: {error}")
    if errors:
        print(f"corpus integrity: {len(errors)} failure(s)")
        return 1
    print("corpus integrity: ok")
    return 0


if __name__ == "__main__":
    sys.exit(main())
