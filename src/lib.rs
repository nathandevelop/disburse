//! disburse library entry point. The binary in `src/main.rs` is a thin CLI
//! wrapper around this; integration tests in `tests/` exercise the same
//! public surface that the binary uses.

pub mod config;
pub mod error;
pub mod proxy;
pub mod routing;
pub mod solana;
pub mod upstream;

pub use crate::config::Config;
pub use crate::proxy::{app, metrics_app, AppState, Metrics};
pub use crate::routing::UpstreamPool;
pub use crate::solana::BlockhashCache;
pub use crate::upstream::spawn_health_monitors;

use std::sync::Arc;

/// Build the shared application state from a loaded config. Does not spawn
/// health monitors — callers that want live slot-lag + blockhash cache seeding
/// should call [`spawn_health_monitors`] separately. Integration tests usually
/// omit health monitors and seed stats manually.
pub fn build_state(cfg: &Config) -> AppState {
    let pool = Arc::new(UpstreamPool::from_config(cfg));
    let blockhash_cache = Arc::new(BlockhashCache::new(
        cfg.health.blockhash_warmup_secs,
        cfg.health.blockhash_cache_size,
    ));
    let metrics = Arc::new(Metrics::new());
    AppState {
        pool,
        blockhash_cache,
        metrics,
    }
}
