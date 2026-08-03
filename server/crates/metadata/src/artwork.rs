//! Local artwork cache (ADR-0027).
//!
//! Downloads TMDB originals, optional derived widths, invalidate on key change.

use std::fs;
use std::path::{Path, PathBuf};
use std::sync::{Arc, Condvar, Mutex};

use crate::fix::ArtworkInvalidate;
use crate::model::ArtworkKind;

/// Simultaneous `image.tmdb.org` downloads (ADR-0027 §4).
pub const IMAGE_CDN_MAX_IN_FLIGHT: usize = 8;

/// First-cut derived widths (ADR-0027 §3); re-measure on dogfood.
pub const THUMB_WIDTHS: &[u32] = &[342, 780];

const TMDB_IMAGE_BASE: &str = "https://image.tmdb.org/t/p/original";

#[derive(Debug)]
struct CdnSlots {
    max: usize,
    active: Mutex<usize>,
    cv: Condvar,
}

impl CdnSlots {
    fn new(max: usize) -> Self {
        Self {
            max,
            active: Mutex::new(0),
            cv: Condvar::new(),
        }
    }

    fn acquire(&self) -> Result<CdnPermit<'_>, String> {
        let mut g = self
            .active
            .lock()
            .map_err(|_| "cdn slots poisoned".to_string())?;
        while *g >= self.max {
            g = self
                .cv
                .wait(g)
                .map_err(|_| "cdn slots wait poisoned".to_string())?;
        }
        *g += 1;
        Ok(CdnPermit { slots: self })
    }
}

struct CdnPermit<'a> {
    slots: &'a CdnSlots,
}

impl Drop for CdnPermit<'_> {
    fn drop(&mut self) {
        if let Ok(mut g) = self.slots.active.lock() {
            *g = g.saturating_sub(1);
            self.slots.cv.notify_one();
        }
    }
}

#[derive(Debug, Clone)]
pub struct ArtworkStore {
    root: PathBuf,
    slots: Arc<CdnSlots>,
    /// Serialise mkdir/download for one key.
    gate: Arc<Mutex<()>>,
}

impl ArtworkStore {
    pub fn new(data_dir: &Path) -> Result<Self, String> {
        let root = data_dir.join("artwork");
        fs::create_dir_all(&root).map_err(|e| format!("create artwork dir: {e}"))?;
        Ok(Self {
            root,
            slots: Arc::new(CdnSlots::new(IMAGE_CDN_MAX_IN_FLIGHT)),
            gate: Arc::new(Mutex::new(())),
        })
    }

    pub fn root(&self) -> &Path {
        &self.root
    }

    pub fn safe_key(item_key: &str) -> String {
        item_key
            .chars()
            .map(|c| match c {
                'a'..='z' | 'A'..='Z' | '0'..='9' | '-' | '_' | '.' => c,
                _ => '_',
            })
            .collect()
    }

