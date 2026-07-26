use nightjar_db::Db;
use nightjar_transcode::{HlsSessionRegistry, SubsCache, TranscodeCapabilities};
use std::sync::Arc;

#[derive(Clone)]
pub struct AppState {
    pub db: Arc<Db>,
    pub hls: Arc<HlsSessionRegistry>,
    pub transcode_caps: Arc<TranscodeCapabilities>,
    pub subs: Arc<SubsCache>,
}

impl AppState {
    pub fn new(
        db: Db,
        hls: Arc<HlsSessionRegistry>,
        transcode_caps: Arc<TranscodeCapabilities>,
        subs: SubsCache,
    ) -> Self {
        Self {
            db: Arc::new(db),
            hls,
            transcode_caps,
            subs: Arc::new(subs),
        }
    }
}
