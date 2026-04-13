//! Prometheus metrics collection and text-format rendering.

use super::server::AppState;
use axum::{extract::State, http::StatusCode, response::IntoResponse};
use dashmap::DashMap;
use std::sync::atomic::{AtomicU64, Ordering};

pub const BUCKETS_MS: [f64; 10] = [
    1.0, 5.0, 10.0, 25.0, 50.0, 100.0, 250.0, 500.0, 1000.0, 2500.0,
];

pub struct Metrics {
    pub requests: DashMap<(String, String, String), AtomicU64>,
    pub duration_buckets: DashMap<(String, String), [AtomicU64; 11]>,
    pub duration_sum: DashMap<(String, String), AtomicU64>,
    pub duration_count: DashMap<(String, String), AtomicU64>,
    pub blockhash_rejections: AtomicU64,
    pub routing_decisions: DashMap<(String, String), AtomicU64>,
    pub panics: AtomicU64,
}

impl Metrics {
    pub fn new() -> Self {
        Self {
            requests: DashMap::new(),
            duration_buckets: DashMap::new(),
            duration_sum: DashMap::new(),
            duration_count: DashMap::new(),
            blockhash_rejections: AtomicU64::new(0),
            routing_decisions: DashMap::new(),
            panics: AtomicU64::new(0),
        }
    }

    pub fn record_routing_decision(&self, upstream: &str, method: &str) {
        let key = (upstream.to_string(), method.to_string());
        self.routing_decisions
            .entry(key)
            .or_insert_with(|| AtomicU64::new(0))
            .fetch_add(1, Ordering::Relaxed);
    }

    pub fn record_request(&self, upstream: &str, method: &str, status: &str, latency_ms: f64) {
        let key = (upstream.to_string(), method.to_string(), status.to_string());
        self.requests
            .entry(key)
            .or_insert_with(|| AtomicU64::new(0))
            .fetch_add(1, Ordering::Relaxed);

        let k2 = (upstream.to_string(), method.to_string());
        {
            let entry = self
                .duration_buckets
                .entry(k2.clone())
                .or_insert_with(|| std::array::from_fn(|_| AtomicU64::new(0)));
            for (i, b) in BUCKETS_MS.iter().enumerate() {
                if latency_ms <= *b {
                    entry[i].fetch_add(1, Ordering::Relaxed);
                }
            }
            entry[10].fetch_add(1, Ordering::Relaxed);
        }
        self.duration_sum
            .entry(k2.clone())
            .or_insert_with(|| AtomicU64::new(0))
            .fetch_add(latency_ms as u64, Ordering::Relaxed);
        self.duration_count
            .entry(k2)
            .or_insert_with(|| AtomicU64::new(0))
            .fetch_add(1, Ordering::Relaxed);
    }
}

impl Default for Metrics {
    fn default() -> Self {
        Self::new()
    }
}

pub async fn handle_metrics(State(state): State<AppState>) -> impl IntoResponse {
    let mut out = String::new();

    out.push_str("# HELP solmux_requests_total Total RPC requests routed.\n");
    out.push_str("# TYPE solmux_requests_total counter\n");
    for entry in state.metrics.requests.iter() {
        let (upstream, method, status) = entry.key();
        out.push_str(&format!(
            "solmux_requests_total{{upstream=\"{}\",method=\"{}\",status=\"{}\"}} {}\n",
            upstream,
            method,
            status,
            entry.value().load(Ordering::Relaxed)
        ));
    }

    out.push_str("# HELP solmux_request_duration_ms Request duration in milliseconds.\n");
    out.push_str("# TYPE solmux_request_duration_ms histogram\n");
    for entry in state.metrics.duration_buckets.iter() {
        let (upstream, method) = entry.key();
        let arr = entry.value();
        for (i, b) in BUCKETS_MS.iter().enumerate() {
            out.push_str(&format!(
                "solmux_request_duration_ms_bucket{{upstream=\"{}\",method=\"{}\",le=\"{}\"}} {}\n",
                upstream,
                method,
                b,
                arr[i].load(Ordering::Relaxed)
            ));
        }
        out.push_str(&format!(
            "solmux_request_duration_ms_bucket{{upstream=\"{}\",method=\"{}\",le=\"+Inf\"}} {}\n",
            upstream,
            method,
            arr[10].load(Ordering::Relaxed)
        ));
        let sum = state
            .metrics
            .duration_sum
            .get(&(upstream.clone(), method.clone()))
            .map(|s| s.load(Ordering::Relaxed))
            .unwrap_or(0);
        let count = state
            .metrics
            .duration_count
            .get(&(upstream.clone(), method.clone()))
            .map(|s| s.load(Ordering::Relaxed))
            .unwrap_or(0);
        out.push_str(&format!(
            "solmux_request_duration_ms_sum{{upstream=\"{}\",method=\"{}\"}} {}\n",
            upstream, method, sum
        ));
        out.push_str(&format!(
            "solmux_request_duration_ms_count{{upstream=\"{}\",method=\"{}\"}} {}\n",
            upstream, method, count
        ));
    }

    out.push_str("# HELP solmux_upstream_slot_lag Slot lag behind the global tip.\n");
    out.push_str("# TYPE solmux_upstream_slot_lag gauge\n");
    let tip = state.pool.tip_slot.load(Ordering::Relaxed);
    for u in &state.pool.upstreams {
        let lag = tip.saturating_sub(u.last_slot.load(Ordering::Relaxed));
        out.push_str(&format!(
            "solmux_upstream_slot_lag{{upstream=\"{}\"}} {}\n",
            u.name, lag
        ));
    }

    out.push_str("# HELP solmux_upstream_health Upstream health (1 healthy, 0 dropped).\n");
    out.push_str("# TYPE solmux_upstream_health gauge\n");
    for u in &state.pool.upstreams {
        let h = if u.is_dropped() { 0.0 } else { 1.0 };
        out.push_str(&format!(
            "solmux_upstream_health{{upstream=\"{}\"}} {}\n",
            u.name, h
        ));
    }

    out.push_str(
        "# HELP solmux_blockhash_rejections_total sendTransaction calls rejected for stale blockhash.\n",
    );
    out.push_str("# TYPE solmux_blockhash_rejections_total counter\n");
    out.push_str(&format!(
        "solmux_blockhash_rejections_total {}\n",
        state.metrics.blockhash_rejections.load(Ordering::Relaxed)
    ));

    out.push_str("# HELP solmux_routing_decisions_total Selections made by the router per (upstream, method).\n");
    out.push_str("# TYPE solmux_routing_decisions_total counter\n");
    for entry in state.metrics.routing_decisions.iter() {
        let (upstream, method) = entry.key();
        out.push_str(&format!(
            "solmux_routing_decisions_total{{upstream=\"{}\",method=\"{}\"}} {}\n",
            upstream,
            method,
            entry.value().load(Ordering::Relaxed)
        ));
    }

    out.push_str("# HELP solmux_panics_total Handler panics caught by the tower layer.\n");
    out.push_str("# TYPE solmux_panics_total counter\n");
    out.push_str(&format!(
        "solmux_panics_total {}\n",
        state.metrics.panics.load(Ordering::Relaxed)
    ));

    (
        StatusCode::OK,
        [("content-type", "text/plain; version=0.0.4")],
        out,
    )
}
