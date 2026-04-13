//! JSON-RPC request handling: forwarding, retries, sendTransaction blockhash checks.

use super::server::AppState;
use crate::error::{jsonrpc_error, redact_url, ErrorCode};
use crate::routing::select_and_admit;
use crate::solana::{decode_tx, extract_blockhash};
use crate::upstream::Upstream;
use axum::{
    extract::State,
    http::{HeaderMap, HeaderValue},
    response::{IntoResponse, Response},
    Json,
};
use bytes::{Bytes, BytesMut};
use futures::StreamExt;
use serde_json::{json, Value};
use std::sync::atomic::Ordering;
use std::sync::Arc;
use std::time::{Duration, Instant};
use tracing::{debug, warn, Instrument};
use uuid::Uuid;

pub async fn handle_rpc(
    State(state): State<AppState>,
    headers: HeaderMap,
    Json(payload): Json<Value>,
) -> Response {
    // Honor client-provided trace IDs so solmux doesn't break existing distributed
    // tracing. Falls back to a fresh UUID.
    let trace_id = headers
        .get("x-trace-id")
        .or_else(|| headers.get("x-request-id"))
        .and_then(|v| v.to_str().ok())
        .filter(|s| !s.is_empty() && s.len() <= 128)
        .map(|s| s.to_string())
        .unwrap_or_else(|| Uuid::new_v4().to_string());

    let span = tracing::info_span!("rpc", trace_id = %trace_id);

    let mut resp = async {
        if let Some(arr) = payload.as_array() {
            // Batch: fan out per-call in parallel. Each sub-call routes independently
            // via per-method scoring, so one client batch may hit multiple upstreams.
            let futs = arr
                .iter()
                .map(|req| handle_single_parsed(&state, req.clone()));
            let results = futures::future::join_all(futs).await;
            Json(Value::Array(results)).into_response()
        } else {
            // Fast path: pass upstream bytes through without re-parsing.
            handle_single_raw(&state, payload).await
        }
    }
    .instrument(span)
    .await;

    if let Ok(val) = HeaderValue::from_str(&trace_id) {
        resp.headers_mut().insert("x-trace-id", val);
    }
    resp
}

// ---------------------------------------------------------------------------
// Handlers. Both are thin wrappers around `forward_core` — they only differ in
// how they package a successful response (raw axum Response vs parsed Value).
// ---------------------------------------------------------------------------

async fn handle_single_raw(state: &AppState, req: Value) -> Response {
    let id = req.get("id").cloned().unwrap_or(Value::Null);
    let method = match req.get("method").and_then(|m| m.as_str()) {
        Some(m) => m.to_string(),
        None => {
            return Json(jsonrpc_error(
                id,
                ErrorCode::InvalidRequest,
                "missing method",
            ))
            .into_response();
        }
    };

    // DIFFERENTIATOR 3: blockhash freshness check for sendTransaction.
    if method == "sendTransaction" {
        if let Some(reject) = check_blockhash_freshness(state, &req, &id).await {
            return Json(reject).into_response();
        }
    }

    let deadline = Duration::from_millis(state.pool.config.retries.deadline_ms);
    match tokio::time::timeout(deadline, forward_core(state, &req, &id, &method)).await {
        Ok(ForwardResult::Ok {
            bytes,
            content_type,
        }) => {
            let mut resp = Response::new(axum::body::Body::from(bytes));
            let ct = content_type.unwrap_or_else(|| HeaderValue::from_static("application/json"));
            resp.headers_mut().insert("content-type", ct);
            resp
        }
        Ok(ForwardResult::Err(v)) => Json(v).into_response(),
        Err(_) => {
            warn!("request exceeded global deadline");
            Json(jsonrpc_error(
                id,
                ErrorCode::DeadlineExceeded,
                "request deadline exceeded",
            ))
            .into_response()
        }
    }
}

