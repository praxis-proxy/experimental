# switchyard_route local demo

A self-contained demonstration of the `switchyard_route` filter (see
`docs/switchyard-route.md`) with a **real** judge model and stubbed
upstreams:

- The **judge is real**: point `JUDGE_ENDPOINT` / `JUDGE_MODEL` at any
  OpenAI-compatible chat-completions server (Ollama, vLLM, LM Studio, …).
  The Capability classifier sends its packaged system prompt, your
  messages, and a `json_schema` response format, and reads the model's
  verdict.
- The **upstreams are stubbed** (`upstreams.py`): two loopback echo servers
  standing in for the weak/strong clusters, so the chosen cluster and the
  rewritten `model` field are plainly visible without needing two real
  model deployments.

## Run

```console
$ JUDGE_ENDPOINT=http://127.0.0.1:11434/v1/chat/completions \
  JUDGE_MODEL=qwen3:8b \
  ./run-demo.sh
```

The script renders `praxis.yaml` from `praxis.yaml.template`, starts the
upstreams and the composed `switchyard-server` (gateway on
`127.0.0.1:18080`), and runs four turns: an easy question (expect
`weak-upstream` / `qwen-mini`), a hard question (expect `strong-upstream` /
`qwen-max`), the same easy question again in the same session (the
no-downgrade floor must hold `strong`), and an easy question in a fresh
session (isolated; back to `weak`).

Filter logs (decisions and fail-open reasons) land in `server.log`.

Note: turns 1 and 4 depend on the judge model's actual verdict — a weak
judge model may classify conservatively. Turn 3 demonstrates the floor
regardless, as long as turn 2 reached `strong`.

## Fail-open behaviour

Stop the judge (or point `JUDGE_ENDPOINT` at a dead port) and re-run a
turn: the filter logs `switchyard_route: routing unavailable`, forwards the
body unmodified, and selects no cluster — in this chain the load balancer
then returns 500 (`no cluster set in context`). See "Failure topology" in
`docs/switchyard-route.md` for why, and when to prefer `on_failure: closed`.
