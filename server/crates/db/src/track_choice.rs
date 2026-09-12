//! Profile track-choice storage (ADR-0038 item 4 and its 2026-09-12
//! amendment). One mutable row per `(profile_id, series_key)`.
//!
//! These are the table's read and write primitives. The one writer that
//! combines them with key resolution and the server clock lives in
//! `nightjar-metadata`, beside the series-key grammar the row is keyed by —
//! the same split watch state uses.
//!
//! A choice is a description, never a stream index (ADR-0038 item 2): the row
//! holds language, kind and the SDH / forced flags, and the ADR-0024 rank
//! function resolves it against the next file's inventory.

use rusqlite::{Connection, OptionalExtension, params};

/// Language, kind and flags of one stored track choice (ADR-0024 §3).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct TrackDescription {
    pub language: Option<String>,
    /// `main` | `commentary` | `signs`; the CHECK is the closed set.
    pub kind: String,
    pub sdh: bool,
    pub forced: bool,
}

/// The three-valued subtitle choice (ADR-0038 item 4). `Unset` and `Off` are
/// deliberately distinct: one means "not chosen", the other "chosen off".
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum SubtitleChoiceRow {
    Unset,
    Off,
    Track(TrackDescription),
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct TrackChoiceRow {
    pub profile_id: i64,
    pub series_key: String,
    pub audio: Option<TrackDescription>,
    pub subtitle: SubtitleChoiceRow,
    pub updated_at: String,
}

pub fn load_track_choice(
    conn: &Connection,
    profile_id: i64,
    series_key: &str,
) -> Result<Option<TrackChoiceRow>, String> {
    conn.query_row(
        "SELECT profile_id, series_key,
                audio_language, audio_kind, audio_sdh, audio_forced,
                subtitle_mode, subtitle_language, subtitle_kind,
                subtitle_sdh, subtitle_forced, updated_at
         FROM profile_track_choice WHERE profile_id = ?1 AND series_key = ?2",
        params![profile_id, series_key],
        |r| {
            let audio_kind: Option<String> = r.get(3)?;
            let audio = match audio_kind {
                Some(kind) => Some(TrackDescription {
                    language: r.get(2)?,
                    kind,
                    sdh: r.get::<_, i64>(4)? != 0,
                    forced: r.get::<_, i64>(5)? != 0,
                }),
                None => None,
            };
            let mode: String = r.get(6)?;
            let subtitle = match mode.as_str() {
                "track" => SubtitleChoiceRow::Track(TrackDescription {
                    language: r.get(7)?,
                    kind: r.get(8)?,
                    sdh: r.get::<_, i64>(9)? != 0,
                    forced: r.get::<_, i64>(10)? != 0,
                }),
                "off" => SubtitleChoiceRow::Off,
                _ => SubtitleChoiceRow::Unset,
            };
            Ok(TrackChoiceRow {
                profile_id: r.get(0)?,
                series_key: r.get(1)?,
                audio,
                subtitle,
                updated_at: r.get(11)?,
            })
        },
    )
    .optional()
    .map_err(|e| format!("load track choice: {e}"))
}

/// Insert or fully replace one row. The route is a full replacement
/// (ADR-0038 amendment §2), so every column is written on every call and a
/// cleared field is written as null rather than left behind.
pub fn upsert_track_choice(
    conn: &Connection,
    profile_id: i64,
    series_key: &str,
    audio: Option<&TrackDescription>,
    subtitle: &SubtitleChoiceRow,
    now: &str,
) -> Result<(), String> {
    let (audio_language, audio_kind, audio_sdh, audio_forced) = match audio {
        Some(a) => (
            a.language.as_deref(),
            Some(a.kind.as_str()),
            Some(a.sdh as i64),
            Some(a.forced as i64),
        ),
        None => (None, None, None, None),
    };
    let (subtitle_mode, subtitle_language, subtitle_kind, subtitle_sdh, subtitle_forced) =
        match subtitle {
            SubtitleChoiceRow::Track(t) => (
                "track",
                t.language.as_deref(),
                Some(t.kind.as_str()),
                Some(t.sdh as i64),
                Some(t.forced as i64),
            ),
            SubtitleChoiceRow::Unset => ("unset", None, None, None, None),
            SubtitleChoiceRow::Off => ("off", None, None, None, None),
        };
    conn.execute(
        "INSERT INTO profile_track_choice
            (profile_id, series_key, audio_language, audio_kind, audio_sdh, audio_forced,
             subtitle_mode, subtitle_language, subtitle_kind, subtitle_sdh, subtitle_forced,
             updated_at)
         VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10, ?11, ?12)
         ON CONFLICT(profile_id, series_key) DO UPDATE SET
            audio_language = excluded.audio_language,
            audio_kind = excluded.audio_kind,
            audio_sdh = excluded.audio_sdh,
            audio_forced = excluded.audio_forced,
            subtitle_mode = excluded.subtitle_mode,
            subtitle_language = excluded.subtitle_language,
            subtitle_kind = excluded.subtitle_kind,
            subtitle_sdh = excluded.subtitle_sdh,
            subtitle_forced = excluded.subtitle_forced,
            updated_at = excluded.updated_at",
        params![
            profile_id,
            series_key,
            audio_language,
            audio_kind,
            audio_sdh,
            audio_forced,
            subtitle_mode,
            subtitle_language,
            subtitle_kind,
            subtitle_sdh,
            subtitle_forced,
            now
        ],
    )
    .map_err(|e| format!("upsert track choice: {e}"))?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::migrate;

    const NOW: &str = "2026-09-12T00:00:00.000Z";
    const LATER: &str = "2026-09-12T00:10:00.000Z";

    fn conn() -> Connection {
        let c = Connection::open_in_memory().unwrap();
        c.execute_batch("PRAGMA foreign_keys = ON;").unwrap();
        migrate(&c).unwrap();
        c.execute_batch(
            "INSERT INTO accounts (id, username, password_hash, role)
                 VALUES (1, 'a', 'x', 'owner');
             INSERT INTO profiles (id, account_id, profile_ref, name)
                 VALUES (1, 1, 'aa', 'P'), (2, 1, 'bb', 'Q');",
        )
        .unwrap();
        c
    }

    fn audio(language: Option<&str>, kind: &str, sdh: bool, forced: bool) -> TrackDescription {
        TrackDescription {
            language: language.map(str::to_string),
            kind: kind.to_string(),
            sdh,
            forced,
        }
    }

    #[test]
    fn round_trips_every_mode() {
        let c = conn();
        upsert_track_choice(
            &c,
            1,
            "tmdb:show:55",
            Some(&audio(Some("ja"), "main", false, false)),
            &SubtitleChoiceRow::Track(audio(Some("en"), "main", true, false)),
            NOW,
        )
        .unwrap();
        let row = load_track_choice(&c, 1, "tmdb:show:55").unwrap().unwrap();
        assert_eq!(row.audio, Some(audio(Some("ja"), "main", false, false)));
        assert_eq!(
            row.subtitle,
            SubtitleChoiceRow::Track(audio(Some("en"), "main", true, false))
        );
        assert_eq!(row.updated_at, NOW);

        // A null language is a legal description, and `unset` clears the
        // subtitle columns rather than leaving the previous track behind.
        upsert_track_choice(
            &c,
            1,
            "tmdb:show:55",
            Some(&audio(None, "commentary", false, true)),
            &SubtitleChoiceRow::Unset,
            LATER,
        )
        .unwrap();
        let row = load_track_choice(&c, 1, "tmdb:show:55").unwrap().unwrap();
        assert_eq!(row.audio, Some(audio(None, "commentary", false, true)));
        assert_eq!(row.subtitle, SubtitleChoiceRow::Unset);
        assert_eq!(row.updated_at, LATER);

        // `off` is a distinct stored mode, not the absence of a row.
        upsert_track_choice(&c, 1, "tmdb:show:55", None, &SubtitleChoiceRow::Off, NOW).unwrap();
        let row = load_track_choice(&c, 1, "tmdb:show:55").unwrap().unwrap();
        assert_eq!(row.audio, None);
        assert_eq!(row.subtitle, SubtitleChoiceRow::Off);
    }

    #[test]
    fn rows_are_keyed_by_profile_and_series() {
        let c = conn();
        upsert_track_choice(&c, 1, "tmdb:show:55", None, &SubtitleChoiceRow::Off, NOW).unwrap();
        upsert_track_choice(&c, 2, "tmdb:show:55", None, &SubtitleChoiceRow::Unset, NOW).unwrap();
        assert_eq!(
            load_track_choice(&c, 1, "tmdb:show:55")
                .unwrap()
                .unwrap()
                .subtitle,
            SubtitleChoiceRow::Off
        );
        assert_eq!(
            load_track_choice(&c, 2, "tmdb:show:55")
                .unwrap()
                .unwrap()
                .subtitle,
            SubtitleChoiceRow::Unset
        );
        assert_eq!(load_track_choice(&c, 1, "tmdb:show:66").unwrap(), None);
    }

    /// ADR-0034 item 7: deleting a profile takes its choices with it.
    #[test]
    fn profile_delete_cascades() {
        let c = conn();
        upsert_track_choice(&c, 1, "tmdb:show:55", None, &SubtitleChoiceRow::Off, NOW).unwrap();
        c.execute("DELETE FROM profiles WHERE id = 1", []).unwrap();
        let n: i64 = c
            .query_row("SELECT COUNT(*) FROM profile_track_choice", [], |r| {
                r.get(0)
            })
            .unwrap();
        assert_eq!(n, 0);
        assert_eq!(load_track_choice(&c, 1, "tmdb:show:55").unwrap(), None);
    }

    /// The CHECK is the mechanism that stops a half-written description, so
    /// the negative case is asserted directly rather than trusted.
    #[test]
    fn the_constraints_refuse_a_half_written_row() {
        let c = conn();
        // Audio kind present but its flags absent.
        assert!(
            c.execute(
                "INSERT INTO profile_track_choice
                    (profile_id, series_key, audio_kind, subtitle_mode, updated_at)
                 VALUES (1, 'k', 'main', 'unset', ?1)",
                params![NOW],
            )
            .is_err(),
            "a present audio description must carry sdh and forced"
        );
        // `track` without a description.
        assert!(
            c.execute(
                "INSERT INTO profile_track_choice
                    (profile_id, series_key, subtitle_mode, updated_at)
                 VALUES (1, 'k', 'track', ?1)",
                params![NOW],
            )
            .is_err(),
            "mode track must carry a description"
        );
        // `off` with description columns present.
        assert!(
            c.execute(
                "INSERT INTO profile_track_choice
                    (profile_id, series_key, subtitle_mode, subtitle_kind,
                     subtitle_sdh, subtitle_forced, updated_at)
                 VALUES (1, 'k', 'off', 'main', 0, 0, ?1)",
                params![NOW],
            )
            .is_err(),
            "mode off must carry no description"
        );
        // An unknown kind is not in the closed set.
        assert!(
            c.execute(
                "INSERT INTO profile_track_choice
                    (profile_id, series_key, audio_kind, audio_sdh, audio_forced,
                     subtitle_mode, updated_at)
                 VALUES (1, 'k', 'dub', 0, 0, 'unset', ?1)",
                params![NOW],
            )
            .is_err()
        );
        // The positive control: the same shapes with the full set insert.
        c.execute(
            "INSERT INTO profile_track_choice
                (profile_id, series_key, audio_kind, audio_sdh, audio_forced,
                 subtitle_mode, updated_at)
             VALUES (1, 'k', 'main', 0, 0, 'unset', ?1)",
            params![NOW],
        )
        .unwrap();
    }
}
