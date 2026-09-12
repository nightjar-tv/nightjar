//! Read-only measure: episode-title agreement vs season fit, per show folder.
//!
//! Answers one question. Season fit (ADR-0043) rejects a whole folder when the
//! bound show's `seasons[]` cannot cover the folder's seasons, which discards
//! every file in that folder including ones that bind correctly today. Episode
//! titles are already in the filenames and are per-file evidence. Which
//! discriminates better?
//!
//! For every file in every folder whose bound show fails season fit, this
//! classifies the filename's episode title against the bound show's stored
//! episode list:
//!
//! - `title_confirms_slot` — title matches the episode at the scanned S/E
//! - `title_elsewhere` — title matches a different S/E on the same show
//! - `title_absent` — the show has episodes for that season, and none carries
//!   this title (the show is refuted)
//! - `season_not_fetched` — no episode rows for that season, so the title is
//!   untestable here (not evidence either way)
//! - `no_title_in_filename` — nothing after the SxxExx / NxNN token
//!
//! Uses the shipped extractor (`after_token_episode_title`) and the shipped
//! comparator (`norm_key`, which applies the orthography fold), so the numbers
//! describe what the matcher could actually do, not what a fresh regex could.
//!
//! Env: `DB` (read-only; a copy is expected). No provider calls, no writes.

use std::collections::{HashMap, HashSet};

use nightjar_metadata::{after_token_episode_title, episode_title_rejected, norm_key};
use rusqlite::Connection;

/// Compare provider ids by leading integer: some ids arrive in slug form
/// (`235930-devil-may-cry`) and a string compare calls equal ids different.
fn leading_int(s: &str) -> Option<i64> {
    let digits: String = s
        .trim()
        .chars()
        .take_while(|c| c.is_ascii_digit())
        .collect();
    digits.parse().ok()
}

#[derive(Debug, Clone)]
struct Item {
    path: String,
    season: Option<i32>,
    episode: Option<i32>,
    status: String,
}

#[derive(Debug, Default, Clone, Copy)]
struct Tally {
    confirms_slot: usize,
    elsewhere: usize,
    absent: usize,
    season_not_fetched: usize,
    no_title: usize,
    /// `Episode 7`, `Ep 7`, or the show's own name: carries no identity, so a
    /// match on it is not evidence. The shipped ADR-0032 title pin rejects
    /// exactly these, and counting them as agreement would inflate the result.
    generic_title: usize,
}

impl Tally {
    fn total(&self) -> usize {
        self.confirms_slot
            + self.elsewhere
            + self.absent
            + self.season_not_fetched
            + self.no_title
            + self.generic_title
    }
    fn add(&mut self, o: &Tally) {
        self.confirms_slot += o.confirms_slot;
        self.elsewhere += o.elsewhere;
        self.absent += o.absent;
        self.season_not_fetched += o.season_not_fetched;
        self.no_title += o.no_title;
        self.generic_title += o.generic_title;
    }
}

