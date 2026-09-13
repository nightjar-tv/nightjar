//! Project stored provider board labels into `metadata_canonical`
//! (ADR-0037 item 8).
//!
//! TMDB's `release_dates` (movie) and `content_ratings` (tv) are already in the
//! append set and already stored in `metadata_raw_payloads`, so no third
//! full-library provider pass is needed. This module parses the stored payload
//! in the same transaction as the canonical write, and back-fills rows that
//! were projected before the column existed.
//!
//! **Regional aggregation is conservative and independent** (ADR-0037 item 8,
//! as corrected after measurement). All entries for one region are combined
//! before any decision: identical non-empty labels collapse; when every
//! distinct label is recognized by the shipped ladder the most restrictive rung
//! is stored; any unrecognized non-empty label denies that region only, and one
//! deterministic unrecognized label is stored so the evaluator reports
//! `unknown_label`. A conflict or unknown label in one country never erases
//! another country's usable projection, and there is no cross-region fallback.

use std::collections::BTreeMap;

use nightjar_core::CertificationLadder;
use rusqlite::{Connection, Transaction, params};
use serde_json::Value;

/// Region → non-empty raw board label, decided in ADR-0037 item 8.
pub type Certifications = BTreeMap<String, String>;

/// The reduction policy this module writes. Version 0 is unprocessed or legacy;
/// increment this whenever the reduction semantics below change, so a stored
/// row can be told apart from a stale one without hashing in playback.
pub const CERTIFICATION_PROJECTION_VERSION: i64 = 1;

/// Parse the board labels from a stored movie/tv detail payload.
///
/// Empty labels are discarded, regions are uppercased and kept separate, and
/// every entry for a region is combined before the decision, so the outcome
/// does not depend on entry order. The result is never an error: a region whose
/// combined labels are all recognized stores its most restrictive rung, and a
/// region with any unrecognized label stores one deterministic unrecognized
/// label (the lexicographically smallest) so the evaluator denies it as
/// `unknown_label` while a sibling region stays usable.
pub fn parse_provider_certifications(
    ladder: &CertificationLadder,
    entity_kind: &str,
    data: &Value,
) -> Certifications {
    let entries = match entity_kind {
        "movie" => data
            .pointer("/release_dates/results")
            .and_then(Value::as_array)
            .map(|results| {
                results
                    .iter()
                    .map(|entry| {
                        let region = entry.get("iso_3166_1").and_then(Value::as_str);
                        let labels = entry
                            .get("release_dates")
                            .and_then(Value::as_array)
                            .map(|dates| {
                                dates
                                    .iter()
                                    .filter_map(|d| d.get("certification").and_then(Value::as_str))
                                    .map(str::trim)
                                    .filter(|s| !s.is_empty())
                                    .map(str::to_string)
                                    .collect::<Vec<_>>()
                            })
                            .unwrap_or_default();
                        (region, labels)
                    })
                    .collect::<Vec<_>>()
            }),
        "tv" => data
            .pointer("/content_ratings/results")
            .and_then(Value::as_array)
            .map(|results| {
                results
                    .iter()
                    .map(|entry| {
                        let region = entry.get("iso_3166_1").and_then(Value::as_str);
                        let labels = entry
                            .get("rating")
                            .and_then(Value::as_str)
                            .map(str::trim)
                            .filter(|s| !s.is_empty())
                            .map(|s| vec![s.to_string()])
                            .unwrap_or_default();
                        (region, labels)
                    })
                    .collect::<Vec<_>>()
            }),
        _ => None,
    };
    let Some(entries) = entries else {
        return Certifications::new();
    };

    let mut by_region: BTreeMap<String, Vec<String>> = BTreeMap::new();
    for (region, labels) in entries {
        let Some(region) = region else { continue };
        let region = region.trim().to_uppercase();
        if region.is_empty() {
            continue;
        }
        by_region.entry(region).or_default().extend(labels);
    }

    let mut out = Certifications::new();
    for (region, labels) in by_region {
        let mut unique: Vec<String> = Vec::new();
        for label in labels {
            if !unique.contains(&label) {
                unique.push(label);
            }
        }
        if unique.is_empty() {
            continue;
        }

        // Any unrecognized label denies the region; store the lexicographically
        // smallest one so the outcome is deterministic and the evaluator reads
        // `unknown_label`. A recognized label is deliberately not stored.
        let mut unrecognized: Option<String> = None;
        for label in &unique {
            if ladder.tier_for(&region, label).is_none() {
                match &unrecognized {
                    Some(current) if current <= label => {}
                    _ => unrecognized = Some(label.clone()),
                }
            }
        }
        if let Some(label) = unrecognized {
            out.insert(region, label);
            continue;
        }

        // Every distinct label is recognized. The ladder's rungs are ordered
        // least-to-most permissive and validated as non-decreasing, so the last
        // rung whose label appears here is both the most restrictive tier and,
        // for a tie within one tier, the latest position in the ladder.
        let mut chosen: Option<String> = None;
        if let Some(region_ladder) = ladder.region(&region) {
            for rung in region_ladder.rungs() {
                if unique.iter().any(|label| label == &rung.label) {
                    chosen = Some(rung.label.clone());
                }
            }
        }
        if let Some(label) = chosen {
            out.insert(region, label);
        }
    }
    out
}

