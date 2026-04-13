use super::*;
use crate::config::{
    Config, HealthConfig, RetryConfig, RoutingConfig, TimeoutConfig, UpstreamConfig,
};
use std::collections::HashMap;
use std::time::Duration;

fn test_pool(names: &[&str]) -> UpstreamPool {
    test_pool_with_margin(names, 0.0)
}

fn test_pool_with_margin(names: &[&str], margin: f64) -> UpstreamPool {
    let cfg = Config {
        listen: "127.0.0.1:0".into(),
        metrics_listen: "127.0.0.1:0".into(),
        upstreams: names
            .iter()
            .map(|n| UpstreamConfig {
                name: n.to_string(),
                url: "http://127.0.0.1:0/".into(),
                headers: HashMap::new(),
            })
            .collect(),
        routing: RoutingConfig {
            latency_weight: 0.7,
            success_weight: 0.3,
            ewma_alpha: 0.2,
            hysteresis_margin: margin,
        },
        health: HealthConfig {
            check_interval_secs: 3,
            max_slot_lag: 10,
            max_blockhash_age_slots: 100,
            circuit_breaker_error_rate: 0.5,
            circuit_breaker_cooldown_secs: 30,
            rolling_window_secs: 60,
            blockhash_warmup_secs: 30,
            blockhash_cache_size: 512,
        },
        retries: RetryConfig {
            max_attempts: 3,
            retry_on_different_upstream: true,
            deadline_ms: 15_000,
        },
        request_timeout: TimeoutConfig::default(),
        max_request_body_bytes: 2 * 1024 * 1024,
        max_response_body_bytes: 16 * 1024 * 1024,
        max_concurrent_per_upstream: 256,
    };
    UpstreamPool::from_config(&cfg)
}

#[test]
fn warmup_default_returns_some_upstream() {
    let pool = test_pool(&["a", "b"]);
    let chosen = select_for_method(&pool, "getSlot", &[]);
    assert!(chosen.is_some());
}

#[test]
fn lower_latency_wins() {
    let pool = test_pool(&["fast", "slow"]);
    pool.stats.record("fast", "getSlot", 20.0, true);
    pool.stats.record("slow", "getSlot", 500.0, true);
    let chosen = select_for_method(&pool, "getSlot", &[]).unwrap();
    assert_eq!(chosen.name, "fast");
}

#[test]
fn excluded_upstreams_are_skipped() {
    let pool = test_pool(&["a", "b"]);
    pool.stats.record("a", "getSlot", 10.0, true);
    pool.stats.record("b", "getSlot", 500.0, true);
    // "a" would win on score, but it's excluded
    let chosen = select_for_method(&pool, "getSlot", &["a".into()]).unwrap();
    assert_eq!(chosen.name, "b");
}

#[test]
fn all_excluded_returns_none() {
    let pool = test_pool(&["a", "b"]);
    let chosen = select_for_method(&pool, "getSlot", &["a".into(), "b".into()]);
    assert!(chosen.is_none());
}

#[test]
fn dropped_upstreams_are_skipped() {
    let pool = test_pool(&["a", "b"]);
    pool.stats.record("a", "getSlot", 10.0, true);
    pool.stats.record("b", "getSlot", 500.0, true);
    pool.upstreams[0].trip(Duration::from_secs(60));
    let chosen = select_for_method(&pool, "getSlot", &[]).unwrap();
    assert_eq!(chosen.name, "b");
}

#[test]
fn all_dropped_returns_none() {
    let pool = test_pool(&["a", "b"]);
    pool.upstreams[0].trip(Duration::from_secs(60));
    pool.upstreams[1].trip(Duration::from_secs(60));
    assert!(select_for_method(&pool, "getSlot", &[]).is_none());
}

