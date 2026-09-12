//! Browse units: what a library listing returns, and what one series holds.
//!
//! A unit is one show in a shows library and one film in a movies library,
//! keyed by `series_key` either way (ADR-0039 items 2 and 4). The folder is the
//! grouping edge for shows because it has an answer before any provider match
//! (ADR-0039 item 6), which is what keeps unmatched shows grouped instead of
//! scattered across the listing as loose files.
//!
//! A unit is not a folder and not a file. Both collide onto one key by design
//! (ADR-0039 item 8, ADR-0025 §2), so the listing groups on the key: one unit
//! per key, item counts summed, and no key repeated in a response.
//!
//! Episode order inside a series comes from canonical season and episode
//! numbers and never from filenames (ADR-0035 item 8). Episodes with no
//! canonical numbering group under their series but cannot be ordered, so they
//! are listed separately rather than guessed into a season.

use rusqlite::{Connection, OptionalExtension, params};
use std::collections::{BTreeMap, HashMap};

use nightjar_db::{resolve_media_path, show_folder_relpath};

use crate::item_links::{
    EPISODE_KEY_PREFIX, FOLDER_KEY_PREFIX, MOVIE_KEY_PREFIX, PathKeyError, SHOW_KEY_PREFIX,
    effective_item_keys_for_library, parse_path_key, series_key_for_show_folder,
};
use crate::model::ArtworkKind;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum UnitKind {
    Series,
    Movie,
}

impl UnitKind {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Series => "series",
            Self::Movie => "movie",
        }
    }
}

/// Which of ADR-0039 item 6's two edges answers for this unit.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum UnitIdentity {
    /// The folder is bound to a show entity, or the item holds a provider link.
    Bound,
    /// No folder binding, but the folder's episodes name a show entity. The
    /// show is known and the folder is not, which is the state a `tv` canonical
    /// row with no `series` row shows up as from the file side.
    EntityOnly,
    /// Neither edge answers: the below-floor fraction ADR-0025 §4 prices.
    Unidentified,
}

impl UnitIdentity {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Bound => "bound",
            Self::EntityOnly => "entityOnly",
            Self::Unidentified => "unidentified",
        }
    }
}

#[derive(Debug, Clone)]
pub struct BrowseUnit {
    pub series_key: String,
    pub kind: UnitKind,
    pub identity: UnitIdentity,
    pub title: String,
    pub year: Option<i32>,
    pub item_count: i64,
    /// The media row to open, present only when the unit collects exactly one.
    /// A film with two versions withholds it rather than picking one.
    pub item_id: Option<i64>,
    /// Provider key the unit's artwork is cached under, when it has one.
    pub poster_key: Option<String>,
}

#[derive(Debug, Clone, Copy, Default)]
pub struct UnitCounts {
    pub units: i64,
    pub bound: i64,
    pub entity_only: i64,
    pub unidentified: i64,
    pub items: i64,
    /// Shows libraries only. Server-wide, because a show entity is not
    /// library-scoped (ADR-0039 item 1).
    pub show_entities_without_binding: Option<i64>,
}

#[derive(Debug, Clone)]
pub struct LibraryUnits {
    pub unit_kind: UnitKind,
    pub units: Vec<BrowseUnit>,
    pub counts: UnitCounts,
}

#[derive(Debug, Clone)]
pub struct SeriesEpisode {
    pub item_id: i64,
    pub item_key: String,
    pub title: String,
    pub season: Option<i32>,
    pub episode: Option<i32>,
    pub file_season: Option<i32>,
    pub file_episode: Option<i32>,
    pub air_date: Option<String>,
    pub path: String,
    pub probe_status: String,
    pub metadata_status: String,
}

impl SeriesEpisode {
    pub fn canonical_numbering(&self) -> bool {
        self.season.is_some() && self.episode.is_some()
    }
}

#[derive(Debug, Clone)]
pub struct SeriesSeason {
    pub season: i32,
    pub episodes: Vec<SeriesEpisode>,
}

#[derive(Debug, Clone)]
pub struct SeriesDetail {
    pub series_key: String,
    pub kind: UnitKind,
    pub identity: UnitIdentity,
    pub title: String,
    pub year: Option<i32>,
    pub plot: Option<String>,
    pub poster_key: Option<String>,
    pub item_count: i64,
    pub seasons: Vec<SeriesSeason>,
    pub unnumbered: Vec<SeriesEpisode>,
}

/// A library row, read here for the same reason [`crate::fix`] reads one: the
/// root is needed to turn a stored relpath into a show folder and a path.
struct Library {
    id: i64,
    name: String,
    root: String,
    kind: String,
}

/// One media row joined to the canonical episode it is linked to, if any.
struct ItemRow {
    id: i64,
    path: String,
    title: String,
    file_season: Option<i32>,
    file_episode: Option<i32>,
    probe_status: String,
    metadata_status: String,
    canonical_season: Option<i32>,
    canonical_episode: Option<i32>,
    canonical_title: Option<String>,
    air_date: Option<String>,
    /// Show entity this episode's canonical row belongs to (ADR-0029 §1.6).
    tmdb_show: Option<i64>,
}

struct ShowMeta {
    title: String,
    year: Option<i32>,
    plot: Option<String>,
    /// Whether the canonical row holds a poster ref at all. A unit with none
    /// must not advertise a URL: the image does not exist to be fetched, so
    /// the client would paint a broken tile rather than wait for one.
    has_poster: bool,
}

pub fn list_library_units(conn: &Connection, library_id: i64) -> Result<LibraryUnits, String> {
    let library =
        library(conn, library_id)?.ok_or_else(|| format!("library {library_id} not found"))?;
    if library.kind == "movies" {
        movie_units(conn, &library)
    } else {
        series_units(conn, &library)
    }
}

/// What one `series_key` has collected so far while the listing is built.
///
/// Keyed on the series key rather than on the folder or the file, because both
/// legitimately collide onto one key: two folders bound to one show are one
/// series (ADR-0039 item 8), and two files of one film are one movie
/// (ADR-0025 §2, the collision ADR-0039 item 7 merges). A unit per folder or
/// per file would show the same title twice and hand a client a key that is
/// not unique in its own listing.
#[derive(Default)]
struct UnitTally {
    items: i64,
    /// Set only when the unit collects exactly one media row, so a caller
    /// cannot silently be pointed at one of several versions.
    only_item_id: Option<i64>,
    show_entity: Option<i64>,
    bound: bool,
    /// Fallback when the unit has no canonical metadata to name it.
    fallback_title: String,
    fallback_year: Option<i32>,
}

impl UnitTally {
    fn add(&mut self, item_id: i64) {
        self.only_item_id = if self.items == 0 { Some(item_id) } else { None };
        self.items += 1;
    }
}

