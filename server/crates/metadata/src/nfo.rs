//! Kodi / Emby-style NFO → [`CanonicalMetadata`].
//!
//! Hand-rolled tag extraction keeps us off an XML crate for the small tag set
//! we actually read (Rule 4.4). Not a general XML parser.

use crate::model::{
    ArtworkKind, ArtworkRef, CanonicalMetadata, CastMember, CollectionRef, MetadataKind,
    ProviderIds, Rating,
};

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum NfoError {
    Empty,
    UnknownRoot,
    MissingTitle,
    Malformed(&'static str),
}

impl std::fmt::Display for NfoError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Empty => write!(f, "nfo is empty"),
            Self::UnknownRoot => write!(f, "nfo has no movie/tvshow/episodedetails root"),
            Self::MissingTitle => write!(f, "nfo has no title"),
            Self::Malformed(msg) => write!(f, "malformed nfo: {msg}"),
        }
    }
}

impl std::error::Error for NfoError {}

/// Parse a Kodi-style NFO document into canonical metadata.
pub fn parse_nfo(xml: &str) -> Result<CanonicalMetadata, NfoError> {
    let trimmed = xml.trim();
    if trimmed.is_empty() {
        return Err(NfoError::Empty);
    }
    // Truncation / unclosed root: require a recognizable root open tag.
    let kind = detect_kind(trimmed)?;
    let title = first_text(trimmed, "title")
        .or_else(|| first_text(trimmed, "showtitle"))
        .ok_or(NfoError::MissingTitle)?;
    if title.trim().is_empty() {
        return Err(NfoError::MissingTitle);
    }

    // Unclosed critical tags → malformed (slice fixture contract).
    if has_unclosed(trimmed, "title") || has_unclosed(trimmed, "plot") {
        return Err(NfoError::Malformed("unclosed title or plot tag"));
    }

    let original_title = first_text(trimmed, "originaltitle");
    let year = first_text(trimmed, "year")
        .and_then(|y| y.trim().parse().ok())
        .or_else(|| {
            first_text(trimmed, "premiered")
                .or_else(|| first_text(trimmed, "aired"))
                .and_then(|d| d.get(0..4)?.parse().ok())
        });
    let plot = first_text(trimmed, "plot").or_else(|| first_text(trimmed, "outline"));
    let genres = all_text(trimmed, "genre");
    let runtime_minutes = first_text(trimmed, "runtime").and_then(|r| {
        let r = r.trim();
        r.parse().ok().or_else(|| {
            // "139 min" style
            r.split_whitespace().next()?.parse().ok()
        })
    });

    Ok(CanonicalMetadata {
        kind,
        title: title.trim().to_string(),
        original_title: original_title
            .map(|s| s.trim().to_string())
            .filter(|s| !s.is_empty()),
        year,
        air_date: first_text(trimmed, "aired")
            .or_else(|| first_text(trimmed, "premiered"))
            .map(|s| s.trim().to_string())
            .filter(|s| !s.is_empty())
            .filter(|_| kind == MetadataKind::Episode),
        plot: plot
            .map(|s| decode_basic_entities(s.trim()))
            .filter(|s| !s.is_empty()),
        genres: genres
            .into_iter()
            .map(|g| g.trim().to_string())
            .filter(|g| !g.is_empty())
            .collect(),
        runtime_minutes,
        cast: parse_actors(trimmed),
        ratings: parse_ratings(trimmed),
        ids: parse_ids(trimmed),
        artwork: parse_artwork(trimmed),
        collection: parse_collection(trimmed),
        season: first_text(trimmed, "season").and_then(|s| s.trim().parse().ok()),
        episode: first_text(trimmed, "episode").and_then(|s| s.trim().parse().ok()),
    })
}

