//! Per-(upstream, method) EWMA latency/success tracking + per-upstream rolling
//! error and sendTransaction latency windows. Feeds the router and circuit breaker.

use dashmap::DashMap;
use parking_lot::Mutex;
use std::collections::VecDeque;
use std::sync::Arc;
use std::time::{Duration, Instant};

// DIFFERENTIATOR 1: per-method routing — stats are keyed by (upstream, method),
// not just upstream, so each method can be routed to its best performer.

#[derive(Debug, Clone)]
pub struct MethodStats {
    pub ewma_latency_ms: f64,
    pub ewma_success: f64,
    pub samples: u64,
}

impl Default for MethodStats {
    fn default() -> Self {
        Self {
            ewma_latency_ms: 0.0,
            ewma_success: 1.0,
            samples: 0,
        }
    }
}

type ErrorLog = Arc<Mutex<VecDeque<(Instant, bool)>>>;
type LatencyLog = Arc<Mutex<VecDeque<(Instant, f64)>>>;

pub struct StatsStore {
    methods: DashMap<(String, String), MethodStats>,
    errors: DashMap<String, ErrorLog>,
    send_tx_latency: DashMap<String, LatencyLog>,
    alpha: f64,
    window: Duration,
}

impl StatsStore {
    pub fn new(alpha: f64, rolling_window_secs: u64) -> Self {
        Self {
            methods: DashMap::new(),
            errors: DashMap::new(),
            send_tx_latency: DashMap::new(),
            alpha,
            window: Duration::from_secs(rolling_window_secs),
        }
    }

    pub fn record(&self, upstream: &str, method: &str, latency_ms: f64, success: bool) {
        let key = (upstream.to_string(), method.to_string());
        {
            let mut entry = self.methods.entry(key).or_default();
            if entry.samples == 0 {
                entry.ewma_latency_ms = latency_ms;
                entry.ewma_success = if success { 1.0 } else { 0.0 };
            } else {
                entry.ewma_latency_ms =
                    self.alpha * latency_ms + (1.0 - self.alpha) * entry.ewma_latency_ms;
                let s = if success { 1.0 } else { 0.0 };
                entry.ewma_success = self.alpha * s + (1.0 - self.alpha) * entry.ewma_success;
            }
            entry.samples += 1;
        }

        let now = Instant::now();
        let log = self
            .errors
            .entry(upstream.to_string())
            .or_insert_with(|| Arc::new(Mutex::new(VecDeque::new())))
            .clone();
        {
            let mut q = log.lock();
            q.push_back((now, !success));
            while let Some(&(t, _)) = q.front() {
                if now.duration_since(t) > self.window {
                    q.pop_front();
                } else {
                    break;
                }
            }
        }

        if method == "sendTransaction" && success {
            let l = self
                .send_tx_latency
                .entry(upstream.to_string())
                .or_insert_with(|| Arc::new(Mutex::new(VecDeque::new())))
                .clone();
            let mut q = l.lock();
            q.push_back((now, latency_ms));
            while let Some(&(t, _)) = q.front() {
                if now.duration_since(t) > self.window {
                    q.pop_front();
                } else {
                    break;
                }
            }
        }
    }

    pub fn get(&self, upstream: &str, method: &str) -> Option<MethodStats> {
        self.methods
            .get(&(upstream.to_string(), method.to_string()))
            .map(|s| s.clone())
    }

    pub fn error_rate(&self, upstream: &str) -> f64 {
        if let Some(entry) = self.errors.get(upstream) {
            let q = entry.lock();
            if q.is_empty() {
                return 0.0;
            }
            let errs = q.iter().filter(|(_, is_err)| *is_err).count();
            errs as f64 / q.len() as f64
        } else {
            0.0
        }
    }

    pub fn send_tx_p50(&self, upstream: &str) -> Option<f64> {
        if let Some(entry) = self.send_tx_latency.get(upstream) {
            let q = entry.lock();
            if q.is_empty() {
                return None;
            }
            let mut v: Vec<f64> = q.iter().map(|(_, l)| *l).collect();
            v.sort_by(|a, b| a.partial_cmp(b).unwrap_or(std::cmp::Ordering::Equal));
            Some(v[v.len() / 2])
        } else {
            None
        }
    }
}

#[cfg(test)]
mod tests;
