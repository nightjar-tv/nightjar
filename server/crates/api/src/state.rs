use nightjar_db::Db;
use nightjar_metadata::ArtworkStore;
use nightjar_scanner::LibraryPool;
use nightjar_transcode::{HlsSessionRegistry, SubsStore, TranscodeCapabilities};
use std::sync::Arc;

#[derive(Clone)]
pub struct AppState {
    pub db: Arc<Db>,
    pub hls: Arc<HlsSessionRegistry>,
    pub transcode_caps: Arc<TranscodeCapabilities>,
    /// Host FFmpeg has `zscale` (libzimg) for HDR→SDR (ADR-0022).
    pub tonemap_available: bool,
    pub subs: Arc<SubsStore>,
    pub pool: Arc<LibraryPool>,
    pub artwork: Option<Arc<ArtworkStore>>,
}
