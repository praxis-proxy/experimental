# `switchyard_route`: Capability-mode Mixture-of-Models routing

> **Status: POC** ([praxis-proxy/experimental#2](https://github.com/praxis-proxy/experimental/issues/2),
> mid-session failure: [#19](https://github.com/praxis-proxy/experimental/issues/19),
> session floor: [#20](https://github.com/praxis-proxy/experimental/issues/20)).
> Built against NVIDIA NeMo Switchyard `=0.2.0` (pre-alpha).

Decision-only router: a judge classifies a turn unless the session floor
already answers it. Switchyard returns `weak` / `strong`; the filter maps
that tag to `(cluster, model)` and selects the Praxis cluster. Switchyard
never sees provider names.

**Session floor** (default: enabled): once a session reaches Strong, it stays
Strong for later turns. The judge is skipped when the stored tier is already
Strong (`decision=floor_skip`). Weak still calls the judge and may rise.

If the judge fails mid-chat and `on_failure: open`, a stored success is
reused (`reuse`). An empty store serves Strong once without writing the map
(`default_strong`).

## Flow

1. **`on_request_body`**: buffer JSON, require `*/chat/completions`, derive
   the session key.
   - If `session_floor` is enabled and the stored tier is Strong: rewrite
     Strong, set `decision=floor_skip`, refresh idle TTL. The judge is not
     called and the map is not rewritten.
   - Otherwise: decode OpenAI chat → Switchyard IR, drive `run_stream`,
     serve the judge `CallLlm` via `SubRequestClient`, rewrite `model`,
     stash cluster metadata. On a live judge success, write the map (highest
     tier when the floor is enabled; last verdict when it is disabled).
   A rewrite failure on either path follows `on_failure` (`unrouted` or 503).
2. **`on_request`**: apply `ctx.cluster` from metadata.

### Metadata

| Key | When |
| --- | --- |
| `switchyard_route.cluster` | A Weak/Strong cluster was applied (live, floor_skip, reuse, or default Strong) |
| `switchyard_route.decision` | `routed` / `floor_skip` / `reuse` / `default_strong` / `rejected` / `unrouted` |
| `switchyard_route.error` | Set on any routing failure (`reuse`, `default_strong`, `rejected`, `unrouted`). Not set on `routed` or `floor_skip`. |

Logs use the same tokens: `switchyard_route: routed`, `floor_skip`, `reuse`,
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

The map holds the **floor** (highest tier seen) per session key when
`session_floor` is enabled. Tier can only go up, never down. When
`session_floor` is disabled, the map stores the last live judge success and
may drop. Default Strong and 503 are never written. Idle TTL is 30
minutes; cap is 10,000 entries (LRU). Lost on process restart or replica hop
([#3](https://github.com/praxis-proxy/experimental/issues/3)). Two chats
without a header that start with the same user line share a key.

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
  on_failure: open      # open | closed
  session_floor: enabled  # enabled (default) | disabled
```

- Path: `*/chat/completions` only.
- Secrets: `judge.auth.value_env` only (never inline).

### `session_floor`

| Mode | Behavior |
| --- | --- |
| `enabled` (default) | Once a session reaches Strong, later turns stay Strong and skip the judge (`decision=floor_skip`). While the stored tier is still Weak, the judge still runs and may raise the floor. |
| `disabled` | Every turn gets a fresh judge decision. The request and the stored tier can drop from Strong to Weak. |

### `on_failure`

| Mode | Judge failed, map has a success | Judge failed, empty store |
| --- | --- | --- |
| `open` | Reuse last Weak or Strong (`decision=reuse`) | Serve Strong, do not write the map (`decision=default_strong`) |
| `closed` | HTTP 503; map is ignored | HTTP 503 |

Wrong path or unparsable JSON still fail-open unrouted or 503; those
requests are not rewritten to a sticky tier.

A Strong floor skips the judge, so a down judge on that session is
`floor_skip`, not `reuse`. This table applies only when the judge (or decode)
actually runs and fails.

## Demo

```console
cd demos/switchyard-route && ./run-demo.sh
```

Mock judge + echo upstreams. Easy → `served_by=weak-upstream`; hard →
`served_by=strong-upstream`. On one session with the judge up: easy → Weak,
hard → Strong, easy again stays Strong (`floor_skip`, no judge call). A Weak
session with the judge down reuses Weak (`reuse`). A new session while the
judge is down gets default Strong. A restart with `session_floor: disabled`
allows Strong → Weak on the next easy turn. Details in
[`demos/switchyard-route/`](../demos/switchyard-route/README.md).
