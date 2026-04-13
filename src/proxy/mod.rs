//! Inbound HTTP: axum wiring, JSON-RPC handlers, /health, and Prometheus metrics.

mod rpc;
mod server;

pub mod metrics;

pub use metrics::Metrics;
pub use server::{app, metrics_app, AppState};
