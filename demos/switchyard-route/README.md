# `switchyard_route` demo

Local end-to-end POC for the [`switchyard_route`](../../docs/switchyard-route.md)
filter: a mock Switchyard judge classifies each chat request as easy or hard,
then Praxis routes to a weak or strong echo upstream.

No Kubernetes cluster and no real LLM — only loopback mocks.

## What you should see

1. Three easy prompts → `served_by=weak-upstream`
2. Three hard prompts → `served_by=strong-upstream`
3. Gateway logs with `switchyard_route: judge verdict` / `routed`
4. Mock logs showing judge `p_solve` and which upstream answered

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
5. Sends 3 easy + 3 hard prompts and greps the logs

## Ports

| Role | Port | Behavior |
| --- | --- | --- |
| Gateway | `:18080` | Praxis + `switchyard_route` + `load_balancer` |
| Judge | `:18091` | Easy → `p_solve=0.95` / `SUP-1`; hard markers → `0.0` / `LIM-2` |
| Weak upstream | `:18092` | Echo `served_by=weak-upstream` |
| Strong upstream | `:18093` | Echo `served_by=strong-upstream` |

Hard prompts include markers such as `undocumented`, `blurry`, `whiteboard`
(see `_HARD_MARKERS` in `upstreams.py`). With `threshold: 0.8` in the demo
YAML, `0.95` routes weak and `0.0` routes strong.

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

## Layout (request path)

```text
Client
  → Gateway :18080
    → switchyard_route (judge callout :18091)
    → load_balancer
      → weak :18092  or  strong :18093
```

Filter docs: [`docs/switchyard-route.md`](../../docs/switchyard-route.md).