fn movie_units(conn: &Connection, library: &Library) -> Result<LibraryUnits, String> {
    let keys = effective_item_keys_for_library(conn, library.id)?;
    let movies = movie_canonical(conn)?;

    let mut stmt = conn
        .prepare("SELECT id, title, year FROM media_items WHERE library_id = ?1")
        .map_err(|e| format!("prepare movie units: {e}"))?;
    let rows = stmt
        .query_map(params![library.id], |r| {
            Ok((
                r.get::<_, i64>(0)?,
                r.get::<_, String>(1)?,
                r.get::<_, Option<i32>>(2)?,
            ))
        })
        .map_err(|e| format!("query movie units: {e}"))?;

    let mut tallies: BTreeMap<String, UnitTally> = BTreeMap::new();
    let mut items = 0i64;
    for row in rows {
        let (id, title, year) = row.map_err(|e| format!("movie unit row: {e}"))?;
        // A movie's series key is its own item_key (ADR-0039 item 2), so the
        // two never disagree and there is no second grammar for the one kind
        // where a series is a single file.
        let key = keys
            .get(&id)
            .cloned()
            .ok_or_else(|| format!("no item key for media item {id}"))?;
        items += 1;
        let tally = tallies.entry(key).or_default();
        tally.add(id);
        if tally.fallback_title.is_empty() {
            tally.fallback_title = title;
            tally.fallback_year = year;
        }
    }

    let mut counts = UnitCounts {
        items,
        ..UnitCounts::default()
    };
    let mut units = Vec::with_capacity(tallies.len());
    for (key, tally) in tallies {
        let meta = key
            .strip_prefix(MOVIE_KEY_PREFIX)
            .and_then(|id| movies.get(id));
        let identity = if meta.is_some() {
            UnitIdentity::Bound
        } else {
            UnitIdentity::Unidentified
        };
        bump(&mut counts, identity);
        units.push(BrowseUnit {
            poster_key: meta.filter(|m| m.has_poster).map(|_| key.clone()),
            title: meta.map_or(tally.fallback_title, |m| m.title.clone()),
            year: meta.and_then(|m| m.year).or(tally.fallback_year),
            series_key: key,
            kind: UnitKind::Movie,
            identity,
            item_count: tally.items,
            item_id: tally.only_item_id,
        });
    }

    sort_units(&mut units);
    counts.units = units.len() as i64;
    Ok(LibraryUnits {
        unit_kind: UnitKind::Movie,
        units,
        counts,
    })
}

fn series_units(conn: &Connection, library: &Library) -> Result<LibraryUnits, String> {
    let bindings = series_bindings(conn, library.id)?;
    let shows = show_canonical(conn)?;
    let items = library_items(conn, library.id)?;

    let mut tallies: BTreeMap<String, UnitTally> = BTreeMap::new();
    for item in &items {
        let folder = show_folder_relpath(&item.path, &library.root);
        let bound = bindings.get(&folder).copied();
        let key = series_key_for_show_folder(library.id, &folder, bound);
        let tally = tallies.entry(key).or_default();
        tally.add(item.id);
        tally.bound |= bound.is_some();
        if tally.show_entity.is_none() {
            tally.show_entity = bound.or(item.tmdb_show);
        }
        if tally.fallback_title.is_empty() {
            tally.fallback_title = folder_title(&folder, &library.name);
        }
    }

    let mut counts = UnitCounts {
        items: items.len() as i64,
        show_entities_without_binding: Some(show_entities_without_binding(conn)?),
        ..UnitCounts::default()
    };
    let mut units = Vec::with_capacity(tallies.len());
    for (key, tally) in tallies {
        let identity = match (tally.bound, tally.show_entity) {
            (true, _) => UnitIdentity::Bound,
            (false, Some(_)) => UnitIdentity::EntityOnly,
            (false, None) => UnitIdentity::Unidentified,
        };
        let meta = tally.show_entity.and_then(|id| shows.get(&id));
        bump(&mut counts, identity);
        units.push(BrowseUnit {
            series_key: key,
            kind: UnitKind::Series,
            identity,
            title: meta.map_or(tally.fallback_title, |m| m.title.clone()),
            year: meta.and_then(|m| m.year),
            item_count: tally.items,
            // A series is opened by its key, never by one of its episodes.
            item_id: None,
            // Art follows the entity edge, so an unbound folder whose episodes
            // know their show still shows the show's poster. Only when the row
            // holds one: a URL for artwork that does not exist is a broken tile.
            poster_key: tally
                .show_entity
                .filter(|_| meta.is_some_and(|m| m.has_poster))
                .map(|id| format!("{SHOW_KEY_PREFIX}{id}")),
        });
    }

    sort_units(&mut units);
    counts.units = units.len() as i64;
    Ok(LibraryUnits {
        unit_kind: UnitKind::Series,
        units,
        counts,
    })
}

/// One unit and the media rows under it, resolved from an opaque `series_key`.
///
/// A `tmdb:show:` key can be bound by more than one folder, and by folders in
/// more than one library; ADR-0039 item 8 makes that one series on purpose, so
/// the detail is the union. A movie key resolves too, because a movie is a
/// series of one (ADR-0039 item 2) and its several files are the only way a
/// caller can reach past the listing's single merged unit. `Ok(None)` means
/// nothing resolves under the key.
pub fn get_series(conn: &Connection, series_key: &str) -> Result<Option<SeriesDetail>, String> {
    match resolve_series_key(conn, series_key)? {
        SeriesScope::Folders { folders, entity } => show_detail(conn, series_key, folders, entity),
        SeriesScope::Movie { libraries } => movie_detail(conn, series_key, libraries),
    }
}

fn show_detail(
    conn: &Connection,
    series_key: &str,
    folders: Vec<(i64, String)>,
    bound_entity: Option<i64>,
) -> Result<Option<SeriesDetail>, String> {
    if folders.is_empty() {
        return Ok(None);
    }

    // Two folders of one show land in one library far more often than in two,
    // so the library read is done once per library rather than once per folder.
    let mut by_library: BTreeMap<i64, Vec<String>> = BTreeMap::new();
    for (library_id, folder) in &folders {
        by_library
            .entry(*library_id)
            .or_default()
            .push(folder.clone());
    }

    let mut episodes = Vec::new();
    let mut entity = bound_entity;
    let mut fallback_title = String::new();
    // One read per folder, not per episode.
    let mut binding_cache: BTreeMap<(i64, String), Vec<crate::series_bindings::SeriesBinding>> =
        BTreeMap::new();
    for (library_id, folders) in &by_library {
        let Some(library) = library(conn, *library_id)? else {
            continue;
        };
        if fallback_title.is_empty() {
            fallback_title = folder_title(&folders[0], &library.name);
        }
        let keys = effective_item_keys_for_library(conn, *library_id)?;
        for item in library_items(conn, *library_id)? {
            let folder = show_folder_relpath(&item.path, &library.root);
            if !folders.contains(&folder) {
                continue;
            }
            if entity.is_none() {
                entity = item.tmdb_show;
            }
            // ADR-0046 item 3(a): the reader sees the **folder's** numbering.
            // A file bound to a second entity carries that entity's canonical
            // season — Will & Grace's revival is season 1 on TMDB 74321 and
            // season 9 on disk — and grouping on the canonical number would
            // put two runs of "season 1" in one unit. Nothing errors; it is
            // simply wrong on screen.
            //
            // The translation is driven by the stored range, so a folder with
            // one binding takes `folder_season_for`'s unbounded path and its
            // response does not move (Rule 2.3).
            let item_entity = item.tmdb_show;
            let mut ep = episode_from(&library, item, &keys)?;
            if let (Some(entity_id), Some(canonical_season)) = (item_entity, ep.season) {
                let bindings = bindings_for(conn, *library_id, &folder, &mut binding_cache)?;
                if let Some(b) = bindings
                    .iter()
                    .find(|b| b.tmdb_show_id == entity_id && !b.is_primary)
                    && let Some(folder_season) = b.folder_season_for(canonical_season)
                {
                    ep.season = Some(folder_season);
                }
            }
            episodes.push(ep);
        }
    }

    if episodes.is_empty() {
        return Ok(None);
    }

    let identity = match (bound_entity, entity) {
        (Some(_), _) => UnitIdentity::Bound,
        (None, Some(_)) => UnitIdentity::EntityOnly,
        (None, None) => UnitIdentity::Unidentified,
    };
    let meta = entity.map(|id| show_meta(conn, id)).transpose()?.flatten();
    let item_count = episodes.len() as i64;
    let (seasons, unnumbered) = group_by_season(episodes);
    Ok(Some(SeriesDetail {
        series_key: series_key.to_string(),
        kind: UnitKind::Series,
        identity,
        title: meta.as_ref().map_or(fallback_title, |m| m.title.clone()),
        year: meta.as_ref().and_then(|m| m.year),
        plot: meta.as_ref().and_then(|m| m.plot.clone()),
        poster_key: entity.map(|id| format!("{SHOW_KEY_PREFIX}{id}")),
        item_count,
        seasons,
        unnumbered,
    }))
}