fn detect_kind(xml: &str) -> Result<MetadataKind, NfoError> {
    let lower = xml.to_ascii_lowercase();
    // Prefer the first root-like open tag.
    for (tag, kind) in [
        ("<movie", MetadataKind::Movie),
        ("<episodedetails", MetadataKind::Episode),
        ("<tvshow", MetadataKind::Show),
    ] {
        if let Some(i) = lower.find(tag) {
            // Ensure it is a tag start, not prose.
            let ok = i == 0
                || xml
                    .as_bytes()
                    .get(i.wrapping_sub(1))
                    .is_none_or(|b| b.is_ascii_whitespace() || *b == b'>');
            if ok {
                return Ok(kind);
            }
        }
    }
    Err(NfoError::UnknownRoot)
}

fn has_unclosed(xml: &str, tag: &str) -> bool {
    let open = format!("<{tag}");
    let close = format!("</{tag}>");
    let opens = count_ci(xml, &open);
    let closes = count_ci(xml, &close);
    opens > closes
}

fn count_ci(hay: &str, needle: &str) -> usize {
    let h = hay.to_ascii_lowercase();
    let n = needle.to_ascii_lowercase();
    h.match_indices(&n).count()
}

fn first_text(xml: &str, tag: &str) -> Option<String> {
    all_text(xml, tag).into_iter().next()
}

fn all_text(xml: &str, tag: &str) -> Vec<String> {
    let open = format!("<{tag}");
    let close = format!("</{tag}>");
    let lower = xml.to_ascii_lowercase();
    let open_l = open.to_ascii_lowercase();
    let close_l = close.to_ascii_lowercase();
    let mut out = Vec::new();
    let mut from = 0;
    while let Some(rel) = lower[from..].find(&open_l) {
        let start = from + rel;
        let after_name = start + open_l.len();
        // Require tag boundary: `>` or whitespace then eventually `>`.
        let bytes = xml.as_bytes();
        let boundary = bytes.get(after_name).copied();
        if boundary != Some(b'>')
            && boundary != Some(b' ')
            && boundary != Some(b'\n')
            && boundary != Some(b'\r')
            && boundary != Some(b'\t')
            && boundary != Some(b'/')
        {
            from = after_name;
            continue;
        }
        let Some(gt_rel) = lower[after_name..].find('>') else {
            break;
        };
        let content_start = after_name + gt_rel + 1;
        // Self-closing.
        if xml.as_bytes().get(content_start.saturating_sub(2)) == Some(&b'/') {
            from = content_start;
            continue;
        }
        let Some(end_rel) = lower[content_start..].find(&close_l) else {
            break;
        };
        let content_end = content_start + end_rel;
        let raw = &xml[content_start..content_end];
        out.push(strip_inner_tags(raw));
        from = content_end + close_l.len();
    }
    out
}

fn strip_inner_tags(s: &str) -> String {
    // Ratings sometimes wrap <value>; actors we handle separately.
    let mut out = String::with_capacity(s.len());
    let mut chars = s.chars().peekable();
    while let Some(c) = chars.next() {
        if c == '<' {
            for n in chars.by_ref() {
                if n == '>' {
                    break;
                }
            }
        } else {
            out.push(c);
        }
    }
    decode_basic_entities(out.trim())
}

fn decode_basic_entities(s: &str) -> String {
    s.replace("&amp;", "&")
        .replace("&lt;", "<")
        .replace("&gt;", ">")
        .replace("&quot;", "\"")
        .replace("&apos;", "'")
}

fn parse_actors(xml: &str) -> Vec<CastMember> {
    let lower = xml.to_ascii_lowercase();
    let mut out = Vec::new();
    let mut from = 0;
    while let Some(rel) = lower[from..].find("<actor") {
        let start = from + rel;
        let Some(gt) = lower[start..].find('>') else {
            break;
        };
        let body_start = start + gt + 1;
        let Some(end_rel) = lower[body_start..].find("</actor>") else {
            break;
        };
        let body = &xml[body_start..body_start + end_rel];
        if let Some(name) = first_text(body, "name").filter(|n| !n.trim().is_empty()) {
            out.push(CastMember {
                name: name.trim().to_string(),
                role: first_text(body, "role").map(|r| r.trim().to_string()),
                order: first_text(body, "order").and_then(|o| o.trim().parse().ok()),
            });
        }
        from = body_start + end_rel + "</actor>".len();
    }
    out
}

