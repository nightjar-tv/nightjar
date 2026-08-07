//! Negative-result cache (ADR-0026 §3).
//!
//! Keyed on `(provider, kind, query_key)` where `query_key` uses the same
//! fold as [`crate::norm_key`] so a cache entry and a match attempt agree.
//!
//! `api_error` is **not** written here: a transient network failure must not
//! park an item for a day. Only `no_results` and `below_threshold` back off
//! on the ADR schedule (1d → 7d → 30d → 90d cap).

use rusqlite::{Connection, OptionalExtension, params};

use crate::match_score::norm_key;

pub const PROVIDER_TMDB: &str = "tmdb";

/// Bump when `norm_key` / title-fold rules change (ADR-0026 §3).
/// Mismatched rows are ignored (treated as cache miss) so old keys re-search.
pub const CLEANER_VERSION: i32 = 1;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum CacheKind {
    Movie,
    Tv,
}

impl CacheKind {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Movie => "movie",
            Self::Tv => "tv",
        }
    }

    pub fn parse(s: &str) -> Option<Self> {
        match s {
            "movie" => Some(Self::Movie),
            "tv" => Some(Self::Tv),
            _ => None,
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum NegativeReason {
    NoResults,
    BelowThreshold,
    /// Present in the ADR schema; this crate does not write it (see module docs).
    ApiError,
}

impl NegativeReason {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::NoResults => "no_results",
            Self::BelowThreshold => "below_threshold",
            Self::ApiError => "api_error",
        }
    }

    pub fn parse(s: &str) -> Option<Self> {
        match s {
            "no_results" => Some(Self::NoResults),
            "below_threshold" => Some(Self::BelowThreshold),
            "api_error" => Some(Self::ApiError),
            _ => None,
        }
    }
}

#[derive(Debug, Clone, PartialEq)]
pub struct NegativeEntry {
    pub reason: NegativeReason,
    pub confidence: Option<f64>,
    pub attempt_count: i32,
    pub attempted_at: String,
    pub next_retry_at: String,
    pub cleaner_version: i32,
}

/// Search-input key: `norm_key(title)|year` with `-` sentinel when yearless.
pub fn query_key(title: &str, year: Option<i32>) -> String {
    let year_part = year
        .map(|y| y.to_string())
        .unwrap_or_else(|| "-".to_string());
    format!("{}|{year_part}", norm_key(title))
}

/// ADR-0033 Q4: a folder with stored series identity caches under its series
/// id, not its title+year query key. Two fold-colliding folders write the
/// same query for different shows, so one folder's miss must never suppress
/// the other's fall-through search. `series:{id}` contains no `|`, so it can
/// never collide with a title+year key (`{norm}|{year}` always has one).
pub fn series_cache_key(show_id: i64) -> String {
    format!("series:{show_id}")
}

fn plus_days_rfc3339(now: &str, days: i64) -> Result<String, String> {
    // Keep deps thin: parse YYYY-MM-DDTHH:MM:SS… and add days via unix seconds.
    let secs = parse_rfc3339_secs(now)?;
    format_rfc3339_secs(secs + days * 86_400)
}

fn parse_rfc3339_secs(s: &str) -> Result<i64, String> {
    // Accept `YYYY-MM-DDTHH:MM:SSZ` or with fractional seconds / offset Z only.
    let t = s.trim();
    let (date, rest) = t
        .split_once('T')
        .ok_or_else(|| format!("bad timestamp (no T): {s}"))?;
    let time = rest.trim_end_matches('Z').split('.').next().unwrap_or(rest);
    let mut dp = date.split('-');
    let y: i32 = dp
        .next()
        .and_then(|x| x.parse().ok())
        .ok_or_else(|| format!("bad year in {s}"))?;
    let mo: u32 = dp
        .next()
        .and_then(|x| x.parse().ok())
        .ok_or_else(|| format!("bad month in {s}"))?;
    let d: u32 = dp
        .next()
        .and_then(|x| x.parse().ok())
        .ok_or_else(|| format!("bad day in {s}"))?;
    let mut tp = time.split(':');
    let h: u32 = tp
        .next()
        .and_then(|x| x.parse().ok())
        .ok_or_else(|| format!("bad hour in {s}"))?;
    let mi: u32 = tp
        .next()
        .and_then(|x| x.parse().ok())
        .ok_or_else(|| format!("bad minute in {s}"))?;
    let se: u32 = tp.next().and_then(|x| x.parse().ok()).unwrap_or(0);
    days_since_unix_epoch(y, mo, d)
        .map(|days| days * 86_400 + i64::from(h) * 3600 + i64::from(mi) * 60 + i64::from(se))
}

