//! End-to-end integration tests. Each test spins up one or more mock upstreams
//! (real axum servers on random localhost ports), builds a real disburse app
//! pointing at them, and fires HTTP requests through to assert behavior.

use axum::{extract::State, routing::post, Json, Router};
use serde_json::{json, Value};
use disburse::config::{HealthConfig, RetryConfig, RoutingConfig, TimeoutConfig, UpstreamConfig};
use disburse::{app, build_state, Config};
use std::collections::HashMap;
use std::net::SocketAddr;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::Arc;
use tokio::net::TcpListener;

// ---------------------------------------------------------------------------
// Helpers
// ---------------------------------------------------------------------------

fn base_config(upstreams: Vec<(String, String)>) -> Config {
    Config {
        listen: "127.0.0.1:0".into(),
        metrics_listen: "127.0.0.1:0".into(),
        upstreams: upstreams
            .into_iter()
            .map(|(name, url)| UpstreamConfig {
                name,
                url,
                headers: HashMap::new(),
            })
            .collect(),
        routing: RoutingConfig {
            latency_weight: 0.7,
            success_weight: 0.3,
            ewma_alpha: 0.2,
            hysteresis_margin: 0.0,
        },
        health: HealthConfig {
            check_interval_secs: 60,
            max_slot_lag: 10,
            max_blockhash_age_slots: 100,
            circuit_breaker_error_rate: 0.5,
            circuit_breaker_cooldown_secs: 30,
            rolling_window_secs: 60,
            blockhash_warmup_secs: 0,
            blockhash_cache_size: 512,
        },
        retries: RetryConfig {
            max_attempts: 3,
            retry_on_different_upstream: true,
            deadline_ms: 5_000,
        },
        request_timeout: TimeoutConfig::default(),
        max_request_body_bytes: 2 * 1024 * 1024,
        max_response_body_bytes: 16 * 1024 * 1024,
        max_concurrent_per_upstream: 256,
    }
}

async fn spawn_disburse(upstreams: Vec<(String, String)>) -> SocketAddr {
    let (addr, _state) = spawn_disburse_with_state(upstreams).await;
    addr
}

async fn spawn_disburse_with_state(
    upstreams: Vec<(String, String)>,
) -> (SocketAddr, disburse::AppState) {
    let cfg = base_config(upstreams);
    let state = build_state(&cfg);
    let router = app(state.clone());
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    tokio::spawn(async move {
        axum::serve(listener, router).await.ok();
    });
    (addr, state)
}

/// A mock upstream that always returns the given JSON body.
async fn spawn_constant_mock(body: Value) -> SocketAddr {
    #[derive(Clone)]
    struct S(Arc<Value>);
    async fn handler(State(s): State<S>, Json(_req): Json<Value>) -> Json<Value> {
        Json((*s.0).clone())
    }
    let state = S(Arc::new(body));
    let router: Router = Router::new().route("/", post(handler)).with_state(state);
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    tokio::spawn(async move {
        axum::serve(listener, router).await.ok();
    });
    addr
}

/// A mock upstream that always returns HTTP 500 with a plain-text body, plus
/// a counter of how many times it was hit (for verifying retries).
async fn spawn_error_mock() -> (SocketAddr, Arc<AtomicU64>) {
    let counter = Arc::new(AtomicU64::new(0));
    #[derive(Clone)]
    struct S(Arc<AtomicU64>);
    async fn handler(
        State(s): State<S>,
        Json(_req): Json<Value>,
    ) -> (axum::http::StatusCode, &'static str) {
        s.0.fetch_add(1, Ordering::Relaxed);
        (axum::http::StatusCode::INTERNAL_SERVER_ERROR, "broken")
    }
    let router: Router = Router::new()
        .route("/", post(handler))
        .with_state(S(counter.clone()));
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    tokio::spawn(async move {
        axum::serve(listener, router).await.ok();
    });
    (addr, counter)
}

