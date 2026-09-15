# AI Gateway: real models, token budgets, span-derived dashboards

Praxis in front of a **real local model** (Ollama), enforcing **token budgets**,
with Grafana dashboards built from Prometheus counters *and* Tempo span metrics
— including per-filter latency, which a trace waterfall can only show one
request at a time.

Runs on its own KIND cluster with ports offset from
[../otel-benchmark](../otel-benchmark), so both demos can be up at once.

| Demo | Question it answers |
| --- | --- |
| `otel-benchmark` | What does OTel tracing cost? |
| `ai-gateway` (this one) | What does a real AI gateway do, and which filter costs what? |

## Prerequisites

Docker or Podman, [KIND](https://kind.sigs.k8s.io/), [Helm](https://helm.sh/),
`python3`, the [Praxis Forge CLI](https://github.com/praxis-proxy/forge)
(`cargo install --locked --git https://github.com/praxis-proxy/forge`), and
[Ollama](https://ollama.com) **0.33+** — older versions fail with
`412: requires a newer version of Ollama`.

```bash
helm repo add prometheus-community https://prometheus-community.github.io/helm-charts
helm repo add grafana https://grafana.github.io/helm-charts && helm repo update
ollama pull qwen3.8:27b     # agent work, and the model the budget rules key on
ollama pull qwen3.5:0.8b    # the rate-limit burst in step 4
# optional — the other two models with an agent-daily budget:
# ollama pull qwen3-coder:30b
# ollama pull deepseek-r1:32b
ollama serve                # leave running on its DEFAULT loopback binding
```

> **Leave Ollama on `127.0.0.1`.** A KIND pod reaches it via
> `host.docker.internal`, which the container runtime proxies from the host
> side. `OLLAMA_HOST=0.0.0.0` is unnecessary and publishes your models to the
> local network. Apple Metal is not reachable from Linux containers, so the
> model server stays native and only the gateway is containerized.

## 1. Get the image

Built locally today. Once praxis-proxy/experimental#22 merges and this PR lands,
`ghcr.io/praxis-proxy/experimental:main` carries the same binary — then set that
as the `image:` in `manifests/praxis.yaml` and skip the build and `kind load`.

From the repository root:

```bash
docker build --build-arg FEATURES=otel -t praxis-experimental:ai-gw -f Containerfile .
docker inspect --format '{{index .Config.Labels "io.praxis.build.features"}}' praxis-experimental:ai-gw
```

## 2. Bring it up

```bash
cd demos/ai-gateway
praxis-forge up --config forge.yaml
kind load docker-image praxis-experimental:ai-gw --name ai-gw-local
kubectl config use-context kind-ai-gw-local
for s in prometheus tempo otel-collector praxis-deploy dashboards; do
  praxis-forge apply --config forge.yaml local "$s"
done
```

> `use-context` matters if you also run `../otel-benchmark`. Forge scopes its
> `manifest`, `helm` and `wait` steps to the cluster, but `exec` steps inherit
> the ambient kubectl context.

## 3. Verify

```bash
curl -s -o /dev/null -w 'gateway %{http_code}\n' http://localhost:38080/v1/models
curl -s -o /dev/null -w 'grafana %{http_code}\n' http://localhost:33000/login

curl -s http://localhost:38080/v1/chat/completions \
  -H 'Content-Type: application/json' \
  -d '{"model":"qwen3.8:27b","max_tokens":300,"messages":[{"role":"user","content":"say hi"}]}'
```

> `qwen3.8` is a **reasoning** model: it returns a separate `reasoning` field and
> those tokens count toward `completion_tokens`. With a small `max_tokens` the
> whole budget goes to reasoning and `content` comes back empty. That is not a
> failure — use `max_tokens: 300` or more.

## 4. Hit a real rate limit

```bash
bash scripts/rate-limit-demo.sh
```

Sends 100 requests as `free`, waits for the sliding window to age out, then the
same 100 as `premium`:

```text
  100 requests as tier: free
  200 OK            53
  429 rate limited  47

  100 requests as tier: premium
  200 OK           100
  429 rate limited   0
```

The 429 carries `Retry-After` and `X-RateLimit-{Limit,Remaining,Reset}-Tokens`,
and is decided at *admission* — before the upstream call — so a denied request
costs no GPU time.

> **Why a small model here.** The burst has to land inside the 1m window.
> `qwen3.5:0.8b` answers in ~0.3s; `qwen3.8:27b` takes 74-78s under concurrency,
> so the budget would age out faster than it was consumed.

## 5. The budget rules

`configs/token-budget.yaml`, evaluated in order, first match wins:

| Rule | Match | Budget |
| --- | --- | --- |
| `premium` | `X-Tier: premium` | 20,000 tokens/**min** |
| `agent-daily`* | `X-Model: qwen3.8:27b` | 10,000,000 tokens/**day** |
| `free` | catch-all | 5,000 tokens/**min** |

\* plus identical rules for `qwen3-coder:30b` and `deepseek-r1:32b`. Rules match
one exact header value each, so a model needs its own rule to get an agent
budget; the two small models are deliberately left on the `free` tier.

`agent-daily` is the same filter with a longer window — that is all a "total
budget" is here. `window` takes `ms`/`s`/`m`/`h`: a day is `"24h"`, a week
`"168h"`. **`"7d"` is rejected**; praxis logs `invalid duration '7d'`, refuses
the reload and keeps running on the previous config.

It matches `X-Model`, which `model_to_header` promotes from the request body, so
it applies to Codex, opencode and Claude Code without any of them sending a
custom header — none of them let you.

**Size reservations from the harness, not from what you type.** A bare `curl`
costs ~406 tokens; one trivial Codex turn cost **9,471**, because system prompts
and tool schemas dominate. At `reserved_tokens: 1500` that recorded 7,971 tokens
of overage over four turns; at 10,000 a turn settles as
`estimated 10000 / actual 9435 / refunded 565 / overage 0`.

> `token_rate_limit` does **not** authenticate. A header-matched rule trusts
> whatever reached it, so a real deployment needs an auth filter to set the tier
> header and strip client copies. Tracked at grid#101.

## 6. Point a coding agent at it

**Codex** — add the provider (safe to re-run):

```bash
mkdir -p ~/.codex
grep -q '^\[model_providers.praxis\]' ~/.codex/config.toml 2>/dev/null \
  || cat >> ~/.codex/config.toml <<'EOF'

[model_providers.praxis]
name = "Praxis to local Ollama"
base_url = "http://localhost:38080/v1"
wire_api = "responses"
env_key = "OPENAI_API_KEY"
EOF
```

```bash
OPENAI_API_KEY=dummy codex -c model_provider=praxis --model qwen3.8:27b
```

`OPENAI_BASE_URL` alone does **not** redirect Codex — it reads the endpoint from
the provider block and the key from `~/.codex/auth.json`, so without
`-c model_provider=...` you silently hit api.openai.com and get a 404 for a
model OpenAI has never heard of. `wire_api` must be `"responses"`; Codex 0.153+
rejects `"chat"`. Do not name a provider `ollama`, `lmstudio` or `openai` —
those are reserved built-in IDs and Codex refuses to start.

**opencode** — writes the provider and preselects it, so opencode opens ready
to use. It consolidates into whichever config file already exists; having both
`opencode.json` and `opencode.jsonc` is ambiguous and makes providers silently
fail to appear.

> Unlike the Codex block this **rewrites** the target file: it re-serialises it
> as plain JSON, so comments in a `.jsonc` are lost, and it deletes the other
> file once merged. Back up an existing config first.

```bash
python3 - <<'EOF'
import json, pathlib
d = pathlib.Path.home() / ".config" / "opencode"
d.mkdir(parents=True, exist_ok=True)
jsonc, plain = d / "opencode.jsonc", d / "opencode.json"
target = jsonc if jsonc.exists() else plain
cfg = {}
for f in (plain, jsonc):
    if f.exists() and f.read_text().strip():
        try: cfg.update(json.loads(f.read_text()))
        except ValueError: pass
models = {m: {"name": m} for m in ["qwen3.8:27b", "qwen3-coder:30b", "deepseek-r1:32b"]}
cfg["$schema"] = "https://opencode.ai/config.json"
cfg.setdefault("provider", {})["praxis-local"] = {
    "npm": "@ai-sdk/openai-compatible",
    "options": {"baseURL": "http://localhost:38080/v1"}, "models": models}
cfg["model"] = cfg["small_model"] = "praxis-local/qwen3.8:27b"
target.write_text(json.dumps(cfg, indent=2) + "\n")
for f in (plain, jsonc):
    if f != target and f.exists(): f.unlink()
print("wrote", target, "| model:", cfg["model"])
EOF
```

```bash
OPENAI_API_KEY=dummy opencode run --model praxis-local/qwen3.8:27b "reply with exactly: PRAXIS_OK"
```

> **Use a big model.** One agent turn costs ~9,000 tokens, and only
> `qwen3.8:27b`, `qwen3-coder:30b` and `deepseek-r1:32b` carry an `agent-daily`
> budget. `qwen3.5:0.8b` and `qwen2.5:3b` fall through to the catch-all `free`
> tier at 5,000 tokens/min and are **429'd on the first turn** — they exist for
> the burst demo, not for agents.

Confirm the traffic actually reached the gateway — an agent that "works" may
just be talking to the vendor:

```bash
kubectl exec deploy/praxis-proxy -n default -- \
  wget -qO- http://127.0.0.1:9901/metrics | grep 'requests_total{decision='
```

`admitted` climbing means praxis served it; `denied` climbing means praxis saw
it and rejected it on budget; neither moving means the agent never arrived.

## 7. Dashboards

<http://localhost:33000> — admin/admin. Every panel carries an `i` tooltip
explaining what it plots and how to read it.

| Dashboard | Shows |
| --- | --- |
| Praxis AI Gateway Overview | both tiers, traffic and latency together |
| Praxis Token Budget & Rate Limiting | admitted/denied, estimation accuracy, reservations |
| Praxis Filter Latency (from spans) | which filter costs what |
| Praxis OTel Traces | per-request waterfall, and whether spans are being lost |

**Two telemetry planes.** Prometheus counters cover **100%** of requests and are
unaffected by sampling; span metrics come only from **sampled** traces. Random
sampling is unbiased for percentiles but not for counts, so Prometheus owns
"how much" and span metrics own "how long and where". The Weighted Filter Cost
panel takes its quantile from spans and its rate from
`praxis_http_requests_total` for exactly that reason. This demo sets
`sampling_rate: 1.0`, so the Sampling Ratio Cross-Check should read ~1.0; a drop
means spans are being lost, not sampled.

**Where token numbers come from.** `praxis_ai_token_rate_limit_tokens_total`
with `kind=estimated|actual|refunded|overage`. `estimated` is reserved up front,
`actual` is what the model reported, and the difference settles as `refunded` or
`overage`. **If `actual` exactly equals `estimated`, reconciliation is not
running** — see the ordering note in `configs/token-budget.yaml`.

## 8. Teardown

```bash
praxis-forge down --config forge.yaml
```

## Host ports

| Port | Service | NodePort |
| --- | --- | --- |
| 38080 | Praxis proxy | 30080 |
| 38901 | Praxis admin | 30901 |
| 33000 | Grafana | 30300 |
| 39090 | Prometheus | 30909 |

## Troubleshooting

| Symptom | Cause |
| --- | --- |
| `404 model ... does not exist` | Codex bypassed praxis — missing `-c model_provider=...` |
| `wire_api = "chat" is no longer supported` | set `wire_api = "responses"` |
| `reserved built-in provider IDs` | you renamed the provider — `ollama`, `lmstudio` and `openai` cannot be redefined |
| agent 429s immediately | small model on the `free` tier — use a model from step 5's table |
| neither `admitted` nor `denied` moves | the agent never reached the gateway |
| opencode does not list your provider | two config files — the block in step 6 consolidates them |
| `000` from curl on 38080 | cluster down, or `praxis-forge apply` not run |
| `invalid duration '7d'` | use `"168h"`; praxis kept the previous config |
| dashboards flat | no traffic through praxis, or the Prometheus target is down |
| 27B feels slow | ~34s per 400-token turn is normal on an M4 Max |

## Security notes — this is a demo configuration

- **`allow_public_admin: true`** binds the admin listener to the pod IP so
  Prometheus can scrape `/metrics`. That listener also serves `/api/log-level`
  and `/api/kv`, so every pod in the cluster can change the gateway's log level.
  Restrict `:9901` with a NetworkPolicy anywhere shared.
- **`allow_private_endpoints: true`** disables SSRF hardening so the gateway can
  reach `host.docker.internal`. Drop it the moment the upstream is public.
- **`X-Tier` and `X-Model` are trusted as-is** — see the auth note in step 5.

## Notes

- `token_rate_limit` is experimental, behind the `token-rate-limit-filter` cargo
  feature; its parent proposal is not accepted (ai#796) and the config surface
  may change.
- `reserved_tokens` is a flat per-request estimate. Deriving it from request
  metadata is deferred upstream (ai#121).
- The `memory` backend is per-process, and a sliding window retains one entry
  per request for the length of the window. `backend.kind: valkey` shares one
  budget across replicas and moves that state out of process.
- Span metrics take a scrape interval or two to appear; empty panels right after
  deploy are expected.
