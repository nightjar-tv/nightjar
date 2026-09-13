//! The one query-layer kids filter (ADR-0037 items 5 and 7).
//!
//! Every item-returning read funnels its candidate media-item ids through
//! [`visible_item_ids`]. The scope parameter has no `Default` and is not
//! `Option`, so a new call site cannot compile without deciding. The filter
//! loads the facts in bounded batches over local SQLite; it never calls a
//! provider and never loops one query per item.
//!
//! One chunk is one joined statement (media item, its provider links, and the
//! canonical movie / episode / tv rows those links name), rather than the five
//! separate batch statements the first cut issued. [`VisibilityCache`] makes the
//! same facts reusable inside one request, so a caller that filters candidates
//! and then filters a detail's episodes pays the batch once.

use std::collections::{BTreeMap, HashMap, HashSet};

use nightjar_core::{
    CertificationLadder, ItemCertification, KidsScopeDecision, KidsScopeFacts, ViewerScope,
    decide_kids_scope,
};
use rusqlite::{Connection, params, params_from_iter};

use crate::negative_cache::PROVIDER_TMDB;

/// How many item ids one batched query carries. SQLite's default variable
/// limit is 999, so this leaves room for the statement's own binds.
pub const SCOPE_BATCH: usize = 400;

/// The facts one media item contributes to the decision.
#[derive(Debug, Clone, PartialEq, Eq)]
struct ItemFacts {
    metadata_ready: bool,
    certification: ItemCertification,
}

/// Request-local reuse for the batched visibility lookups.
///
/// The facts are keyed by media item id and are valid for one region, so the
/// cache clears itself if it is handed a different profile region. It lives for
/// one request and is dropped with it: nothing is persisted and nothing crosses
/// requests. Account scope performs no lookup and does not populate it.
#[derive(Default)]
pub struct VisibilityCache {
    region: Option<String>,
    facts: HashMap<i64, ItemFacts>,
}

impl VisibilityCache {
    pub fn new() -> Self {
        Self::default()
    }
}

/// The visible subset of `item_ids` for this viewer. Account scope returns
/// every id without a lookup; profile scope reads the stored facts and runs the
/// pure evaluator. This is the convenience entry point; a caller that makes
/// more than one visibility decision in a request holds a [`VisibilityCache`]
/// and calls [`visible_item_ids_cached`] instead.
pub fn visible_item_ids(
    conn: &Connection,
    scope: &ViewerScope,
    item_ids: &[i64],
) -> Result<HashSet<i64>, String> {
    let mut cache = VisibilityCache::new();
    visible_item_ids_cached(conn, scope, item_ids, &mut cache)
}

/// [`visible_item_ids`] with a caller-owned cache. Facts already loaded for
/// another id set in the same request are not queried again.
pub fn visible_item_ids_cached(
    conn: &Connection,
    scope: &ViewerScope,
    item_ids: &[i64],
    cache: &mut VisibilityCache,
) -> Result<HashSet<i64>, String> {
    match scope {
        ViewerScope::Account => Ok(item_ids.iter().copied().collect()),
        ViewerScope::Profile { cap, region } => {
            if cache.region.as_deref() != Some(region.as_str()) {
                cache.facts.clear();
                cache.region = Some(region.clone());
            }
            let missing: Vec<i64> = item_ids
                .iter()
                .copied()
                .filter(|id| !cache.facts.contains_key(id))
                .collect();
            for chunk in missing.chunks(SCOPE_BATCH) {
                load_chunk(conn, chunk, region, &mut cache.facts)?;
            }

            let ladder = CertificationLadder::shipped();
            let mut visible = HashSet::new();
            for id in item_ids {
                let Some(fact) = cache.facts.get(id) else {
                    continue;
                };
                let decision = decide_kids_scope(&KidsScopeFacts {
                    metadata_ready: fact.metadata_ready,
                    region,
                    cap: *cap,
                    certification: &fact.certification,
                    ladder: &ladder,
                });
                if decision == KidsScopeDecision::Visible {
                    visible.insert(*id);
                }
            }
            Ok(visible)
        }
    }
}

/// One item's visibility, for the direct item/bytes routes.
pub fn item_is_visible(
    conn: &Connection,
    scope: &ViewerScope,
    item_id: i64,
) -> Result<bool, String> {
    Ok(visible_item_ids(conn, scope, &[item_id])?.contains(&item_id))
}