async fn handle_single_parsed(state: &AppState, req: Value) -> Value {
    let id = req.get("id").cloned().unwrap_or(Value::Null);
    let method = match req.get("method").and_then(|m| m.as_str()) {
        Some(m) => m.to_string(),
        None => return jsonrpc_error(id, ErrorCode::InvalidRequest, "missing method"),
    };

    if method == "sendTransaction" {
        if let Some(reject) = check_blockhash_freshness(state, &req, &id).await {
            return reject;
        }
    }

    let deadline = Duration::from_millis(state.pool.config.retries.deadline_ms);
    match tokio::time::timeout(deadline, forward_core(state, &req, &id, &method)).await {
        Ok(ForwardResult::Ok { bytes, .. }) => serde_json::from_slice::<Value>(&bytes)
            .unwrap_or_else(|_| {
                jsonrpc_error(
                    id,
                    ErrorCode::UpstreamBadResponse,
                    "upstream returned non-JSON",
                )
            }),
        Ok(ForwardResult::Err(v)) => v,
        Err(_) => jsonrpc_error(id, ErrorCode::DeadlineExceeded, "request deadline exceeded"),
    }
}

// ---------------------------------------------------------------------------
// Shared forwarding core: handles selection, retries, stats, metrics,
// half-open probe settlement, blockhash-cache harvest.
// ---------------------------------------------------------------------------

enum ForwardResult {
    Ok {
        bytes: Bytes,
        content_type: Option<HeaderValue>,
    },
    /// Retries exhausted — returns the last JSON-RPC error response we have.
    Err(Value),
}

async fn forward_core(state: &AppState, req: &Value, id: &Value, method: &str) -> ForwardResult {
    let cfg = &state.pool.config;
    let cooldown = Duration::from_secs(cfg.health.circuit_breaker_cooldown_secs);

    let req_bytes = match serde_json::to_vec(req) {
        Ok(b) => b,
        Err(_) => {
            return ForwardResult::Err(jsonrpc_error(
                id.clone(),
                ErrorCode::Internal,
                "failed to serialize request",
            ));
        }
    };

    let mut excluded: Vec<String> = Vec::new();
    let mut last_error: Option<Value> = None;

    for attempt in 0..cfg.retries.max_attempts {
        let (upstream, _permit, is_probe) = match select_and_admit(&state.pool, method, &excluded) {
            Some(x) => x,
            None => break,
        };
        state
            .metrics
            .record_routing_decision(&upstream.name, method);

        let outcome = fire_once(state, &upstream, method, &req_bytes, id).await;
        let (success, ok_output, error_value) = match outcome {
            AttemptOutcome::Ok {
                bytes,
                content_type,
            } => (true, Some((bytes, content_type)), None),
            AttemptOutcome::Err(v) => (false, None, Some(v)),
        };

        if is_probe {
            upstream.settle_probe(success, cooldown);
        }

        if let Some((bytes, content_type)) = ok_output {
            debug!(
                upstream = %upstream.name,
                method = %method,
                attempt = attempt,
                "forwarded ok"
            );
            return ForwardResult::Ok {
                bytes,
                content_type,
            };
        }

        last_error = error_value;

        if cfg.retries.retry_on_different_upstream {
            excluded.push(upstream.name.clone());
        }
    }

    ForwardResult::Err(last_error.unwrap_or_else(|| {
        jsonrpc_error(
            id.clone(),
            ErrorCode::AllUpstreamsExhausted,
            "no upstream available",
        )
    }))
}

/// The outcome of one attempt against one upstream. On success carries the
/// raw response bytes; on failure carries the JSON-RPC error Value to surface
/// if this turns out to be the last attempt.
enum AttemptOutcome {
    Ok {
        bytes: Bytes,
        content_type: Option<HeaderValue>,
    },
    Err(Value),
}

