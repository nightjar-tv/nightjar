use nightjar_db::Db;
use nightjar_transcode::{HlsSessionRegistry, RemuxRegistry, TranscodeCapabilities};
use std::path::PathBuf;
use std::sync::Arc;

#[derive(Clone)]
pub struct AppState {
    pub db: Arc<Db>,
    pub remux: Arc<RemuxRegistry>,
    pub hls: Arc<HlsSessionRegistry>,
    pub transcode_caps: Arc<TranscodeCapabilities>,
    pub subs_cache_dir: PathBuf,
}

impl AppState {
    pub fn new(
        db: Db,
        remux: RemuxRegistry,
        hls: Arc<HlsSessionRegistry>,
        transcode_caps: Arc<TranscodeCapabilities>,
        subs_cache_dir: PathBuf,
    ) -> Self {
        Self {
            db: Arc::new(db),
            remux: Arc::new(remux),
            hls,
            transcode_caps,
            subs_cache_dir,
        }
    }
}
