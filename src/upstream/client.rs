//! A single configured upstream: HTTP client, connection-pooled for reuse, plus
//! per-upstream circuit-breaker state and concurrency-limit semaphore.

use parking_lot::RwLock;
use reqwest::header::{HeaderMap, HeaderName, HeaderValue};
use reqwest::Client;
use std::collections::HashMap;
use std::sync::atomic::AtomicU64;
use std::sync::Arc;
use std::time::{Duration, Instant};
use tokio::sync::{OwnedSemaphorePermit, Semaphore};

/// Finite-state circuit breaker per upstream.
#[derive(Debug)]
enum CircuitState {
    /// Normal operation: traffic flows freely.
    Closed,
    /// Rejecting all traffic until `until`. Set by the health loop's breaker.
    Open { until: Instant },
    /// Cooldown elapsed. The next request becomes a probe; concurrent probe
    /// requests are blocked until the first one settles.
    HalfOpen { probe_in_flight: bool },
}

pub struct Upstream {
    pub name: String,
    pub url: String,
    pub client: Client,
    pub headers: HeaderMap,
    pub last_slot: AtomicU64,
    circuit: RwLock<CircuitState>,
    /// Caps concurrent in-flight requests to this upstream to stop a slow
    /// provider from accumulating thousands of hanging calls.
    semaphore: Arc<Semaphore>,
}

impl Upstream {
    pub fn new(
        name: String,
        url: String,
        headers: HashMap<String, String>,
        max_concurrent: usize,
    ) -> Self {
        let client = Client::builder()
            .pool_idle_timeout(Duration::from_secs(90))
            .pool_max_idle_per_host(512)
            .tcp_nodelay(true)
            .tcp_keepalive(Duration::from_secs(60))
            .http2_keep_alive_interval(Duration::from_secs(30))
            .http2_keep_alive_timeout(Duration::from_secs(10))
            .http2_keep_alive_while_idle(true)
            .timeout(Duration::from_secs(30))
            .build()
            .expect("reqwest client");

        let mut header_map = HeaderMap::new();
        for (k, v) in headers {
            if let (Ok(name_h), Ok(val)) = (HeaderName::try_from(&k), HeaderValue::try_from(&v)) {
                header_map.insert(name_h, val);
            } else {
                tracing::warn!(upstream=%name, header=%k, "invalid header skipped");
            }
        }

        Self {
            name,
            url,
            client,
            headers: header_map,
            last_slot: AtomicU64::new(0),
            circuit: RwLock::new(CircuitState::Closed),
            semaphore: Arc::new(Semaphore::new(max_concurrent)),
        }
    }

    /// Router-level visibility: `true` when the circuit is Open and we should
    /// skip this upstream during scoring. HalfOpen is still eligible — the
    /// single probe slot is enforced atomically inside `try_admit`.
    pub fn is_dropped(&self) -> bool {
        matches!(*self.circuit.read(), CircuitState::Open { .. })
    }

    /// Force the circuit open for `cooldown`. Called by the health loop when
    /// rolling error rate exceeds the threshold.
    pub fn trip(&self, cooldown: Duration) {
        *self.circuit.write() = CircuitState::Open {
            until: Instant::now() + cooldown,
        };
    }

    /// Try to acquire one routing slot on this upstream. On success returns a
    /// semaphore permit (held for the lifetime of the request) and a flag
    /// indicating whether this call is the half-open probe. Returns `None` if
    /// the circuit is Open, a probe is already in flight, or the concurrency
    /// cap is full.
    pub fn try_admit(&self) -> Option<(OwnedSemaphorePermit, bool)> {
        let is_probe = {
            let mut c = self.circuit.write();
            match &*c {
                CircuitState::Closed => false,
                CircuitState::Open { until } => {
                    if Instant::now() < *until {
                        return None;
                    }
                    *c = CircuitState::HalfOpen {
                        probe_in_flight: true,
                    };
                    true
                }
                CircuitState::HalfOpen { probe_in_flight } => {
                    if *probe_in_flight {
                        return None;
                    }
                    *c = CircuitState::HalfOpen {
                        probe_in_flight: true,
                    };
                    true
                }
            }
        };

        match self.semaphore.clone().try_acquire_owned() {
            Ok(permit) => Some((permit, is_probe)),
            Err(_) => {
                if is_probe {
                    // Return the probe slot so a later call can try again.
                    *self.circuit.write() = CircuitState::HalfOpen {
                        probe_in_flight: false,
                    };
                }
                None
            }
        }
    }

    /// Close out a half-open probe. Success closes the circuit; failure
    /// re-opens it for another full cooldown.
    pub fn settle_probe(&self, success: bool, cooldown: Duration) {
        let mut c = self.circuit.write();
        if matches!(
            &*c,
            CircuitState::HalfOpen {
                probe_in_flight: true
            }
        ) {
            *c = if success {
                CircuitState::Closed
            } else {
                CircuitState::Open {
                    until: Instant::now() + cooldown,
                }
            };
        }
    }
}