/// Fire one request. Records stats + metrics before returning. Does not touch
/// circuit-breaker state — that's the caller's job after deciding whether this
/// was a probe.
async fn fire_once(
    state: &AppState,
    upstream: &Arc<Upstream>,
    method: &str,
    req_bytes: &[u8],
    id: &Value,
) -> AttemptOutcome {
    let timeout_ms = state.pool.config.request_timeout.for_method(method);
    let start = Instant::now();
    let send_res = upstream
        .client
        .post(&upstream.url)
        .headers(upstream.headers.clone())
        .header("content-type", "application/json")
        .timeout(Duration::from_millis(timeout_ms))
        .body(req_bytes.to_vec())
        .send()
        .await;

    let resp = match send_res {
        Ok(r) => r,
        Err(e) => {
            let latency = start.elapsed().as_secs_f64() * 1000.0;
            record_failure(state, &upstream.name, method, latency);
            let (code, msg) = classify_reqwest_error(&e, upstream);
            return AttemptOutcome::Err(jsonrpc_error(id.clone(), code, &msg));
        }
    };

    let max_body = state.pool.config.max_response_body_bytes;
    let (status_code, content_type, bytes) = match read_bytes_limited(resp, max_body).await {
        Ok(triple) => triple,
        Err(ReadError::TooLarge) => {
            let latency = start.elapsed().as_secs_f64() * 1000.0;
            record_failure(state, &upstream.name, method, latency);
            warn!(
                upstream = %upstream.name,
                method = %method,
                "upstream response exceeded max_response_body_bytes"
            );
            return AttemptOutcome::Err(jsonrpc_error(
                id.clone(),
                ErrorCode::ResponseTooLarge,
                "upstream response exceeded configured size limit",
            ));
        }
        Err(ReadError::Read(msg)) => {
            let latency = start.elapsed().as_secs_f64() * 1000.0;
            record_failure(state, &upstream.name, method, latency);
            return AttemptOutcome::Err(jsonrpc_error(
                id.clone(),
                ErrorCode::UpstreamBadResponse,
                &format!("upstream read failed: {msg}"),
            ));
        }
    };

    let latency = start.elapsed().as_secs_f64() * 1000.0;
    let has_jsonrpc_error = contains_jsonrpc_error(&bytes);
    let success = status_code.is_success() && !has_jsonrpc_error;
    state
        .pool
        .stats
        .record(&upstream.name, method, latency, success);
    let status_str = if success { "ok" } else { "error" };
    state
        .metrics
        .record_request(&upstream.name, method, status_str, latency);

    if success {
        // Opportunistic blockhash harvest. Only parses on the specific method so
        // the fast path stays bytes-only for everything else.
        if method == "getLatestBlockhash" {
            if let Ok(v) = serde_json::from_slice::<Value>(&bytes) {
                if let Some(bh) = v
                    .pointer("/result/value/blockhash")
                    .and_then(|b| b.as_str())
                {
                    let slot = v
                        .pointer("/result/context/slot")
                        .and_then(|s| s.as_u64())
                        .unwrap_or_else(|| state.pool.tip_slot.load(Ordering::Relaxed));
                    state.blockhash_cache.insert(bh.to_string(), slot);
                }
            }
        }
        AttemptOutcome::Ok {
            bytes,
            content_type,
        }
    } else {
        warn!(
            upstream = %upstream.name,
            method = %method,
            status = %status_code,
            latency_ms = latency,
            "upstream returned error"
        );
        let err_value = serde_json::from_slice::<Value>(&bytes).unwrap_or_else(|_| {
            jsonrpc_error(
                id.clone(),
                ErrorCode::UpstreamBadResponse,
                "upstream returned non-JSON",
            )
        });
        AttemptOutcome::Err(err_value)
    }
}

fn record_failure(state: &AppState, upstream_name: &str, method: &str, latency_ms: f64) {
    state
        .pool
        .stats
        .record(upstream_name, method, latency_ms, false);
    state
        .metrics
        .record_request(upstream_name, method, "error", latency_ms);
}

// ---------------------------------------------------------------------------
// Shared helpers.
// ---------------------------------------------------------------------------

/// Cheap byte scan for a JSON-RPC error. Lets us skip full parsing on the hot path.
fn contains_jsonrpc_error(bytes: &[u8]) -> bool {
    bytes.windows(8).any(|w| w == b"\"error\":")
}

/// Read an upstream response body with a hard cap. Streams chunks and aborts
/// as soon as `max` is exceeded, without buffering the whole body first.
async fn read_bytes_limited(
    resp: reqwest::Response,
    max: usize,
) -> Result<(reqwest::StatusCode, Option<HeaderValue>, Bytes), ReadError> {
    let status = resp.status();
    let content_type = resp.headers().get("content-type").cloned();
    if let Some(len) = resp.content_length() {
        if len as usize > max {
            return Err(ReadError::TooLarge);
        }
    }
    let mut buf = BytesMut::with_capacity(1024);
    let mut stream = resp.bytes_stream();
    while let Some(chunk) = stream.next().await {
        let chunk = chunk.map_err(|e| ReadError::Read(e.to_string()))?;
        if buf.len() + chunk.len() > max {
            return Err(ReadError::TooLarge);
        }
        buf.extend_from_slice(&chunk);
    }
    Ok((status, content_type, buf.freeze()))
}

