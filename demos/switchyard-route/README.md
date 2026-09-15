# `switchyard_route` demo

Local end-to-end POC for the [`switchyard_route`](../../docs/switchyard-route.md)
filter: a mock Switchyard judge classifies each chat request as easy or hard,
then Praxis routes to a weak or strong echo upstream.

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
4. Mid-session: **Weak** turn, then judge down, then another easy turn on the
   **same** session → still Weak and a `switchyard_route: reuse` log.
   (A Strong floor never calls the judge, so judge-down after Strong is
   `floor_skip`, not `reuse`.)
5. A **new** session while the judge is still down → Strong with
   `switchyard_route: default_strong` (empty store; not written as a success)
6. Restart with `session_floor: disabled`: hard then easy on one session →
   Strong then Weak (`routed` both times; downgrade allowed)
7. Gateway logs with `judge verdict` / `routed` / `floor_skip` / `reuse` /
   `default_strong`
8. Mock logs showing judge `p_solve` and which upstream answered

The script fails if `floor_skip`, `reuse`, or `default_strong` are missing,
if the floor-stay prompt reached the judge, or if the disabled-easy prompt
did not reach the judge.

## Quick start

```console
cd demos/switchyard-route
./run-demo.sh
```

The script:

1. Starts `upstreams.py` (judge `:18091`, weak `:18092`, strong `:18093`)
2. Builds `praxis-experimental-server` if needed
3. Renders `praxis.yaml` from `praxis.yaml.template` with
   `session_floor: enabled`
4. Starts the gateway on `:18080`
5. Sends one-shot easy/hard prompts, the healthy-path floor sequence, then
   the mid-session judge-down scenario
6. Restarts the gateway with `session_floor: disabled` and sends Strong then
   Weak on one session
7. Greps the logs and checks the floor assertions

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
| `praxis.yaml.template` | Full Praxis config (placeholders for judge and `session_floor`) |
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
    → switchyard_route (judge callout :18091, skipped on floor_skip)
    → load_balancer
      → weak :18092  or  strong :18093
```

Filter docs: [`docs/switchyard-route.md`](../../docs/switchyard-route.md).
