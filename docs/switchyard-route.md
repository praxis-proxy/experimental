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

For a separately configured failure target on buffered requests, place
`switchyard_route` inside an `iterative_request_router` primary step and set
`failure_scope` to include `judge`. A judge failure then becomes a local 503
that gives the router a transition point; the standalone default remains
fail-open.

## Flow

1. **`on_request_body`**: buffer JSON, require a supported inference endpoint
   (a path ending in `/chat/completions`, exact `/v1/responses`, or exact
   `/v1/messages`), derive the session
   key, and expect Responses/Anthropic bodies to have been normalized to Chat
   Completions by an upstream Praxis AI translator.
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

> **No absolute lifetime:** active sessions only expire by idle TTL (30
> minutes without a request). A session that keeps receiving turns never
> expires and is never re-judged. Absolute lifetime and re-evaluation are
> future work ([#3](https://github.com/praxis-proxy/experimental/issues/3))
> — re-evaluation in particular needs design, because the judge classifies a
> single turn, not the full conversation context.

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
  failure_scope: target # target (default) | judge | both
  request_scope: all    # all (default) | streaming
  session_floor: enabled  # enabled (default) | disabled
  max_body_bytes: 1048576 # 1 MiB default; requests above this receive 413
```

- Path: Chat Completions accepts any path ending in `/chat/completions` to
  preserve the original Switchyard contract. Responses accepts exact
  `/v1/responses` and Anthropic Messages accepts exact `/v1/messages` (each
  also accepts one trailing slash); the current Praxis AI translators require
  those exact public paths. Responses and Anthropic requests require their
  upstream protocol translator before this filter; the public path remains
  available for downstream path and response translation.
- The local demo exercises finite JSON responses, SSE streams, and upstream
  `503` errors for Responses and Anthropic, in addition to the existing Chat
  routing scenarios.

### Failure target through `iterative_request_router`

The experimental filter `switchyard_failure_target` applies the one logical
`cluster`/`model` pair configured by the fallback step. It does not call the
judge and it is not included in normal Weak/Strong selection. The router owns
the bounded orchestration:

```yaml
- filter: iterative_request_router
  initial_step: primary
  max_iterations: 2
  steps:
    - name: primary
      filters:
        - filter: switchyard_route
          # judge, targets, and session settings as above
          on_failure: open
          failure_scope: both
        - filter: url_rewrite
          operations:
            - regex_replace:
                pattern: "^/v1/(responses|messages)/?$"
                replacement: "/v1/chat/completions"
        - filter: load_balancer
          # Weak and Strong clusters
      on_result:
        - origin: local
          status: [503]
          next: failure
        - origin: transport
          next: failure
        - origin: upstream
          status: [500, 502, 503, 504]
          next: failure
        - default: true
          done: true
    - name: failure
      filters:
        - filter: switchyard_failure_target
          cluster: failure-cluster
          model: fallback-model
        # IRR steps do not inherit headers; configure this target independently.
        - filter: headers
          request_set:
            - name: Content-Type
              value: application/json
            - name: Accept
              value: application/json
        - filter: credential_injection
          clusters:
            - name: failure-cluster
              header: Authorization
              env_var: FAILURE_UPSTREAM_API_KEY
              header_prefix: "Bearer "
        - filter: url_rewrite
          operations:
            - regex_replace:
                pattern: "^/v1/(responses|messages)/?$"
                replacement: "/v1/chat/completions"
        - filter: load_balancer
          # failure-cluster endpoints
      on_result:
        - default: true
          done: true
```

`max_iterations: 2` means primary plus one failure-target attempt. The failure
step has no transition to another step, so a failure-target error cannot create
a third attempt. `origin: upstream` excludes locally generated errors such as
the token-limit 429; status matching also excludes client errors and other
4xx responses. A response that has already exposed headers/body is not replaced.

The request body is buffered by the router and replayed to the failure step.
IRR steps do not inherit headers, so each target configures its own
representation and credentials. The adapter changes only `model`, binds the
configured `cluster`, and leaves token accounting to the existing Praxis
accounting path. The demo validates judge failure, selected-target failure,
one fallback attempt, and the final Responses failure response.

`switchyard_failure_target` buffers up to Praxis's 64 MiB JSON ceiling. It can
therefore replay every body admitted by `switchyard_route.max_body_bytes`,
which is validated against the same ceiling. It has no separate body-limit
setting.

IRR nested steps receive fresh metadata. `switchyard_route` preserves verified
translator state in request extensions and accepts it only for the matching
public path; a public path alone never enables routing.

IRR also buffers the complete terminal response, so it cannot preserve SSE
chunk timing. The demo keeps `stream: true` requests out of IRR: an outer
`switchyard_route` with `request_scope: streaming` emits the
`status=selected` filter result, a terminal branch performs the normal path
rewrite and load balancing, and the buffered path falls through to IRR.
Streaming keeps incremental delivery but does not receive failure-target
replay. Because this POC uses two filter instances, their in-process session
maps are separate.

#### Failure scope and precedence

`failure_scope` is an explicit enum for the failure target's intended scope:

| Scope | Judge failure | Selected-target failure |
| --- | --- | --- |
| `judge` | Produces local 503 instead of applying `on_failure`. | Not controlled by this field. |
| `target` | Existing `on_failure` behavior remains in effect. | Not controlled by this field. |
| `both` | Produces local 503 instead of applying `on_failure`. | Not controlled by this field. |

`failure_scope` controls only judge-side behavior inside `switchyard_route`.
The selected-target side is controlled entirely by the primary step's IRR
`on_result` rules. For judge-only fallback, include only the local-503
transition. For target-only fallback, keep `failure_scope: target` and include
only transport/upstream transitions. For both, include all three as in the
example. When omitted, the field defaults to `target`, preserving the existing
judge `on_failure` behavior. `failure_scope` does not disable target transitions
that are present in IRR configuration.

`request_scope: streaming` is an opt-in composition aid: that filter instance
ignores bodies without `stream: true`. Its default is `all`, so existing
configurations and the Switchyard instance inside IRR are unchanged.

- Secrets: `judge.auth.value_env` only (never inline).

### `max_body_bytes`

The maximum translated Chat Completions request body that Switchyard accepts.
It defaults to 1 MiB. A larger body is rejected with HTTP `413 Payload Too
Large` before Switchyard parses it, calls the judge, or selects an upstream;
this is independent of `on_failure`.

This setting controls Switchyard only. It directly limits Chat and can allow
larger Responses requests, subject to the outer transport limit. Anthropic
filters run before Switchyard and default to 1 MiB, so raising this value alone
does not enlarge Messages requests; raise their matching limits and the outer
transport cap too.

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

An unrelated path or unparsable JSON still fail-open unrouted or 503; those
requests are not rewritten to a sticky tier.

A Strong floor skips the judge, so a down judge on that session is
`floor_skip`, not `reuse`. This table applies only when the judge (or decode)
actually runs and fails.

> **Observability note:** a Strong floor makes judge outages invisible on
> that session — no error metadata, no `reuse`, just `floor_skip`. Sessions
> still at Weak or with no entry call the judge normally, so `on_failure`
> applies there. Health monitoring should check the judge endpoint directly,
> not rely on routing errors alone.

## Demo

```console
cd demos/switchyard-route && ./run-demo.sh
```

Mock judge + echo upstreams. Easy → `served_by=weak-upstream`; hard →
`served_by=strong-upstream`. On one session with the judge up: easy → Weak,
hard → Strong, easy again stays Strong (`floor_skip`, no judge call). A judge
failure or selected-target failure goes once to
`served_by=failure-upstream`; a failure-target failure returns the final error.
A restart with `session_floor: disabled` allows Strong → Weak on the next easy
turn. Details in
[`demos/switchyard-route/`](../demos/switchyard-route/README.md).
