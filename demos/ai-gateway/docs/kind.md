# Run the demo on KIND

One cluster, six forge stacks, closest to a real deployment. Give docker 6 GB;
it takes about three minutes, mostly Helm.

## 1. Install the tools

`kind`, `kubectl`, `helm`, a running docker, and:

```bash
cargo install --locked --git https://github.com/praxis-proxy/forge
helm repo add prometheus-community https://prometheus-community.github.io/helm-charts
helm repo add grafana https://grafana.github.io/helm-charts && helm repo update
```

## 2. Start it

```bash
cd demos/ai-gateway
./demo-kind up
```

The first run builds the image and pulls a 0.5 GB Ollama model. Store an
optional hosted-provider key first with `./demo-kind remember <provider>`; it
stays in your OS keychain and is copied into a Kubernetes Secret, never into
this repository.

<details>
<summary>The same thing by hand</summary>

```bash
cd demos/ai-gateway
docker build --build-arg FEATURES=otel -t praxis-experimental:ai-gw \
  -f ../../Containerfile ../..

praxis-forge up --config forge.yaml
kind load docker-image praxis-experimental:ai-gw --name ai-gw-local
kubectl config use-context kind-ai-gw-local

# Every key must exist; empty values keep providers optional.
kubectl create secret generic praxis-provider-keys \
  --from-literal=OPENAI_API_KEY="${OPENAI_API_KEY:-}" \
  --from-literal=ANTHROPIC_API_KEY="${ANTHROPIC_API_KEY:-}" \
  --from-literal=OPENROUTER_API_KEY="${OPENROUTER_API_KEY:-}" \
  --dry-run=client -o yaml | kubectl apply -f -

for s in prometheus tempo otel-collector praxis-deploy dashboards perses; do
  praxis-forge apply --config forge.yaml local "$s"
done
```

The Secret may contain only empty values; Ollama itself needs no provider credential.

</details>

## 3. Check it

```bash
./demo-kind verify
```

Six checks, ending in `all checks passed`: the gateway serves a chat, refuses
the free tier once its budget is spent, leaves premium alone, and is scraped by
Prometheus. Tempo publishes no host port here, so the trace check is skipped and
the traces dashboard shows them instead. The budget probe floods the gateway, so
it is skipped for hosted providers, which bill per admitted request.

## 4. Open the dashboards

| | |
| --- | --- |
| Perses | <http://localhost:33001> |
| — overview | <http://localhost:33001/projects/praxis/dashboards/ai-gateway-overview> |
| — token budget | <http://localhost:33001/projects/praxis/dashboards/token-budget> |
| — filter latency | <http://localhost:33001/projects/praxis/dashboards/filter-latency> |
| — traces | <http://localhost:33001/projects/praxis/dashboards/traces> |
| Grafana | <http://localhost:33000> (admin/admin) |
| Prometheus | <http://localhost:39090> |
| gateway `/metrics` | <http://localhost:38901/metrics> |

Click a row in **Recent Traces** for its span waterfall. For more traffic first:
`GATEWAY=http://localhost:38080 ./scripts/rate-limit-demo.sh`.

## 5. Point a coding agent at it

Claude Code needs nothing but environment:

```bash
ANTHROPIC_BASE_URL=http://localhost:38081 \
ANTHROPIC_AUTH_TOKEN=dummy \
ANTHROPIC_MODEL=claude-sonnet-4-6 \
  claude
```

The placeholder is replaced with the gateway's `ANTHROPIC_API_KEY`. Serving a
local OpenAI-compatible model to Claude Code requires an `anthropic_to_openai`
chain, which this demo does not configure.

Codex and opencode need a provider block first, and one session can produce
several model names, each wanting its own budget rule.
**[coding-agents.md](coding-agents.md)** covers all three.

## 6. Tear down

```bash
./demo-kind down
```

---

<details>
<summary>Ports are fixed at cluster creation</summary>

They come from `extraPortMappings`, which KIND applies when the node container
is created. **Adding a port to `forge.yaml` does nothing to a running cluster**
— it needs a `down` and `up`.

| OpenAI-compatible | Anthropic | OpenRouter | admin | Grafana | Perses | Prometheus |
| --- | --- | --- | --- | --- | --- | --- |
| 38080 | 38081 | 38082 | 38901 | 33000 | 33001 | 39090 |

Perses is deliberately **not** on 38081: that port is commonly taken by a second
praxis gateway pointed at OpenAI, and an agent configured against it would
silently get a dashboard instead of a gateway.

</details>

<details>
<summary>Where the API key is stored</summary>

In order of preference, and nothing is ever written inside this repository:

1. **Already exported** — used as-is, never stored.
2. **OS keychain** — macOS Keychain, or libsecret on Linux.
3. **`$XDG_CONFIG_HOME/praxis-ai-gateway/env`**, mode 600, when neither exists.

Whatever is in scope becomes the `praxis-provider-keys` Secret, which the
Deployment consumes with `envFrom` and `optional: true`.

For a real cluster, source that Secret from External Secrets, Vault or a sealed
secret instead. `env_var` is resolved once when the filter is built, so rotating
a key means restarting the pod.

</details>

<details>
<summary>Why this target uses docker</summary>

`forge.yaml` sets `runtime.provider: docker` and `kind load docker-image` talks
to that runtime. forge also accepts `podman` and `auto`, but KIND's podman
support is experimental and this demo has only been exercised with docker. If
you try it, also set `KIND_EXPERIMENTAL_PROVIDER=podman`.

The explicit `kubectl config use-context` is not redundant: forge scopes only
`manifest`, `helm` and `wait` steps to the cluster, while `exec` steps inherit
the ambient kubectl context.

</details>

<details>
<summary>How this differs from the compose target</summary>

- Prometheus discovers the gateway through a Prometheus Operator
  `ServiceMonitor` rather than a static scrape config. Both produce the job
  label `praxis-proxy`, so dashboard queries are identical.
- Grafana is present here and reads a labelled ConfigMap.
- Perses reads four ConfigMaps built from `observability/perses/`, of which only
  `datasources/kind/` differs. The dashboards are byte-for-byte the same files.
- The OTLP endpoint is an env var on the Deployment, so the shared config needs
  no `telemetry.otlp_endpoint` key.

</details>
