# AI Gateway: real models, token budgets, span-derived dashboards

Runs the Praxis experimental gateway in front of a **real local model**
(Ollama), enforcing a **token budget**, with dashboards built from **trace
span metrics** — including per-filter latency, which a trace waterfall can
only show one request at a time.

Runs on its own KIND cluster with ports offset from
[../otel-benchmark](../otel-benchmark), so both demos can be up at once.

| Demo | Question it answers |
| --- | --- |
| `otel-benchmark` | What does OTel tracing cost? (synthetic backend, load test) |
| `ai-gateway` (this one) | What does a real AI gateway do, and which filter costs what? |

## Why Ollama runs on the host

Apple Metal is not reachable from Linux containers on macOS — Docker Desktop
and Rancher Desktop run a Linux VM with no GPU passthrough, so an
in-container Ollama would be CPU-only. The model server stays native; only
the gateway is containerized.

Leave Ollama on its **default `127.0.0.1` binding**. Verified: a KIND pod
reaches it at `host.docker.internal:11434` (which resolves to the host
gateway) because the container runtime proxies that name from the host side.
Setting `OLLAMA_HOST=0.0.0.0` is unnecessary and would publish your models to
the local network.

## Prerequisites

1. Docker or Podman
2. [KIND](https://kind.sigs.k8s.io/)
3. [Helm](https://helm.sh/) with repos added (step 1 below)
4. `python3`
5. [Praxis Forge CLI](https://github.com/praxis-proxy/forge) with
   `extraPortMappings` support (praxis-proxy/forge#16):

   ```bash
   cargo install --locked --git https://github.com/praxis-proxy/forge --branch feat/extra-port-mappings-v2
   ```

   Verify: `praxis-forge doctor`
6. [Ollama](https://ollama.com) **0.33+** with a model. Older versions fail
   with `412: requires a newer version of Ollama`:

   ```bash
   ollama --version
   ollama pull qwen3.8:27b     # quality demo, step 7
   ollama pull qwen3.5:0.8b    # rate-limit demo, step 8 (see why there)
   ```

## Step-by-Step

### 1. Add Helm repos (one-time)

```bash
helm repo add prometheus-community https://prometheus-community.github.io/helm-charts
helm repo add grafana https://grafana.github.io/helm-charts
helm repo update
```

### 2. Start Ollama

```bash
ollama serve
```

### 3. Build the image

From the **experimental repo root**. `FEATURES=otel` compiles in the OTLP
exporter; the `token-rate-limit-filter` cargo feature is on by default in
this image:

```bash
cd /path/to/experimental
docker build --build-arg FEATURES=otel -t praxis-experimental:ai-gw -f Containerfile .
docker inspect --format '{{index .Config.Labels "io.praxis.build.features"}}' \
  praxis-experimental:ai-gw
```

### 4. Create the cluster and load the image

```bash
cd demos/ai-gateway
praxis-forge up --config forge.yaml
kind load docker-image praxis-experimental:ai-gw --name ai-gw-local
```

### 5. Deploy

```bash
for stack in prometheus tempo otel-collector praxis-deploy dashboards; do
  praxis-forge apply --config forge.yaml local "$stack"
done
```

### 6. Verify

```bash
curl http://localhost:38901/healthy    # gateway is up
open http://localhost:33000            # Grafana (admin/admin)
```

### 7. Send a request through the gateway

```bash
curl -s http://localhost:38080/v1/chat/completions \
  -H 'Content-Type: application/json' \
  -d '{"model":"qwen3.8:27b","messages":[{"role":"user","content":"say hi"}],"max_tokens":300}'
```

> `qwen3.8` is a **reasoning** model: it returns a separate `reasoning` field
> alongside `content`, and those tokens count toward `completion_tokens`.
> With a small `max_tokens` the whole budget goes to reasoning and `content`
> comes back empty — that is not a failure. Use `max_tokens: 300` or more.

### 8. Hit a real rate limit

The deployed config gives two tiers, matched on a header:

| Tier | Match | Budget | Approx requests/min |
| --- | --- | --- | --- |
| `premium` | `X-Tier: premium` | 20,000 tokens/min | ~290 |
| `free` | catch-all | 5,000 tokens/min | ~70 |

`reserved_tokens` is 200 -- a realistic estimate for a short chat completion,
not a number tuned to trip quickly. The requests/min column is *measured*
usage, not `budget / reserved_tokens`: reservations reconcile against what the
model actually returned (~68 tokens here), so the estimate governs admission
while real usage governs throughput. See the callout below.

```bash
bash scripts/rate-limit-demo.sh
```

It sends 100 requests as `free` (8 at a time), waits for the sliding window to
age out, then sends the same 100 as `premium`:

```text
  100 requests as tier: free
  200 OK            53
  429 rate limited  47
  -> 47% of requests hit the free tier's token budget

  100 requests as tier: premium
  200 OK           100
  429 rate limited   0
  -> 0% of requests hit the premium tier's token budget
```

The exact split moves with whatever is already inside the 1m window -- on a
completely fresh budget the free tier admits closer to 70. The point is that
one tier reaches its limit under the same load the other absorbs.

The 429 carries `Retry-After` and `X-RateLimit-{Limit,Remaining,Reset}-Tokens`.

> **Why a small model here.** The limiter decides at *admission*, before the
> upstream is called, so the model is irrelevant to what this demonstrates --
> but the burst has to land inside the 1m window. Measured on an M4 Max:
> `qwen3.5:0.8b` answers in ~0.3s warm, while `qwen3.8:27b` takes 74-78s under
> concurrency, so 40 requests would span ~12 minutes and the budget would age
> out faster than it was consumed. The limit would never be reached.

> **Reservations are estimates, and the gap shows up in bursts, not in the
> sustained rate.** `reserved_tokens: 200` is charged at admission, then
> reconciled against what the model reported -- measured here as
> `estimated 22,000 / actual 7,511 / refunded 14,489` over 110 requests, or
> ~68 real tokens each. Because the refund lands quickly, the *sustained*
> limit follows real usage (~70 requests/min on free), not
> `budget / reserved_tokens` (~25). What the estimate does bound is
> concurrency: every in-flight request holds 200 tokens, so a burst of 25
> simultaneous requests exhausts the free budget on reservations alone even
> though the same 25 spread out would cost ~1,700 tokens. That is the current
> upstream milestone's design -- a flat placeholder pending configurable
> estimation strategies (ai#121). The "Estimation Accuracy" panel plots
> `estimated` against `actual`, `refunded` and `overage` so the gap is visible.

> `token_rate_limit` does **not** authenticate. A rule matched on a header
> trusts whatever upstream set it, so in a real deployment an auth filter must
> populate `X-Tier` and strip any client-supplied copy. Tracked at grid#101.

### 9. Generate traffic for the dashboards

Span metrics are rate-based, so they need sustained traffic to be meaningful.
Use `basic.yaml` (no budget) so nothing is rejected:

```bash
kubectl create configmap praxis-config \
  --from-file=praxis.yaml=configs/basic.yaml \
  -n default --dry-run=client -o yaml | kubectl apply -f -
kubectl rollout restart deployment/praxis-proxy -n default
kubectl rollout status deployment/praxis-proxy -n default

for i in $(seq 1 60); do
  curl -s -o /dev/null http://localhost:38080/v1/chat/completions \
    -H 'Content-Type: application/json' \
    -d '{"model":"qwen3.8:27b","messages":[{"role":"user","content":"hi"}],"max_tokens":40}'
done
```

### 10. Teardown

```bash
praxis-forge down --config forge.yaml
```

## Dashboards

| Dashboard | URL |
| --- | --- |
| Praxis AI Gateway Overview | <http://localhost:33000/d/praxis-ai-gateway-overview> |
| Praxis Filter Latency (from spans) | <http://localhost:33000/d/praxis-filter-latency> |
| Praxis Token Budget & Rate Limiting | <http://localhost:33000/d/praxis-token-budget> |
| Praxis OTel Traces | <http://localhost:33000/d/praxis-traces> |

### Two telemetry planes, and why panels say which one they use

Prometheus counters are incremented on **every** request and are unaffected by
trace sampling. Span metrics are derived only from **sampled** traces. They do
not cover the same traffic:

| Plane | Coverage | Use it for |
| --- | --- | --- |
| Prometheus (`praxis_*`) | 100% of requests | counts, rates, totals, budget state |
| Span metrics (`traces_spanmetrics_*`) | the sampled fraction | latency distributions, per-filter breakdown |

Random sampling is unbiased for percentiles, so span-derived latency is
trustworthy even at `sampling_rate: 0.1`. **Counts are not** -- they are low by
exactly the sampling factor. The "Weighted Filter Cost" panel therefore takes
its quantile from span metrics and its request rate from
`praxis_http_requests_total`; multiplying two sampled series would under-report
10x while looking entirely plausible.

The "Sampling Ratio Cross-Check" panel divides one plane by the other. It
should sit near the configured sampling rate. A sudden drop means spans are
being *lost* rather than sampled -- see the span-export-failure series on the
same panel.

This demo sets `sampling_rate: 1.0` on purpose: it pushes a handful of requests
per minute through a local model, and the span-derived panels need all of them.
So here the cross-check should read ~1.0, and the two planes agree. To see them
diverge, set `sampling_rate: 0.1` in `configs/*.yaml` and drive sustained
traffic -- which is what `../otel-benchmark` does at ~500 RPS.

### Where the numbers come from

**Token budget** panels read counters the `token_rate_limit` filter exports
directly:

```text
praxis_ai_token_rate_limit_tokens_total{kind="actual"|"estimated"|"refunded"|"overage"}
praxis_ai_token_rate_limit_requests_total{decision="admitted"|"denied"}
```

`estimated` is reserved up front; `actual` is what the model reported. The
difference reconciles as `refunded` or `overage`. **If `actual` exactly equals
`estimated`, reconciliation is not running** — see the ordering note in
`configs/token-budget.yaml`.

**Filter latency** panels read `traces_spanmetrics_*`, which Tempo's
metrics-generator derives from the spans themselves. Praxis emits one span per
filter per phase, so `span_name` becomes the breakdown key and the question
"which filter costs what" becomes a Prometheus query rather than an
eyeball-the-waterfall exercise.

## Host Ports

| Port | Service | KIND NodePort |
| --- | --- | --- |
| 38080 | Praxis proxy | 30080 |
| 38901 | Praxis admin | 30901 |
| 33000 | Grafana | 30300 |
| 39090 | Prometheus | 30909 |

## Security notes — this is a demo configuration

- **`allow_public_admin: true`** binds praxis's admin listener to the pod IP
  so Prometheus can scrape `/metrics`. That listener also serves
  `/api/log-level` (accepts `PUT` and `DELETE`) and `/api/kv`, so every pod in
  the cluster can change the gateway's log level. Acceptable on a throwaway
  KIND cluster; in anything shared, restrict `:9901` with a NetworkPolicy.
  Metrics on a separate port from the mutating admin API would be the proper
  fix and is worth an upstream issue.
- **`allow_private_endpoints: true`** disables SSRF / DNS-rebinding hardening
  so the gateway can reach `host.docker.internal`. Drop it the moment the
  upstream is a public provider.
- **`X-Tier` is trusted as-is.** `token_rate_limit` does not authenticate;
  a client can send `X-Tier: premium` and get the premium budget. An auth
  filter must set it and strip client-supplied copies (grid#101).

## Notes

- `token_rate_limit` is experimental, requiring the `token-rate-limit-filter`
  cargo feature. Its parent proposal is not yet accepted (ai#796) and the
  config surface may change.
- `reserved_tokens` is a flat per-request estimate. Deriving it from request
  metadata (e.g. `max_tokens`) is deferred upstream.
- The `memory` backend is per-process. `backend.kind: valkey` shares one
  budget across replicas.
- Span metrics take a scrape interval or two to appear after the first
  traffic; empty panels immediately after deploy are expected.
