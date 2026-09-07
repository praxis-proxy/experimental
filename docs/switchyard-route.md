# `switchyard_route`: Capability-mode Mixture-of-Models routing

> **Status: POC** ([praxis-proxy/experimental#2](https://github.com/praxis-proxy/experimental/issues/2),
> mid-session failure: [#19](https://github.com/praxis-proxy/experimental/issues/19)).
> Built against NVIDIA NeMo Switchyard `=0.2.0` (pre-alpha).

Decision-only router: a judge classifies each request; Switchyard returns
`weak` / `strong`; the filter maps that tag to `(cluster, model)` and selects
the Praxis cluster. Switchyard never sees provider names.

A live judge success is remembered per session (in-process). If the judge
fails later in the same chat, `on_failure: open` reuses that tier instead of
thrashing or continuing unrouted.

## Flow

1. **`on_request_body`**: buffer JSON, derive the session key, decode OpenAI
   chat → Switchyard IR, drive `run_stream`, serve the judge `CallLlm` via
   `SubRequestClient`, rewrite `model`, stash cluster metadata. On a real
   success, store the tier for that key.
2. **`on_request`**: apply `ctx.cluster` from metadata.

### Metadata

| Key | When |
| --- | --- |
| `switchyard_route.cluster` | A Weak/Strong cluster was applied (live, reuse, or default Strong) |
| `switchyard_route.decision` | `routed` / `reuse` / `default_strong` / `rejected` / `unrouted` |
| `switchyard_route.error` | Judge/decode failure (also present on reuse and default Strong) |

Logs use the same tokens: `switchyard_route: routed`, `reuse`,
`default_strong`, `routing failed`, `fail-open`.

## Session key

Recomputed on every request (the key is not stored as a token to compare):

1. If `x-switchyard-session-id` is present and non-empty, that is the key
   (Switchyard-style sticky name). Values longer than 256 characters are
   truncated.
2. Otherwise hash the system prompt (if any) plus the **first** user message
   in JSON `messages`. Follow-up turns must keep that opening line in the
   history, or send the header.
3. Neither → no session. The turn behaves like an empty store.

The map holds only `key → last real judge success`. Default Strong and 503
are never written. Idle TTL is 30 minutes; cap is 10_000 entries (LRU).
Lost on process restart or replica hop
([#3](https://github.com/praxis-proxy/experimental/issues/3)). Two chats
without a header that start with the same user line share a key.

Healthy-path “no downgrade” while the judge is up is
[#20](https://github.com/praxis-proxy/experimental/issues/20), not this filter
path.

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

### `on_failure`

| Mode | Judge failed, map has a success | Judge failed, empty store |
| --- | --- | --- |
| `open` | Reuse last Weak or Strong (`decision=reuse`) | Serve Strong, do not write the map (`decision=default_strong`) |
| `closed` | HTTP 503; map is ignored | HTTP 503 |

Wrong path or unparsable JSON still fail-open unrouted or 503; those
requests are not rewritten to a sticky tier.

## Demo

```console
cd demos/switchyard-route && ./run-demo.sh
```

Mock judge + echo upstreams. Easy → `served_by=weak-upstream`; hard →
`served_by=strong-upstream`. After a Strong session turn, the script takes
the judge down: the next easy turn on the same
`x-switchyard-session-id` stays Strong (`reuse`), and a new session while
the judge is down gets default Strong. Details in
[`demos/switchyard-route/`](../demos/switchyard-route/README.md).
