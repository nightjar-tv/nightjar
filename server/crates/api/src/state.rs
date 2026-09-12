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

#[cfg(test)]
pub(crate) mod test_support {
    use crate::state::AppState;
    use nightjar_db::Db;
    use std::sync::Arc;

    /// The smallest `AppState` a handler test can run against.
    ///
    /// Achievable only because `TranscodeCapabilities::software_only` exists:
    /// the real startup path probes ffmpeg encoders, which is far too slow to
    /// pay per test. The pool still spawns its worker threads, which idle.
    pub(crate) fn state(dir: &std::path::Path) -> AppState {
        state_with_encoder_cap(dir, 1)
    }

    /// The same state with an explicit operator encoder cap. A test that wants
    /// the per-account ceiling to be the binding limit raises this above the
    /// account value, so measured/operator admission does not refuse first.
    pub(crate) fn state_with_encoder_cap(dir: &std::path::Path, max_encoders: usize) -> AppState {
        let db = Arc::new(Db::open(std::path::Path::new(":memory:")).unwrap());
        let subs = Arc::new(nightjar_transcode::SubsStore::new(dir.join("subs")).unwrap());
        let caps = Arc::new(nightjar_transcode::TranscodeCapabilities::software_only(
            None,
            "test state does not probe encoders",
        ));
        let hls = nightjar_transcode::HlsSessionRegistry::with_cap(
            dir.join("hls"),
            max_encoders,
            caps.preferred_encode_leg.clone(),
            Some(Arc::clone(&subs)),
            Some(Arc::clone(&db)),
        )
        .unwrap();
        let pool = nightjar_scanner::LibraryPool::spawn(Arc::clone(&db), Arc::clone(&subs));
        AppState {
            db,
            hls,
            transcode_caps: caps,
            tonemap_available: false,
            subs,
            pool,
            artwork: None,
        }
    }
}