/// How many stored regions carry a label the shipped ladder does not recognize.
/// The row is still processed and stored (see [`parse_provider_certifications`]);
/// this is the observability count the drain reports.
pub fn count_unknown_regions(ladder: &CertificationLadder, certifications: &Certifications) -> i64 {
    certifications
        .iter()
        .filter(|(region, label)| ladder.tier_for(region, label).is_none())
        .count() as i64
}

/// Decide whether a stored projection must be recomputed against the current
/// policy and raw payload. Pure, and NULL-safe: a processed no-payload row
/// legitimately keeps a NULL hash on both sides and must not be reprojected, or
/// the bounded back-fill would never terminate.
pub fn needs_reprojection(
    stored_version: i64,
    stored_json: Option<&str>,
    stored_hash: Option<&str>,
    current_hash: Option<&str>,
) -> bool {
    stored_version != CERTIFICATION_PROJECTION_VERSION
        || stored_json.is_none()
        || !source_hash_matches(stored_hash, current_hash)
}

/// `None` matches `None` (no payload before and after); `Some` matches `Some`
/// only when equal; a payload appearing or disappearing is a change.
fn source_hash_matches(stored: Option<&str>, current: Option<&str>) -> bool {
    match (stored, current) {
        (None, None) => true,
        (Some(stored), Some(current)) => stored == current,
        _ => false,
    }
}

/// Write the projected object, the policy version and the source hash in one
/// statement, so the processed marker never advances without its result. `{}`
/// means "projected, no usable label"; NULL JSON means "not projected yet".
/// `source_sha256` is `None` only for a row with no stored payload.
pub fn write_certifications(
    tx: &Transaction<'_>,
    provider: &str,
    entity_kind: &str,
    provider_id: &str,
    certifications: &Certifications,
    source_sha256: Option<&str>,
) -> Result<(), String> {
    let json =
        serde_json::to_string(certifications).map_err(|e| format!("certifications JSON: {e}"))?;
    tx.execute(
        "UPDATE metadata_canonical
            SET certifications_json = ?1,
                certifications_projection_version = ?2,
                certifications_source_sha256 = ?3
          WHERE provider = ?4 AND entity_kind = ?5 AND provider_id = ?6",
        params![
            json,
            CERTIFICATION_PROJECTION_VERSION,
            source_sha256,
            provider,
            entity_kind,
            provider_id
        ],
    )
    .map_err(|e| format!("write certifications: {e}"))?;
    Ok(())
}

#[derive(Debug, Default, Clone, Copy, PartialEq, Eq)]
pub struct CertificationBackfillStats {
    pub scanned: i64,
    /// Rows whose projection (label or `{}`) was written this pass.
    pub updated: i64,
    /// Rows with no stored raw payload; they store `{}`, version 1, NULL hash.
    pub no_payload: i64,
    /// Rows whose stored payload was not JSON; they store `{}`, version 1 and
    /// the payload's byte hash rather than rolling the write back.
    pub malformed: i64,
    /// Regions across scanned rows whose stored label is unrecognized.
    pub unknown_regions: i64,
}