/// A movie and its files. Every file whose effective item key is this key is a
/// version of the same film (ADR-0025 §2), and none of them carries canonical
/// season or episode numbers, so they all come back unnumbered. Which one to
/// play is the version affordance ADR-0039 leaves to Block 3; listing them is
/// what stops the merged unit from hiding a file.
fn movie_detail(
    conn: &Connection,
    series_key: &str,
    libraries: Vec<i64>,
) -> Result<Option<SeriesDetail>, String> {
    let mut files = Vec::new();
    let mut fallback_title = String::new();
    for library_id in libraries {
        let Some(library) = library(conn, library_id)? else {
            continue;
        };
        let keys = effective_item_keys_for_library(conn, library_id)?;
        for item in library_items(conn, library_id)? {
            if keys.get(&item.id).map(String::as_str) != Some(series_key) {
                continue;
            }
            if fallback_title.is_empty() {
                fallback_title = item.title.clone();
            }
            files.push(episode_from(&library, item, &keys)?);
        }
    }

    if files.is_empty() {
        return Ok(None);
    }

    let meta = series_key
        .strip_prefix(MOVIE_KEY_PREFIX)
        .map(|id| movie_meta(conn, id))
        .transpose()?
        .flatten();
    files.sort_by(|a, b| a.path.cmp(&b.path));
    Ok(Some(SeriesDetail {
        series_key: series_key.to_string(),
        kind: UnitKind::Movie,
        identity: if meta.is_some() {
            UnitIdentity::Bound
        } else {
            UnitIdentity::Unidentified
        },
        title: meta.as_ref().map_or(fallback_title, |m| m.title.clone()),
        year: meta.as_ref().and_then(|m| m.year),
        plot: meta.as_ref().and_then(|m| m.plot.clone()),
        poster_key: meta.as_ref().map(|_| series_key.to_string()),
        item_count: files.len() as i64,
        seasons: Vec::new(),
        unnumbered: files,
    }))
}

/// Bindings for one folder, read once and cached for the request.
fn bindings_for<'a>(
    conn: &Connection,
    library_id: i64,
    folder: &str,
    cache: &'a mut BTreeMap<(i64, String), Vec<crate::series_bindings::SeriesBinding>>,
) -> Result<&'a Vec<crate::series_bindings::SeriesBinding>, String> {
    let key = (library_id, folder.to_string());
    if !cache.contains_key(&key) {
        let rows = crate::series_bindings::for_folder(conn, library_id, folder)?;
        cache.insert(key.clone(), rows);
    }
    Ok(cache.get(&key).expect("just inserted"))
}

fn episode_from(
    library: &Library,
    item: ItemRow,
    keys: &HashMap<i64, String>,
) -> Result<SeriesEpisode, String> {
    let item_key = keys
        .get(&item.id)
        .cloned()
        .ok_or_else(|| format!("no item key for media item {}", item.id))?;
    Ok(SeriesEpisode {
        path: resolve_media_path(&library.root, &item.path)
            .to_string_lossy()
            .into_owned(),
        item_id: item.id,
        item_key,
        title: item.canonical_title.unwrap_or(item.title),
        season: item.canonical_season,
        episode: item.canonical_episode,
        file_season: item.file_season,
        file_episode: item.file_episode,
        air_date: item.air_date,
        probe_status: item.probe_status,
        metadata_status: item.metadata_status,
    })
}

/// Canonical season and episode order (ADR-0035 item 8). Episodes without
/// canonical numbers cannot be ordered against those that have them, so they
/// come back separately rather than being folded in on filename numbering.
fn group_by_season(episodes: Vec<SeriesEpisode>) -> (Vec<SeriesSeason>, Vec<SeriesEpisode>) {
    let mut seasons: BTreeMap<i32, Vec<SeriesEpisode>> = BTreeMap::new();
    let mut unnumbered = Vec::new();
    for episode in episodes {
        match (episode.season, episode.episode) {
            (Some(season), Some(_)) => seasons.entry(season).or_default().push(episode),
            _ => unnumbered.push(episode),
        }
    }
    for episodes in seasons.values_mut() {
        // Path is the tiebreak, not the order: two files claiming one canonical
        // episode number (a duplicate rip) must still come out in a fixed order.
        episodes.sort_by(|a, b| a.episode.cmp(&b.episode).then_with(|| a.path.cmp(&b.path)));
    }
    unnumbered.sort_by(|a, b| a.path.cmp(&b.path));
    let seasons = seasons
        .into_iter()
        .map(|(season, episodes)| SeriesSeason { season, episodes })
        .collect();
    (seasons, unnumbered)
}

/// What a series key covers, before any media row is read.
enum SeriesScope {
    /// A show: `(library_id, show folder relpath)` pairs, plus the entity the
    /// key names when it names one.
    Folders {
        folders: Vec<(i64, String)>,
        entity: Option<i64>,
    },
    /// A movie, whose files are found by item key rather than by folder.
    Movie { libraries: Vec<i64> },
}

fn resolve_series_key(conn: &Connection, series_key: &str) -> Result<SeriesScope, String> {
    if let Some(id) = series_key.strip_prefix(SHOW_KEY_PREFIX) {
        let show_id: i64 = id
            .parse()
            .map_err(|_| format!("series key has a non-numeric show id: {series_key}"))?;
        let mut stmt = conn
            .prepare("SELECT library_id, relpath FROM series WHERE tmdb_show_id = ?1")
            .map_err(|e| format!("prepare series folders: {e}"))?;
        let rows = stmt
            .query_map(params![show_id], |r| {
                Ok((r.get::<_, i64>(0)?, r.get::<_, String>(1)?))
            })
            .map_err(|e| format!("query series folders: {e}"))?;
        let mut folders = Vec::new();
        for row in rows {
            folders.push(row.map_err(|e| format!("series folder row: {e}"))?);
        }
        return Ok(SeriesScope::Folders {
            folders,
            entity: Some(show_id),
        });
    }
    if let Some(rest) = series_key.strip_prefix(FOLDER_KEY_PREFIX) {
        let (library_id, relpath) = split_library_id(rest, series_key)?;
        return Ok(SeriesScope::Folders {
            folders: vec![(library_id, relpath.to_string())],
            entity: None,
        });
    }
    if series_key.starts_with(MOVIE_KEY_PREFIX) {
        let mut stmt = conn
            .prepare(
                "SELECT DISTINCT m.library_id
                 FROM media_item_links l
                 JOIN media_items m ON m.id = l.media_item_id
                 WHERE l.item_key = ?1",
            )
            .map_err(|e| format!("prepare movie libraries: {e}"))?;
        let rows = stmt
            .query_map(params![series_key], |r| r.get::<_, i64>(0))
            .map_err(|e| format!("query movie libraries: {e}"))?;
        let mut libraries = Vec::new();
        for row in rows {
            libraries.push(row.map_err(|e| format!("movie library row: {e}"))?);
        }
        return Ok(SeriesScope::Movie { libraries });
    }
    match parse_path_key(series_key) {
        Ok((library_id, _)) => {
            return Ok(SeriesScope::Movie {
                libraries: vec![library_id],
            });
        }
        Err(PathKeyError::MissingLibraryId) => {
            return Err(format!("series key has no library id: {series_key}"));
        }
        Err(PathKeyError::NonNumericLibraryId) => {
            return Err(format!(
                "series key has a non-numeric library id: {series_key}"
            ));
        }
        // Not a path key: fall through to the same final refusal it always
        // got, so this branch changes no accepted key.
        Err(PathKeyError::NotAPathKey) => {}
    }
    Err(format!("not a series key: {series_key}"))
}

