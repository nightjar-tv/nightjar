//! The continue-watching rail (ADR-0035 items 6 and 8).
//!
//! Profile-scoped in the path, like the watch-state route, because an account
//! holder reading a child's rail is a real case and the route has to name whose
//! rail it is. The rollup itself is one server-side function
//! (`nightjar_metadata::continue_watching`), because three clients each
//! computing "next episode" is how the rail drifts.

use crate::authority::{Caller, authorize_profile_ref};
use crate::error::{ApiError, ApiResult, blocking};
use crate::state::AppState;
use axum::Json;
use axum::extract::{Path, Query, State};
use nightjar_metadata::{ContinueWatchingEntry, continue_watching};
use serde::{Deserialize, Serialize};

/// Optional rail length. It is applied after series collapse, so one show with
/// twenty watched episodes still costs one slot. No ADR fixes a default, so
/// absent means the whole rail.
///
/// Read as a string so a bad value is the repository's typed 400 rather than
/// axum's plain-text query rejection, which would contradict the OpenAPI error
/// shape.
#[derive(Deserialize)]
pub struct ContinueWatchingQuery {
    pub limit: Option<String>,
}

/// Parse `limit`, or refuse it as the typed bad request.
///
/// The OpenAPI parameter is `integer, format: int32, minimum: 0`, so the code
/// accepts exactly that range rather than the wider `u32` range a bare
/// `parse::<u32>` would.
fn parse_limit(query: &ContinueWatchingQuery) -> ApiResult<Option<usize>> {
    match query.limit.as_deref() {
        None => Ok(None),
        Some(raw) => match raw.parse::<u32>() {
            Ok(limit) if limit <= i32::MAX as u32 => Ok(Some(limit as usize)),
            _ => Err(ApiError::bad_request(
                "limit must be a non-negative integer",
            )),
        },
    }
}

#[derive(Serialize)]
#[serde(rename_all = "camelCase")]
pub struct ContinueWatchingEntryDto {
    /// Opaque series key (ADR-0039 item 2).
    pub series_key: String,
    /// Opaque item key of the episode or movie to resume (ADR-0025 §1).
    pub item_key: String,
    pub item_id: i64,
    pub title: String,
    pub kind: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub season: Option<i32>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub episode: Option<i32>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub show_title: Option<String>,
    pub position_ms: i64,
    pub duration_ms: i64,
    pub played: bool,
    pub last_played_at: String,
}

impl From<ContinueWatchingEntry> for ContinueWatchingEntryDto {
    fn from(entry: ContinueWatchingEntry) -> Self {
        Self {
            series_key: entry.series_key,
            item_key: entry.item_key,
            item_id: entry.item_id,
            title: entry.title,
            kind: entry.kind,
            season: entry.season,
            episode: entry.episode,
            show_title: entry.show_title,
            position_ms: entry.position_ms,
            duration_ms: entry.duration_ms,
            played: entry.played,
            last_played_at: entry.last_played_at,
        }
    }
}

#[derive(Serialize)]
pub struct ContinueWatchingEnvelope {
    pub items: Vec<ContinueWatchingEntryDto>,
}

pub async fn list(
    State(state): State<AppState>,
    caller: Caller,
    Path(profile_ref): Path<String>,
    Query(query): Query<ContinueWatchingQuery>,
) -> ApiResult<Json<ContinueWatchingEnvelope>> {
    blocking(move || {
        let profile_id = authorize_profile_ref(&state, &caller, &profile_ref)?;
        let limit = parse_limit(&query)?;
        let entries = state
            .db
            .with_conn(|conn| continue_watching(conn, profile_id, limit))
            .map_err(ApiError::internal)?;
        Ok(Json(ContinueWatchingEnvelope {
            items: entries.into_iter().map(Into::into).collect(),
        }))
    })
    .await
}

#[cfg(test)]
mod tests {
    use crate::routes::router;
    use crate::state::{AppState, test_support};
    use axum::body::Body;
    use axum::http::{Request, StatusCode};
    use nightjar_auth::mint_session_token;
    use nightjar_db::{NewLibrary, UpsertItem};
    use serde_json::Value;
    use tower::ServiceExt;

    struct Actor {
        account_id: i64,
        profile_id: i64,
        profile_ref: String,
    }

    fn actor(state: &AppState, username: &str, role: &str, profile_ref: &str) -> Actor {
        let (account_id, profile_id) = state
            .db
            .with_conn(|conn| {
                let hash = nightjar_auth::hash_password("x").map_err(|e| format!("hash: {e:?}"))?;
                nightjar_db::create_account_with_profile(
                    conn,
                    username,
                    &hash,
                    role,
                    "P",
                    profile_ref,
                )?;
                let account = nightjar_db::account_by_username(conn, username)?.unwrap();
                let profile = nightjar_db::profile_by_ref(conn, profile_ref)?.unwrap();
                Ok((account.id, profile.id))
            })
            .unwrap();
        Actor {
            account_id,
            profile_id,
            profile_ref: profile_ref.to_string(),
        }
    }