/// Whether an artwork request key is visible to this viewer (ADR-0037 item 7).
///
/// The artwork route takes an opaque key, so it must resolve that key to the
/// media rows it authoritatively names before it reads or fetches a cached
/// image. Account scope is unrestricted; a capped profile may fetch only art
/// whose rows it can see. A key that resolves to no media row, and a key whose
/// rows are all hidden, both answer `false`, so hidden and missing are
/// indistinguishable at the route.
pub fn artwork_key_is_visible(
    conn: &Connection,
    scope: &ViewerScope,
    item_key: &str,
) -> Result<bool, String> {
    match scope {
        ViewerScope::Account => Ok(true),
        ViewerScope::Profile { .. } => {
            let ids = artwork_item_ids(conn, item_key)?;
            if ids.is_empty() {
                return Ok(false);
            }
            Ok(!visible_item_ids(conn, scope, &ids)?.is_empty())
        }
    }
}

/// The media-item ids an artwork request key names, through the same identity
/// grammar the item routes use (ADR-0025 §1, ADR-0039 items 2 and 6).
///
/// A provider movie/episode key names its linked media rows. A provisional
/// `tmdb:show:` key names the show entity, so its rows are the episodes whose
/// canonical row's `tmdb_show` is that entity, plus any rows under a folder
/// bound to it. A path key names its row directly; a `folder:` key names the
/// rows under that folder. Anything else names nothing.
fn artwork_item_ids(conn: &Connection, item_key: &str) -> Result<Vec<i64>, String> {
    use crate::item_links::{
        EPISODE_KEY_PREFIX, FOLDER_KEY_PREFIX, MOVIE_KEY_PREFIX, SHOW_KEY_PREFIX, parse_path_key,
    };

    if let Ok((library_id, relpath)) = parse_path_key(item_key) {
        return query_ids(
            conn,
            "SELECT id FROM media_items WHERE library_id = ?1 AND path = ?2",
            params![library_id, relpath],
        );
    }

    if item_key.starts_with(MOVIE_KEY_PREFIX) || item_key.starts_with(EPISODE_KEY_PREFIX) {
        return query_ids(
            conn,
            "SELECT media_item_id FROM media_item_links WHERE item_key = ?1",
            params![item_key],
        );
    }

    if let Some(id) = item_key.strip_prefix(SHOW_KEY_PREFIX) {
        let Ok(show_id) = id.parse::<i64>() else {
            return Ok(Vec::new());
        };
        let mut ids = query_ids(
            conn,
            "SELECT DISTINCT l.media_item_id
               FROM media_item_links l
               JOIN metadata_canonical c
                 ON c.provider = ?1 AND c.entity_kind = 'episode'
                AND c.provider_id = substr(l.item_key, ?2)
              WHERE l.item_key LIKE 'tmdb:episode:%' AND c.tmdb_show = ?3",
            params![PROVIDER_TMDB, EPISODE_KEY_PREFIX.len() as i64 + 1, show_id],
        )?;
        // A folder bound to the show can hold rows the entity edge does not
        // reach; browse advertises the show's art for them too.
        ids.extend(query_ids(
            conn,
            "SELECT m.id FROM media_items m
               JOIN series s ON s.library_id = m.library_id
              WHERE s.tmdb_show_id = ?1
                AND (m.path = s.relpath OR m.path LIKE s.relpath || '/%')",
            params![show_id],
        )?);
        ids.sort_unstable();
        ids.dedup();
        return Ok(ids);
    }

    if let Some(rest) = item_key.strip_prefix(FOLDER_KEY_PREFIX) {
        let Some((library_id, relpath)) = rest.split_once(':') else {
            return Ok(Vec::new());
        };
        let Ok(library_id) = library_id.parse::<i64>() else {
            return Ok(Vec::new());
        };
        return query_ids(
            conn,
            "SELECT id FROM media_items
              WHERE library_id = ?1 AND (path = ?2 OR path LIKE ?2 || '/%')",
            params![library_id, relpath],
        );
    }

    Ok(Vec::new())
}

fn query_ids(
    conn: &Connection,
    sql: &str,
    bindings: impl rusqlite::Params,
) -> Result<Vec<i64>, String> {
    let mut stmt = conn
        .prepare(sql)
        .map_err(|e| format!("prepare artwork identity: {e}"))?;
    let rows = stmt
        .query_map(bindings, |r| r.get::<_, i64>(0))
        .map_err(|e| format!("query artwork identity: {e}"))?;
    let mut out = Vec::new();
    for row in rows {
        out.push(row.map_err(|e| format!("artwork identity row: {e}"))?);
    }
    Ok(out)
}

fn placeholders(n: usize) -> String {
    let mut sql = String::from("?1");
    for index in 2..=n {
        sql.push_str(&format!(",?{index}"));
    }
    sql
}

