//! Library scanner: index pass, then bounded ffprobe pool (ADR-0004).

mod ffprobe_child;
mod keymap;
mod pool;
mod probe;
mod reachability;
mod walk;
mod watch;

pub use keymap::{KeyframeEntry, KeyframeMapBuild, build_keyframe_map};
pub use pool::LibraryPool;
pub use probe::{ProbeResult, ffprobe};
pub use reachability::{Reachability, allow_delete_missing, check_root};
pub use walk::{
    WalkCache, WalkOutcome, is_media, walk_concurrency, walk_media_files_cached,
    walk_media_files_cached_with_concurrency,
};
pub use watch::spawn_library_watcher;

use nightjar_core::MediaKind;
use nightjar_db::{
    Db, ItemPathRow, ScanAdmission, UpsertItem, fold_path, resolve_media_path,
    season_number_for_path, show_folder_relpath, to_relpath, under_numbered_season_directory,
};
use std::collections::{HashMap, HashSet};
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::{Duration, Instant};

/// The title an episode takes when its filename carries none.
///
/// `parse_filename` returns an empty title when the name has no series title
/// in it at all — `S03E09 WS PDTV XviD FUtV`, `1x04`. The folder carries the
/// title in that layout, and **the scanner is the layer that has the folder**;
/// the parser only ever sees a basename.
///
/// The folder is the show folder, so `Season 1/` and `Specials/` walk up to it
/// — the same [`show_folder_relpath`] the queue groups by, so the two cannot
/// disagree. A file sitting directly in the library root has no folder to
/// borrow from and keeps the empty title, which `drain_pending` then refuses to
/// search on.
///
/// **Only an episode reaches this.** The parser's movie and season-pack arms
/// substitute the stem rather than return an empty title, so a movie whose name
/// is only release junk keeps `1080p x264` as its title. Giving it a folder
/// instead means changing the rule in `cut_at_title_junk` that a name of only
/// junk keeps its junk, and that is a change with its own blast radius and its
/// own measurement, not a branch to leave here untaken.
fn title_from_folder(stored: &str, library_root: &str) -> String {
    show_folder_relpath(stored, library_root)
        .rsplit('/')
        .next()
        .unwrap_or_default()
        .trim()
        .to_string()
}

/// The title the scanner stores for one file: the parsed title, or the show
/// folder's name when the parse carries none.
///
/// **One rule, three consumers (Rule 4.11).** Both indexing paths built this
/// expression inline and identically ([`hint_ingest`] and the batch walk), and
/// the matcher oracle's replay harness needs the same answer — it loads a
/// capture instead of walking a filesystem, so it re-derives every field the
/// scanner interprets. It was re-deriving this one with `parse_filename` alone,
/// which is not this rule: a titleless episode came out with an empty title, an
/// empty title is not a query, and 5,644 generated rows scored `absent` for a
/// reason that was the harness rather than the product. A reimplemented
/// predicate has misreported this project before — hence a shared function
/// rather than a copy.
///
/// Takes the title by value so the callers still move it rather than clone.
pub fn stored_title(parsed_title: String, stored: &str, library_root: &str) -> String {
    if parsed_title.is_empty() {
        title_from_folder(stored, library_root)
    } else {
        parsed_title
    }
}

/// The whole stored record for one file, from its path.
///
/// **One layer decides.** Until now the parser answered from a basename, the
/// scanner overrode `kind` afterwards with [`stored_kind`], and
/// `nightjar_core::parse_filename_in` had a third opinion nothing called. Three
/// places, three answers, and eight corpus cases sat on the disagreement.
///
/// ## The reconciliation, as a rule rather than as control flow
///
/// | field | who decides | with what |
/// |---|---|---|
/// | season, episode, year | `parse_with_parent` | the basename and its **immediate parent**, by the precedence stated there |
/// | title | the basename, then [`stored_title`] | **not the immediate parent** — that is often `Season 1`, and `stored_title` walks to the show folder |
/// | `kind` | this function, via [`stored_kind`] | the **merged** record and the path, never the basename alone |
/// | season, when still absent **and the record is an episode** | this function | `season_number_for_path`, which **walks** the path — `Show/Season 03/Extras/x.mkv` is season 3 |
/// | episode, when still absent **and a season is known** | this function | a leading one- or two-digit number in the basename |
/// | title, when still empty | this function, via [`stored_title`] | the show folder's name |
///
/// **[`stored_kind`] is no longer an override.** It was applied to the parser's
/// answer and could contradict it; it is applied to the merged record now, so
/// there is one chain and one decision point. Its rule is unchanged and its
/// tests are untouched — what changed is what it is handed.
///
/// **Both of the parser's blind spots are answered here** and nowhere else: a
/// basename cannot tell whether it is an episode, and it cannot tell whether its
/// number is absolute. This function sees the path, so it is the layer that can.
///
/// ## Why the walk stays here rather than moving into the merge
///
/// `nightjar_core` parses names and knows nothing about libraries or roots.
/// `season_number_for_path` is a path rule and lives in `nightjar-db` with the
/// rest of them. **The merge takes an immediate parent because that is a name;
/// the walk stays with the layer that owns paths.**
pub fn stored_parse(store_path: &str, library_root: &str) -> nightjar_core::ParsedName {
    let unix = store_path.replace('\\', "/");
    let base = unix.rsplit('/').next().unwrap_or(&unix).to_string();
    let parent = unix.rsplit('/').nth(1).map(str::to_string);
    let mut parsed = nightjar_core::parse_with_parent(&base, parent.as_deref());

    // **The immediate parent must not name the show.** `parse_with_parent`
    // fills an empty title from the directory beside the file, and that
    // directory is often `Season 1`. [`stored_title`] walks past the
    // season-directory tail to the show folder, which is the rule this scanner
    // has always used, so the title is handed back to it. **Nothing in the
    // 25,043-path library has an empty basename title, so the probe cannot see
    // this** — a test is the only thing that can.
    if nightjar_core::parse_filename(&base).title.is_empty() {
        parsed.title.clear();
    }

    // **The walk answers what one parent name cannot.** `Season 16` on its own
    // parses to no season — `find_bare_season` wants a letter in the head — and
    // a file two directories below its season folder has no season in its
    // parent at all. Three real library paths turn on the first and two on the
    // second.
    //
    // **An absolute number still refuses it**, for the reason
    // `ParsedName::episode_absolute` gives: the folder's season and a
    // series-wide number are different schemes, and pairing them binds a slot
    // that does not exist.
    // **Kind first, then the season — in that order, and the order is the
    // rule.** A season belongs to an episode. Filling it before the kind is
    // settled put season 5 on four real films:
    // `Futurama/Season 5/Futurama Bender's Big Score (2007).avi` and its three
    // siblings, which `stored_kind` correctly keeps as films because they carry
    // their own year. **`title_from_folder` names that exact file as the thing
    // not to do**, in this file, and a first draft did it anyway.
    parsed.kind = match stored_kind(parsed.kind, parsed.year, store_path, library_root) {
        "episode" => MediaKind::Episode,
        "movie" => MediaKind::Movie,
        _ => parsed.kind,
    };
    if parsed.kind == MediaKind::Episode && parsed.season.is_none() && !parsed.episode_absolute {
        parsed.season = season_number_for_path(store_path, library_root).map(|n| n as i32);
    }

    // **A leading number is the episode, but only once a season is known.**
    // `Season 01/01 Pilot (1080p HD).mkv` is episode 1, and the season above is
    // what licenses the claim — evidence a basename cannot see.
    //
    // **`under_numbered_season_directory` was written here as well and
    // removed.** Its control stayed green: `parsed.season.is_some()` refuses
    // everything it refused, in every case there is. A guard whose control
    // cannot fail is a comment, and this file has already dropped two on that
    // basis.
    //
    // **A first draft put this in the parser and it was wrong there.** The
    // sweep renders 360 names beginning with a one- or two-digit run, and
    // `12.Angry.Men.1080p.BluRay.x264-GRP.mkv` — a yearless film — became
    // episode 12. Nothing in a basename separates it from `01 Pilot`; the
    // season directory does.
    //
    // The year guard rides along for the same reason it does everywhere else in
    // this file: `65 (2023)` is a film even in a folder that looks televisual.
    if parsed.kind == MediaKind::Episode
        && parsed.episode.is_none()
        && parsed.season.is_some()
        && parsed.year.is_none()
        && let Some(n) = nightjar_core::leading_episode_number(&base)
    {
        parsed.episode = Some(n);
    }
    parsed.title = stored_title(parsed.title, store_path, library_root);
    parsed
}

/// The kind the scanner stores for one file: the parsed kind, except that a
/// file inside a **numbered** season directory is an episode — unless the
/// basename asserts its own year, which no episode title does and every film
/// does.
///
/// **One rule, three consumers (Rule 4.11)**, and the third is the reason this
/// is a function rather than two lines inline. Both scanner indexing paths need
/// it, and so does the matcher oracle's replay harness — which re-derives every
/// field the scanner interprets and has now been caught twice re-deriving one of
/// them differently. `stored_title` is the sibling this copies.
///
/// ## Why the folder decides
///
/// `parse_filename` takes a basename. `Closure.mkv` carries no season, no
/// episode and nothing that says "television", so the parser calls it a movie —
/// correctly, on the evidence it has. **The scanner is the layer that has the
/// folder**, and `Show/Season 1/Closure.mkv` is not a film. The oracle measures
/// 573 episode files bound to films for exactly this reason, and a wrong kind is
/// the worst verdict in the suite: the file has left the TV library altogether,
/// which no re-match inside that library can fix.
///
/// ## Why the file overrides the folder
///
/// **The folder is evidence, not proof.** The first cut of this rule read the
/// folder alone, and "a file under a season directory is not a film" is false
/// wherever a library files a film under `Season N/` — which the dogfood
/// library does, four times:
///
/// ```text
/// Futurama/Season 5/Futurama Bender's Big Score (2007).avi
/// Futurama/Season 5/Futurama Bender's Game (2008).avi
/// Futurama/Season 5/Futurama Into the Wild Green Yonder (2009).avi
/// Futurama/Season 5/Futurama The Beast with a Billion Backs (2008).avi
/// ```
///
/// Four standalone direct-to-DVD features, each with its own TMDB movie record.
/// They are misfiled — they belong in a specials directory — and the matcher
/// still has to cope, because real libraries are misfiled. **And it is worse
/// than a wrong search**: `episode_group_key` ignores the cleaned title when the
/// show folder is non-empty, so once these are episodes they join the group
/// bound to the Futurama series and cannot reach their movie records by any
/// route.
///
/// So the discriminator is the file, not the folder: **a basename asserting its
/// own year and carrying no episode marker is a film, wherever it sits.** A
/// `MediaKind::Movie` from `parse_filename` is already the statement that no
/// season/episode token was read — the parser's movie arm is the only one that
/// returns it, and it returns `season: None, episode: None` with it — so the
/// year is the one bit this needs beyond the kind.
///
/// **The cost is measured, and it is zero.** All 573 `wrong.kind` rows the
/// oracle scores are `tv.episodetitle` — `Dept. Q/Season 1/Episode 1.mkv` — and
/// **not one of the 573 carries a four-digit run of any kind in its basename**,
/// let alone a year. The rule buys the four Futurama films and gives up none of
/// the 573. What it does give up is an episode whose *title* contains a year and
/// which carries no episode number — `Season 1/Christmas 1999.mkv`. None exists
/// in the dogfood library or in any generated shape; it is the honest price, and
/// it is a file the folder rule was guessing about anyway.
///
/// ## Why *numbered*, and not any season directory
///
/// **The specials season is not numbered, deliberately.** TMDB models
/// `Top Gear: Polar Special` as a standalone movie record, so a file in a
/// specials folder may honestly bind to a film. The rule "a season directory
/// means the file is not a film" was tried, scored as a free win on the oracle,
/// and destroyed five correct bindings in the real library — because no
/// generated shape held a `Specials/` directory over a file whose right answer
/// was a movie. One does now (`movie.specials`, 1,712 rows).
///
/// That carve-out is [`is_numbered_season_directory`]'s, and it covers
/// `Season 0` and `S00` as well as `Specials` — see its doc for why the first
/// cut of it did not. **The two rules are not redundant**: this one keeps a
/// specials film that carries its own year wherever it is filed, and that one
/// keeps a specials film that carries no year — `Polar Special.mkv` — when the
/// library spells its specials folder the Plex way.
///
/// ## What this does **not** do
///
/// It does not give the file a title, a season or an episode number. A file that
/// flips to `episode` here searches under whatever title its basename carries —
/// the *episode* title, for the population this serves — which is a wrong show
/// rather than a wrong library. Better, and not right. Reaching right needs the
/// folder's title and season too, which is a signature change to the parser's
/// two production call sites, not this rule.
///
/// [`is_numbered_season_directory`]: nightjar_db::is_numbered_season_directory
pub fn stored_kind(
    parsed: MediaKind,
    parsed_year: Option<i32>,
    stored: &str,
    library_root: &str,
) -> &'static str {
    if parsed == MediaKind::Movie
        && parsed_year.is_none()
        && under_numbered_season_directory(stored, library_root)
    {
        return MediaKind::Episode.as_str();
    }
    parsed.as_str()
}

/// ADR-0030 §3: refuse repoint if matched/current < this fraction.
///
/// **REASONED, not MEASURED.** ADR-0030 §3 says so in its own words: "a
/// **default judgement**, not a measured floor. It was **not** run against the
/// ~24 800-item dogfood library before acceptance; it was picked to catch
/// wrong roots while allowing small tree churn." The ADR also records the
/// revisit trigger: dogfood remount evidence that keep-relpath remounts
/// routinely land under 0.90 without being a wrong root, or that 0.90 still
/// admits destructive mis-points.
pub const REPOINT_RETAIN_FRACTION: f64 = 0.90;

/// After a repoint with deferred_remove > 0, poll skips full walks for this
/// long so the operator can review before delete_missing runs (ADR-0030).
///
/// **GUESS — the hour itself has no stated origin.** ADR-0030 (amended
/// 2026-08-04) supplies the mechanism — poll must not apply the deferred
/// deletes before review, while manual scan stays allowed — and names the
/// duration only as a "default **1 hour**". Nothing in ADR-0030, this file's
/// git history, or any measurement derives why an hour rather than another
/// review window. The value that gates a destructive `delete_missing` is
/// unsourced, never derived.
pub const REPOINT_DELETE_HOLDOFF: Duration = Duration::from_secs(3600);

/// Flush size of the index upsert: the walk collects changed and new files
/// and commits them as one transaction every `INDEX_BATCH` rows (plus a final
/// partial flush; `upsert_items_indexed`). It is the commit granularity,
/// which is what makes the scan counter move in bursts: the visible
/// `added`/`updated` progress advances in 200-row steps, not continuously.
///
/// The reach is wider than the counter. Every flush also enqueues that
/// batch's probes and runs their sidecar association, and the transaction is
/// the batch-sized hold on the shared `Db` connection that concurrent API
/// reads wait out during a cold walk (only the metadata drain holds its own
/// connection — ADR-0026 §8).
///
/// **GUESS (Rule 4.14).** No derivation is recorded. It shipped bare with
/// the Phase 1 scanner (#1); no ADR, git history entry, or note derives why
/// 200. It is load-bearing for annotated constants elsewhere: the
/// `scan_progress` route's own doc and the library page's progress poll
/// (`web/src/routes/libraries/[id]/+page.svelte`) both justify their poll
/// cadence with "commits 200 rows at a time", so two annotated comments rest
/// on this unannotated server constant.
const INDEX_BATCH: usize = 200;

/// Who asked for a full-library walk (ADR-0015). Notify creates use
/// [`hint_ingest`] alone and do not go through this entry.
///
/// Every trigger walks fresh. The trigger shapes scheduling and coalescing
/// only; it no longer selects a cached walk. Poll and manual both observe the
/// tree as it is now (CHK-FC), so a trigger must not be the reason a directory
/// is skipped.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ScanTrigger {
    /// Periodic poll. While a scan is already active, no-op on the dirty bit
    /// so long walks can still run `delete_missing`.
    Poll,
    /// `POST .../scan` (and tests that mean manual discovery). Coalesces to one
    /// follow-up if a job is already active.
    Manual,
    /// Library create. Same coalesce as Manual if somehow concurrent.
    Create,
    /// Internal follow-up after a manual dirty bit.
    FollowUp,
}

/// Request a full-library scan (ADR-0015). Entry for poll, manual scan, library
/// create, and internal follow-up — not for notify creates ([`hint_ingest`]).
///
/// Returns the active or newly accepted job id. Returns `0` when a poll meets an
/// active repoint-delete holdoff: nothing is inserted and no worker starts
/// (ADR-0059).
pub fn request_scan(
    db: Arc<Db>,
    pool: Arc<LibraryPool>,
    library_id: i64,
    trigger: ScanTrigger,
) -> Result<i64, String> {
    let lib = db
        .get_library(library_id)?
        .ok_or_else(|| format!("library {library_id} not found"))?;
    match check_root(Path::new(&lib.path)) {
        Reachability::Unreachable => {
            let _ = pool.set_library_reachability(library_id, &lib.path, false);
            return Err(format!("library path is not reachable: {}", lib.path));
        }
        // Reachable, or the check instrument itself failed (Rule 4.15: not a
        // finding — do not refuse the scan on it).
        Reachability::Reachable | Reachability::CheckFailed => {}
    }
    // ADR-0059: the active-row lookup and any insert are one BEGIN IMMEDIATE
    // transaction. A poll's holdoff check runs inside it, after the lookup and
    // immediately before a possible insert, so a holdoff observed after the
    // lookup never leaves an inserted row. The check closes over the pool's
    // synchronized holdoff state, so it reads the state at admission time
    // rather than a snapshot taken before it.
    let holdoff_check = match trigger {
        ScanTrigger::Poll => {
            let pool = Arc::clone(&pool);
            Some(move || pool.repoint_delete_holdoff_active(library_id))
        }
        _ => None,
    };
    let job_id = match db.admit_scan_job(library_id, holdoff_check)? {
        ScanAdmission::Existing(existing) => {
            match trigger {
                // Running walk is this poll; do not suppress delete_missing.
                ScanTrigger::Poll | ScanTrigger::FollowUp => {}
                ScanTrigger::Manual | ScanTrigger::Create => {
                    pool.mark_scan_dirty(library_id);
                }
            }
            return Ok(existing);
        }
        ScanAdmission::Skipped => {
            tracing::info!(
                library_id,
                "poll skipped; repoint deferred_remove holdoff active"
            );
            return Ok(0);
        }
        ScanAdmission::Created(job_id) => job_id,
    };
    let db_worker = Arc::clone(&db);
    let pool_worker = Arc::clone(&pool);
    spawn_job_worker(&db, job_id, "scan", None, move || {
        let outcome = match run_scan_job(&db_worker, &pool_worker, job_id, library_id) {
            Ok(probe_duration_ms) => Some(probe_duration_ms),
            Err(e) => {
                tracing::error!(job_id, library_id, error = %e, "scan job failed");
                let _ = db_worker.fail_scan_job(job_id, &e);
                None
            }
        };

        // Every pool-state side effect this job owns runs *before* the row goes
        // terminal. The row is the only thing an outside observer can wait on,
        // so a job that reads `completed` while its worker is still mutating
        // pool state is making a promise it does not keep. That window is what
        // made `repoint_holdoff_blocks_poll_not_manual` flaky on CI: a holdoff
        // armed after wait_job returned was wiped by this tail a moment later,
        // and the next poll started a job it should have skipped.
        //
        // Ordering matters within the tail too: take_scan_dirty must be
        // consumed here, before the row is terminal, so a trigger that arrives
        // during the scan is not lost.
        let _ = pool_worker.take_dirty_add(library_id);
        if outcome.is_some() {
            // Ordinary scan is the clear for deferred_remove holdoff.
            pool_worker.clear_repoint_delete_holdoff(library_id);
        }
        let dirty = pool_worker.take_scan_dirty(library_id);

        if let Some(probe_duration_ms) = outcome {
            complete_job(&db_worker, job_id, library_id, probe_duration_ms);
        }

        // The follow-up is the one thing that cannot move ahead of completion:
        // request_scan refuses to start a job while this one is still active
        // (one job per library), so asking earlier would return this job's id
        // and silently drop the follow-up. A new job appearing after this one
        // goes terminal is correct -- it is a different job, not this job's
        // unfinished business.
        if dirty {
            tracing::info!(
                library_id,
                "library dirty after scan; starting follow-up job"
            );
            if let Err(e) = request_scan(
                Arc::clone(&db_worker),
                Arc::clone(&pool_worker),
                library_id,
                ScanTrigger::FollowUp,
            ) {
                tracing::warn!(library_id, error = %e, "follow-up scan failed");
            }
        }
    })?;
    Ok(job_id)
}

/// Alias for manual / test discovery starts.
pub fn start_scan_job(db: Arc<Db>, pool: Arc<LibraryPool>, library_id: i64) -> Result<i64, String> {
    request_scan(db, pool, library_id, ScanTrigger::Manual)
}

/// Path-hinted notify ingest (ADR-0015). Upserts one media file immediately so a
/// new episode can appear without waiting for the full walk. Never calls
/// `delete_missing` — poll (or manual scan) remains the heal/delete path.
///
/// Skips non-media, missing, non-file, and zero-size paths (copy-in-progress /
/// debounce miss). Does not take the index epoch: concurrent with an in-flight
/// walk is intentional. Does **not** call [`request_scan`]; callers must not
/// force a full walk after a successful hint.
pub fn hint_ingest(
    db: &Db,
    pool: &LibraryPool,
    library_id: i64,
    path: &Path,
) -> Result<HintIngestOutcome, String> {
    if !is_media(path) {
        return Ok(HintIngestOutcome::Ignored);
    }
    let meta = match std::fs::metadata(path) {
        Ok(m) if m.is_file() => m,
        Ok(_) => return Ok(HintIngestOutcome::Ignored),
        Err(_) => return Ok(HintIngestOutcome::Ignored),
    };
    let size_bytes = meta.len() as i64;
    if size_bytes <= 0 {
        return Ok(HintIngestOutcome::Ignored);
    }
    let mtime_ms = meta
        .modified()
        .ok()
        .and_then(|t| t.duration_since(std::time::UNIX_EPOCH).ok())
        .map(|d| d.as_millis() as i64)
        .unwrap_or(0);

    let lib = db
        .get_library(library_id)?
        .ok_or_else(|| format!("library {library_id} not found"))?;
    if !pool.is_library_reachable(library_id) {
        return Ok(HintIngestOutcome::Ignored);
    }
    let library_root = std::fs::canonicalize(&lib.path)
        .map(|p| nightjar_db::normalize_library_root(&p.to_string_lossy()))
        .unwrap_or_else(|_| nightjar_db::normalize_library_root(&lib.path));
    let resolved = std::fs::canonicalize(path).unwrap_or_else(|_| path.to_path_buf());
    let Some(rel) = to_relpath(&library_root, &resolved) else {
        return Ok(HintIngestOutcome::Ignored);
    };

    let folded = fold_path(&rel);
    let matches: Vec<ItemPathRow> = db
        .list_item_paths(library_id)?
        .into_iter()
        .filter(|r| fold_path(&r.path) == folded)
        .collect();

    if matches.len() > 1 {
        tracing::warn!(
            library_id,
            path = %rel,
            count = matches.len(),
            "hint ingest: fold-equal path collision; refusing upsert"
        );
        return Ok(HintIngestOutcome::Collision);
    }

    if let Some(row) = matches.first()
        && row.mtime_ms == mtime_ms
        && row.size_bytes == size_bytes
    {
        if row.probe_status == "indexed" {
            let abs = resolve_media_path(&library_root, &row.path);
            pool.enqueue(pool::WorkItem::probe(row.id, library_id, abs, None));
        }
        return Ok(HintIngestOutcome::Unchanged { item_id: row.id });
    }

    let store_path = matches
        .first()
        .map(|r| r.path.clone())
        .unwrap_or_else(|| rel.clone());
    let content_id = match nightjar_db::content_id_for_path(path) {
        Ok(id) => Some(id),
        Err(e) => {
            tracing::warn!(
                path = %path.display(),
                error = %e,
                "content_id read failed; map rebuild will retry"
            );
            None
        }
    };
    let parsed = stored_parse(&store_path, &library_root);
    let item = UpsertItem {
        path: store_path.clone(),
        mtime_ms,
        size_bytes,
        title: parsed.title,
        kind: parsed.kind.as_str().to_string(),
        year: parsed.year,
        season: parsed.season,
        episode: parsed.episode,
        content_id,
    };
    let ids = {
        // If a full walk is in flight, mark dirty_add so that job skips
        // delete_missing (would otherwise drop this row). The marker and the
        // upsert are one critical section with the walk's marker read and
        // delete_missing, so the walk either sees the marker or reads its
        // delete candidates before this row exists. Poll heals deletes later;
        // do not schedule a follow-up full walk for the hint alone.
        let mut guard = pool.dirty_add_guard(library_id);
        if db.active_scan_job(library_id)?.is_some() {
            guard.mark();
        }
        db.upsert_items_indexed(library_id, &[item])?
    };
    let item_id = ids
        .into_iter()
        .next()
        .ok_or_else(|| "hint upsert returned no id".to_string())?;
    let abs = resolve_media_path(&library_root, &store_path);
    pool.enqueue(pool::WorkItem::probe(
        item_id,
        library_id,
        abs.clone(),
        None,
    ));
    let mut sidecar_dirs = nightjar_transcode::SidecarDirCache::default();
    // Sidecar association stays index-time; extraction is no longer enqueued
    // at scan (ADR-0041 Decision 10 — the probe classifies this item and the
    // on-demand path in ADR-0013 §11 / ADR-0041 Decision 5 triggers extracts).
    match associate_sidecars(db, item_id, &library_root, &abs, &mut sidecar_dirs) {
        Ok((_, skipped)) if skipped > 0 => {
            // No scan job here, so the rejection lands on the library's visible
            // counter instead of a job counter (ADR-0030 §1).
            let current = db
                .get_library(library_id)?
                .map(|l| l.skipped_outside_root)
                .unwrap_or(0);
            let _ =
                db.set_library_path_counters(library_id, lib.paths_unresolved, current + skipped);
        }
        Ok(_) => {}
        Err(e) => tracing::warn!(
            item_id,
            path = %abs.display(),
            error = %e,
            "hint sidecar association failed"
        ),
    }
    tracing::info!(
        library_id,
        item_id,
        path = %rel,
        "hint ingest upserted media file"
    );
    Ok(HintIngestOutcome::Upserted { item_id })
}

/// Result of [`hint_ingest`].
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum HintIngestOutcome {
    Ignored,
    Unchanged { item_id: i64 },
    Upserted { item_id: i64 },
    Collision,
}

/// Async library repoint (ADR-0030 §3). Returns a job id immediately; dry-run
/// walk + commit run on a worker thread.
pub fn request_repoint(
    db: Arc<Db>,
    pool: Arc<LibraryPool>,
    library_id: i64,
    candidate_path: &str,
) -> Result<i64, String> {
    let _ = db
        .get_library(library_id)?
        .ok_or_else(|| format!("library {library_id} not found"))?;
    if let Some(existing) = db.active_scan_job(library_id)? {
        return Err(format!(
            "library {library_id} already has active job {existing}"
        ));
    }
    let job_id = db.create_repoint_job(library_id, candidate_path)?;
    let candidate = candidate_path.to_string();
    let db_worker = Arc::clone(&db);
    let pool_worker = Arc::clone(&pool);
    spawn_job_worker(&db, job_id, "repoint", None, move || {
        match run_repoint_job(&db_worker, &pool_worker, job_id, library_id, &candidate) {
            // Repoint owns no pool-state tail; it arms the holdoff inside its
            // own index pass and never clears it.
            Ok(probe_duration_ms) => {
                complete_job(&db_worker, job_id, library_id, probe_duration_ms)
            }
            Err(e) => {
                tracing::error!(job_id, library_id, error = %e, "repoint job failed");
                let _ = db_worker.fail_scan_job(job_id, &e);
            }
        }
    })?;
    Ok(job_id)
}

/// Spawn the worker thread for a scan/repoint job whose `queued` row was
/// already inserted by the caller. A failed spawn fails that row immediately
/// (Rule 4.8): leaving it `queued` would wedge the library until the next
/// process start runs `fail_stale_scan_jobs`. `kind` is `scan` or `repoint`
/// and shapes both the thread name and the error message. `stack_size` is
/// `None` in production; tests pass an oversized value to force a
/// deterministic spawn failure.
fn spawn_job_worker(
    db: &Db,
    job_id: i64,
    kind: &str,
    stack_size: Option<usize>,
    worker: impl FnOnce() + Send + 'static,
) -> Result<(), String> {
    let mut builder = std::thread::Builder::new().name(format!("{kind}-job-{job_id}"));
    if let Some(size) = stack_size {
        builder = builder.stack_size(size);
    }
    match builder.spawn(worker) {
        Ok(_) => Ok(()),
        Err(e) => {
            let msg = format!("spawn {kind} job {job_id}: {e}");
            let _ = db.fail_scan_job(job_id, &msg);
            Err(msg)
        }
    }
}

fn run_repoint_job(
    db: &Arc<Db>,
    pool: &Arc<LibraryPool>,
    job_id: i64,
    library_id: i64,
    candidate_path: &str,
) -> Result<u64, String> {
    db.set_scan_job_state(job_id, "indexing")?;
    // Probes enqueue as the walk discovers them, so the barrier opens before
    // the epoch and closes in `finish_scan_probes` (ADR-0004 §2.4).
    let probes = pool.start_probe_batch();
    {
        // One epoch for dry-run walk + commit index so another library cannot
        // interleave a cold walk on the same share (ADR-0015).
        let _epoch = pool.enter_index_epoch(library_id);
        let candidate = std::fs::canonicalize(candidate_path)
            .map(|p| nightjar_db::normalize_library_root(&p.to_string_lossy()))
            .unwrap_or_else(|_| nightjar_db::normalize_library_root(candidate_path));
        let root = Path::new(&candidate);
        match check_root(root) {
            Reachability::Reachable => {}
            Reachability::Unreachable => {
                return Err(format!("repoint path is not reachable: {candidate}"));
            }
            // Check instrument failure is not a finding about the path
            // (Rule 4.15); let the walk below report the truth.
            Reachability::CheckFailed => {}
        }
        let current = db.count_items(library_id)?;
        let existing = db.list_item_paths(library_id)?;
        let existing_folds: HashSet<String> = existing
            .iter()
            .filter(|r| !nightjar_db::is_absolute_stored(&r.path))
            .map(|r| fold_path(&r.path))
            .collect();

        // Single cold walk: retain math + commit index reuse the same file list
        // (ADR-0030). Seed WalkCache under the new absolute root for the next poll.
        let mut dry_cache = walk::WalkCache::new();
        let outcome = walk::walk_media_files_cached(root, Some(&mut dry_cache))?;
        let mut walked_folds = HashSet::new();
        for file in &outcome.files {
            if let Some(rel) = to_relpath(&candidate, &file.path) {
                walked_folds.insert(fold_path(&rel));
            }
        }
        let matched = existing_folds
            .iter()
            .filter(|f| walked_folds.contains(*f))
            .count() as i64;
        let would_remove = current - matched;

        if current >= 1 && matched == 0 {
            return Err(format!(
                "repoint_empty_match: current={current} walked={} matched=0 would_remove={would_remove}",
                walked_folds.len()
            ));
        }
        if current > 0 {
            let retain = matched as f64 / current as f64;
            if retain < REPOINT_RETAIN_FRACTION {
                return Err(format!(
                    "repoint_below_retain_threshold: current={current} walked={} matched={matched} would_remove={would_remove} retain={retain:.3}",
                    walked_folds.len()
                ));
            }
        }

        db.update_library_path(library_id, &candidate)?;
        let _ = db.repair_library_paths(library_id)?;
        let _ = pool.set_library_reachability(library_id, &candidate, true);
        pool.replace_walk_cache(library_id, dry_cache);
        run_index_pass(db, pool, job_id, library_id, Some(outcome), &probes)?;
    }
    finish_scan_probes(pool, library_id, probes)
}