    fn token(state: &AppState, actor: &Actor, scoped: bool) -> String {
        let minted = mint_session_token();
        state
            .db
            .with_conn(|conn| {
                let expires = nightjar_db::session_expiry(conn)?;
                let session = nightjar_db::create_session(
                    conn,
                    actor.account_id,
                    &minted.sha256_hex,
                    "t",
                    &expires,
                )?;
                if scoped {
                    nightjar_db::set_active_profile(conn, session, Some(actor.profile_id))?;
                }
                Ok(())
            })
            .unwrap();
        minted.plaintext
    }

    fn seed_library(state: &AppState, name: &str, path: &str, kind: &str) -> i64 {
        state
            .db
            .create_library(&NewLibrary {
                name: name.to_string(),
                path: path.to_string(),
                kind: kind.to_string(),
            })
            .unwrap()
            .id
    }

    /// `(path, title, kind, season, episode)` in input order; ids come back in
    /// that order.
    type ItemSpec<'a> = (&'a str, &'a str, &'a str, Option<i32>, Option<i32>);

    fn seed_items(state: &AppState, library_id: i64, items: &[ItemSpec<'_>]) -> Vec<i64> {
        let upserts: Vec<UpsertItem> = items
            .iter()
            .map(|(path, title, kind, season, episode)| UpsertItem {
                path: path.to_string(),
                mtime_ms: 0,
                size_bytes: 1,
                title: title.to_string(),
                kind: kind.to_string(),
                year: None,
                season: *season,
                episode: *episode,
                content_id: None,
            })
            .collect();
        state.db.upsert_items_indexed(library_id, &upserts).unwrap()
    }

    fn link(state: &AppState, item_id: i64, key: &str) {
        state
            .db
            .with_conn(|conn| {
                conn.execute(
                    "INSERT INTO media_item_links (media_item_id, item_key, manually_matched)
                     VALUES (?1, ?2, 0)",
                    rusqlite::params![item_id, key],
                )
                .map_err(|e| e.to_string())?;
                Ok(())
            })
            .unwrap();
    }

    fn series(state: &AppState, library_id: i64, relpath: &str, show_id: Option<i64>) {
        state
            .db
            .with_conn(|conn| {
                conn.execute(
                    "INSERT INTO series (library_id, relpath, tmdb_show_id) VALUES (?1, ?2, ?3)",
                    rusqlite::params![library_id, relpath, show_id],
                )
                .map_err(|e| e.to_string())?;
                Ok(())
            })
            .unwrap();
    }

    /// A second entity bound to one folder (ADR-0046 item 2), with the
    /// folder-season range that entity's seasons appear under.
    #[allow(clippy::too_many_arguments)]
    fn secondary_binding(
        state: &AppState,
        library_id: i64,
        relpath: &str,
        show_id: i64,
        folder_seasons: (i32, i32),
        entity_seasons: (i32, i32),
    ) {
        state
            .db
            .with_conn(|conn| {
                conn.execute(
                    "INSERT INTO series_entity_bindings
                        (library_id, relpath, tmdb_show_id, is_primary,
                         folder_season_start, folder_season_end,
                         entity_season_start, entity_season_end)
                     VALUES (?1, ?2, ?3, 0, ?4, ?5, ?6, ?7)",
                    rusqlite::params![
                        library_id,
                        relpath,
                        show_id,
                        folder_seasons.0,
                        folder_seasons.1,
                        entity_seasons.0,
                        entity_seasons.1
                    ],
                )
                .map_err(|e| e.to_string())?;
                Ok(())
            })
            .unwrap();
    }

    fn canonical_episode(
        state: &AppState,
        episode_id: i64,
        title: &str,
        season: Option<i32>,
        episode: Option<i32>,
        show_id: i64,
    ) {
        state
            .db
            .with_conn(|conn| {
                conn.execute(
                    "INSERT INTO metadata_canonical
                        (provider, entity_kind, provider_id, title, season, episode,
                         tmdb_show, ids_json, projected_at)
                     VALUES ('tmdb', 'episode', ?1, ?2, ?3, ?4, ?5, '{}',
                             '2026-01-01T00:00:00.000Z')",
                    rusqlite::params![episode_id.to_string(), title, season, episode, show_id],
                )
                .map_err(|e| e.to_string())?;
                Ok(())
            })
            .unwrap();
    }

    fn canonical_show(state: &AppState, show_id: i64, title: &str) {
        state
            .db
            .with_conn(|conn| {
                conn.execute(
                    "INSERT INTO metadata_canonical
                        (provider, entity_kind, provider_id, title, ids_json, projected_at)
                     VALUES ('tmdb', 'tv', ?1, ?2, '{}', '2026-01-01T00:00:00.000Z')",
                    rusqlite::params![show_id.to_string(), title],
                )
                .map_err(|e| e.to_string())?;
                Ok(())
            })
            .unwrap();
    }

    fn canonical_movie(state: &AppState, movie_id: i64, title: &str) {
        state
            .db
            .with_conn(|conn| {
                conn.execute(
                    "INSERT INTO metadata_canonical
                        (provider, entity_kind, provider_id, title, ids_json, projected_at)
                     VALUES ('tmdb', 'movie', ?1, ?2, '{}', '2026-01-01T00:00:00.000Z')",
                    rusqlite::params![movie_id.to_string(), title],
                )
                .map_err(|e| e.to_string())?;
                Ok(())
            })
            .unwrap();
    }

    #[allow(clippy::too_many_arguments)]
    fn watch(
        state: &AppState,
        profile_id: i64,
        item_key: &str,
        position_ms: i64,
        duration_ms: i64,
        played: bool,
        hidden: bool,
        at: &str,
    ) {
        state
            .db
            .with_conn(|conn| {
                conn.execute(
                    "INSERT INTO watch_state
                        (profile_id, item_key, position_ms, duration_ms, played, hidden,
                         first_played_at, last_played_at)
                     VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?7)",
                    rusqlite::params![
                        profile_id,
                        item_key,
                        position_ms,
                        duration_ms,
                        played as i32,
                        hidden as i32,
                        at
                    ],
                )
                .map_err(|e| e.to_string())?;
                Ok(())
            })
            .unwrap();
    }

    async fn rail(
        state: &AppState,
        token: &str,
        profile_ref: &str,
        query: &str,
    ) -> (StatusCode, Value) {
        let request = Request::builder()
            .method("GET")
            .uri(format!(
                "/api/v0/profiles/{profile_ref}/continue-watching{query}"
            ))
            .header("authorization", format!("Bearer {token}"))
            .body(Body::empty())
            .unwrap();
        let response = router(state.clone()).oneshot(request).await.unwrap();
        let status = response.status();
        let bytes = axum::body::to_bytes(response.into_body(), 1024 * 1024)
            .await
            .unwrap();
        let value: Value = serde_json::from_slice(&bytes).unwrap_or(Value::Null);
        (status, value)
    }

    fn items(body: &Value) -> &Vec<Value> {
        body["items"]
            .as_array()
            .unwrap_or_else(|| panic!("no items array in {body}"))
    }

    /// Three episodes of a bound show, one canonical row each, linked by
    /// provider key. Returns the media item ids in S01E01..E03 order.
    fn seed_bound_show(state: &AppState, library_id: i64) -> Vec<i64> {
        let ids = seed_items(
            state,
            library_id,
            &[
                ("Alpha/S01E01.mkv", "E1", "episode", Some(1), Some(1)),
                ("Alpha/S01E02.mkv", "E2", "episode", Some(1), Some(2)),
                ("Alpha/S01E03.mkv", "E3", "episode", Some(1), Some(3)),
            ],
        );
        series(state, library_id, "Alpha", Some(100));
        canonical_show(state, 100, "Alpha Show");
        for (index, episode) in [1001, 1002, 1003].iter().enumerate() {
            canonical_episode(state, *episode, "Ep", Some(1), Some(index as i32 + 1), 100);
            link(state, ids[index], &format!("tmdb:episode:{episode}"));
        }
        ids
    }

    /// A movie, its duplicate file, and a second movie. Duplicate files share
    /// one provider key, so they are one logical item (ADR-0025 §2).
    #[tokio::test]
    async fn movies_are_their_own_series_and_duplicate_files_collapse() {
        let dir = tempfile::tempdir().unwrap();
        let state = test_support::state(dir.path());
        let owner = actor(&state, "owner", "owner", "aa");
        let token = token(&state, &owner, true);
        let library = seed_library(&state, "movies", "/media/movies", "movies");
        let ids = seed_items(
            &state,
            library,
            &[
                ("A.mkv", "A", "movie", None, None),
                ("A-dup.mkv", "A", "movie", None, None),
                ("B.mkv", "B", "movie", None, None),
            ],
        );
        link(&state, ids[0], "tmdb:movie:550");
        link(&state, ids[1], "tmdb:movie:550");
        link(&state, ids[2], "tmdb:movie:551");
        canonical_movie(&state, 550, "Alpha");
        canonical_movie(&state, 551, "Beta");
        watch(
            &state,
            owner.profile_id,
            "tmdb:movie:550",
            5_000,
            10_000,
            false,
            false,
            "2026-09-12T10:00:00.000Z",
        );
        watch(
            &state,
            owner.profile_id,
            "tmdb:movie:551",
            1_000,
            10_000,
            false,
            false,
            "2026-09-12T09:00:00.000Z",
        );

        let (status, body) = rail(&state, &token, &owner.profile_ref, "").await;
        assert_eq!(status, StatusCode::OK, "{body}");
        let items = items(&body);
        assert_eq!(items.len(), 2, "duplicate files collapse: {body}");
        assert_eq!(items[0]["itemKey"], "tmdb:movie:550");
        assert_eq!(items[0]["seriesKey"], "tmdb:movie:550");
        assert_eq!(items[0]["itemId"].as_i64(), Some(ids[0]));
        assert_eq!(items[0]["title"], "Alpha");
        assert_eq!(items[0]["kind"], "movie");
        assert_eq!(items[1]["itemKey"], "tmdb:movie:551");
    }

    /// A movie at the played threshold drops off the rail (ADR-0035 item 2),
    /// while an in-progress movie stays.
    #[tokio::test]
    async fn played_movie_drops_off_the_rail() {
        let dir = tempfile::tempdir().unwrap();
        let state = test_support::state(dir.path());
        let owner = actor(&state, "owner", "owner", "aa");
        let token = token(&state, &owner, true);
        let library = seed_library(&state, "movies", "/media/movies", "movies");
        let ids = seed_items(
            &state,
            library,
            &[
                ("Watched.mkv", "Watched", "movie", None, None),
                ("Started.mkv", "Started", "movie", None, None),
            ],
        );
        link(&state, ids[0], "tmdb:movie:550");
        link(&state, ids[1], "tmdb:movie:551");
        canonical_movie(&state, 550, "Watched");
        canonical_movie(&state, 551, "Started");
        watch(
            &state,
            owner.profile_id,
            "tmdb:movie:550",
            9_500,
            10_000,
            true,
            false,
            "2026-09-12T10:00:00.000Z",
        );
        watch(
            &state,
            owner.profile_id,
            "tmdb:movie:551",
            1_000,
            10_000,
            false,
            false,
            "2026-09-12T09:00:00.000Z",
        );

        let (status, body) = rail(&state, &token, &owner.profile_ref, "").await;
        assert_eq!(status, StatusCode::OK, "{body}");
        let items = items(&body);
        assert_eq!(items.len(), 1, "the played movie is gone: {body}");
        assert_eq!(items[0]["itemKey"], "tmdb:movie:551");
    }

    /// A stale `path:` row and a provider row for one matched item collapse to
    /// one entry, and the newer activity wins.
    #[tokio::test]
    async fn stale_path_and_provider_rows_collapse_to_one_entry() {
        let dir = tempfile::tempdir().unwrap();
        let state = test_support::state(dir.path());
        let owner = actor(&state, "owner", "owner", "aa");
        let token = token(&state, &owner, true);
        let library = seed_library(&state, "movies", "/media/movies", "movies");
        let ids = seed_items(&state, library, &[("A.mkv", "A", "movie", None, None)]);
        link(&state, ids[0], "tmdb:movie:550");
        canonical_movie(&state, 550, "Alpha");
        // The row written before the match, then the row written after it.
        watch(
            &state,
            owner.profile_id,
            &format!("path:{library}:A.mkv"),
            9_000,
            10_000,
            false,
            false,
            "2026-09-12T09:00:00.000Z",
        );
        watch(
            &state,
            owner.profile_id,
            "tmdb:movie:550",
            4_000,
            10_000,
            false,
            false,
            "2026-09-12T10:00:00.000Z",
        );

        let (status, body) = rail(&state, &token, &owner.profile_ref, "").await;
        assert_eq!(status, StatusCode::OK, "{body}");
        let items = items(&body);
        assert_eq!(items.len(), 1, "one logical item, one entry: {body}");
        assert_eq!(items[0]["itemKey"], "tmdb:movie:550");
        assert_eq!(items[0]["positionMs"], 4_000, "the newer row wins");
    }

    /// The newest in-progress episode wins, and the show collapses to one row.
    #[tokio::test]
    async fn newest_in_progress_episode_wins() {
        let dir = tempfile::tempdir().unwrap();
        let state = test_support::state(dir.path());
        let owner = actor(&state, "owner", "owner", "aa");
        let token = token(&state, &owner, true);
        let library = seed_library(&state, "shows", "/media/shows", "shows");
        seed_bound_show(&state, library);
        watch(
            &state,
            owner.profile_id,
            "tmdb:episode:1001",
            5_000,
            10_000,
            false,
            false,
            "2026-09-12T10:00:00.000Z",
        );
        watch(
            &state,
            owner.profile_id,
            "tmdb:episode:1002",
            2_000,
            10_000,
            false,
            false,
            "2026-09-12T10:05:00.000Z",
        );

        let (status, body) = rail(&state, &token, &owner.profile_ref, "").await;
        assert_eq!(status, StatusCode::OK, "{body}");
        let items = items(&body);
        assert_eq!(items.len(), 1, "one entry per series: {body}");
        assert_eq!(items[0]["itemKey"], "tmdb:episode:1002");
        assert_eq!(items[0]["seriesKey"], "tmdb:show:100");
        assert_eq!(items[0]["positionMs"], 2_000);
        assert_eq!(items[0]["season"], 1);
        assert_eq!(items[0]["episode"], 2);
        assert_eq!(items[0]["showTitle"], "Alpha Show");
    }

    /// The in-progress entry reports the series' most recent non-hidden
    /// activity, not the chosen episode's own time (ADR-0035 second amendment
    /// item 3). A played episode is newer than the chosen in-progress one, and a
    /// hidden row is newer still but must not set the value.
    #[tokio::test]
    async fn in_progress_entry_reports_newest_series_activity() {
        let dir = tempfile::tempdir().unwrap();
        let state = test_support::state(dir.path());
        let owner = actor(&state, "owner", "owner", "aa");
        let token = token(&state, &owner, true);
        let library = seed_library(&state, "shows", "/media/shows", "shows");
        seed_bound_show(&state, library);
        watch(
            &state,
            owner.profile_id,
            "tmdb:episode:1001",
            9_500,
            10_000,
            true,
            false,
            "2026-09-12T10:05:00.000Z",
        );
        watch(
            &state,
            owner.profile_id,
            "tmdb:episode:1002",
            4_000,
            10_000,
            false,
            false,
            "2026-09-12T10:00:00.000Z",
        );
        watch(
            &state,
            owner.profile_id,
            "tmdb:episode:1003",
            1_000,
            10_000,
            false,
            true,
            "2026-09-12T10:10:00.000Z",
        );

        let (status, body) = rail(&state, &token, &owner.profile_ref, "").await;
        assert_eq!(status, StatusCode::OK, "{body}");
        let items = items(&body);
        assert_eq!(items.len(), 1, "{body}");
        assert_eq!(items[0]["itemKey"], "tmdb:episode:1002");
        assert_eq!(
            items[0]["lastPlayedAt"], "2026-09-12T10:05:00.000Z",
            "the newest non-hidden row, not the chosen row (10:00) or the hidden row (10:10)"
        );
    }

    /// A completed episode advances the show to the next unwatched canonical
    /// episode, with no stored progress.
    #[tokio::test]
    async fn next_episode_after_completed() {
        let dir = tempfile::tempdir().unwrap();
        let state = test_support::state(dir.path());
        let owner = actor(&state, "owner", "owner", "aa");
        let token = token(&state, &owner, true);
        let library = seed_library(&state, "shows", "/media/shows", "shows");
        seed_bound_show(&state, library);
        watch(
            &state,
            owner.profile_id,
            "tmdb:episode:1001",
            9_500,
            10_000,
            true,
            false,
            "2026-09-12T10:00:00.000Z",
        );

        let (status, body) = rail(&state, &token, &owner.profile_ref, "").await;
        assert_eq!(status, StatusCode::OK, "{body}");
        let items = items(&body);
        assert_eq!(items.len(), 1, "{body}");
        assert_eq!(items[0]["itemKey"], "tmdb:episode:1002");
        assert_eq!(items[0]["positionMs"], 0);
        assert_eq!(items[0]["durationMs"], 0);
        assert_eq!(items[0]["played"], false);
        assert_eq!(items[0]["episode"], 2);
        assert_eq!(items[0]["lastPlayedAt"], "2026-09-12T10:00:00.000Z");
    }

    /// ADR-0046 item 3(a): a folder that binds a second entity reads the
    /// folder's numbering. The completed ordinal and the next-episode walk must
    /// use that one scheme, so a folder numbering the entity's S9 as S1
    /// advances S1E1 to S1E2 instead of dropping the show or jumping to an
    /// episode from the other entity.
    #[tokio::test]
    async fn renumbered_binding_walks_the_folders_numbering() {
        let dir = tempfile::tempdir().unwrap();
        let state = test_support::state(dir.path());
        let owner = actor(&state, "owner", "owner", "aa");
        let token = token(&state, &owner, true);
        let library = seed_library(&state, "shows", "/media/shows", "shows");
        let ids = seed_items(
            &state,
            library,
            &[
                ("Alpha/S01E01.mkv", "E1", "episode", Some(1), Some(1)),
                ("Alpha/S01E02.mkv", "E2", "episode", Some(1), Some(2)),
                ("Alpha/S01E03.mkv", "E3", "episode", Some(1), Some(3)),
            ],
        );
        series(&state, library, "Alpha", Some(100));
        canonical_show(&state, 100, "Primary Show");
        // The second entity numbers the run S9 while the folder numbers it S1.
        canonical_show(&state, 200, "Renumbered Show");
        secondary_binding(&state, library, "Alpha", 200, (1, 1), (9, 9));
        for (index, episode) in [2001, 2002, 2003].iter().enumerate() {
            canonical_episode(&state, *episode, "Ep", Some(9), Some(index as i32 + 1), 200);
            link(&state, ids[index], &format!("tmdb:episode:{episode}"));
        }
        watch(
            &state,
            owner.profile_id,
            "tmdb:episode:2001",
            9_500,
            10_000,
            true,
            false,
            "2026-09-12T10:00:00.000Z",
        );

        let (status, body) = rail(&state, &token, &owner.profile_ref, "").await;
        assert_eq!(status, StatusCode::OK, "{body}");
        let next = items(&body);
        assert_eq!(next.len(), 1, "the show stays on the rail: {body}");
        assert_eq!(next[0]["itemKey"], "tmdb:episode:2002");
        assert_eq!(next[0]["seriesKey"], "tmdb:show:100");
        assert_eq!(
            next[0]["season"], 1,
            "the folder's season, not the entity's 9"
        );
        assert_eq!(next[0]["episode"], 2);

        // The same scheme applies to an in-progress entry: it reports the
        // folder's season rather than the entity's canonical season.
        watch(
            &state,
            owner.profile_id,
            "tmdb:episode:2002",
            4_000,
            10_000,
            false,
            false,
            "2026-09-12T10:05:00.000Z",
        );
        let (status, body) = rail(&state, &token, &owner.profile_ref, "").await;
        assert_eq!(status, StatusCode::OK, "{body}");
        let progress = items(&body);
        assert_eq!(progress.len(), 1, "{body}");
        assert_eq!(progress[0]["itemKey"], "tmdb:episode:2002");
        assert_eq!(
            progress[0]["season"], 1,
            "the folder's season, not the entity's 9"
        );
        assert_eq!(progress[0]["episode"], 2);
        assert_eq!(progress[0]["positionMs"], 4_000);
    }

    /// A hidden row never appears and never satisfies the next-episode walk.
    #[tokio::test]
    async fn hidden_rows_never_appear() {
        let dir = tempfile::tempdir().unwrap();
        let state = test_support::state(dir.path());
        let owner = actor(&state, "owner", "owner", "aa");
        let token = token(&state, &owner, true);
        let library = seed_library(&state, "shows", "/media/shows", "shows");
        seed_bound_show(&state, library);
        watch(
            &state,
            owner.profile_id,
            "tmdb:episode:1001",
            9_500,
            10_000,
            true,
            false,
            "2026-09-12T10:00:00.000Z",
        );
        // The episode right after the completed one is hidden, so the walk
        // must skip it rather than offer it.
        watch(
            &state,
            owner.profile_id,
            "tmdb:episode:1002",
            1_000,
            10_000,
            false,
            true,
            "2026-09-12T10:01:00.000Z",
        );

        let (status, body) = rail(&state, &token, &owner.profile_ref, "").await;
        assert_eq!(status, StatusCode::OK, "{body}");
        let items = items(&body);
        assert_eq!(items.len(), 1, "{body}");
        assert_eq!(items[0]["itemKey"], "tmdb:episode:1003");
    }

    /// An unmatched show groups by folder, resumes an in-progress episode, and
    /// never advances to a next episode because filenames are not an order.
    #[tokio::test]
    async fn unmatched_show_resumes_but_never_advances() {
        let dir = tempfile::tempdir().unwrap();
        let state = test_support::state(dir.path());
        let owner = actor(&state, "owner", "owner", "aa");
        let token = token(&state, &owner, true);
        let library = seed_library(&state, "shows", "/media/shows", "shows");
        let ids = seed_items(
            &state,
            library,
            &[
                ("Mystery/E01.mkv", "E1", "episode", Some(1), Some(1)),
                ("Mystery/E02.mkv", "E2", "episode", Some(1), Some(2)),
                ("Other/E01.mkv", "O1", "episode", Some(1), Some(1)),
                // The file a filename order would offer as Other's next
                // episode. It is present so the test can fail if filenames
                // ever order an unmatched show.
                ("Other/E02.mkv", "O2", "episode", Some(1), Some(2)),
            ],
        );
        // The shipped unmatched state: a series row with no entity.
        series(&state, library, "Mystery", None);
        series(&state, library, "Other", None);
        let mystery = format!("path:{library}:Mystery/E01.mkv");
        let mystery2 = format!("path:{library}:Mystery/E02.mkv");
        let other = format!("path:{library}:Other/E01.mkv");
        watch(
            &state,
            owner.profile_id,
            &mystery,
            9_500,
            10_000,
            true,
            false,
            "2026-09-12T10:00:00.000Z",
        );
        watch(
            &state,
            owner.profile_id,
            &mystery2,
            4_000,
            10_000,
            false,
            false,
            "2026-09-12T10:05:00.000Z",
        );
        // Only a completed episode: no in-progress and no canonical order, so
        // this folder drops off the rail rather than advancing by filename.
        watch(
            &state,
            owner.profile_id,
            &other,
            9_500,
            10_000,
            true,
            false,
            "2026-09-12T11:00:00.000Z",
        );
        assert_eq!(ids.len(), 4);

        let (status, body) = rail(&state, &token, &owner.profile_ref, "").await;
        assert_eq!(status, StatusCode::OK, "{body}");
        let items = items(&body);
        assert_eq!(items.len(), 1, "Other must not advance: {body}");
        assert_eq!(items[0]["itemKey"], mystery2);
        assert_eq!(items[0]["seriesKey"], format!("folder:{library}:Mystery"));
        assert_eq!(items[0]["positionMs"], 4_000);
    }

    /// A linked episode with no canonical season/episode cannot be ordered, so
    /// the show does not guess a next episode from its filename numbers.
    #[tokio::test]
    async fn missing_canonical_order_is_not_guessed() {
        let dir = tempfile::tempdir().unwrap();
        let state = test_support::state(dir.path());
        let owner = actor(&state, "owner", "owner", "aa");
        let token = token(&state, &owner, true);
        let library = seed_library(&state, "shows", "/media/shows", "shows");
        let ids = seed_items(
            &state,
            library,
            &[
                ("Alpha/S01E01.mkv", "E1", "episode", Some(1), Some(1)),
                ("Alpha/S01E02.mkv", "E2", "episode", Some(1), Some(2)),
            ],
        );
        series(&state, library, "Alpha", Some(100));
        canonical_show(&state, 100, "Alpha Show");
        canonical_episode(&state, 1001, "One", Some(1), Some(1), 100);
        link(&state, ids[0], "tmdb:episode:1001");
        // Matched, but no canonical numbering: the file's own S01E02 is not an
        // order the rollup may use.
        link(&state, ids[1], "tmdb:episode:1002");
        watch(
            &state,
            owner.profile_id,
            "tmdb:episode:1001",
            9_500,
            10_000,
            true,
            false,
            "2026-09-12T10:00:00.000Z",
        );

        let (status, body) = rail(&state, &token, &owner.profile_ref, "").await;
        assert_eq!(status, StatusCode::OK, "{body}");
        assert!(
            items(&body).is_empty(),
            "no canonical order means no next episode: {body}"
        );
    }

    /// Two series with the same timestamp sort deterministically by series key.
    #[tokio::test]
    async fn deterministic_tie_sorts_by_series_key() {
        let dir = tempfile::tempdir().unwrap();
        let state = test_support::state(dir.path());
        let owner = actor(&state, "owner", "owner", "aa");
        let token = token(&state, &owner, true);
        let library = seed_library(&state, "movies", "/media/movies", "movies");
        let ids = seed_items(
            &state,
            library,
            &[
                ("A.mkv", "A", "movie", None, None),
                ("B.mkv", "B", "movie", None, None),
            ],
        );
        link(&state, ids[0], "tmdb:movie:551");
        link(&state, ids[1], "tmdb:movie:550");
        let at = "2026-09-12T10:00:00.000Z";
        watch(
            &state,
            owner.profile_id,
            "tmdb:movie:551",
            1_000,
            10_000,
            false,
            false,
            at,
        );
        watch(
            &state,
            owner.profile_id,
            "tmdb:movie:550",
            1_000,
            10_000,
            false,
            false,
            at,
        );

        let (status, body) = rail(&state, &token, &owner.profile_ref, "").await;
        assert_eq!(status, StatusCode::OK, "{body}");
        let items = items(&body);
        assert_eq!(items.len(), 2);
        assert_eq!(items[0]["seriesKey"], "tmdb:movie:550");
        assert_eq!(items[1]["seriesKey"], "tmdb:movie:551");
    }

    /// The limit is applied after series collapse, so a show with two watched
    /// episodes still costs one slot.
    #[tokio::test]
    async fn limit_applies_after_series_collapse() {
        let dir = tempfile::tempdir().unwrap();
        let state = test_support::state(dir.path());
        let owner = actor(&state, "owner", "owner", "aa");
        let token = token(&state, &owner, true);
        let library = seed_library(&state, "shows", "/media/shows", "shows");
        seed_bound_show(&state, library);
        watch(
            &state,
            owner.profile_id,
            "tmdb:episode:1001",
            5_000,
            10_000,
            false,
            false,
            "2026-09-12T10:00:00.000Z",
        );
        watch(
            &state,
            owner.profile_id,
            "tmdb:episode:1002",
            3_000,
            10_000,
            false,
            false,
            "2026-09-12T10:01:00.000Z",
        );
        let movie_library = seed_library(&state, "movies", "/media/movies", "movies");
        let movie_ids = seed_items(
            &state,
            movie_library,
            &[("Film.mkv", "Film", "movie", None, None)],
        );
        link(&state, movie_ids[0], "tmdb:movie:550");
        watch(
            &state,
            owner.profile_id,
            "tmdb:movie:550",
            1_000,
            10_000,
            false,
            false,
            "2026-09-12T09:00:00.000Z",
        );

        let (status, body) = rail(&state, &token, &owner.profile_ref, "").await;
        assert_eq!(status, StatusCode::OK, "{body}");
        assert_eq!(items(&body).len(), 2, "two series without a limit: {body}");

        let (status, body) = rail(&state, &token, &owner.profile_ref, "?limit=1").await;
        assert_eq!(status, StatusCode::OK, "{body}");
        let limited = items(&body);
        assert_eq!(limited.len(), 1, "{body}");
        assert_eq!(limited[0]["seriesKey"], "tmdb:show:100");

        let (status, body) = rail(&state, &token, &owner.profile_ref, "?limit=0").await;
        assert_eq!(status, StatusCode::OK, "{body}");
        assert!(items(&body).is_empty(), "{body}");

        // A limit the server cannot read is the typed 400, not axum's
        // plain-text query rejection.
        let (status, body) = rail(&state, &token, &owner.profile_ref, "?limit=abc").await;
        assert_eq!(status, StatusCode::BAD_REQUEST, "{body}");
        assert_eq!(body["code"], "bad_request", "{body}");

        // A negative limit is outside the documented non-negative int32.
        let (status, body) = rail(&state, &token, &owner.profile_ref, "?limit=-1").await;
        assert_eq!(status, StatusCode::BAD_REQUEST, "{body}");
        assert_eq!(body["code"], "bad_request", "{body}");

        // The documented type is int32, so its maximum is accepted and one past
        // it is refused rather than silently accepted as a wider unsigned value.
        let (status, body) = rail(&state, &token, &owner.profile_ref, "?limit=2147483647").await;
        assert_eq!(status, StatusCode::OK, "{body}");
        let (status, body) = rail(&state, &token, &owner.profile_ref, "?limit=2147483648").await;
        assert_eq!(status, StatusCode::BAD_REQUEST, "{body}");
        assert_eq!(body["code"], "bad_request", "{body}");
    }

    #[tokio::test]
    async fn empty_rail_is_an_empty_list() {
        let dir = tempfile::tempdir().unwrap();
        let state = test_support::state(dir.path());
        let owner = actor(&state, "owner", "owner", "aa");
        let token = token(&state, &owner, true);

        let (status, body) = rail(&state, &token, &owner.profile_ref, "").await;
        assert_eq!(status, StatusCode::OK, "{body}");
        assert_eq!(items(&body).len(), 0, "{body}");
    }

    /// Every authorization branch of ADR-0035 item 7, and the refusals do not
    /// leak whether the ref exists.
    #[tokio::test]
    async fn every_authorization_branch_is_pinned() {
        let dir = tempfile::tempdir().unwrap();
        let state = test_support::state(dir.path());
        let owner = actor(&state, "owner", "owner", "aa");
        let member = actor(&state, "member", "member", "bb");
        let manager = actor(&state, "manager", "manager", "cc");

        let watcher = token(&state, &member, true);
        let member_scope = token(&state, &member, false);
        let owner_scope = token(&state, &owner, false);
        let manager_scope = token(&state, &manager, false);

        // A profile session reaches its own profile.
        let (status, body) = rail(&state, &watcher, &member.profile_ref, "").await;
        assert_eq!(status, StatusCode::OK, "{body}");

        // ... and no other, whether it exists or not.
        for ref_ in [&owner.profile_ref, "ffffffffffffffffffffffffffffffff"] {
            let (status, body) = rail(&state, &watcher, ref_, "").await;
            assert_eq!(status, StatusCode::FORBIDDEN, "{ref_}: {body}");
            assert_eq!(body["code"], "forbidden", "{ref_}: {body}");
        }

        // A member account scope reaches its own account's profiles, not
        // another account's.
        let (status, body) = rail(&state, &member_scope, &member.profile_ref, "").await;
        assert_eq!(status, StatusCode::OK, "{body}");
        let (status, body) = rail(&state, &member_scope, &owner.profile_ref, "").await;
        assert_eq!(status, StatusCode::FORBIDDEN, "{body}");
        assert_eq!(body["code"], "forbidden", "{body}");

        // An owner or manager account scope reaches any profile.
        for scope in [&owner_scope, &manager_scope] {
            let (status, body) = rail(&state, scope, &member.profile_ref, "").await;
            assert_eq!(status, StatusCode::OK, "{body}");
        }
    }
}