    fn kind_name(kind: ArtworkKind) -> &'static str {
        match kind {
            ArtworkKind::Poster => "poster",
            ArtworkKind::Backdrop => "backdrop",
            ArtworkKind::Banner => "banner",
            ArtworkKind::Logo => "logo",
            ArtworkKind::Other => "still",
        }
    }

    pub fn parse_kind(s: &str) -> Option<ArtworkKind> {
        match s {
            "poster" => Some(ArtworkKind::Poster),
            "backdrop" => Some(ArtworkKind::Backdrop),
            "banner" => Some(ArtworkKind::Banner),
            "logo" => Some(ArtworkKind::Logo),
            "still" | "other" => Some(ArtworkKind::Other),
            _ => None,
        }
    }

    fn dir_for(&self, item_key: &str) -> PathBuf {
        self.root.join(Self::safe_key(item_key))
    }

    pub fn original_path(&self, item_key: &str, kind: ArtworkKind) -> PathBuf {
        self.dir_for(item_key)
            .join(format!("{}.orig", Self::kind_name(kind)))
    }

    pub fn derived_path(&self, item_key: &str, kind: ArtworkKind, width: u32) -> PathBuf {
        self.dir_for(item_key)
            .join(format!("{}.w{width}.jpg", Self::kind_name(kind)))
    }

    /// Remove cached art for keys (assign/clear).
    pub fn invalidate_keys(&self, keys: &[String]) {
        for k in keys {
            let dir = self.dir_for(k);
            let _ = fs::remove_dir_all(dir);
        }
    }

    /// Ensure original is on disk. `tmdb_path` is a `/abc.jpg` relative path.
    pub fn ensure_tmdb_original(
        &self,
        item_key: &str,
        kind: ArtworkKind,
        tmdb_path: &str,
    ) -> Result<PathBuf, String> {
        let dest = self.original_path(item_key, kind);
        if dest.is_file() {
            return Ok(dest);
        }
        let _g = self
            .gate
            .lock()
            .map_err(|_| "artwork gate poisoned".to_string())?;
        if dest.is_file() {
            return Ok(dest);
        }
        fs::create_dir_all(dest.parent().unwrap()).map_err(|e| format!("mkdir art: {e}"))?;
        let path = tmdb_path.trim();
        if path.is_empty() {
            return Err("empty tmdb artwork path".into());
        }
        let url = format!("{TMDB_IMAGE_BASE}{path}");
        let _permit = self.slots.acquire()?;
        let resp = ureq::get(&url)
            .call()
            .map_err(|e| format!("artwork download: {e}"))?;
        if !(200..300).contains(&resp.status()) {
            return Err(format!("artwork HTTP {}", resp.status()));
        }
        let mut reader = resp.into_reader();
        let mut file = fs::File::create(&dest).map_err(|e| format!("create art: {e}"))?;
        std::io::copy(&mut reader, &mut file).map_err(|e| format!("write art: {e}"))?;
        Ok(dest)
    }

    /// Best path for serve: derived width if present/creatable, else original.
    pub fn resolve_file(
        &self,
        item_key: &str,
        kind: ArtworkKind,
        width: Option<u32>,
        tmdb_path: Option<&str>,
    ) -> Result<PathBuf, String> {
        let orig = self.original_path(item_key, kind);
        if !orig.is_file() {
            let Some(tp) = tmdb_path.filter(|p| !p.is_empty()) else {
                return Err("artwork not cached and no tmdb path".into());
            };
            self.ensure_tmdb_original(item_key, kind, tp)?;
        }
        if let Some(w) = width.filter(|w| THUMB_WIDTHS.contains(w)) {
            let derived = self.derived_path(item_key, kind, w);
            if derived.is_file() {
                return Ok(derived);
            }
            if try_ffmpeg_scale(&orig, &derived, w).is_ok() {
                return Ok(derived);
            }
        }
        Ok(self.original_path(item_key, kind))
    }
}

impl ArtworkInvalidate for ArtworkStore {
    fn on_item_key_change(&self, old_keys: &[String], _new_key: Option<&str>) {
        self.invalidate_keys(old_keys);
    }
}

fn try_ffmpeg_scale(src: &Path, dest: &Path, width: u32) -> Result<(), String> {
    let status = std::process::Command::new("ffmpeg")
        .args([
            "-y",
            "-loglevel",
            "error",
            "-i",
            src.to_str().unwrap_or(""),
            "-vf",
            &format!("scale={width}:-1"),
            dest.to_str().unwrap_or(""),
        ])
        .status()
        .map_err(|e| format!("ffmpeg: {e}"))?;
    if status.success() && dest.is_file() {
        Ok(())
    } else {
        let _ = fs::remove_file(dest);
        Err("ffmpeg scale failed".into())
    }
}

/// Look up first poster path from canonical JSON for an item_key.
pub fn poster_path_for_item_key(conn: &rusqlite::Connection, item_key: &str) -> Option<String> {
    let (kind, id) = parse_provider_key(item_key)?;
    let artwork_json: Option<String> = conn
        .query_row(
            "SELECT artwork_json FROM metadata_canonical
             WHERE provider = 'tmdb' AND entity_kind = ?1 AND provider_id = ?2",
            rusqlite::params![kind, id],
            |r| r.get(0),
        )
        .ok()?;
    let raw = artwork_json?;
    let arr: Vec<crate::model::ArtworkRef> = serde_json::from_str(&raw).ok()?;
    arr.into_iter()
        .find(|a| a.kind == ArtworkKind::Poster && a.path.starts_with('/'))
        .map(|a| a.path)
        .or_else(|| {
            let arr: Vec<crate::model::ArtworkRef> = serde_json::from_str(&raw).ok()?;
            arr.into_iter()
                .find(|a| a.path.starts_with('/'))
                .map(|a| a.path)
        })
}

fn parse_provider_key(item_key: &str) -> Option<(&'static str, String)> {
    if let Some(id) = item_key.strip_prefix("tmdb:movie:") {
        return Some(("movie", id.to_string()));
    }
    if let Some(id) = item_key.strip_prefix("tmdb:episode:") {
        return Some(("episode", id.to_string()));
    }
    None
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn safe_key_strips_colons() {
        assert_eq!(ArtworkStore::safe_key("tmdb:movie:550"), "tmdb_movie_550");
    }
}
