//! The corpus, scored through the entry point the **scanner** calls.
//!
//! `corpus_run.rs` calls `nightjar_core::parse_filename` on the **basename**,
//! and 25 of the residual failures are cases where the answer is in a directory
//! that call never sees — `classify.py` puts them in `harness-gives-a-path`,
//! `slash-inside-the-name` and `pick-a-title-before-a-slash`.
//!
//! The product does not call `parse_filename`. It calls
//! `nightjar_scanner::stored_parse(path, library_root)`, which reads the path:
//! the immediate parent for the season and episode, the show folder for the
//! title. **This file is the same scoring against that call**, so the residual
//! can be split into what the parser owes and what the harness invented.
//!
//! **It is a second measurement, not a replacement.** `corpus_run.rs` is
//! untouched and every number reported tonight still comes from it.
//!
//! Env: `CASES`, `OUT`, and `ROOT` for the library root (default `/media`).
//! No DB, no network.

use nightjar_core::MediaKind;
use nightjar_scanner::stored_parse;
use nightjar_metadata::clean_show_title;
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
    let mut out = Vec::new();

    for c in &cases {
        let input = c["input"].as_str().unwrap();
        let expect = &c["expect"];
        if expect.as_object().map(|o| o.is_empty()).unwrap_or(true) {
            out.push(json!({"input": input, "form": c["form"], "verdict": "not_applicable"}));
            continue;
        }
        let root = std::env::var("ROOT").unwrap_or_else(|_| "/media".into());
        // The scanner is handed a stored path, not a basename. A case written
        // as a bare release name is a file directly under the root, which is
        // what the scanner would hand it too.
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
