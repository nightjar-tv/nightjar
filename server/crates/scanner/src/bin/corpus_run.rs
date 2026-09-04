//! Run the shipped parser over the reduced Sonarr/Radarr corpus, **through the
//! entry point the product calls**.
//!
//! Calls `nightjar_scanner::stored_parse(store_path, library_root)` — the
//! function both scanner indexing paths call (`scanner/src/lib.rs:501`, `:951`).
//! It merges the basename with its immediate parent through
//! `parse_with_parent`, applies `stored_kind` to the merged record, walks the
//! path for a season, and falls back to the show folder for an empty title.
//!
//! Titles are compared through the shipped `clean_show_title` soft key as well
//! as exactly, because the soft key is what matching actually consumes; both
//! are reported.
//!
//! ## Why this is not `parse_filename` any more
//!
//! It was, and the reason it was is written down: *"because that is what the
//! scanner passes."* **`2af44de` — `scanner: one layer decides what a file is`
//! (#172), 2026-08-29 — stopped that being true.** It introduced `stored_parse`
//! and wired `parse_with_parent` behind it, so a harness passing a basename
//! models a caller the product no longer has.
//!
//! Measured at `f527198`, same cases, same scoring, only the call changed:
//! **598/734 (81.5%) through `parse_filename`, 608/734 (82.8%) through
//! `stored_parse`** — `+10 / −0`, no field gained in any failing case, and not
//! one title moved. All ten gains read a season and an episode out of the
//! immediate parent directory.
//!
//! ## One harness, and that is the rule
//!
//! `corpus_run_merge.rs` used to sit beside this file and score
//! `parse_with_parent` directly, as a projection of work that was not yet
//! wired. **It is deleted, in the commit that switched this file.** Two
//! binaries produced two rates, `590/734` and `600/734`, and they were quoted
//! interchangeably for weeks. **A second number must never be produced without
//! deleting the first**, and a file count enforces that where a convention did
//! not.
//!
//! For the record, the projection and this file agreed on all 734 verdicts and
//! differed on 8 parses — `parse_with_parent` alone fills a title from the
//! immediate parent, so it emitted `Season 3` and `Specials` as titles. Those
//! cases assert no title, so it was right for the wrong reason.
//!
//! ## The input is scored as written, and four cases stay red because of it
//!
//! The corpus carries Sonarr fixtures like
//! `C:\Test\Series\Season 01\01 Pilot (1080p HD).mkv`. **The product would
//! never receive that string**: `nightjar_db::require_relpath` refuses a drive
//! letter, a backslash and a leading slash outright — *"v1 is POSIX/Docker
//! only"*. The path walk splits on `/`, so a backslash path reads as one
//! segment and the walk finds nothing.
//!
//! **Normalising them here was measured and refused.** It scores 612/734
//! instead of 608, and it earns those four cases by editing the harness's own
//! inputs to produce a better number. They are recorded as **outside the
//! product's input contract** instead, which is a truer statement than a
//! rate that includes them.
//!
//! Env: `CASES` (cases.json), `OUT` (results.json), `ROOT` (library root,
//! default `/media`). No DB, no network, no filesystem — `stored_parse` and its
//! whole call graph are string and path arithmetic.

use nightjar_core::MediaKind;
use nightjar_metadata::clean_show_title;
use nightjar_scanner::stored_parse;
use serde_json::{Value, json};

/// Sonarr's `should_parse_series_name` asserts its own normalised key --
/// lowercase, punctuation and spaces removed. The extractor emits those as
/// `title_key` so they are compared on the same footing instead of failing on
/// spacing alone. Cases where their key does more than this (appending the
/// year, dropping stop-words) are marked not-applicable at extraction.
fn squash(s: &str) -> String {
    s.chars().filter(|c| c.is_ascii_alphanumeric()).flat_map(|c| c.to_lowercase()).collect()
}