fn run_scan_job(
    db: &Arc<Db>,
    pool: &Arc<LibraryPool>,
    job_id: i64,
    library_id: i64,
) -> Result<u64, String> {
    db.set_scan_job_state(job_id, "indexing")?;
    run_index_and_probe(db, pool, job_id, library_id)
}

fn run_index_and_probe(
    db: &Arc<Db>,
    pool: &Arc<LibraryPool>,
    job_id: i64,
    library_id: i64,
) -> Result<u64, String> {
    let probes = pool.start_probe_batch();
    {
        let _epoch = pool.enter_index_epoch(library_id);
        run_index_pass(db, pool, job_id, library_id, None, &probes)?;
    }
    finish_scan_probes(pool, library_id, probes)
}

/// Wait out this job's probes and report how long probing took. Does **not**
/// mark the job terminal -- see [`complete_job`].
fn finish_scan_probes(
    pool: &Arc<LibraryPool>,
    library_id: i64,
    probes: pool::ProbeBatch,
) -> Result<u64, String> {
    if !pool.is_library_reachable(library_id) {
        return Ok(0);
    }

    // Measured from the first push, not from here, so it still means "how long
    // probing took" now that probing overlaps the walk. It therefore overlaps
    // `index_duration_ms` and the two no longer sum to job wall time. It has
    // also never covered the fs-notify or `drain_pending_probes` paths, which
    // enqueue outside any batch.
    let probe_duration = probes.wait();
    // No scan-time subtitle extract enqueue: the probe already classified each
    // item (ADR-0041 Decision 2), and extraction is triggered on demand only
    // (ADR-0041 Decision 10, deleting the ADR-0013 §1 scan-time enqueue).
    // No scan-time keyframe-map enqueue either (ADR-0023 §2/§9 amendment):
    // the map builds when a consumer asks — playbackInfo, session create, or
    // a seek's bounded wait — never as a whole-library consequence of scanning.
    Ok(probe_duration.as_millis() as u64)
}

/// Mark a job terminal. Call this **after** every side effect the job owns, so
/// a caller that observes `completed` sees finished state (see
/// [`request_scan`]'s worker).
fn complete_job(db: &Db, job_id: i64, library_id: i64, probe_duration_ms: u64) {
    if let Err(e) = db.complete_scan_job(job_id, probe_duration_ms) {
        tracing::error!(job_id, library_id, error = %e, "complete scan job");
        return;
    }
    tracing::info!(job_id, library_id, probe_duration_ms, "scan job completed");
}

/// Walk + upsert only. Caller must hold [`LibraryPool::enter_index_epoch`].
///
/// When `prewalked` is `Some`, the file list is reused (repoint: same cold walk
/// as the retain dry-run). Caller must have reseeded WalkCache for the new root.
///
/// Otherwise the pass re-lists every directory through
/// [`walk::walk_media_files_fresh`]. Poll, manual, and create share that one
/// authoritative enumeration, so an unchanged parent mtime cannot hide a media
/// size change or an adjacent/nested sidecar add/edit/remove (CHK-FC). The
/// fresh listing replaces the shared walk cache; the cache is retained for
/// repoint seeding but no longer decides which directories a scan may skip.
/// Probes stay keyed to the observed `(mtime, size)` tuple, so unchanged media
/// enqueues none.
///
/// Probes are pushed into `probes` as they are discovered rather than returned
/// as a batch for the caller to enqueue afterwards (ADR-0004 §2.4). Returns how
/// many were pushed. Note that the readdir walk still runs to completion before
/// the upsert loop begins, so the first probe lands at readdir plus one
/// `INDEX_BATCH` flush — not at the start of the pass.
fn run_index_pass(
    db: &Arc<Db>,
    pool: &Arc<LibraryPool>,
    job_id: i64,
    library_id: i64,
    prewalked: Option<walk::WalkOutcome>,
    probes: &pool::ProbeBatch,
) -> Result<usize, String> {
    let lib = db
        .get_library(library_id)?
        .ok_or_else(|| format!("library {library_id} not found"))?;
    // Canonical root for under-root checks (macOS /var vs /private/var).
    let library_root = std::fs::canonicalize(&lib.path)
        .map(|p| nightjar_db::normalize_library_root(&p.to_string_lossy()))
        .unwrap_or_else(|_| nightjar_db::normalize_library_root(&lib.path));
    let root = Path::new(&library_root);
    let root_before = check_root(root);
    match root_before {
        Reachability::Reachable => {}
        Reachability::Unreachable => {
            let _ = pool.set_library_reachability(library_id, &library_root, false);
            return Err(format!("library path is not reachable: {library_root}"));
        }
        // Check instrument failure is not a finding (Rule 4.15): do not pause
        // or abort the scan on it.
        Reachability::CheckFailed => {}
    }

    // Scan library (and poll) re-try availability failures; permanent error stays
    // until mtime change (ADR-0014). Must run before list_item_paths so the
    // unchanged branch sees probe_status=indexed and re-queues probes.
    let (rq_probes, rq_extracts, rq_maps) = db.requeue_unavailable_for_library(library_id)?;
    if rq_probes > 0 || rq_extracts > 0 || rq_maps > 0 {
        tracing::info!(
            library_id,
            probes = rq_probes,
            extracts = rq_extracts,
            maps = rq_maps,
            "scan re-queued availability failures"
        );
    }

    let existing_count = db.count_items(library_id)?;
    let index_started = Instant::now();
    // Caller holds IndexEpochGuard for this walk/upsert (ADR-0013/0015).
    #[allow(clippy::type_complexity)]
    let index_result = (|| -> Result<(u32, u32, u32, u32, usize, u64), String> {
        let reused = prewalked.is_some();
        let walk_started = Instant::now();
        let outcome = if let Some(outcome) = prewalked {
            // Repoint reseeded cache from the dry-run; reuse that cold walk.
            outcome
        } else {
            // One authoritative fresh enumeration for poll, manual, and create:
            // re-list every directory so an unchanged parent mtime cannot hide
            // a media or sidecar change, then keep the fresh listing in the
            // shared cache. Sidecar reconciliation below keys off
            // `relisted_dirs`, which a fresh walk fills with every directory.
            //
            // The cancellation check is the ADR-0014 §2 reachability pause: the
            // walk stops at the next directory boundary when the library goes
            // unreachable, so a dead mount cannot make the scan keep reading it
            // (CHK-WC). The handle is owned so the parallel walk can share it.
            let availability = Arc::clone(&pool.availability);
            pool.with_walk_cache(library_id, |cache| {
                walk::walk_media_files_fresh(
                    root,
                    cache,
                    Arc::new(move || availability.pause.is_paused(library_id)),
                )
            })?
        };
        // A cancelled walk is a partial listing. The keep-set is incomplete, so
        // `delete_missing` must not run (ADR-0014 §2), and the upsert/sidecar
        // block is abandoned with it: the catalog rows stay as they were. The
        // caller's worker reports this error through `fail_scan_job` and its
        // own `tracing::error!`.
        if outcome.cancelled {
            tracing::warn!(
                library_id,
                job_id,
                "walk cancelled; index pass abandoned, catalog left intact"
            );
            return Err(format!(
                "walk cancelled: library {library_id} paused mid-walk"
            ));
        }
        // `index_duration_ms` covers readdir and upsert together, which is why
        // no run so far can say which of the two the cold-scan minutes were
        // spent in. Split here; the scan-job row and the Gate 1 harness keep
        // reading `index_duration_ms` unchanged.
        let walk_ms = walk_started.elapsed().as_millis() as u64;
        if reused {
            tracing::info!(
                library_id,
                job_id,
                files = outcome.files.len(),
                "index reusing repoint dry-run walk (no second readdir)"
            );
        }
        let files = outcome.files;
        let relisted_dirs = outcome.relisted_dirs;
        let listing_errors = outcome.listing_errors;
        let mut added = 0u32;
        let mut updated = 0u32;
        let mut unchanged = 0u32;
        let mut skipped_outside_root = 0i64;
        let mut fold_collisions = 0i64;
        let mut keep_folds: HashSet<String> = HashSet::with_capacity(files.len());
        let mut pending_upserts: Vec<UpsertItem> = Vec::with_capacity(INDEX_BATCH);
        let mut pending_were_existing: Vec<bool> = Vec::with_capacity(INDEX_BATCH);
        let mut to_probe = 0usize;
        // One listing per parent for the whole index job (flat 10k dirs).
        let mut sidecar_dirs = nightjar_transcode::SidecarDirCache::default();

        let mut by_fold: HashMap<String, Vec<ItemPathRow>> = HashMap::new();
        for row in db.list_item_paths(library_id)? {
            by_fold.entry(fold_path(&row.path)).or_default().push(row);
        }

        #[allow(clippy::too_many_arguments)]
        let flush = |db: &Db,
                     library_id: i64,
                     library_root: &str,
                     pending: &mut Vec<UpsertItem>,
                     were_existing: &mut Vec<bool>,
                     to_probe: &mut usize,
                     added: &mut u32,
                     updated: &mut u32,
                     sidecar_dirs: &mut nightjar_transcode::SidecarDirCache,
                     skipped_outside_root: &mut i64|
         -> Result<(), String> {
            if pending.is_empty() {
                return Ok(());
            }
            let abs_paths: Vec<PathBuf> = pending
                .iter()
                .map(|p| resolve_media_path(library_root, &p.path))
                .collect();
            let ids = db.upsert_items_indexed(library_id, pending)?;
            for (i, id) in ids.into_iter().enumerate() {
                if were_existing[i] {
                    *updated += 1;
                } else {
                    *added += 1;
                }
                pool.enqueue_probe_in_batch(
                    pool::WorkItem::probe(id, library_id, abs_paths[i].clone(), Some(job_id)),
                    probes,
                );
                *to_probe += 1;
                match associate_sidecars(db, id, library_root, &abs_paths[i], sidecar_dirs) {
                    Ok((_, skipped)) => *skipped_outside_root += skipped,
                    Err(e) => tracing::warn!(
                        item_id = id,
                        path = %abs_paths[i].display(),
                        error = %e,
                        "sidecar association failed"
                    ),
                }
            }
            pending.clear();
            were_existing.clear();
            Ok(())
        };

        for file in &files {
            // Resolve symlinks before the under-root check (ADR-0030 §1).
            // Walk paths stay under the walked root as strings; the inode can
            // still escape via symlink / bind-mount. Fail closed to skip.
            let resolved = std::fs::canonicalize(&file.path).unwrap_or_else(|_| file.path.clone());
            let Some(rel) = to_relpath(&library_root, &resolved) else {
                skipped_outside_root += 1;
                continue;
            };
            let folded = fold_path(&rel);
            keep_folds.insert(folded.clone());

            let matched = by_fold.get(&folded).cloned();
            match matched.as_deref() {
                Some(rows) if rows.len() > 1 => {
                    fold_collisions += 1;
                    tracing::warn!(
                        library_id,
                        path = %rel,
                        count = rows.len(),
                        "fold-equal path collision; refusing upsert"
                    );
                    continue;
                }
                Some([row])
                    if row.mtime_ms == file.mtime_ms && row.size_bytes == file.size_bytes =>
                {
                    unchanged += 1;
                    if row.probe_status == "indexed" {
                        let abs = resolve_media_path(&library_root, &row.path);
                        pool.enqueue_probe_in_batch(
                            pool::WorkItem::probe(row.id, library_id, abs, Some(job_id)),
                            probes,
                        );
                        to_probe += 1;
                    }
                }
                other => {
                    // Sticky spelling: keep existing path on fold match.
                    let store_path = match other {
                        Some([row]) => row.path.clone(),
                        _ => rel.clone(),
                    };
                    let were_existing = matches!(other, Some([_]));
                    let content_id = match nightjar_db::content_id_for_path(&file.path) {
                        Ok(id) => Some(id),
                        Err(e) => {
                            tracing::warn!(
                                path = %file.path.display(),
                                error = %e,
                                "content_id read failed; map rebuild will retry"
                            );
                            None
                        }
                    };
                    let parsed = stored_parse(&store_path, &library_root);
                    pending_upserts.push(UpsertItem {
                        path: store_path.clone(),
                        mtime_ms: file.mtime_ms,
                        size_bytes: file.size_bytes,
                        title: parsed.title,
                        kind: parsed.kind.as_str().to_string(),
                        year: parsed.year,
                        season: parsed.season,
                        episode: parsed.episode,
                        content_id,
                    });
                    pending_were_existing.push(were_existing);
                    if !were_existing {
                        by_fold.insert(
                            folded,
                            vec![ItemPathRow {
                                id: 0,
                                path: store_path,
                                mtime_ms: file.mtime_ms,
                                size_bytes: file.size_bytes,
                                probe_status: "indexed".into(),
                            }],
                        );
                    }
                    if pending_upserts.len() >= INDEX_BATCH {
                        flush(
                            db,
                            library_id,
                            &library_root,
                            &mut pending_upserts,
                            &mut pending_were_existing,
                            &mut to_probe,
                            &mut added,
                            &mut updated,
                            &mut sidecar_dirs,
                            &mut skipped_outside_root,
                        )?;
                    }
                }
            }
        }

        flush(
            db,
            library_id,
            &library_root,
            &mut pending_upserts,
            &mut pending_were_existing,
            &mut to_probe,
            &mut added,
            &mut updated,
            &mut sidecar_dirs,
            &mut skipped_outside_root,
        )?;

        let _ = fold_collisions;
        let root_after = check_root(root);
        let root_ok_after = matches!(root_after, Reachability::Reachable);
        // Pause only on a positive finding. A check instrument failure is not
        // a finding (Rule 4.15): the walk still counts as doubtful below
        // (root_ok_after false skips delete_missing) but must not pause a
        // healthy library.
        if matches!(root_after, Reachability::Unreachable) {
            let _ = pool.set_library_reachability(library_id, &lib.path, false);
        }
        // First index after a successful repoint: report unmatched rows but do
        // not delete_missing (ADR-0030). Next ordinary scan deletes.
        // dirty_add (path-hint mid-walk): skip delete so keep-set cannot drop
        // the hinted row. Poll-while-active does not set dirty — long walks may
        // still delete (ADR-0015). Manual dirty also skips until follow-up.
        let job_kind = db
            .get_scan_job(job_id)?
            .map(|j| j.kind)
            .unwrap_or_else(|| "scan".into());
        let defer_repoint = job_kind == "repoint";
        #[cfg(test)]
        pool.hold_delete_if_armed(library_id);
        // The marker read and the delete it authorizes are one critical section
        // (ADR-0014 §2): a hint that upserts while this pass is active either
        // marks before this read — and the delete is skipped — or upserts after
        // `delete_missing_fold` read its candidates, and its row is not among
        // them. The pass cannot drop a row a hint added mid-scan.
        let mut dirty_add_guard = pool.dirty_add_guard(library_id);
        let dirty_add = dirty_add_guard.take();
        let manual_dirty = pool.is_scan_dirty(library_id);
        let skip_delete_hint_or_manual = dirty_add || manual_dirty;
        let allow_delete = !defer_repoint
            && !skip_delete_hint_or_manual
            && allow_delete_missing(
                true,
                root_ok_after,
                listing_errors,
                keep_folds.is_empty(),
                existing_count,
            );
        let deferred_remove = if defer_repoint {
            db.count_missing_fold(library_id, &keep_folds)?
        } else {
            0
        };
        let (removed, deleted_ids) = if allow_delete {
            let deleted_ids = db.delete_missing_fold(library_id, &keep_folds)?;
            (deleted_ids.len() as u32, deleted_ids)
        } else {
            if defer_repoint {
                tracing::info!(
                    library_id,
                    job_id,
                    deferred_remove,
                    "repoint index: deferring delete_missing until next scan"
                );
            } else if dirty_add {
                tracing::info!(
                    library_id,
                    job_id,
                    "skipping delete_missing; hint dirt during scan (next poll heals deletes)"
                );
            } else if manual_dirty {
                tracing::info!(
                    library_id,
                    job_id,
                    "skipping delete_missing; manual rescan pending follow-up"
                );
            } else {
                tracing::warn!(
                    library_id,
                    listing_errors,
                    existing_count,
                    files = keep_folds.len(),
                    root_ok_after,
                    "skipping delete_missing; reachability in doubt"
                );
            }
            (0, Vec::new())
        };
        // The critical section ends with the delete itself: a hint may mark and
        // upsert from here on, and its row is no longer a delete candidate.
        drop(dirty_add_guard);
        for item_id in &deleted_ids {
            if let Err(e) = pool.remove_item_subtitles(*item_id) {
                tracing::warn!(item_id, error = %e, "remove deleted subtitle directory failed");
            }
        }
        let _ = db.set_scan_job_deferred_remove(job_id, deferred_remove);
        if defer_repoint && deferred_remove > 0 {
            pool.set_repoint_delete_holdoff(library_id, REPOINT_DELETE_HOLDOFF);
            tracing::info!(
                library_id,
                job_id,
                deferred_remove,
                holdoff_s = REPOINT_DELETE_HOLDOFF.as_secs(),
                "repoint deferred_remove holdoff armed; poll will skip until clear or expiry"
            );
        }
        if let Err(e) = pool.cleanup_orphan_subtitles() {
            tracing::warn!(error = %e, "subtitle orphan cleanup failed");
        }

        // Rediscover sidecars beside media whose parent the walk re-listed.
        // Every full pass now walks fresh, so `relisted_dirs` holds each
        // directory and this path reconciles supported sidecars for unchanged
        // media too — adjacent and nested add/edit/remove included (CHK-FC,
        // ADR-0013 §3.5 amendment). Existing sidecar rows stay in the DB across
        // restarts, and the shared `sidecar_dirs` cache keeps siblings to one
        // listing: no per-item directory listing. Deletion doubt still gates
        // the pass, so an incomplete listing reconciles nothing.
        let mut sidecar_checked = 0u32;
        if allow_delete {
            for file in &files {
                let Some(parent) = file.path.parent() else {
                    continue;
                };
                if !relisted_dirs.contains(parent) {
                    continue;
                }
                let Some(rel) = to_relpath(&library_root, &file.path) else {
                    continue;
                };
                let folded = fold_path(&rel);
                let Some(rows) = by_fold.get(&folded) else {
                    continue;
                };
                if rows.len() != 1 {
                    continue;
                }
                let row = &rows[0];
                if row.mtime_ms != file.mtime_ms || row.size_bytes != file.size_bytes {
                    continue;
                }
                let item_id = row.id;
                if item_id == 0 {
                    continue;
                }
                sidecar_checked += 1;
                // Sidecar rows stay fresh at index time; a re-probe of the item
                // (mtime change, operator pass) reclassifies it — no extract is
                // enqueued at scan (ADR-0041 Decision 10).
                match associate_sidecars(db, item_id, &library_root, &file.path, &mut sidecar_dirs)
                {
                    Ok((_, skipped)) => skipped_outside_root += skipped,
                    Err(e) => tracing::warn!(
                        item_id,
                        path = %file.path.display(),
                        error = %e,
                        "sidecar association failed"
                    ),
                }
            }
        }
        // Written after sidecar reconciliation: a rejected sidecar symlink
        // discovered for unchanged media lands here, and the visible counters
        // must include it (ADR-0030 §1).
        let _ = db.set_scan_job_skipped_outside_root(job_id, skipped_outside_root);
        let unresolved = db
            .get_library(library_id)?
            .map(|l| l.paths_unresolved)
            .unwrap_or(0);
        let _ = db.set_library_path_counters(library_id, unresolved, skipped_outside_root);

        let index_duration_ms = index_started.elapsed().as_millis() as u64;
        // Everything after the readdir: stat comparison, upsert batches,
        // sidecar association, delete_missing.
        let upsert_ms = index_duration_ms.saturating_sub(walk_ms);
        // `to_probe` and the batch counter increment at the same two sites; a
        // future push that updates one and not the other is the realistic
        // mistake this catches.
        debug_assert_eq!(
            to_probe,
            probes.pushed(),
            "index-pass probe count disagrees with the batch"
        );
        pool.record_index_duration_ms(index_duration_ms);
        db.set_scan_job_index_done(
            job_id,
            added,
            updated,
            removed,
            unchanged,
            index_duration_ms,
        )?;

        tracing::info!(
            job_id,
            library_id,
            added,
            updated,
            removed,
            unchanged,
            sidecar_checked,
            relisted_dirs = relisted_dirs.len(),
            to_probe,
            walk_ms,
            upsert_ms,
            index_duration_ms,
            "index pass done"
        );
        Ok((
            added,
            updated,
            removed,
            unchanged,
            to_probe,
            index_duration_ms,
        ))
    })();
    let (_added, _updated, _removed, _unchanged, to_probe, _index_duration_ms) = index_result?;
    Ok(to_probe)
}

/// True when `target` is component-wise beneath `root`, both canonical.
///
/// Component comparison, not a string prefix: `/tmp/Library/x` is not under
/// `/tmp/library`. `to_relpath`'s ASCII-case-folded fallback cannot tell those
/// apart, so confinement is decided here before any relative-path conversion
/// (ADR-0030 §1, §2).
fn is_beneath(root: &Path, target: &Path) -> bool {
    target != root && target.starts_with(root)
}

/// Reconcile the stored sidecar set for one media item against a successful
/// discovery (ADR-0010 §4). Returns the applied delta and the count of
/// discovered sidecars whose canonical target was not confined to the library
/// root.
///
/// Discovery reuses the caller's shared per-directory listing cache, so
/// siblings cost one listing. The bounded identity read is the only additional
/// filesystem work, and it runs only here, during explicit reconciliation —
/// never on playback or an unchanged-media probe. A sidecar whose identity
/// cannot be read fails the whole item: the prior set is preserved rather than
/// a row written without identity.
///
/// A sidecar symlink inside the library can target a file outside it. Each
/// discovered candidate is therefore canonicalized and required to be
/// component-wise beneath the canonical library root before `to_relpath` or
/// the identity read. A rejected candidate is dropped, never read, and counted
/// into the caller's visible `skipped_outside_root`. The discovered in-root
/// path is stored, not the resolved target, so the association keeps the name
/// the operator placed beside the media.
fn associate_sidecars(
    db: &Db,
    item_id: i64,
    library_root: &str,
    video_path: &Path,
    cache: &mut nightjar_transcode::SidecarDirCache,
) -> Result<(nightjar_db::SidecarDelta, i64), String> {
    let found = nightjar_transcode::discover_sidecars_cached(video_path, Some(cache))?;
    let canonical_root = std::fs::canonicalize(library_root).ok();
    let mut observed = Vec::with_capacity(found.len());
    let mut skipped_outside_root = 0i64;
    for s in found {
        let confined = match (canonical_root.as_deref(), std::fs::canonicalize(&s.path)) {
            (Some(root), Ok(target)) => is_beneath(root, &target),
            _ => false,
        };
        if !confined {
            skipped_outside_root += 1;
            continue;
        }
        let Some(path) = to_relpath(library_root, &s.path) else {
            skipped_outside_root += 1;
            continue;
        };
        let content_id = nightjar_db::content_id_for_path(&s.path)?;
        observed.push(nightjar_db::ObservedSidecar {
            track_id: s.track_id,
            path,
            mtime_ms: s.mtime_ms,
            size_bytes: s.size_bytes,
            format: s.format,
            language: s.language,
            forced: s.forced,
            sdh: s.sdh,
            content_id,
        });
    }
    let delta = db.reconcile_item_sidecars(item_id, &observed)?;
    if !delta.is_empty() {
        tracing::info!(
            item_id,
            added = delta.added.len(),
            changed = delta.changed.len(),
            removed = delta.removed.len(),
            "sidecar set reconciled"
        );
    }
    Ok((delta, skipped_outside_root))
}

pub fn version() -> &'static str {
    env!("CARGO_PKG_VERSION")
}

#[cfg(test)]
mod tests {
    use super::*;
    use nightjar_db::NewLibrary;
    use nightjar_transcode::SubsStore;
    use std::fs;
    use std::process::Command;

    fn test_pool(db: &Arc<Db>, data_dir: &Path) -> Arc<LibraryPool> {
        let subs = Arc::new(SubsStore::new(data_dir.join("subs")).unwrap());
        LibraryPool::spawn(Arc::clone(db), subs)
    }

    /// An `ObservedSidecar` whose mtime/size come from the file on disk, so the
    /// D2B.2 before/after recheck matches the captured tuple.
    fn observed_sidecar(
        abs: &Path,
        relpath: &str,
        track_id: &str,
        format: &str,
    ) -> nightjar_db::ObservedSidecar {
        let meta = fs::metadata(abs).unwrap();
        let mtime_ms = meta
            .modified()
            .ok()
            .and_then(|t| t.duration_since(std::time::SystemTime::UNIX_EPOCH).ok())
            .map(|d| d.as_millis() as i64)
            .unwrap_or(0);
        nightjar_db::ObservedSidecar {
            track_id: track_id.to_string(),
            path: relpath.to_string(),
            mtime_ms,
            size_bytes: meta.len() as i64,
            format: format.to_string(),
            language: Some("en".to_string()),
            forced: false,
            sdh: false,
            content_id: format!("{}-disk", meta.len()),
        }
    }

    /// Publish a certified probe snapshot (ADR-0058) so a D2B.2 standalone
    /// extract has a certified source. `subtitle_streams` are
    /// `(stream_index, codec)`; the snapshot classifies the item `eligible`.
    ///
    /// The item row's `mtime_ms`/`size_bytes` are aligned to the file on disk
    /// first, because D2B.2 rechecks the captured media identity before
    /// extraction and before final publication (ADR-0013 §13.3.2).
    fn certify_item(db: &Arc<Db>, item_id: i64, subtitle_streams: &[(i64, &str)]) {
        let row = db.get_item(item_id).unwrap().expect("item row");
        let lib = db.get_library(row.library_id).unwrap().expect("library");
        let abs = nightjar_db::resolve_media_path(&lib.path, &row.path);
        let (mtime_ms, size_bytes) = match fs::metadata(&abs) {
            Ok(meta) => {
                let mtime_ms = meta
                    .modified()
                    .ok()
                    .and_then(|t| t.duration_since(std::time::SystemTime::UNIX_EPOCH).ok())
                    .map(|d| d.as_millis() as i64)
                    .unwrap_or(0);
                (mtime_ms, meta.len() as i64)
            }
            // A fixture whose media file is absent on purpose keeps the row's
            // tuple; the extract then reports the access failure itself.
            Err(_) => (row.mtime_ms, row.size_bytes),
        };
        db.with_conn(|c| {
            c.execute(
                "UPDATE media_items SET mtime_ms = ?2, size_bytes = ?3 WHERE id = ?1",
                [item_id, mtime_ms, size_bytes],
            )
            .map(|_| ())
            .map_err(|e| e.to_string())
        })
        .unwrap();
        let row = db.get_item(item_id).unwrap().expect("item row");
        let content_id = format!("{}-{}-cert", row.size_bytes, row.mtime_ms);
        db.set_content_id(item_id, &content_id).unwrap();
        let expectation = nightjar_db::ProbeExpectation {
            item_id,
            library_id: row.library_id,
            library_root: lib.path.clone(),
            path: row.path.clone(),
            media_revision: row.media_revision,
            probe_revision: row.probe_revision,
            content_id: Some(content_id),
            mtime_ms: row.mtime_ms,
            size_bytes: row.size_bytes,
        };
        let subtitle_tracks = subtitle_streams
            .iter()
            .map(|(stream_index, codec)| nightjar_db::SubtitleTrackRow {
                media_item_id: item_id,
                stream_index: *stream_index,
                codec: (*codec).to_string(),
                language: None,
                title: None,
                forced: false,
                sdh: false,
                kind: nightjar_transcode::subtitle_codec_kind(codec)
                    .as_str()
                    .to_string(),
            })
            .collect();
        let snapshot = nightjar_db::ProbeSnapshot {
            duration_ms: Some(4000),
            container: Some("mkv".into()),
            video_codec: Some("h264".into()),
            video_stream_index: Some(0),
            audio_codec: Some("aac".into()),
            audio_channels: Some(2),
            width: Some(160),
            height: Some(120),
            video_bitrate_bps: None,
            video_frame_rate_num: Some(10),
            video_frame_rate_den: Some(1),
            hdr: None,
            audio_tracks: vec![],
            subtitle_tracks,
            subtitle_status: "eligible".into(),
        };
        let publication = db
            .publish_probe(
                &expectation,
                &nightjar_db::ProbeOutcome::Success(Box::new(snapshot)),
            )
            .unwrap();
        assert!(
            matches!(publication, nightjar_db::ProbePublication::Published { .. }),
            "test fixture probe must certify, got {publication:?}"
        );
    }

    /// The finalized artifact path of one track, resolved from the committed
    /// publication row exactly the way serving resolves it (ADR-0013 §13.2).
    fn committed_artifact_path(
        db: &Arc<Db>,
        store: &SubsStore,
        item_id: i64,
        track_id: &str,
    ) -> PathBuf {
        let source = db
            .certified_subtitle_source(item_id)
            .unwrap()
            .expect("certified source");
        let token = source.token_for_track(track_id).expect("member token");
        let artifact = source
            .artifact_for(track_id)
            .expect("committed publication reference");
        store.artifact_path(item_id, &token, track_id, artifact.artifact_revision)
    }

    /// `subtitle_attempt_count` straight from the row, so a test can prove a
    /// deferral consumed no failure backoff.
    fn subtitle_attempt_count(db: &Arc<Db>, item_id: i64) -> i64 {
        db.with_conn(|c| {
            c.query_row(
                "SELECT subtitle_attempt_count FROM media_items WHERE id = ?1",
                [item_id],
                |r| r.get::<_, i64>(0),
            )
            .map_err(|e| e.to_string())
        })
        .unwrap()
    }

    /// `(stream_index, codec)` pairs for the text subtitle streams of a file.
    fn text_streams(path: &Path) -> Vec<(i64, &'static str)> {
        nightjar_transcode::list_text_subtitles(path)
            .unwrap()
            .into_iter()
            .map(|s| (i64::from(s.stream_index), "subrip"))
            .collect()
    }

