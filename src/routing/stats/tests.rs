use super::*;

#[test]
fn first_sample_seeds_ewma_directly() {
    let s = StatsStore::new(0.2, 60);
    s.record("u", "m", 150.0, true);
    let got = s.get("u", "m").unwrap();
    assert_eq!(got.ewma_latency_ms, 150.0);
    assert_eq!(got.ewma_success, 1.0);
    assert_eq!(got.samples, 1);
}

#[test]
fn ewma_converges_with_repeated_samples() {
    let s = StatsStore::new(0.5, 60);
    s.record("u", "m", 100.0, true);
    s.record("u", "m", 200.0, true);
    // alpha=0.5: ewma = 0.5*200 + 0.5*100 = 150
    let got = s.get("u", "m").unwrap();
    assert!((got.ewma_latency_ms - 150.0).abs() < 1e-9);
}

#[test]
fn success_rate_decays_with_failures() {
    let s = StatsStore::new(0.5, 60);
    s.record("u", "m", 10.0, true);
    s.record("u", "m", 10.0, false);
    // 0.5*0 + 0.5*1 = 0.5
    let got = s.get("u", "m").unwrap();
    assert!((got.ewma_success - 0.5).abs() < 1e-9);
}

#[test]
fn error_rate_counts_recent_failures() {
    let s = StatsStore::new(0.2, 60);
    s.record("u", "m", 10.0, true);
    s.record("u", "m", 10.0, false);
    s.record("u", "m", 10.0, false);
    s.record("u", "m", 10.0, true);
    assert!((s.error_rate("u") - 0.5).abs() < 1e-9);
}

#[test]
fn error_rate_zero_for_unknown_upstream() {
    let s = StatsStore::new(0.2, 60);
    assert_eq!(s.error_rate("unknown"), 0.0);
}

#[test]
fn send_tx_p50_returns_median() {
    let s = StatsStore::new(0.2, 60);
    s.record("u", "sendTransaction", 100.0, true);
    s.record("u", "sendTransaction", 300.0, true);
    s.record("u", "sendTransaction", 200.0, true);
    assert_eq!(s.send_tx_p50("u"), Some(200.0));
}

#[test]
fn send_tx_p50_only_tracks_successes() {
    let s = StatsStore::new(0.2, 60);
    s.record("u", "sendTransaction", 999.0, false);
    assert_eq!(s.send_tx_p50("u"), None);
}

#[test]
fn send_tx_p50_none_for_other_methods() {
    let s = StatsStore::new(0.2, 60);
    s.record("u", "getSlot", 100.0, true);
    assert_eq!(s.send_tx_p50("u"), None);
}

#[test]
fn get_returns_none_for_unknown_pair() {
    let s = StatsStore::new(0.2, 60);
    assert!(s.get("u", "m").is_none());
}