fn days_since_unix_epoch(y: i32, mo: u32, d: u32) -> Result<i64, String> {
    // Civil date → days since 1970-01-01 (Howard Hinnant algorithm).
    let y = y as i64;
    let mo = mo as i64;
    let d = d as i64;
    let (y, mo) = if mo <= 2 {
        (y - 1, mo + 9)
    } else {
        (y, mo - 3)
    };
    let era = if y >= 0 { y } else { y - 399 } / 400;
    let yoe = y - era * 400;
    let doy = (153 * mo + 2) / 5 + d - 1;
    let doe = yoe * 365 + yoe / 4 - yoe / 100 + doy;
    Ok(era * 146_097 + doe - 719_468)
}

fn format_rfc3339_secs(secs: i64) -> Result<String, String> {
    let days = secs.div_euclid(86_400);
    let sod = secs.rem_euclid(86_400) as u32;
    let (y, mo, d) = civil_from_days(days)?;
    let h = sod / 3600;
    let mi = (sod % 3600) / 60;
    let se = sod % 60;
    Ok(format!("{y:04}-{mo:02}-{d:02}T{h:02}:{mi:02}:{se:02}Z"))
}

fn civil_from_days(z: i64) -> Result<(i32, u32, u32), String> {
    let z = z + 719_468;
    let era = if z >= 0 { z } else { z - 146_096 } / 146_097;
    let doe = z - era * 146_097;
    let yoe = (doe - doe / 1460 + doe / 36524 - doe / 146_096) / 365;
    let y = (yoe + era * 400) as i32;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let d = (doy - (153 * mp + 2) / 5 + 1) as u32;
    let mo = if mp < 10 { mp + 3 } else { mp - 9 } as u32;
    let y = if mo <= 2 { y + 1 } else { y };
    Ok((y, mo, d))
}

pub fn now_rfc3339() -> String {
    // Prefer system clock via libc-free path: SQLite strftime when available
    // isn't here; use UNIX_EPOCH duration.
    use std::time::{SystemTime, UNIX_EPOCH};
    let secs = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_secs() as i64)
        .unwrap_or(0);
    format_rfc3339_secs(secs).unwrap_or_else(|_| "1970-01-01T00:00:00Z".into())
}

pub fn lookup(
    conn: &Connection,
    provider: &str,
    kind: CacheKind,
    query_key: &str,
) -> Result<Option<NegativeEntry>, String> {
    conn.query_row(
        "SELECT reason, confidence, attempt_count, attempted_at, next_retry_at,
                cleaner_version
         FROM metadata_negative_cache
         WHERE provider = ?1 AND kind = ?2 AND query_key = ?3",
        params![provider, kind.as_str(), query_key],
        |r| {
            let reason_s: String = r.get(0)?;
            Ok(NegativeEntry {
                reason: NegativeReason::parse(&reason_s).unwrap_or(NegativeReason::NoResults),
                confidence: r.get(1)?,
                attempt_count: r.get(2)?,
                attempted_at: r.get(3)?,
                next_retry_at: r.get(4)?,
                cleaner_version: r.get(5)?,
            })
        },
    )
    .optional()
    .map_err(|e| format!("negative cache lookup: {e}"))
}