/// Load the facts for one bounded id chunk in a single joined statement.
///
/// The join shape preserves the first cut's semantics exactly:
/// * an item with no link resolves to `Missing`;
/// * a `path:` link alongside a `tmdb:` link does not hide the provider link,
///   because only a row whose provider id matched contributes a decision;
/// * a `tmdb:movie:` link with no canonical movie row is `Missing`;
/// * a `tmdb:episode:` link with no canonical episode row is `Missing`, with a
///   canonical row and a NULL `tmdb_show` is `UnresolvedParent`, and with a
///   `tmdb_show` that names no canonical `tv` row is `UnresolvedParent`;
/// * a link for the wrong kind contributes nothing.
fn load_chunk(
    conn: &Connection,
    ids: &[i64],
    region: &str,
    out: &mut HashMap<i64, ItemFacts>,
) -> Result<(), String> {
    let list = placeholders(ids.len());
    let sql = format!(
        "SELECT m.id, m.kind, m.metadata_status,
                cm.provider_id, cm.certifications_json,
                ce.provider_id, ce.tmdb_show,
                ct.provider_id, ct.certifications_json
           FROM media_items m
           LEFT JOIN media_item_links l
                  ON l.media_item_id = m.id
           LEFT JOIN metadata_canonical cm
                  ON cm.provider = '{PROVIDER_TMDB}' AND cm.entity_kind = 'movie'
                 AND l.item_key LIKE 'tmdb:movie:%'
                 AND cm.provider_id = substr(l.item_key, 12)
           LEFT JOIN metadata_canonical ce
                  ON ce.provider = '{PROVIDER_TMDB}' AND ce.entity_kind = 'episode'
                 AND l.item_key LIKE 'tmdb:episode:%'
                 AND ce.provider_id = substr(l.item_key, 14)
           LEFT JOIN metadata_canonical ct
                  ON ct.provider = '{PROVIDER_TMDB}' AND ct.entity_kind = 'tv'
                 AND ct.provider_id = CAST(ce.tmdb_show AS TEXT)
          WHERE m.id IN ({list})"
    );
    let mut stmt = conn
        .prepare(&sql)
        .map_err(|e| format!("prepare scope items: {e}"))?;
    let rows = stmt
        .query_map(params_from_iter(ids.iter()), |r| {
            Ok((
                r.get::<_, i64>(0)?,
                r.get::<_, String>(1)?,
                r.get::<_, String>(2)?,
                r.get::<_, Option<String>>(3)?,
                r.get::<_, Option<String>>(4)?,
                r.get::<_, Option<String>>(5)?,
                r.get::<_, Option<i64>>(6)?,
                r.get::<_, Option<String>>(7)?,
                r.get::<_, Option<String>>(8)?,
            ))
        })
        .map_err(|e| format!("query scope items: {e}"))?;

    for row in rows {
        let (
            id,
            kind,
            metadata_status,
            movie_pid,
            movie_certs,
            episode_pid,
            episode_show,
            show_pid,
            show_certs,
        ) = row.map_err(|e| format!("scope item row: {e}"))?;
        let entry = out.entry(id).or_insert_with(|| ItemFacts {
            metadata_ready: metadata_status == "ready",
            certification: ItemCertification::Missing,
        });
        match kind.as_str() {
            "movie" if movie_pid.is_some() => {
                entry.certification = stored_certification(movie_certs.as_deref(), region);
            }
            "episode" if episode_pid.is_some() => {
                entry.certification = if episode_show.is_some() && show_pid.is_some() {
                    stored_certification(show_certs.as_deref(), region)
                } else {
                    ItemCertification::UnresolvedParent
                };
            }
            _ => {}
        }
    }
    Ok(())
}