fn main() {
    let db = std::env::var("DB").expect("set DB to a read-only copy of nightjar.db");
    let conn = Connection::open_with_flags(
        &db,
        rusqlite::OpenFlags::SQLITE_OPEN_READ_ONLY | rusqlite::OpenFlags::SQLITE_OPEN_URI,
    )
    .expect("open db read-only");

    // ---- folder identity: the series rows, longest-prefix matched -----------
    let mut series: HashMap<(i64, String), i64> = HashMap::new();
    {
        let mut st = conn
            .prepare("SELECT library_id, relpath, tmdb_show_id FROM series")
            .unwrap();
        let rows = st
            .query_map([], |r| {
                Ok((
                    r.get::<_, i64>(0)?,
                    r.get::<_, String>(1)?,
                    r.get::<_, Option<i64>>(2)?,
                ))
            })
            .unwrap();
        for row in rows {
            let (lib, relpath, show) = row.unwrap();
            // A row with a null `tmdb_show_id` is a folder that has formed a
            // group and has no entity yet (ADR-0039 item 3). It is not
            // identity, so it is not a folder this measurement can key on.
            if let Some(show) = show {
                series.insert((lib, relpath), show);
            }
        }
    }
    let folder_of = |lib: i64, path: &str| -> String {
        let parts: Vec<&str> = path.split('/').collect();
        for n in (1..parts.len()).rev() {
            let cand = parts[..n].join("/");
            if series.contains_key(&(lib, cand.clone())) {
                return cand;
            }
        }
        parts.first().map(|s| s.to_string()).unwrap_or_default()
    };

    // ---- the bound show's season list, from the stored /tv/{id} payload ----
    let mut show_seasons: HashMap<i64, HashSet<i32>> = HashMap::new();
    let mut show_name: HashMap<i64, String> = HashMap::new();
    {
        // (provider, entity_kind, provider_id) — a show id and an episode id
        // can be the same integer, so entity_kind always constrains the read.
        let mut st = conn
            .prepare(
                "SELECT provider_id, payload FROM metadata_raw_payloads
                 WHERE provider = 'tmdb' AND entity_kind = 'tv'",
            )
            .unwrap();
        let rows = st
            .query_map([], |r| Ok((r.get::<_, String>(0)?, r.get::<_, String>(1)?)))
            .unwrap();
        for row in rows {
            let (pid, payload) = row.unwrap();
            let Some(id) = leading_int(&pid) else {
                continue;
            };
            let Ok(v) = serde_json::from_str::<serde_json::Value>(&payload) else {
                continue;
            };
            if let Some(n) = v.get("name").and_then(|n| n.as_str()) {
                show_name.insert(id, n.to_string());
            }
            let set: HashSet<i32> = v
                .get("seasons")
                .and_then(|s| s.as_array())
                .map(|arr| {
                    arr.iter()
                        .filter_map(|s| s.get("season_number")?.as_i64())
                        .map(|n| n as i32)
                        .collect()
                })
                .unwrap_or_default();
            show_seasons.insert(id, set);
        }
    }

    // ---- the bound show's projected episodes: (show, season, ep) -> title --
    let mut episodes: HashMap<(i64, i32, i32), String> = HashMap::new();
    let mut seasons_with_rows: HashSet<(i64, i32)> = HashSet::new();
    {
        let mut st = conn
            .prepare(
                "SELECT tmdb_show, season, episode, title FROM metadata_canonical
                 WHERE provider = 'tmdb' AND entity_kind = 'episode'
                   AND tmdb_show IS NOT NULL AND season IS NOT NULL AND episode IS NOT NULL",
            )
            .unwrap();
        let rows = st
            .query_map([], |r| {
                Ok((
                    r.get::<_, i64>(0)?,
                    r.get::<_, i32>(1)?,
                    r.get::<_, i32>(2)?,
                    r.get::<_, String>(3)?,
                ))
            })
            .unwrap();
        for row in rows {
            let (show, s, e, t) = row.unwrap();
            seasons_with_rows.insert((show, s));
            episodes.insert((show, s, e), t);
        }
    }
    // Reverse index for "anywhere on this show".
    let mut by_show_title: HashMap<i64, HashMap<String, (i32, i32)>> = HashMap::new();
    for ((show, s, e), t) in &episodes {
        by_show_title
            .entry(*show)
            .or_default()
            .entry(norm_key(t))
            .or_insert((*s, *e));
    }

    // ---- items ------------------------------------------------------------
    let mut items: Vec<(i64, Item)> = Vec::new();
    {
        let mut st = conn
            .prepare(
                "SELECT id, library_id, path, season, episode, metadata_status
                 FROM media_items",
            )
            .unwrap();
        let rows = st
            .query_map([], |r| {
                Ok((
                    r.get::<_, i64>(1)?,
                    Item {
                        path: r.get(2)?,
                        season: r.get(3)?,
                        episode: r.get(4)?,
                        status: r.get(5)?,
                    },
                ))
            })
            .unwrap();
        for row in rows {
            items.push(row.unwrap());
        }
    }

    let mut by_folder: HashMap<(i64, String), Vec<Item>> = HashMap::new();
    for (lib, it) in &items {
        by_folder
            .entry((*lib, folder_of(*lib, &it.path)))
            .or_default()
            .push(it.clone());
    }

    // ---- which folders does season fit reject? -----------------------------
    println!("DB: {db}");
    println!();
    println!(
        "{:<44} {:>7} {:>5} {:>5} {:>5} {:>5} {:>5} {:>5} {:>5}",
        "folder", "show", "files", "slot", "elsw", "absnt", "nofet", "gnrc", "ready"
    );
    println!("{}", "-".repeat(104));

    let mut grand = Tally::default();
    let mut grand_files = 0usize;
    let mut grand_ready = 0usize;
    let mut rows_out: Vec<(String, i64, Tally, usize)> = Vec::new();
    let mut absent_samples: Vec<String> = Vec::new();

    for ((lib, folder), its) in &by_folder {
        let Some(&show) = series.get(&(*lib, folder.clone())) else {
            continue;
        };
        let required: HashSet<i32> = its
            .iter()
            .filter_map(|i| i.season)
            .filter(|s| *s != 0)
            .collect();
        let have = show_seasons.get(&show);
        let fits = match have {
            None => true,
            Some(h) => required.iter().all(|s| h.contains(s)),
        };
        if fits || required.is_empty() {
            continue; // season fit leaves this folder alone
        }

        let mut t = Tally::default();
        let ready = its.iter().filter(|i| i.status == "ready").count();
        for it in its {
            let (Some(s), Some(e)) = (it.season, it.episode) else {
                t.no_title += 1;
                continue;
            };
            let base = it.path.rsplit('/').next().unwrap_or(&it.path);
            let Some(raw) = after_token_episode_title(base, s, e) else {
                t.no_title += 1;
                continue;
            };
            let want = norm_key(&raw);
            if want.is_empty() {
                t.no_title += 1;
                continue;
            }
            if episode_title_rejected(&raw, folder) {
                t.generic_title += 1;
                continue;
            }
            if let Some(actual) = episodes.get(&(show, s, e))
                && norm_key(actual) == want
            {
                t.confirms_slot += 1;
                continue;
            }
            if let Some(found) = by_show_title.get(&show).and_then(|m| m.get(&want)) {
                let _ = found;
                t.elsewhere += 1;
                continue;
            }
            if seasons_with_rows.contains(&(show, s)) {
                t.absent += 1;
                if absent_samples.len() < 60 {
                    absent_samples.push(format!(
                        "  {folder} S{s}E{e}\n      file : {raw}\n      tmdb : {}",
                        episodes
                            .get(&(show, s, e))
                            .cloned()
                            .unwrap_or_else(|| "(no episode at this number)".into())
                    ));
                }
            } else {
                t.season_not_fetched += 1;
            }
        }
        grand.add(&t);
        grand_files += t.total();
        grand_ready += ready;
        rows_out.push((folder.clone(), show, t, ready));
    }

    rows_out.sort_by_key(|(_, _, t, _)| std::cmp::Reverse(t.total()));
    for (folder, show, t, ready) in &rows_out {
        let name = show_name.get(show).cloned().unwrap_or_default();
        println!(
            "{:<44} {:>7} {:>5} {:>5} {:>5} {:>5} {:>5} {:>5} {:>5}   [{}]",
            folder.chars().take(44).collect::<String>(),
            show,
            t.total(),
            t.confirms_slot,
            t.elsewhere,
            t.absent,
            t.season_not_fetched,
            t.generic_title,
            ready,
            name.chars().take(38).collect::<String>(),
        );
    }

    println!();
    println!("folders season fit rejects            : {}", rows_out.len());
    println!("files in them                         : {grand_files}");
    println!("  currently ready                     : {grand_ready}");
    println!();
    println!(
        "SEASON FIT resolves correctly         : 0   (rejects the folder; every file unmatched)"
    );
    println!(
        "TITLE MATCH confirms the binding      : {}   (title matches the episode at the scanned S/E)",
        grand.confirms_slot
    );
    println!(
        "TITLE MATCH refutes the binding       : {}   (season fetched, no episode carries this title)",
        grand.absent
    );
    println!(
        "  title found at a different S/E      : {}   (right show, wrong number — an ordering problem)",
        grand.elsewhere
    );
    println!(
        "  untestable, season never fetched    : {}",
        grand.season_not_fetched
    );
    println!("  no title in the filename            : {}", grand.no_title);
    println!(
        "  generic title, no identity          : {}   (`Episode 7` and the like — excluded, not evidence)",
        grand.generic_title
    );
    println!();
    println!("--- every `absent` file: is this refutation, or a title-format difference? ---");
    for line in &absent_samples {
        println!("{line}");
    }
}
