//! Per-method upstream selection: EWMA-based scoring with slot-lag and sendTransaction
//! p50 heuristics, plus sticky-leader hysteresis. Pure logic over `StatsStore` +
//! `UpstreamPool` — no I/O.

use super::pool::UpstreamPool;
use crate::upstream::Upstream;
use rand::seq::SliceRandom;
use std::sync::atomic::Ordering;
use std::sync::Arc;
use tracing::debug;

/// Pick the best upstream for a given method. `exclude` names are skipped
/// (used for retry-on-different-upstream). Returns None if no candidate remains.
///
/// Hysteresis: once a method has a "leader" upstream, we keep sending traffic
/// there until a challenger beats its score by more than
/// `routing.hysteresis_margin`. Prevents routing from flapping between two
/// upstreams whose scores oscillate within measurement noise.
pub fn select_for_method(
    pool: &UpstreamPool,
    method: &str,
    exclude: &[String],
) -> Option<Arc<Upstream>> {
    let tip = pool.tip_slot.load(Ordering::Relaxed);
    let cfg = &pool.config;
    let margin = cfg.routing.hysteresis_margin;

    // DIFFERENTIATOR 4: leader-aware sendTransaction routing.
    // v0 heuristic: lowest p50 latency on sendTransaction over the last 60s.
    // TODO v1: cache getLeaderSchedule and route to the upstream with the best
    // measured connection latency to the current/upcoming leader.
    if method == "sendTransaction" {
        let mut measured: Vec<(Arc<Upstream>, f64)> = Vec::new();
        for u in &pool.upstreams {
            if exclude.contains(&u.name) || u.is_dropped() {
                continue;
            }
            if let Some(p50) = pool.stats.send_tx_p50(&u.name) {
                measured.push((u.clone(), p50));
            }
        }
        if !measured.is_empty() {
            // Lower p50 = better. Flip to an "up-is-better" score so the
            // hysteresis math matches the generic path.
            let best_p50 = measured
                .iter()
                .map(|(_, p)| *p)
                .fold(f64::INFINITY, f64::min);
            let scored: Vec<(Arc<Upstream>, f64)> = measured
                .into_iter()
                .map(|(u, p)| (u, best_p50 / p.max(0.001)))
                .collect();
            return apply_hysteresis(pool, method, scored, margin);
        }
        // no sendTransaction history yet — fall through to generic scoring
    }

    // DIFFERENTIATOR 1: per-method EWMA scoring.
    let mut candidates: Vec<(Arc<Upstream>, f64)> = Vec::new();
    for u in &pool.upstreams {
        if exclude.contains(&u.name) || u.is_dropped() {
            continue;
        }

        let (latency_score, success_score) = match pool.stats.get(&u.name, method) {
            Some(s) if s.samples > 0 => {
                let ls = 1.0 / (1.0 + s.ewma_latency_ms / 100.0);
                (ls, s.ewma_success)
            }
            // warmup default: give every upstream a fair chance until we have data
            _ => (0.5, 0.5),
        };

        // DIFFERENTIATOR 2: slot lag penalty.
        let slot = u.last_slot.load(Ordering::Relaxed);
        let lag = tip.saturating_sub(slot);
        let slot_lag_penalty = if lag > cfg.health.max_slot_lag {
            0.1
        } else {
            1.0
        };

        let score = (cfg.routing.latency_weight * latency_score
            + cfg.routing.success_weight * success_score)
            * slot_lag_penalty;

        if score > 0.0 {
            candidates.push((u.clone(), score));
        }
    }

    apply_hysteresis(pool, method, candidates, margin)
}

/// Given a set of (upstream, score) candidates where higher score is better,
/// apply sticky-leader hysteresis and return the chosen upstream. `None` if
/// no candidates.
fn apply_hysteresis(
    pool: &UpstreamPool,
    method: &str,
    candidates: Vec<(Arc<Upstream>, f64)>,
    margin: f64,
) -> Option<Arc<Upstream>> {
    if candidates.is_empty() {
        return None;
    }

    let top_score = candidates.iter().map(|(_, s)| *s).fold(f64::MIN, f64::max);

    // Keep the current leader if it's still a viable candidate AND its score
    // is within margin of the top. Otherwise crown a new leader.
    if let Some(leader_name) = pool.current_leader.get(method).map(|r| r.clone()) {
        if let Some((leader_up, leader_score)) =
            candidates.iter().find(|(u, _)| u.name == leader_name)
        {
            let leader_score = *leader_score;
            let leader_up = leader_up.clone();
            if leader_score >= top_score * (1.0 - margin) {
                debug!(
                    method = %method,
                    chosen = %leader_up.name,
                    score = leader_score,
                    top = top_score,
                    "routing decision (leader held via hysteresis)"
                );
                return Some(leader_up);
            }
        }
    }

    // Either no leader yet, leader was excluded/dropped, or challenger beats
    // margin. Pick the highest-scoring candidate (random tiebreak), install as leader.
    let top: Vec<(Arc<Upstream>, f64)> = candidates
        .into_iter()
        .filter(|(_, s)| (*s - top_score).abs() < 1e-9)
        .collect();
    let mut rng = rand::thread_rng();
    let chosen = top.choose(&mut rng).map(|x| x.0.clone());
    if let Some(ref c) = chosen {
        pool.current_leader
            .insert(method.to_string(), c.name.clone());
        debug!(
            method = %method,
            chosen = %c.name,
            score = top_score,
            "routing decision (new leader)"
        );
    }
    chosen
}

#[cfg(test)]
mod tests;
