use nightjar_db::Db;
use nightjar_transcode::RemuxRegistry;
use std::sync::Arc;

#[derive(Clone)]
pub struct AppState {
    pub db: Arc<Db>,
    pub remux: Arc<RemuxRegistry>,
}

impl AppState {
    pub fn new(db: Db, remux: RemuxRegistry) -> Self {
        Self {
            db: Arc::new(db),
            remux: Arc::new(remux),
        }
    }
}
