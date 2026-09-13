//! Single-row server settings (ADR-0037 item 2).
//!
//! The classification region belongs to the server, not to a household, and it
//! is locked once selected. The write path is the setup boundary; there is no
//! HTTP route that changes it.

use rusqlite::{Connection, OptionalExtension, params};

/// The one classification board the server compares against, or `None` before
/// setup has selected one.
pub fn classification_region(conn: &Connection) -> Result<Option<String>, String> {
    conn.query_row(
        "SELECT classification_region FROM server_settings WHERE id = 1",
        [],
        |r| r.get(0),
    )
    .optional()
    .map_err(|e| format!("read classification region: {e}"))
}

/// Select the region once. Selecting the same region again is a no-op so the
/// call is repeat-safe; selecting a different one is refused because changing
/// the region rescopes every item at once (ADR-0037 item 2). The escape hatch
/// is a `nightjar` subcommand, not an HTTP route.
pub fn select_classification_region(conn: &Connection, region: &str) -> Result<(), String> {
    let region = region.trim();
    if region.is_empty() {
        return Err("classification region must not be empty".into());
    }
    if region != region.to_uppercase() {
        return Err(format!("classification region must be uppercase: {region}"));
    }
    match classification_region(conn)? {
        Some(existing) if existing == region => Ok(()),
        Some(existing) => Err(format!(
            "classification region is locked to {existing}; {region} refused"
        )),
        None => {
            conn.execute(
                "INSERT INTO server_settings (id, classification_region) VALUES (1, ?1)",
                params![region],
            )
            .map_err(|e| format!("select classification region: {e}"))?;
            Ok(())
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::migrate;

    fn mem() -> Connection {
        let c = Connection::open_in_memory().unwrap();
        migrate(&c).unwrap();
        c
    }

    #[test]
    fn region_round_trips_and_is_absent_before_selection() {
        let c = mem();
        assert_eq!(classification_region(&c).unwrap(), None);
        select_classification_region(&c, "DE").unwrap();
        assert_eq!(classification_region(&c).unwrap().as_deref(), Some("DE"));
    }

    #[test]
    fn the_same_selection_is_repeat_safe_and_a_different_one_is_refused() {
        let c = mem();
        select_classification_region(&c, "US").unwrap();
        select_classification_region(&c, "US").unwrap();
        let err = select_classification_region(&c, "AU").unwrap_err();
        assert!(err.contains("locked"), "{err}");
        assert_eq!(classification_region(&c).unwrap().as_deref(), Some("US"));
    }

    #[test]
    fn only_one_row_can_exist() {
        let c = mem();
        select_classification_region(&c, "US").unwrap();
        // The CHECK on `id` is what makes "single-row" a constraint rather
        // than a convention; a second insert fails even at the SQL level.
        let err = c
            .execute(
                "INSERT INTO server_settings (id, classification_region) VALUES (2, 'AU')",
                [],
            )
            .unwrap_err();
        assert!(err.to_string().contains("CHECK"), "{err}");
    }

    #[test]
    fn region_shape_is_validated_before_it_is_stored() {
        let c = mem();
        assert!(select_classification_region(&c, "").is_err());
        assert!(select_classification_region(&c, "us").is_err());
        assert_eq!(classification_region(&c).unwrap(), None);
        // The column CHECK is a second gate: even a direct write cannot store a
        // lowercase region and then silently fail every lookup.
        let err = c
            .execute(
                "INSERT INTO server_settings (id, classification_region) VALUES (1, 'us')",
                [],
            )
            .unwrap_err();
        assert!(err.to_string().contains("CHECK"), "{err}");
    }
}