fn upstream_url(addr: SocketAddr) -> String {
    format!("http://{addr}/")
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[tokio::test]
async fn forwards_getslot_to_single_upstream() {
    let mock = spawn_constant_mock(json!({"jsonrpc":"2.0","id":1,"result":42})).await;
    let proxy = spawn_disburse(vec![("mock".into(), upstream_url(mock))]).await;

    let resp: Value = reqwest::Client::new()
        .post(format!("http://{proxy}/"))
        .json(&json!({"jsonrpc":"2.0","id":1,"method":"getSlot"}))
        .send()
        .await
        .unwrap()
        .json()
        .await
        .unwrap();
    assert_eq!(resp["result"], 42);
    assert_eq!(resp["id"], 1);
}

#[tokio::test]
async fn batch_request_returns_array() {
    let mock = spawn_constant_mock(json!({"jsonrpc":"2.0","id":1,"result":"ok"})).await;
    let proxy = spawn_disburse(vec![("mock".into(), upstream_url(mock))]).await;

    let resp: Value = reqwest::Client::new()
        .post(format!("http://{proxy}/"))
        .json(&json!([
            {"jsonrpc":"2.0","id":1,"method":"getSlot"},
            {"jsonrpc":"2.0","id":2,"method":"getVersion"},
        ]))
        .send()
        .await
        .unwrap()
        .json()
        .await
        .unwrap();
    assert!(resp.is_array(), "batch response must be array");
    assert_eq!(resp.as_array().unwrap().len(), 2);
}

#[tokio::test]
async fn failing_upstream_triggers_retry_to_healthy_one() {
    let (broken, broken_hits) = spawn_error_mock().await;
    let good = spawn_constant_mock(json!({"jsonrpc":"2.0","id":1,"result":"rescued"})).await;

    let (proxy, state) = spawn_disburse_with_state(vec![
        ("broken".into(), upstream_url(broken)),
        ("good".into(), upstream_url(good)),
    ])
    .await;

    // Pre-seed stats so 'broken' wins the first routing decision deterministically.
    // We want to verify that when it fails, the retry lands on 'good'.
    state.pool.stats.record("broken", "getSlot", 5.0, true);
    state.pool.stats.record("good", "getSlot", 500.0, true);

    let resp: Value = reqwest::Client::new()
        .post(format!("http://{proxy}/"))
        .json(&json!({"jsonrpc":"2.0","id":1,"method":"getSlot"}))
        .send()
        .await
        .unwrap()
        .json()
        .await
        .unwrap();
    assert_eq!(resp["result"], "rescued");
    assert!(
        broken_hits.load(Ordering::Relaxed) >= 1,
        "broken should have been tried before failover"
    );
}

#[tokio::test]
async fn all_upstreams_failing_returns_exhausted_error() {
    let (a, _) = spawn_error_mock().await;
    let (b, _) = spawn_error_mock().await;
    let proxy = spawn_disburse(vec![
        ("a".into(), upstream_url(a)),
        ("b".into(), upstream_url(b)),
    ])
    .await;

    let resp: Value = reqwest::Client::new()
        .post(format!("http://{proxy}/"))
        .json(&json!({"jsonrpc":"2.0","id":9,"method":"getSlot"}))
        .send()
        .await
        .unwrap()
        .json()
        .await
        .unwrap();
    // Upstream 500s are caught as errors; after retries exhaust, client sees
    // an error Value (shape: last upstream's body, not parseable as JSON here).
    assert!(
        resp.get("error").is_some() || resp.get("result").is_none(),
        "expected error-shaped response, got: {resp}"
    );
}

#[tokio::test]
async fn client_trace_id_is_echoed_in_response_header() {
    let mock = spawn_constant_mock(json!({"jsonrpc":"2.0","id":1,"result":1})).await;
    let proxy = spawn_disburse(vec![("mock".into(), upstream_url(mock))]).await;

    let resp = reqwest::Client::new()
        .post(format!("http://{proxy}/"))
        .header("x-trace-id", "client-provided-xyz")
        .json(&json!({"jsonrpc":"2.0","id":1,"method":"getSlot"}))
        .send()
        .await
        .unwrap();
    assert_eq!(
        resp.headers().get("x-trace-id").unwrap().to_str().unwrap(),
        "client-provided-xyz"
    );
}

#[tokio::test]
async fn livez_always_returns_ok() {
    let mock = spawn_constant_mock(json!({"jsonrpc":"2.0","id":1,"result":1})).await;
    let proxy = spawn_disburse(vec![("mock".into(), upstream_url(mock))]).await;

    let resp = reqwest::Client::new()
        .get(format!("http://{proxy}/livez"))
        .send()
        .await
        .unwrap();
    assert!(resp.status().is_success());
}