    #[test]
    fn index_pass_lists_before_probe_and_broken_moov_errors() {
        let dir = tempfile::tempdir().unwrap();
        let media = dir.path().join("media");
        fs::create_dir_all(&media).unwrap();

        // Tiny valid-ish mp4 via ffmpeg if available; otherwise skip probe-success path.
        let good = media.join("Good Movie (2020).mp4");
        let ffmpeg_ok = Command::new("ffmpeg")
            .args([
                "-y",
                "-hide_banner",
                "-loglevel",
                "error",
                "-f",
                "lavfi",
                "-i",
                "color=c=black:s=64x64:d=0.2",
                "-f",
                "lavfi",
                "-i",
                "anullsrc=r=48000:cl=stereo",
                "-c:v",
                "libx264",
                "-pix_fmt",
                "yuv420p",
                "-c:a",
                "aac",
                "-shortest",
                good.to_str().unwrap(),
            ])
            .status()
            .map(|s| s.success())
            .unwrap_or(false);

        let broken = media.join("broken_moov.mp4");
        fs::write(&broken, b"not a real mp4").unwrap();

        let db = Arc::new(nightjar_db::open(dir.path()).unwrap());
        let pool = test_pool(&db, dir.path());
        let lib = db
            .create_library(&NewLibrary {
                name: "t".into(),
                path: media.to_string_lossy().into_owned(),
                kind: "movies".into(),
            })
            .unwrap();

        let job_id = start_scan_job(Arc::clone(&db), Arc::clone(&pool), lib.id).unwrap();
        // Wait for completion.
        for _ in 0..200 {
            let job = db.get_scan_job(job_id).unwrap().unwrap();
            if job.state == "completed" || job.state == "failed" {
                assert_eq!(job.state, "completed");
                assert!(job.index_duration_ms.is_some());
                break;
            }
            std::thread::sleep(std::time::Duration::from_millis(50));
        }

        let items = db.list_items(lib.id).unwrap();
        assert!(!items.is_empty());
        let broken_item = items
            .iter()
            .find(|i| i.path.ends_with("broken_moov.mp4"))
            .expect("broken_moov indexed");
        assert_eq!(broken_item.probe_status, "error");
        assert!(broken_item.scan_error.is_some());

        if ffmpeg_ok {
            let good_item = items
                .iter()
                .find(|i| i.path.ends_with("Good Movie (2020).mp4"))
                .expect("good file");
            assert_eq!(good_item.probe_status, "probed");
            assert!(good_item.video_codec.is_some());
        }

        // Unchanged rescan: index fast, nothing to probe.
        let job2 = start_scan_job(Arc::clone(&db), Arc::clone(&pool), lib.id).unwrap();
        for _ in 0..200 {
            let job = db.get_scan_job(job2).unwrap().unwrap();
            if job.state == "completed" {
                assert!(job.unchanged >= 1);
                assert_eq!(job.probed, 0);
                break;
            }
            std::thread::sleep(std::time::Duration::from_millis(50));
        }
    }

    #[test]
    fn index_associates_sidecar_srt_not_as_media_item() {
        let dir = tempfile::tempdir().unwrap();
        let media = dir.path().join("media");
        fs::create_dir_all(media.join("Subs")).unwrap();
        let video = media.join("Movie.mp4");
        fs::write(&video, b"not a real mp4").unwrap();
        fs::write(
            media.join("Movie.en.srt"),
            b"1\n00:00:00,000 --> 00:00:01,000\nHi\n",
        )
        .unwrap();
        fs::write(
            media.join("Subs").join("Movie.en.srt"),
            b"1\n00:00:00,000 --> 00:00:01,000\nSubs\n",
        )
        .unwrap();

        let db = Arc::new(nightjar_db::open(dir.path()).unwrap());
        let pool = test_pool(&db, dir.path());
        let lib = db
            .create_library(&NewLibrary {
                name: "t".into(),
                path: media.to_string_lossy().into_owned(),
                kind: "movies".into(),
            })
            .unwrap();

        let job_id = start_scan_job(Arc::clone(&db), Arc::clone(&pool), lib.id).unwrap();
        for _ in 0..200 {
            let job = db.get_scan_job(job_id).unwrap().unwrap();
            if job.state == "completed" || job.state == "failed" {
                assert_eq!(job.state, "completed");
                break;
            }
            std::thread::sleep(std::time::Duration::from_millis(50));
        }

        let items = db.list_items(lib.id).unwrap();
        assert_eq!(items.len(), 1, "srt must not become media items: {items:?}");
        let sidecars = db.list_item_sidecars(items[0].id).unwrap();
        let ids: Vec<_> = sidecars.iter().map(|s| s.track_id.as_str()).collect();
        assert!(ids.contains(&"s-en"), "{ids:?}");
        assert!(ids.contains(&"s-Subs.en"), "{ids:?}");
    }

    /// Same path, size and restored mtime; only the bounded identity of the
    /// sidecar bytes differs. The reconciliation must still see a change and
    /// allocate a new generation.
    #[test]
    fn sidecar_bytes_changed_with_restored_mtime_and_size_receive_a_new_generation() {
        let tmp = tempfile::tempdir().unwrap();
        let dir = fs::canonicalize(tmp.path()).unwrap();
        let media = dir.join("media");
        fs::create_dir_all(&media).unwrap();
        let video = media.join("Movie.mp4");
        fs::write(&video, b"not a real mp4").unwrap();
        let srt = media.join("Movie.en.srt");
        fs::write(&srt, b"1\n00:00:00,000 --> 00:00:01,000\nHi\n").unwrap();
        let mtime = fs::metadata(&srt).unwrap().modified().unwrap();

        let db = nightjar_db::open(&dir).unwrap();
        let lib = db
            .create_library(&NewLibrary {
                name: "t".into(),
                path: media.to_string_lossy().into_owned(),
                kind: "movies".into(),
            })
            .unwrap();
        let item_id = db
            .upsert_items_indexed(
                lib.id,
                &[nightjar_db::UpsertItem {
                    path: "Movie.mp4".into(),
                    mtime_ms: 1,
                    size_bytes: 15,
                    title: "Movie".into(),
                    kind: "movie".into(),
                    year: None,
                    season: None,
                    episode: None,
                    content_id: None,
                }],
            )
            .unwrap()[0];
        let root = media.to_string_lossy().into_owned();

        let (first, _) = associate_sidecars(
            &db,
            item_id,
            &root,
            &video,
            &mut nightjar_transcode::SidecarDirCache::default(),
        )
        .unwrap();
        assert_eq!(first.added.len(), 1);
        let before = db.get_item_sidecar(item_id, "s-en").unwrap().unwrap();
        assert_eq!(before.sidecar_generation, Some(1));

        // Same length, different bytes, mtime restored to the stored value.
        fs::write(&srt, b"1\n00:00:00,000 --> 00:00:01,000\nYo\n").unwrap();
        fs::File::options()
            .write(true)
            .open(&srt)
            .unwrap()
            .set_modified(mtime)
            .unwrap();

        let (delta, _) = associate_sidecars(
            &db,
            item_id,
            &root,
            &video,
            &mut nightjar_transcode::SidecarDirCache::default(),
        )
        .unwrap();
        assert_eq!(delta.changed.len(), 1);
        assert_eq!(
            delta.changed[0].before.mtime_ms, delta.changed[0].after.mtime_ms,
            "the mtime is restored"
        );
        assert_eq!(
            delta.changed[0].before.size_bytes, delta.changed[0].after.size_bytes,
            "the size is unchanged"
        );
        assert_ne!(
            delta.changed[0].before.content_id, delta.changed[0].after.content_id,
            "the bounded identity is the only signal that changed"
        );
        assert_eq!(delta.changed[0].after.sidecar_generation, Some(2));
    }

    /// ADR-0013 §3 amendment: the cold-cache skip is the automatic-poll rule
    /// only. A manual scan after restart reconciles supported sidecars for
    /// unchanged media, and an unchanged sidecar keeps its row and generation.
    #[test]
    fn manual_scan_after_restart_reconciles_sidecars_for_unchanged_media() {
        let dir = tempfile::tempdir().unwrap();
        let media = dir.path().join("media");
        fs::create_dir_all(&media).unwrap();
        let video = media.join("Movie.mp4");
        fs::write(&video, b"not a real mp4").unwrap();
        let srt = media.join("Movie.en.srt");
        fs::write(&srt, b"1\n00:00:00,000 --> 00:00:01,000\nHi\n").unwrap();

        let db = Arc::new(nightjar_db::open(dir.path()).unwrap());
        let lib = db
            .create_library(&NewLibrary {
                name: "t".into(),
                path: media.to_string_lossy().into_owned(),
                kind: "movies".into(),
            })
            .unwrap();
        let pool = test_pool(&db, dir.path());
        wait_job(
            &db,
            start_scan_job(Arc::clone(&db), Arc::clone(&pool), lib.id).unwrap(),
        );

        let item_id = db.list_items(lib.id).unwrap()[0].id;
        let first = db.get_item_sidecar(item_id, "s-en").unwrap().unwrap();
        assert_eq!(first.sidecar_generation, Some(1));
        assert!(first.content_id.is_some());

        // Restart: a new pool has a cold walk cache, and the media is unchanged.
        let pool2 = test_pool(&db, dir.path());
        wait_job(
            &db,
            start_scan_job(Arc::clone(&db), Arc::clone(&pool2), lib.id).unwrap(),
        );
        assert_eq!(
            db.get_item_sidecar(item_id, "s-en").unwrap().unwrap(),
            first,
            "an unchanged sidecar keeps its row and generation across restart"
        );

        // Change only the sidecar bytes, leave the media untouched, restart
        // again: the fresh scan must observe it.
        fs::write(&srt, b"1\n00:00:00,000 --> 00:00:02,000\nYo\n").unwrap();
        let pool3 = test_pool(&db, dir.path());
        wait_job(
            &db,
            start_scan_job(Arc::clone(&db), Arc::clone(&pool3), lib.id).unwrap(),
        );
        let changed = db.get_item_sidecar(item_id, "s-en").unwrap().unwrap();
        assert_eq!(changed.sidecar_generation, Some(2));
        assert_ne!(changed.content_id, first.content_id);
    }

    /// D2B.1 relist gap: a manual scan must reconcile sidecars for every
    /// directory it re-listed, not only the ones whose mtime moved. The pool's
    /// walk cache is warm from the first scan, and every edit below restores
    /// both parent directory mtimes, so only the fresh walk's relist can
    /// surface the change.
    ///
    /// One pass covers the four cases plan step 1 names: in-place edit, add,
    /// remove, and a nested `Subs/` change. The unchanged sidecar proves the
    /// pass leaves a row it did not need to touch alone.
    #[cfg(unix)]
    #[test]
    fn manual_scan_with_warm_cache_reconciles_edited_added_removed_and_nested_sidecars() {
        let tmp = tempfile::tempdir().unwrap();
        let dir = fs::canonicalize(tmp.path()).unwrap();
        let media = dir.join("media");
        let subs = media.join("Subs");
        fs::create_dir_all(&subs).unwrap();
        fs::write(media.join("Movie.mp4"), b"not a real mp4").unwrap();
        let edited = media.join("Movie.en.srt");
        fs::write(&edited, b"1\n00:00:00,000 --> 00:00:01,000\nHi\n").unwrap();
        let removed = media.join("Movie.es.srt");
        fs::write(&removed, b"1\n00:00:00,000 --> 00:00:01,000\nHola\n").unwrap();
        let nested_edited = subs.join("Movie.fr.srt");
        fs::write(&nested_edited, b"1\n00:00:00,000 --> 00:00:01,000\nSalut\n").unwrap();
        fs::write(
            media.join("Movie.de.srt"),
            b"1\n00:00:00,000 --> 00:00:01,000\nHallo\n",
        )
        .unwrap();

        let db = Arc::new(nightjar_db::open(&dir).unwrap());
        let pool = test_pool(&db, &dir);
        let lib = db
            .create_library(&NewLibrary {
                name: "t".into(),
                path: media.to_string_lossy().into_owned(),
                kind: "movies".into(),
            })
            .unwrap();
        wait_job(
            &db,
            start_scan_job(Arc::clone(&db), Arc::clone(&pool), lib.id).unwrap(),
        );
        let item_id = db.list_items(lib.id).unwrap()[0].id;
        assert_eq!(
            db.list_item_sidecars(item_id).unwrap().len(),
            4,
            "setup: four sidecars associated"
        );
        let untouched_before = db.get_item_sidecar(item_id, "s-de").unwrap().unwrap();
        let untouched_generation = untouched_before.sidecar_generation.unwrap();

        // Mutate with both parent directory mtimes restored, so nothing but the
        // fresh relist can surface the change.
        let media_mtime = fs::metadata(&media).unwrap().modified().unwrap();
        let subs_mtime = fs::metadata(&subs).unwrap().modified().unwrap();
        std::thread::sleep(Duration::from_millis(1100));
        fs::write(&edited, b"1\n00:00:00,000 --> 00:00:02,000\nYo\n").unwrap();
        fs::remove_file(&removed).unwrap();
        fs::write(
            media.join("Movie.it.srt"),
            b"1\n00:00:00,000 --> 00:00:01,000\nCiao\n",
        )
        .unwrap();
        fs::write(
            &nested_edited,
            b"1\n00:00:00,000 --> 00:00:02,000\nBonjour\n",
        )
        .unwrap();
        fs::write(
            subs.join("Movie.nl.srt"),
            b"1\n00:00:00,000 --> 00:00:01,000\nHoi\n",
        )
        .unwrap();
        fs::File::open(&media)
            .unwrap()
            .set_times(fs::FileTimes::new().set_modified(media_mtime))
            .unwrap();
        fs::File::open(&subs)
            .unwrap()
            .set_times(fs::FileTimes::new().set_modified(subs_mtime))
            .unwrap();

        // The second scan reuses the first scan's warm walk cache.
        wait_job(
            &db,
            start_scan_job(Arc::clone(&db), Arc::clone(&pool), lib.id).unwrap(),
        );

        let mut rows: Vec<(String, i64)> = db
            .list_item_sidecars(item_id)
            .unwrap()
            .into_iter()
            .map(|s| (s.track_id, s.sidecar_generation.expect("generation")))
            .collect();
        rows.sort();
        assert_eq!(
            rows.iter().map(|(id, _)| id.as_str()).collect::<Vec<_>>(),
            vec!["s-Subs.fr", "s-Subs.nl", "s-de", "s-en", "s-it"],
            "one manual scan must produce the exact current track set"
        );
        let generation_of = |want: &str| rows.iter().find(|(id, _)| id == want).unwrap().1;
        for changed in ["s-en", "s-Subs.fr", "s-it", "s-Subs.nl"] {
            assert!(
                generation_of(changed) > untouched_generation,
                "{changed} must receive a new generation"
            );
        }
        assert_eq!(
            db.get_item_sidecar(item_id, "s-de").unwrap().unwrap(),
            untouched_before,
            "an unchanged sidecar keeps its row and generation"
        );
        assert!(
            db.get_item_sidecar(item_id, "s-es").unwrap().is_none(),
            "a removed sidecar leaves no row"
        );
    }

    #[test]
    fn incomplete_sidecar_discovery_preserves_the_prior_set() {
        let tmp = tempfile::tempdir().unwrap();
        let dir = fs::canonicalize(tmp.path()).unwrap();
        let media = dir.join("media");
        fs::create_dir_all(&media).unwrap();
        let video = media.join("Movie.mp4");
        fs::write(&video, b"not a real mp4").unwrap();
        fs::write(
            media.join("Movie.en.srt"),
            b"1\n00:00:00,000 --> 00:00:01,000\nHi\n",
        )
        .unwrap();
        fs::write(
            media.join("Movie.fr.srt"),
            b"1\n00:00:00,000 --> 00:00:01,000\nSalut\n",
        )
        .unwrap();

        let db = nightjar_db::open(&dir).unwrap();
        let lib = db
            .create_library(&NewLibrary {
                name: "t".into(),
                path: media.to_string_lossy().into_owned(),
                kind: "movies".into(),
            })
            .unwrap();
        let item_id = db
            .upsert_items_indexed(
                lib.id,
                &[nightjar_db::UpsertItem {
                    path: "Movie.mp4".into(),
                    mtime_ms: 1,
                    size_bytes: 15,
                    title: "Movie".into(),
                    kind: "movie".into(),
                    year: None,
                    season: None,
                    episode: None,
                    content_id: None,
                }],
            )
            .unwrap()[0];
        let root = media.to_string_lossy().into_owned();
        associate_sidecars(
            &db,
            item_id,
            &root,
            &video,
            &mut nightjar_transcode::SidecarDirCache::default(),
        )
        .unwrap();
        let before = db.list_item_sidecars(item_id).unwrap();
        assert_eq!(before.len(), 2);

        // Deterministically make the parent listing incomplete: discovery must
        // fail before reconciliation instead of publishing an empty removal.
        fs::remove_dir_all(&media).unwrap();
        let error = associate_sidecars(
            &db,
            item_id,
            &root,
            &video,
            &mut nightjar_transcode::SidecarDirCache::default(),
        )
        .unwrap_err();
        assert!(error.contains("list subtitle dir"), "{error}");
        assert_eq!(db.list_item_sidecars(item_id).unwrap(), before);
    }

    /// Component-wise, not a string prefix. `to_relpath`'s case-folded fallback
    /// would call `/tmp/Library/x` in-root for `/tmp/library`; this must not.
    #[test]
    fn is_beneath_is_component_wise() {
        assert!(is_beneath(
            Path::new("/tmp/library"),
            Path::new("/tmp/library/a.srt")
        ));
        assert!(!is_beneath(
            Path::new("/tmp/library"),
            Path::new("/tmp/Library/a.srt")
        ));
        assert!(!is_beneath(
            Path::new("/tmp/library"),
            Path::new("/tmp/library")
        ));
        assert!(!is_beneath(
            Path::new("/tmp/library"),
            Path::new("/tmp/library-other/a.srt")
        ));
    }

    /// True when `dir`'s filesystem keeps `X` and `x` as distinct names.
    #[cfg(unix)]
    fn case_sensitive_fs(dir: &Path) -> bool {
        let upper = dir.join("CaseProbe");
        if fs::create_dir(&upper).is_err() {
            return false;
        }
        let distinct = !dir.join("caseprobe").exists();
        let _ = fs::remove_dir(&upper);
        distinct
    }

    #[cfg(unix)]
    fn write_sidecar(path: &Path, text: &str) {
        fs::write(path, format!("1\n00:00:00,000 --> 00:00:01,000\n{text}\n")).unwrap();
    }

    #[cfg(unix)]
    fn sidecar_track_ids(db: &Db, item_id: i64) -> Vec<String> {
        let mut ids: Vec<String> = db
            .list_item_sidecars(item_id)
            .unwrap()
            .into_iter()
            .map(|s| s.track_id)
            .collect();
        ids.sort();
        ids
    }

    /// A sidecar symlink whose canonical target leaves the library root is
    /// rejected before `to_relpath` and before the content-identity read.
    ///
    /// The directory cache is primed while the link points at a regular file,
    /// then the link is retargeted to an external directory, so the candidate
    /// still reaches `associate_sidecars`. Bypassing confinement would make
    /// `content_id_for_path` fail the whole item (a directory read); correct
    /// rejection is per-candidate, so the ordinary in-root sidecar still
    /// associates. That association is the live control: the test fails if
    /// discovery ever stops returning the candidate.
    #[cfg(unix)]
    #[test]
    fn sidecar_symlink_outside_root_is_rejected_before_the_identity_read() {
        use std::os::unix::fs::symlink;

        let tmp = tempfile::tempdir().unwrap();
        let dir = fs::canonicalize(tmp.path()).unwrap();
        let media = dir.join("media");
        let outside = dir.join("outside");
        fs::create_dir_all(&media).unwrap();
        fs::create_dir_all(&outside).unwrap();
        let video = media.join("Movie.mp4");
        fs::write(&video, b"not a real mp4").unwrap();
        write_sidecar(&media.join("Movie.fr.srt"), "Salut");
        let source = media.join("escape-source.srt");
        write_sidecar(&source, "En");
        let link = media.join("Movie.en.srt");
        symlink(&source, &link).unwrap();

        let db = nightjar_db::open(&dir).unwrap();
        let lib = db
            .create_library(&NewLibrary {
                name: "t".into(),
                path: media.to_string_lossy().into_owned(),
                kind: "movies".into(),
            })
            .unwrap();
        let item_id = db
            .upsert_items_indexed(
                lib.id,
                &[nightjar_db::UpsertItem {
                    path: "Movie.mp4".into(),
                    mtime_ms: 1,
                    size_bytes: 15,
                    title: "Movie".into(),
                    kind: "movie".into(),
                    year: None,
                    season: None,
                    episode: None,
                    content_id: None,
                }],
            )
            .unwrap()[0];
        let root = media.to_string_lossy().into_owned();

        let mut cache = nightjar_transcode::SidecarDirCache::default();
        let primed =
            nightjar_transcode::discover_sidecars_cached(&video, Some(&mut cache)).unwrap();
        assert_eq!(primed.len(), 2, "setup: both candidates must be cached");

        fs::remove_file(&link).unwrap();
        symlink(&outside, &link).unwrap();

        let (delta, skipped) = associate_sidecars(&db, item_id, &root, &video, &mut cache).unwrap();
        assert_eq!(skipped, 1, "the external-directory target must be counted");
        assert_eq!(
            sidecar_track_ids(&db, item_id),
            vec!["s-fr"],
            "the in-root sidecar must still associate, proving per-candidate rejection"
        );
        assert!(
            delta.added.len() == 1,
            "the delta must contain only the in-root sidecar"
        );
    }

    /// A full scan rejects an in-library sidecar symlink whose target is
    /// outside the root, associates the ordinary in-root sidecar, and counts
    /// the rejection in the visible job and library counters.
    #[cfg(unix)]
    #[test]
    fn scan_rejects_sidecar_symlink_outside_root_and_counts_it() {
        use std::os::unix::fs::symlink;

        let tmp = tempfile::tempdir().unwrap();
        let dir = fs::canonicalize(tmp.path()).unwrap();
        let media = dir.join("media");
        let outside = dir.join("outside");
        fs::create_dir_all(&media).unwrap();
        fs::create_dir_all(&outside).unwrap();
        fs::write(media.join("Movie.mp4"), b"not a real mp4").unwrap();
        write_sidecar(&media.join("Movie.fr.srt"), "Salut");
        let secret = outside.join("secret.srt");
        write_sidecar(&secret, "Secret");
        symlink(&secret, media.join("Movie.en.srt")).unwrap();

        let db = Arc::new(nightjar_db::open(&dir).unwrap());
        let pool = test_pool(&db, &dir);
        let lib = db
            .create_library(&NewLibrary {
                name: "t".into(),
                path: media.to_string_lossy().into_owned(),
                kind: "movies".into(),
            })
            .unwrap();
        let job_id = start_scan_job(Arc::clone(&db), Arc::clone(&pool), lib.id).unwrap();
        wait_job(&db, job_id);

        let item = db
            .list_items(lib.id)
            .unwrap()
            .into_iter()
            .find(|i| i.path == "Movie.mp4")
            .expect("media item");
        assert_eq!(
            sidecar_track_ids(&db, item.id),
            vec!["s-fr"],
            "the external target must not associate"
        );
        let job = db.get_scan_job(job_id).unwrap().unwrap();
        assert!(
            job.skipped_outside_root >= 1,
            "the scan job must count the rejection, got {}",
            job.skipped_outside_root
        );
        let lib = db.get_library(lib.id).unwrap().unwrap();
        assert!(
            lib.skipped_outside_root >= 1,
            "the library must count the rejection, got {}",
            lib.skipped_outside_root
        );
    }

    /// The path-hint association path shares the same confinement and counts
    /// the rejection on the library counter, because a hint has no scan job.
    #[cfg(unix)]
    #[test]
    fn hint_ingest_rejects_sidecar_symlink_outside_root_and_counts_it() {
        use std::os::unix::fs::symlink;

        let tmp = tempfile::tempdir().unwrap();
        let dir = fs::canonicalize(tmp.path()).unwrap();
        let media = dir.join("media");
        let outside = dir.join("outside");
        fs::create_dir_all(&media).unwrap();
        fs::create_dir_all(&outside).unwrap();
        let video = media.join("Movie.mp4");
        fs::write(&video, b"not a real mp4").unwrap();
        write_sidecar(&media.join("Movie.fr.srt"), "Salut");
        write_sidecar(&outside.join("secret.srt"), "Secret");
        symlink(outside.join("secret.srt"), media.join("Movie.en.srt")).unwrap();

        let db = Arc::new(nightjar_db::open(&dir).unwrap());
        let pool = test_pool(&db, &dir);
        let lib = db
            .create_library(&NewLibrary {
                name: "t".into(),
                path: media.to_string_lossy().into_owned(),
                kind: "movies".into(),
            })
            .unwrap();

        let out = hint_ingest(db.as_ref(), pool.as_ref(), lib.id, &video).unwrap();
        let HintIngestOutcome::Upserted { item_id } = out else {
            panic!("expected Upserted, got {out:?}");
        };
        assert_eq!(
            sidecar_track_ids(&db, item_id),
            vec!["s-fr"],
            "the external target must not associate on the hint path"
        );
        let lib = db.get_library(lib.id).unwrap().unwrap();
        assert!(
            lib.skipped_outside_root >= 1,
            "the hint must count the rejection on the library, got {}",
            lib.skipped_outside_root
        );
    }

    /// An in-root symlinked sidecar keeps associating, and the stored path is
    /// the discovered name beside the media, not the resolved target.
    #[cfg(unix)]
    #[test]
    fn in_root_symlinked_sidecar_associates_under_its_discovered_path() {
        use std::os::unix::fs::symlink;

        let tmp = tempfile::tempdir().unwrap();
        let dir = fs::canonicalize(tmp.path()).unwrap();
        let media = dir.join("media");
        fs::create_dir_all(&media).unwrap();
        fs::write(media.join("Movie.mp4"), b"not a real mp4").unwrap();
        let target = media.join("source.srt");
        write_sidecar(&target, "En");
        symlink(&target, media.join("Movie.en.srt")).unwrap();

        let db = Arc::new(nightjar_db::open(&dir).unwrap());
        let pool = test_pool(&db, &dir);
        let lib = db
            .create_library(&NewLibrary {
                name: "t".into(),
                path: media.to_string_lossy().into_owned(),
                kind: "movies".into(),
            })
            .unwrap();
        let job_id = start_scan_job(Arc::clone(&db), Arc::clone(&pool), lib.id).unwrap();
        wait_job(&db, job_id);

        let item_id = db.list_items(lib.id).unwrap()[0].id;
        let row = db.get_item_sidecar(item_id, "s-en").unwrap().unwrap();
        assert_eq!(
            row.path, "Movie.en.srt",
            "the discovered in-root path is stored, not the resolved target"
        );
        assert!(row.content_id.is_some(), "the in-root target is read");
        let job = db.get_scan_job(job_id).unwrap().unwrap();
        assert_eq!(
            job.skipped_outside_root, 0,
            "an in-root symlink is not a rejection"
        );
    }

    /// Retargeting an associated in-root sidecar symlink outside the root is
    /// reconciled by the next full scan: the prior association is removed and
    /// the rejection is visible in the counters.
    #[cfg(unix)]
    #[test]
    fn scan_removes_prior_sidecar_association_after_symlink_retarget_outside_root() {
        use std::os::unix::fs::symlink;

        let tmp = tempfile::tempdir().unwrap();
        let dir = fs::canonicalize(tmp.path()).unwrap();
        let media = dir.join("media");
        let outside = dir.join("outside");
        fs::create_dir_all(&media).unwrap();
        fs::create_dir_all(&outside).unwrap();
        fs::write(media.join("Movie.mp4"), b"not a real mp4").unwrap();
        let in_root = media.join("source.srt");
        write_sidecar(&in_root, "En");
        let link = media.join("Movie.en.srt");
        symlink(&in_root, &link).unwrap();
        write_sidecar(&outside.join("secret.srt"), "Secret");

        let db = Arc::new(nightjar_db::open(&dir).unwrap());
        let pool = test_pool(&db, &dir);
        let lib = db
            .create_library(&NewLibrary {
                name: "t".into(),
                path: media.to_string_lossy().into_owned(),
                kind: "movies".into(),
            })
            .unwrap();
        let job1 = start_scan_job(Arc::clone(&db), Arc::clone(&pool), lib.id).unwrap();
        wait_job(&db, job1);
        let item_id = db.list_items(lib.id).unwrap()[0].id;
        assert_eq!(sidecar_track_ids(&db, item_id), vec!["s-en"]);

        fs::remove_file(&link).unwrap();
        symlink(outside.join("secret.srt"), &link).unwrap();

        let job2 = start_scan_job(Arc::clone(&db), Arc::clone(&pool), lib.id).unwrap();
        wait_job(&db, job2);
        assert!(
            sidecar_track_ids(&db, item_id).is_empty(),
            "the prior association must be removed without reading the external target"
        );
        let job = db.get_scan_job(job2).unwrap().unwrap();
        assert!(
            job.skipped_outside_root >= 1,
            "the retarget rejection must be counted, got {}",
            job.skipped_outside_root
        );
        let lib = db.get_library(lib.id).unwrap().unwrap();
        assert!(lib.skipped_outside_root >= 1);
    }

    /// The same retarget, reconciled by a path hint instead of a full scan. The
    /// media mtime changes so the hint takes its upsert branch, which is the
    /// only branch that reconciles sidecars.
    #[cfg(unix)]
    #[test]
    fn hint_ingest_removes_prior_sidecar_association_after_symlink_retarget_outside_root() {
        use std::os::unix::fs::symlink;

        let tmp = tempfile::tempdir().unwrap();
        let dir = fs::canonicalize(tmp.path()).unwrap();
        let media = dir.join("media");
        let outside = dir.join("outside");
        fs::create_dir_all(&media).unwrap();
        fs::create_dir_all(&outside).unwrap();
        let video = media.join("Movie.mp4");
        fs::write(&video, b"not a real mp4").unwrap();
        let in_root = media.join("source.srt");
        write_sidecar(&in_root, "En");
        let link = media.join("Movie.en.srt");
        symlink(&in_root, &link).unwrap();
        write_sidecar(&outside.join("secret.srt"), "Secret");

        let db = Arc::new(nightjar_db::open(&dir).unwrap());
        let pool = test_pool(&db, &dir);
        let lib = db
            .create_library(&NewLibrary {
                name: "t".into(),
                path: media.to_string_lossy().into_owned(),
                kind: "movies".into(),
            })
            .unwrap();
        let job = start_scan_job(Arc::clone(&db), Arc::clone(&pool), lib.id).unwrap();
        wait_job(&db, job);
        let item_id = db.list_items(lib.id).unwrap()[0].id;
        assert_eq!(sidecar_track_ids(&db, item_id), vec!["s-en"]);

        fs::remove_file(&link).unwrap();
        symlink(outside.join("secret.srt"), &link).unwrap();
        std::thread::sleep(std::time::Duration::from_millis(20));
        fs::write(&video, b"not a real mp4 but longer").unwrap();

        hint_ingest(db.as_ref(), pool.as_ref(), lib.id, &video).unwrap();
        assert!(
            sidecar_track_ids(&db, item_id).is_empty(),
            "the hint must remove the prior association without reading the external target"
        );
        let lib = db.get_library(lib.id).unwrap().unwrap();
        assert!(
            lib.skipped_outside_root >= 1,
            "the hint must count the rejection on the library, got {}",
            lib.skipped_outside_root
        );
    }

