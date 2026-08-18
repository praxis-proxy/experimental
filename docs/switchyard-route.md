# `switchyard_route`: Capability-mode Mixture-of-Models routing

> **Status: POC** (praxis-proxy/experimental#2, Track A item 2 of
> praxis-proxy/ai#758, discussion praxis-proxy/praxis#976). Built against
> NVIDIA NeMo Switchyard `=0.2.0` (pre-alpha; git tag `v0.2.0`, commit
> `1fc9ab887d1c663b0048ae24d5f473d15ed8daaa`). Any version bump is a
> deliberate change: re-run `make audit` and re-verify the API notes in the
> module docs.

`switchyard_route` embeds Switchyard's `LlmTaskClassifier` (Capability mode)
in the Praxis request path as a **decision-only** router. One cheap judge
callout classifies each request (easy vs hard); Switchyard names an abstract
tier tag (`weak` / `strong`); the filter resolves that tag through its own
config table into a real `(cluster, model)` pair, rewriting the request
body's `model` field *and* selecting the Praxis cluster. The answer is served
once, by the real upstream — the filter never serves it.

Switchyard only ever sees the abstract tags. Repointing `strong` from one
provider to another is a one-line config change; Switchyard is untouched.

## How it works

With `BodyMode::StreamBuffer`, the buffered-body hook runs *before*
`on_request`:

1. `on_request_body` (end of stream): parse the JSON body, detect the wire
   format (OpenAI chat completions or Anthropic messages, by path first and
   body shape as fallback), decode it into the Switchyard IR, and drive
   `run_stream` until the routed `Step::Decision`. The judge `CallLlm` step
   is served through the server-shared `SubRequestClient` against the
   configured judge endpoint; the stream is dropped at the decision, before
   any answer call. The chosen tier's model is written into the body in
   place (provider-specific fields preserved — no IR round-trip), and the
   chosen cluster is stashed in filter metadata.
2. `on_request`: applies the stashed cluster to `ctx.cluster` (preserving a
   cluster an earlier filter already chose).

Observability metadata: `switchyard_route.cluster`, `.tier`, `.model` on
success; `switchyard_route.error` on the failure path.

## Configuration

```yaml
filter_chains:
  - name: routing
    filters:
      - filter: switchyard_route
        judge:                       # required — the classifier callout
          endpoint: "http://judge.internal:8000/v1/chat/completions"
          model: qwen3-judge         # model id sent to the judge
          timeout_ms: 2000           # DNS + HTTP deadline (default 2000)
          max_response_bytes: 65536  # judge reply cap (default 64 KiB)
        threshold: 0.5               # classifier base threshold, [0,1]
        targets:                     # required — exactly weak + strong
          weak:
            cluster: local-vllm      # -> ctx.cluster
            model: qwen2.5-7b        # -> body["model"]
          strong:
            cluster: openai-frontier
            model: gpt-4o
        session_floor:               # host-owned no-downgrade ratchet
          enabled: true              # default true
          ttl_secs: 3600             # inactivity eviction (default 1 h)
          exclude_below: true        # also bar weak inside Switchyard
        on_failure: open             # open | closed (default open)
        max_body_bytes: 1048576      # StreamBuffer cap (default 1 MiB)
        session_header: x-switchyard-session-id
      - filter: load_balancer
        clusters:
          - name: local-vllm
            endpoints: ["10.0.1.1:8000"]
          - name: openai-frontier
            endpoints: ["10.0.2.1:8443"]
```

Notes:

- The failure knob is **`on_failure`**, not `failure_mode`: `failure_mode`
  is a structural key of Praxis's pipeline `FilterEntry` wrapper and is
  stripped before the filter sees its config.
- There is deliberately **no `default_tier`**: on failure the filter never
  forces a tier (see below).
- Bodies over `max_body_bytes` are rejected with 413 by the `StreamBuffer`
  machinery itself, before the filter runs.

## The no-downgrade guarantee

**Requirement:** once a session has routed to `strong`, a later turn must
never be silently downgraded to `weak`.

Switchyard v0.2.0 cannot provide this. Its Capability classifier is
per-request/stateless; `session_affinity: true` is a *first-decision-wins*
latch (it can pin a session to `weak` and block a needed upgrade); its
session state is in-process, TTL-bound, and not seedable from outside. The
filter therefore owns the guarantee, in two layers:

1. **Don't overwrite on failure.** On *any* failure (bad body, unknown
   format, judge callout error, missing subrequest client, decision-less
   stream) the request passes through with the client's own `model`
   untouched and no cluster set. The filter can never *cause* a downgrade by
   clobbering a good model. This is stateless and survives everything.
