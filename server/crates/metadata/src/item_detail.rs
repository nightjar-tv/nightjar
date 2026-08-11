//! One item's canonical facts, assembled for a detail surface.
//!
//! ADR-0029 §1.2 is the whole shape of this module: canonical persistence is
//! kind-sparse, so an episode row carries its own plot, air date, rating and
//! runtime while genres and cast are deliberately not copied onto 23k rows and
//! are read from the show instead. "Filled episode card = episode row + one tv
//! get" is the rule, and [`item_metadata`] is where it lives.

use rusqlite::{Connection, OptionalExtension, params};

use crate::artwork::resolve_artwork_key;
use crate::canonical::get_canonical;
use crate::item_links::{
    EPISODE_KEY_PREFIX, MOVIE_KEY_PREFIX, SHOW_KEY_PREFIX, effective_item_key,
};
use crate::model::{ArtworkKind, CanonicalMetadata, CastMember, Rating};

/// Canonical facts behind one media row, ready to render.
///
/// Empty vectors and `None` mean "this title has none", not "not looked up":
/// an absent field is a layout the client draws without, and never a request
/// it should make separately.
#[derive(Debug, Clone, Default)]
pub struct ItemMetadata {
    /// Opaque item_key (ADR-0025 §1), resolved once here so the caller does
    /// not compute it a second time and risk a different answer.
    pub item_key: String,
    /// Canonical title, when the item has a canonical row at all.
    pub title: Option<String>,
    pub plot: Option<String>,
    pub runtime_minutes: Option<i32>,
    pub air_date: Option<String>,
    pub genres: Vec<String>,
    pub ratings: Vec<Rating>,
    pub cast: Vec<CastMember>,
    /// Artwork the item actually has, by kind. A kind missing here is absent
    /// for this title; the client draws the layout without it rather than
    /// requesting an image that cannot exist.
    pub artwork: Vec<ItemArtwork>,
    /// Series this episode belongs to, so the page can link back to it.
    pub series_key: Option<String>,
    pub show_title: Option<String>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ItemArtwork {
    pub kind: ArtworkKind,
    /// Key the image is cached and served under (ADR-0027 §1).
    pub item_key: String,
}

/// Kinds an item page renders. `Other` (the episode still) and `Banner` are
/// stored but have no surface yet, so they are not advertised.
const RENDERED_ARTWORK: [ArtworkKind; 3] = [
    ArtworkKind::Poster,
    ArtworkKind::Backdrop,
    ArtworkKind::Logo,
];

/// Canonical metadata for one media row, or an empty set when it is unmatched.
///
/// ADR-0029 §1.2 is the whole shape of this function: canonical persistence is
/// kind-sparse, so an episode row carries its own plot, air date, rating and
/// runtime while genres and cast are deliberately *not* copied onto 23k rows
/// and are read from the show instead. A filled episode card is the episode row
/// plus one tv get, and that is what this does.
pub fn item_metadata(
    conn: &Connection,
    media_item_id: i64,
    library_id: i64,
    relpath: &str,
) -> Result<ItemMetadata, String> {
    let item_key = effective_item_key(conn, media_item_id, library_id, relpath)?;
    let Some((entity_kind, provider_id)) = provider_entity(&item_key) else {
        // ADR-0029 §1.3: no provider entity, no canonical row. The client falls
        // back to the scan fields already on the response.
        return unmatched(conn, item_key);
    };
    let Some(own) = get_canonical(conn, "tmdb", entity_kind, &provider_id)? else {
        return unmatched(conn, item_key);
    };

    let show = show_row_for_episode(conn, entity_kind, &provider_id)?;
    let show_key = show
        .as_ref()
        .map(|(id, _)| format!("{SHOW_KEY_PREFIX}{id}"));
    // Art follows the entity edge (ADR-0039 item 6). An episode row holds a
    // still and nothing else (ADR-0029 §1.2), so poster, backdrop and logo come
    // from the show, which is also where a viewer expects them from.
    let art_key = show_key.clone().unwrap_or_else(|| item_key.clone());
    let artwork = advertised_artwork(conn, &art_key)?;

    let show_meta = show.as_ref().map(|(_, meta)| meta);
    Ok(ItemMetadata {
        item_key,
        title: Some(own.title),
        plot: own.plot,
        runtime_minutes: own.runtime_minutes,
        air_date: own.air_date,
        genres: pick_inherited(own.genres, show_meta.map(|m| &m.genres)),
        cast: pick_inherited(own.cast, show_meta.map(|m| &m.cast)),
        // Ratings are the episode's own vote (ADR-0029 §1.2) and do not
        // inherit: a show's score is not this episode's score.
        ratings: own.ratings,
        artwork,
        series_key: show_key,
        show_title: show_meta.map(|m| m.title.clone()),
    })
}

/// An item with no canonical row: a key, whatever art the serve path can find,
/// and nothing else.
///
/// **The artwork is the part that is not obvious.** An unmatched key still
/// reaches art through the ADR-0026 §8.4 provisional show link, which is what
/// the grid already renders posters from, so returning an empty array here
/// would advertise "this title has none" about images the very next request
/// serves.
fn unmatched(conn: &Connection, item_key: String) -> Result<ItemMetadata, String> {
    let artwork = advertised_artwork(conn, &item_key)?;
    Ok(ItemMetadata {
        item_key,
        artwork,
        ..ItemMetadata::default()
    })
}

/// The kinds this key can actually be served, resolved the way the serve route
/// resolves them.
///
/// **One resolver, because two were answering the same question differently**
/// (Rule 4.11). `GET /items/{id}` used to read the canonical row directly while
/// `GET /artwork/{key}/{kind}` used [`resolve_artwork_key`], which also follows
/// a path key to the item's `tmdb:show:` / `tmdb:movie:` link. On the dogfood
/// library that gap covered 326 TV files: the detail response said `artwork:
/// null` for titles whose poster and backdrop the artwork route served on
/// request. ADR-0027 §6 makes absence from this array load-bearing — it is how
/// a client tells "none exists" from "not cached yet" — so an advertisement
/// narrower than the serve path is not a missing feature, it is a wrong answer
/// in the direction that hides working images.
///
/// The entry carries the key the bytes are **served** under rather than the key
/// asked about, so the URL a client follows is the one the store caches and the
/// drain warms, and the fallback is walked once here instead of on every image
/// GET.
fn advertised_artwork(conn: &Connection, art_key: &str) -> Result<Vec<ItemArtwork>, String> {
    let mut artwork = Vec::new();
    for kind in RENDERED_ARTWORK {
        let (serve_key, source) = resolve_artwork_key(conn, art_key, kind)?;
        if source.is_some() {
            artwork.push(ItemArtwork {
                kind,
                item_key: serve_key,
            });
        }
    }
    Ok(artwork)
}

/// The show entity behind an episode, with its canonical row. `None` for a
/// movie, which has no parent to inherit from.
fn show_row_for_episode(
    conn: &Connection,
    entity_kind: &str,
    provider_id: &str,
) -> Result<Option<(i64, CanonicalMetadata)>, String> {
    if entity_kind != "episode" {
        return Ok(None);
    }
    let show_id: Option<i64> = conn
        .query_row(
            "SELECT tmdb_show FROM metadata_canonical
             WHERE provider = 'tmdb' AND entity_kind = 'episode' AND provider_id = ?1",
            params![provider_id],
            |r| r.get(0),
        )
        .optional()
        .map_err(|e| format!("episode parent show: {e}"))?
        .flatten();
    let Some(show_id) = show_id else {
        return Ok(None);
    };
    Ok(get_canonical(conn, "tmdb", "tv", &show_id.to_string())?.map(|meta| (show_id, meta)))
}

/// The item's own value when it has one, else the show's. Kind-sparse
/// persistence means an empty vector on an episode row is "look up tv", not
/// "this show has no cast" (ADR-0029 §1.2).
fn pick_inherited<T: Clone>(own: Vec<T>, inherited: Option<&Vec<T>>) -> Vec<T> {
    if !own.is_empty() {
        return own;
    }
    inherited.cloned().unwrap_or_default()
}

fn provider_entity(item_key: &str) -> Option<(&'static str, String)> {
    if let Some(id) = item_key.strip_prefix(MOVIE_KEY_PREFIX) {
        return Some(("movie", id.to_string()));
    }
    if let Some(id) = item_key.strip_prefix(EPISODE_KEY_PREFIX) {
        return Some(("episode", id.to_string()));
    }
    None
}

#[cfg(test)]
mod tests {
    use super::*;
    use nightjar_db::migrate;