    /// Confinement is component-wise on canonical paths, so a target under a
    /// case-distinct sibling root is rejected even though `to_relpath`'s
    /// case-folded fallback would accept it. Skipped on case-insensitive
    /// filesystems, where the two roots are one directory.
    #[cfg(unix)]
    #[test]
    fn sidecar_symlink_under_case_distinct_sibling_root_is_rejected() {
        use std::os::unix::fs::symlink;

        let tmp = tempfile::tempdir().unwrap();
        let base = fs::canonicalize(tmp.path()).unwrap();
        let root = base.join("library");
        fs::create_dir_all(&root).unwrap();
        if !case_sensitive_fs(&base) {
            eprintln!("skipping: filesystem is case-insensitive");
            return;
        }
        let sibling = base.join("Library");
        fs::create_dir_all(&sibling).unwrap();
        fs::write(root.join("Movie.mp4"), b"not a real mp4").unwrap();
        write_sidecar(&root.join("Movie.fr.srt"), "Salut");
        write_sidecar(&sibling.join("secret.srt"), "Secret");
        symlink(sibling.join("secret.srt"), root.join("Movie.en.srt")).unwrap();

        let db = Arc::new(nightjar_db::open(&base).unwrap());
        let pool = test_pool(&db, &base);
        let lib = db
            .create_library(&NewLibrary {
                name: "t".into(),
                path: root.to_string_lossy().into_owned(),
                kind: "movies".into(),
            })
            .unwrap();
        let job_id = start_scan_job(Arc::clone(&db), Arc::clone(&pool), lib.id).unwrap();
        wait_job(&db, job_id);

        let item = db
            .list_items(lib.id)
            .unwrap()
            .into_iter()
            .find(|i| i.path == "Movie.mp4")
            .expect("media item");
        assert_eq!(
            sidecar_track_ids(&db, item.id),
            vec!["s-fr"],
            "a case-distinct sibling root is not beneath the library root"
        );
        let job = db.get_scan_job(job_id).unwrap().unwrap();
        assert!(
            job.skipped_outside_root >= 1,
            "the case-distinct rejection must be counted, got {}",
            job.skipped_outside_root
        );
    }

    #[test]
    fn empty_walk_with_existing_items_does_not_delete() {
        let dir = tempfile::tempdir().unwrap();
        let media = dir.path().join("media");
        fs::create_dir_all(&media).unwrap();
        let video = media.join("Keep.mp4");
        fs::write(&video, b"not a real mp4").unwrap();

        let db = Arc::new(nightjar_db::open(dir.path()).unwrap());
        let pool = test_pool(&db, dir.path());
        let lib = db
            .create_library(&NewLibrary {
                name: "t".into(),
                path: media.to_string_lossy().into_owned(),
                kind: "movies".into(),
            })
            .unwrap();

        let job_id = start_scan_job(Arc::clone(&db), Arc::clone(&pool), lib.id).unwrap();
        for _ in 0..200 {
            let job = db.get_scan_job(job_id).unwrap().unwrap();
            if job.state == "completed" || job.state == "failed" {
                break;
            }
            std::thread::sleep(std::time::Duration::from_millis(50));
        }
        assert_eq!(db.count_items(lib.id).unwrap(), 1);

        // Simulate stale empty mount: wipe files but keep the directory.
        fs::remove_file(&video).unwrap();
        let before = pool.transition_count();
        let job2 = start_scan_job(Arc::clone(&db), Arc::clone(&pool), lib.id).unwrap();
        for _ in 0..200 {
            let job = db.get_scan_job(job2).unwrap().unwrap();
            if job.state == "completed" || job.state == "failed" {
                assert_eq!(job.removed, 0, "empty walk must not delete under doubt");
                break;
            }
            std::thread::sleep(std::time::Duration::from_millis(50));
        }
        assert_eq!(db.count_items(lib.id).unwrap(), 1);
        let _ = before;
    }

    /// CHK-WC: the ADR-0014 §2 reachability pause is the walk's cancel signal.
    /// A scan started while the library is paused abandons the pass at its
    /// first directory boundary: the job fails with the cancellation message
    /// and every catalog row stays as it was. The unpaused control then removes
    /// a file and shows the ordinary path still deletes it, so cancellation
    /// changed nothing about clean-walk deletion.
    #[test]
    fn paused_library_cancels_the_walk_and_leaves_catalog_rows_intact() {
        let dir = tempfile::tempdir().unwrap();
        let media = dir.path().join("media");
        fs::create_dir_all(&media).unwrap();
        let a = media.join("A.mp4");
        let b = media.join("B.mkv");
        fs::write(&a, b"a").unwrap();
        fs::write(&b, b"b").unwrap();

        let db = Arc::new(nightjar_db::open(dir.path()).unwrap());
        let pool = test_pool(&db, dir.path());
        let lib = db
            .create_library(&NewLibrary {
                name: "t".into(),
                path: media.to_string_lossy().into_owned(),
                kind: "movies".into(),
            })
            .unwrap();

        let setup = start_scan_job(Arc::clone(&db), Arc::clone(&pool), lib.id).unwrap();
        wait_job(&db, setup);
        assert_eq!(
            db.count_items(lib.id).unwrap(),
            2,
            "setup: both rows indexed"
        );

        // Pause before the job starts, so the walk cannot race the test: it
        // begins, observes the pause at its first directory boundary, and stops.
        pool.availability.pause.set_paused(lib.id, true);
        let cancelled_job = start_scan_job(Arc::clone(&db), Arc::clone(&pool), lib.id).unwrap();
        let mut terminal = None;
        for _ in 0..200 {
            let job = db.get_scan_job(cancelled_job).unwrap().unwrap();
            if job.state == "completed" || job.state == "failed" {
                terminal = Some(job);
                break;
            }
            std::thread::sleep(std::time::Duration::from_millis(50));
        }
        let job = terminal.expect("cancelled scan job did not finish");
        assert_eq!(
            job.state, "failed",
            "a cancelled walk must fail its job, not complete it"
        );
        let message = job.error_message.unwrap_or_default();
        assert!(
            message.contains("walk cancelled"),
            "the cancellation must be visible on the job: {message:?}"
        );
        assert_eq!(job.removed, 0, "a cancelled walk must not delete");
        assert_eq!(
            db.count_items(lib.id).unwrap(),
            2,
            "a cancelled walk must leave every catalog row intact"
        );

        // Positive control: the same fixture, unpaused, still deletes a removed
        // file. The cancellation above did not change the clean-walk path.
        pool.availability.pause.set_paused(lib.id, false);
        fs::remove_file(&b).unwrap();
        let clean_job = start_scan_job(Arc::clone(&db), Arc::clone(&pool), lib.id).unwrap();
        wait_job(&db, clean_job);
        assert_eq!(
            db.get_scan_job(clean_job).unwrap().unwrap().removed,
            1,
            "a clean walk must still delete the removed row"
        );
        assert_eq!(db.count_items(lib.id).unwrap(), 1);
    }

    /// R4 storage bounds (SCAN-D1, preserved by CHK-FC): an incomplete listing
    /// must not authorize `delete_missing`. Scan 1 lists A and B. B then becomes
    /// a dangling symlink, so scan 2 reports a partial listing and skips delete
    /// under doubt. Scan 3 sees the same directory mtime; the partial listing
    /// must not become authoritative, so B's row survives. Restoring B gives a
    /// clean listing, and only then does removing B delete it by the ordinary
    /// path. Unfixed, scan 3 loses B. The polls now walk fresh (CHK-FC), so the
    /// doubt comes from the fresh listing itself, not a reused cache entry.
    #[cfg(unix)]
    #[test]
    fn incomplete_cached_listing_never_authorizes_deletion() {
        use std::os::unix::fs::symlink;
        use std::thread;
        use std::time::Duration;

        let dir = tempfile::tempdir().unwrap();
        let media = dir.path().join("media");
        fs::create_dir_all(&media).unwrap();
        let a = media.join("A.mp4");
        let b = media.join("B.mkv");
        fs::write(&a, b"a").unwrap();
        fs::write(&b, b"b").unwrap();

        let db = Arc::new(nightjar_db::open(dir.path()).unwrap());
        let pool = test_pool(&db, dir.path());
        let lib = db
            .create_library(&NewLibrary {
                name: "t".into(),
                path: media.to_string_lossy().into_owned(),
                kind: "movies".into(),
            })
            .unwrap();

        // Scan 1: complete listing. Both rows exist.
        let job0 = start_scan_job(Arc::clone(&db), Arc::clone(&pool), lib.id).unwrap();
        wait_job(&db, job0);
        assert_eq!(db.count_items(lib.id).unwrap(), 2, "setup: A and B indexed");

        // Scans 2-5 use the automatic poll, which now walks fresh.
        //
        // Scan 2: B's stat fails, so the fresh listing is partial.
        thread::sleep(Duration::from_millis(1100));
        fs::remove_file(&b).unwrap();
        symlink("missing-target.mkv", &b).unwrap();
        let job1 = request_scan(
            Arc::clone(&db),
            Arc::clone(&pool),
            lib.id,
            ScanTrigger::Poll,
        )
        .unwrap();
        wait_job(&db, job1);
        assert_eq!(
            db.count_items(lib.id).unwrap(),
            2,
            "a partial listing must not delete the unreadable entry's row"
        );

        // Scan 3: same directory mtime. The partial listing must stay doubtful,
        // so B's row is kept.
        let job2 = request_scan(
            Arc::clone(&db),
            Arc::clone(&pool),
            lib.id,
            ScanTrigger::Poll,
        )
        .unwrap();
        wait_job(&db, job2);
        let job2_row = db.get_scan_job(job2).unwrap().unwrap();
        assert_eq!(
            db.count_items(lib.id).unwrap(),
            2,
            "cached uncertainty must not authorize deletion; removed={}",
            job2_row.removed
        );

        // Scan 4: B is a real file again, so the listing is clean and clears
        // the doubt. Both rows are still there.
        thread::sleep(Duration::from_millis(1100));
        fs::remove_file(&b).unwrap();
        fs::write(&b, b"b2").unwrap();
        let job3 = request_scan(
            Arc::clone(&db),
            Arc::clone(&pool),
            lib.id,
            ScanTrigger::Poll,
        )
        .unwrap();
        wait_job(&db, job3);
        assert_eq!(
            db.count_items(lib.id).unwrap(),
            2,
            "a clean listing keeps both rows"
        );

        // Scan 5: B is really gone. A complete listing deletes it normally.
        thread::sleep(Duration::from_millis(1100));
        fs::remove_file(&b).unwrap();
        let job4 = request_scan(
            Arc::clone(&db),
            Arc::clone(&pool),
            lib.id,
            ScanTrigger::Poll,
        )
        .unwrap();
        wait_job(&db, job4);
        let job4_row = db.get_scan_job(job4).unwrap().unwrap();
        assert_eq!(job4_row.removed, 1, "a real removal must still delete");
        assert_eq!(db.count_items(lib.id).unwrap(), 1);
    }

    /// R4 storage bounds: a mount that disappears is a disconnect, not an empty
    /// library. The scan is refused, the library pauses, and no rows are
    /// deleted. Reconnecting recovers: reachability returns and a fresh scan
    /// completes. The row count on both sides of the flap is the corruption
    /// check.
    #[test]
    fn mount_disconnect_then_reconnect_keeps_items_and_rescans() {
        let dir = tempfile::tempdir().unwrap();
        let media = dir.path().join("media");
        let offline = dir.path().join("media-offline");
        fs::create_dir_all(&media).unwrap();
        fs::write(media.join("Movie.mp4"), b"not a real mp4").unwrap();

        let db = Arc::new(nightjar_db::open(dir.path()).unwrap());
        let pool = test_pool(&db, dir.path());
        let lib = db
            .create_library(&NewLibrary {
                name: "t".into(),
                path: media.to_string_lossy().into_owned(),
                kind: "movies".into(),
            })
            .unwrap();

        let job = start_scan_job(Arc::clone(&db), Arc::clone(&pool), lib.id).unwrap();
        wait_job(&db, job);
        assert_eq!(db.count_items(lib.id).unwrap(), 1);

        // Disconnect: the mount root disappears.
        fs::rename(&media, &offline).unwrap();
        let refused = start_scan_job(Arc::clone(&db), Arc::clone(&pool), lib.id);
        assert!(refused.is_err(), "a disconnected root must refuse the scan");
        assert!(
            !pool.is_library_reachable(lib.id),
            "a missing root must pause the library"
        );
        assert_eq!(
            db.count_items(lib.id).unwrap(),
            1,
            "a disconnect must not delete rows"
        );

        // Reconnect.
        fs::rename(&offline, &media).unwrap();
        pool.tick_reachability().unwrap();
        assert!(pool.is_library_reachable(lib.id), "the root is back");

        let job2 = start_scan_job(Arc::clone(&db), Arc::clone(&pool), lib.id).unwrap();
        wait_job(&db, job2);
        assert_eq!(db.count_items(lib.id).unwrap(), 1);
    }

    /// R4 storage bounds: a write failure on the subtitle volume (simulated
    /// with a read-only subs root) is an availability failure, not a permanent
    /// `error`. The existing read-failure test covers ENOENT; this covers the
    /// write half. The recovered extract is the positive control: the same item
    /// reaches `ready` once the volume is writable, so `unavailable` is not
    /// "nothing ever works".
    #[cfg(unix)]
    #[test]
    fn write_failure_during_extract_is_unavailable_and_recovers() {
        use std::os::unix::fs::PermissionsExt;

        let dir = tempfile::tempdir().unwrap();
        let media = dir.path().join("media");
        fs::create_dir_all(&media).unwrap();
        let video = media.join("Movie.mp4");
        fs::write(&video, b"not a real mp4").unwrap();
        fs::write(
            media.join("Movie.en.srt"),
            b"1\n00:00:00,000 --> 00:00:01,000\nHi\n",
        )
        .unwrap();

        let db = Arc::new(nightjar_db::open(dir.path()).unwrap());
        let pool = test_pool(&db, dir.path());
        let subs_root = dir.path().join("subs");
        let lib = db
            .create_library(&NewLibrary {
                name: "t".into(),
                path: media.to_string_lossy().into_owned(),
                kind: "movies".into(),
            })
            .unwrap();
        let ids = db
            .upsert_items_indexed(
                lib.id,
                &[UpsertItem {
                    path: "Movie.mp4".into(),
                    mtime_ms: 1,
                    size_bytes: 15,
                    title: "Movie".into(),
                    kind: "movie".into(),
                    year: None,
                    season: None,
                    episode: None,
                    content_id: None,
                }],
            )
            .unwrap();
        let item_id = ids[0];
        db.reconcile_item_sidecars(
            item_id,
            &[observed_sidecar(
                &media.join("Movie.en.srt"),
                "Movie.en.srt",
                "s-en",
                "srt",
            )],
        )
        .unwrap();
        certify_item(&db, item_id, &[]);
        db.set_subtitle_status(item_id, "eligible").unwrap();

        fs::set_permissions(&subs_root, fs::Permissions::from_mode(0o500)).unwrap();
        pool.enqueue(pool::WorkItem::extract(
            item_id,
            lib.id,
            video.clone(),
            None,
        ));

        let mut status = String::new();
        for _ in 0..200 {
            status = db.get_item(item_id).unwrap().unwrap().subtitle_status;
            if status != "pending" && status != "eligible" {
                break;
            }
            std::thread::sleep(std::time::Duration::from_millis(25));
        }
        fs::set_permissions(&subs_root, fs::Permissions::from_mode(0o700)).unwrap();
        assert_eq!(
            status, "unavailable",
            "a write failure is availability, not a permanent error"
        );

        // Recovery: the same item extracts once the volume is writable.
        db.set_subtitle_status(item_id, "eligible").unwrap();
        pool.enqueue(pool::WorkItem::extract(item_id, lib.id, video, None));
        let mut ready = false;
        for _ in 0..200 {
            if db.get_item(item_id).unwrap().unwrap().subtitle_status == "ready" {
                ready = true;
                break;
            }
            std::thread::sleep(std::time::Duration::from_millis(25));
        }
        assert!(ready, "extract must succeed once the volume is writable");
    }

    /// R4 storage bounds, embedded half: an FFmpeg write failure while it
    /// demuxes an embedded subtitle stream is availability, not a permanent
    /// `error`. The item dir is pre-created read-only, so `create_dir_all`
    /// succeeds and the failure is the real `ffmpeg` child failing to open its
    /// output. That is the path the sidecar test cannot reach. Recovery to
    /// `ready` once the dir is writable is the positive control.
    #[cfg(unix)]
    #[test]
    fn embedded_ffmpeg_write_failure_is_unavailable_and_recovers() {
        use std::os::unix::fs::PermissionsExt;

        if !require_ffprobe() {
            eprintln!("skip: ffprobe not on PATH");
            return;
        }
        if !require_ffmpeg() {
            eprintln!("skip: ffmpeg not on PATH");
            return;
        }

        let dir = tempfile::tempdir().unwrap();
        let media = dir.path().join("media");
        fs::create_dir_all(&media).unwrap();
        let fixture = corpus_fixture("h264_aac_srt_mkv.mkv");
        if skip_without_fixture(&fixture) {
            return;
        }
        fs::copy(&fixture, media.join("h264_aac_srt_mkv.mkv")).unwrap();

        let db = Arc::new(nightjar_db::open(dir.path()).unwrap());
        let pool = test_pool(&db, dir.path());
        let lib = db
            .create_library(&NewLibrary {
                name: "t".into(),
                path: media.to_string_lossy().into_owned(),
                kind: "movies".into(),
            })
            .unwrap();
        let ids = db
            .upsert_items_indexed(
                lib.id,
                &[UpsertItem {
                    path: "h264_aac_srt_mkv.mkv".into(),
                    mtime_ms: 1,
                    size_bytes: 64240,
                    title: "subbed".into(),
                    kind: "movie".into(),
                    year: None,
                    season: None,
                    episode: None,
                    content_id: None,
                }],
            )
            .unwrap();
        let item_id = ids[0];
        certify_item(
            &db,
            item_id,
            &text_streams(&media.join("h264_aac_srt_mkv.mkv")),
        );
        db.set_subtitle_status(item_id, "eligible").unwrap();

        // The item dir exists but is read-only, so the failure is the child's
        // write, not `create_dir_all`.
        let item_dir = dir.path().join("subs").join(item_id.to_string());
        fs::create_dir_all(&item_dir).unwrap();
        fs::set_permissions(&item_dir, fs::Permissions::from_mode(0o500)).unwrap();

        pool.enqueue(pool::WorkItem::extract(
            item_id,
            lib.id,
            media.join("x"),
            None,
        ));
        let mut status = String::new();
        for _ in 0..200 {
            status = db.get_item(item_id).unwrap().unwrap().subtitle_status;
            if status != "pending" && status != "eligible" {
                break;
            }
            std::thread::sleep(std::time::Duration::from_millis(25));
        }
        fs::set_permissions(&item_dir, fs::Permissions::from_mode(0o700)).unwrap();
        assert_eq!(
            status, "unavailable",
            "an ffmpeg write failure is availability, not a permanent error"
        );

        // Recovery: the same item extracts once the dir is writable.
        db.set_subtitle_status(item_id, "eligible").unwrap();
        pool.enqueue(pool::WorkItem::extract(
            item_id,
            lib.id,
            media.join("x"),
            None,
        ));
        let mut ready = false;
        for _ in 0..400 {
            if db.get_item(item_id).unwrap().unwrap().subtitle_status == "ready" {
                ready = true;
                break;
            }
            std::thread::sleep(std::time::Duration::from_millis(25));
        }
        assert!(ready, "extract must succeed once the dir is writable");
    }

    #[test]
    fn unavailable_root_dispatches_no_item_work() {
        let dir = tempfile::tempdir().unwrap();
        let media = dir.path().join("media");
        fs::create_dir_all(&media).unwrap();
        fs::write(media.join("A.mp4"), b"x").unwrap();

        let db = Arc::new(nightjar_db::open(dir.path()).unwrap());
        let pool = test_pool(&db, dir.path());
        let lib = db
            .create_library(&NewLibrary {
                name: "t".into(),
                path: media.to_string_lossy().into_owned(),
                kind: "movies".into(),
            })
            .unwrap();

        let job_id = start_scan_job(Arc::clone(&db), Arc::clone(&pool), lib.id).unwrap();
        for _ in 0..200 {
            let job = db.get_scan_job(job_id).unwrap().unwrap();
            if job.state == "completed" || job.state == "failed" {
                break;
            }
            std::thread::sleep(std::time::Duration::from_millis(50));
        }

        // Mark items unavailable as if a prior mount flap wrote them.
        for item in db.list_items(lib.id).unwrap() {
            db.apply_probe_update(&nightjar_db::ProbeUpdate {
                item_id: item.id,
                duration_ms: None,
                container: None,
                video_codec: None,
                audio_codec: None,
                audio_channels: None,
                width: None,
                height: None,
                video_bitrate_bps: None,
                video_frame_rate_num: None,
                video_frame_rate_den: None,
                hdr: None,
                probe_status: "unavailable".into(),
                scan_error: Some("unavailable: test".into()),
            })
            .unwrap();
            db.set_subtitle_status(item.id, "unavailable").unwrap();
        }

        let before = pool.transition_count();
        // Point library at a missing path via DB (simulate unmount).
        {
            // recreate library path by renaming away
            let gone = dir.path().join("gone");
            fs::rename(&media, &gone).unwrap();
        }
        pool.set_library_reachability(lib.id, &lib.path, false)
            .unwrap();
        assert_eq!(pool.transition_count(), before + 1);
        assert!(!pool.is_library_reachable(lib.id));

        // Enqueue must be a no-op while paused.
        for item in db.list_items(lib.id).unwrap() {
            pool.enqueue(pool::WorkItem::probe(
                item.id,
                lib.id,
                PathBuf::from(&item.path),
                None,
            ));
            pool.enqueue(pool::WorkItem::extract(
                item.id,
                lib.id,
                PathBuf::from(&item.path),
                None,
            ));
        }
        std::thread::sleep(std::time::Duration::from_millis(200));
        for item in db.list_items(lib.id).unwrap() {
            assert_eq!(item.probe_status, "unavailable");
            assert_ne!(item.probe_status, "error");
        }

        // Restore path and recover.
        fs::rename(dir.path().join("gone"), &media).unwrap();
        let path = media.to_string_lossy().into_owned();
        // Update stored path still points at media which exists again.
        pool.set_library_reachability(lib.id, &path, true).unwrap();
        assert!(pool.is_library_reachable(lib.id));
        for _ in 0..100 {
            let items = db.list_items(lib.id).unwrap();
            if items.iter().all(|i| i.probe_status != "unavailable") {
                break;
            }
            std::thread::sleep(std::time::Duration::from_millis(50));
        }
        let items = db.list_items(lib.id).unwrap();
        assert!(
            items.iter().all(|i| i.probe_status != "unavailable"),
            "recovery must clear unavailable: {items:?}"
        );
    }

    #[test]
    fn corrupt_file_stays_error_across_reachability_recovery() {
        let dir = tempfile::tempdir().unwrap();
        let media = dir.path().join("media");
        fs::create_dir_all(&media).unwrap();
        fs::write(media.join("broken_moov.mp4"), b"not a real mp4").unwrap();

        let db = Arc::new(nightjar_db::open(dir.path()).unwrap());
        let pool = test_pool(&db, dir.path());
        let lib = db
            .create_library(&NewLibrary {
                name: "t".into(),
                path: media.to_string_lossy().into_owned(),
                kind: "movies".into(),
            })
            .unwrap();

        let job_id = start_scan_job(Arc::clone(&db), Arc::clone(&pool), lib.id).unwrap();
        for _ in 0..200 {
            let job = db.get_scan_job(job_id).unwrap().unwrap();
            if job.state == "completed" || job.state == "failed" {
                break;
            }
            std::thread::sleep(std::time::Duration::from_millis(50));
        }
        let item = db.list_items(lib.id).unwrap().into_iter().next().unwrap();
        assert_eq!(item.probe_status, "error");

        pool.set_library_reachability(lib.id, &lib.path, false)
            .unwrap();
        pool.set_library_reachability(lib.id, &lib.path, true)
            .unwrap();
        std::thread::sleep(std::time::Duration::from_millis(100));
        let again = db.get_item(item.id).unwrap().unwrap();
        assert_eq!(
            again.probe_status, "error",
            "permanent errors must not be re-queued by reachability recovery"
        );
    }

    fn wait_scan(db: &Db, job_id: i64) {
        for _ in 0..200 {
            let job = db.get_scan_job(job_id).unwrap().unwrap();
            if job.state == "completed" || job.state == "failed" {
                return;
            }
            std::thread::sleep(std::time::Duration::from_millis(50));
        }
        panic!("scan job {job_id} did not finish");
    }

    /// Scan requeues probe_status=unavailable (ADR-0014 retryable) but not error.
    #[test]
    fn scan_requeues_unavailable_not_permanent_error() {
        let dir = tempfile::tempdir().unwrap();
        let media = dir.path().join("media");
        fs::create_dir_all(&media).unwrap();
        fs::write(media.join("A.mp4"), b"x").unwrap();
        fs::write(media.join("broken_moov.mp4"), b"not a real mp4").unwrap();

        let db = Arc::new(nightjar_db::open(dir.path()).unwrap());
        let pool = test_pool(&db, dir.path());
        let lib = db
            .create_library(&NewLibrary {
                name: "t".into(),
                path: media.to_string_lossy().into_owned(),
                kind: "movies".into(),
            })
            .unwrap();

        let job1 = start_scan_job(Arc::clone(&db), Arc::clone(&pool), lib.id).unwrap();
        wait_scan(&db, job1);

        let items = db.list_items(lib.id).unwrap();
        let a = items
            .iter()
            .find(|i| i.path.ends_with("A.mp4"))
            .expect("A.mp4");
        let broken = items
            .iter()
            .find(|i| i.path.ends_with("broken_moov.mp4"))
            .expect("broken");
        assert_eq!(broken.probe_status, "error");

        // Simulate prior ENOENT-class failures on A; leave broken as permanent error.
        db.apply_probe_update(&nightjar_db::ProbeUpdate {
            item_id: a.id,
            duration_ms: None,
            container: None,
            video_codec: None,
            audio_codec: None,
            audio_channels: None,
            width: None,
            height: None,
            video_bitrate_bps: None,
            video_frame_rate_num: None,
            video_frame_rate_den: None,
            hdr: None,
            probe_status: "unavailable".into(),
            scan_error: Some("no such file or directory".into()),
        })
        .unwrap();
        db.set_subtitle_status(a.id, "unavailable").unwrap();

        // Drain with stored relpath only (dogfood failure mode) must still resolve.
        let n = pool.drain_pending_probes().unwrap();
        assert_eq!(
            n, 0,
            "unavailable is not indexed; drain skips until requeue"
        );

        let job2 = start_scan_job(Arc::clone(&db), Arc::clone(&pool), lib.id).unwrap();
        wait_scan(&db, job2);

        let a2 = db.get_item(a.id).unwrap().unwrap();
        assert_ne!(
            a2.probe_status, "unavailable",
            "scan must requeue unavailable; got {:?}",
            a2
        );
        assert!(
            a2.probe_status == "error"
                || a2.probe_status == "probed"
                || a2.probe_status == "indexed",
            "expected re-probe outcome, got {}",
            a2.probe_status
        );

        let broken2 = db.get_item(broken.id).unwrap().unwrap();
        assert_eq!(
            broken2.probe_status, "error",
            "permanent error must not be cleared by scan requeue"
        );
    }

    /// drain_pending_probes joins library root to ADR-0030 relpaths.
    #[test]
    fn drain_pending_probes_resolves_relpath() {
        let dir = tempfile::tempdir().unwrap();
        let media = dir.path().join("media");
        fs::create_dir_all(&media).unwrap();
        // Real tiny mp4 when ffmpeg exists so probe can succeed; otherwise accept error.
        let good = media.join("clip.mp4");
        let _ = Command::new("ffmpeg")
            .args([
                "-y",
                "-hide_banner",
                "-loglevel",
                "error",
                "-f",
                "lavfi",
                "-i",
                "color=c=black:s=64x64:d=0.2",
                "-c:v",
                "libx264",
                "-pix_fmt",
                "yuv420p",
                good.to_str().unwrap(),
            ])
            .status();
        if !good.exists() {
            fs::write(&good, b"x").unwrap();
        }

        let db = Arc::new(nightjar_db::open(dir.path()).unwrap());
        let pool = test_pool(&db, dir.path());
        let lib = db
            .create_library(&NewLibrary {
                name: "t".into(),
                path: media.to_string_lossy().into_owned(),
                kind: "movies".into(),
            })
            .unwrap();
        let job = start_scan_job(Arc::clone(&db), Arc::clone(&pool), lib.id).unwrap();
        wait_scan(&db, job);

        let item = db.list_items(lib.id).unwrap().into_iter().next().unwrap();
        // Force indexed with clear error as if restart mid-scan left the row.
        db.apply_probe_update(&nightjar_db::ProbeUpdate {
            item_id: item.id,
            duration_ms: None,
            container: None,
            video_codec: None,
            audio_codec: None,
            audio_channels: None,
            width: None,
            height: None,
            video_bitrate_bps: None,
            video_frame_rate_num: None,
            video_frame_rate_den: None,
            hdr: None,
            probe_status: "indexed".into(),
            scan_error: None,
        })
        .unwrap();

        assert!(item.path == "clip.mp4" || item.path.ends_with("clip.mp4"));
        assert!(!item.path.starts_with('/'), "stored path should be relpath");

        let n = pool.drain_pending_probes().unwrap();
        assert_eq!(n, 1);
        for _ in 0..100 {
            let row = db.get_item(item.id).unwrap().unwrap();
            if row.probe_status != "indexed" {
                assert_ne!(
                    row.probe_status, "unavailable",
                    "relpath drain must not ENOENT: {:?}",
                    row.scan_error
                );
                return;
            }
            std::thread::sleep(std::time::Duration::from_millis(50));
        }
        panic!("probe never left indexed");
    }

    #[test]
    fn triggers_during_scan_coalesce_to_one_follow_up() {
        let dir = tempfile::tempdir().unwrap();
        let media = dir.path().join("media");
        fs::create_dir_all(&media).unwrap();
        for i in 0..200 {
            let season = media.join(format!("Show/Season {}", i % 10));
            fs::create_dir_all(&season).unwrap();
            fs::write(season.join(format!("E{i:03}.mp4")), b"x").unwrap();
        }

        let db = Arc::new(nightjar_db::open(dir.path()).unwrap());
        let pool = test_pool(&db, dir.path());
        let lib = db
            .create_library(&NewLibrary {
                name: "t".into(),
                path: media.to_string_lossy().into_owned(),
                kind: "shows".into(),
            })
            .unwrap();

        let job1 = request_scan(
            Arc::clone(&db),
            Arc::clone(&pool),
            lib.id,
            ScanTrigger::Manual,
        )
        .unwrap();

        let mut overlapped = false;
        for _ in 0..400 {
            if db.active_scan_job(lib.id).unwrap() == Some(job1) {
                for _ in 0..8 {
                    let id = request_scan(
                        Arc::clone(&db),
                        Arc::clone(&pool),
                        lib.id,
                        ScanTrigger::Manual,
                    )
                    .unwrap();
                    assert_eq!(id, job1, "must reuse active job");
                }
                overlapped = true;
                break;
            }
            std::thread::sleep(std::time::Duration::from_millis(5));
        }
        assert!(
            overlapped,
            "scan finished before overlap window; enlarge fixture if this flakes"
        );

        // Job1 clearing `active` can race the follow-up spawn: wait until we
        // either see two finished jobs or the follow-up has run and gone idle.
        let mut completed = 0;
        for _ in 0..800 {
            completed = 0;
            for id in job1..job1 + 8 {
                if let Ok(Some(j)) = db.get_scan_job(id)
                    && j.library_id == lib.id
                    && (j.state == "completed" || j.state == "failed")
                {
                    completed += 1;
                }
            }
            let active = db.active_scan_job(lib.id).unwrap();
            if active.is_none() && completed >= 2 {
                break;
            }
            std::thread::sleep(std::time::Duration::from_millis(25));
        }
        assert_eq!(
            completed, 2,
            "one follow-up after coalesced triggers, got {completed}"
        );
    }

