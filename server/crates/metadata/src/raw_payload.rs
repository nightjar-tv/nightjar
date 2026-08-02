//! Entity-keyed raw provider payload store (ADR-0026 §4).
//!
//! Keyed by `(provider, entity_kind, provider_id)` — movie / tv / season —
//! not per file. Upserts must run in the **same transaction** as the
//! canonical metadata write so a later mapping change can re-project without
//! re-fetching.
//!
//! Stored uncompressed UTF-8 JSON so the SQLite file stays readable. Gzip is
//! a pure implementation option later if on-disk size becomes a complaint.

use rusqlite::{Connection, OptionalExtension, Transaction, params};

use crate::negative_cache::now_rfc3339;
use crate::tmdb::RawProviderPayload;

/// Upsert one entity payload. Call inside the same transaction as the
/// canonical metadata write (ADR-0026 §4).
pub fn upsert_raw_payload(
    tx: &Transaction<'_>,
    provider: &str,
    raw: &RawProviderPayload,
    fetched_at: &str,
) -> Result<(), String> {
    tx.execute(
        "INSERT INTO metadata_raw_payloads
            (provider, entity_kind, provider_id, fetched_at, payload)
         VALUES (?1, ?2, ?3, ?4, ?5)
         ON CONFLICT(provider, entity_kind, provider_id) DO UPDATE SET
            fetched_at = excluded.fetched_at,
            payload = excluded.payload",
        params![
            provider,
            raw.entity_kind,
            raw.provider_id,
            fetched_at,
            raw.payload,
        ],
    )
    .map_err(|e| format!("upsert raw payload: {e}"))?;
    Ok(())
}

/// Open a transaction, run `canonical_write`, upsert `raw`, commit.
///
/// Canonical writers pass their SQL into `canonical_write` so both lands
/// atomically (see [`crate::canonical::persist_mapped_hit`]).
pub fn persist_hit_with_canonical<F>(
    conn: &Connection,
    provider: &str,
    raw: &RawProviderPayload,
    mut canonical_write: F,
) -> Result<(), String>
where
    F: FnMut(&Transaction<'_>) -> Result<(), String>,
{
    let tx = conn
        .unchecked_transaction()
        .map_err(|e| format!("begin payload tx: {e}"))?;
    canonical_write(&tx)?;
    upsert_raw_payload(&tx, provider, raw, &now_rfc3339())?;
    tx.commit().map_err(|e| format!("commit payload tx: {e}"))?;
    Ok(())
}

pub fn get_raw_payload(
    conn: &Connection,
    provider: &str,
    entity_kind: &str,
    provider_id: &str,
) -> Result<Option<String>, String> {
    conn.query_row(
        "SELECT payload FROM metadata_raw_payloads
         WHERE provider = ?1 AND entity_kind = ?2 AND provider_id = ?3",
        params![provider, entity_kind, provider_id],
        |r| r.get(0),
    )
    .optional()
    .map_err(|e| format!("get raw payload: {e}"))
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct PayloadStoreStats {
    pub row_count: i64,
    /// `SUM(LENGTH(payload))` — UTF-8 JSON bytes in the payload column.
    /// Not the SQLite file size (which also holds the library copy).
    pub payload_bytes: i64,
}

pub fn payload_store_stats(conn: &Connection) -> Result<PayloadStoreStats, String> {
    conn.query_row(
        "SELECT COUNT(*), COALESCE(SUM(LENGTH(payload)), 0) FROM metadata_raw_payloads",
        [],
        |r| {
            Ok(PayloadStoreStats {
                row_count: r.get(0)?,
                payload_bytes: r.get(1)?,
            })
        },
    )
    .map_err(|e| format!("payload store stats: {e}"))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::negative_cache::PROVIDER_TMDB;
    use nightjar_db::migrate;
    use rusqlite::Connection;
    use std::cell::Cell;

    fn mem() -> Connection {
        let c = Connection::open_in_memory().unwrap();
        migrate(&c).unwrap();
        c
    }

    #[test]
    fn payload_and_canonical_share_one_transaction() {
        let c = mem();
        c.execute_batch(
            "CREATE TABLE canonical_stub (
                provider_id TEXT PRIMARY KEY,
                title TEXT NOT NULL
             );",
        )
        .unwrap();
        let raw = RawProviderPayload {
            entity_kind: "movie".into(),
            provider_id: "550".into(),
            payload: r#"{"id":550,"title":"Fight Club"}"#.into(),
        };
        let wrote = Cell::new(false);
        persist_hit_with_canonical(&c, PROVIDER_TMDB, &raw, |tx| {
            tx.execute(
                "INSERT INTO canonical_stub (provider_id, title) VALUES (?1, ?2)",
                params!["550", "Fight Club"],
            )
            .map_err(|e| e.to_string())?;
            wrote.set(true);
            Ok(())
        })
        .unwrap();
        assert!(wrote.get());
        let title: String = c
            .query_row(
                "SELECT title FROM canonical_stub WHERE provider_id = '550'",
                [],
                |r| r.get(0),
            )
            .unwrap();
        assert_eq!(title, "Fight Club");
        let body = get_raw_payload(&c, PROVIDER_TMDB, "movie", "550")
            .unwrap()
            .unwrap();
        assert!(body.contains("Fight Club"));
        let stats = payload_store_stats(&c).unwrap();
        assert_eq!(stats.row_count, 1);
        assert_eq!(stats.payload_bytes, raw.payload.len() as i64);
    }

    #[test]
    fn rollback_drops_both_sides() {
        let c = mem();
        c.execute_batch(
            "CREATE TABLE canonical_stub (
                provider_id TEXT PRIMARY KEY,
                title TEXT NOT NULL
             );",
        )
        .unwrap();
        let raw = RawProviderPayload {
            entity_kind: "tv".into(),
            provider_id: "1".into(),
            payload: "{}".into(),
        };
        let err = persist_hit_with_canonical(&c, PROVIDER_TMDB, &raw, |tx| {
            tx.execute(
                "INSERT INTO canonical_stub (provider_id, title) VALUES ('1', 'x')",
                [],
            )
            .map_err(|e| e.to_string())?;
            Err("canonical failed".into())
        })
        .unwrap_err();
        assert_eq!(err, "canonical failed");
        let n: i64 = c
            .query_row("SELECT COUNT(*) FROM canonical_stub", [], |r| r.get(0))
            .unwrap();
        assert_eq!(n, 0);
        assert!(
            get_raw_payload(&c, PROVIDER_TMDB, "tv", "1")
                .unwrap()
                .is_none()
        );
    }
}