/// Process one bounded batch of canonical movie/tv rows in
/// `(entity_kind, provider_id)` order, starting after `after`.
///
/// The cursor walks **every** movie/tv row, not only the state-stale ones,
/// because a raw payload can be replaced without its projection changing. The
/// per-row decision is [`needs_reprojection`], which compares the stored source
/// hash with the hash of the raw payload bytes read now. A row at the current
/// version whose hash matches is skipped before the payload is parsed, so an
/// unchanged row costs one payload read and one SHA-256, never a parse or a
/// write. A projection or storage failure returns `Err` before the write
/// commits, so the row keeps its stale state and is retried on the next run.
///
/// Returns the stats and the last cursor seen. The caller repeats with that
/// cursor until `scanned == 0`, which terminates because the cursor only moves
/// forward and the final batch leaves it past the last row. A fresh call starts
/// a new sweep; it re-reads every row's hash but writes nothing that is already
/// current.
pub fn backfill_certifications(
    conn: &Connection,
    provider: &str,
    after: Option<(&str, &str)>,
    batch: i64,
) -> Result<(CertificationBackfillStats, Option<(String, String)>), String> {
    let (after_kind, after_id) = match after {
        Some((kind, id)) => (Some(kind), Some(id)),
        None => (None, None),
    };
    let mut stmt = conn
        .prepare(
            "SELECT c.entity_kind, c.provider_id,
                    c.certifications_projection_version,
                    c.certifications_json,
                    c.certifications_source_sha256
               FROM metadata_canonical c
              WHERE c.provider = ?1
                AND c.entity_kind IN ('movie', 'tv')
                AND (?2 IS NULL OR c.entity_kind > ?2
                     OR (c.entity_kind = ?2 AND c.provider_id > ?3))
              ORDER BY c.entity_kind, c.provider_id
              LIMIT ?4",
        )
        .map_err(|e| format!("prepare certification backfill: {e}"))?;
    let rows = stmt
        .query_map(params![provider, after_kind, after_id, batch], |r| {
            Ok((
                r.get::<_, String>(0)?,
                r.get::<_, String>(1)?,
                r.get::<_, i64>(2)?,
                r.get::<_, Option<String>>(3)?,
                r.get::<_, Option<String>>(4)?,
            ))
        })
        .map_err(|e| format!("query certification backfill: {e}"))?;

    let ladder = CertificationLadder::shipped();
    let mut stats = CertificationBackfillStats::default();
    let mut cursor: Option<(String, String)> = None;
    for row in rows {
        let (entity_kind, provider_id, stored_version, stored_json, stored_hash) =
            row.map_err(|e| format!("certification backfill row: {e}"))?;
        stats.scanned += 1;
        cursor = Some((entity_kind.clone(), provider_id.clone()));

        // Hash first, decide second: an unchanged row never pays for a parse.
        let payload =
            crate::raw_payload::get_raw_payload(conn, provider, &entity_kind, &provider_id)?;
        let current_hash = payload
            .as_ref()
            .map(|body| nightjar_db::sha256_hex(body.as_bytes()));

        if !needs_reprojection(
            stored_version,
            stored_json.as_deref(),
            stored_hash.as_deref(),
            current_hash.as_deref(),
        ) {
            continue;
        }

        let (certifications, no_payload, malformed) = match &payload {
            None => (Certifications::new(), true, false),
            Some(body) => match serde_json::from_str::<Value>(body) {
                Ok(data) => (
                    parse_provider_certifications(&ladder, &entity_kind, &data),
                    false,
                    false,
                ),
                Err(_) => (Certifications::new(), false, true),
            },
        };

        let tx = conn
            .unchecked_transaction()
            .map_err(|e| format!("begin certification backfill tx: {e}"))?;
        write_certifications(
            &tx,
            provider,
            &entity_kind,
            &provider_id,
            &certifications,
            current_hash.as_deref(),
        )?;
        tx.commit()
            .map_err(|e| format!("commit certification backfill: {e}"))?;

        stats.updated += 1;
        if no_payload {
            stats.no_payload += 1;
        }
        if malformed {
            stats.malformed += 1;
        }
        stats.unknown_regions += count_unknown_regions(&ladder, &certifications);
    }
    Ok((stats, cursor))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::negative_cache::PROVIDER_TMDB;
    use crate::tmdb::{RawProviderPayload, map_movie_detail, map_tv_detail};
    use nightjar_db::migrate;
    use rusqlite::Connection;

    fn mem() -> Connection {
        let c = Connection::open_in_memory().unwrap();
        migrate(&c).unwrap();
        c
    }

    fn ladder() -> CertificationLadder {
        CertificationLadder::shipped()
    }

    fn movie_payload_for(id: i64, release_dates: Value) -> Value {
        serde_json::json!({
            "id": id,
            "title": "Fight Club",
            "release_date": "1999-10-15",
            "release_dates": release_dates,
        })
    }

    fn movie_payload(release_dates: Value) -> Value {
        movie_payload_for(550, release_dates)
    }

    fn parse_movie(release_dates: Value) -> Certifications {
        parse_provider_certifications(&ladder(), "movie", &movie_payload(release_dates))
    }

    fn parse_tv(results: Value) -> Certifications {
        parse_provider_certifications(
            &ladder(),
            "tv",
            &serde_json::json!({
                "id": 1,
                "name": "Show",
                "content_ratings": {"results": results},
            }),
        )
    }

    #[test]
    fn non_empty_labels_are_kept_per_region() {
        let certs = parse_movie(serde_json::json!({
            "results": [
                {"iso_3166_1": "US", "release_dates": [{"certification": "R"}]},
                {"iso_3166_1": "DE", "release_dates": [{"certification": "16"}]},
            ]
        }));
        assert_eq!(certs.get("US").map(String::as_str), Some("R"));
        assert_eq!(certs.get("DE").map(String::as_str), Some("16"));
    }

    #[test]
    fn empty_labels_are_discarded_and_absent_regions_are_omitted() {
        let certs = parse_movie(serde_json::json!({
            "results": [
                {"iso_3166_1": "US", "release_dates": [{"certification": ""}]},
                {"iso_3166_1": "DE", "release_dates": [{"certification": "  "}]},
                {"iso_3166_1": "FR", "release_dates": []}
            ]
        }));
        assert!(certs.is_empty(), "{certs:?}");
    }

    #[test]
    fn duplicate_identical_labels_collapse() {
        let certs = parse_movie(serde_json::json!({
            "results": [
                {"iso_3166_1": "US", "release_dates": [
                    {"certification": "PG-13"},
                    {"certification": "PG-13"}
                ]}
            ]
        }));
        assert_eq!(certs.get("US").map(String::as_str), Some("PG-13"));
    }

    #[test]
    fn identical_labels_across_entries_collapse() {
        let certs = parse_movie(serde_json::json!({
            "results": [
                {"iso_3166_1": "US", "release_dates": [{"certification": "PG-13"}]},
                {"iso_3166_1": "US", "release_dates": [{"certification": "PG-13"}]}
            ]
        }));
        assert_eq!(certs.get("US").map(String::as_str), Some("PG-13"));
    }

    /// The most restrictive recognized label wins, and a tie inside one tier is
    /// broken by the latest position in that region's ladder. AU's `MA 15+` is
    /// teen and `R 18+` is adult, so adult wins; the two `little_kid` labels
    /// resolve to the later rung.
    #[test]
    fn most_restrictive_recognized_label_wins() {
        let certs = parse_movie(serde_json::json!({
            "results": [
                {"iso_3166_1": "AU", "release_dates": [{"certification": "MA 15+"}]},
                {"iso_3166_1": "AU", "release_dates": [{"certification": "R 18+"}]},
                {"iso_3166_1": "AU", "release_dates": [{"certification": "G"}]},
            ]
        }));
        assert_eq!(certs.get("AU").map(String::as_str), Some("R 18+"));
    }

    /// Same tier, two rungs: AU `G` is the first rung and `PG` the second, both
    /// little_kid, so the later position wins.
    #[test]
    fn a_tie_within_one_tier_uses_the_latest_ladder_position() {
        let certs = parse_movie(serde_json::json!({
            "results": [
                {"iso_3166_1": "AU", "release_dates": [
                    {"certification": "G"}, {"certification": "PG"}
                ]}
            ]
        }));
        assert_eq!(certs.get("AU").map(String::as_str), Some("PG"));
    }

    #[test]
    fn order_does_not_decide_the_outcome() {
        let first = parse_movie(serde_json::json!({
            "results": [
                {"iso_3166_1": "AU", "release_dates": [{"certification": "MA 15+"}]},
                {"iso_3166_1": "AU", "release_dates": [{"certification": "R 18+"}]},
            ]
        }));
        let second = parse_movie(serde_json::json!({
            "results": [
                {"iso_3166_1": "AU", "release_dates": [{"certification": "R 18+"}]},
                {"iso_3166_1": "AU", "release_dates": [{"certification": "MA 15+"}]},
            ]
        }));
        assert_eq!(first, second);
        assert_eq!(first.get("AU").map(String::as_str), Some("R 18+"));
    }

    /// An unknown nonempty label denies only its region, and the deterministic
    /// unrecognized label is the lexicographically smallest. The sibling region
    /// keeps its recognized label.
    #[test]
    fn an_unknown_label_denies_only_its_region() {
        let certs = parse_movie(serde_json::json!({
            "results": [
                {"iso_3166_1": "US", "release_dates": [{"certification": "R"}]},
                {"iso_3166_1": "DE", "release_dates": [
                    {"certification": "ZZ"},
                    {"certification": "AA"},
                    {"certification": "16"}
                ]},
            ]
        }));
        assert_eq!(certs.get("US").map(String::as_str), Some("R"));
        assert_eq!(
            certs.get("DE").map(String::as_str),
            Some("AA"),
            "the lexicographically smallest unrecognized label is stored"
        );
        assert_eq!(count_unknown_regions(&ladder(), &certs), 1);
    }

    /// A region not in the ladder is unknown for all its labels; a known region
    /// with an unknown label is still stored as unknown.
    #[test]
    fn an_unknown_region_denies_as_unknown() {
        let certs = parse_movie(serde_json::json!({
            "results": [
                {"iso_3166_1": "ZZ", "release_dates": [{"certification": "G"}]},
                {"iso_3166_1": "AU", "release_dates": [{"certification": "G"}]},
            ]
        }));
        assert_eq!(certs.get("ZZ").map(String::as_str), Some("G"));
        assert_eq!(certs.get("AU").map(String::as_str), Some("G"));
        assert_eq!(count_unknown_regions(&ladder(), &certs), 1);
    }

    #[test]
    fn tv_reads_content_ratings_and_uses_the_same_reduction() {
        let certs = parse_tv(serde_json::json!([
            {"iso_3166_1": "US", "rating": "TV-PG"},
            {"iso_3166_1": "US", "rating": "TV-MA"},
            {"iso_3166_1": "US", "rating": "TV-MA"}
        ]));
        assert_eq!(certs.get("US").map(String::as_str), Some("TV-MA"));
    }

    #[test]
    fn tv_unknown_region_does_not_erase_a_known_one() {
        let certs = parse_tv(serde_json::json!([
            {"iso_3166_1": "DE", "rating": "UNRATED"},
            {"iso_3166_1": "US", "rating": "TV-PG"}
        ]));
        assert_eq!(certs.get("DE").map(String::as_str), Some("UNRATED"));
        assert_eq!(certs.get("US").map(String::as_str), Some("TV-PG"));
    }

    #[test]
    fn a_payload_without_the_appendix_has_no_labels() {
        let data = serde_json::json!({"id": 550, "title": "Fight Club"});
        assert!(parse_provider_certifications(&ladder(), "movie", &data).is_empty());
        assert!(parse_provider_certifications(&ladder(), "tv", &data).is_empty());
    }

    #[test]
    fn needs_reprojection_is_null_safe() {
        // Current version, JSON present, both hashes absent: a processed
        // no-payload row. Reprojecting it would loop forever.
        assert!(!needs_reprojection(1, Some("{}"), None, None));
        assert!(!needs_reprojection(1, Some("{}"), Some("aa"), Some("aa")));
        // Version differs.
        assert!(needs_reprojection(0, Some("{}"), Some("aa"), Some("aa")));
        // JSON is NULL.
        assert!(needs_reprojection(1, None, Some("aa"), Some("aa")));
        // Hash appears, disappears, or differs.
        assert!(needs_reprojection(1, Some("{}"), None, Some("aa")));
        assert!(needs_reprojection(1, Some("{}"), Some("aa"), None));
        assert!(needs_reprojection(1, Some("{}"), Some("aa"), Some("bb")));
    }

    fn persist(conn: &Connection, payload: Value, entity_kind: &str, id: &str) {
        let raw = RawProviderPayload {
            entity_kind: entity_kind.into(),
            provider_id: id.into(),
            payload: payload.to_string(),
        };
        let meta = match entity_kind {
            "movie" => map_movie_detail(&payload).unwrap(),
            "tv" => map_tv_detail(&payload).unwrap(),
            _ => unreachable!(),
        };
        crate::canonical::persist_mapped_hit(conn, PROVIDER_TMDB, &raw, &meta).unwrap();
    }

    fn stored(conn: &Connection, kind: &str, id: &str) -> Option<String> {
        conn.query_row(
            "SELECT certifications_json FROM metadata_canonical
              WHERE provider = ?1 AND entity_kind = ?2 AND provider_id = ?3",
            params![PROVIDER_TMDB, kind, id],
            |r| r.get(0),
        )
        .unwrap()
    }

    fn processed(conn: &Connection, kind: &str, id: &str) -> (i64, Option<String>, Option<String>) {
        conn.query_row(
            "SELECT certifications_projection_version, certifications_json,
                    certifications_source_sha256
               FROM metadata_canonical
              WHERE provider = ?1 AND entity_kind = ?2 AND provider_id = ?3",
            params![PROVIDER_TMDB, kind, id],
            |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?)),
        )
        .unwrap()
    }

    fn drain_to_completion(conn: &Connection) -> CertificationBackfillStats {
        let mut total = CertificationBackfillStats::default();
        let mut cursor: Option<(String, String)> = None;
        // A bound that cannot be hit by a terminating pass: 1000 batches of 100
        // for a handful of rows. If the predicate looped, this test fails by
        // panicking rather than hanging.
        for _ in 0..1000 {
            let after = cursor.as_ref().map(|(k, id)| (k.as_str(), id.as_str()));
            let (stats, next) = backfill_certifications(conn, PROVIDER_TMDB, after, 100).unwrap();
            total.scanned += stats.scanned;
            total.updated += stats.updated;
            total.no_payload += stats.no_payload;
            total.malformed += stats.malformed;
            total.unknown_regions += stats.unknown_regions;
            if stats.scanned == 0 {
                return total;
            }
            cursor = next;
        }
        panic!("certification backfill did not terminate");
    }

    #[test]
    fn the_projection_transaction_writes_json_version_and_hash() {
        let c = mem();
        persist(
            &c,
            movie_payload(serde_json::json!({
                "results": [{"iso_3166_1": "US", "release_dates": [{"certification": "R"}]}]
            })),
            "movie",
            "550",
        );
        let json = stored(&c, "movie", "550").unwrap();
        let map: BTreeMap<String, String> = serde_json::from_str(&json).unwrap();
        assert_eq!(map.get("US").map(String::as_str), Some("R"));
        let (version, json, hash) = processed(&c, "movie", "550");
        assert_eq!(version, CERTIFICATION_PROJECTION_VERSION);
        assert!(json.is_some());
        let expected = nightjar_db::sha256_hex(
            crate::raw_payload::get_raw_payload(&c, PROVIDER_TMDB, "movie", "550")
                .unwrap()
                .unwrap()
                .as_bytes(),
        );
        assert_eq!(hash.as_deref(), Some(expected.as_str()));
    }

    /// A present payload that is not JSON writes `{}`, version 1 and its byte
    /// hash rather than rolling the projection back. The canonical row is
    /// supplied by the caller, so the write still commits.
    #[test]
    fn a_malformed_present_payload_writes_empty_version_and_hash() {
        let c = mem();
        let payload = serde_json::json!({
            "id": 550,
            "title": "Fight Club",
            "release_date": "1999-10-15"
        });
        let meta = map_movie_detail(&payload).unwrap();
        let raw = RawProviderPayload {
            entity_kind: "movie".into(),
            provider_id: "550".into(),
            payload: "not json".into(),
        };
        crate::canonical::persist_mapped_hit(&c, PROVIDER_TMDB, &raw, &meta).unwrap();
        let (version, json, hash) = processed(&c, "movie", "550");
        assert_eq!(version, CERTIFICATION_PROJECTION_VERSION);
        assert_eq!(json.as_deref(), Some("{}"));
        assert_eq!(
            hash.as_deref(),
            Some(nightjar_db::sha256_hex(b"not json").as_str())
        );
    }

    /// A no-payload row written by the projection stores `{}`, version 1 and a
    /// NULL hash.
    #[test]
    fn a_no_payload_row_writes_empty_version_and_null_hash() {
        let c = mem();
        c.execute(
            "INSERT INTO metadata_canonical
                (provider, entity_kind, provider_id, title, ids_json, projected_at)
             VALUES ('tmdb', 'movie', '550', 'T', '{}', 'now')",
            [],
        )
        .unwrap();
        let stats = drain_to_completion(&c);
        assert_eq!(stats.no_payload, 1);
        let (version, json, hash) = processed(&c, "movie", "550");
        assert_eq!(version, CERTIFICATION_PROJECTION_VERSION);
        assert_eq!(json.as_deref(), Some("{}"));
        assert_eq!(hash, None);
        // The no-payload row is still visited by the next full hash sweep, but
        // it is not rewritten and the sweep terminates.
        let second = drain_to_completion(&c);
        assert_eq!(second.scanned, 1, "the sweep visits the row");
        assert_eq!(
            second.updated, 0,
            "an unchanged no-payload row is not rewritten"
        );
    }

    /// Reprojection fires on a stale version and on a NULL JSON even when the
    /// hash matches, and a hash mismatch alone is enough per the pure helper.
    #[test]
    fn backfill_reprojects_stale_version_and_null_json() {
        let c = mem();
        persist(
            &c,
            movie_payload(serde_json::json!({
                "results": [{"iso_3166_1": "US", "release_dates": [{"certification": "R"}]}]
            })),
            "movie",
            "550",
        );
        // Stale version, valid JSON and matching hash: version alone reprojects.
        c.execute(
            "UPDATE metadata_canonical SET certifications_projection_version = 0
              WHERE provider_id = '550'",
            [],
        )
        .unwrap();
        let stats = drain_to_completion(&c);
        assert_eq!(stats.scanned, 1);
        assert_eq!(stats.updated, 1);
        let (version, json, _) = processed(&c, "movie", "550");
        assert_eq!(version, CERTIFICATION_PROJECTION_VERSION);
        assert!(json.is_some());

        // NULL JSON at the current version: JSON alone reprojects.
        c.execute(
            "UPDATE metadata_canonical SET certifications_json = NULL
              WHERE provider_id = '550'",
            [],
        )
        .unwrap();
        let stats = drain_to_completion(&c);
        assert_eq!(stats.scanned, 1);
        assert_eq!(stats.updated, 1);
        assert!(stored(&c, "movie", "550").is_some());
    }

    /// The ADR's literal hash trigger: a row already at the current version
    /// with a stored hash is re-projected when the raw payload bytes change.
    /// The projection write is deliberately skipped, so the pairing invariant
    /// is broken and only the cursor's hash comparison can catch it.
    #[test]
    fn backfill_reprojects_a_current_version_row_when_the_raw_payload_changes() {
        let c = mem();
        persist(
            &c,
            movie_payload(serde_json::json!({
                "results": [{"iso_3166_1": "US", "release_dates": [{"certification": "G"}]}]
            })),
            "movie",
            "550",
        );
        let (version, json, hash) = processed(&c, "movie", "550");
        assert_eq!(version, CERTIFICATION_PROJECTION_VERSION);
        assert!(json.unwrap().contains("G"));
        assert!(hash.is_some());

        // Replace the stored payload in place, leaving the projection and its
        // hash stale at the current version.
        c.execute(
            "UPDATE metadata_raw_payloads SET payload = ?1
              WHERE provider = 'tmdb' AND entity_kind = 'movie' AND provider_id = '550'",
            params![
                movie_payload(serde_json::json!({
                    "results": [{"iso_3166_1": "US", "release_dates": [{"certification": "R"}]}]
                }))
                .to_string()
            ],
        )
        .unwrap();

        let stats = drain_to_completion(&c);
        assert_eq!(stats.scanned, 1, "the cursor reaches the row");
        assert_eq!(stats.updated, 1, "the hash difference is reachable");
        let after = stored(&c, "movie", "550").unwrap();
        assert!(after.contains("R"), "{after}");
        let (_, _, new_hash) = processed(&c, "movie", "550");
        assert_ne!(new_hash, hash, "the stored hash advanced");
    }

    /// A processed no-payload row (NULL hash) is re-projected when a raw
    /// payload appears: `None` to `Some` is a hash change.
    #[test]
    fn backfill_reprojects_when_a_raw_payload_appears() {
        let c = mem();
        c.execute(
            "INSERT INTO metadata_canonical
                (provider, entity_kind, provider_id, title, ids_json, projected_at)
             VALUES ('tmdb', 'movie', '550', 'T', '{}', 'now')",
            [],
        )
        .unwrap();
        let first = drain_to_completion(&c);
        assert_eq!(first.no_payload, 1);
        let (_, _, hash) = processed(&c, "movie", "550");
        assert_eq!(hash, None);

        c.execute(
            "INSERT INTO metadata_raw_payloads
                (provider, entity_kind, provider_id, fetched_at, payload)
             VALUES ('tmdb', 'movie', '550', 'now', ?1)",
            params![
                movie_payload(serde_json::json!({
                    "results": [{"iso_3166_1": "US", "release_dates": [{"certification": "G"}]}]
                }))
                .to_string()
            ],
        )
        .unwrap();

        let second = drain_to_completion(&c);
        assert_eq!(second.updated, 1, "payload appearance reprojects");
        assert!(stored(&c, "movie", "550").unwrap().contains("G"));
        let (_, _, hash) = processed(&c, "movie", "550");
        assert!(hash.is_some());
    }

    /// Unchanged unresolved data (a no-payload row) is visited but never
    /// rewritten, and repeated sweeps terminate rather than looping.
    #[test]
    fn unchanged_unresolved_rows_terminate_without_rewriting() {
        let c = mem();
        c.execute(
            "INSERT INTO metadata_canonical
                (provider, entity_kind, provider_id, title, ids_json, projected_at)
             VALUES ('tmdb', 'movie', '550', 'T', '{}', 'now')",
            [],
        )
        .unwrap();
        assert_eq!(drain_to_completion(&c).updated, 1);
        for _ in 0..3 {
            let sweep = drain_to_completion(&c);
            assert_eq!(sweep.scanned, 1, "the row is still visited");
            assert_eq!(sweep.updated, 0, "and never rewritten");
        }
    }

    /// The measured residual shape: every movie/tv row with a stored payload is
    /// processed to version 1 with non-NULL JSON, and the pass terminates.
    #[test]
    fn backfill_leaves_no_unprocessed_null_residual_and_terminates() {
        let c = mem();
        // Two movies and one show, each with a stored payload; one payload is
        // deliberately malformed and one region is unknown. All start at the
        // default version 0, exactly like the measured residuals.
        persist(
            &c,
            movie_payload(serde_json::json!({
                "results": [
                    {"iso_3166_1": "US", "release_dates": [{"certification": "PG"}]},
                    {"iso_3166_1": "US", "release_dates": [{"certification": "R"}]}
                ]
            })),
            "movie",
            "550",
        );
        persist(
            &c,
            movie_payload_for(
                551,
                serde_json::json!({
                    "results": [{"iso_3166_1": "DE", "release_dates": [{"certification": "16"}]}]
                }),
            ),
            "movie",
            "551",
        );
        persist(
            &c,
            serde_json::json!({
                "id": 1,
                "name": "Show",
                "content_ratings": {"results": [{"iso_3166_1": "US", "rating": "TV-MA"}]}
            }),
            "tv",
            "1",
        );
        c.execute(
            "UPDATE metadata_canonical SET certifications_projection_version = 0,
                    certifications_json = NULL",
            [],
        )
        .unwrap();
        // A malformed stored payload on 551.
        c.execute(
            "UPDATE metadata_raw_payloads SET payload = 'not json' WHERE provider_id = '551'",
            [],
        )
        .unwrap();
        // An unknown region on the show.
        c.execute(
            "UPDATE metadata_raw_payloads SET payload = ?1 WHERE provider_id = '1'",
            params![
                serde_json::json!({
                    "id": 1,
                    "name": "Show",
                    "content_ratings": {"results": [{"iso_3166_1": "DE", "rating": "NOPE"}]}
                })
                .to_string()
            ],
        )
        .unwrap();

        let stats = drain_to_completion(&c);
        assert_eq!(stats.scanned, 3);
        assert_eq!(stats.updated, 3);
        assert_eq!(stats.malformed, 1);
        assert_eq!(stats.no_payload, 0);
        assert_eq!(stats.unknown_regions, 1);

        let residuals: i64 = c
            .query_row(
                "SELECT COUNT(*) FROM metadata_canonical
                  WHERE entity_kind IN ('movie', 'tv')
                    AND (certifications_json IS NULL
                         OR certifications_projection_version != 1)",
                [],
                |r| r.get(0),
            )
            .unwrap();
        assert_eq!(residuals, 0, "no unprocessed NULL residual remains");
        let second = drain_to_completion(&c);
        assert_eq!(second.updated, 0, "the pass terminates without rewriting");
        assert_eq!(second.scanned, 3, "the hash sweep still visits every row");
    }

    #[test]
    fn backfill_makes_progress_and_preserves_identity() {
        let c = mem();
        persist(
            &c,
            movie_payload(serde_json::json!({
                "results": [{"iso_3166_1": "US", "release_dates": [{"certification": "R"}]}]
            })),
            "movie",
            "550",
        );
        c.execute(
            "UPDATE metadata_canonical SET certifications_json = NULL WHERE provider_id = '550'",
            [],
        )
        .unwrap();
        let before: (String, String) = c
            .query_row(
                "SELECT title, ids_json FROM metadata_canonical WHERE provider_id = '550'",
                [],
                |r| Ok((r.get(0)?, r.get(1)?)),
            )
            .unwrap();
        let (stats, cursor) = backfill_certifications(&c, PROVIDER_TMDB, None, 100).unwrap();
        assert_eq!(stats.scanned, 1);
        assert_eq!(stats.updated, 1);
        assert_eq!(cursor, Some(("movie".to_string(), "550".to_string())));
        let after: (String, String) = c
            .query_row(
                "SELECT title, ids_json FROM metadata_canonical WHERE provider_id = '550'",
                [],
                |r| Ok((r.get(0)?, r.get(1)?)),
            )
            .unwrap();
        assert_eq!(before, after, "identity is untouched");
    }

    #[test]
    fn backfill_batch_cursor_walks_every_row() {
        let c = mem();
        for id in ["550", "551", "552"] {
            persist(
                &c,
                movie_payload_for(
                    id.parse().unwrap(),
                    serde_json::json!({
                        "results": [{"iso_3166_1": "US", "release_dates": [{"certification": "G"}]}]
                    }),
                ),
                "movie",
                id,
            );
        }
        c.execute(
            "UPDATE metadata_canonical SET certifications_json = NULL",
            [],
        )
        .unwrap();
        let (first, cursor) = backfill_certifications(&c, PROVIDER_TMDB, None, 1).unwrap();
        assert_eq!(first.scanned, 1);
        assert_eq!(cursor, Some(("movie".to_string(), "550".to_string())));
        let (second, _) =
            backfill_certifications(&c, PROVIDER_TMDB, Some(("movie", "550")), 1).unwrap();
        assert_eq!(second.scanned, 1);
        assert_eq!(second.updated, 1);
    }
}