/// True when a durable miss is still inside its backoff window and the
/// cleaner stamp matches the current fold rules.
pub fn should_skip(
    conn: &Connection,
    provider: &str,
    kind: CacheKind,
    query_key: &str,
    now: &str,
) -> Result<Option<NegativeEntry>, String> {
    let Some(entry) = lookup(conn, provider, kind, query_key)? else {
        return Ok(None);
    };
    if entry.cleaner_version != CLEANER_VERSION {
        return Ok(None);
    }
    if entry.next_retry_at.as_str() > now {
        Ok(Some(entry))
    } else {
        Ok(None)
    }
}

/// Record a genuine miss (`no_results` / `below_threshold` only). Does not
/// accept `api_error` — callers must not pass it.
pub fn record_miss(
    conn: &Connection,
    provider: &str,
    kind: CacheKind,
    query_key: &str,
    reason: NegativeReason,
    confidence: Option<f64>,
    now: &str,
) -> Result<(), String> {
    if reason == NegativeReason::ApiError {
        return Err("api_error is not cached (transient failures retry next resolve)".into());
    }
    let existing = lookup(conn, provider, kind, query_key)?;
    // Stale cleaner stamp does not inherit attempt_count (new fold, new life).
    let attempt_count = existing
        .filter(|e| e.cleaner_version == CLEANER_VERSION)
        .map(|e| e.attempt_count + 1)
        .unwrap_or(1);
    let next = plus_days_rfc3339(now, nightjar_db::backoff_days(i64::from(attempt_count)))?;
    conn.execute(
        "INSERT INTO metadata_negative_cache
            (provider, kind, query_key, reason, confidence, attempt_count,
             attempted_at, next_retry_at, cleaner_version)
         VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9)
         ON CONFLICT(provider, kind, query_key) DO UPDATE SET
            reason = excluded.reason,
            confidence = excluded.confidence,
            attempt_count = excluded.attempt_count,
            attempted_at = excluded.attempted_at,
            next_retry_at = excluded.next_retry_at,
            cleaner_version = excluded.cleaner_version",
        params![
            provider,
            kind.as_str(),
            query_key,
            reason.as_str(),
            confidence,
            attempt_count,
            now,
            next,
            CLEANER_VERSION,
        ],
    )
    .map_err(|e| format!("negative cache record: {e}"))?;
    Ok(())
}

/// Drop rows written under older cleaner versions (optional startup sweep).
pub fn sweep_stale_cleaner_versions(conn: &Connection) -> Result<usize, String> {
    let n = conn
        .execute(
            "DELETE FROM metadata_negative_cache WHERE cleaner_version != ?1",
            params![CLEANER_VERSION],
        )
        .map_err(|e| format!("sweep stale cleaner versions: {e}"))?;
    Ok(n)
}

/// Manual retry: delete the row so the next resolve hits the provider
/// (ADR-0026 §3).
pub fn clear(
    conn: &Connection,
    provider: &str,
    kind: CacheKind,
    query_key: &str,
) -> Result<(), String> {
    conn.execute(
        "DELETE FROM metadata_negative_cache
         WHERE provider = ?1 AND kind = ?2 AND query_key = ?3",
        params![provider, kind.as_str(), query_key],
    )
    .map_err(|e| format!("negative cache clear: {e}"))?;
    Ok(())
}

#[derive(Debug, Clone, Default)]
pub struct ReasonCounts {
    pub no_results: i64,
    pub below_threshold: i64,
    pub api_error: i64,
    pub other: i64,
}

pub fn counts_by_reason(conn: &Connection) -> Result<(i64, ReasonCounts), String> {
    let total: i64 = conn
        .query_row("SELECT COUNT(*) FROM metadata_negative_cache", [], |r| {
            r.get(0)
        })
        .map_err(|e| format!("count negative cache: {e}"))?;
    let mut counts = ReasonCounts::default();
    let mut stmt = conn
        .prepare("SELECT reason, COUNT(*) FROM metadata_negative_cache GROUP BY reason")
        .map_err(|e| format!("group negative cache: {e}"))?;
    let rows = stmt
        .query_map([], |r| Ok((r.get::<_, String>(0)?, r.get::<_, i64>(1)?)))
        .map_err(|e| format!("group negative cache: {e}"))?;
    for row in rows {
        let (reason, n) = row.map_err(|e| format!("group row: {e}"))?;
        match reason.as_str() {
            "no_results" => counts.no_results = n,
            "below_threshold" => counts.below_threshold = n,
            "api_error" => counts.api_error = n,
            _ => counts.other += n,
        }
    }
    Ok((total, counts))
}