    #[test]
    fn symlink_escape_increments_skipped_outside_root() {
        let dir = tempfile::tempdir().unwrap();
        let media = dir.path().join("media");
        let outside = dir.path().join("outside");
        fs::create_dir_all(&media).unwrap();
        fs::create_dir_all(&outside).unwrap();
        let escape_target = outside.join("escaped.mp4");
        fs::write(&escape_target, b"x").unwrap();
        let link = media.join("escaped.mp4");
        #[cfg(unix)]
        std::os::unix::fs::symlink(&escape_target, &link).unwrap();
        #[cfg(not(unix))]
        {
            let _ = (escape_target, link);
            return;
        }
        fs::write(media.join("kept.mp4"), b"y").unwrap();

        let db = Arc::new(nightjar_db::open(dir.path()).unwrap());
        let pool = test_pool(&db, dir.path());
        let lib = db
            .create_library(&NewLibrary {
                name: "t".into(),
                path: media.to_string_lossy().into_owned(),
                kind: "movies".into(),
            })
            .unwrap();
        let job_id = start_scan_job(Arc::clone(&db), Arc::clone(&pool), lib.id).unwrap();

        for _ in 0..200 {
            let job = db.get_scan_job(job_id).unwrap().unwrap();
            if job.state == "completed" || job.state == "failed" {
                assert_eq!(job.state, "completed");
                assert!(
                    job.skipped_outside_root >= 1,
                    "symlink escape should skip, got {}",
                    job.skipped_outside_root
                );
                break;
            }
            std::thread::sleep(std::time::Duration::from_millis(50));
        }
        let items = db.list_items(lib.id).unwrap();
        assert!(items.iter().any(|i| i.path == "kept.mp4"));
        assert!(!items.iter().any(|i| i.path.contains("escaped")));
        let lib = db.get_library(lib.id).unwrap().unwrap();
        assert!(lib.skipped_outside_root >= 1);
    }

    #[test]
    fn sticky_spelling_survives_case_only_walk() {
        let dir = tempfile::tempdir().unwrap();
        let media = dir.path().join("media");
        fs::create_dir_all(&media).unwrap();
        let original = media.join("Title.mp4");
        fs::write(&original, b"x").unwrap();

        let db = Arc::new(nightjar_db::open(dir.path()).unwrap());
        let pool = test_pool(&db, dir.path());
        let lib = db
            .create_library(&NewLibrary {
                name: "t".into(),
                path: media.to_string_lossy().into_owned(),
                kind: "movies".into(),
            })
            .unwrap();
        let job1 = start_scan_job(Arc::clone(&db), Arc::clone(&pool), lib.id).unwrap();
        wait_job(&db, job1);
        let before = db.list_items(lib.id).unwrap();
        assert_eq!(before.len(), 1);
        assert_eq!(before[0].path, "Title.mp4");
        let id = before[0].id;

        // Case-only rename of the directory entry (works on folding and
        // case-sensitive hosts). Sticky spelling must keep the first path.
        let renamed = media.join("title.mp4");
        let _ = fs::rename(&original, &renamed);
        // Bump mtime/size so the index treats it as changed content.
        fs::write(&renamed, b"xy").unwrap();

        let job2 = start_scan_job(Arc::clone(&db), Arc::clone(&pool), lib.id).unwrap();
        wait_job(&db, job2);
        let after = db.list_items(lib.id).unwrap();
        assert_eq!(after.len(), 1);
        assert_eq!(after[0].id, id);
        assert_eq!(after[0].path, "Title.mp4");
    }

    #[test]
    fn fold_collision_refuses_upsert() {
        let dir = tempfile::tempdir().unwrap();
        let media = dir.path().join("media");
        fs::create_dir_all(&media).unwrap();
        fs::write(media.join("a.mp4"), b"x").unwrap();

        let db = Arc::new(nightjar_db::open(dir.path()).unwrap());
        let pool = test_pool(&db, dir.path());
        let lib = db
            .create_library(&NewLibrary {
                name: "t".into(),
                path: media.to_string_lossy().into_owned(),
                kind: "movies".into(),
            })
            .unwrap();
        let job1 = start_scan_job(Arc::clone(&db), Arc::clone(&pool), lib.id).unwrap();
        wait_job(&db, job1);
        assert_eq!(db.list_items(lib.id).unwrap().len(), 1);

        // Corrupt DB: two BINARY-distinct rows that fold-collide.
        db.upsert_items_indexed(
            lib.id,
            &[UpsertItem {
                path: "A.mp4".into(),
                mtime_ms: 1,
                size_bytes: 1,
                title: "A".into(),
                kind: "movie".into(),
                year: None,
                season: None,
                episode: None,
                content_id: None,
            }],
        )
        .unwrap();
        assert_eq!(db.list_items(lib.id).unwrap().len(), 2);

        let job2 = start_scan_job(Arc::clone(&db), Arc::clone(&pool), lib.id).unwrap();
        wait_job(&db, job2);
        // Upsert refused for the colliding fold; both corrupt rows may remain
        // until operator cleanup — walk must not silently pick one.
        let paths: Vec<_> = db
            .list_items(lib.id)
            .unwrap()
            .into_iter()
            .map(|i| i.path)
            .collect();
        assert!(
            paths.contains(&"a.mp4".into()) && paths.contains(&"A.mp4".into()),
            "collision must not collapse rows: {paths:?}"
        );
    }

    fn wait_job(db: &Db, job_id: i64) {
        for _ in 0..200 {
            let job = db.get_scan_job(job_id).unwrap().unwrap();
            if job.state == "completed" || job.state == "failed" {
                assert_eq!(
                    job.state, "completed",
                    "job {job_id}: {:?}",
                    job.error_message
                );
                return;
            }
            std::thread::sleep(std::time::Duration::from_millis(50));
        }
        panic!("job {job_id} did not finish");
    }

    #[test]
    fn hint_ingest_upserts_media_and_skips_non_media() {
        let dir = tempfile::tempdir().unwrap();
        let media = dir.path().join("media");
        fs::create_dir_all(&media).unwrap();
        let ep = media.join("Show.S01E01.mkv");
        fs::write(&ep, b"not empty").unwrap();
        fs::write(media.join("notes.txt"), b"x").unwrap();
        fs::write(media.join("Show.S01E01.en.srt"), b"1\n").unwrap();

        let db = Arc::new(nightjar_db::open(dir.path()).unwrap());
        let pool = test_pool(&db, dir.path());
        let lib = db
            .create_library(&NewLibrary {
                name: "t".into(),
                path: media.to_string_lossy().into_owned(),
                kind: "shows".into(),
            })
            .unwrap();

        assert_eq!(
            hint_ingest(db.as_ref(), pool.as_ref(), lib.id, &media.join("notes.txt")).unwrap(),
            HintIngestOutcome::Ignored
        );
        assert_eq!(
            hint_ingest(
                db.as_ref(),
                pool.as_ref(),
                lib.id,
                &media.join("Show.S01E01.en.srt")
            )
            .unwrap(),
            HintIngestOutcome::Ignored
        );
        let out = hint_ingest(db.as_ref(), pool.as_ref(), lib.id, &ep).unwrap();
        let HintIngestOutcome::Upserted { item_id } = out else {
            panic!("expected Upserted, got {out:?}");
        };
        assert!(item_id > 0);
        let items = db.list_items(lib.id).unwrap();
        assert_eq!(items.len(), 1);
        assert_eq!(items[0].path, "Show.S01E01.mkv");
        assert_eq!(items[0].probe_status, "indexed");

        // Zero-size skipped (copy-in-progress).
        let empty = media.join("empty.mp4");
        fs::write(&empty, b"").unwrap();
        assert_eq!(
            hint_ingest(db.as_ref(), pool.as_ref(), lib.id, &empty).unwrap(),
            HintIngestOutcome::Ignored
        );
        assert_eq!(db.list_items(lib.id).unwrap().len(), 1);
    }

    #[test]
    fn hint_ingest_unchanged_mtime_and_update_on_change() {
        let dir = tempfile::tempdir().unwrap();
        let media = dir.path().join("media");
        fs::create_dir_all(&media).unwrap();
        let path = media.join("clip.mp4");
        fs::write(&path, b"v1").unwrap();

        let db = Arc::new(nightjar_db::open(dir.path()).unwrap());
        let pool = test_pool(&db, dir.path());
        let lib = db
            .create_library(&NewLibrary {
                name: "t".into(),
                path: media.to_string_lossy().into_owned(),
                kind: "movies".into(),
            })
            .unwrap();

        let first = hint_ingest(db.as_ref(), pool.as_ref(), lib.id, &path).unwrap();
        let HintIngestOutcome::Upserted { item_id } = first else {
            panic!("expected Upserted, got {first:?}");
        };
        let second = hint_ingest(db.as_ref(), pool.as_ref(), lib.id, &path).unwrap();
        assert_eq!(second, HintIngestOutcome::Unchanged { item_id });
        assert_eq!(db.list_items(lib.id).unwrap().len(), 1);

        // Bump mtime/size so the short-circuit does not apply.
        std::thread::sleep(std::time::Duration::from_millis(20));
        fs::write(&path, b"v2-longer").unwrap();
        let third = hint_ingest(db.as_ref(), pool.as_ref(), lib.id, &path).unwrap();
        let HintIngestOutcome::Upserted { item_id: id2 } = third else {
            panic!("expected Upserted after mtime change, got {third:?}");
        };
        assert_eq!(id2, item_id, "same path must keep media_items.id");
        let row = db.get_item(item_id).unwrap().unwrap();
        // `hint_ingest` enqueues a background probe on every upsert (line
        // ~240), including this one, against a real `test_pool` with live
        // worker threads. Under load that probe can dequeue, spawn ffprobe
        // against the 9-byte garbage content, fail, and land `error` before
        // this read runs — a real race, not a bug in the assertion below it.
        // Both `indexed` (probe has not landed yet) and `error` (it has, and
        // correctly rejected non-media) are the only outcomes reachable here;
        // anything else means stale data leaked through the upsert, which is
        // the actual invariant this test protects.
        assert!(
            matches!(row.probe_status.as_str(), "indexed" | "error"),
            "unexpected probe_status after mtime-changed upsert: {}",
            row.probe_status
        );
        assert_eq!(row.path, "clip.mp4");
        assert_eq!(db.list_items(lib.id).unwrap().len(), 1);
    }

    #[test]
    fn hint_ingest_fold_collision_refuses() {
        let dir = tempfile::tempdir().unwrap();
        let media = dir.path().join("media");
        fs::create_dir_all(&media).unwrap();
        fs::write(media.join("a.mp4"), b"data").unwrap();

        let db = Arc::new(nightjar_db::open(dir.path()).unwrap());
        let pool = test_pool(&db, dir.path());
        let lib = db
            .create_library(&NewLibrary {
                name: "t".into(),
                path: media.to_string_lossy().into_owned(),
                kind: "movies".into(),
            })
            .unwrap();
        let job = start_scan_job(Arc::clone(&db), Arc::clone(&pool), lib.id).unwrap();
        wait_job(&db, job);

        db.upsert_items_indexed(
            lib.id,
            &[UpsertItem {
                path: "A.mp4".into(),
                mtime_ms: 1,
                size_bytes: 1,
                title: "A".into(),
                kind: "movie".into(),
                year: None,
                season: None,
                episode: None,
                content_id: None,
            }],
        )
        .unwrap();
        assert_eq!(db.list_items(lib.id).unwrap().len(), 2);

        let out = hint_ingest(db.as_ref(), pool.as_ref(), lib.id, &media.join("a.mp4")).unwrap();
        assert_eq!(out, HintIngestOutcome::Collision);
        assert_eq!(db.list_items(lib.id).unwrap().len(), 2);
    }

    #[test]
    fn hint_ingest_does_not_delete_missing_rows() {
        let dir = tempfile::tempdir().unwrap();
        let media = dir.path().join("media");
        fs::create_dir_all(&media).unwrap();
        fs::write(media.join("keep.mp4"), b"data").unwrap();
        fs::write(media.join("gone.mp4"), b"data").unwrap();

        let db = Arc::new(nightjar_db::open(dir.path()).unwrap());
        let pool = test_pool(&db, dir.path());
        let lib = db
            .create_library(&NewLibrary {
                name: "t".into(),
                path: media.to_string_lossy().into_owned(),
                kind: "movies".into(),
            })
            .unwrap();
        let job = start_scan_job(Arc::clone(&db), Arc::clone(&pool), lib.id).unwrap();
        wait_job(&db, job);
        assert_eq!(db.list_items(lib.id).unwrap().len(), 2);

        fs::remove_file(media.join("gone.mp4")).unwrap();
        let new_ep = media.join("new.mp4");
        fs::write(&new_ep, b"fresh").unwrap();
        // Hint only — no full scan. gone.mp4 must remain in DB.
        hint_ingest(db.as_ref(), pool.as_ref(), lib.id, &new_ep).unwrap();
        let paths: std::collections::HashSet<_> = db
            .list_items(lib.id)
            .unwrap()
            .into_iter()
            .map(|i| i.path)
            .collect();
        assert!(
            paths.contains("gone.mp4"),
            "hint must not delete_missing: {paths:?}"
        );
        assert!(paths.contains("new.mp4"), "hint must upsert: {paths:?}");
        assert!(paths.contains("keep.mp4"));
        assert_eq!(paths.len(), 3);
    }

    /// A hint that upserts while a walk is active must set `dirty_add`, so that
    /// walk skips `delete_missing` (the row is outside its keep-set) and the row
    /// survives; it must not schedule a follow-up walk. The delete hold parks
    /// the walk at its delete decision, so the hint lands while the walk is
    /// provably active and before the walk reads the marker. Without the hold
    /// the test races the walk's completion: the hint's own `active_scan_job`
    /// read can see a job that already finished.
    #[test]
    fn hint_during_active_scan_sets_dirty_add_not_follow_up() {
        let dir = tempfile::tempdir().unwrap();
        let media = dir.path().join("media");
        fs::create_dir_all(&media).unwrap();
        for i in 0..80 {
            fs::write(media.join(format!("f{i:03}.mp4")), b"x").unwrap();
        }

        let db = Arc::new(nightjar_db::open(dir.path()).unwrap());
        let pool = test_pool(&db, dir.path());
        let lib = db
            .create_library(&NewLibrary {
                name: "t".into(),
                path: media.to_string_lossy().into_owned(),
                kind: "movies".into(),
            })
            .unwrap();

        let hold = pool.arm_delete_hold();
        let job1 = request_scan(
            Arc::clone(&db),
            Arc::clone(&pool),
            lib.id,
            ScanTrigger::Manual,
        )
        .unwrap();
        assert_eq!(
            hold.entered_rx
                .recv_timeout(std::time::Duration::from_secs(5))
                .expect("the walk must reach its delete decision"),
            lib.id,
            "the held walk is this library's"
        );

        let late = media.join("late.mp4");
        fs::write(&late, b"late").unwrap();
        let out = hint_ingest(db.as_ref(), pool.as_ref(), lib.id, &late).unwrap();
        let HintIngestOutcome::Upserted { .. } = out else {
            panic!("the hint must upsert the new file: {out:?}");
        };
        assert!(
            pool.is_dirty_add(lib.id),
            "upsert hint during active scan must set dirty_add"
        );
        assert!(
            !pool.is_scan_dirty(lib.id),
            "hint must not set manual follow-up dirty"
        );
        // Poll while active is a dirty no-op.
        assert_eq!(
            request_scan(
                Arc::clone(&db),
                Arc::clone(&pool),
                lib.id,
                ScanTrigger::Poll
            )
            .unwrap(),
            job1
        );
        assert!(
            !pool.is_scan_dirty(lib.id),
            "poll while active must not set scan_dirty"
        );

        hold.release_tx.send(()).unwrap();
        wait_job(&db, job1);
        // No automatic follow-up from hint-only dirt.
        std::thread::sleep(std::time::Duration::from_millis(100));
        assert!(
            db.active_scan_job(lib.id).unwrap().is_none(),
            "hint dirt must not schedule a follow-up scan"
        );
        let paths: Vec<_> = db
            .list_items(lib.id)
            .unwrap()
            .into_iter()
            .map(|i| i.path)
            .collect();
        assert!(
            paths.iter().any(|p| p == "late.mp4"),
            "hinted file must survive: {paths:?}"
        );
    }

    /// The dirty_add marker is set before the hint's probe is enqueued, so an
    /// active walk's delete_missing cannot race the upsert-to-marker window.
    /// The armed hold proves the marker is already set when the probe reports
    /// that it started.
    #[test]
    fn hint_marks_dirty_add_before_probe_capture() {
        let fixture = corpus_fixture("h264_aac_srt_mkv.mkv");
        if skip_without_fixture(&fixture) {
            return;
        }
        let dir = tempfile::tempdir().unwrap();
        let media = dir.path().join("media");
        fs::create_dir_all(&media).unwrap();
        let late = media.join("late.mkv");
        fs::copy(&fixture, &late).unwrap();

        let db = Arc::new(nightjar_db::open(dir.path()).unwrap());
        let pool = test_pool(&db, dir.path());
        let lib = db
            .create_library(&NewLibrary {
                name: "t".into(),
                path: media.to_string_lossy().into_owned(),
                kind: "movies".into(),
            })
            .unwrap();
        // A queued job is active for `active_scan_job`; no walk needs to run.
        let _job = db.create_scan_job(lib.id).unwrap();

        let hold = pool.arm_probe_hold();
        let out = hint_ingest(db.as_ref(), pool.as_ref(), lib.id, &late).unwrap();
        let HintIngestOutcome::Upserted { item_id } = out else {
            panic!("the hint must upsert the new file: {out:?}");
        };
        let entered = hold
            .entered_rx
            .recv_timeout(std::time::Duration::from_secs(5))
            .expect("the hinted probe must report that it started");
        assert_eq!(entered, item_id, "the held probe is the hinted item");
        assert!(
            pool.is_dirty_add(lib.id),
            "the dirty_add marker must precede the captured probe"
        );
        assert!(
            !pool.is_scan_dirty(lib.id),
            "the hint must not set manual follow-up dirty"
        );

        hold.release_tx.send(()).unwrap();
        let mut released = false;
        for _ in 0..400 {
            if pool.probe_slot_count() == 0 {
                released = true;
                break;
            }
            std::thread::sleep(std::time::Duration::from_millis(5));
        }
        assert!(released, "the hinted probe must release its slot");
        assert_eq!(pool.probe_slot_count(), 0);
    }

    #[test]
    fn poll_while_active_does_not_suppress_delete_missing() {
        let dir = tempfile::tempdir().unwrap();
        let media = dir.path().join("media");
        fs::create_dir_all(&media).unwrap();
        fs::write(media.join("keep.mp4"), b"data").unwrap();
        fs::write(media.join("gone.mp4"), b"data").unwrap();

        let db = Arc::new(nightjar_db::open(dir.path()).unwrap());
        let pool = test_pool(&db, dir.path());
        let lib = db
            .create_library(&NewLibrary {
                name: "t".into(),
                path: media.to_string_lossy().into_owned(),
                kind: "movies".into(),
            })
            .unwrap();
        let job0 = start_scan_job(Arc::clone(&db), Arc::clone(&pool), lib.id).unwrap();
        wait_job(&db, job0);
        assert_eq!(db.list_items(lib.id).unwrap().len(), 2);

        fs::remove_file(media.join("gone.mp4")).unwrap();
        // Many files so the walk stays active long enough for a mid-walk poll.
        for i in 0..120 {
            fs::write(media.join(format!("extra{i:03}.mp4")), b"x").unwrap();
        }

        let job1 = request_scan(
            Arc::clone(&db),
            Arc::clone(&pool),
            lib.id,
            ScanTrigger::Manual,
        )
        .unwrap();
        let mut polled = false;
        for _ in 0..500 {
            if db.active_scan_job(lib.id).unwrap() == Some(job1) {
                let id = request_scan(
                    Arc::clone(&db),
                    Arc::clone(&pool),
                    lib.id,
                    ScanTrigger::Poll,
                )
                .unwrap();
                assert_eq!(id, job1);
                assert!(!pool.is_scan_dirty(lib.id));
                assert!(!pool.is_dirty_add(lib.id));
                polled = true;
                break;
            }
            std::thread::sleep(std::time::Duration::from_millis(5));
        }
        assert!(polled, "job finished before mid-walk poll");

        wait_job(&db, job1);
        let job1_row = db.get_scan_job(job1).unwrap().unwrap();
        assert!(
            job1_row.removed >= 1,
            "poll-while-active must not suppress delete_missing; removed={}",
            job1_row.removed
        );
        let paths: std::collections::HashSet<_> = db
            .list_items(lib.id)
            .unwrap()
            .into_iter()
            .map(|i| i.path)
            .collect();
        assert!(
            !paths.contains("gone.mp4"),
            "gone.mp4 must be deleted: {paths:?}"
        );
    }

    #[test]
    fn manual_scan_while_active_still_coalesces_follow_up() {
        let dir = tempfile::tempdir().unwrap();
        let media = dir.path().join("media");
        fs::create_dir_all(&media).unwrap();
        for i in 0..100 {
            fs::write(media.join(format!("f{i:03}.mp4")), b"x").unwrap();
        }

        let db = Arc::new(nightjar_db::open(dir.path()).unwrap());
        let pool = test_pool(&db, dir.path());
        let lib = db
            .create_library(&NewLibrary {
                name: "t".into(),
                path: media.to_string_lossy().into_owned(),
                kind: "movies".into(),
            })
            .unwrap();

        let job1 = request_scan(
            Arc::clone(&db),
            Arc::clone(&pool),
            lib.id,
            ScanTrigger::Manual,
        )
        .unwrap();
        let mut overlapped = false;
        for _ in 0..400 {
            if db.active_scan_job(lib.id).unwrap() == Some(job1) {
                assert_eq!(
                    request_scan(
                        Arc::clone(&db),
                        Arc::clone(&pool),
                        lib.id,
                        ScanTrigger::Manual
                    )
                    .unwrap(),
                    job1
                );
                assert!(pool.is_scan_dirty(lib.id));
                overlapped = true;
                break;
            }
            std::thread::sleep(std::time::Duration::from_millis(5));
        }
        assert!(overlapped);

        let mut completed = 0;
        for _ in 0..800 {
            completed = 0;
            for id in job1..job1 + 8 {
                if let Ok(Some(j)) = db.get_scan_job(id)
                    && j.library_id == lib.id
                    && (j.state == "completed" || j.state == "failed")
                {
                    completed += 1;
                }
            }
            if db.active_scan_job(lib.id).unwrap().is_none() && completed >= 2 {
                break;
            }
            std::thread::sleep(std::time::Duration::from_millis(25));
        }
        assert_eq!(completed, 2, "manual dirty must spawn one follow-up");
    }

    /// CHK-FC: an in-place edit keeps the parent directory mtime, so a
    /// cache-mtime poll would reuse its listing and miss the change. The poll
    /// now re-lists every directory, so it must observe the new size. The
    /// restored parent mtime is asserted, so the test cannot pass because the
    /// directory happened to bump.
    #[cfg(unix)]
    #[test]
    fn poll_observes_in_place_edit_with_unchanged_parent_mtime() {
        let dir = tempfile::tempdir().unwrap();
        let media = dir.path().join("media");
        fs::create_dir_all(&media).unwrap();
        let file = media.join("Movie.mp4");
        fs::write(&file, b"v1").unwrap();

        let db = Arc::new(nightjar_db::open(dir.path()).unwrap());
        let pool = test_pool(&db, dir.path());
        let lib = db
            .create_library(&NewLibrary {
                name: "t".into(),
                path: media.to_string_lossy().into_owned(),
                kind: "movies".into(),
            })
            .unwrap();

        let job0 = start_scan_job(Arc::clone(&db), Arc::clone(&pool), lib.id).unwrap();
        wait_job(&db, job0);
        assert_eq!(db.list_items(lib.id).unwrap()[0].size_bytes, 2);

        // In-place edit: the file mtime moves, the parent mtime is put back.
        let dir_mtime = fs::metadata(&media).unwrap().modified().unwrap();
        std::thread::sleep(Duration::from_millis(1100));
        fs::write(&file, b"v2-longer").unwrap();
        std::fs::File::open(&media)
            .unwrap()
            .set_times(fs::FileTimes::new().set_modified(dir_mtime))
            .unwrap();
        assert_eq!(
            fs::metadata(&media).unwrap().modified().unwrap(),
            dir_mtime,
            "setup: the parent mtime must be unchanged"
        );

        let poll = request_scan(
            Arc::clone(&db),
            Arc::clone(&pool),
            lib.id,
            ScanTrigger::Poll,
        )
        .unwrap();
        wait_job(&db, poll);
        let poll_row = db.get_scan_job(poll).unwrap().unwrap();
        assert_eq!(
            poll_row.updated, 1,
            "a fresh poll must observe the in-place edit"
        );
        assert_eq!(db.list_items(lib.id).unwrap()[0].size_bytes, 9);
    }

    /// CHK-FC: a poll must reconcile sidecar membership for unchanged media
    /// even when the parent directory mtimes are preserved. Adjacent add/edit/
    /// remove and a nested add are all observed by the one fresh poll.
    #[cfg(unix)]
    #[test]
    fn poll_reconciles_sidecar_membership_under_unchanged_parent_mtime() {
        let tmp = tempfile::tempdir().unwrap();
        let dir = fs::canonicalize(tmp.path()).unwrap();
        let media = dir.join("media");
        let subs = media.join("Subs");
        fs::create_dir_all(&subs).unwrap();
        fs::write(media.join("Movie.mp4"), b"not a real mp4").unwrap();
        let edited = media.join("Movie.en.srt");
        fs::write(&edited, b"1\n00:00:00,000 --> 00:00:01,000\nHi\n").unwrap();
        let removed = media.join("Movie.es.srt");
        fs::write(&removed, b"1\n00:00:00,000 --> 00:00:01,000\nHola\n").unwrap();
        fs::write(
            subs.join("Movie.fr.srt"),
            b"1\n00:00:00,000 --> 00:00:01,000\nSalut\n",
        )
        .unwrap();

        let db = Arc::new(nightjar_db::open(&dir).unwrap());
        let pool = test_pool(&db, &dir);
        let lib = db
            .create_library(&NewLibrary {
                name: "t".into(),
                path: media.to_string_lossy().into_owned(),
                kind: "movies".into(),
            })
            .unwrap();
        wait_job(
            &db,
            start_scan_job(Arc::clone(&db), Arc::clone(&pool), lib.id).unwrap(),
        );
        let item_id = db.list_items(lib.id).unwrap()[0].id;
        assert_eq!(
            db.list_item_sidecars(item_id).unwrap().len(),
            3,
            "setup: three sidecars associated"
        );
        let en_before = db.get_item_sidecar(item_id, "s-en").unwrap().unwrap();

        // Mutate with both parent directory mtimes restored, so only the fresh
        // poll's relist can surface the change.
        let media_mtime = fs::metadata(&media).unwrap().modified().unwrap();
        let subs_mtime = fs::metadata(&subs).unwrap().modified().unwrap();
        std::thread::sleep(Duration::from_millis(1100));
        fs::write(&edited, b"1\n00:00:00,000 --> 00:00:02,000\nYo\n").unwrap();
        fs::remove_file(&removed).unwrap();
        fs::write(
            media.join("Movie.it.srt"),
            b"1\n00:00:00,000 --> 00:00:01,000\nCiao\n",
        )
        .unwrap();
        fs::write(
            subs.join("Movie.nl.srt"),
            b"1\n00:00:00,000 --> 00:00:01,000\nHoi\n",
        )
        .unwrap();
        fs::File::open(&media)
            .unwrap()
            .set_times(fs::FileTimes::new().set_modified(media_mtime))
            .unwrap();
        fs::File::open(&subs)
            .unwrap()
            .set_times(fs::FileTimes::new().set_modified(subs_mtime))
            .unwrap();

        let poll = request_scan(
            Arc::clone(&db),
            Arc::clone(&pool),
            lib.id,
            ScanTrigger::Poll,
        )
        .unwrap();
        wait_job(&db, poll);

        let mut ids: Vec<String> = db
            .list_item_sidecars(item_id)
            .unwrap()
            .into_iter()
            .map(|s| s.track_id)
            .collect();
        ids.sort();
        assert_eq!(
            ids,
            vec!["s-Subs.fr", "s-Subs.nl", "s-en", "s-it"],
            "one poll must reconcile the exact current sidecar set"
        );
        assert!(
            db.get_item_sidecar(item_id, "s-es").unwrap().is_none(),
            "a removed sidecar leaves no row"
        );
        let en_after = db.get_item_sidecar(item_id, "s-en").unwrap().unwrap();
        assert_ne!(
            en_after.content_id, en_before.content_id,
            "the edited sidecar must persist its new content identity"
        );
        assert_ne!(
            en_after.mtime_ms, en_before.mtime_ms,
            "the edited sidecar must persist its new mtime"
        );
        assert!(
            en_after.sidecar_generation.unwrap() > en_before.sidecar_generation.unwrap(),
            "the edited sidecar must receive a new generation"
        );
    }

    /// CHK-FC: the fresh poll still compares the observed `(mtime, size)` tuple,
    /// so an unchanged repeat enqueues no probe. The first pass is the positive
    /// control: it enqueued one.
    #[test]
    fn unchanged_poll_enqueues_zero_probes() {
        if !require_ffprobe() {
            eprintln!("skip: ffprobe not on PATH");
            return;
        }
        let dir = tempfile::tempdir().unwrap();
        let media = dir.path().join("media");
        fs::create_dir_all(&media).unwrap();
        fs::write(media.join("Movie.mp4"), b"not a real mp4").unwrap();

        let db = Arc::new(nightjar_db::open(dir.path()).unwrap());
        let pool = test_pool(&db, dir.path());
        let lib = db
            .create_library(&NewLibrary {
                name: "t".into(),
                path: media.to_string_lossy().into_owned(),
                kind: "movies".into(),
            })
            .unwrap();

        let first = start_scan_job(Arc::clone(&db), Arc::clone(&pool), lib.id).unwrap();
        wait_job(&db, first);
        let first_row = db.get_scan_job(first).unwrap().unwrap();
        assert!(
            first_row.probed >= 1,
            "the first pass must probe the new item"
        );

        let second = request_scan(
            Arc::clone(&db),
            Arc::clone(&pool),
            lib.id,
            ScanTrigger::Poll,
        )
        .unwrap();
        wait_job(&db, second);
        let second_row = db.get_scan_job(second).unwrap().unwrap();
        assert_eq!(second_row.unchanged, 1, "the tuple is unchanged");
        assert_eq!(
            second_row.probed, 0,
            "an unchanged tuple must enqueue no probe"
        );
    }