fn parse_ratings(xml: &str) -> Vec<Rating> {
    let mut out = Vec::new();
    let lower = xml.to_ascii_lowercase();
    let mut from = 0;
    while let Some(rel) = lower[from..].find("<rating") {
        let start = from + rel;
        // Skip <ratings> wrapper open.
        if lower[start..].starts_with("<ratings") {
            from = start + 8;
            continue;
        }
        let Some(gt) = lower[start..].find('>') else {
            break;
        };
        let open_end = start + gt + 1;
        let open_tag = &xml[start..open_end];
        let source = attr_value(open_tag, "name").unwrap_or_else(|| "default".to_string());
        let Some(end_rel) = lower[open_end..].find("</rating>") else {
            break;
        };
        let body = &xml[open_end..open_end + end_rel];
        let value = first_text(body, "value")
            .or_else(|| {
                let t = strip_inner_tags(body);
                if t.is_empty() { None } else { Some(t) }
            })
            .and_then(|v| v.trim().parse().ok());
        if let Some(value) = value {
            out.push(Rating {
                source,
                value,
                votes: first_text(body, "votes").and_then(|v| v.trim().parse().ok()),
            });
        }
        from = open_end + end_rel + "</rating>".len();
    }
    // Legacy single <rating>8.8</rating> without nested value already handled
    // when body has no child tags — covered above.
    out
}

fn parse_ids(xml: &str) -> ProviderIds {
    let mut ids = ProviderIds::default();
    let lower = xml.to_ascii_lowercase();
    let mut from = 0;
    while let Some(rel) = lower[from..].find("<uniqueid") {
        let start = from + rel;
        let Some(gt) = lower[start..].find('>') else {
            break;
        };
        let open_end = start + gt + 1;
        let open_tag = &xml[start..open_end];
        let id_type = attr_value(open_tag, "type").unwrap_or_default();
        let Some(end_rel) = lower[open_end..].find("</uniqueid>") else {
            break;
        };
        let raw = strip_inner_tags(&xml[open_end..open_end + end_rel]);
        let raw = raw.trim();
        match id_type.to_ascii_lowercase().as_str() {
            "tmdb" | "tmdbid" => {
                if let Ok(v) = raw.parse() {
                    ids.tmdb = Some(v);
                }
            }
            "imdb" => {
                if !raw.is_empty() {
                    ids.imdb = Some(raw.to_string());
                }
            }
            "tvdb" | "tvdbid" => {
                if let Ok(v) = raw.parse() {
                    ids.tvdb = Some(v);
                }
            }
            _ => {}
        }
        from = open_end + end_rel + "</uniqueid>".len();
    }
    // Legacy root-level tags: `first_text` would also match a `<tmdbid>`
    // nested inside `<actor>` (some exporters write actor-level TMDB ids), so
    // scope these to direct children of the root element only.
    if ids.tmdb.is_none()
        && let Some(v) = first_root_text(xml, "tmdbid").and_then(|s| s.trim().parse().ok())
    {
        ids.tmdb = Some(v);
    }
    if ids.imdb.is_none()
        && let Some(v) = first_root_text(xml, "imdbid").map(|s| s.trim().to_string())
        && !v.is_empty()
    {
        ids.imdb = Some(v);
    }
    if ids.tvdb.is_none()
        && let Some(v) = first_root_text(xml, "tvdbid").and_then(|s| s.trim().parse().ok())
    {
        ids.tvdb = Some(v);
    }
    ids
}

