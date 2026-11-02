# `switchyard_route` demo

Local end-to-end POC for the [`switchyard_route`](../../docs/switchyard-route.md)
filter: a mock Switchyard judge classifies each inference request as easy or
hard, then Praxis routes to a weak or strong echo upstream. Buffered requests
use `iterative_request_router` to send eligible judge or selected-target
failures to one separately configured failure target. Streaming requests use a
direct terminal branch because the iterative router buffers complete responses.
The same listener accepts OpenAI Chat Completions, OpenAI Responses, and
Anthropic Messages. The stock Praxis AI translators normalize the latter two
request bodies before Switchyard, and translate the selected upstream response
back to the public protocol.

The Anthropic profile supports plain-text Messages content and untyped or
`custom` client tools. It rejects `thinking`, `context_management`, thinking
content blocks, and typed server tools with an HTTP `400` error. Other
Anthropic-specific features are outside this demo's contract.

No Kubernetes cluster and no real LLM — only loopback mocks.

## What you should see

1. Three easy prompts → `served_by=weak-upstream`
2. Three hard prompts → `served_by=strong-upstream`
3. Session floor (judge **up**, `session_floor: enabled`, same
   `x-switchyard-session-id`):
   - easy → Weak (`routed`)
   - hard → Strong (`routed`, floor can still rise)
   - easy again → Strong (`floor_skip`); the mock judge is **not** called.
     After that turn the script prints the gateway `floor_skip` line. The mock
     log must not contain `preview='Thanks, just say ok.'` — that would mean
     the judge saw the prompt.
4. Judge down before buffered routing → the local judge failure is sent to the
   failure target with exactly one judge attempt.
5. Selected Weak failure → the request is replayed once to the failure target.
6. A 1.1 MiB Chat request is replayed to the failure target after Weak fails.
7. Restart with `session_floor: disabled`: hard then easy on one session →
   Strong then Weak (`routed` both times; downgrade allowed)
8. Gateway logs with `judge verdict` / `routed` / `floor_skip`
9. Mock logs showing judge `p_solve` and which upstream answered
10. Responses and Anthropic finite-response translation
11. Responses and Anthropic SSE translation, including fragmented upstream
    writes
12. An OpenAI Responses 503 envelope when both the selected upstream and
    failure target are unavailable. Anthropic is checked through successful
    selected-target fallback, but not through the final-failure case.

The script fails if `floor_skip` is missing, if the floor-stay prompt reached
the judge, if judge-down caused anything other than one judge attempt, if the
disabled-easy prompt did not reach the judge, or if an expected status,
upstream, translated event, or fragmented first event is missing.

## Quick start

```console
cd demos/switchyard-route
./run-demo.sh
```

The default run is an automated check: it starts the gateway and mocks, runs
the scenarios, and shuts everything down when it finishes. To run the checks
and then keep the servers available for manual `curl` requests, use:

```console
KEEP_SERVERS=1 ./run-demo.sh
```

Leave that terminal open, send requests from a second terminal, and press
`Ctrl-C` in the first terminal when finished.

The script:

1. Starts `upstreams.py` (judge `:18091`, weak `:18092`, strong `:18093`,
   failure `:18094`)
2. Builds `praxis-experimental-server` if needed
3. Renders `praxis.yaml` from `praxis.yaml.template` with
   `session_floor: enabled`
4. Starts the gateway on `:18080`
5. Sends one-shot easy/hard prompts, the healthy-path floor sequence, then
   judge-down and selected-target failure scenarios
6. Restarts the gateway with `session_floor: disabled` and sends Strong then
   Weak on one session
7. Greps the logs and checks the floor assertions

## Ports

| Role | Port | Behavior |
| --- | --- | --- |
| Gateway | `:18080` | Praxis + Switchyard + iterative failure routing |
| Judge | `:18091` | Easy → `p_solve=0.95` / `SUP-1`; hard markers → `0.0` / `LIM-2`. `POST /control/down` and `/control/up` toggle 503s |
| Weak upstream | `:18092` | Echo `served_by=weak-upstream` |
| Strong upstream | `:18093` | Echo `served_by=strong-upstream` |
| Failure upstream | `:18094` | Echo `served_by=failure-upstream` |

Hard prompts include markers such as `undocumented`, `blurry`, `whiteboard`
(see `_HARD_MARKERS` in `upstreams.py`). With `threshold: 0.8` in the demo
YAML, `0.95` routes weak and `0.0` routes strong.

The buffered primary step uses `on_failure: open` with `failure_scope: both`.
The scope changes judge failures into a local 503 that IRR can transition from;
selected-target failures are independently classified by IRR's `on_result`
rules. This makes the scope override observable instead of relying on
`on_failure: closed` to produce the same result.

IRR steps do not inherit headers. The failure step configures its JSON
representation and `failure-cluster` credential with
`SWITCHYARD_FAILURE_API_KEY` (default: `demo-failure-key`).

After normalization, protocol translators retain verified state in request
extensions. This lets the buffered Switchyard instance accept translated
Responses and Messages requests without accepting a public path as proof.

`stream: true` requests cannot use this fallback path: IRR buffers terminal
responses and does not support incremental SSE. A `request_scope: streaming`
Switchyard instance writes `status=selected`, a terminal branch sends the
request through the ordinary load balancer, and the parent chain stops before
IRR. Streaming therefore preserves chunk delivery but a streaming target
failure is returned directly rather than retried at the failure target. The
streaming and buffered instances also have separate in-process session maps in
this POC.

## Files

| File | Role |
| --- | --- |
| `run-demo.sh` | One-shot demo driver |
| `upstreams.py` | Mock judge + weak/strong/failure echo servers |
| `praxis.yaml.template` | Full Praxis config (placeholders for judge and `session_floor`) |
| `praxis.yaml` | Generated at run time (gitignored) |
| `server.log` | Symlink to gateway log (gitignored) |

## Mocks only

To run the four mock servers without Praxis:

```console
python3 upstreams.py
```

Then `curl -X POST http://127.0.0.1:18091/control/down` to make the judge
return 503.

## Layout (request path)

```text
Client
  → Gateway :18080
  → protocol translator (Responses/Anthropic only)
    → stream=true: scoped switchyard_route → terminal direct branch
      → rewrite → weak :18092 or strong :18093 (incremental SSE)
    → buffered: iterative_request_router (max 2 iterations)
      → primary: switchyard_route → rewrite → weak :18092 or strong :18093
      → failure: failure-target rewrite → failure :18094
```

Filter docs: [`docs/switchyard-route.md`](../../docs/switchyard-route.md).