    /// SCAN-D2A: a replacement that restores the file mtime but changes the
    /// size must count as updated. The mtime alone cannot see it; the size in
    /// the item comparison can. The repeat scan is the negative control: an
    /// unchanged (mtime, size) pair is `unchanged`, not rewritten.
    #[test]
    fn replacement_with_restored_mtime_and_new_size_is_updated() {
        let dir = tempfile::tempdir().unwrap();
        let media = dir.path().join("media");
        fs::create_dir_all(&media).unwrap();
        let file = media.join("Movie.mp4");
        fs::write(&file, b"v1").unwrap();

        let db = Arc::new(nightjar_db::open(dir.path()).unwrap());
        let pool = test_pool(&db, dir.path());
        let lib = db
            .create_library(&NewLibrary {
                name: "t".into(),
                path: media.to_string_lossy().into_owned(),
                kind: "movies".into(),
            })
            .unwrap();

        let job0 = start_scan_job(Arc::clone(&db), Arc::clone(&pool), lib.id).unwrap();
        wait_job(&db, job0);
        let before = db.list_items(lib.id).unwrap();
        assert_eq!(before.len(), 1);
        assert_eq!(before[0].size_bytes, 2);
        let before_mtime = before[0].mtime_ms;

        // Replace with longer content, then put the original mtime back.
        fs::write(&file, b"v2-longer").unwrap();
        std::fs::File::open(&file)
            .unwrap()
            .set_times(
                fs::FileTimes::new().set_modified(
                    std::time::UNIX_EPOCH + Duration::from_millis(before_mtime as u64),
                ),
            )
            .unwrap();
        let restored_mtime = fs::metadata(&file)
            .unwrap()
            .modified()
            .unwrap()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_millis() as i64;
        assert_eq!(
            restored_mtime, before_mtime,
            "setup: the file mtime must match the indexed value"
        );

        let manual = start_scan_job(Arc::clone(&db), Arc::clone(&pool), lib.id).unwrap();
        wait_job(&db, manual);
        let row = db.get_scan_job(manual).unwrap().unwrap();
        assert_eq!(row.updated, 1, "a size change at the same mtime is updated");
        assert_eq!(db.list_items(lib.id).unwrap()[0].size_bytes, 9);

        // Same (mtime, size) again: unchanged, no rewrite.
        let repeat = start_scan_job(Arc::clone(&db), Arc::clone(&pool), lib.id).unwrap();
        wait_job(&db, repeat);
        let row = db.get_scan_job(repeat).unwrap().unwrap();
        assert_eq!(row.updated, 0, "unchanged (mtime, size) must not rewrite");
        assert_eq!(row.unchanged, 1);
    }

    /// CHK-FC: a manual request that coalesces onto an active poll must still
    /// produce one follow-up job. The active poll now walks fresh, so it
    /// observes the in-place edit; the follow-up then runs and confirms the
    /// tuple unchanged. Coalescing must not drop the dirty bit.
    #[cfg(unix)]
    #[test]
    fn manual_during_active_scan_coalesces_follow_up() {
        let dir = tempfile::tempdir().unwrap();
        let media = dir.path().join("media");
        fs::create_dir_all(&media).unwrap();
        let target = media.join("target.mp4");
        fs::write(&target, b"v1").unwrap();
        for i in 0..100 {
            fs::write(media.join(format!("f{i:03}.mp4")), b"x").unwrap();
        }

        let db = Arc::new(nightjar_db::open(dir.path()).unwrap());
        let pool = test_pool(&db, dir.path());
        let lib = db
            .create_library(&NewLibrary {
                name: "t".into(),
                path: media.to_string_lossy().into_owned(),
                kind: "movies".into(),
            })
            .unwrap();

        // Seed the cache (manual scan) and index the target.
        let job0 = start_scan_job(Arc::clone(&db), Arc::clone(&pool), lib.id).unwrap();
        wait_job(&db, job0);
        assert_eq!(
            db.list_items(lib.id)
                .unwrap()
                .into_iter()
                .find(|i| i.path == "target.mp4")
                .unwrap()
                .size_bytes,
            2
        );

        // Hold the index epoch so the active poll cannot run its walk while we
        // edit the file and coalesce a manual request onto it.
        let epoch = pool.enter_index_epoch(lib.id);
        let job1 = request_scan(
            Arc::clone(&db),
            Arc::clone(&pool),
            lib.id,
            ScanTrigger::Poll,
        )
        .unwrap();

        let dir_mtime = fs::metadata(&media).unwrap().modified().unwrap();
        fs::write(&target, b"v2-longer").unwrap();
        std::fs::File::open(&media)
            .unwrap()
            .set_times(fs::FileTimes::new().set_modified(dir_mtime))
            .unwrap();
        let coalesced = request_scan(
            Arc::clone(&db),
            Arc::clone(&pool),
            lib.id,
            ScanTrigger::Manual,
        )
        .unwrap();
        assert_eq!(coalesced, job1, "manual during active must coalesce");
        assert!(
            pool.is_scan_dirty(lib.id),
            "coalescing must arm the dirty bit"
        );
        drop(epoch);

        wait_job(&db, job1);
        let job1_row = db.get_scan_job(job1).unwrap().unwrap();
        assert_eq!(
            job1_row.updated, 1,
            "the fresh poll must observe the in-place edit"
        );

        // The coalesced follow-up must exist and complete, proving the dirty
        // bit was consumed rather than dropped.
        let follow = job1 + 1;
        let mut follow_row = None;
        for _ in 0..400 {
            if let Ok(Some(j)) = db.get_scan_job(follow)
                && (j.state == "completed" || j.state == "failed")
            {
                follow_row = Some(j);
                break;
            }
            std::thread::sleep(Duration::from_millis(25));
        }
        let follow_row = follow_row.expect("coalesced follow-up job must exist");
        assert_eq!(
            follow_row.state, "completed",
            "{:?}",
            follow_row.error_message
        );
        assert_eq!(
            follow_row.updated, 0,
            "the follow-up re-observes an already-updated tuple as unchanged"
        );
        assert_eq!(
            db.list_items(lib.id)
                .unwrap()
                .into_iter()
                .find(|i| i.path == "target.mp4")
                .unwrap()
                .size_bytes,
            9
        );
    }

    #[test]
    fn create_then_request_scan_returns_job_without_blocking_on_probe() {
        let dir = tempfile::tempdir().unwrap();
        let media = dir.path().join("media");
        fs::create_dir_all(&media).unwrap();
        fs::write(media.join("A.mp4"), b"x").unwrap();

        let db = Arc::new(nightjar_db::open(dir.path()).unwrap());
        let pool = test_pool(&db, dir.path());
        let lib = db
            .create_library(&NewLibrary {
                name: "t".into(),
                path: media.to_string_lossy().into_owned(),
                kind: "movies".into(),
            })
            .unwrap();

        let t0 = std::time::Instant::now();
        let job_id = request_scan(
            Arc::clone(&db),
            Arc::clone(&pool),
            lib.id,
            ScanTrigger::Manual,
        )
        .unwrap();
        assert!(
            t0.elapsed() < std::time::Duration::from_secs(2),
            "request_scan must return before the walk finishes"
        );
        assert!(job_id > 0);
        let job = db.get_scan_job(job_id).unwrap().unwrap();
        assert_eq!(job.library_id, lib.id);
    }

    /// A3.2: if the worker-thread spawn fails after the `queued` row was
    /// inserted, the row must land `failed` immediately rather than wedge the
    /// library until restart (Rule 4.8). Forced deterministically with
    /// `stack_size(usize::MAX)` (plan Decisions); covers both the `scan` kind
    /// (`request_scan`'s row) and the `repoint` kind (`request_repoint`'s row)
    /// through the shared [`spawn_job_worker`] spawn path.
    #[test]
    fn spawn_failure_fails_queued_job_not_left_wedged() {
        let dir = tempfile::tempdir().unwrap();
        let db = Arc::new(nightjar_db::open(dir.path()).unwrap());
        let lib = db
            .create_library(&NewLibrary {
                name: "t".into(),
                path: dir.path().join("media").to_string_lossy().into_owned(),
                kind: "movies".into(),
            })
            .unwrap();

        // `request_scan` path: a queued scan row, then a failing spawn.
        let scan_job_id = db.create_scan_job(lib.id).unwrap();
        let err = spawn_job_worker(&db, scan_job_id, "scan", Some(usize::MAX), || {}).unwrap_err();
        assert!(err.starts_with("spawn scan job"), "{err}");
        let job = db.get_scan_job(scan_job_id).unwrap().unwrap();
        assert_eq!(
            job.state, "failed",
            "spawn failure must fail the queued scan row, not leave it queued"
        );

        // `request_repoint` path: a queued repoint row, then a failing spawn.
        let repoint_job_id = db.create_repoint_job(lib.id, &lib.path).unwrap();
        let err =
            spawn_job_worker(&db, repoint_job_id, "repoint", Some(usize::MAX), || {}).unwrap_err();
        assert!(err.starts_with("spawn repoint job"), "{err}");
        let job = db.get_scan_job(repoint_job_id).unwrap().unwrap();
        assert_eq!(
            job.state, "failed",
            "spawn failure must fail the queued repoint row, not leave it queued"
        );
    }

    #[test]
    fn repoint_holdoff_blocks_poll_not_manual() {
        let dir = tempfile::tempdir().unwrap();
        let media = dir.path().join("media");
        fs::create_dir_all(&media).unwrap();
        fs::write(media.join("a.mp4"), b"data").unwrap();

        let db = Arc::new(nightjar_db::open(dir.path()).unwrap());
        let pool = test_pool(&db, dir.path());
        let lib = db
            .create_library(&NewLibrary {
                name: "t".into(),
                path: media.to_string_lossy().into_owned(),
                kind: "movies".into(),
            })
            .unwrap();
        let job0 = start_scan_job(Arc::clone(&db), Arc::clone(&pool), lib.id).unwrap();
        wait_job(&db, job0);

        pool.set_repoint_delete_holdoff(lib.id, Duration::from_secs(3600));
        assert!(pool.repoint_delete_holdoff_active(lib.id));

        let polled = request_scan(
            Arc::clone(&db),
            Arc::clone(&pool),
            lib.id,
            ScanTrigger::Poll,
        )
        .unwrap();
        assert_eq!(polled, 0, "poll must no-op under holdoff");
        assert!(
            db.active_scan_job(lib.id).unwrap().is_none(),
            "poll must not start a job under holdoff"
        );

        let manual = request_scan(
            Arc::clone(&db),
            Arc::clone(&pool),
            lib.id,
            ScanTrigger::Manual,
        )
        .unwrap();
        assert!(manual > 0);
        wait_job(&db, manual);
        assert!(
            !pool.repoint_delete_holdoff_active(lib.id),
            "successful ordinary scan clears holdoff"
        );

        let after = request_scan(
            Arc::clone(&db),
            Arc::clone(&pool),
            lib.id,
            ScanTrigger::Poll,
        )
        .unwrap();
        assert!(after > 0, "poll works again after holdoff clear");
        wait_job(&db, after);
    }

    /// ADR-0059: every trigger coalesces onto a held active job, and only
    /// Manual/Create set the dirty bit. The held row is inserted directly, so
    /// the test is deterministic and no worker runs.
    #[test]
    fn each_trigger_coalesces_onto_a_held_active_job() {
        let dir = tempfile::tempdir().unwrap();
        let media = dir.path().join("media");
        fs::create_dir_all(&media).unwrap();
        fs::write(media.join("a.mp4"), b"data").unwrap();

        let db = Arc::new(nightjar_db::open(dir.path()).unwrap());
        let pool = test_pool(&db, dir.path());
        let lib = db
            .create_library(&NewLibrary {
                name: "t".into(),
                path: media.to_string_lossy().into_owned(),
                kind: "movies".into(),
            })
            .unwrap();
        let held = db.create_scan_job(lib.id).unwrap();

        for (trigger, marks_dirty) in [
            (ScanTrigger::Poll, false),
            (ScanTrigger::FollowUp, false),
            (ScanTrigger::Manual, true),
            (ScanTrigger::Create, true),
        ] {
            let _ = pool.take_scan_dirty(lib.id);
            let id = request_scan(Arc::clone(&db), Arc::clone(&pool), lib.id, trigger).unwrap();
            assert_eq!(id, held, "{trigger:?} must coalesce onto the active job");
            assert_eq!(
                pool.is_scan_dirty(lib.id),
                marks_dirty,
                "{trigger:?} dirty-bit behavior"
            );
        }
        assert_eq!(
            db.latest_scan_job(lib.id).unwrap().unwrap().id,
            held,
            "no trigger may insert a second row while one is active"
        );
    }

    /// ADR-0059: a poll under holdoff inserts no row, and a row is the only
    /// thing that can spawn a worker, so it starts none.
    #[test]
    fn poll_holdoff_inserts_no_row() {
        let dir = tempfile::tempdir().unwrap();
        let media = dir.path().join("media");
        fs::create_dir_all(&media).unwrap();
        fs::write(media.join("a.mp4"), b"data").unwrap();

        let db = Arc::new(nightjar_db::open(dir.path()).unwrap());
        let pool = test_pool(&db, dir.path());
        let lib = db
            .create_library(&NewLibrary {
                name: "t".into(),
                path: media.to_string_lossy().into_owned(),
                kind: "movies".into(),
            })
            .unwrap();

        assert!(db.latest_scan_job(lib.id).unwrap().is_none());
        pool.set_repoint_delete_holdoff(lib.id, Duration::from_secs(3600));

        let polled = request_scan(
            Arc::clone(&db),
            Arc::clone(&pool),
            lib.id,
            ScanTrigger::Poll,
        )
        .unwrap();
        assert_eq!(polled, 0, "poll must no-op under holdoff");
        assert!(
            db.latest_scan_job(lib.id).unwrap().is_none(),
            "a held-off poll must insert no row (and so start no worker)"
        );
    }

    /// ADR-0059: the scanner's holdoff check is live, not a precomputed
    /// snapshot. Another connection holds the write lock, so the poll blocks
    /// inside `BEGIN IMMEDIATE`; the holdoff is armed while it is blocked and
    /// the poll still observes it.
    #[test]
    fn poll_observes_holdoff_armed_while_admission_is_blocked() {
        let dir = tempfile::tempdir().unwrap();
        let media = dir.path().join("media");
        fs::create_dir_all(&media).unwrap();
        fs::write(media.join("a.mp4"), b"data").unwrap();

        let db = Arc::new(nightjar_db::open(dir.path()).unwrap());
        let pool = test_pool(&db, dir.path());
        let lib = db
            .create_library(&NewLibrary {
                name: "t".into(),
                path: media.to_string_lossy().into_owned(),
                kind: "movies".into(),
            })
            .unwrap()
            .id;

        let blocker_db =
            Arc::new(nightjar_db::Db::open(&nightjar_db::db_path(dir.path())).unwrap());
        let (held_tx, held_rx) = std::sync::mpsc::channel();
        let (release_tx, release_rx) = std::sync::mpsc::channel();
        let blocker = std::thread::spawn(move || {
            blocker_db
                .with_conn(|c| {
                    let tx = nightjar_db::write_tx(c)?;
                    tx.execute("UPDATE libraries SET name = name WHERE id = ?1", [lib])
                        .map_err(|e| e.to_string())?;
                    held_tx.send(()).unwrap();
                    release_rx.recv().unwrap();
                    Ok(())
                })
                .unwrap();
        });
        held_rx
            .recv_timeout(Duration::from_secs(5))
            .expect("the blocker must hold the write lock");

        let poll_db = Arc::clone(&db);
        let poll_pool = Arc::clone(&pool);
        let poll =
            std::thread::spawn(move || request_scan(poll_db, poll_pool, lib, ScanTrigger::Poll));

        // The poll cannot pass BEGIN IMMEDIATE while the blocker holds the
        // write lock, so the holdoff is armed before its live check runs.
        pool.set_repoint_delete_holdoff(lib, Duration::from_secs(3600));
        release_tx.send(()).unwrap();

        let id = poll.join().unwrap().unwrap();
        blocker.join().unwrap();
        assert_eq!(id, 0, "the blocked poll must observe the armed holdoff");
        assert!(
            db.latest_scan_job(lib).unwrap().is_none(),
            "no row inserted by a held-off poll"
        );
    }

    #[test]
    fn repoint_holdoff_expires() {
        let dir = tempfile::tempdir().unwrap();
        let media = dir.path().join("media");
        fs::create_dir_all(&media).unwrap();
        fs::write(media.join("a.mp4"), b"data").unwrap();

        let db = Arc::new(nightjar_db::open(dir.path()).unwrap());
        let pool = test_pool(&db, dir.path());
        let lib = db
            .create_library(&NewLibrary {
                name: "t".into(),
                path: media.to_string_lossy().into_owned(),
                kind: "movies".into(),
            })
            .unwrap();

        pool.set_repoint_delete_holdoff(lib.id, Duration::from_millis(80));
        assert!(pool.repoint_delete_holdoff_active(lib.id));
        std::thread::sleep(Duration::from_millis(120));
        assert!(!pool.repoint_delete_holdoff_active(lib.id));
        let polled = request_scan(
            Arc::clone(&db),
            Arc::clone(&pool),
            lib.id,
            ScanTrigger::Poll,
        )
        .unwrap();
        assert!(polled > 0);
        wait_job(&db, polled);
    }

    #[test]
    fn repoint_with_deferred_remove_arms_holdoff() {
        let dir = tempfile::tempdir().unwrap();
        let old_root = dir.path().join("old");
        let new_root = dir.path().join("new");
        fs::create_dir_all(&old_root).unwrap();
        fs::create_dir_all(&new_root).unwrap();
        // 10 keep + 1 gone → retain 10/11 ≥ 0.90, deferred_remove = 1.
        for i in 0..10 {
            let name = format!("keep{i}.mp4");
            fs::write(old_root.join(&name), b"data").unwrap();
            fs::write(new_root.join(&name), b"data").unwrap();
        }
        fs::write(old_root.join("gone.mp4"), b"data").unwrap();

        let db = Arc::new(nightjar_db::open(dir.path()).unwrap());
        let pool = test_pool(&db, dir.path());
        let lib = db
            .create_library(&NewLibrary {
                name: "t".into(),
                path: old_root.to_string_lossy().into_owned(),
                kind: "movies".into(),
            })
            .unwrap();
        let job0 = start_scan_job(Arc::clone(&db), Arc::clone(&pool), lib.id).unwrap();
        wait_job(&db, job0);
        assert_eq!(db.list_items(lib.id).unwrap().len(), 11);

        let repoint_id = request_repoint(
            Arc::clone(&db),
            Arc::clone(&pool),
            lib.id,
            &new_root.to_string_lossy(),
        )
        .unwrap();
        wait_job(&db, repoint_id);
        let job = db.get_scan_job(repoint_id).unwrap().unwrap();
        assert_eq!(job.state, "completed", "repoint: {:?}", job.error_message);
        assert_eq!(job.deferred_remove, 1);
        assert!(
            pool.repoint_delete_holdoff_active(lib.id),
            "deferred_remove > 0 must arm holdoff"
        );
        assert_eq!(
            request_scan(
                Arc::clone(&db),
                Arc::clone(&pool),
                lib.id,
                ScanTrigger::Poll
            )
            .unwrap(),
            0
        );
    }

    #[test]
    fn repoint_reseeds_walk_cache_under_new_root() {
        let dir = tempfile::tempdir().unwrap();
        let old_root = dir.path().join("old");
        let new_root = dir.path().join("new");
        fs::create_dir_all(old_root.join("Show")).unwrap();
        fs::create_dir_all(new_root.join("Show")).unwrap();
        fs::write(old_root.join("Show/ep.mp4"), b"data").unwrap();
        fs::write(new_root.join("Show/ep.mp4"), b"data").unwrap();

        let db = Arc::new(nightjar_db::open(dir.path()).unwrap());
        let pool = test_pool(&db, dir.path());
        let lib = db
            .create_library(&NewLibrary {
                name: "t".into(),
                path: old_root.to_string_lossy().into_owned(),
                kind: "shows".into(),
            })
            .unwrap();
        let job0 = start_scan_job(Arc::clone(&db), Arc::clone(&pool), lib.id).unwrap();
        wait_job(&db, job0);

        let repoint_id = request_repoint(
            Arc::clone(&db),
            Arc::clone(&pool),
            lib.id,
            &new_root.to_string_lossy(),
        )
        .unwrap();
        wait_job(&db, repoint_id);
        let job = db.get_scan_job(repoint_id).unwrap().unwrap();
        assert_eq!(job.state, "completed", "{:?}", job.error_message);
        assert_eq!(job.unchanged + job.added + job.updated, 1);

        let dir_count = pool.with_walk_cache(lib.id, |c| c.dir_count());
        assert!(
            dir_count >= 1,
            "repoint must reseed WalkCache under new root, dir_count={dir_count}"
        );
        let lib_row = db.get_library(lib.id).unwrap().unwrap();
        let new_canon = std::fs::canonicalize(&new_root).unwrap();
        assert!(
            lib_row.path.contains("new")
                || Path::new(&lib_row.path) == new_canon.as_path()
                || lib_row.path == new_canon.to_string_lossy(),
            "library path updated: {}",
            lib_row.path
        );
    }

    // ---- ADR-0041 probe-time classification (plan step 2) ----

    fn corpus_fixture(name: &str) -> PathBuf {
        PathBuf::from(env!("CARGO_MANIFEST_DIR"))
            .join("../../../testdata/files")
            .join(name)
    }

    fn require_ffprobe() -> bool {
        if std::env::var_os("NIGHTJAR_TEST_REQUIRE_FFMPEG").is_some() {
            return true;
        }
        Command::new("ffprobe")
            .arg("-version")
            .output()
            .map(|o| o.status.success())
            .unwrap_or(false)
    }

    /// The ffmpeg sibling of [`require_ffprobe`]. The fixture builders below
    /// must not skip under `NIGHTJAR_TEST_REQUIRE_FFMPEG` just because `ffmpeg`
    /// is missing while `ffprobe` is present: an ad-hoc skip makes a missing
    /// `ffmpeg` a pass when the env var has already demanded the test run.
    fn require_ffmpeg() -> bool {
        if std::env::var_os("NIGHTJAR_TEST_REQUIRE_FFMPEG").is_some() {
            return true;
        }
        Command::new("ffmpeg")
            .arg("-version")
            .output()
            .map(|o| o.status.success())
            .unwrap_or(false)
    }

    fn skip_without_fixture(path: &Path) -> bool {
        if path.is_file() {
            return false;
        }
        if std::env::var_os("NIGHTJAR_TEST_REQUIRE_FIXTURES").is_some() {
            panic!(
                "fixture required (NIGHTJAR_TEST_REQUIRE_FIXTURES set) but missing: {}",
                path.display()
            );
        }
        eprintln!("skipping: missing {}", path.display());
        true
    }

    /// ADR-0041 Decision 8.2 / 8.3 acceptance: a simulated I/O failure during
    /// extract lands `subtitle_status = unavailable` (never `error`) and
    /// records the first backoff attempt. The sidecar read fails with an
    /// ENOENT-class error, the pool's single classifier routes it to
    /// `unavailable`, and `subtitle_attempt_count` advances so the re-queue
    /// gate can pace retries (ADR-0026 §3 schedule).
    #[test]
    fn extract_io_failure_marks_item_unavailable_not_error() {
        let dir = tempfile::tempdir().unwrap();
        let media = dir.path().join("media");
        fs::create_dir_all(&media).unwrap();

        let db = Arc::new(nightjar_db::open(dir.path()).unwrap());
        let pool = test_pool(&db, dir.path());
        let lib = db
            .create_library(&NewLibrary {
                name: "t".into(),
                path: media.to_string_lossy().into_owned(),
                kind: "movies".into(),
            })
            .unwrap();
        let ids = db
            .upsert_items_indexed(
                lib.id,
                &[nightjar_db::UpsertItem {
                    path: "Video.mp4".into(),
                    mtime_ms: 1,
                    size_bytes: 2,
                    title: "Video".into(),
                    kind: "movie".into(),
                    year: None,
                    season: None,
                    episode: None,
                    content_id: None,
                }],
            )
            .unwrap();
        let item_id = ids[0];
        // Sidecar row pointing at a file that does not exist on disk.
        db.reconcile_item_sidecars(
            item_id,
            &[nightjar_db::ObservedSidecar {
                track_id: "s-en".into(),
                path: "Video.en.srt".into(),
                mtime_ms: 1,
                size_bytes: 2,
                format: "srt".into(),
                language: Some("en".into()),
                forced: false,
                sdh: false,
                content_id: "2-first-last".into(),
            }],
        )
        .unwrap();
        certify_item(&db, item_id, &[]);
        db.set_subtitle_status(item_id, "eligible").unwrap();

        pool.enqueue(pool::WorkItem::extract(
            item_id,
            lib.id,
            media.join("Video.mp4"),
            None,
        ));

        let mut status = String::new();
        for _ in 0..200 {
            status = db.get_item(item_id).unwrap().unwrap().subtitle_status;
            if status == "unavailable" {
                break;
            }
            std::thread::sleep(Duration::from_millis(50));
        }
        assert_eq!(
            status, "unavailable",
            "an extract I/O failure must classify unavailable, not error"
        );
        // ADR-0041 Decision 8.3 end-to-end: the first failure's backoff
        // deadline (1 day) gates the reachability re-queue, so the item is
        // not re-drained immediately even on a library transition.
        let (_, extracts, _) = db.requeue_unavailable_for_library(lib.id).unwrap();
        assert_eq!(
            extracts, 0,
            "first attempt is inside its 1-day backoff window"
        );
    }

    /// Sidecar-only → classified `eligible` at probe time, then converted
    /// in-process on the on-demand extract path; the source video is never
    /// opened (a non-media "video" still converts — existing sidecar-path
    /// test pattern). ADR-0041 Decision 2 acceptance.
    #[test]
    fn probe_sidecar_only_classifies_eligible_and_converts_in_process() {
        if !require_ffprobe() {
            eprintln!("skip: ffprobe not on PATH");
            return;
        }
        // The sidecar extract spawns ffmpeg, so this needs the tool at run
        // time and not merely to build a fixture. Without the guard it failed
        // on a machine that simply has no ffmpeg, where it should skip.
        if !require_ffmpeg() {
            eprintln!("skip: ffmpeg not on PATH");
            return;
        }
        let dir = tempfile::tempdir().unwrap();
        let media = dir.path().join("media");
        fs::create_dir_all(&media).unwrap();
        let video = media.join("Movie.mp4");
        fs::copy(corpus_fixture("sidecar_beside/Movie.mp4"), &video).unwrap();
        fs::copy(
            corpus_fixture("sidecar_beside/Movie.en.srt"),
            media.join("Movie.en.srt"),
        )
        .unwrap();

        let db = Arc::new(nightjar_db::open(dir.path()).unwrap());
        let pool = test_pool(&db, dir.path());
        let lib = db
            .create_library(&NewLibrary {
                name: "t".into(),
                path: media.to_string_lossy().into_owned(),
                kind: "movies".into(),
            })
            .unwrap();
        let job_id = start_scan_job(Arc::clone(&db), Arc::clone(&pool), lib.id).unwrap();
        wait_job(&db, job_id);

        let items = db.list_items(lib.id).unwrap();
        assert_eq!(items.len(), 1, "{items:?}");
        let item = &items[0];
        assert_eq!(item.probe_status, "probed");
        assert_eq!(
            item.subtitle_status, "eligible",
            "sidecar convertible to WebVTT must classify eligible"
        );
        assert!(
            db.list_item_subtitle_tracks(item.id).unwrap().is_empty(),
            "no embedded streams to persist for a sidecar-only item"
        );

        // On-demand extract (the ADR-0013 §11 path step 3 gates): the sidecar
        // converts in-process and the item flips to ready.
        pool.enqueue(pool::WorkItem::extract(
            item.id,
            lib.id,
            video.clone(),
            None,
        ));
        let mut ready = false;
        for _ in 0..200 {
            if db.get_item(item.id).unwrap().unwrap().subtitle_status == "ready" {
                ready = true;
                break;
            }
            std::thread::sleep(Duration::from_millis(50));
        }
        assert!(ready, "sidecar extract never reached ready");
        let store = SubsStore::new(dir.path().join("subs")).unwrap();
        let artifact = committed_artifact_path(&db, &store, item.id, "s-en");
        assert!(
            artifact.is_file(),
            "sidecar webvtt missing under {}",
            artifact.display()
        );
        let body = fs::read_to_string(&artifact).unwrap();
        assert!(body.contains("WEBVTT"), "not webvtt: {body}");

        // Source video never opened: a fake "video" + sidecar still converts
        // in-process (any real source read would fail on a non-media file).
        fs::write(media.join("Fake.mp4"), b"not a real mp4").unwrap();
        fs::copy(
            corpus_fixture("sidecar_beside/Movie.en.srt"),
            media.join("Fake.en.srt"),
        )
        .unwrap();
        let job2 = start_scan_job(Arc::clone(&db), Arc::clone(&pool), lib.id).unwrap();
        wait_job(&db, job2);
        let fake = db
            .list_items(lib.id)
            .unwrap()
            .into_iter()
            .find(|i| i.path.ends_with("Fake.mp4"))
            .expect("fake video indexed");
        assert_eq!(
            fake.subtitle_status, "pending",
            "probe of a non-media file fails; status must stay pending, got {:?}",
            fake
        );
        pool.enqueue(pool::WorkItem::extract(
            fake.id,
            lib.id,
            media.join("Fake.mp4"),
            None,
        ));
        let mut fake_ready = false;
        for _ in 0..20 {
            if db.get_item(fake.id).unwrap().unwrap().subtitle_status == "ready" {
                fake_ready = true;
                break;
            }
            std::thread::sleep(Duration::from_millis(50));
        }
        // D2B.2 acceptance 1: a failed probe leaves the source uncertified, so
        // the extract defers. The item is not marked ready and nothing is
        // published; AV playback is unaffected.
        assert!(
            !fake_ready,
            "an uncertified source must defer, never publish"
        );
    }

