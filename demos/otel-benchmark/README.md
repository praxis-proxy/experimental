# AI Gateway OTel Benchmark

Benchmarks OTel tracing overhead on the Praxis experimental AI gateway
using `POST /v1/chat/completions` against a mock LLM backend (llm-d
inference-sim). Deploys a full observability stack on KIND.

For the core (non-AI) proxy benchmark, see [README-core.md](README-core.md).

## Stack

| Component | Purpose |
| --------- | ------- |
| Praxis experimental AI gateway | Proxy under test (baseline + OTel) |
| llm-d inference-sim | Mock LLM backend |
| Fortio echo | Mock HTTP backend (core scenarios) |
| Prometheus + Grafana 11.x | Metrics + dashboards |
| Tempo | Distributed trace storage |
| Loki + Promtail | Log aggregation |
| OTel Collector | Trace pipeline (OTLP -> Tempo) |

## Prerequisites

1. Docker or Podman
2. [KIND](https://kind.sigs.k8s.io/)
3. [Helm](https://helm.sh/) with repos added (step 1 below)
4. [vegeta](https://github.com/tsenart/vegeta) (for benchmarks)
5. `python3` (used by `scripts/report.sh`)
6. [Praxis Forge CLI](https://github.com/praxis-proxy/forge) with
   `extraPortMappings` support (praxis-proxy/forge#16):

   ```bash
   cargo install --locked --git https://github.com/praxis-proxy/forge --branch feat/extra-port-mappings-v2
   ```

   Verify: `praxis-forge doctor`

## Step-by-Step

### 1. Add Helm repos (one-time)

```bash
helm repo add prometheus-community https://prometheus-community.github.io/helm-charts
helm repo add grafana https://grafana.github.io/helm-charts
helm repo add community-charts https://community-charts.github.io/helm-charts
helm repo update
```

### 2. Build images

From the **experimental repo root**:

```bash
cd /path/to/experimental

# Baseline (no OTel)
docker build -t praxis-experimental:dev -f Containerfile .

# With OTel tracing
docker build --build-arg FEATURES=otel -t praxis-experimental:dev-otel -f Containerfile .
```

### 3. Create the KIND cluster

```bash
cd demos/otel-benchmark
praxis-forge up --config forge.yaml
```

### 4. Load images into KIND

```bash
kind load docker-image praxis-experimental:dev praxis-experimental:dev-otel --name otel-bench-local
```

### 5. Deploy all stacks

```bash
for stack in prometheus tempo loki otel-collector mock-backends praxis-deploy dashboards; do
  praxis-forge apply --config forge.yaml local "$stack"
done
```

### 6. Verify

```bash
praxis-forge status --config forge.yaml
curl http://localhost:18901/healthy               # proxy is up
open http://localhost:13000                       # Grafana (admin/admin)
open http://localhost:19090                       # Prometheus
```

`GET http://localhost:18080/` returns 500, which is correct: the AI config
routes with `intelligent_route`, which selects a cluster from the request's
model. A bare `GET /` names no model, so no cluster matches. Use the admin
listener's `/healthy` for a liveness check and the chat endpoint in step 7
for a real request.

### 7. Send AI traffic

```bash
for i in $(seq 1 100); do
  curl -sf http://localhost:18080/v1/chat/completions \
    -X POST -H 'Content-Type: application/json' \
    -d '{"model":"test-model","messages":[{"role":"user","content":"hello"}]}'
done
```

### 8. View traces

Open Grafana > **Praxis OTel Traces** dashboard. Click a trace ID to see
the span waterfall:

```text
POST /v1/chat/completions -> inference-sim  (root)
  |-- filter:request_id:request
  |-- filter:access_log:request
  |-- filter:model_to_header:request
  |-- filter:model_to_header:request_body   -> promotes body "model" to X-Model header
  |-- filter:token_usage_headers:request
  |-- filter:token_count:request
  |-- filter:time_to_first_token:request
  |-- filter:intelligent_route:request
  |     +-- routing.select                  -> candidate scoring (inference wins, fresh)
  |-- filter:load_balancer:request
  |-- filter:load_balancer:response
  |-- filter:intelligent_route:response
  |-- filter:time_to_first_token:response
  |-- filter:token_count:response
  |-- filter:token_usage_headers:response
  |-- filter:model_to_header:response
  |-- filter:access_log:response
  |-- filter:request_id:response
  |-- filter:time_to_first_token:response_body
  |-- filter:token_count:response_body      -> extracts usage.prompt/completion_tokens
  |-- filter:access_log:response_body
  +-- upstream_exchange [inference-sim:8000]
```

23 spans per request, captured live from Tempo. The `routing.select`
span is a child of `filter:intelligent_route:request`.

Note that no `Praxis-Token-*` response headers appear for this backend.
`token_count` resolves usage in the response-**body** phase, which runs after
response headers have already gone downstream, so `token_usage_headers` has
nothing to inject by then. The counts land in filter metadata, which nothing
in this chain currently reads. `bedrock_invoke_model` is the exception,
because that provider reports usage in headers. Note: `intelligent_route`
is the sole cluster-selecting filter in the AI configs — Praxis's pipeline
validator rejects a chain with both `router` and `intelligent_route` ahead of
`load_balancer` (both implement `selects_cluster() -> true`), so the `ai`
benchmark scenario (which only sends `POST /v1/chat/completions`) relies on
`intelligent_route` alone to select the `inference` cluster.

### 9. Run the benchmark

```bash
bash scripts/benchmark.sh --scenario ai
```

Runs 3 configurations at 500 RPS for 30s each:

- **A: Baseline** — `praxis-experimental:dev` (no OTel feature)
- **B: OTel noop** — `praxis-experimental:dev-otel` (spans created,
  not exported)
- **C: OTel full** — `praxis-experimental:dev-otel` (spans exported
  to collector -> Tempo)

Generate the report:

```bash
bash scripts/report.sh <results-dir>
```

### How the runs are ordered

Configurations are **interleaved** (`A B C, A B C, ...`), not grouped
(`AAA BBB CCC`). Grouped ordering aliases any drift over time -- thermal,
page-cache warming, background load -- onto the config comparison, and
whichever config runs first absorbs all of it. In an earlier grouped run the
baseline's three measurements fell monotonically (815 -> 775 -> 735 us) purely
because it ran first, which biased it slow and made OTel look free. Every
config now sees the same distribution of machine states.

Each run gets its own pod rollout and its own warmup, so no single measurement
is a rollout artifact while its peers are warm.

### Interpreting the report

Figures are the **median** across runs, not the mean. Each config's first run
follows a fresh pod rollout and pays connection-setup and warm-up costs that
the 10s warmup does not fully absorb -- a P99 of 30ms+ against a ~1.8ms steady
state is routine, and it affects the baseline as much as the OTel runs. A mean
lets one such run swamp the result and report a several-hundred-percent
"regression" that does not exist.

The **P99 range** column reports min-max across runs so those outliers stay
visible rather than being silently smoothed away. A wide range with a stable
median is the expected shape; a wide range in only one config is worth
investigating.

### What this benchmark can and cannot resolve

It runs on a single-node KIND cluster inside a container-runtime VM, sharing
CPU with Prometheus, Tempo, the collector and the load generator. At 500 RPS
that VM is close to saturated, and run-to-run variance of several percent is
normal. **An effect smaller than that variance cannot be resolved here**, and
a reported delta below it should be read as "no measurable difference", not as
a speed-up or a regression.

Two things follow. Give the VM as many CPUs as you can spare -- 4 is not
enough to measure a proxy at this rate. And run it on an otherwise idle
machine; a concurrent container build or a second cluster will show up in the
numbers.

For attributing cost to individual filters, span durations are a better
instrument than end-to-end latency, because they exclude network and upstream
time. See the [ai-gateway demo](../ai-gateway/README.md), which derives
per-filter latency from span metrics.

### 10. Teardown

```bash
praxis-forge down --config forge.yaml
```

## Dashboards

| Dashboard | URL |
| --- | --- |
| Praxis Proxy Overview | <http://localhost:13000/d/praxis-proxy-overview> |
| Praxis OTel Traces | <http://localhost:13000/d/praxis-traces> |
| Praxis Benchmark Results | <http://localhost:13000/d/praxis-benchmark> |
| Praxis AI/LLM Golden Signals | <http://localhost:13000/d/praxis-ai-golden-signals> |
| Praxis Structured Logs | <http://localhost:13000/d/praxis-logs> |

## Host Ports

| Port | Service | KIND NodePort |
| --- | --- | --- |
| 18080 | Praxis proxy | 30080 |
| 18901 | Praxis admin | 30901 |
| 13000 | Grafana | 30300 |
| 19090 | Prometheus | 30909 |

## Known Issues

- **Grafana 11.x required**: Pinned via `grafana.image.tag`. Grafana 12.0 has
  rendering bugs with provisioned dashboards.
- **AI token metrics**: The Tokens/sec and Hourly Cost panels require upstream
  work (ai#141).
- **praxis-ai is a git dependency**: `praxis-ai-proxy` is not published to
  crates.io, so it is pinned to the `v0.3.0` tag. The praxis core crates come
  from crates.io at 0.5.4.
