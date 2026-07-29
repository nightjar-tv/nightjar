use nightjar_db::Db;
use nightjar_scanner::LibraryPool;
use nightjar_transcode::{HlsSessionRegistry, SubsStore, TranscodeCapabilities};
use std::sync::Arc;

#[derive(Clone)]
pub struct AppState {
    pub db: Arc<Db>,
    pub hls: Arc<HlsSessionRegistry>,
    pub transcode_caps: Arc<TranscodeCapabilities>,
    pub subs: Arc<SubsStore>,
    pub pool: Arc<LibraryPool>,
}
