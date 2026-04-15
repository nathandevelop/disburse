# disburse

A correctness and performance layer for Solana RPC.

Every serious Solana team pays for three or more RPC providers because no single one is reliable enough, and glues them together with hand-rolled failover. That is not enough. Read-heavy and write-heavy paths behave differently. Providers degrade unpredictably. Stale reads and stale blockhashes silently break production. `disburse` sits in front of your providers and makes those problems someone else's job.

**Community:** jump into the [Discord](https://discord.gg/HQxCaE62wH) to ask questions, share what you're running, or just chat about Solana infra. New contributors welcome.

## What it does

**Per-method routing.** Helius may be faster at `getProgramAccounts`. Triton may land `sendTransaction` better. `disburse` measures every `(provider, method)` pair and routes each call to whoever is actually fastest right now, with hysteresis so it does not oscillate between two close performers.

**Slot-lag rejection.** A provider that falls behind the chain tip silently serves stale data. `disburse` polls every upstream's slot every few seconds and downgrades any laggard before it poisons your reads.

**Blockhash guardrails.** `sendTransaction` calls with an expired blockhash are rejected at the proxy with a clear error, before they silently fail on chain and cost you priority fees.

**Circuit breaking.** A provider that starts erroring gets kicked out of rotation automatically. Your app keeps running on the rest. When the cooldown expires, exactly one probe request decides whether the provider is readmitted.

**Per-upstream concurrency cap.** A slow provider cannot accumulate thousands of hanging calls. Each upstream gets a configurable in-flight limit.

One Rust binary. One YAML config. No database. Prometheus metrics. Around 90,000 req/s per instance on a laptop. MIT licensed.

## Quick start

```bash
git clone https://github.com/nathandevelop/disburse && cd disburse
cargo build --release

cp config.example.yaml config.yaml
$EDITOR config.yaml

./target/release/disburse --config ./config.yaml
```

Point your Solana client at `http://localhost:8899` (or whatever you set `listen` to). Prometheus metrics are served at `http://localhost:9090/metrics`.

### Install

```bash
cargo install --path .
```

### Docker

```bash
docker build -t disburse .
docker run -p 8899:8899 -p 9090:9090 \
  -v $(pwd)/config.yaml:/app/config.yaml:ro \
  disburse
```

Image is distroless, non-root, with a binary-level `HEALTHCHECK` directive. No shell inside.

## Example config

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
  hysteresis_margin: 0.1

health:
  check_interval_secs: 3
  max_slot_lag: 10
  max_blockhash_age_slots: 100
  circuit_breaker_error_rate: 0.5
  circuit_breaker_cooldown_secs: 30

retries:
  max_attempts: 3
  retry_on_different_upstream: true
  deadline_ms: 15000
```

## Smoke test

Run `disburse` against a public endpoint and curl `getSlot` through it:

```bash
cat > config.smoke.yaml <<'EOF'
listen: "127.0.0.1:18899"
metrics_listen: "127.0.0.1:19090"
upstreams:
  - name: mainnet-beta
    url: "https://api.mainnet-beta.solana.com"
routing: { latency_weight: 0.7, success_weight: 0.3, ewma_alpha: 0.2 }
health:
  check_interval_secs: 3
  max_slot_lag: 10
  max_blockhash_age_slots: 100
  circuit_breaker_error_rate: 0.5
  circuit_breaker_cooldown_secs: 30
retries: { max_attempts: 3, retry_on_different_upstream: true }
EOF

RUST_LOG=info ./target/release/disburse --config ./config.smoke.yaml &

curl -s -X POST http://127.0.0.1:18899/ \
  -H 'content-type: application/json' \
  -d '{"jsonrpc":"2.0","id":1,"method":"getSlot"}'

curl -s http://127.0.0.1:18899/health | jq
curl -s http://127.0.0.1:19090/metrics
```

## Endpoints

| Port             | Path           | What                                                       |
| ---------------- | -------------- | ---------------------------------------------------------- |
| `listen`         | `POST /`       | JSON-RPC 2.0, single or batch                              |
| `listen`         | `GET /health`  | JSON summary of each upstream                              |
| `listen`         | `GET /livez`   | Always 200 while the process is alive (for Kubernetes)     |
| `listen`         | `GET /readyz`  | 200 when at least one upstream is healthy and warmed up    |
| `metrics_listen` | `GET /metrics` | Prometheus text format                                     |

## CLI

```
disburse --config ./config.yaml     # run the server
disburse --check --config path      # validate the config and exit
disburse --healthcheck              # used by the Docker HEALTHCHECK
disburse --version                  # print version
```

## Metrics

At minimum:

- `disburse_requests_total{upstream, method, status}` counter
- `disburse_request_duration_ms{upstream, method}` histogram
- `disburse_routing_decisions_total{upstream, method}` counter
- `disburse_upstream_slot_lag{upstream}` gauge
- `disburse_upstream_health{upstream}` gauge
- `disburse_blockhash_rejections_total` counter
- `disburse_panics_total` counter

Point Grafana at `/metrics` and you can see which provider is winning which method, how fast they are, whether any are lagging, and how often the circuit breaker trips.

## Access log

Every client request produces one structured `INFO` line with `trace_id`, `method`, `upstream`, `latency_ms`, and `status`:

```
INFO rpc{trace_id=e13295bb-...}: request method=getVersion upstream=helius latency_ms=128.82 status=ok
```

Clients may supply `x-trace-id` or `x-request-id` to have `disburse` preserve that ID for distributed tracing. Otherwise a UUID v4 is generated and echoed back in the response headers.

## What's next (not in v0)

- gRPC and Yellowstone passthrough
- WebSocket subscription fan-out
- Dedicated priority fee oracle endpoint
- True leader-aware `sendTransaction` routing from cached leader schedules
- Hosted version with anonymized cross-deployment intelligence sharing

## License

MIT. See `LICENSE`.