fn main() {
    let cases: Vec<Value> =
        serde_json::from_str(&std::fs::read_to_string(std::env::var("CASES").unwrap()).unwrap())
            .unwrap();
    let out_path = std::env::var("OUT").unwrap();
    let root = std::env::var("ROOT").unwrap_or_else(|_| "/media".into());
    let mut out = Vec::new();

    for c in &cases {
        let input = c["input"].as_str().unwrap();
        let expect = &c["expect"];
        if expect.as_object().map(|o| o.is_empty()).unwrap_or(true) {
            out.push(json!({"input": input, "form": c["form"], "verdict": "not_applicable"}));
            continue;
        }
        // **`input` is what goes in the output, unchanged.** `classify.py --diff`
        // compares two result files positionally and asserts the inputs match;
        // a harness that recorded a rewritten string would break every
        // historical comparison. A prototype did exactly that and the assert
        // caught it.
        let p = stored_parse(input, &root);

        let mut checks: Vec<(&str, bool, String, String)> = Vec::new();

        if expect.get("reject").is_some() {
            let claimed = p.season.is_some() && p.episode.is_some();
            checks.push((
                "no_episode_claimed",
                !claimed,
                "no episode".into(),
                if claimed {
                    format!("S{:?}E{:?}", p.season.unwrap(), p.episode.unwrap())
                } else {
                    "no episode".into()
                },
            ));
        }
        if let Some(s) = expect.get("season").and_then(|v| v.as_i64()) {
            checks.push(("season", p.season == Some(s as i32), s.to_string(), format!("{:?}", p.season)));
        }
        if let Some(e) = expect.get("episode").and_then(|v| v.as_i64()) {
            checks.push(("episode", p.episode == Some(e as i32), e.to_string(), format!("{:?}", p.episode)));
        }
        if let Some(list) = expect.get("episodes").and_then(|v| v.as_array()) {
            let want: Vec<i32> = list.iter().filter_map(|v| v.as_i64()).map(|v| v as i32).collect();
            let got = p.episode_numbers();
            checks.push(("episodes", got == want, format!("{want:?}"), format!("{got:?}")));
        }
        if let Some(t) = expect.get("title").and_then(|v| v.as_str()) {
            let want_soft = clean_show_title(t).0;
            let got_soft = clean_show_title(&p.title).0;
            checks.push(("title", want_soft == got_soft, t.to_string(), p.title.clone()));
        }
        if let Some(k) = expect.get("title_key").and_then(|v| v.as_str()) {
            let want_k = squash(k);
            let got_k = squash(&p.title);
            checks.push(("title_key", want_k == got_k, k.to_string(), p.title.clone()));
        }
        if let Some(y) = expect.get("year").and_then(|v| v.as_i64()) {
            checks.push(("year", p.year == Some(y as i32), y.to_string(), format!("{:?}", p.year)));
        }

        let failed: Vec<&(&str, bool, String, String)> = checks.iter().filter(|c| !c.1).collect();
        out.push(json!({
            "input": input, "form": c["form"], "src": c["src"], "method": c["method"],
            "verdict": if failed.is_empty() { "pass" } else { "fail" },
            "failed": failed.iter().map(|(f, _, w, g)| json!({"field": f, "want": w, "got": g})).collect::<Vec<_>>(),
            "parsed": {
                "title": p.title, "season": p.season, "episode": p.episode,
                "episode_end": p.episode_end, "year": p.year,
                "kind": match p.kind { MediaKind::Episode => "episode", MediaKind::Movie => "movie", _ => "other" },
            },
        }));
    }

    std::fs::write(&out_path, serde_json::to_string(&out).unwrap()).unwrap();
    let pass = out.iter().filter(|r| r["verdict"] == "pass").count();
    let fail = out.iter().filter(|r| r["verdict"] == "fail").count();
    let na = out.iter().filter(|r| r["verdict"] == "not_applicable").count();
    println!("pass {pass}  fail {fail}  not_applicable {na}  total {}", out.len());
    println!("applicable pass rate: {:.1}%", 100.0 * pass as f64 / (pass + fail) as f64);
}