enum ReadError {
    TooLarge,
    Read(String),
}

fn classify_reqwest_error(e: &reqwest::Error, upstream: &Upstream) -> (ErrorCode, String) {
    let redacted = redact_url(&upstream.url);
    if e.is_timeout() {
        (
            ErrorCode::UpstreamTimeout,
            format!("upstream {} timed out", upstream.name),
        )
    } else if e.is_connect() {
        (
            ErrorCode::UpstreamUnreachable,
            format!("upstream {} unreachable ({redacted})", upstream.name),
        )
    } else {
        (
            ErrorCode::Internal,
            format!(
                "upstream {} failed: {}",
                upstream.name,
                sanitize(&e.to_string(), &upstream.url)
            ),
        )
    }
}

/// Strip a raw upstream URL out of an error string so we don't log API keys.
fn sanitize(msg: &str, url: &str) -> String {
    if msg.contains(url) {
        msg.replace(url, &redact_url(url))
    } else {
        msg.to_string()
    }
}

// ---------------------------------------------------------------------------
// Blockhash freshness: decode the tx enough to extract recent_blockhash, then
// verify it against the local cache (with isBlockhashValid fallback).
// ---------------------------------------------------------------------------

/// Returns Some(error_response) when the call should be rejected, None to continue.
async fn check_blockhash_freshness(state: &AppState, req: &Value, id: &Value) -> Option<Value> {
    if !state.blockhash_cache.is_warmed_up() {
        return None;
    }

    let params = req.get("params").and_then(|p| p.as_array())?;
    let data = params.first().and_then(|d| d.as_str())?;
    let encoding = params
        .get(1)
        .and_then(|o| o.get("encoding"))
        .and_then(|e| e.as_str())
        .unwrap_or("base58");
    let tx_bytes = decode_tx(data, encoding)?;
    let bh = extract_blockhash(&tx_bytes)?;

    let tip = state.pool.tip_slot.load(Ordering::Relaxed);
    let max_age = state.pool.config.health.max_blockhash_age_slots;

    let stale = match state.blockhash_cache.get_slot(&bh) {
        Some(seen_slot) => tip.saturating_sub(seen_slot) > max_age,
        None => match verify_blockhash(state, &bh).await {
            Some(true) => {
                state.blockhash_cache.insert(bh.clone(), tip);
                false
            }
            Some(false) => true,
            None => true,
        },
    };

    if stale {
        state
            .metrics
            .blockhash_rejections
            .fetch_add(1, Ordering::Relaxed);
        warn!(
            blockhash = %bh,
            tip = tip,
            "rejecting sendTransaction: stale or unknown blockhash"
        );
        Some(jsonrpc_error(
            id.clone(),
            ErrorCode::BlockhashStale,
            "blockhash too old or unknown, would likely fail on chain",
        ))
    } else {
        None
    }
}

async fn verify_blockhash(state: &AppState, blockhash: &str) -> Option<bool> {
    let healthy: Vec<Arc<Upstream>> = state
        .pool
        .upstreams
        .iter()
        .filter(|u| !u.is_dropped())
        .cloned()
        .collect();
    if healthy.is_empty() {
        return None;
    }
    let body = json!({
        "jsonrpc":"2.0","id":1,
        "method":"isBlockhashValid",
        "params":[blockhash,{"commitment":"processed"}]
    });
    let timeout_ms = state
        .pool
        .config
        .request_timeout
        .for_method("isBlockhashValid");
    for u in healthy {
        let r = u
            .client
            .post(&u.url)
            .headers(u.headers.clone())
            .timeout(Duration::from_millis(timeout_ms))
            .json(&body)
            .send()
            .await;
        if let Ok(resp) = r {
            if let Ok(v) = resp.json::<Value>().await {
                if let Some(b) = v.pointer("/result/value").and_then(|x| x.as_bool()) {
                    return Some(b);
                }
            }
        }
    }
    None
}