    /// One bound show with an episode, one matched movie, one file with no
    /// canonical row at all. Sized to what this module asserts rather than
    /// copied from the browse fixture, which exists to exercise unit identity.
    fn fixture() -> Connection {
        let c = Connection::open_in_memory().unwrap();
        migrate(&c).unwrap();
        c.execute_batch(
            "INSERT INTO libraries (id, name, path, kind)
                  VALUES (1, 'Movies', '/Movies', 'movies'),
                         (2, 'TV', '/TV', 'shows');

             INSERT INTO media_items (id, library_id, path, mtime_ms, size_bytes, title, kind,
                                      season, episode)
             VALUES (1, 2, 'Futurama/Season 1/S01E04.mkv', 1, 1, 'S01E04', 'episode', 1, 4),
                    (9, 2, 'Mystery Folder/e1.mkv', 1, 1, 'e1', 'episode', NULL, NULL),
                    (20, 1, 'Fight Club (1999)/f.mkv', 1, 1, 'Fight Club', 'movie', NULL, NULL);

             INSERT INTO metadata_canonical
                  (provider, entity_kind, provider_id, title, year, plot, runtime_minutes,
                   genres_json, cast_json, ratings_json, artwork_json, ids_json, projected_at,
                   season, episode, tmdb_show)
             VALUES
               -- The show: genres, cast and artwork live here, not on episodes.
               ('tmdb', 'tv', '615', 'Futurama', 1999, 'A delivery boy is frozen.', NULL,
                '[\"Animation\",\"Comedy\"]',
                '[{\"name\":\"Billy West\",\"role\":\"Fry\",\"order\":0}]',
                NULL,
                '[{\"kind\":\"poster\",\"path\":\"/p.jpg\"},
                  {\"kind\":\"backdrop\",\"path\":\"/b.jpg\"}]',
                '{}', 'now', NULL, NULL, NULL),
               -- The episode: its own plot, runtime and vote; no genres or cast.
               ('tmdb', 'episode', '101', 'Space Pilot 3000', 1999, 'Fry is frozen.', 22,
                NULL, NULL, '[{\"source\":\"themoviedb\",\"value\":8.1,\"votes\":42}]',
                NULL, '{}', 'now', 1, 1, 615),
               ('tmdb', 'movie', '550', 'Fight Club', 1999, 'A ticking-time-bomb insomniac.',
                139, '[\"Drama\"]',
                '[{\"name\":\"Edward Norton\",\"role\":\"The Narrator\",\"order\":0}]',
                '[{\"source\":\"imdb\",\"value\":8.4,\"votes\":993807}]',
                '[{\"kind\":\"poster\",\"path\":\"/p.jpg\"}]', '{}', 'now',
                NULL, NULL, NULL);

             INSERT INTO media_item_links (media_item_id, item_key)
                  VALUES (1, 'tmdb:episode:101'), (20, 'tmdb:movie:550');",
        )
        .unwrap();
        c
    }

    /// ADR-0029 §1.2: "filled episode card = episode row + one tv get".
    /// Persistence is kind-sparse, so the episode's own plot, air date, rating
    /// and runtime win, while genres and cast are read from the show because
    /// they were deliberately never copied onto 23k episode rows.
    #[test]
    fn episode_metadata_inherits_from_its_show() {
        let c = fixture();

        let meta = item_metadata(&c, 1, 2, "Futurama/Season 1/S01E04.mkv").unwrap();
        assert_eq!(meta.item_key, "tmdb:episode:101");
        assert_eq!(meta.title.as_deref(), Some("Space Pilot 3000"));
        assert_eq!(meta.plot.as_deref(), Some("Fry is frozen."));
        assert_eq!(meta.runtime_minutes, Some(22));
        // Inherited from the show, because the episode row holds neither.
        assert_eq!(meta.genres, vec!["Animation", "Comedy"]);
        assert_eq!(meta.cast.len(), 1);
        assert_eq!(meta.cast[0].role.as_deref(), Some("Fry"));
        // Not inherited: a show's score is not this episode's score.
        assert_eq!(meta.ratings.len(), 1);
        assert_eq!(meta.ratings[0].value, 8.1);
        // Art follows the entity edge and is served under the show's key.
        assert_eq!(meta.series_key.as_deref(), Some("tmdb:show:615"));
        assert_eq!(meta.show_title.as_deref(), Some("Futurama"));
        assert_eq!(
            meta.artwork,
            vec![
                ItemArtwork {
                    kind: ArtworkKind::Poster,
                    item_key: "tmdb:show:615".into()
                },
                ItemArtwork {
                    kind: ArtworkKind::Backdrop,
                    item_key: "tmdb:show:615".into()
                },
            ],
            "a logo the show does not have is absent, not an empty URL"
        );
    }

    #[test]
    fn movie_metadata_comes_from_its_own_row() {
        let c = fixture();
        let meta = item_metadata(&c, 20, 1, "Fight Club (1999)/f.mkv").unwrap();
        assert_eq!(meta.item_key, "tmdb:movie:550");
        assert_eq!(meta.runtime_minutes, Some(139));
        assert_eq!(meta.genres, vec!["Drama"]);
        assert_eq!(meta.ratings[0].source, "imdb");
        assert_eq!(meta.ratings[0].votes, Some(993_807));
        // A movie has no parent to inherit from and no series to link to.
        assert_eq!(meta.series_key, None);
        assert_eq!(meta.artwork.len(), 1);
        assert_eq!(meta.artwork[0].item_key, "tmdb:movie:550");
    }

    /// ADR-0029 §1.3: no provider entity, no canonical row. The response still
    /// carries a key, and the client falls back to the scan fields it already
    /// has rather than showing an error.
    ///
    /// This item has no link at all, so it has no art either — which is the
    /// case the next test exists to distinguish from.
    #[test]
    fn unmatched_item_with_no_link_has_a_key_and_nothing_else() {
        let c = fixture();
        let meta = item_metadata(&c, 9, 2, "Mystery Folder/e1.mkv").unwrap();
        assert_eq!(meta.item_key, "path:2:Mystery Folder/e1.mkv");
        assert_eq!(meta.title, None);
        assert!(meta.genres.is_empty());
        assert!(meta.artwork.is_empty());
        assert_eq!(meta.series_key, None);
    }

    /// An episode matched at show level but not yet season-bound: the effective
    /// key is the path key (`tmdb:show:` is not watch-shaped, `item_links::
    /// is_watch_item_key`), and the show link still carries art.
    ///
    /// **This is the regression.** The detail response advertised nothing while
    /// `GET /artwork/{path key}/poster` served the show's poster through the
    /// same link, so ADR-0027 §6's "absent means this title has none" was false
    /// for 326 files on the dogfood library. The assertion that matters is not
    /// that art appears, but that it appears **under the key the serve path
    /// resolves to**, because that is what makes the advertised URL the one the
    /// store already holds.
    #[test]
    fn a_show_linked_episode_advertises_the_art_the_serve_path_would_return() {
        let c = fixture();
        c.execute_batch(
            "INSERT INTO media_items (id, library_id, path, mtime_ms, size_bytes, title, kind,
                                      season, episode)
                  VALUES (30, 2, 'Futurama/Season 9/S09E01.mkv', 1, 1, 'S09E01', 'episode', 9, 1);
             INSERT INTO media_item_links (media_item_id, item_key)
                  VALUES (30, 'tmdb:show:615');",
        )
        .unwrap();

        let meta = item_metadata(&c, 30, 2, "Futurama/Season 9/S09E01.mkv").unwrap();
        assert_eq!(
            meta.item_key, "path:2:Futurama/Season 9/S09E01.mkv",
            "a provisional show link is not a watch key (ADR-0026 §8.4)"
        );
        assert_eq!(
            meta.title, None,
            "still no canonical row, so still no facts"
        );
        assert_eq!(
            meta.artwork,
            vec![
                ItemArtwork {
                    kind: ArtworkKind::Poster,
                    item_key: "tmdb:show:615".into()
                },
                ItemArtwork {
                    kind: ArtworkKind::Backdrop,
                    item_key: "tmdb:show:615".into()
                },
            ],
            "advertised under the show key the artwork route serves from, not \
             under the path key that was asked about"
        );
    }
}