/// Turn a stored `certifications_json` value into the evaluator's input. A NULL
/// column is missing; a malformed object is malformed; a well-formed object
/// without the server region is missing. A stored unrecognized label is a
/// label, and the ladder denies it as `unknown_label`.
fn stored_certification(json: Option<&str>, region: &str) -> ItemCertification {
    match json {
        None => ItemCertification::Missing,
        Some(text) => match serde_json::from_str::<BTreeMap<String, String>>(text) {
            Ok(map) => match map.get(region) {
                Some(label) if !label.trim().is_empty() => ItemCertification::Label(label.clone()),
                _ => ItemCertification::Missing,
            },
            Err(_) => ItemCertification::Malformed,
        },
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use nightjar_core::CertificationTier;
    use nightjar_db::migrate;
    use rusqlite::{Connection, params};

    fn mem() -> Connection {
        let c = Connection::open_in_memory().unwrap();
        migrate(&c).unwrap();
        c.execute_batch("INSERT INTO libraries (name, path, kind) VALUES ('L', '/L', 'movies');")
            .unwrap();
        c
    }

    fn insert_item(c: &Connection, id: i64, kind: &str, status: &str, library_id: i64) {
        c.execute(
            "INSERT INTO media_items
                (id, library_id, path, mtime_ms, size_bytes, title, kind, metadata_status)
             VALUES (?1, ?2, ?3, 1, 1, 'T', ?4, ?5)",
            params![id, library_id, format!("f{id}.mkv"), kind, status],
        )
        .unwrap();
    }

    fn insert_canonical(c: &Connection, kind: &str, pid: &str, certs: &str, show: Option<i64>) {
        c.execute(
            "INSERT INTO metadata_canonical
                (provider, entity_kind, provider_id, title, ids_json, projected_at,
                 certifications_json, tmdb_show)
             VALUES ('tmdb', ?1, ?2, 'T', '{}', 'now', ?3, ?4)",
            params![kind, pid, certs, show],
        )
        .unwrap();
    }

    fn link(c: &Connection, item_id: i64, key: &str) {
        c.execute(
            "INSERT INTO media_item_links (media_item_id, item_key) VALUES (?1, ?2)",
            params![item_id, key],
        )
        .unwrap();
    }

    fn profile_scope(cap: CertificationTier) -> ViewerScope {
        ViewerScope::Profile {
            cap,
            region: "US".into(),
        }
    }

    #[test]
    fn account_scope_sees_every_item_without_a_lookup() {
        let c = mem();
        insert_item(&c, 1, "movie", "pending", 1);
        let visible = visible_item_ids(&c, &ViewerScope::Account, &[1, 2, 3]).unwrap();
        assert_eq!(visible.len(), 3);
    }

    #[test]
    fn an_unknown_item_id_is_absent_rather_than_visible() {
        let c = mem();
        insert_item(&c, 1, "movie", "ready", 1);
        insert_canonical(&c, "movie", "10", r#"{"US":"G"}"#, None);
        link(&c, 1, "tmdb:movie:10");
        let visible =
            visible_item_ids(&c, &profile_scope(CertificationTier::LittleKid), &[1, 99]).unwrap();
        assert!(visible.contains(&1));
        assert!(!visible.contains(&99), "a missing row is not visible");
    }

    #[test]
    fn missing_and_unknown_and_over_cap_deny_a_movie() {
        let c = mem();
        insert_item(&c, 1, "movie", "ready", 1); // no link -> missing cert
        insert_item(&c, 2, "movie", "ready", 1);
        insert_canonical(&c, "movie", "20", r#"{"US":"UNRATED"}"#, None);
        link(&c, 2, "tmdb:movie:20");
        insert_item(&c, 3, "movie", "ready", 1);
        insert_canonical(&c, "movie", "30", r#"{"US":"R"}"#, None);
        link(&c, 3, "tmdb:movie:30");
        insert_item(&c, 4, "movie", "ready", 1);
        insert_canonical(&c, "movie", "40", r#"{"US":"G"}"#, None);
        link(&c, 4, "tmdb:movie:40");

        let scope = profile_scope(CertificationTier::LittleKid);
        let visible = visible_item_ids(&c, &scope, &[1, 2, 3, 4]).unwrap();
        assert_eq!(
            visible,
            HashSet::from([4]),
            "missing, unknown and over-cap all deny"
        );
    }

    #[test]
    fn a_not_ready_item_denies_even_with_a_perfect_label() {
        let c = mem();
        insert_item(&c, 1, "movie", "pending", 1);
        insert_canonical(&c, "movie", "10", r#"{"US":"G"}"#, None);
        link(&c, 1, "tmdb:movie:10");
        let visible = visible_item_ids(&c, &profile_scope(CertificationTier::Adult), &[1]).unwrap();
        assert!(visible.is_empty());
    }

    #[test]
    fn an_episode_inherits_its_show_certification_along_the_entity_edge() {
        let c = mem();
        insert_item(&c, 1, "episode", "ready", 1);
        insert_canonical(&c, "episode", "100", "{}", Some(9));
        insert_canonical(&c, "tv", "9", r#"{"US":"TV-PG"}"#, None);
        link(&c, 1, "tmdb:episode:100");

        assert!(
            visible_item_ids(&c, &profile_scope(CertificationTier::BigKid), &[1])
                .unwrap()
                .contains(&1)
        );
        assert!(
            visible_item_ids(&c, &profile_scope(CertificationTier::LittleKid), &[1])
                .unwrap()
                .is_empty(),
            "TV-PG is over a little-kid cap"
        );
    }

    #[test]
    fn an_episode_with_a_missing_or_unresolved_parent_denies() {
        let c = mem();
        // 1: episode row with no tv parent.
        insert_item(&c, 1, "episode", "ready", 1);
        insert_canonical(&c, "episode", "100", "{}", Some(9));
        link(&c, 1, "tmdb:episode:100");
        // 2: episode link but no episode canonical row at all.
        insert_item(&c, 2, "episode", "ready", 1);
        link(&c, 2, "tmdb:episode:200");
        // 3: episode row with a null parent.
        insert_item(&c, 3, "episode", "ready", 1);
        insert_canonical(&c, "episode", "300", "{}", None);
        link(&c, 3, "tmdb:episode:300");

        let visible =
            visible_item_ids(&c, &profile_scope(CertificationTier::Adult), &[1, 2, 3]).unwrap();
        assert!(visible.is_empty());
    }

    #[test]
    fn regional_isolation_denies_a_label_from_another_board() {
        let c = mem();
        insert_item(&c, 1, "movie", "ready", 1);
        insert_canonical(&c, "movie", "10", r#"{"AU":"MA 15+"}"#, None);
        link(&c, 1, "tmdb:movie:10");
        // The scope is US, so the AU-only label is unknown.
        let visible = visible_item_ids(&c, &profile_scope(CertificationTier::Adult), &[1]).unwrap();
        assert!(visible.is_empty());
    }

    #[test]
    fn malformed_stored_json_denies() {
        let c = mem();
        insert_item(&c, 1, "movie", "ready", 1);
        insert_canonical(&c, "movie", "10", "not json", None);
        link(&c, 1, "tmdb:movie:10");
        let visible = visible_item_ids(&c, &profile_scope(CertificationTier::Adult), &[1]).unwrap();
        assert!(visible.is_empty());
    }

    /// A `path:` link alongside a `tmdb:` link must not hide the provider link:
    /// the path row contributes nothing and the provider row resolves.
    #[test]
    fn a_path_link_beside_a_provider_link_does_not_hide_it() {
        let c = mem();
        insert_item(&c, 1, "movie", "ready", 1);
        insert_canonical(&c, "movie", "10", r#"{"US":"G"}"#, None);
        link(&c, 1, "path:1:f1.mkv");
        link(&c, 1, "tmdb:movie:10");
        let visible =
            visible_item_ids(&c, &profile_scope(CertificationTier::LittleKid), &[1]).unwrap();
        assert!(visible.contains(&1));
    }

    /// The batch is bounded but complete: more ids than one batch still all
    /// resolve. This is the positive control for the chunking.
    #[test]
    fn a_batch_larger_than_the_chunk_size_is_fully_considered() {
        let c = mem();
        let mut ids = Vec::new();
        for index in 0..(SCOPE_BATCH as i64 + 5) {
            let id = index + 1;
            insert_item(&c, id, "movie", "ready", 1);
            insert_canonical(&c, "movie", &id.to_string(), r#"{"US":"G"}"#, None);
            link(&c, id, &format!("tmdb:movie:{id}"));
            ids.push(id);
        }
        let visible =
            visible_item_ids(&c, &profile_scope(CertificationTier::LittleKid), &ids).unwrap();
        assert_eq!(visible.len(), ids.len());
    }

    /// The cache is request-local and reuses loaded facts across calls with an
    /// overlapping id set. The second call must not re-query the ids it already
    /// holds; the observable is that a row deleted between calls stays resolved
    /// from the cache.
    #[test]
    fn the_visibility_cache_reuses_loaded_facts() {
        let c = mem();
        insert_item(&c, 1, "movie", "ready", 1);
        insert_canonical(&c, "movie", "10", r#"{"US":"G"}"#, None);
        link(&c, 1, "tmdb:movie:10");
        let scope = profile_scope(CertificationTier::LittleKid);

        let mut cache = VisibilityCache::new();
        let first = visible_item_ids_cached(&c, &scope, &[1], &mut cache).unwrap();
        assert!(first.contains(&1));
        // Removing the item would change a fresh lookup; the cached fact is
        // reused instead, which is what makes the second call cheap.
        c.execute("DELETE FROM media_items WHERE id = 1", [])
            .unwrap();
        let second = visible_item_ids_cached(&c, &scope, &[1], &mut cache).unwrap();
        assert!(second.contains(&1), "the fact came from the cache");
        // A fresh call sees the removal.
        let fresh = visible_item_ids(&c, &scope, &[1]).unwrap();
        assert!(fresh.is_empty());
    }
}
