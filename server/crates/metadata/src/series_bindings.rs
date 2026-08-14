//! The folder -> entity record (ADR-0046 item 2), written by migration 021.
//!
//! `series` keeps `tmdb_show_id` as the folder's **primary** binding. This
//! table records every entity the folder binds to, keyed on the same
//! `(library_id, relpath)` pair, with the folder-season range each binding
//! covers and the entity-season range it maps to.
//!
//! **Nothing here is consulted by `resolve_series_key`, deliberately.** That
//! function selects folders on `series.tmdb_show_id`. If it read this table, a
//! second folder that binds the same entity as its primary would collapse into
//! one browse unit with the first — the duplicate-key collision ADR-0039
//! exists to prevent, arriving through a different door. One folder is one
//! unit and one key, whatever it binds.

use rusqlite::{Connection, OptionalExtension, params};

/// One `(folder, entity)` row. `None` on a range bound means unbounded: a
/// primary backfilled by migration 021 is unbounded on all four, which reads
/// as "covers every folder season, numbering unchanged".
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SeriesBinding {
    pub tmdb_show_id: i64,
    pub is_primary: bool,
    pub folder_seasons: (Option<i32>, Option<i32>),
    pub entity_seasons: (Option<i32>, Option<i32>),
}

impl SeriesBinding {
    /// The folder's number for an entity season — the read path's direction.
    pub fn folder_season_for(&self, entity_season: i32) -> Option<i32> {
        let (Some(flo), Some(fhi)) = self.folder_seasons else {
            return Some(entity_season);
        };
        let (Some(elo), Some(_ehi)) = self.entity_seasons else {
            return Some(entity_season);
        };
        let mapped = flo + (entity_season - elo);
        (mapped <= fhi).then_some(mapped)
    }
}

/// Every binding for a folder, primary first.
pub fn for_folder(
    conn: &Connection,
    library_id: i64,
    relpath: &str,
) -> Result<Vec<SeriesBinding>, String> {
    let mut stmt = conn
        .prepare(
            "SELECT tmdb_show_id, is_primary,
                    folder_season_start, folder_season_end,
                    entity_season_start, entity_season_end
             FROM series_entity_bindings
             WHERE library_id = ?1 AND relpath = ?2
             ORDER BY is_primary DESC, tmdb_show_id",
        )
        .map_err(|e| format!("prepare series bindings: {e}"))?;
    let rows = stmt
        .query_map(params![library_id, relpath], |r| {
            Ok(SeriesBinding {
                tmdb_show_id: r.get(0)?,
                is_primary: r.get::<_, i64>(1)? == 1,
                folder_seasons: (r.get(2)?, r.get(3)?),
                entity_seasons: (r.get(4)?, r.get(5)?),
            })
        })
        .map_err(|e| format!("query series bindings: {e}"))?;
    let mut out = Vec::new();
    for row in rows {
        out.push(row.map_err(|e| format!("series binding row: {e}"))?);
    }
    Ok(out)
}