/// The relpath may itself contain ':', the library id may not, so the first
/// separator is the only one that divides them.
fn split_library_id<'a>(rest: &'a str, series_key: &str) -> Result<(i64, &'a str), String> {
    let (library_id, relpath) = rest
        .split_once(':')
        .ok_or_else(|| format!("series key has no library id: {series_key}"))?;
    let library_id: i64 = library_id
        .parse()
        .map_err(|_| format!("series key has a non-numeric library id: {series_key}"))?;
    Ok((library_id, relpath))
}

fn library(conn: &Connection, library_id: i64) -> Result<Option<Library>, String> {
    conn.query_row(
        "SELECT id, name, path, kind FROM libraries WHERE id = ?1",
        params![library_id],
        |r| {
            Ok(Library {
                id: r.get(0)?,
                name: r.get(1)?,
                root: r.get(2)?,
                kind: r.get(3)?,
            })
        },
    )
    .optional()
    .map_err(|e| format!("browse library {library_id}: {e}"))
}

fn series_bindings(conn: &Connection, library_id: i64) -> Result<HashMap<String, i64>, String> {
    let mut stmt = conn
        .prepare("SELECT relpath, tmdb_show_id FROM series WHERE library_id = ?1")
        .map_err(|e| format!("prepare series bindings: {e}"))?;
    let rows = stmt
        .query_map(params![library_id], |r| {
            Ok((r.get::<_, String>(0)?, r.get::<_, i64>(1)?))
        })
        .map_err(|e| format!("query series bindings: {e}"))?;
    let mut out = HashMap::new();
    for row in rows {
        let (relpath, show_id) = row.map_err(|e| format!("series binding row: {e}"))?;
        out.insert(relpath, show_id);
    }
    Ok(out)
}

/// Media rows of one library, each carrying the canonical episode it is linked
/// to when it has one.
///
/// The join goes through `substr` rather than a concatenated comparison so the
/// canonical primary key is usable; the reverse form scans the whole canonical
/// table once per item. A file linked to more than one episode (ADR-0025 §2)
/// yields more than one row, and the first in `effective_item_key`'s ordering
/// wins so the listing and the item key never disagree about which episode a
/// double-length file is.
fn library_items(conn: &Connection, library_id: i64) -> Result<Vec<ItemRow>, String> {
    let mut stmt = conn
        .prepare(
            "SELECT m.id, m.path, m.title, m.season, m.episode,
                    m.probe_status, m.metadata_status,
                    c.season, c.episode, c.title, c.air_date, c.tmdb_show
             FROM media_items m
             LEFT JOIN media_item_links l
                    ON l.media_item_id = m.id AND l.item_key LIKE 'tmdb:episode:%'
             LEFT JOIN metadata_canonical c
                    ON c.provider = 'tmdb' AND c.entity_kind = 'episode'
                   AND c.provider_id = substr(l.item_key, ?2)
             WHERE m.library_id = ?1
             ORDER BY m.id, l.manually_matched DESC, l.item_key",
        )
        .map_err(|e| format!("prepare library items: {e}"))?;
    let prefix_len = EPISODE_KEY_PREFIX.len() as i64 + 1;
    let rows = stmt
        .query_map(params![library_id, prefix_len], |r| {
            Ok(ItemRow {
                id: r.get(0)?,
                path: r.get(1)?,
                title: r.get(2)?,
                file_season: r.get(3)?,
                file_episode: r.get(4)?,
                probe_status: r.get(5)?,
                metadata_status: r.get(6)?,
                canonical_season: r.get(7)?,
                canonical_episode: r.get(8)?,
                canonical_title: r.get(9)?,
                air_date: r.get(10)?,
                tmdb_show: r.get(11)?,
            })
        })
        .map_err(|e| format!("query library items: {e}"))?;
    let mut out: Vec<ItemRow> = Vec::new();
    for row in rows {
        let row = row.map_err(|e| format!("library item row: {e}"))?;
        if out.last().is_some_and(|prev| prev.id == row.id) {
            continue;
        }
        out.push(row);
    }
    Ok(out)
}

fn show_canonical(conn: &Connection) -> Result<HashMap<i64, ShowMeta>, String> {
    let mut stmt = conn
        .prepare(
            "SELECT provider_id, title, year, plot, artwork_json FROM metadata_canonical
             WHERE provider = 'tmdb' AND entity_kind = 'tv'",
        )
        .map_err(|e| format!("prepare show canonical: {e}"))?;
    let rows = stmt
        .query_map([], |r| {
            Ok((
                r.get::<_, String>(0)?,
                ShowMeta {
                    title: r.get(1)?,
                    year: r.get(2)?,
                    plot: r.get(3)?,
                    has_poster: has_poster_ref(r.get::<_, Option<String>>(4)?.as_deref()),
                },
            ))
        })
        .map_err(|e| format!("query show canonical: {e}"))?;
    let mut out = HashMap::new();
    for row in rows {
        let (provider_id, meta) = row.map_err(|e| format!("show canonical row: {e}"))?;
        // A non-numeric tv provider id cannot be reached from a series row, so
        // it can never be a unit's entity; skipping it keeps the map keyed the
        // way `series.tmdb_show_id` is.
        if let Ok(id) = provider_id.parse::<i64>() {
            out.insert(id, meta);
        }
    }
    Ok(out)
}

fn show_meta(conn: &Connection, show_id: i64) -> Result<Option<ShowMeta>, String> {
    conn.query_row(
        "SELECT title, year, plot, artwork_json FROM metadata_canonical
         WHERE provider = 'tmdb' AND entity_kind = 'tv' AND provider_id = ?1",
        params![show_id.to_string()],
        |r| {
            Ok(ShowMeta {
                title: r.get(0)?,
                year: r.get(1)?,
                plot: r.get(2)?,
                has_poster: has_poster_ref(r.get::<_, Option<String>>(3)?.as_deref()),
            })
        },
    )
    .optional()
    .map_err(|e| format!("show canonical {show_id}: {e}"))
}

fn movie_meta(conn: &Connection, provider_id: &str) -> Result<Option<ShowMeta>, String> {
    conn.query_row(
        "SELECT title, year, plot, artwork_json FROM metadata_canonical
         WHERE provider = 'tmdb' AND entity_kind = 'movie' AND provider_id = ?1",
        params![provider_id],
        |r| {
            Ok(ShowMeta {
                title: r.get(0)?,
                year: r.get(1)?,
                plot: r.get(2)?,
                has_poster: has_poster_ref(r.get::<_, Option<String>>(3)?.as_deref()),
            })
        },
    )
    .optional()
    .map_err(|e| format!("movie canonical {provider_id}: {e}"))
}

