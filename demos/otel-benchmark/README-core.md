# Core Proxy OTel Benchmark

Benchmarks OTel tracing overhead on the core Praxis proxy using
`GET /` against a Fortio echo backend. Shares the same cluster and
observability stack as the [AI benchmark](README.md).

## Prerequisites

Complete steps 1-6 from the [AI benchmark README](README.md) first.
The cluster, stacks, and images must already be deployed.

## Send core traffic

```bash
for i in $(seq 1 100); do
  curl -sf http://localhost:18080/
done
```

10 spans per GET request:

```text
GET / -> echo-backend  (root)
  |-- filter:request_id:request
  |-- filter:access_log:request
  |-- filter:router:request          -> routes / to echo cluster
  |-- filter:load_balancer:request
  |-- filter:load_balancer:response
  |-- filter:router:response
  |-- filter:access_log:response
  |-- filter:request_id:response
  +-- upstream_exchange [echo-backend:8080]
```

## Run the benchmark

```bash
bash scripts/benchmark.sh --scenario core
```

Runs 3 configurations at 2000 RPS for 30s each:

- **A: Baseline** — `praxis-experimental:dev` (no OTel feature)
- **B: OTel noop** — `praxis-experimental:dev-otel` (spans created,
  not exported)
- **C: OTel full** — `praxis-experimental:dev-otel` (spans exported
  to collector -> Tempo)

Generate the report:

```bash
bash scripts/report.sh <results-dir>
```