#[test]
fn slot_lag_penalty_downgrades_laggard() {
    let pool = test_pool(&["fresh", "laggy"]);
    // laggy is faster on getSlot itself
    pool.stats.record("fresh", "getSlot", 100.0, true);
    pool.stats.record("laggy", "getSlot", 10.0, true);
    // but laggy is behind the tip
    pool.tip_slot.store(1000, Ordering::Relaxed);
    pool.upstreams[0].last_slot.store(1000, Ordering::Relaxed);
    pool.upstreams[1].last_slot.store(500, Ordering::Relaxed); // 500 behind
    let chosen = select_for_method(&pool, "getSlot", &[]).unwrap();
    assert_eq!(chosen.name, "fresh");
}

#[test]
fn send_transaction_prefers_lowest_p50() {
    let pool = test_pool(&["fast_tx", "slow_tx"]);
    pool.stats.record("fast_tx", "sendTransaction", 50.0, true);
    pool.stats.record("slow_tx", "sendTransaction", 400.0, true);
    let chosen = select_for_method(&pool, "sendTransaction", &[]).unwrap();
    assert_eq!(chosen.name, "fast_tx");
}

#[test]
fn send_transaction_falls_back_to_generic_scoring_without_history() {
    let pool = test_pool(&["a", "b"]);
    // no sendTransaction samples, but per-method EWMA exists for other data
    pool.stats.record("a", "sendTransaction", 10.0, false); // failed samples don't populate p50
    let chosen = select_for_method(&pool, "sendTransaction", &[]);
    assert!(chosen.is_some()); // falls through to warmup default
}

#[test]
fn hysteresis_keeps_close_leader() {
    let pool = test_pool_with_margin(&["a", "b"], 0.2); // 20% margin
                                                        // Install 'a' as the incumbent leader with a solid score.
    pool.stats.record("a", "getSlot", 100.0, true);
    let first = select_for_method(&pool, "getSlot", &[]).unwrap();
    assert_eq!(first.name, "a");

    // Now 'b' shows up slightly faster but not past the margin.
    pool.stats.record("b", "getSlot", 90.0, true);
    let second = select_for_method(&pool, "getSlot", &[]).unwrap();
    assert_eq!(second.name, "a", "leader should hold within margin");
}

#[test]
fn hysteresis_lets_decisive_challenger_through() {
    let pool = test_pool_with_margin(&["a", "b"], 0.1); // 10% margin
                                                        // 'a' at 100ms wins the warmup shootout (score 0.65 > b's warmup 0.5)
    pool.stats.record("a", "getSlot", 100.0, true);
    let first = select_for_method(&pool, "getSlot", &[]).unwrap();
    assert_eq!(first.name, "a");

    // 'b' is dramatically faster — should dethrone.
    pool.stats.record("b", "getSlot", 10.0, true);
    let second = select_for_method(&pool, "getSlot", &[]).unwrap();
    assert_eq!(second.name, "b");
}

#[test]
fn hysteresis_skips_dropped_leader() {
    let pool = test_pool_with_margin(&["a", "b"], 0.5); // very sticky
    pool.stats.record("a", "getSlot", 50.0, true);
    pool.stats.record("b", "getSlot", 100.0, true);
    // Lock in 'a' as leader.
    let first = select_for_method(&pool, "getSlot", &[]).unwrap();
    assert_eq!(first.name, "a");
    // Kill 'a' — router should pick 'b' despite large margin.
    pool.upstreams[0].trip(Duration::from_secs(60));
    let second = select_for_method(&pool, "getSlot", &[]).unwrap();
    assert_eq!(second.name, "b");
}

#[test]
fn hysteresis_per_method_leaders_are_independent() {
    let pool = test_pool_with_margin(&["a", "b"], 0.1);
    // 'a' wins getSlot, 'b' wins getAccountInfo.
    pool.stats.record("a", "getSlot", 20.0, true);
    pool.stats.record("b", "getSlot", 500.0, true);
    pool.stats.record("a", "getAccountInfo", 500.0, true);
    pool.stats.record("b", "getAccountInfo", 20.0, true);

    assert_eq!(select_for_method(&pool, "getSlot", &[]).unwrap().name, "a");
    assert_eq!(
        select_for_method(&pool, "getAccountInfo", &[])
            .unwrap()
            .name,
        "b"
    );
}