fn movie_canonical(conn: &Connection) -> Result<HashMap<String, ShowMeta>, String> {
    let mut stmt = conn
        .prepare(
            "SELECT provider_id, title, year, plot, artwork_json FROM metadata_canonical
             WHERE provider = 'tmdb' AND entity_kind = 'movie'",
        )
        .map_err(|e| format!("prepare movie canonical: {e}"))?;
    let rows = stmt
        .query_map([], |r| {
            Ok((
                r.get::<_, String>(0)?,
                ShowMeta {
                    title: r.get(1)?,
                    year: r.get(2)?,
                    plot: r.get(3)?,
                    has_poster: has_poster_ref(r.get::<_, Option<String>>(4)?.as_deref()),
                },
            ))
        })
        .map_err(|e| format!("query movie canonical: {e}"))?;
    let mut out = HashMap::new();
    for row in rows {
        let (provider_id, meta) = row.map_err(|e| format!("movie canonical row: {e}"))?;
        out.insert(provider_id, meta);
    }
    Ok(out)
}

/// Show entities no `series` row binds anywhere. They produce no unit, because
/// a unit is a folder of files, so the listing reports the number rather than
/// letting them vanish (ADR-0039 item 1: the entity outlives any folder).
fn show_entities_without_binding(conn: &Connection) -> Result<i64, String> {
    conn.query_row(
        "SELECT COUNT(*) FROM metadata_canonical c
         WHERE c.provider = 'tmdb' AND c.entity_kind = 'tv'
           AND NOT EXISTS (
                SELECT 1 FROM series s WHERE CAST(s.tmdb_show_id AS TEXT) = c.provider_id
           )",
        [],
        |r| r.get(0),
    )
    .map_err(|e| format!("show entities without binding: {e}"))
}

/// Whether a canonical `artwork_json` holds a poster ref. Parsed once per
/// canonical row while the map is built, never once per unit.
fn has_poster_ref(artwork_json: Option<&str>) -> bool {
    let Some(raw) = artwork_json else {
        return false;
    };
    serde_json::from_str::<Vec<crate::model::ArtworkRef>>(raw)
        .map(|refs| refs.iter().any(|a| a.kind == ArtworkKind::Poster))
        .unwrap_or(false)
}

fn folder_title(folder: &str, library_name: &str) -> String {
    match folder.rsplit('/').next() {
        // ADR-0033 Q2: an empty folder key means the library root is itself the
        // show folder, so the library is what the unit is called.
        Some("") | None => library_name.to_string(),
        Some(name) => name.to_string(),
    }
}

fn bump(counts: &mut UnitCounts, identity: UnitIdentity) {
    match identity {
        UnitIdentity::Bound => counts.bound += 1,
        UnitIdentity::EntityOnly => counts.entity_only += 1,
        UnitIdentity::Unidentified => counts.unidentified += 1,
    }
}

fn sort_units(units: &mut [BrowseUnit]) {
    units.sort_by(|a, b| {
        a.title
            .to_lowercase()
            .cmp(&b.title.to_lowercase())
            .then_with(|| a.series_key.cmp(&b.series_key))
    });
}

#[cfg(test)]
mod tests {
    use super::*;
    use nightjar_db::migrate;

    /// A shows library carrying each of the three identity states plus the two
    /// numbering irregularities the dogfood library is checked against: a show
    /// with specials, and a show whose filenames use absolute numbering.
    fn shows_library() -> Connection {
        let c = Connection::open_in_memory().unwrap();
        migrate(&c).unwrap();
        c.execute_batch(
            "INSERT INTO libraries (id, name, path, kind)
                  VALUES (2, 'TV', '/TV', 'shows');

             -- Bound: a folder with a series row.
             INSERT INTO series (library_id, relpath, tmdb_show_id)
                  VALUES (2, 'Futurama', 615), (2, 'Bleach', 30984);

             INSERT INTO media_items (id, library_id, path, mtime_ms, size_bytes, title, kind,
                                      season, episode, metadata_status)
             VALUES
               -- Futurama: filenames in one order, canonical numbers in another,
               -- plus a special and one episode that never matched.
               (1, 2, 'Futurama/Season 1/S01E04.mkv', 1, 1, 'S01E04', 'episode', 1, 4, 'ready'),
               (2, 2, 'Futurama/Season 1/S01E02.mkv', 1, 1, 'S01E02', 'episode', 1, 2, 'ready'),
               (3, 2, 'Futurama/Season 2/S02E01.mkv', 1, 1, 'S02E01', 'episode', 2, 1, 'ready'),
               (4, 2, 'Futurama/Specials/S00E01.mkv', 1, 1, 'S00E01', 'episode', 0, 1, 'ready'),
               (5, 2, 'Futurama/Season 1/S01E99.mkv', 1, 1, 'S01E99', 'episode', 1, 99,
                'unmatched'),
               -- Bleach: absolute numbering on disk, canonical season 2 in TMDB.
               (6, 2, 'Bleach/Season 1/S01E021.mkv', 1, 1, 'S01E021', 'episode', 1, 21, 'ready'),
               (7, 2, 'Bleach/Season 1/S01E020.mkv', 1, 1, 'S01E020', 'episode', 1, 20, 'ready'),
               -- Entity only: no series row, but the episode names its show.
               (8, 2, 'Known Show/Season 1/e1.mkv', 1, 1, 'e1', 'episode', 1, 1, 'ready'),
               -- Unidentified: below the floor in both edges (ADR-0025 §4).
               (9, 2, 'Mystery Folder/e1.mkv', 1, 1, 'e1', 'episode', NULL, NULL, 'unmatched');

             INSERT INTO metadata_canonical
                  (provider, entity_kind, provider_id, title, year, ids_json, projected_at,
                   season, episode, tmdb_show, artwork_json)
             VALUES
               ('tmdb', 'tv', '615', 'Futurama', 1999, '{}', 'now', NULL, NULL, NULL,
                '[{\"kind\":\"poster\",\"path\":\"/f.jpg\"}]'),
               ('tmdb', 'tv', '30984', 'Bleach', 2004, '{}', 'now', NULL, NULL, NULL, NULL),
               -- Known Show has canonical metadata and no artwork of any kind.
               ('tmdb', 'tv', '999', 'Known Show', 2011, '{}', 'now', NULL, NULL, NULL, NULL),
               ('tmdb', 'tv', '4242', 'Show With No Files', 1988, '{}', 'now', NULL, NULL, NULL,
                NULL),
               ('tmdb', 'episode', '101', 'Space Pilot 3000', 1999, '{}', 'now', 1, 1, 615, NULL),
               ('tmdb', 'episode', '102', 'The Series Has Landed', 1999, '{}', 'now', 1, 2, 615, NULL),
               ('tmdb', 'episode', '201', 'I Second That Emotion', 2000, '{}', 'now', 2, 1, 615, NULL),
               ('tmdb', 'episode', '001', 'Christmas Special', 1999, '{}', 'now', 0, 1, 615, NULL),
               ('tmdb', 'episode', '2004', 'Bleach 20', 2005, '{}', 'now', 2, 4, 30984, NULL),
               ('tmdb', 'episode', '2005', 'Bleach 21', 2005, '{}', 'now', 2, 5, 30984, NULL),
               ('tmdb', 'episode', '9001', 'Known Ep', 2011, '{}', 'now', 1, 1, 999, NULL);

             INSERT INTO media_item_links (media_item_id, item_key)
             VALUES (1, 'tmdb:episode:101'),
                    (2, 'tmdb:episode:102'),
                    (3, 'tmdb:episode:201'),
                    (4, 'tmdb:episode:001'),
                    (6, 'tmdb:episode:2005'),
                    (7, 'tmdb:episode:2004'),
                    (8, 'tmdb:episode:9001');",
        )
        .unwrap();
        c
    }

    /// A client keys its list on `seriesKey`, so a duplicate is a rendering
    /// bug, not a cosmetic one. Every listing test asserts this.
    fn assert_unit_keys_unique(listed: &LibraryUnits) {
        let mut keys: Vec<&str> = listed.units.iter().map(|u| u.series_key.as_str()).collect();
        let total = keys.len();
        keys.sort_unstable();
        keys.dedup();
        assert_eq!(keys.len(), total, "duplicate series_key in {keys:?}");
        assert_eq!(listed.counts.units, total as i64);
    }

