//! Routing brain: per-method scoring, upstream pool, and rolling stats.

mod pool;
mod router;
pub mod stats;

pub use pool::UpstreamPool;
pub use router::select_for_method;

use crate::upstream::Upstream;
use std::sync::Arc;
use tokio::sync::OwnedSemaphorePermit;

/// Find the best upstream that will also admit the request (circuit closed or
/// probe-available, concurrency cap not hit). Returns `None` when every
/// candidate is excluded, circuit-open, or at capacity.
///
/// Returns `(upstream, permit, is_probe)`. The permit must be held for the
/// lifetime of the request; `is_probe` indicates whether this call gates the
/// half-open probe on the upstream.
pub fn select_and_admit(
    pool: &UpstreamPool,
    method: &str,
    excluded: &[String],
) -> Option<(Arc<Upstream>, OwnedSemaphorePermit, bool)> {
    let mut admission_skips: Vec<String> = Vec::new();
    loop {
        let filter: Vec<String> = excluded
            .iter()
            .chain(admission_skips.iter())
            .cloned()
            .collect();
        let u = select_for_method(pool, method, &filter)?;
        if let Some((permit, is_probe)) = u.try_admit() {
            return Some((u, permit, is_probe));
        }
        admission_skips.push(u.name.clone());
    }
}
