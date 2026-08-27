# `switchyard_route`: Capability-mode Mixture-of-Models routing

> **Status: POC** ([praxis-proxy/experimental#2](https://github.com/praxis-proxy/experimental/issues/2)).
> Built against NVIDIA NeMo Switchyard `=0.2.0` (pre-alpha).

Decision-only router: a judge classifies each request; Switchyard returns
`weak` / `strong`; the filter maps that tag to `(cluster, model)` and selects
the Praxis cluster. Switchyard never sees provider names.

## Flow

1. **`on_request_body`**: buffer JSON, decode OpenAI chat → Switchyard IR,
   drive `run_stream`, serve the judge `CallLlm` via `SubRequestClient`,
   rewrite `model`, stash cluster metadata.
2. **`on_request`**: apply `ctx.cluster` from metadata.

Metadata: `switchyard_route.cluster` on success; `switchyard_route.error` on
failure.

## Configuration

```yaml
- filter: switchyard_route
  judge:
    endpoint: "http://127.0.0.1:18091/v1/chat/completions"
    model: mock-switchyard-judge
    # auth:
    #   value_env: OPENAI_API_KEY
    timeout_ms: 5000
  threshold: 0.8
  targets:
    weak:
      cluster: weak-cluster
      model: mock-weak
    strong:
      cluster: strong-cluster
      model: mock-strong
  on_failure: open   # open | closed
```

- Path: `*/chat/completions` only.
- Secrets: `judge.auth.value_env` only (never inline).
- `on_failure: open` passes through; `closed` → HTTP 503.

## Demo

```console
cd demos/switchyard-route && ./run-demo.sh
```

Mock judge + echo upstreams. Easy → `served_by=weak-upstream`; hard →
`served_by=strong-upstream`. Details in [`demos/switchyard-route/`](../demos/switchyard-route/README.md).
