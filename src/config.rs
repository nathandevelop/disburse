//! YAML-backed configuration: upstreams, routing weights, health + retry tuning.

use serde::Deserialize;
use std::collections::HashMap;
use std::path::Path;

#[derive(Debug, Deserialize, Clone)]
pub struct Config {
    pub listen: String,
    pub metrics_listen: String,
    pub upstreams: Vec<UpstreamConfig>,
    pub routing: RoutingConfig,
    pub health: HealthConfig,
    pub retries: RetryConfig,
    #[serde(default)]
    pub request_timeout: TimeoutConfig,
    #[serde(default = "default_body_limit")]
    pub max_request_body_bytes: usize,
    #[serde(default = "default_response_body_limit")]
    pub max_response_body_bytes: usize,
    /// Maximum concurrent in-flight requests per upstream. A slow provider
    /// can't accumulate more than this many hanging calls.
    #[serde(default = "default_max_concurrent_per_upstream")]
    pub max_concurrent_per_upstream: usize,
}

fn default_max_concurrent_per_upstream() -> usize {
    256
}

#[derive(Debug, Deserialize, Clone)]
pub struct UpstreamConfig {
    pub name: String,
    pub url: String,
    #[serde(default)]
    pub headers: HashMap<String, String>,
}

#[derive(Debug, Deserialize, Clone)]
pub struct RoutingConfig {
    pub latency_weight: f64,
    pub success_weight: f64,
    pub ewma_alpha: f64,
    /// Minimum relative score improvement a challenger must beat the current
    /// leader by to take over routing for a method. Prevents oscillation when
    /// two upstreams are close in score. 0.1 means "must be 10% better".
    #[serde(default = "default_hysteresis_margin")]
    pub hysteresis_margin: f64,
}

fn default_hysteresis_margin() -> f64 {
    0.1
}

#[derive(Debug, Deserialize, Clone)]
pub struct HealthConfig {
    pub check_interval_secs: u64,
    pub max_slot_lag: u64,
    pub max_blockhash_age_slots: u64,
    pub circuit_breaker_error_rate: f64,
    pub circuit_breaker_cooldown_secs: u64,
    /// Width of the sliding window used for rolling error rate and sendTransaction p50.
    #[serde(default = "default_rolling_window_secs")]
    pub rolling_window_secs: u64,
    /// Grace period after startup before blockhash freshness rejections kick in.
    #[serde(default = "default_blockhash_warmup_secs")]
    pub blockhash_warmup_secs: u64,
    /// Maximum number of blockhashes retained in the freshness cache.
    #[serde(default = "default_blockhash_cache_size")]
    pub blockhash_cache_size: usize,
}

fn default_rolling_window_secs() -> u64 {
    60
}

fn default_blockhash_warmup_secs() -> u64 {
    30
}

fn default_blockhash_cache_size() -> usize {
    512
}

#[derive(Debug, Deserialize, Clone)]
pub struct RetryConfig {
    pub max_attempts: u32,
    pub retry_on_different_upstream: bool,
    /// Hard deadline across all retries for a single client request. Prevents
    /// `max_attempts × per_attempt_timeout` from stacking into a 30s hang.
    #[serde(default = "default_request_deadline_ms")]
    pub deadline_ms: u64,
}

fn default_request_deadline_ms() -> u64 {
    15_000
}

#[derive(Debug, Deserialize, Clone)]
pub struct TimeoutConfig {
    #[serde(default = "default_timeout_ms")]
    pub default_ms: u64,
    #[serde(default)]
    pub per_method: HashMap<String, u64>,
}

impl Default for TimeoutConfig {
    fn default() -> Self {
        Self {
            default_ms: default_timeout_ms(),
            per_method: HashMap::new(),
        }
    }
}

impl TimeoutConfig {
    pub fn for_method(&self, method: &str) -> u64 {
        self.per_method
            .get(method)
            .copied()
            .unwrap_or(self.default_ms)
    }
}

fn default_timeout_ms() -> u64 {
    10_000
}

fn default_body_limit() -> usize {
    2 * 1024 * 1024 // 2 MiB
}

fn default_response_body_limit() -> usize {
    16 * 1024 * 1024 // 16 MiB — generous for getProgramAccounts
}

impl Config {
    pub fn load(path: impl AsRef<Path>) -> anyhow::Result<Self> {
        let contents = std::fs::read_to_string(path)?;
        let cfg: Config = serde_yaml::from_str(&contents)?;
        cfg.validate()?;
        Ok(cfg)
    }

    /// Catches misconfiguration at startup with clear messages, rather than
    /// silently accepting nonsense values and failing weirdly at runtime.
    pub fn validate(&self) -> anyhow::Result<()> {
        use anyhow::{anyhow, bail};

        self.listen
            .parse::<std::net::SocketAddr>()
            .map_err(|e| anyhow!("invalid `listen` address {:?}: {e}", self.listen))?;
        self.metrics_listen
            .parse::<std::net::SocketAddr>()
            .map_err(|e| {
                anyhow!(
                    "invalid `metrics_listen` address {:?}: {e}",
                    self.metrics_listen
                )
            })?;

        if self.upstreams.is_empty() {
            bail!("`upstreams` must not be empty");
        }

        let mut seen = std::collections::HashSet::new();
        for u in &self.upstreams {
            if u.name.trim().is_empty() {
                bail!("upstream has empty name");
            }
            if !seen.insert(&u.name) {
                bail!("duplicate upstream name: {:?}", u.name);
            }
            url::Url::parse(&u.url)
                .map_err(|e| anyhow!("upstream {:?} has invalid url: {e}", u.name))?;
        }

        let r = &self.routing;
        if !(0.0..=1.0).contains(&r.latency_weight) {
            bail!(
                "routing.latency_weight must be in [0, 1], got {}",
                r.latency_weight
            );
        }
        if !(0.0..=1.0).contains(&r.success_weight) {
            bail!(
                "routing.success_weight must be in [0, 1], got {}",
                r.success_weight
            );
        }
        if !(0.0..1.0).contains(&r.ewma_alpha) || r.ewma_alpha == 0.0 {
            bail!("routing.ewma_alpha must be in (0, 1), got {}", r.ewma_alpha);
        }
        if !(0.0..1.0).contains(&r.hysteresis_margin) {
            bail!(
                "routing.hysteresis_margin must be in [0, 1), got {}",
                r.hysteresis_margin
            );
        }

        let h = &self.health;
        if h.check_interval_secs == 0 {
            bail!("health.check_interval_secs must be > 0");
        }
        if !(0.0..=1.0).contains(&h.circuit_breaker_error_rate) {
            bail!(
                "health.circuit_breaker_error_rate must be in [0, 1], got {}",
                h.circuit_breaker_error_rate
            );
        }
        if h.rolling_window_secs == 0 {
            bail!("health.rolling_window_secs must be > 0");
        }
        if h.blockhash_cache_size == 0 {
            bail!("health.blockhash_cache_size must be > 0");
        }

        if self.retries.max_attempts == 0 {
            bail!("retries.max_attempts must be > 0");
        }
        if self.retries.deadline_ms == 0 {
            bail!("retries.deadline_ms must be > 0");
        }

        if self.max_concurrent_per_upstream == 0 {
            bail!("max_concurrent_per_upstream must be > 0");
        }

        if self.request_timeout.default_ms == 0 {
            bail!("request_timeout.default_ms must be > 0");
        }

        Ok(())
    }
}

#[cfg(test)]
mod tests;
