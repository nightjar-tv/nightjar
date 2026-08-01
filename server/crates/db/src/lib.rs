//! SQLite layer: WAL, numbered append-only migrations, library/item access.

mod content_id;
mod migrate;
mod status;
mod store;

pub use content_id::{CONTENT_ID_WINDOW, content_id_matches, format_content_id};
pub use status::{
    parse_map_container_kind, parse_map_status, parse_probe_status, parse_subtitle_status,
};
pub use store::{
    Db, LibraryRow, MediaItemRow, NewLibrary, ProbeUpdate, ScanJobRow, SidecarRow, UpsertItem,
};

use std::path::{Path, PathBuf};

/// Open (or create) the Nightjar database under `data_dir`, run migrations, enable WAL.
pub fn open(data_dir: &Path) -> Result<Db, String> {
    std::fs::create_dir_all(data_dir)
        .map_err(|e| format!("create data dir {}: {e}", data_dir.display()))?;
    let path = db_path(data_dir);
    Db::open(&path)
}

pub fn db_path(data_dir: &Path) -> PathBuf {
    data_dir.join("nightjar.db")
}

pub fn version() -> &'static str {
    env!("CARGO_PKG_VERSION")
}
