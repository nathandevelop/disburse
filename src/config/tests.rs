use super::*;

const FULL_YAML: &str = r#"
listen: "0.0.0.0:8899"
metrics_listen: "0.0.0.0:9090"
upstreams:
  - name: a
    url: "http://a.example/"
    headers:
      x-api-key: secret
  - name: b
    url: "http://b.example/"
routing:
  latency_weight: 0.7
  success_weight: 0.3
  ewma_alpha: 0.2
health:
  check_interval_secs: 3
  max_slot_lag: 10
  max_blockhash_age_slots: 100
  circuit_breaker_error_rate: 0.5
  circuit_breaker_cooldown_secs: 30
retries:
  max_attempts: 3
  retry_on_different_upstream: true
request_timeout:
  default_ms: 8000
  per_method:
    getProgramAccounts: 30000
    getSlot: 2000
max_request_body_bytes: 1048576
"#;

#[test]
fn parses_full_config() {
    let cfg: Config = serde_yaml::from_str(FULL_YAML).unwrap();
    assert_eq!(cfg.listen, "0.0.0.0:8899");
    assert_eq!(cfg.upstreams.len(), 2);
    assert_eq!(
        cfg.upstreams[0].headers.get("x-api-key"),
        Some(&"secret".to_string())
    );
    assert!(cfg.upstreams[1].headers.is_empty());
    assert_eq!(cfg.max_request_body_bytes, 1048576);
}

#[test]
fn per_method_timeout_lookup() {
    let cfg: Config = serde_yaml::from_str(FULL_YAML).unwrap();
    assert_eq!(cfg.request_timeout.for_method("getSlot"), 2000);
    assert_eq!(cfg.request_timeout.for_method("getProgramAccounts"), 30000);
    assert_eq!(cfg.request_timeout.for_method("getSignatureStatuses"), 8000); // default
}

#[test]
fn defaults_applied_when_optional_fields_missing() {
    let yaml = r#"
listen: "0.0.0.0:8899"
metrics_listen: "0.0.0.0:9090"
upstreams:
  - { name: a, url: "http://a/" }
routing: { latency_weight: 0.7, success_weight: 0.3, ewma_alpha: 0.2 }
health: { check_interval_secs: 3, max_slot_lag: 10, max_blockhash_age_slots: 100, circuit_breaker_error_rate: 0.5, circuit_breaker_cooldown_secs: 30 }
retries: { max_attempts: 3, retry_on_different_upstream: true }
"#;
    let cfg: Config = serde_yaml::from_str(yaml).unwrap();
    assert_eq!(cfg.request_timeout.default_ms, 10_000);
    assert!(cfg.request_timeout.per_method.is_empty());
    assert_eq!(cfg.max_request_body_bytes, 2 * 1024 * 1024);
    assert!(cfg.upstreams[0].headers.is_empty());
}

#[test]
fn example_config_file_parses() {
    // Verifies the shipped config.example.yaml stays in sync with the Config struct.
    let path = std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("config.example.yaml");
    if path.exists() {
        let cfg = Config::load(&path).expect("example config loads");
        assert!(!cfg.upstreams.is_empty());
    }
}
