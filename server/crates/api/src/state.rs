use nightjar_db::Db;
use nightjar_transcode::{HlsSessionRegistry, RemuxRegistry};
use std::sync::Arc;

#[derive(Clone)]
pub struct AppState {
    pub db: Arc<Db>,
    pub remux: Arc<RemuxRegistry>,
    pub hls: Arc<HlsSessionRegistry>,
}

impl AppState {
    pub fn new(db: Db, remux: RemuxRegistry, hls: Arc<HlsSessionRegistry>) -> Self {
        Self {
            db: Arc::new(db),
            remux: Arc::new(remux),
            hls,
        }
    }
}
