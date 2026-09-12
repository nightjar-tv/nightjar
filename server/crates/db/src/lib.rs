//! SQLite layer: WAL, numbered append-only migrations, library/item access.

mod accounts;
mod content_id;
mod migrate;
mod paths;
mod status;
mod store;
mod watch;

pub use accounts::{
    AccountRow, ProfileRow, SessionRejection, SessionRow, account_by_id, account_by_username,
    account_exists, admin_exists, classify_session, create_account_with_profile, create_profile,
    create_session, delete_account, delete_profile, library_exists, list_accounts, now_iso,
    profile_by_ref, profiles_for_account, revoke_all_for_account, revoke_session, session_expiry,
    session_for_token, set_active_profile, set_password_hash, set_role, touch_last_seen,
    transfer_ownership,
};
pub use content_id::{
    CONTENT_ID_WINDOW, content_id_for_path, content_id_from_reader, content_id_matches,
    format_content_id,
};
pub use migrate::migrate;
pub use paths::{
    fold_path, is_absolute_stored, is_numbered_season_directory, normalize_library_root,
    paths_fold_equal, require_library_root, require_relpath, resolve_media_path,
    season_number_for_path, show_folder_relpath, to_relpath, under_numbered_season_directory,
};
pub use status::{
    SidecarPresence, SubtitleTrackKind, backoff_days, classify_subtitle_status,
    parse_map_container_kind, parse_map_status, parse_probe_status, parse_subtitle_status,
};
pub use store::{
    Db, ItemPathRow, KeyframeMapRows, LibraryRow, MediaItemRow, NewLibrary, ProbeUpdate,
    ScanJobRow, ScanProgressCounts, SidecarRow, SubtitleTrackRow, UpsertItem, with_write_tx,
    write_tx,
};
pub use watch::{WatchStateRow, delete_watch_state, load_watch_state, upsert_watch_state};

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
