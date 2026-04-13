//! `UpstreamPool`: the collection of configured upstreams plus shared routing state.

use super::stats::StatsStore;
use crate::config::Config;
use crate::upstream::Upstream;
use dashmap::DashMap;
use std::sync::atomic::AtomicU64;
use std::sync::Arc;

pub struct UpstreamPool {
    pub upstreams: Vec<Arc<Upstream>>,
    pub stats: Arc<StatsStore>,
    // DIFFERENTIATOR 2: global tip slot — max slot seen across all upstreams.
    pub tip_slot: Arc<AtomicU64>,
    /// Sticky leader per method. The router only swaps leaders when a challenger
    /// beats the incumbent by more than `routing.hysteresis_margin`.
    pub current_leader: DashMap<String, String>,
    pub config: Config,
}

impl UpstreamPool {
    pub fn from_config(cfg: &Config) -> Self {
        let upstreams = cfg
            .upstreams
            .iter()
            .map(|u| {
                Arc::new(Upstream::new(
                    u.name.clone(),
                    u.url.clone(),
                    u.headers.clone(),
                    cfg.max_concurrent_per_upstream,
                ))
            })
            .collect();
        Self {
            upstreams,
            stats: Arc::new(StatsStore::new(
                cfg.routing.ewma_alpha,
                cfg.health.rolling_window_secs,
            )),
            tip_slot: Arc::new(AtomicU64::new(0)),
            current_leader: DashMap::new(),
            config: cfg.clone(),
        }
    }
}