/// [`first_text`] restricted to direct children of the document root, so a
/// `<tmdbid>` / `<imdbid>` / `<tvdbid>` nested inside `<actor>`, `<set>`,
/// `<ratings>`, … never satisfies the provider-id fallback.
fn first_root_text(xml: &str, tag: &str) -> Option<String> {
    let root = detect_kind(xml).ok()?.as_str();
    let lower = xml.to_ascii_lowercase();
    let open = format!("<{root}");
    let start = lower.find(&open)?;
    let after_name = start + open.len();
    let gt_rel = lower[after_name..].find('>')?;
    let body_start = after_name + gt_rel + 1;
    let close = format!("</{root}>");
    let close_rel = lower[body_start..].rfind(&close)?;
    let body_end = body_start + close_rel;

    // Walk the root body top-level elements; nested subtrees are skipped whole
    // so `<tag>` matches only at depth 1.
    let mut i = body_start;
    while i < body_end {
        if lower[i..].starts_with("<!--") {
            i = i + 4 + lower[i + 4..body_end].find("-->")? + 3;
            continue;
        }
        if !lower[i..].starts_with('<') {
            i += 1;
            continue;
        }
        let gt_rel = lower[i..].find('>')?;
        let open_end = i + gt_rel + 1;
        let open_tag = &lower[i..open_end];
        // Self-closing (`<thumb ... />`) — no body.
        if open_tag[..open_tag.len() - 1].ends_with('/') {
            i = open_end;
            continue;
        }
        let name_end = open_tag[1..]
            .find(|c: char| c.is_ascii_whitespace() || c == '>')
            .map(|n| n + 1)
            .unwrap_or(open_tag.len());
        let name = &open_tag[1..name_end];
        let content_start = open_end;
        let close_tag = format!("</{name}>");
        let end_rel = lower[content_start..body_end].find(&close_tag)?;
        if name == tag {
            let raw = strip_inner_tags(&xml[content_start..content_start + end_rel]);
            let raw = raw.trim();
            return (!raw.is_empty()).then(|| raw.to_string());
        }
        i = content_start + end_rel + close_tag.len();
    }
    None
}

fn parse_artwork(xml: &str) -> Vec<ArtworkRef> {
    let mut out = Vec::new();
    let lower = xml.to_ascii_lowercase();
    let mut from = 0;
    while let Some(rel) = lower[from..].find("<thumb") {
        let start = from + rel;
        let Some(gt) = lower[start..].find('>') else {
            break;
        };
        let open_end = start + gt + 1;
        let open_tag = &xml[start..open_end];
        let aspect = attr_value(open_tag, "aspect").unwrap_or_default();
        let kind = match aspect.to_ascii_lowercase().as_str() {
            "poster" | "" => ArtworkKind::Poster,
            "banner" => ArtworkKind::Banner,
            "clearlogo" | "logo" => ArtworkKind::Logo,
            _ => ArtworkKind::Other,
        };
        if open_tag.trim_end().ends_with("/>") || open_tag.contains("/>") {
            from = open_end;
            continue;
        }
        let Some(end_rel) = lower[open_end..].find("</thumb>") else {
            break;
        };
        let path = strip_inner_tags(&xml[open_end..open_end + end_rel]);
        if !path.is_empty() {
            out.push(ArtworkRef { kind, path });
        }
        from = open_end + end_rel + "</thumb>".len();
    }
    if let Some(path) = first_text(xml, "fanart").filter(|p| !p.is_empty()) {
        out.push(ArtworkRef {
            kind: ArtworkKind::Backdrop,
            path,
        });
    }
    out
}

fn parse_collection(xml: &str) -> Option<CollectionRef> {
    let lower = xml.to_ascii_lowercase();
    let start = lower.find("<set")?;
    let gt = lower[start..].find('>')?;
    let body_start = start + gt + 1;
    let end_rel = lower[body_start..].find("</set>")?;
    let body = &xml[body_start..body_start + end_rel];
    let name = first_text(body, "name").map(|s| s.trim().to_string());
    let id = first_text(body, "tmdb")
        .or_else(|| first_text(body, "tmdbcolid"))
        .and_then(|s| s.trim().parse().ok());
    if name.is_none() && id.is_none() {
        return None;
    }
    Some(CollectionRef { id, name })
}

