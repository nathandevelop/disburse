//! Background per-upstream health monitor: polls `getSlot`, tracks the global tip,
//! drives the circuit breaker, and seeds the blockhash freshness cache.

use super::Upstream;
use crate::routing::UpstreamPool;
use crate::solana::BlockhashCache;
use serde_json::{json, Value};
use std::sync::atomic::Ordering;
use std::sync::Arc;
use std::time::{Duration, Instant};
use tracing::{debug, warn};

pub fn spawn_health_monitors(pool: Arc<UpstreamPool>, blockhash_cache: Arc<BlockhashCache>) {
    for u in pool.upstreams.clone() {
        let pool = pool.clone();
        let bhc = blockhash_cache.clone();
        tokio::spawn(async move {
            run_monitor(u, pool, bhc).await;
        });
    }
}

async fn run_monitor(u: Arc<Upstream>, pool: Arc<UpstreamPool>, bhc: Arc<BlockhashCache>) {
    let interval = Duration::from_secs(pool.config.health.check_interval_secs);
    let cooldown = Duration::from_secs(pool.config.health.circuit_breaker_cooldown_secs);
    let threshold = pool.config.health.circuit_breaker_error_rate;
    let slot_timeout = Duration::from_millis(pool.config.request_timeout.for_method("getSlot"));
    let bh_timeout =
        Duration::from_millis(pool.config.request_timeout.for_method("getLatestBlockhash"));

    loop {
        tokio::time::sleep(interval).await;

        // DIFFERENTIATOR 2: slot lag awareness — poll getSlot and update tip.
        let start = Instant::now();
        let req = json!({"jsonrpc":"2.0","id":1,"method":"getSlot","params":[]});
        let result = u
            .client
            .post(&u.url)
            .headers(u.headers.clone())
            .timeout(slot_timeout)
            .json(&req)
            .send()
            .await;
        let latency = start.elapsed().as_secs_f64() * 1000.0;

        let (ok, slot) = match result {
            Ok(resp) => match resp.json::<Value>().await {
                Ok(v) => match v.get("result").and_then(|r| r.as_u64()) {
                    Some(s) => {
                        u.last_slot.store(s, Ordering::Relaxed);
                        (true, Some(s))
                    }
                    None => (false, None),
                },
                Err(_) => (false, None),
            },
            Err(_) => (false, None),
        };

        pool.stats.record(&u.name, "getSlot", latency, ok);

        if let Some(s) = slot {
            let mut tip = pool.tip_slot.load(Ordering::Relaxed);
            while s > tip {
                match pool
                    .tip_slot
                    .compare_exchange(tip, s, Ordering::Relaxed, Ordering::Relaxed)
                {
                    Ok(_) => break,
                    Err(cur) => tip = cur,
                }
            }
        }

        // Circuit breaker: drop upstream when rolling 60s error rate exceeds threshold.
        let err_rate = pool.stats.error_rate(&u.name);
        if err_rate > threshold && !u.is_dropped() {
            warn!(upstream=%u.name, error_rate=err_rate, "circuit breaker tripped");
            u.trip(cooldown);
        }

        // DIFFERENTIATOR 3 support: seed the blockhash cache from getLatestBlockhash.
        let req = json!({"jsonrpc":"2.0","id":1,"method":"getLatestBlockhash","params":[]});
        if let Ok(resp) = u
            .client
            .post(&u.url)
            .headers(u.headers.clone())
            .timeout(bh_timeout)
            .json(&req)
            .send()
            .await
        {
            if let Ok(v) = resp.json::<Value>().await {
                if let Some(bh) = v
                    .pointer("/result/value/blockhash")
                    .and_then(|b| b.as_str())
                {
                    let ctx_slot = v
                        .pointer("/result/context/slot")
                        .and_then(|s| s.as_u64())
                        .unwrap_or_else(|| u.last_slot.load(Ordering::Relaxed));
                    bhc.insert(bh.to_string(), ctx_slot);
                }
            }
        }

        debug!(upstream=%u.name, slot=?slot, latency_ms=latency, err_rate=err_rate, "health tick");
    }
}