    /// ≥3 embedded text tracks including one unrecognised codec → all rows
    /// persisted, `kind = 'unknown'` for the unrecognised one, and the item
    /// classifies `eligible` (ADR-0041 Decision 1 acceptance).
    #[test]
    fn probe_persists_multi_track_inventory_with_unknown_kind() {
        if !require_ffprobe() {
            eprintln!("skip: ffprobe not on PATH");
            return;
        }
        if !require_ffmpeg() {
            eprintln!("skip: ffmpeg not on PATH");
            return;
        }
        let dir = tempfile::tempdir().unwrap();
        let media = dir.path().join("media");
        fs::create_dir_all(&media).unwrap();
        let mkv = media.join("multi.mkv");
        let srt_a = dir.path().join("a.srt");
        let srt_b = dir.path().join("b.srt");
        let srt_c = dir.path().join("c.srt");
        fs::write(&srt_a, "1\n00:00:00,000 --> 00:00:01,000\nTrack A\n").unwrap();
        fs::write(&srt_b, "1\n00:00:00,000 --> 00:00:01,000\nTrack B\n").unwrap();
        fs::write(&srt_c, "1\n00:00:00,000 --> 00:00:01,000\nTrack C\n").unwrap();
        let status = Command::new("ffmpeg")
            .args([
                "-nostdin",
                "-hide_banner",
                "-loglevel",
                "error",
                "-y",
                "-f",
                "lavfi",
                "-i",
                "color=c=black:s=64x64:d=0.3",
                "-f",
                "lavfi",
                "-i",
                "anullsrc=r=48000:cl=stereo",
                "-i",
            ])
            .arg(&srt_a)
            .arg("-i")
            .arg(&srt_b)
            .arg("-i")
            .arg(&srt_c)
            .args([
                "-map",
                "0:v:0",
                "-map",
                "1:a:0",
                "-map",
                "2:0",
                "-map",
                "3:0",
                "-map",
                "4:0",
                "-c:v",
                "libx264",
                "-pix_fmt",
                "yuv420p",
                "-c:a",
                "aac",
                "-c:s",
                "srt",
                "-shortest",
            ])
            .arg(&mkv)
            .status();
        let status = match status {
            Ok(s) => s,
            Err(e) => panic!("could not spawn ffmpeg to build the multi-track fixture: {e}"),
        };
        if !status.success() {
            panic!(
                "ffmpeg mux of the multi-track fixture failed with exit code {:?}",
                status.code()
            );
        }
        // ffprobe reports an unmapped CodecID as no codec_name; make the third
        // track unrecognised by patching its CodecID (same-length swap).
        let mut bytes = fs::read(&mkv).unwrap();
        let needle = b"S_TEXT/UTF8";
        let mut last = None;
        for i in 0..bytes.len().saturating_sub(needle.len()) {
            if &bytes[i..i + needle.len()] == needle {
                last = Some(i);
            }
        }
        let Some(pos) = last else {
            panic!("no S_TEXT/UTF8 CodecID found in the muxed fixture to patch");
        };
        bytes[pos..pos + 11].copy_from_slice(b"S_TEXT/FOO!");
        fs::write(&mkv, &bytes).unwrap();

        let db = Arc::new(nightjar_db::open(dir.path()).unwrap());
        let pool = test_pool(&db, dir.path());
        let lib = db
            .create_library(&NewLibrary {
                name: "t".into(),
                path: media.to_string_lossy().into_owned(),
                kind: "movies".into(),
            })
            .unwrap();
        let job_id = start_scan_job(Arc::clone(&db), Arc::clone(&pool), lib.id).unwrap();
        wait_job(&db, job_id);

        let items = db.list_items(lib.id).unwrap();
        assert_eq!(items.len(), 1, "{items:?}");
        let item = &items[0];
        assert_eq!(item.probe_status, "probed");
        assert_eq!(item.subtitle_status, "eligible");
        let tracks = db.list_item_subtitle_tracks(item.id).unwrap();
        assert_eq!(tracks.len(), 3, "{tracks:?}");
        assert_eq!(tracks[0].kind, "text");
        assert_eq!(tracks[0].codec, "subrip");
        assert_eq!(tracks[1].kind, "text");
        assert_eq!(tracks[1].codec, "subrip");
        assert_eq!(
            tracks[2].kind, "unknown",
            "unrecognised codec must be counted, never dropped: {tracks:?}"
        );
        assert_eq!(tracks[2].codec, "unknown");
    }

    /// Two media rows and a pool for the bulk-reader gate / cancel tests.
    fn gate_fixture(
        dir: &tempfile::TempDir,
        files: &[(&str, usize)],
    ) -> (Arc<Db>, Arc<LibraryPool>, nightjar_db::LibraryRow, Vec<i64>) {
        let media = dir.path().join("media");
        fs::create_dir_all(&media).unwrap();
        let mut paths = Vec::new();
        for (name, fill) in files {
            let path = media.join(name);
            fs::write(&path, vec![b'x'; *fill]).unwrap();
            paths.push(path);
        }
        let db = Arc::new(nightjar_db::open(dir.path()).unwrap());
        let pool = test_pool(&db, dir.path());
        let lib = db
            .create_library(&NewLibrary {
                name: "t".into(),
                path: media.to_string_lossy().into_owned(),
                kind: "movies".into(),
            })
            .unwrap();
        let items: Vec<UpsertItem> = files
            .iter()
            .enumerate()
            .map(|(i, (name, fill))| UpsertItem {
                path: (*name).to_string(),
                mtime_ms: (i as i64) + 1,
                size_bytes: *fill as i64,
                title: format!("T{i}"),
                kind: "movie".into(),
                year: None,
                season: None,
                episode: None,
                content_id: None,
            })
            .collect();
        let ids = db.upsert_items_indexed(lib.id, &items).unwrap();
        // A certified source with one embedded text stream: the junk fixtures
        // make the extract fail, which is what these gate tests exercise.
        for id in &ids {
            certify_item(&db, *id, &[(2, "subrip")]);
        }
        (db, pool, lib, ids)
    }

    /// Wait until the background queue drains and `item_ids` have left
    /// `pending` (their extract/map runs actually finished, not just popped).
    fn wait_items_terminal(
        db: &Db,
        pool: &LibraryPool,
        item_ids: &[i64],
    ) -> crate::pool::BackgroundProgress {
        for _ in 0..400 {
            let p = pool.background_progress();
            if p.queued_extracts == 0 && p.queued_maps == 0 {
                let all_terminal = item_ids.iter().all(|id| {
                    db.get_item(*id).ok().flatten().is_some_and(|row| {
                        row.subtitle_status != "pending" || row.map_status != "pending"
                    })
                });
                if all_terminal {
                    return p;
                }
            }
            std::thread::sleep(std::time::Duration::from_millis(25));
        }
        panic!(
            "background work never drained: {:?}",
            pool.background_progress()
        );
    }

    /// ADR-0041 Decision 8.6 acceptance: one bulk-reader gate serialises a
    /// subtitle extract and a keyframe-map build. While the gate is held (the
    /// in-flight reader case from the dogfood race), neither kind starts;
    /// releasing it lets both drain. The Decision 8.8 progress counters are
    /// queryable through the same pool accessor, and a failing pass must not
    /// inflate them (the junk fixtures fail both runs).
    #[test]
    fn bulk_reader_gate_serializes_extract_and_map_with_queryable_progress() {
        let dir = tempfile::tempdir().unwrap();
        let (db, pool, lib, ids) = gate_fixture(&dir, &[("A.mp4", 16), ("B.mp4", 16)]);
        let media = dir.path().join("media");
        let a = media.join("A.mp4");
        let b = media.join("B.mp4");

        // Hold the gate exactly as an in-flight reader would.
        let _gate = pool.bulk_reader.lock().unwrap();
        pool.enqueue(crate::pool::WorkItem::extract(
            ids[0],
            lib.id,
            a.clone(),
            None,
        ));
        pool.enqueue_map_rebuild(ids[1], lib.id, b.clone());
        std::thread::sleep(std::time::Duration::from_millis(250));

        let held = pool.background_progress();
        assert_eq!(held.queued_extracts, 1, "{held:?}");
        assert_eq!(held.queued_maps, 1, "{held:?}");
        assert_eq!(
            held.completed, 0,
            "the gate must block starts, not just queue them: {held:?}"
        );

        drop(_gate);
        let drained = wait_items_terminal(&db, &pool, &[ids[0], ids[1]]);
        assert_eq!(drained.queued_extracts, 0, "{drained:?}");
        assert_eq!(drained.queued_maps, 0, "{drained:?}");
        assert_eq!(
            drained.completed, 0,
            "failed runs must not inflate the 8.8 counter: {drained:?}"
        );
        assert_eq!(
            drained.rate_per_min, 0.0,
            "no successful completions, no rate: {drained:?}"
        );
        let _ = (lib, ids);
    }

    /// The ADR-0013 §8.4 index-phase pause covers keyframe-map builds too
    /// (ADR-0023 §2 amendment): neither an extract nor a map build starts
    /// while the index epoch is held, from begin_index through
    /// set_scan_job_index_done.
    #[test]
    fn index_epoch_pause_covers_extract_and_map_starts() {
        let dir = tempfile::tempdir().unwrap();
        let (db, pool, lib, ids) = gate_fixture(&dir, &[("A.mp4", 16), ("B.mp4", 16)]);
        let media = dir.path().join("media");
        let a = media.join("A.mp4");
        let b = media.join("B.mp4");

        let _epoch = pool.enter_index_epoch(lib.id);
        pool.enqueue(crate::pool::WorkItem::extract(
            ids[0],
            lib.id,
            a.clone(),
            None,
        ));
        pool.enqueue_map_rebuild(ids[1], lib.id, b.clone());
        std::thread::sleep(std::time::Duration::from_millis(250));

        let held = pool.background_progress();
        assert_eq!(held.queued_extracts + held.queued_maps, 2, "{held:?}");
        assert_eq!(
            held.completed, 0,
            "the index phase must pause background starts: {held:?}"
        );

        drop(_epoch);
        let drained = wait_items_terminal(&db, &pool, &[ids[0], ids[1]]);
        assert_eq!(
            drained.queued_extracts + drained.queued_maps,
            0,
            "{drained:?}"
        );
        let _ = (lib, ids);
    }

    /// Decision 8.8 positive case: a genuinely successful extract counts
    /// toward the completed counter — the operator's "is it moving" signal.
    /// (The negative case, failures never counting, is locked by the gate
    /// test above.)
    #[test]
    fn successful_extract_counts_as_background_completion() {
        if !require_ffprobe() {
            eprintln!("skip: ffprobe not on PATH");
            return;
        }
        if !require_ffmpeg() {
            eprintln!("skip: ffmpeg not on PATH");
            return;
        }
        let dir = tempfile::tempdir().unwrap();
        let media = dir.path().join("media");
        fs::create_dir_all(&media).unwrap();
        let corpus = Path::new(env!("CARGO_MANIFEST_DIR"))
            .join("../../../testdata/files/h264_aac_srt_mkv.mkv");
        if skip_without_fixture(&corpus) {
            return;
        }
        let stored = media.join("Subs.mkv");
        fs::copy(&corpus, &stored).unwrap();

        let db = Arc::new(nightjar_db::open(dir.path()).unwrap());
        let pool = test_pool(&db, dir.path());
        let lib = db
            .create_library(&NewLibrary {
                name: "t".into(),
                path: media.to_string_lossy().into_owned(),
                kind: "movies".into(),
            })
            .unwrap();
        let size_bytes = fs::metadata(&stored).unwrap().len() as i64;
        let ids = db
            .upsert_items_indexed(
                lib.id,
                &[UpsertItem {
                    path: "Subs.mkv".into(),
                    mtime_ms: 1,
                    size_bytes,
                    title: "Subs".into(),
                    kind: "movie".into(),
                    year: None,
                    season: None,
                    episode: None,
                    content_id: None,
                }],
            )
            .unwrap();
        let item_id = ids[0];
        certify_item(&db, item_id, &text_streams(&stored));

        pool.enqueue(crate::pool::WorkItem::extract(
            item_id, lib.id, stored, None,
        ));
        for _ in 0..400 {
            let row = db.get_item(item_id).unwrap().unwrap();
            if row.subtitle_status == "ready" {
                break;
            }
            std::thread::sleep(std::time::Duration::from_millis(25));
        }
        let row = db.get_item(item_id).unwrap().unwrap();
        assert_eq!(row.subtitle_status, "ready", "{row:?}");
        let p = pool.background_progress();
        assert_eq!(
            p.completed, 1,
            "a successful extract must count toward the 8.8 counter: {p:?}"
        );
        assert!(p.rate_per_min > 0.0, "{p:?}");
    }

    /// ADR-0041 Decision 8.7 acceptance: marking a library unreachable while
    /// an extract is in flight cancels that extract (kills the demux); it
    /// does not merely stop new starts. The fixture's only SRT cue sits at
    /// 29 s of a 30 s title, so a cancelled demux can never have flushed it
    /// and the run lands `unavailable`, never `ready`.
    #[test]
    fn extract_in_flight_is_cancelled_when_library_unreachable() {
        if !require_ffprobe() {
            eprintln!("skip: ffprobe not on PATH");
            return;
        }
        if !require_ffmpeg() {
            eprintln!("skip: ffmpeg not on PATH");
            return;
        }
        let dir = tempfile::tempdir().unwrap();
        let media = dir.path().join("media");
        fs::create_dir_all(&media).unwrap();
        let srt = dir.path().join("late.srt");
        fs::write(&srt, "1\n00:00:29,000 --> 00:00:30,000\nLate cue\n").unwrap();
        let mkv = media.join("Slow.mkv");
        let status = Command::new("ffmpeg")
            .args([
                "-nostdin",
                "-hide_banner",
                "-loglevel",
                "error",
                "-y",
                "-f",
                "lavfi",
                "-i",
                "testsrc2=s=320x240:d=30:r=30",
                "-i",
            ])
            .arg(&srt)
            .args([
                "-c:v",
                "libx264",
                "-pix_fmt",
                "yuv420p",
                "-c:s",
                "srt",
                "-map",
                "0:v:0",
                "-map",
                "1:0",
                "-shortest",
            ])
            .arg(&mkv)
            .status();
        let status = match status {
            Ok(s) => s,
            Err(e) => panic!("could not spawn ffmpeg to build the slow-fixture mkv: {e}"),
        };
        if !status.success() {
            panic!(
                "ffmpeg mux of the slow-fixture mkv failed with exit code {:?}",
                status.code()
            );
        }

        let db = Arc::new(nightjar_db::open(dir.path()).unwrap());
        let pool = test_pool(&db, dir.path());
        let lib = db
            .create_library(&NewLibrary {
                name: "t".into(),
                path: media.to_string_lossy().into_owned(),
                kind: "movies".into(),
            })
            .unwrap();
        let size_bytes = fs::metadata(&mkv).unwrap().len() as i64;
        let ids = db
            .upsert_items_indexed(
                lib.id,
                &[UpsertItem {
                    path: "Slow.mkv".into(),
                    mtime_ms: 1,
                    size_bytes,
                    title: "Slow".into(),
                    kind: "movie".into(),
                    year: None,
                    season: None,
                    episode: None,
                    content_id: None,
                }],
            )
            .unwrap();
        let item_id = ids[0];
        certify_item(&db, item_id, &text_streams(&mkv));

        pool.enqueue(crate::pool::WorkItem::extract(item_id, lib.id, mkv, None));
        // Wait until the worker has popped the extract: it is now in flight.
        for _ in 0..400 {
            if pool.background_progress().queued_extracts == 0 {
                break;
            }
            std::thread::sleep(std::time::Duration::from_millis(5));
        }
        assert_eq!(
            pool.background_progress().queued_extracts,
            0,
            "extract never started"
        );

        pool.set_library_reachability(lib.id, &lib.path, false)
            .unwrap();
        for _ in 0..400 {
            let row = db.get_item(item_id).unwrap().unwrap();
            if row.subtitle_status == "unavailable" {
                break;
            }
            std::thread::sleep(std::time::Duration::from_millis(25));
        }
        let row = db.get_item(item_id).unwrap().unwrap();
        assert_eq!(
            row.subtitle_status, "unavailable",
            "an in-flight extract must be cancelled, never claimed ready: {:?}",
            row
        );
        assert_eq!(
            pool.background_progress().completed,
            0,
            "a cancelled extract must not count as a completed run"
        );
    }

    /// ADR-0041 Decision 8.7 (amended 2026-08-07) for the probe side: marking
    /// a library unreachable while a probe is in flight kills the ffprobe
    /// child and the item lands `unavailable`, never `probed` or `error`.
    /// The source is removed as the library goes away, so the final stat
    /// proves a real access failure (ADR-0058); a cancellation whose source is
    /// still readable publishes nothing.
    /// The fixture is an MKV carrying a large attachment: ffprobe reads the
    /// whole Segment header (attachments live there), so the probe stays
    /// observably in flight for hundreds of milliseconds instead of the few
    /// tens of milliseconds a header-only fixture takes — the cancellation
    /// races a real process, never a sleep-based fake.
    #[test]
    fn probe_in_flight_is_cancelled_when_library_unreachable() {
        if !require_ffprobe() {
            eprintln!("skip: ffprobe not on PATH");
            return;
        }
        if !require_ffmpeg() {
            eprintln!("skip: ffmpeg not on PATH");
            return;
        }
        let dir = tempfile::tempdir().unwrap();
        let media = dir.path().join("media");
        fs::create_dir_all(&media).unwrap();
        // Attachment content is generated, not copied from the host, so the
        // fixture is portable across machines.
        let blob = dir.path().join("blob.bin");
        {
            use std::io::Write;
            let mut f = fs::File::create(&blob).unwrap();
            let chunk = vec![0xABu8; 1 << 20];
            for _ in 0..200 {
                f.write_all(&chunk).unwrap();
            }
        }
        let mkv = media.join("SlowProbe.mkv");
        let status = Command::new("ffmpeg")
            .args([
                "-nostdin",
                "-hide_banner",
                "-loglevel",
                "error",
                "-y",
                "-f",
                "lavfi",
                "-i",
                "testsrc2=s=320x240:d=5:r=30",
                "-c:v",
                "libx264",
                "-pix_fmt",
                "yuv420p",
                "-attach",
            ])
            .arg(&blob)
            .args([
                "-metadata:s:t",
                "mimetype=application/octet-stream",
                "-metadata:s:t",
                "filename=blob.bin",
            ])
            .arg(&mkv)
            .status();
        let status = match status {
            Ok(s) => s,
            Err(e) => panic!("could not spawn ffmpeg to build the slow-probe fixture: {e}"),
        };
        if !status.success() {
            panic!(
                "ffmpeg mux of the slow-probe fixture failed with exit code {:?}",
                status.code()
            );
        }

        let db = Arc::new(nightjar_db::open(dir.path()).unwrap());
        let pool = test_pool(&db, dir.path());
        let lib = db
            .create_library(&NewLibrary {
                name: "t".into(),
                path: media.to_string_lossy().into_owned(),
                kind: "movies".into(),
            })
            .unwrap();
        let size_bytes = fs::metadata(&mkv).unwrap().len() as i64;
        let ids = db
            .upsert_items_indexed(
                lib.id,
                &[UpsertItem {
                    path: "SlowProbe.mkv".into(),
                    mtime_ms: 1,
                    size_bytes,
                    title: "SlowProbe".into(),
                    kind: "movie".into(),
                    year: None,
                    season: None,
                    episode: None,
                    content_id: None,
                }],
            )
            .unwrap();
        let item_id = ids[0];

        pool.enqueue(crate::pool::WorkItem::probe(item_id, lib.id, mkv, None));
        // Wait until the worker has popped the probe: it is now in flight.
        for _ in 0..400 {
            if pool.background_progress().queued_probes == 0 {
                break;
            }
            std::thread::sleep(std::time::Duration::from_millis(5));
        }
        assert_eq!(
            pool.background_progress().queued_probes,
            0,
            "probe never started"
        );

        // The mount goes away with the cancel: the source can no longer be
        // stat'ed, which is what makes the result `unavailable` (ADR-0058).
        fs::remove_dir_all(&media).unwrap();
        pool.set_library_reachability(lib.id, &lib.path, false)
            .unwrap();
        for _ in 0..400 {
            let row = db.get_item(item_id).unwrap().unwrap();
            if row.probe_status != "indexed" {
                break;
            }
            std::thread::sleep(std::time::Duration::from_millis(25));
        }
        let row = db.get_item(item_id).unwrap().unwrap();
        assert_eq!(
            row.probe_status, "unavailable",
            "an in-flight probe must be cancelled, never probed or error: {:?}",
            row
        );
    }

    /// ADR-0041 Decision 8.7 for the map side: a packet walk (whole-file
    /// reader) is cancelled in flight when its library goes unreachable and
    /// the item lands `unavailable`, never a ready map. The fixture is a
    /// tail-truncated MKV (Cues live at the end), so the index read cannot
    /// succeed and the build must take the packet-walk fallback.
    ///
    /// **The in-flight state is deterministic, not raced.** The old form
    /// waited for `queued_maps == 0` and then flipped reachability — and
    /// between "started" and "flipped" a fast runner could finish the whole
    /// walk and store a ready map, which the assertion then reported as the
    /// product's bug. It was a race in the test: the walk that finished had
    /// already proved nothing about cancellation. This test arms the pool's
    /// test-only hold ([`pool::LibraryPool::arm_map_walk_hold`]), so the map
    /// worker reports that the build has entered and then parks until the
    /// test releases it. The reachability flip therefore always lands while
    /// the build is provably still running. A run where the hold never fires
    /// proves nothing, so the wait is bounded and panics loudly instead of
    /// passing.
    #[test]
    fn map_packet_walk_in_flight_is_cancelled_when_library_unreachable() {
        if !require_ffprobe() {
            eprintln!("skip: ffprobe not on PATH");
            return;
        }
        if !require_ffmpeg() {
            eprintln!("skip: ffmpeg not on PATH");
            return;
        }
        let dir = tempfile::tempdir().unwrap();
        let media = dir.path().join("media");
        fs::create_dir_all(&media).unwrap();
        let muxed = media.join("Muxed.mkv");
        let status = Command::new("ffmpeg")
            .args([
                "-nostdin",
                "-hide_banner",
                "-loglevel",
                "error",
                "-y",
                "-f",
                "lavfi",
                "-i",
                "testsrc2=s=320x240:d=30:r=30",
                "-c:v",
                "libx264",
                "-pix_fmt",
                "yuv420p",
            ])
            .arg(&muxed)
            .status();
        let status = match status {
            Ok(s) => s,
            Err(e) => panic!("could not spawn ffmpeg to build the map-walk fixture: {e}"),
        };
        if !status.success() {
            panic!(
                "ffmpeg mux of the map-walk fixture failed with exit code {:?}",
                status.code()
            );
        }
        let data = fs::read(&muxed).unwrap();
        let cut = (data.len() as f64 * 0.80) as usize;
        let mkv = media.join("NoCues.mkv");
        fs::write(&mkv, &data[..cut]).unwrap();

        let db = Arc::new(nightjar_db::open(dir.path()).unwrap());
        let pool = test_pool(&db, dir.path());
        let lib = db
            .create_library(&NewLibrary {
                name: "t".into(),
                path: media.to_string_lossy().into_owned(),
                kind: "movies".into(),
            })
            .unwrap();
        let size_bytes = cut as i64;
        let ids = db
            .upsert_items_indexed(
                lib.id,
                &[UpsertItem {
                    path: "NoCues.mkv".into(),
                    mtime_ms: 1,
                    size_bytes,
                    title: "NoCues".into(),
                    kind: "movie".into(),
                    year: None,
                    season: None,
                    episode: None,
                    content_id: None,
                }],
            )
            .unwrap();
        let item_id = ids[0];

        // Arm the hold, then enqueue: the worker reports the build has
        // entered and parks until released. Flipping reachability while the
        // worker is parked cannot race the walk, so the map below must be
        // cancelled in flight — never a ready map.
        let hold = pool.arm_map_walk_hold();
        pool.enqueue_map_rebuild(item_id, lib.id, mkv);
        let entered = hold
            .entered_rx
            .recv_timeout(std::time::Duration::from_secs(30))
            .unwrap_or_else(|e| {
                panic!(
                    "map build never entered the test hold; the walk was not held in flight: {e}"
                )
            });
        assert_eq!(entered, item_id, "the held map build is a different item");

        pool.set_library_reachability(lib.id, &lib.path, false)
            .unwrap();
        // Release the parked build: it must observe the flip and cancel.
        let _ = hold.release_tx.send(());
        drop(hold);
        for _ in 0..400 {
            let row = db.get_item(item_id).unwrap().unwrap();
            if row.map_status != "pending" {
                break;
            }
            std::thread::sleep(std::time::Duration::from_millis(25));
        }
        let row = db.get_item(item_id).unwrap().unwrap();
        assert_eq!(
            row.map_status, "unavailable",
            "an in-flight packet walk must be cancelled, never a ready map: {:?}",
            row
        );
        assert_eq!(
            pool.background_progress().completed,
            0,
            "a cancelled packet walk must not count as a completed run"
        );
    }

    /// ADR-0023 §2/§9: the scan/index path no longer enqueues keyframe-map
    /// builds. A fresh item that goes through a full scan (walk + probe)
    /// ends unmapped: nothing is queued, no map was built, and `map_status`
    /// is the column default (`pending` = unmapped, not queued — the
    /// whole-library pending sweep is retired). The map builds when a
    /// consumer asks.
    #[test]
    fn scan_index_path_does_not_enqueue_map_builds() {
        let dir = tempfile::tempdir().unwrap();
        let media = dir.path().join("media");
        fs::create_dir_all(&media).unwrap();
        let junk = media.join("A.mp4");
        fs::write(&junk, b"not a real mp4").unwrap();

        let db = Arc::new(nightjar_db::open(dir.path()).unwrap());
        let pool = test_pool(&db, dir.path());
        let lib = db
            .create_library(&NewLibrary {
                name: "t".into(),
                path: media.to_string_lossy().into_owned(),
                kind: "movies".into(),
            })
            .unwrap();

        let job_id = start_scan_job(Arc::clone(&db), Arc::clone(&pool), lib.id).unwrap();
        for _ in 0..200 {
            let job = db.get_scan_job(job_id).unwrap().unwrap();
            if job.state == "completed" || job.state == "failed" {
                break;
            }
            std::thread::sleep(std::time::Duration::from_millis(25));
        }
        // Let any mis-scheduled background work surface.
        std::thread::sleep(std::time::Duration::from_millis(300));

        let progress = pool.background_progress();
        assert_eq!(
            progress.queued_maps, 0,
            "scan must not queue map builds: {progress:?}"
        );
        let rows = db.list_items(lib.id).unwrap();
        assert_eq!(rows.len(), 1);
        assert_eq!(
            rows[0].map_status, "pending",
            "fresh item is unmapped (`pending` default), never queued for a sweep"
        );
        assert!(
            db.keyframe_map(rows[0].id).unwrap().is_none(),
            "the scan must not build a map for a fresh item"
        );

        // The demand trigger is what builds it. Hold the bulk-reader gate so
        // the build cannot pop before we observe it queued (ADR-0041 8.6).
        let _gate = pool.bulk_reader.lock().unwrap();
        pool.prioritize_map_rebuild(rows[0].id, lib.id, junk);
        assert!(
            pool.map_build_pending(rows[0].id),
            "the demand trigger queues the build"
        );
    }

    /// ADR-0023 §9.1: the demand trigger (what playbackInfo calls) builds the
    /// map for an unmapped item — index-first — and the pool reports the
    /// build as pending until it lands as a ready, usable map.
    #[test]
    fn demand_trigger_builds_map_and_reports_pending_until_ready() {
        if !require_ffprobe() {
            eprintln!("skip: ffprobe not on PATH");
            return;
        }
        if !require_ffmpeg() {
            eprintln!("skip: ffmpeg not on PATH");
            return;
        }
        let dir = tempfile::tempdir().unwrap();
        let media = dir.path().join("media");
        fs::create_dir_all(&media).unwrap();
        let mkv = media.join("clip.mkv");
        let status = Command::new("ffmpeg")
            .args([
                "-nostdin",
                "-hide_banner",
                "-loglevel",
                "error",
                "-y",
                "-f",
                "lavfi",
                "-i",
                "testsrc2=s=320x240:d=2:r=30",
                "-c:v",
                "libx264",
                "-pix_fmt",
                "yuv420p",
            ])
            .arg(&mkv)
            .status();
        let status = match status {
            Ok(s) => s,
            Err(e) => panic!("could not spawn ffmpeg to build the demand-trigger clip: {e}"),
        };
        if !status.success() {
            panic!(
                "ffmpeg mux of the demand-trigger clip failed with exit code {:?}",
                status.code()
            );
        }

        let db = Arc::new(nightjar_db::open(dir.path()).unwrap());
        let pool = test_pool(&db, dir.path());
        let lib = db
            .create_library(&NewLibrary {
                name: "t".into(),
                path: media.to_string_lossy().into_owned(),
                kind: "movies".into(),
            })
            .unwrap();
        let ids = db
            .upsert_items_indexed(
                lib.id,
                &[UpsertItem {
                    path: "clip.mkv".into(),
                    mtime_ms: 1,
                    size_bytes: 100,
                    title: "clip".into(),
                    kind: "movie".into(),
                    year: None,
                    season: None,
                    episode: None,
                    content_id: None,
                }],
            )
            .unwrap();
        let item_id = ids[0];
        assert_eq!(
            pool.background_progress().queued_maps,
            0,
            "indexing alone must not queue a map build"
        );

        // Hold the bulk-reader gate so the build cannot pop before we observe
        // it queued (ADR-0041 8.6): the pool must report the build pending
        // until it actually runs. The gate is taken *before* the enqueue: a
        // worker takes it at pop time, so holding it first is what keeps the
        // item in the queue. Taken after, the worker can pop, build and clear
        // in the gap, and the assertion below reads a finished build as a
        // missing one.
        let _gate = pool.bulk_reader.lock().unwrap();
        pool.prioritize_map_rebuild(item_id, lib.id, mkv);
        assert!(
            pool.map_build_pending(item_id),
            "the build must be visible as queued while the gate is held"
        );
        drop(_gate);
        for _ in 0..400 {
            let row = db.get_item(item_id).unwrap().unwrap();
            if row.map_status == "ready" || row.map_status == "error" {
                break;
            }
            std::thread::sleep(std::time::Duration::from_millis(25));
        }
        assert!(
            db.keyframe_map(item_id).unwrap().is_some(),
            "a demand-triggered build must land a ready, usable map"
        );
        assert!(
            !pool.map_build_pending(item_id),
            "build done, nothing pending"
        );
    }

    /// D2B.2 acceptance 1: an uncertified source defers promptly. The item keeps
    /// its status, nothing is published, no probe is started, and no failure
    /// backoff is consumed.
    #[test]
    fn uncertified_source_defers_without_probing_or_backoff() {
        let dir = tempfile::tempdir().unwrap();
        let media = dir.path().join("media");
        fs::create_dir_all(&media).unwrap();
        fs::write(media.join("Movie.mp4"), b"not a real mp4").unwrap();

        let db = Arc::new(nightjar_db::open(dir.path()).unwrap());
        let pool = test_pool(&db, dir.path());
        let lib = db
            .create_library(&NewLibrary {
                name: "t".into(),
                path: media.to_string_lossy().into_owned(),
                kind: "movies".into(),
            })
            .unwrap();
        let ids = db
            .upsert_items_indexed(
                lib.id,
                &[UpsertItem {
                    path: "Movie.mp4".into(),
                    mtime_ms: 1,
                    size_bytes: 15,
                    title: "Movie".into(),
                    kind: "movie".into(),
                    year: None,
                    season: None,
                    episode: None,
                    content_id: None,
                }],
            )
            .unwrap();
        let item_id = ids[0];

        // Rendezvous: arm the extract hold, wait until the worker has claimed
        // the item, release it, then wait on the pool's deferral counter. No
        // fixed sleep is needed to know the worker reached the deferral.
        let hold = pool.arm_extract_hold();
        pool.enqueue(pool::WorkItem::extract(
            item_id,
            lib.id,
            media.join("Movie.mp4"),
            None,
        ));
        assert_eq!(
            hold.entered_rx
                .recv_timeout(Duration::from_secs(10))
                .unwrap(),
            item_id,
            "the extract worker must claim the item"
        );
        hold.release_tx.send(()).unwrap();
        pool.wait_unverified_deferrals(1);

        let row = db.get_item(item_id).unwrap().unwrap();
        assert_eq!(
            row.subtitle_status, "pending",
            "a deferral must not write a subtitle status"
        );
        assert_eq!(
            row.probe_status, "indexed",
            "a deferral must not start a probe"
        );
        let attempts: i64 = db
            .with_conn(|c| {
                c.query_row(
                    "SELECT subtitle_attempt_count FROM media_items WHERE id = ?1",
                    [item_id],
                    |r| r.get(0),
                )
                .map_err(|e| e.to_string())
            })
            .unwrap();
        assert_eq!(attempts, 0, "a deferral must not consume failure backoff");
        let store = SubsStore::new(dir.path().join("subs")).unwrap();
        assert!(
            !store.item_dir(item_id).exists(),
            "a deferral must publish nothing"
        );
        assert_eq!(pool.background_progress().completed, 0);
    }