2. **Session floor.** An in-process `session → tier` map (keyed by
   `session_header`, TTL-evicted, dropped on
   `x-switchyard-session-final: true`). Every decision is clamped to
   `max(floor, decision)` — the ratchet only moves up. With
   `exclude_below: true` the below-floor tier is additionally excluded
   inside Switchyard for that turn.

**Honesty note for operators:** the floor cache has the same loss profile as
Switchyard's own state — a process restart, config reload, failover, or
replica hop wipes it. In those cases layer 1 degrades the behaviour to
"route as the request already asked", never "downgrade below what the client
asked". A durable floor store (Redis/KV behind the `SessionFloorStore` seam
in `floor.rs`) is the planned follow-up that makes the strict ratchet hold
across process boundaries.

Judge failures inside Switchyard would default the decision to the capable
tier; the filter deliberately does not rely on that — a judge transport
error takes the host's failure path (pass through unmodified) so the outcome
is owned here, not by library defaulting.

## Failure topology: what `open` really does

`on_failure: open` means the filter forwards the request unmodified — it
does not select a cluster. What happens next depends on the chain:

- If `switchyard_route` is the **only** cluster selector before
  `load_balancer` (the demo topology below), the load balancer fails the
  request: `no cluster set in context` → HTTP 500. Praxis rejects chains
  with a second cluster selector, so there is no in-chain fallback route.
- `on_failure: closed` rejects deliberately with **503** and a clear body —
  for this topology it is the clearer choice if you prefer explicit errors.

In other words: `open` guarantees the *optimizer can never rewrite your
request wrongly*; it cannot conjure a route where none exists. Deployments
that need judge-outage survivability should front a default route at a
different layer.

## Local demo (verified)

Everything lives in `hack/switchyard-demo/`: `praxis.yaml` (gateway on
`127.0.0.1:18080`, judge callout to `:18091`, weak/strong clusters on
`:18092`/`:18093`) and `stubs.py` (a stdlib-only judge + two echo
upstreams; the judge routes to `strong` when the newest user message
contains the word "hard").

```console
$ cargo build -p switchyard-server
$ cd hack/switchyard-demo
$ python3 stubs.py &
$ ../../target/debug/switchyard-server &   # reads ./praxis.yaml
```

Transcript (recorded 2026-08-18 against this revision):

```console
$ curl -s -X POST http://127.0.0.1:18080/v1/chat/completions \
    -H 'content-type: application/json' \
    -H 'x-switchyard-session-id: demo-A' \
    -d '{"model":"agent-default","messages":[{"role":"user","content":"what is 2+2?"}]}'
{"served_by": "weak-upstream", "model_received": "qwen-mini"}

$ # hard question, same session -> strong
$ curl -s ... -H 'x-switchyard-session-id: demo-A' \
    -d '{"model":"agent-default","messages":[{"role":"user","content":"prove a hard novel theorem in algebraic topology"}]}'
{"served_by": "strong-upstream", "model_received": "qwen-max"}

$ # easy question again, same session -> the floor HOLDS strong
$ curl -s ... -H 'x-switchyard-session-id: demo-A' \
    -d '{"model":"agent-default","messages":[{"role":"user","content":"what is 3+3?"}]}'
{"served_by": "strong-upstream", "model_received": "qwen-max"}

$ # fresh session is isolated -> weak again
$ curl -s ... -H 'x-switchyard-session-id: demo-B' \
    -d '{"model":"agent-default","messages":[{"role":"user","content":"what is 4+4?"}]}'
{"served_by": "weak-upstream", "model_received": "qwen-mini"}
```

With the judge stopped (upstreams still running), the same request logs
`switchyard_route: routing unavailable ... Connection refused` at the
filter, passes through unmodified, and the load balancer returns 500 (`no
cluster set in context`) — the topology consequence described above.

## Scope and follow-ups

- **Capability mode only.** Passthrough/Random make no LLM decision
  (Praxis already load-balances); Escalation cannot be decision-only (its
  verdict needs the efficient model's actual answer). Documented, not built.
- **Two tiers** (`weak` < `strong`); N-way buckets would be Switchyard's
  `Custom` path — follow-up territory.
- **Durable session-floor store** (Redis/KV adapter behind
  `SessionFloorStore`) — follow-up issue; required for the strict ratchet
  across restarts/replicas.
