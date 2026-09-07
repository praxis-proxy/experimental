# `switchyard_route` demo

Local end-to-end POC for the [`switchyard_route`](../../docs/switchyard-route.md)
filter: a mock Switchyard judge classifies each chat request as easy or hard,
then Praxis routes to a weak or strong echo upstream.

No Kubernetes cluster and no real LLM — only loopback mocks.

## What you should see

1. Three easy prompts → `served_by=weak-upstream`
2. Three hard prompts → `served_by=strong-upstream`
3. Mid-session: hard turn with `x-switchyard-session-id`, then judge down,
   then an easy turn on the **same** session → still `served_by=strong-upstream`
   and a `switchyard_route: reuse` log (not a fresh Weak verdict)
4. A **new** session while the judge is still down → Strong with
   `switchyard_route: default_strong` (empty store; not written as a success)
5. Gateway logs with `switchyard_route: judge verdict` / `routed` / `reuse` /
   `default_strong`
6. Mock logs showing judge `p_solve` and which upstream answered

## Quick start

```console
cd demos/switchyard-route
./run-demo.sh
```

The script:

1. Starts `upstreams.py` (judge `:18091`, weak `:18092`, strong `:18093`)
2. Builds `praxis-experimental-server` if needed
3. Renders `praxis.yaml` from `praxis.yaml.template`
4. Starts the gateway on `:18080`
5. Sends 3 easy + 3 hard prompts, then the mid-session judge-down scenario,
   and greps the logs

## Ports

| Role | Port | Behavior |
| --- | --- | --- |
| Gateway | `:18080` | Praxis + `switchyard_route` + `load_balancer` |
| Judge | `:18091` | Easy → `p_solve=0.95` / `SUP-1`; hard markers → `0.0` / `LIM-2`. `POST /control/down` and `/control/up` toggle 503s |
| Weak upstream | `:18092` | Echo `served_by=weak-upstream` |
| Strong upstream | `:18093` | Echo `served_by=strong-upstream` |

Hard prompts include markers such as `undocumented`, `blurry`, `whiteboard`
(see `_HARD_MARKERS` in `upstreams.py`). With `threshold: 0.8` in the demo
YAML, `0.95` routes weak and `0.0` routes strong.

The demo YAML uses `on_failure: open`. `closed` (HTTP 503, ignore the map) is
covered by unit tests, not this script.

## Files

| File | Role |
| --- | --- |
| `run-demo.sh` | One-shot demo driver |
| `upstreams.py` | Mock judge + weak/strong echo servers |
| `praxis.yaml.template` | Full Praxis config (placeholders for judge) |
| `praxis.yaml` | Generated at run time (gitignored) |
| `server.log` | Symlink to gateway log (gitignored) |

## Mocks only

To run the three mock servers without Praxis:

```console
python3 upstreams.py
```

Then `curl -X POST http://127.0.0.1:18091/control/down` to make the judge
return 503.

## Layout (request path)

```text
Client
  → Gateway :18080
    → switchyard_route (judge callout :18091)
    → load_balancer
      → weak :18092  or  strong :18093
```

Filter docs: [`docs/switchyard-route.md`](../../docs/switchyard-route.md).