#[cfg(test)]
mod tests {
    use super::*;
    use nightjar_db::migrate;
    use rusqlite::Connection;

    fn mem() -> Connection {
        let c = Connection::open_in_memory().unwrap();
        migrate(&c).unwrap();
        c
    }

    #[test]
    fn query_key_uses_norm_key_fold() {
        assert_eq!(query_key("The Matrix", Some(1999)), "matrix|1999");
        assert_eq!(
            query_key("Foo & Bar", None),
            format!("{}|-", norm_key("Foo & Bar"))
        );
    }

    #[test]
    fn series_key_is_distinct_from_title_year_keys() {
        assert_eq!(series_cache_key(55), "series:55");
        assert!(
            !series_cache_key(55).contains('|'),
            "a series key must never look like a title+year key"
        );
        for title in ["Shameless", "Series", "A"] {
            assert_ne!(series_cache_key(55), query_key(title, Some(55)));
            assert_ne!(series_cache_key(55), query_key(title, None));
        }
    }

    #[test]
    fn backoff_skips_until_retry_at() {
        let c = mem();
        let qk = query_key("Unmatchable Film", Some(2099));
        record_miss(
            &c,
            PROVIDER_TMDB,
            CacheKind::Movie,
            &qk,
            NegativeReason::NoResults,
            None,
            "2026-08-01T00:00:00Z",
        )
        .unwrap();
        let skipped = should_skip(
            &c,
            PROVIDER_TMDB,
            CacheKind::Movie,
            &qk,
            "2026-08-01T12:00:00Z",
        )
        .unwrap();
        assert!(skipped.is_some());
        let open = should_skip(
            &c,
            PROVIDER_TMDB,
            CacheKind::Movie,
            &qk,
            "2026-08-03T00:00:00Z",
        )
        .unwrap();
        assert!(open.is_none());
    }

    #[test]
    fn manual_clear_removes_entry() {
        let c = mem();
        let qk = query_key("X", None);
        record_miss(
            &c,
            PROVIDER_TMDB,
            CacheKind::Tv,
            &qk,
            NegativeReason::BelowThreshold,
            Some(0.72),
            "2026-08-01T00:00:00Z",
        )
        .unwrap();
        clear(&c, PROVIDER_TMDB, CacheKind::Tv, &qk).unwrap();
        assert!(
            lookup(&c, PROVIDER_TMDB, CacheKind::Tv, &qk)
                .unwrap()
                .is_none()
        );
    }

    #[test]
    fn stale_cleaner_version_does_not_skip() {
        let c = mem();
        let qk = query_key("Old Fold", Some(2000));
        record_miss(
            &c,
            PROVIDER_TMDB,
            CacheKind::Movie,
            &qk,
            NegativeReason::NoResults,
            None,
            "2026-08-01T00:00:00Z",
        )
        .unwrap();
        c.execute(
            "UPDATE metadata_negative_cache SET cleaner_version = 0 WHERE query_key = ?1",
            params![qk],
        )
        .unwrap();
        let skipped = should_skip(
            &c,
            PROVIDER_TMDB,
            CacheKind::Movie,
            &qk,
            "2026-08-01T12:00:00Z",
        )
        .unwrap();
        assert!(skipped.is_none());
        let n = sweep_stale_cleaner_versions(&c).unwrap();
        assert_eq!(n, 1);
    }

    #[test]
    fn api_error_record_is_rejected() {
        let c = mem();
        let err = record_miss(
            &c,
            PROVIDER_TMDB,
            CacheKind::Movie,
            "x|-",
            NegativeReason::ApiError,
            None,
            "2026-08-01T00:00:00Z",
        )
        .unwrap_err();
        assert!(err.contains("not cached"));
    }
}