/// Record a non-primary binding. Idempotent on `(library_id, relpath, entity)`.
///
/// **Never writes or moves the primary.** A primary that moves is a
/// `series_key` change, which restates the folder's identity to every consumer
/// keyed on it, including stored `media_item_links` rows and bookmarked browse
/// URLs. Which entity *should* be primary is ADR-0046 item 5 and is open.
pub fn record_secondary(
    conn: &Connection,
    library_id: i64,
    relpath: &str,
    tmdb_show_id: i64,
    folder_seasons: (i32, i32),
    entity_seasons: (i32, i32),
) -> Result<(), String> {
    let already_primary: Option<i64> = conn
        .query_row(
            "SELECT 1 FROM series_entity_bindings
             WHERE library_id = ?1 AND relpath = ?2 AND tmdb_show_id = ?3
               AND is_primary = 1",
            params![library_id, relpath, tmdb_show_id],
            |r| r.get(0),
        )
        .optional()
        .map_err(|e| format!("check primary binding: {e}"))?;
    if already_primary.is_some() {
        // The folder's primary already is this entity. Nothing to add, and
        // rewriting it as a secondary would be the primary move this refuses.
        return Ok(());
    }
    conn.execute(
        "INSERT INTO series_entity_bindings
            (library_id, relpath, tmdb_show_id, is_primary,
             folder_season_start, folder_season_end,
             entity_season_start, entity_season_end)
         VALUES (?1, ?2, ?3, 0, ?4, ?5, ?6, ?7)
         ON CONFLICT (library_id, relpath, tmdb_show_id) DO UPDATE SET
            folder_season_start = excluded.folder_season_start,
            folder_season_end   = excluded.folder_season_end,
            entity_season_start = excluded.entity_season_start,
            entity_season_end   = excluded.entity_season_end",
        params![
            library_id,
            relpath,
            tmdb_show_id,
            folder_seasons.0,
            folder_seasons.1,
            entity_seasons.0,
            entity_seasons.1
        ],
    )
    .map_err(|e| format!("record secondary binding: {e}"))?;
    Ok(())
}

/// Does this folder already carry a secondary binding? The drain asks before
/// searching, so a rescan of a folder that has been resolved issues **zero**
/// search requests (ADR-0033 item 1, and Gate 3's zero-request property).
pub fn has_secondary(conn: &Connection, library_id: i64, relpath: &str) -> Result<bool, String> {
    let found: Option<i64> = conn
        .query_row(
            "SELECT 1 FROM series_entity_bindings
             WHERE library_id = ?1 AND relpath = ?2 AND is_primary = 0 LIMIT 1",
            params![library_id, relpath],
            |r| r.get(0),
        )
        .optional()
        .map_err(|e| format!("check secondary binding: {e}"))?;
    Ok(found.is_some())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn secondary(folder: (i32, i32), entity: (i32, i32)) -> SeriesBinding {
        SeriesBinding {
            tmdb_show_id: 74321,
            is_primary: false,
            folder_seasons: (Some(folder.0), Some(folder.1)),
            entity_seasons: (Some(entity.0), Some(entity.1)),
        }
    }

    /// Will & Grace: the entity numbers its revival S1-S3, the folder numbers
    /// the same episodes S9-S11, and the reader must see the folder's.
    #[test]
    fn renumbered_binding_translates_entity_seasons_to_the_folders() {
        let b = secondary((9, 11), (1, 3));
        assert_eq!(b.folder_season_for(1), Some(9));
        assert_eq!(b.folder_season_for(2), Some(10));
        assert_eq!(b.folder_season_for(3), Some(11));
    }

    /// A season outside the entity's mapped range is not this binding's to
    /// translate. Returning a number anyway is how a season lands in the wrong
    /// bucket without anything erroring.
    #[test]
    fn a_season_past_the_range_does_not_translate() {
        assert_eq!(secondary((9, 11), (1, 3)).folder_season_for(4), None);
    }

    /// The backfilled primary is unbounded and does not renumber, so a folder
    /// that binds one entity reads exactly as it does today (Rule 2.3).
    #[test]
    fn an_unbounded_primary_never_renumbers() {
        let primary = SeriesBinding {
            tmdb_show_id: 4454,
            is_primary: true,
            folder_seasons: (None, None),
            entity_seasons: (None, None),
        };
        for season in 1..=8 {
            assert_eq!(primary.folder_season_for(season), Some(season));
        }
    }

    /// An anthology has no offset — both installments start at season 1 and
    /// the folder's number is an ordinal over them. A range says this; a
    /// stored delta cannot.
    #[test]
    fn anthology_translates_without_an_offset() {
        let b = secondary((3, 3), (1, 1));
        assert_eq!(b.folder_season_for(1), Some(3));
        assert_eq!(b.folder_season_for(2), None);
    }
}