    /// D2B.2 acceptance 2/6: combined-generation single-flight. A newer
    /// generation that arrives while the older run is in flight is preserved as
    /// a successor and runs after it; the older run never executes as the newer
    /// generation.
    #[test]
    fn newer_generation_is_preserved_as_a_successor() {
        let dir = tempfile::tempdir().unwrap();
        let media = dir.path().join("media");
        fs::create_dir_all(&media).unwrap();
        let video = media.join("Movie.mkv");
        fs::write(&video, b"not a real mkv").unwrap();
        let sidecar_path = media.join("Movie.en.srt");
        let sidecar_src = corpus_fixture("sidecar_beside/Movie.en.srt");
        if !sidecar_src.is_file() {
            eprintln!("skipping: missing {}", sidecar_src.display());
            return;
        }
        fs::copy(&sidecar_src, &sidecar_path).unwrap();

        let db = Arc::new(nightjar_db::open(dir.path()).unwrap());
        let pool = test_pool(&db, dir.path());
        let lib = db
            .create_library(&NewLibrary {
                name: "t".into(),
                path: media.to_string_lossy().into_owned(),
                kind: "movies".into(),
            })
            .unwrap();
        let ids = db
            .upsert_items_indexed(
                lib.id,
                &[UpsertItem {
                    path: "Movie.mkv".into(),
                    mtime_ms: 1,
                    size_bytes: 15,
                    title: "Movie".into(),
                    kind: "movie".into(),
                    year: None,
                    season: None,
                    episode: None,
                    content_id: None,
                }],
            )
            .unwrap();
        let item_id = ids[0];
        db.reconcile_item_sidecars(
            item_id,
            &[observed_sidecar(
                &sidecar_path,
                "Movie.en.srt",
                "s-en",
                "srt",
            )],
        )
        .unwrap();
        certify_item(&db, item_id, &[]);
        let first = db
            .certified_subtitle_source(item_id)
            .unwrap()
            .expect("certified source");
        let first_identity = first.source_identity();
        let first_token = first.token_for_track("s-en").expect("first token");

        // The older run is in flight and parked. While it is parked the sidecar
        // changes, which allocates a new generation: the same item now demands
        // different work.
        let hold = pool.arm_extract_hold();
        pool.enqueue(pool::WorkItem::extract(
            item_id,
            lib.id,
            video.clone(),
            Some(first_identity.clone()),
        ));
        assert_eq!(
            hold.entered_rx
                .recv_timeout(Duration::from_secs(10))
                .unwrap(),
            item_id,
            "the older extract must be in flight"
        );

        fs::write(
            &sidecar_path,
            "1\n00:00:00,000 --> 00:00:02,000\nA longer replacement body\n",
        )
        .unwrap();
        db.reconcile_item_sidecars(
            item_id,
            &[observed_sidecar(
                &sidecar_path,
                "Movie.en.srt",
                "s-en",
                "srt",
            )],
        )
        .unwrap();
        let second = db
            .certified_subtitle_source(item_id)
            .unwrap()
            .expect("certified source");
        let second_identity = second.source_identity();
        assert_ne!(first_identity, second_identity, "a new generation exists");
        assert_ne!(
            first_token,
            second.token_for_track("s-en").expect("second token")
        );

        // The newer demand while the older run is active is queued, not dropped.
        pool.prioritize_extract(item_id, lib.id, video);
        assert_eq!(
            pool.background_progress().queued_extracts,
            1,
            "a newer generation must be preserved as a successor"
        );

        hold.release_tx.send(()).unwrap();

        // The successor runs after the older item and publishes the new
        // generation. The older run could not execute as the newer generation:
        // it defers, so only the second token can ever become ready.
        pool.wait_extract_finishes(2);
        assert_eq!(
            db.get_item(item_id).unwrap().unwrap().subtitle_status,
            "ready",
            "the successor must publish"
        );
        let store = SubsStore::new(dir.path().join("subs")).unwrap();
        let published = committed_artifact_path(&db, &store, item_id, "s-en");
        assert_eq!(
            published.parent(),
            Some(
                store
                    .generation_dir(item_id, &second.token_for_track("s-en").unwrap())
                    .as_path()
            ),
            "the successor's generation must hold the committed artifact"
        );
        assert!(
            !store.generation_dir(item_id, &first_token).exists(),
            "the older generation must never be written by the successor's work"
        );
    }

    /// Round-1 item 8: an actual media replacement while the extract is
    /// provably in flight defers the run. Nothing is published, no status is
    /// written, and no failure backoff is consumed.
    #[test]
    fn media_replaced_while_extract_is_blocked_defers_without_artifacts() {
        let dir = tempfile::tempdir().unwrap();
        let media = dir.path().join("media");
        fs::create_dir_all(&media).unwrap();
        let video_src = corpus_fixture("h264_aac_srt_mkv.mkv");
        if !video_src.is_file() {
            eprintln!("skipping: missing {}", video_src.display());
            return;
        }
        let video = media.join("Movie.mkv");
        fs::copy(&video_src, &video).unwrap();

        let db = Arc::new(nightjar_db::open(dir.path()).unwrap());
        let pool = test_pool(&db, dir.path());
        let lib = db
            .create_library(&NewLibrary {
                name: "t".into(),
                path: media.to_string_lossy().into_owned(),
                kind: "movies".into(),
            })
            .unwrap();
        let ids = db
            .upsert_items_indexed(
                lib.id,
                &[UpsertItem {
                    path: "Movie.mkv".into(),
                    mtime_ms: 1,
                    size_bytes: 15,
                    title: "Movie".into(),
                    kind: "movie".into(),
                    year: None,
                    season: None,
                    episode: None,
                    content_id: None,
                }],
            )
            .unwrap();
        let item_id = ids[0];
        certify_item(&db, item_id, &text_streams(&video));
        let status_before = db.get_item(item_id).unwrap().unwrap().subtitle_status;
        let attempts_before = subtitle_attempt_count(&db, item_id);

        // Park the worker after it claimed the item, then replace the media.
        let hold = pool.arm_extract_hold();
        pool.enqueue(pool::WorkItem::extract(
            item_id,
            lib.id,
            video.clone(),
            None,
        ));
        assert_eq!(
            hold.entered_rx
                .recv_timeout(Duration::from_secs(10))
                .unwrap(),
            item_id,
            "the extract worker must claim the item"
        );
        fs::write(&video, b"a different, longer replacement body").unwrap();
        hold.release_tx.send(()).unwrap();
        pool.wait_extract_finishes(1);

        let row = db.get_item(item_id).unwrap().unwrap();
        assert_eq!(
            row.subtitle_status, status_before,
            "a media replacement must defer without writing a status"
        );
        assert_eq!(
            subtitle_attempt_count(&db, item_id),
            attempts_before,
            "a deferral must not consume failure backoff"
        );
        let store = SubsStore::new(dir.path().join("subs")).unwrap();
        assert!(
            !store.item_dir(item_id).exists(),
            "a deferred run must publish nothing"
        );
        assert_eq!(pool.background_progress().completed, 0);
    }

    /// Round-2 item 1 / D2B.2 acceptance 3: a completed artifact is immutable.
    /// A second run for the same `(item, track, token)` skips the committed
    /// track, so it never re-demuxes or renames over the finished bytes.
    #[test]
    fn same_token_second_run_never_overwrites_a_completed_artifact() {
        let dir = tempfile::tempdir().unwrap();
        let media = dir.path().join("media");
        fs::create_dir_all(&media).unwrap();
        let video = media.join("Movie.mp4");
        fs::write(&video, b"not a real mp4").unwrap();
        let sidecar_path = media.join("Movie.en.srt");
        let sidecar_src = corpus_fixture("sidecar_beside/Movie.en.srt");
        if !sidecar_src.is_file() {
            eprintln!("skipping: missing {}", sidecar_src.display());
            return;
        }
        fs::copy(&sidecar_src, &sidecar_path).unwrap();

        let db = Arc::new(nightjar_db::open(dir.path()).unwrap());
        let pool = test_pool(&db, dir.path());
        let lib = db
            .create_library(&NewLibrary {
                name: "t".into(),
                path: media.to_string_lossy().into_owned(),
                kind: "movies".into(),
            })
            .unwrap();
        let ids = db
            .upsert_items_indexed(
                lib.id,
                &[UpsertItem {
                    path: "Movie.mp4".into(),
                    mtime_ms: 1,
                    size_bytes: 15,
                    title: "Movie".into(),
                    kind: "movie".into(),
                    year: None,
                    season: None,
                    episode: None,
                    content_id: None,
                }],
            )
            .unwrap();
        let item_id = ids[0];
        db.reconcile_item_sidecars(
            item_id,
            &[observed_sidecar(
                &sidecar_path,
                "Movie.en.srt",
                "s-en",
                "srt",
            )],
        )
        .unwrap();
        certify_item(&db, item_id, &[]);

        pool.enqueue(pool::WorkItem::extract(
            item_id,
            lib.id,
            video.clone(),
            None,
        ));
        pool.wait_extract_finishes(1);
        let source = db.certified_subtitle_source(item_id).unwrap().unwrap();
        assert!(source.is_complete("s-en"), "the first run must commit");
        let store = SubsStore::new(dir.path().join("subs")).unwrap();
        let artifact = committed_artifact_path(&db, &store, item_id, "s-en");
        assert!(artifact.is_file(), "the first run must write the artifact");

        // The committed bytes are immutable. Overwrite them with a marker a
        // re-demux could never produce, then run again for the same token.
        fs::write(&artifact, "COMMITTED").unwrap();
        pool.enqueue(pool::WorkItem::extract(item_id, lib.id, video, None));
        pool.wait_complete_skips(1);

        assert_eq!(
            fs::read_to_string(&artifact).unwrap(),
            "COMMITTED",
            "a second run must not overwrite a completed artifact"
        );
        assert_eq!(
            db.get_item(item_id).unwrap().unwrap().subtitle_status,
            "ready",
            "the committed reference must survive the skipped run"
        );
    }

    /// Round-2 item 1: one sidecar edit must not re-demux or overwrite the
    /// unrelated embedded and sidecar tracks under their unchanged URLs.
    #[test]
    fn one_sidecar_edit_leaves_unrelated_artifacts_untouched() {
        let dir = tempfile::tempdir().unwrap();
        let media = dir.path().join("media");
        fs::create_dir_all(&media).unwrap();
        let video_src = corpus_fixture("h264_aac_srt_mkv.mkv");
        let sidecar_src = corpus_fixture("sidecar_beside/Movie.en.srt");
        if !video_src.is_file() || !sidecar_src.is_file() {
            eprintln!("skipping: missing D2B.2 fixtures");
            return;
        }
        let video = media.join("Movie.mkv");
        fs::copy(&video_src, &video).unwrap();
        let english = media.join("Movie.en.srt");
        let french = media.join("Movie.fr.srt");
        fs::copy(&sidecar_src, &english).unwrap();
        fs::copy(&sidecar_src, &french).unwrap();

        let db = Arc::new(nightjar_db::open(dir.path()).unwrap());
        let pool = test_pool(&db, dir.path());
        let lib = db
            .create_library(&NewLibrary {
                name: "t".into(),
                path: media.to_string_lossy().into_owned(),
                kind: "movies".into(),
            })
            .unwrap();
        let ids = db
            .upsert_items_indexed(
                lib.id,
                &[UpsertItem {
                    path: "Movie.mkv".into(),
                    mtime_ms: 1,
                    size_bytes: 15,
                    title: "Movie".into(),
                    kind: "movie".into(),
                    year: None,
                    season: None,
                    episode: None,
                    content_id: None,
                }],
            )
            .unwrap();
        let item_id = ids[0];
        db.reconcile_item_sidecars(
            item_id,
            &[
                observed_sidecar(&english, "Movie.en.srt", "s-en", "srt"),
                observed_sidecar(&french, "Movie.fr.srt", "s-fr", "srt"),
            ],
        )
        .unwrap();
        certify_item(&db, item_id, &text_streams(&video));

        pool.enqueue(pool::WorkItem::extract(
            item_id,
            lib.id,
            video.clone(),
            None,
        ));
        pool.wait_extract_finishes(1);
        let first = db.certified_subtitle_source(item_id).unwrap().unwrap();
        let embedded_token = first.token_for_track("e2").expect("embedded token");
        let french_token = first.token_for_track("s-fr").expect("french token");
        let english_token = first.token_for_track("s-en").expect("english token");
        let store = SubsStore::new(dir.path().join("subs")).unwrap();
        let embedded_artifact = committed_artifact_path(&db, &store, item_id, "e2");
        let french_artifact = committed_artifact_path(&db, &store, item_id, "s-fr");
        assert!(embedded_artifact.is_file(), "embedded artifact");
        assert!(french_artifact.is_file(), "french artifact");

        // Mark the two tracks that must survive the next run untouched with
        // bytes a re-demux could never produce.
        fs::write(&embedded_artifact, "COMMITTED-E2").unwrap();
        fs::write(&french_artifact, "COMMITTED-FR").unwrap();

        // Edit only the English sidecar and reconcile the change.
        fs::write(
            &english,
            "1\n00:00:00,000 --> 00:00:02,000\nA longer replacement body\n",
        )
        .unwrap();
        db.reconcile_item_sidecars(
            item_id,
            &[
                observed_sidecar(&english, "Movie.en.srt", "s-en", "srt"),
                observed_sidecar(&french, "Movie.fr.srt", "s-fr", "srt"),
            ],
        )
        .unwrap();
        let second = db.certified_subtitle_source(item_id).unwrap().unwrap();
        let english_second = second.token_for_track("s-en").expect("new english token");
        assert_ne!(english_token, english_second);
        assert_eq!(
            second.token_for_track("e2").as_deref(),
            Some(embedded_token.as_str()),
            "the embedded token is unchanged"
        );
        assert_eq!(
            second.token_for_track("s-fr").as_deref(),
            Some(french_token.as_str()),
            "the unrelated sidecar token is unchanged"
        );

        pool.enqueue(pool::WorkItem::extract(item_id, lib.id, video, None));
        pool.wait_extract_finishes(2);

        // The edited sidecar published under its new token; the unchanged
        // tracks were never re-demuxed or overwritten.
        assert_eq!(
            fs::read_to_string(&embedded_artifact).unwrap(),
            "COMMITTED-E2",
            "the embedded artifact must be untouched"
        );
        assert_eq!(
            fs::read_to_string(&french_artifact).unwrap(),
            "COMMITTED-FR",
            "the unrelated sidecar artifact must be untouched"
        );
        let english_artifact = committed_artifact_path(&db, &store, item_id, "s-en");
        assert_eq!(
            english_artifact.parent(),
            Some(store.generation_dir(item_id, &english_second).as_path()),
            "the edited sidecar publishes under its new generation"
        );
        let body = fs::read_to_string(&english_artifact).unwrap();
        assert!(body.contains("A longer replacement body"), "{body}");
        assert_eq!(
            db.get_item(item_id).unwrap().unwrap().subtitle_status,
            "ready"
        );
    }
}

#[cfg(test)]
mod folder_title_tests {
    use super::{stored_kind, stored_parse, stored_title, title_from_folder};
    use nightjar_core::{MediaKind, parse_filename};

    /// The scanner is the layer that has the folder. `parse_filename` only
    /// ever sees a basename, so a name carrying no title at all — `S01E04.mkv`
    /// — has to borrow one here.
    #[test]
    fn an_episode_borrows_its_show_folder() {
        for (rel, want) in [
            ("Anon Show/Season 1/S01E04.mkv", "Anon Show"),
            ("Anon Show (1988)/Season 12/S12E01.mkv", "Anon Show (1988)"),
            ("Anon Show/Specials/S00E01.mkv", "Anon Show"),
            // A nested layout takes the show folder, not the whole path.
            ("Kids/Anon Show/Season 2/2x03.mkv", "Anon Show"),
            // No season directory at all.
            ("Anon Show/1x04.mkv", "Anon Show"),
        ] {
            assert_eq!(title_from_folder(rel, "/media/TV"), want, "{rel}");
        }
    }

    /// **A file directly in the library root has no folder to borrow from**,
    /// and gets an empty title rather than the library's own name — which
    /// would be the same wrong answer for every such file. `drain_pending`
    /// then refuses to search on it.
    #[test]
    fn a_file_in_the_library_root_borrows_nothing() {
        assert_eq!(title_from_folder("S01E04.mkv", "/media/TV"), "");
    }

    /// **The folder decides, and only a numbered one.** `Closure.mkv` carries
    /// no season, no episode and nothing that says television, so the parser
    /// calls it a movie on the evidence it has. The scanner has the folder.
    ///
    /// Measured on the warmed oracle: this removes all 573 `wrong.kind` rows and
    /// moves nothing else across 81,094. On the real library it reclassified
    /// three `Top Gear` specials — `16x00`, episode zero, which the matcher
    /// rejects — and left the five `Specials/` files whose correct binding is a
    /// movie record exactly where they were.
    ///
    /// **The negative cases are not the whole guard.** The first cut of this
    /// test had four of them and no positive-but-wrong case, and the rule it
    /// guarded was false for every film a library files under `Season N/`. The
    /// Futurama block below is that case, taken from the real library.
    /// **The three paths this reconciliation exists for.** `16x00` makes
    /// `find_season_episode` decline, so the basename parses as a movie with no
    /// numbering; `stored_kind` calls it an episode because it sits under a
    /// numbered season directory; and until now nothing gave it the season.
    ///
    /// **Negative control:** remove the walk in [`stored_parse`] and all three
    /// lose their season.
    #[test]
    fn a_season_directory_supplies_the_season_an_episode_lacks() {
        let root = "/media/TV";
        for (path, season) in [
            (
                "Top Gear/Season 16/Top Gear - 16x00 -  The three wise men christmas special - 720p.mkv",
                16,
            ),
            (
                "Top Gear/Season 22/Top Gear - 22x00 - Special Patagonia Part One.mkv",
                22,
            ),
            (
                "Top Gear/Season 22/Top Gear - 22x00 - Special Patagonia Part Two.mkv",
                22,
            ),
        ] {
            let p = stored_parse(path, root);
            assert_eq!(p.kind, MediaKind::Episode, "{path}");
            assert_eq!(p.season, Some(season), "{path}");
            assert_eq!(p.episode, None, "episode 0 stays refused: {path}");
            // The title is untouched by this slice: the basename's own title
            // is not empty, so `stored_title` leaves it alone.
            assert!(p.title.starts_with("Top Gear"), "{path}");
        }
    }

    /// **A leading number is the episode once a season is known.** The season
    /// above the file is what licenses the claim, and it is evidence a basename
    /// cannot see.
    #[test]
    fn a_leading_number_is_the_episode_in_a_season_directory() {
        let root = "/media/TV";
        for (path, season, episode) in [
            ("Series/Season 01/01 Pilot (1080p HD).mkv", 1, 1),
            ("Series/Season 01/1 Pilot (1080p HD).mkv", 1, 1),
            ("Series/Season 1/02 Honor Thy Father (1080p HD).m4v", 1, 2),
            ("Series/Season 1/2 Honor Thy Developer (1080p HD).m4v", 1, 2),
        ] {
            let p = stored_parse(path, root);
            assert_eq!(p.kind, MediaKind::Episode, "{path}");
            assert_eq!(
                (p.season, p.episode),
                (Some(season), Some(episode)),
                "{path}"
            );
        }
    }

    /// **What the leading-number rule must not claim.**
    ///
    /// **Line 1 is the case a first draft got wrong.** The rule lived in the
    /// parser, where a basename is all there is, and
    /// `12.Angry.Men.1080p.BluRay.x264-GRP.mkv` — a yearless film — became
    /// episode 12. **The parser sweep renders 360 names beginning with a one-
    /// or two-digit run**, and nothing in a basename separates that from
    /// `01 Pilot`. The season directory does, so the rule moved here.
    ///
    /// **Negative controls.** Remove the year condition and line 2 claims.
    /// Remove the
    /// `episode.is_none()` condition and line 3 is overwritten. Remove
    /// `season.is_some()` and **both line 1 and line 4** claim — line 4 an
    /// episode with no season at all, which is the path
    /// `season_number_for_path`'s own doc predicts: a digit run too wide for a
    /// `u32` is a season directory and is not a season number. **Every field
    /// each guard covers is asserted** — kind, season and episode — because a
    /// guard applied to one field of a merged record is not applied to the
    /// record.
    #[test]
    fn a_leading_number_outside_a_season_directory_is_not_an_episode() {
        let root = "/media";
        // A yearless film, in a film's folder.
        let p = stored_parse(
            "Movies/12 Angry Men/12.Angry.Men.1080p.BluRay.x264-GRP.mkv",
            root,
        );
        assert_eq!(p.kind, MediaKind::Movie);
        assert_eq!((p.season, p.episode), (None, None));

        // A film that asserts its own year, even inside a season directory.
        let q = stored_parse("Series/Season 3/65 (2023) WEBDL-1080p.mkv", root);
        assert_eq!(q.kind, MediaKind::Movie);
        assert_eq!(
            (q.season, q.episode),
            (None, None),
            "a year says film wherever it sits"
        );

        // A name that already claims keeps its own numbers.
        let r = stored_parse("Series/Season 3/07 Show - 4x09 - Title.mkv", root);
        assert_eq!(
            (r.season, r.episode),
            (Some(4), Some(9)),
            "the basename's claim stands"
        );

        // **The one path where the two conditions diverge**, and
        // `season_number_for_path`'s own doc predicts it: a digit run too wide
        // for a `u32` **is** a season directory and **is not** a season number.
        // Without `season.is_some()` this claims episode 5 with no season —
        // a season-relative number and nothing to relate it to.
        let w = stored_parse("Series/Season 99999999999999999999/05 Title.mkv", root);
        assert_eq!(w.kind, MediaKind::Episode, "it is still a season directory");
        assert_eq!(
            (w.season, w.episode),
            (None, None),
            "and still not a season number"
        );
    }

    /// **A film under a numbered season directory gets no season, and the order
    /// of the two rules is what makes that true.**
    ///
    /// `stored_kind` keeps these as films because they carry their own year, and
    /// **deciding the kind before filling the season is the whole guard** — a
    /// first draft filled first and put season 5 on all four. `title_from_folder`
    /// names the first of them as exactly the thing not to do.
    ///
    /// **Negative control:** move the season fill above the kind decision and
    /// every line fails. **Every field the guard covers is asserted** — kind,
    /// season and episode — because a guard applied to one field of a merged
    /// record is not applied to the record.
    #[test]
    fn a_film_in_a_season_directory_gets_no_season() {
        let root = "/media/TV";
        for path in [
            "Futurama/Season 5/Futurama Bender's Big Score (2007).avi",
            "Futurama/Season 5/Futurama Bender's Game (2008).avi",
            "Futurama/Season 5/Futurama Into the Wild Green Yonder (2009).avi",
            "Futurama/Season 5/Futurama The Beast with a Billion Backs (2008).avi",
        ] {
            let p = stored_parse(path, root);
            assert_eq!(p.kind, MediaKind::Movie, "{path}");
            assert_eq!(p.season, None, "a season on a movie row: {path}");
            assert_eq!(p.episode, None, "{path}");
        }
    }

    /// **The walk reaches past a directory that is not a season directory** —
    /// the shape a merge taking only the immediate parent cannot see. The two
    /// real library paths of this shape carry their season in the basename, so
    /// the first line asserts they are unaffected.
    #[test]
    fn the_walk_reaches_a_season_directory_that_is_not_the_parent() {
        let root = "/media/TV";
        let p = stored_parse(
            "Show (2023)/Season 3/Show.S01E03.1080p.WEB.H264-CBFM/Sample/show.s01e03.1080p-sample.mkv",
            root,
        );
        assert_eq!(
            (p.season, p.episode),
            (Some(1), Some(3)),
            "the basename claims and wins"
        );

        // **The walk pops season directories, not arbitrary ones.** `Extras`
        // and `Specials` it knows; a release directory it does not, and it
        // stops there. That is `is_season_directory`'s rule and this slice does
        // not widen it.
        let q = stored_parse("Show (2023)/Season 3/Extras/whatever.mkv", root);
        assert_eq!(q.kind, MediaKind::Episode);
        assert_eq!(
            q.season,
            Some(3),
            "the walk pops `Extras` and finds the season"
        );

        let r = stored_parse("Show (2023)/Season 3/Some Release Dir/whatever.mkv", root);
        assert_eq!(
            r.season, None,
            "an unknown directory stops the walk, and it still does"
        );
    }

    /// **An absolute number refuses the walked season, exactly as it refuses a
    /// folder's.** `Season 2/Show - E56.mkv` is season 2 of the folder and
    /// episode 56 of the series; pairing them binds a slot that does not exist.
    ///
    /// **Negative control:** drop `!parsed.episode_absolute` and the season
    /// becomes `Some(2)`.
    #[test]
    fn an_absolute_number_refuses_the_walked_season() {
        let p = stored_parse("Show/Season 2/Show - E56.mkv", "/media/TV");
        assert!(p.episode_absolute);
        assert_eq!(p.episode, Some(56));
        assert_eq!(p.season, None, "different numbering schemes must not pair");
    }

    /// **`stored_title` and `stored_kind` still run, and on the merged record.**
    #[test]
    fn the_stored_record_still_borrows_the_folders_title() {
        let p = stored_parse("Anon Show/Season 1/S01E04.mkv", "/media/TV");
        assert_eq!(
            p.title, "Anon Show",
            "an empty title takes the show folder's name"
        );
        assert_eq!((p.season, p.episode), (Some(1), Some(4)));
        assert_eq!(p.kind, MediaKind::Episode);
    }

    #[test]
    fn a_numbered_season_directory_means_the_file_is_not_a_film() {
        use MediaKind::{Episode, Movie};
        let root = "/media/TV";

        // The population this exists for.
        assert_eq!(
            stored_kind(Movie, None, "Show/Season 1/Closure.mkv", root),
            "episode"
        );
        assert_eq!(
            stored_kind(Movie, None, "Show/S01/Closure.mkv", root),
            "episode"
        );

        // **The counterexample that killed the previous attempt.** A film in a
        // show's Specials folder stays a film.
        assert_eq!(
            stored_kind(Movie, None, "Top Gear/Specials/Polar Special.mkv", root),
            "movie",
            "TMDB models this as a movie record; a rule that says otherwise \
             destroyed five real bindings"
        );
        assert_eq!(
            stored_kind(Movie, None, "Top Gear/Extras/x.mkv", root),
            "movie"
        );

        // **The positive-but-wrong case: the film filed under a numbered
        // season.** Four of these sit in the dogfood library, each with its own
        // TMDB movie record, and the folder rule alone called every one an
        // episode — after which `episode_group_key` bound them to the Futurama
        // series with no route back. The basename asserts its own year, and no
        // episode title does.
        for (name, year) in [
            ("Futurama Bender's Big Score (2007).avi", 2007),
            ("Futurama Bender's Game (2008).avi", 2008),
            ("Futurama Into the Wild Green Yonder (2009).avi", 2009),
            ("Futurama The Beast with a Billion Backs (2008).avi", 2008),
        ] {
            let rel = format!("Futurama/Season 5/{name}");
            let p = parse_filename(name);
            assert_eq!(p.kind, Movie, "{name}");
            assert_eq!(p.year, Some(year), "{name}");
            assert_eq!(
                stored_kind(p.kind, p.year, &rel, root),
                "movie",
                "{rel} — the file says what it is; the folder is only evidence"
            );
        }

        // The year travels with the file, not the folder: the same basename
        // under a plain show folder is a film for the same reason.
        assert_eq!(
            stored_kind(
                Movie,
                Some(2007),
                "Futurama/Bender's Big Score (2007).avi",
                root
            ),
            "movie"
        );

        // A parsed episode is unaffected wherever it sits, and an ordinary
        // movie outside any season directory is untouched.
        assert_eq!(
            stored_kind(Episode, None, "Show/Season 1/Show - S01E01.mkv", root),
            "episode"
        );
        assert_eq!(
            stored_kind(Episode, None, "Top Gear/Specials/x.mkv", root),
            "episode"
        );
        assert_eq!(
            stored_kind(Movie, Some(1999), "Fight Club (1999)/Fight Club.mkv", root),
            "movie"
        );
    }

    /// The rule both indexing paths and the oracle's replay harness share.
    /// Asserted on the composed function rather than on its halves, because the
    /// harness was re-deriving this with `parse_filename` alone and the halves
    /// each looked right.
    #[test]
    fn stored_title_substitutes_the_folder_only_for_an_empty_parse() {
        // Titleless episode: the folder carries it.
        let p = parse_filename("S01E01.mkv");
        assert_eq!(p.title, "");
        assert_eq!(
            stored_title(
                p.title,
                "Anon Show (1988)/Season 01/S01E01.mkv",
                "/media/TV"
            ),
            "Anon Show (1988)"
        );
        // A parsed title always wins, and the folder is never consulted.
        let p = parse_filename("Anon Show - S01E01 - Pilot.mkv");
        assert!(!p.title.is_empty());
        assert_eq!(
            stored_title(
                p.title.clone(),
                "Other Folder (1999)/Season 01/x.mkv",
                "/media/TV"
            ),
            p.title
        );
        // No folder to borrow from stays empty, so the drain still declines to
        // search rather than searching on the library's name.
        let p = parse_filename("S01E01.mkv");
        assert_eq!(stored_title(p.title, "S01E01.mkv", "/media/TV"), "");
    }

    /// **Only an episode can reach the fallback**, which is why there is no
    /// movie path to borrow a containing folder. The parser's movie and
    /// season-pack arms substitute the stem, so their titles are never empty
    /// and the caller never asks.
    #[test]
    fn only_an_episode_can_reach_the_fallback() {
        for name in [
            "1080p.x264.mkv",
            "Anon Film (2019) Bluray-1080p.mkv",
            "Anon Show S01 1080p WEB-DL.mkv",
            "1x04.mkv",
            "S03E09 WS PDTV XviD FUtV.mkv",
        ] {
            let p = parse_filename(name);
            assert!(
                !p.title.is_empty() || p.episode.is_some(),
                "{name} reached the fallback as {:?}",
                p.kind
            );
        }
    }
}