fn attr_value(open_tag: &str, name: &str) -> Option<String> {
    let lower = open_tag.to_ascii_lowercase();
    let key = format!("{name}=");
    let key_l = key.to_ascii_lowercase();
    let idx = lower.find(&key_l)?;
    let rest = &open_tag[idx + key.len()..];
    let mut chars = rest.chars();
    let quote = chars.next()?;
    if quote != '"' && quote != '\'' {
        return None;
    }
    let mut val = String::new();
    for c in chars {
        if c == quote {
            return Some(val);
        }
        val.push(c);
    }
    None
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::model::item_key_for_metadata;

    fn fixture(name: &str) -> String {
        let path = format!("{}/tests/fixtures/{name}", env!("CARGO_MANIFEST_DIR"));
        std::fs::read_to_string(&path).unwrap_or_else(|e| panic!("read {path}: {e}"))
    }

    #[test]
    fn parses_movie_nfo() {
        let meta = parse_nfo(&fixture("movie.nfo")).unwrap();
        assert_eq!(meta.kind, MetadataKind::Movie);
        assert_eq!(meta.title, "Fight Club");
        assert_eq!(meta.original_title.as_deref(), Some("Fight Club"));
        assert_eq!(meta.year, Some(1999));
        assert_eq!(meta.runtime_minutes, Some(139));
        assert!(meta.genres.iter().any(|g| g == "Drama"));
        assert_eq!(meta.ids.tmdb, Some(550));
        assert_eq!(meta.ids.imdb.as_deref(), Some("tt0137523"));
        assert_eq!(
            item_key_for_metadata(&meta).as_deref(),
            Some("tmdb:movie:550")
        );
        assert!(meta.cast.iter().any(|c| c.name == "Brad Pitt"));
        assert_eq!(
            meta.collection.as_ref().and_then(|c| c.name.as_deref()),
            Some("Fight Club Collection")
        );
    }

    #[test]
    fn parses_episode_nfo() {
        let meta = parse_nfo(&fixture("episode.nfo")).unwrap();
        assert_eq!(meta.kind, MetadataKind::Episode);
        assert_eq!(meta.title, "Pilot");
        assert_eq!(meta.season, Some(1));
        assert_eq!(meta.episode, Some(1));
        assert_eq!(meta.ids.tmdb, Some(62085));
        assert_eq!(
            item_key_for_metadata(&meta).as_deref(),
            Some("tmdb:episode:62085")
        );
    }

    #[test]
    fn rejects_malformed_nfo() {
        let err = parse_nfo(&fixture("malformed.nfo")).unwrap_err();
        assert!(matches!(
            err,
            NfoError::Malformed(_) | NfoError::MissingTitle | NfoError::UnknownRoot
        ));
    }

    /// An actor-level `<tmdbid>` must never satisfy the provider-id fallback:
    /// `parse_ids` scopes legacy ids to direct children of the root.
    #[test]
    fn cast_tmdbid_is_not_a_provider_id() {
        let meta = parse_nfo(
            r#"<?xml version="1.0" encoding="UTF-8"?>
<movie>
  <title>Fight Club</title>
  <year>1999</year>
  <actor>
    <name>Brad Pitt</name>
    <tmdbid>287</tmdbid>
  </actor>
  <actor>
    <name>Edward Norton</name>
    <tmdbid>819</tmdbid>
  </actor>
</movie>"#,
        )
        .unwrap();
        assert_eq!(
            meta.ids.tmdb, None,
            "actor tmdbid must not become the movie id"
        );
        assert_eq!(meta.ids.imdb, None);
        assert_eq!(meta.ids.tvdb, None);
        assert_eq!(item_key_for_metadata(&meta), None);
    }

    /// Root-level legacy `<tmdbid>` still works as a fallback, even when a
    /// cast block with `<tmdbid>` appears earlier in the document.
    #[test]
    fn root_tmdbid_fallback_ignores_actor_block_before_it() {
        let meta = parse_nfo(
            r#"<?xml version="1.0" encoding="UTF-8"?>
<movie>
  <title>Fight Club</title>
  <year>1999</year>
  <actor>
    <name>Brad Pitt</name>
    <tmdbid>287</tmdbid>
  </actor>
  <tmdbid>550</tmdbid>
</movie>"#,
        )
        .unwrap();
        assert_eq!(meta.ids.tmdb, Some(550));
        assert_eq!(
            item_key_for_metadata(&meta).as_deref(),
            Some("tmdb:movie:550")
        );
    }
}
