//! HTTP server wiring: axum routers, shared `AppState`, and the `/health` endpoint.

use super::metrics::{handle_metrics, Metrics};
use super::rpc::handle_rpc;
use crate::error::{jsonrpc_error, ErrorCode};
use crate::routing::UpstreamPool;
use crate::solana::BlockhashCache;
use axum::{
    extract::{DefaultBodyLimit, State},
    http::StatusCode,
    response::{IntoResponse, Response},
    routing::{get, post},
    Json, Router,
};
use serde_json::{json, Value};
use std::any::Any;
use std::sync::atomic::Ordering;
use std::sync::Arc;
use tower_http::catch_panic::CatchPanicLayer;
use tracing::error;

#[derive(Clone)]
pub struct AppState {
    pub pool: Arc<UpstreamPool>,
    pub blockhash_cache: Arc<BlockhashCache>,
    pub metrics: Arc<Metrics>,
}

pub fn app(state: AppState) -> Router {
    let limit = state.pool.config.max_request_body_bytes;
    let metrics_for_panic = state.metrics.clone();
    Router::new()
        .route("/", post(handle_rpc))
        .route("/health", get(handle_health))
        .route("/livez", get(handle_livez))
        .route("/readyz", get(handle_readyz))
        .layer(DefaultBodyLimit::max(limit))
        .layer(CatchPanicLayer::custom(move |err| {
            metrics_for_panic.panics.fetch_add(1, Ordering::Relaxed);
            panic_response(err)
        }))
        .with_state(state)
}

fn panic_response(err: Box<dyn Any + Send + 'static>) -> Response {
    let details = if let Some(s) = err.downcast_ref::<&str>() {
        s.to_string()
    } else if let Some(s) = err.downcast_ref::<String>() {
        s.clone()
    } else {
        "unknown panic".to_string()
    };
    error!(panic = %details, "handler panicked");
    let body = jsonrpc_error(
        Value::Null,
        ErrorCode::Internal,
        "internal server error — a handler panicked",
    );
    (StatusCode::INTERNAL_SERVER_ERROR, Json(body)).into_response()
}

pub fn metrics_app(state: AppState) -> Router {
    Router::new()
        .route("/metrics", get(handle_metrics))
        .with_state(state)
}

/// Liveness: the process is running and able to respond. Never flips to
/// unhealthy based on upstream state. k8s will restart the pod on `/livez` failure.
async fn handle_livez() -> &'static str {
    "ok"
}

/// Readiness: this instance should receive traffic. True only when at least
/// one upstream is healthy AND the blockhash freshness cache has warmed up,
/// so sendTransaction won't false-reject. k8s pulls the pod from rotation
/// on `/readyz` failure but does NOT restart it.
async fn handle_readyz(State(state): State<AppState>) -> Response {
    let any_healthy = state.pool.upstreams.iter().any(|u| !u.is_dropped());
    let warm = state.blockhash_cache.is_warmed_up();
    if any_healthy && warm {
        (StatusCode::OK, "ready").into_response()
    } else {
        let reason = if !any_healthy {
            "no healthy upstream"
        } else {
            "blockhash cache warming up"
        };
        (StatusCode::SERVICE_UNAVAILABLE, reason).into_response()
    }
}

async fn handle_health(State(state): State<AppState>) -> Json<Value> {
    let tip = state.pool.tip_slot.load(Ordering::Relaxed);
    let mut upstreams = Vec::new();
    for u in &state.pool.upstreams {
        let slot = u.last_slot.load(Ordering::Relaxed);
        let slot_lag = tip.saturating_sub(slot);
        let err_rate = state.pool.stats.error_rate(&u.name);
        let ewma = state
            .pool
            .stats
            .get(&u.name, "getSlot")
            .map(|s| s.ewma_latency_ms)
            .unwrap_or(0.0);
        upstreams.push(json!({
            "name": u.name,
            "healthy": !u.is_dropped(),
            "slot": slot,
            "slot_lag": slot_lag,
            "ewma_latency_ms": ewma,
            "error_rate": err_rate,
        }));
    }
    Json(json!({ "tip": tip, "upstreams": upstreams }))
}
