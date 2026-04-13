# solmux

> **solmux** — a correctness and performance layer for Solana RPC.
>
> Every serious Solana team pays for three or more RPC providers because no single one is reliable enough, and glues them together with hand-rolled failover. That's not enough. Read-heavy and write-heavy paths behave differently. Providers degrade unpredictably. Stale reads and stale blockhashes silently break production. solmux sits in front of your providers and makes those problems someone else's job.
>
> - **Per-method routing.** Helius is faster at `getProgramAccounts`. Triton lands `sendTransaction` better. solmux measures every `(provider, method)` pair and routes each call to whoever's actually fastest right now — with hysteresis, so it doesn't oscillate.
> - **Slot-lag rejection.** A provider that falls behind the chain tip silently serves stale data. solmux polls every upstream's slot every few seconds and downgrades any laggard before it poisons your reads.
> - **Blockhash guardrails.** `sendTransaction` calls with an expired blockhash are rejected at the proxy with a clear error — before they silently fail on chain and cost you priority fees.
> - **Circuit-breaking.** A provider that starts erroring gets kicked out of rotation automatically; your app keeps running on the rest.
>
> One Rust binary. One YAML config. No database. Prometheus metrics. ~90,000 req/s per instance. MIT licensed.

## Quick start

```bash
# clone + build
git clone <this-repo> && cd solmux
cargo build --release

# copy the example config and fill in your upstreams
cp config.example.yaml config.yaml
$EDITOR config.yaml

# run it
./target/release/solmux --config ./config.yaml
```

Point your Solana client at `http://localhost:8899` (or whatever you set `listen` to). Prometheus metrics are served at `http://localhost:9090/metrics`.

### Install

```bash
cargo install --path .
```

### Example config

```yaml
listen: "0.0.0.0:8899"
metrics_listen: "0.0.0.0:9090"

upstreams:
  - name: helius
    url: "https://mainnet.helius-rpc.com/?api-key=REPLACE_ME"
  - name: triton
    url: "https://your.triton-endpoint/REPLACE_ME"
  - name: quicknode
    url: "https://your.quicknode-endpoint/REPLACE_ME"

routing:
  latency_weight: 0.7
  success_weight: 0.3
  ewma_alpha: 0.2

health:
  check_interval_secs: 3
  max_slot_lag: 10
  max_blockhash_age_slots: 100
  circuit_breaker_error_rate: 0.5
  circuit_breaker_cooldown_secs: 30

retries:
  max_attempts: 3
  retry_on_different_upstream: true
```

## Smoke test

Run solmux against two free public endpoints and curl `getSlot` through it:

```yaml
# config.smoke.yaml
listen: "127.0.0.1:18899"
metrics_listen: "127.0.0.1:19090"
upstreams:
  - name: mainnet-beta
    url: "https://api.mainnet-beta.solana.com"
  - name: helius-free
    url: "https://mainnet.helius-rpc.com/?api-key=YOUR_FREE_KEY"
routing: { latency_weight: 0.7, success_weight: 0.3, ewma_alpha: 0.2 }
health:
  check_interval_secs: 3
  max_slot_lag: 10
  max_blockhash_age_slots: 100
  circuit_breaker_error_rate: 0.5
  circuit_breaker_cooldown_secs: 30
retries: { max_attempts: 3, retry_on_different_upstream: true }
```

```bash
RUST_LOG=info ./target/release/solmux --config ./config.smoke.yaml &

# issue a request through the proxy
curl -s -X POST http://127.0.0.1:18899/ \
  -H 'content-type: application/json' \
  -d '{"jsonrpc":"2.0","id":1,"method":"getSlot"}'

# see per-upstream health + slot lag
curl -s http://127.0.0.1:18899/health | jq

# see Prometheus metrics
curl -s http://127.0.0.1:19090/metrics
```

You should see `getSlot` being routed to whichever upstream is fastest, both upstreams reporting their current tip in `/health`, and per-`(upstream, method)` counters + histograms showing up in `/metrics`.

## Endpoints

| Port | Path | What |
|---|---|---|
| `listen` | `POST /` | JSON-RPC 2.0, single or batch |
| `listen` | `GET /health` | JSON summary of each upstream |
| `metrics_listen` | `GET /metrics` | Prometheus text format |

## How the four differentiators work

- **Per-method routing** lives in `stats.rs` + `router.rs`. Every upstream has a per-method EWMA of latency and success rate. Scoring: `(latency_weight * 1/(1+latency/100) + success_weight * success_rate) * slot_lag_penalty`. Ties are broken randomly so upstreams can differentiate during warmup.
- **Slot lag** lives in `health.rs`. A tokio task per upstream polls `getSlot` every `check_interval_secs`. The max slot across all upstreams is tracked as the global tip in an `AtomicU64`. Any upstream lagging more than `max_slot_lag` gets a `0.1x` score multiplier.
- **Blockhash freshness** lives in `solana.rs` + `server.rs`. A rolling cache of `(blockhash -> slot first seen)` is populated from health-loop `getLatestBlockhash` polls and opportunistically from client `getLatestBlockhash` responses. Incoming `sendTransaction` calls are decoded just enough to extract `recent_blockhash`; if `tip - seen_slot > max_blockhash_age_slots` (or the blockhash is unknown after a 30s warmup), the proxy returns JSON-RPC error `-32002` instead of forwarding.
- **Leader-aware tx routing** lives in `router.rs`. `sendTransaction` prefers the upstream with the lowest p50 latency on `sendTransaction` over the last 60s. v1 will cache `getLeaderSchedule` and route based on per-upstream latency to the upcoming leader — the TODO is in the code.

## What's next (not in v0)

- gRPC / Yellowstone passthrough (week 2)
- WebSocket subscription fan-out and dedup (week 2)
- Dedicated priority fee oracle endpoint (week 3)
- Hosted version with anonymized cross-deployment intelligence sharing (week 4)

## License

MIT.