    fn unit<'a>(units: &'a [BrowseUnit], title: &str) -> &'a BrowseUnit {
        units
            .iter()
            .find(|u| u.title == title)
            .unwrap_or_else(|| panic!("no unit titled {title}"))
    }

    /// One unit per show folder, not one per episode.
    #[test]
    fn shows_collapse_to_folders() {
        let c = shows_library();
        let listed = list_library_units(&c, 2).unwrap();
        assert_unit_keys_unique(&listed);
        assert_eq!(listed.unit_kind, UnitKind::Series);
        assert_eq!(listed.units.len(), 4, "four folders, nine episode files");
        assert_eq!(unit(&listed.units, "Futurama").item_count, 5);
        assert_eq!(unit(&listed.units, "Bleach").item_count, 2);
        // Sorted by title, so the listing is stable across calls.
        let titles: Vec<&str> = listed.units.iter().map(|u| u.title.as_str()).collect();
        assert_eq!(
            titles,
            vec!["Bleach", "Futurama", "Known Show", "Mystery Folder"]
        );
    }

    /// The grouping must not drop a row: every media item lands in exactly one
    /// unit, including the ones with no identity at all.
    #[test]
    fn every_item_is_covered_by_a_unit() {
        let c = shows_library();
        let listed = list_library_units(&c, 2).unwrap();
        let items: i64 = c
            .query_row(
                "SELECT COUNT(*) FROM media_items WHERE library_id = 2",
                [],
                |r| r.get(0),
            )
            .unwrap();
        assert_eq!(listed.counts.items, items);
        assert_eq!(
            listed.units.iter().map(|u| u.item_count).sum::<i64>(),
            items,
            "unit item counts sum to the library"
        );
    }

    /// Bound unit count tracks the `series` table; the other two states are
    /// counted rather than hidden.
    #[test]
    fn identity_states_are_counted() {
        let c = shows_library();
        let listed = list_library_units(&c, 2).unwrap();
        // Distinct shows, not `series` rows: two folders bound to one show are
        // one unit (ADR-0039 item 8). On the dogfood library that is the
        // difference between 722 rows and 720 units.
        let distinct_shows: i64 = c
            .query_row(
                "SELECT COUNT(DISTINCT tmdb_show_id) FROM series WHERE library_id = 2",
                [],
                |r| r.get(0),
            )
            .unwrap();
        assert_eq!(listed.counts.bound, distinct_shows);
        assert_eq!(listed.counts.entity_only, 1);
        assert_eq!(listed.counts.unidentified, 1);
        assert_eq!(
            listed.counts.units,
            listed.counts.bound + listed.counts.entity_only + listed.counts.unidentified
        );
        // 999 and 4242 have canonical rows and no `series` row anywhere.
        assert_eq!(listed.counts.show_entities_without_binding, Some(2));
    }

    /// A folder with no `series` row whose episodes still name a show appears
    /// with the show's title and the marker, rather than silently as a folder.
    #[test]
    fn entity_only_folder_keeps_its_show_metadata() {
        let c = shows_library();
        let listed = list_library_units(&c, 2).unwrap();
        let known = unit(&listed.units, "Known Show");
        assert_eq!(known.identity, UnitIdentity::EntityOnly);
        assert_eq!(known.series_key, "folder:2:Known Show");
        assert_eq!(known.year, Some(2011));
        // The show has no artwork ref, so no URL is offered. A URL here would
        // paint a broken tile rather than fetch anything.
        assert_eq!(known.poster_key, None);
        // A show that does have one still gets it, so this is the predicate
        // rather than the entityOnly state suppressing art.
        assert_eq!(
            unit(&listed.units, "Futurama").poster_key.as_deref(),
            Some("tmdb:show:615")
        );
    }

    #[test]
    fn unidentified_folder_falls_back_to_its_name() {
        let c = shows_library();
        let listed = list_library_units(&c, 2).unwrap();
        let mystery = unit(&listed.units, "Mystery Folder");
        assert_eq!(mystery.identity, UnitIdentity::Unidentified);
        assert_eq!(mystery.series_key, "folder:2:Mystery Folder");
        assert_eq!(mystery.poster_key, None);
    }

    /// ADR-0035 item 8: order is canonical, and the filenames here disagree
    /// with it in both directions so a filename sort cannot pass this.
    #[test]
    fn episodes_order_by_canonical_numbers() {
        let c = shows_library();
        let series = get_series(&c, "tmdb:show:615").unwrap().unwrap();
        assert_eq!(series.title, "Futurama");
        assert_eq!(series.identity, UnitIdentity::Bound);

        let seasons: Vec<i32> = series.seasons.iter().map(|s| s.season).collect();
        assert_eq!(seasons, vec![0, 1, 2], "specials sort as season 0");

        let s1 = &series.seasons[1];
        let titles: Vec<&str> = s1.episodes.iter().map(|e| e.title.as_str()).collect();
        assert_eq!(
            titles,
            vec!["Space Pilot 3000", "The Series Has Landed"],
            "canonical E01 before E02, though the files are S01E04 then S01E02"
        );
        assert_eq!(s1.episodes[0].file_episode, Some(4));
        assert_eq!(s1.episodes[0].episode, Some(1));
    }

    /// Absolute numbering on disk against canonical season/episode: the
    /// canonical season wins and the file numbers stay visible beside it.
    #[test]
    fn absolute_numbering_files_land_in_their_canonical_season() {
        let c = shows_library();
        let series = get_series(&c, "tmdb:show:30984").unwrap().unwrap();
        assert_eq!(series.seasons.len(), 1);
        let season = &series.seasons[0];
        assert_eq!(season.season, 2, "canonical season, not the filename's 1");
        let episodes: Vec<(Option<i32>, Option<i32>)> = season
            .episodes
            .iter()
            .map(|e| (e.episode, e.file_episode))
            .collect();
        assert_eq!(episodes, vec![(Some(4), Some(20)), (Some(5), Some(21))]);
    }

    /// ADR-0035 item 8: an unmatched episode groups under its series but has no
    /// canonical numbering to order by, so it is listed rather than guessed
    /// into a season from its filename.
    #[test]
    fn below_floor_episodes_are_reachable_not_swallowed() {
        let c = shows_library();
        let series = get_series(&c, "tmdb:show:615").unwrap().unwrap();
        assert_eq!(series.item_count, 5);
        assert_eq!(series.unnumbered.len(), 1);
        let stray = &series.unnumbered[0];
        assert_eq!(stray.item_id, 5);
        assert!(!stray.canonical_numbering());
        assert_eq!(stray.season, None);
        assert_eq!(
            stray.file_episode,
            Some(99),
            "the filename numbering is reported, just not ordered on"
        );
        let numbered: usize = series.seasons.iter().map(|s| s.episodes.len()).sum();
        assert_eq!(numbered + series.unnumbered.len(), 5);
    }

    #[test]
    fn folder_key_resolves_an_unbound_series() {
        let c = shows_library();
        let series = get_series(&c, "folder:2:Mystery Folder").unwrap().unwrap();
        assert_eq!(series.identity, UnitIdentity::Unidentified);
        assert_eq!(series.title, "Mystery Folder");
        assert_eq!(series.seasons.len(), 0);
        assert_eq!(series.unnumbered.len(), 1);
        assert_eq!(
            series.unnumbered[0].item_key,
            "path:2:Mystery Folder/e1.mkv"
        );
    }

    #[test]
    fn unknown_and_malformed_series_keys() {
        let c = shows_library();
        assert!(get_series(&c, "tmdb:show:1").unwrap().is_none());
        assert!(get_series(&c, "folder:2:Nothing Here").unwrap().is_none());
        assert!(get_series(&c, "tmdb:movie:550").unwrap().is_none());
        assert!(get_series(&c, "folder:notanumber:x").is_err());
        assert!(get_series(&c, "nonsense").is_err());
    }

    /// ADR-0039 item 8: several folders bound to one show are one unit, and a
    /// unit key is therefore unique within a listing. The shape is the dogfood
    /// library's: a show folder plus two `Sample/` subdirectories that each
    /// took their own `series` row, which is what produced duplicate keys.
    #[test]
    fn folders_bound_to_one_show_collapse_to_one_unit() {
        let c = shows_library();
        c.execute_batch(
            "INSERT INTO series (library_id, relpath, tmdb_show_id)
             VALUES (2, 'Futurama/Season 1/Release.Name/Sample', 615),
                    (2, 'Futurama/Season 2/Release.Name/Sample', 615);
             INSERT INTO media_items (id, library_id, path, mtime_ms, size_bytes, title, kind)
             VALUES (20, 2, 'Futurama/Season 1/Release.Name/Sample/s.mkv', 1, 1, 's1', 'episode'),
                    (21, 2, 'Futurama/Season 2/Release.Name/Sample/s.mkv', 1, 1, 's2', 'episode');",
        )
        .unwrap();

        let listed = list_library_units(&c, 2).unwrap();
        assert_unit_keys_unique(&listed);
        // Three `series` rows for show 615, one unit.
        assert_eq!(listed.counts.bound, 2, "Futurama and Bleach");
        let futurama = unit(&listed.units, "Futurama");
        assert_eq!(futurama.series_key, "tmdb:show:615");
        assert_eq!(futurama.item_count, 7, "5 episodes plus the 2 samples");
        // Still nothing dropped.
        assert_eq!(
            listed.units.iter().map(|u| u.item_count).sum::<i64>(),
            listed.counts.items
        );
        // The detail is the union over all three folders (ADR-0039 item 8).
        let series = get_series(&c, "tmdb:show:615").unwrap().unwrap();
        assert_eq!(series.item_count, 7);
    }

    /// ADR-0025 §2: two rips of one film share an item_key, so they share a
    /// series key (ADR-0039 item 2) and are one unit. `itemId` is withheld
    /// rather than pointing at one of them, and the files stay reachable
    /// through the detail route.
    #[test]
    fn two_files_of_one_movie_collapse_to_one_unit() {
        let c = Connection::open_in_memory().unwrap();
        migrate(&c).unwrap();
        c.execute_batch(
            "INSERT INTO libraries (id, name, path, kind)
                  VALUES (1, 'Movies', '/Movies', 'movies');
             INSERT INTO media_items (id, library_id, path, mtime_ms, size_bytes, title, kind, year)
             VALUES (1, 1, 'Fight Club (1999)/1080p.mkv', 1, 1, 'Fight Club', 'movie', 1999),
                    (2, 1, 'Fight Club (1999)/remux.mkv', 1, 1, 'Fight Club', 'movie', 1999),
                    (3, 1, 'Solo/only.mkv', 1, 1, 'Solo', 'movie', 2005);
             INSERT INTO metadata_canonical
                  (provider, entity_kind, provider_id, title, year, ids_json, projected_at)
             VALUES ('tmdb', 'movie', '550', 'Fight Club', 1999, '{}', 'now'),
                    ('tmdb', 'movie', '7', 'Solo', 2005, '{}', 'now');
             INSERT INTO media_item_links (media_item_id, item_key)
                  VALUES (1, 'tmdb:movie:550'), (2, 'tmdb:movie:550'), (3, 'tmdb:movie:7');",
        )
        .unwrap();

        let listed = list_library_units(&c, 1).unwrap();
        assert_unit_keys_unique(&listed);
        assert_eq!(listed.units.len(), 2, "three files, two films");
        assert_eq!(listed.counts.items, 3, "no file is dropped by the merge");

        let merged = unit(&listed.units, "Fight Club");
        assert_eq!(merged.item_count, 2);
        assert_eq!(
            merged.item_id, None,
            "two versions, so no single row to point at"
        );
        assert_eq!(unit(&listed.units, "Solo").item_id, Some(3));

        // Both versions are reachable rather than hidden behind the merge.
        let detail = get_series(&c, "tmdb:movie:550").unwrap().unwrap();
        assert_eq!(detail.kind, UnitKind::Movie);
        assert_eq!(detail.title, "Fight Club");
        assert_eq!(detail.seasons.len(), 0, "a film has no episode numbering");
        let ids: Vec<i64> = detail.unnumbered.iter().map(|e| e.item_id).collect();
        assert_eq!(ids, vec![1, 2]);
    }

    #[test]
    fn unmatched_movie_resolves_through_its_path_key() {
        let c = Connection::open_in_memory().unwrap();
        migrate(&c).unwrap();
        c.execute_batch(
            "INSERT INTO libraries (id, name, path, kind)
                  VALUES (1, 'Movies', '/Movies', 'movies');
             INSERT INTO media_items (id, library_id, path, mtime_ms, size_bytes, title, kind)
             VALUES (1, 1, 'Unknown Rip/u.mkv', 1, 1, 'Unknown Rip', 'movie');",
        )
        .unwrap();
        let detail = get_series(&c, "path:1:Unknown Rip/u.mkv").unwrap().unwrap();
        assert_eq!(detail.kind, UnitKind::Movie);
        assert_eq!(detail.identity, UnitIdentity::Unidentified);
        assert_eq!(detail.item_count, 1);
        assert_eq!(detail.unnumbered[0].item_id, 1);
    }

    #[test]
    fn movie_library_lists_one_unit_per_item() {
        let c = Connection::open_in_memory().unwrap();
        migrate(&c).unwrap();
        c.execute_batch(
            "INSERT INTO libraries (id, name, path, kind)
                  VALUES (1, 'Movies', '/Movies', 'movies');
             INSERT INTO media_items (id, library_id, path, mtime_ms, size_bytes, title, kind, year)
             VALUES (1, 1, 'Fight Club (1999)/f.mkv', 1, 1, 'Fight Club', 'movie', 1999),
                    (2, 1, 'Unknown Rip/u.mkv', 1, 1, 'Unknown Rip', 'movie', NULL);
             INSERT INTO metadata_canonical
                  (provider, entity_kind, provider_id, title, year, ids_json, projected_at)
             VALUES ('tmdb', 'movie', '550', 'Fight Club', 1999, '{}', 'now');
             INSERT INTO media_item_links (media_item_id, item_key)
                  VALUES (1, 'tmdb:movie:550');",
        )
        .unwrap();

        let listed = list_library_units(&c, 1).unwrap();
        assert_unit_keys_unique(&listed);
        assert_eq!(listed.unit_kind, UnitKind::Movie);
        assert_eq!(listed.units.len(), 2);
        assert_eq!(listed.counts.items, 2);
        // A shows-only count stays absent rather than reporting zero.
        assert_eq!(listed.counts.show_entities_without_binding, None);

        let matched = unit(&listed.units, "Fight Club");
        assert_eq!(matched.kind, UnitKind::Movie);
        assert_eq!(matched.identity, UnitIdentity::Bound);
        // A movie's series key is its own item_key (ADR-0039 item 2).
        assert_eq!(matched.series_key, "tmdb:movie:550");
        assert_eq!(matched.item_id, Some(1));
        assert_eq!(matched.item_count, 1);

        let unmatched = unit(&listed.units, "Unknown Rip");
        assert_eq!(unmatched.identity, UnitIdentity::Unidentified);
        assert_eq!(unmatched.series_key, "path:1:Unknown Rip/u.mkv");
        assert_eq!(unmatched.poster_key, None);
    }

    /// ADR-0033 Q2: an empty folder key means the library root is the show
    /// folder, which is one unit rather than a unit per loose file.
    #[test]
    fn library_root_as_show_folder() {
        let c = Connection::open_in_memory().unwrap();
        migrate(&c).unwrap();
        c.execute_batch(
            "INSERT INTO libraries (id, name, path, kind)
                  VALUES (3, 'One Show', '/OneShow', 'shows');
             INSERT INTO media_items (library_id, path, mtime_ms, size_bytes, title, kind)
             VALUES (3, 'Season 1/a.mkv', 1, 1, 'a', 'episode'),
                    (3, 'Season 1/b.mkv', 1, 1, 'b', 'episode');",
        )
        .unwrap();
        let listed = list_library_units(&c, 3).unwrap();
        assert_eq!(listed.units.len(), 1);
        assert_eq!(listed.units[0].series_key, "folder:3:");
        assert_eq!(listed.units[0].title, "One Show");
        assert_eq!(listed.units[0].item_count, 2);
    }

    /// Fixture: a folder holding S1-S2 bound to 4454, and S9-S10 whose
    /// canonical rows belong to 74321 numbering them S1-S2.
    fn will_and_grace(c: &Connection) {
        c.execute_batch(
            "INSERT INTO libraries (id, name, path, kind)
                  VALUES (1, 'Shows', '/Shows', 'shows');
             INSERT INTO series (library_id, relpath, tmdb_show_id)
                  VALUES (1, 'Will & Grace', 4454);
             INSERT INTO media_items
                  (id, library_id, path, mtime_ms, size_bytes, title, kind, season, episode)
             VALUES
                (1, 1, 'Will & Grace/Season 1/w.s01e01.mkv', 1, 1, 'W', 'episode', 1, 1),
                (2, 1, 'Will & Grace/Season 2/w.s02e01.mkv', 1, 1, 'W', 'episode', 2, 1),
                (3, 1, 'Will & Grace/Season 9/w.s09e01.mkv', 1, 1, 'W', 'episode', 9, 1),
                (4, 1, 'Will & Grace/Season 10/w.s10e01.mkv', 1, 1, 'W', 'episode', 10, 1);
             INSERT INTO metadata_canonical
                  (provider, entity_kind, provider_id, title, ids_json, tmdb_show,
                   season, episode, projected_at)
             VALUES
                ('tmdb', 'episode', '101', 'Pilot',      '{}', 4454,  1, 1, 'now'),
                ('tmdb', 'episode', '201', 'Guess Who',  '{}', 4454,  2, 1, 'now'),
                ('tmdb', 'episode', '901', 'Eleven Years Later', '{}', 74321, 1, 1, 'now'),
                ('tmdb', 'episode', '1001','The Curmudgeon',     '{}', 74321, 2, 1, 'now');
             INSERT INTO media_item_links (media_item_id, item_key) VALUES
                (1, 'tmdb:episode:101'), (2, 'tmdb:episode:201'),
                (3, 'tmdb:episode:901'), (4, 'tmdb:episode:1001');",
        )
        .unwrap();
    }

    /// **The defect the whole numbering design exists to prevent.** 74321
    /// numbers the revival S1-S2 and the folder numbers it S9-S10. Without the
    /// stored range the reader gets two season 1 buckets and two season 2
    /// buckets in one unit — nothing errors, it is simply wrong on screen.
    #[test]
    fn multi_entity_folder_reads_in_the_folders_numbering() {
        let c = Connection::open_in_memory().unwrap();
        migrate(&c).unwrap();
        will_and_grace(&c);
        c.execute_batch(
            "INSERT INTO series_entity_bindings
                (library_id, relpath, tmdb_show_id, is_primary,
                 folder_season_start, folder_season_end,
                 entity_season_start, entity_season_end)
             VALUES (1, 'Will & Grace', 74321, 0, 9, 10, 1, 2);",
        )
        .unwrap();

        let detail = get_series(&c, "tmdb:show:4454").unwrap().unwrap();
        let seasons: Vec<i32> = detail.seasons.iter().map(|s| s.season).collect();
        assert_eq!(
            seasons,
            vec![1, 2, 9, 10],
            "the folder's numbering, with no duplicate bucket"
        );
        let mut sorted = seasons.clone();
        sorted.dedup();
        assert_eq!(sorted.len(), seasons.len(), "no season appears twice");
        assert!(
            detail.seasons.iter().all(|s| s.episodes.len() == 1),
            "one episode per season bucket; nothing was merged"
        );
    }

    /// Without the binding row the same data interleaves — the state this
    /// slice exists to prevent, asserted so the fix is demonstrated rather
    /// than assumed.
    #[test]
    fn without_the_binding_row_the_seasons_collide() {
        let c = Connection::open_in_memory().unwrap();
        migrate(&c).unwrap();
        will_and_grace(&c);

        let detail = get_series(&c, "tmdb:show:4454").unwrap().unwrap();
        let seasons: Vec<i32> = detail.seasons.iter().map(|s| s.season).collect();
        assert_eq!(
            seasons,
            vec![1, 2],
            "two buckets, each holding two shows' episodes"
        );
        assert!(
            detail.seasons.iter().all(|s| s.episodes.len() == 2),
            "this is the interleave: 1998's S1 and 2017's S1 in one bucket"
        );
    }

    /// Rule 2.3: a folder that binds one entity must not move. Its backfilled
    /// primary is unbounded, so the translation returns the season unchanged.
    #[test]
    fn single_entity_folder_is_unchanged_by_the_translation() {
        let c = Connection::open_in_memory().unwrap();
        migrate(&c).unwrap();
        will_and_grace(&c);
        c.execute_batch(
            "DELETE FROM media_items WHERE id IN (3, 4);
             INSERT INTO series_entity_bindings
                (library_id, relpath, tmdb_show_id, is_primary,
                 folder_season_start, folder_season_end,
                 entity_season_start, entity_season_end)
             VALUES (1, 'Will & Grace', 4454, 1, NULL, NULL, NULL, NULL);",
        )
        .unwrap();

        let detail = get_series(&c, "tmdb:show:4454").unwrap().unwrap();
        assert_eq!(
            detail.seasons.iter().map(|s| s.season).collect::<Vec<_>>(),
            vec![1, 2]
        );
        assert_eq!(detail.item_count, 2);
    }

    /// One folder stays one browse unit however many entities it binds
    /// (ADR-0046 item 1). The corollary that does the work: several folders
    /// give several units, and that is the user's decision.
    #[test]
    fn a_multi_entity_folder_is_still_one_unit() {
        let c = Connection::open_in_memory().unwrap();
        migrate(&c).unwrap();
        will_and_grace(&c);
        c.execute_batch(
            "INSERT INTO series_entity_bindings
                (library_id, relpath, tmdb_show_id, is_primary,
                 folder_season_start, folder_season_end,
                 entity_season_start, entity_season_end)
             VALUES (1, 'Will & Grace', 74321, 0, 9, 10, 1, 2);",
        )
        .unwrap();

        let listed = list_library_units(&c, 1).unwrap();
        assert_unit_keys_unique(&listed);
        assert_eq!(listed.units.len(), 1, "one folder, one unit");
        assert_eq!(listed.units[0].series_key, "tmdb:show:4454");
        assert_eq!(listed.units[0].item_count, 4);
    }
}
