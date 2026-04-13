//! Everything about talking to an upstream RPC provider: the pooled HTTP
//! client, per-upstream state, and the background health-check loop.

mod client;
mod health;

pub use client::Upstream;
pub use health::spawn_health_monitors;
