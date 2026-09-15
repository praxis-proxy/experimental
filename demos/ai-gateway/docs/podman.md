# The compose target, from a checkout

Five containers, no Kubernetes, about 250 MB. The [quickstart](quickstart.md)
brings this up without a checkout; this page is the version you use when you are
changing something, plus the reference for both.

Every command also exists as `./demo-docker`, which is the same script on docker.

## 1. Start it

```bash
cd demos/ai-gateway
./demo-podman up
```

The first run builds the image and pulls a 0.5 GB Ollama model. Store an
optional hosted-provider key first with `./demo-podman remember <provider>`; it
stays in your OS keychain, never in this repository.

<details>
<summary>The same thing by hand</summary>

```bash
cd demos/ai-gateway
podman build --build-arg FEATURES=otel -t praxis-experimental:ai-gw \
  -f ../../Containerfile ../..

ollama pull qwen3.5:0.8b
export OPENAI_API_KEY=sk-...                # only to call a hosted provider yourself

cd compose && podman compose up -d
```

There is nothing to pick: the stack runs `configs/laptop.yaml`, which serves every
provider. It supplies empty provider variables when no keys are configured, so
Ollama still starts. A real value is held by the gateway and substituted into hosted
calls. On native Linux docker also pass
`-f compose.yaml -f compose.linux.yaml`.

</details>

## 2. Check it

```bash
./demo-podman verify
```

Seven checks, ending in `all checks passed`: the gateway serves a chat, refuses
the free tier once its budget is spent, leaves premium alone, traces to Tempo
and is scraped by Prometheus. The budget probe floods the gateway, so it is
skipped for hosted providers, which bill per admitted request.

## 3. Open the dashboards

| | |
| --- | --- |
| Perses | <http://localhost:8081> |
| — overview | <http://localhost:8081/projects/praxis/dashboards/ai-gateway-overview> |
| — token budget | <http://localhost:8081/projects/praxis/dashboards/token-budget> |
| — filter latency | <http://localhost:8081/projects/praxis/dashboards/filter-latency> |
| — traces | <http://localhost:8081/projects/praxis/dashboards/traces> |
| Prometheus | <http://localhost:9090> |
| gateway `/metrics` | <http://localhost:9901/metrics> |

Click a row in **Recent Traces** for its span waterfall. For more traffic first:
`GATEWAY=http://localhost:8080 ./scripts/rate-limit-demo.sh`.

## 4. Point a coding agent at it

Claude Code needs nothing but environment:

```bash
ANTHROPIC_BASE_URL=http://localhost:8083 \
ANTHROPIC_AUTH_TOKEN=dummy \
ANTHROPIC_MODEL=claude-sonnet-4-6 \
  claude
```

Use `http://localhost:8083`, not `:8080`: Compose reserves `:8081` for Perses and
publishes the native Anthropic chain on `:8083`. The placeholder is replaced with
the gateway's `ANTHROPIC_API_KEY`. Serving a local OpenAI-compatible model to Claude
Code requires an `anthropic_to_openai` chain, which this demo does not configure.

Codex and opencode need a provider block first, and one session can produce
several model names, each wanting its own budget rule.
**[coding-agents.md](coding-agents.md)** covers all three.

## 5. Stop

```bash
./demo-podman down
```

---

<details>
<summary>How the no-checkout path works</summary>

The image carries about 130 KB of demo assets under `/usr/share/praxis/demo`: the
six configs, the Tempo, Prometheus and collector configs, and the Perses config,
project, dashboards and datasources.

`compose.quick.yaml` — shipped in the image as `/usr/share/praxis/demo/compose.yaml`
— has a one-shot `seed` service that copies them into named volumes the other
services mount, then exits. Nothing bind-mounts a path from your disk, which is
what removes the checkout, and it means the dashboards cannot drift from the
gateway they describe because they ship together.

The seed runs as root only because a fresh named volume is root-owned; every other
container runs unprivileged. If the stack refuses to start, `podman compose logs
seed` names the reason.

</details>

<details>
<summary>Ports, and running both targets at once</summary>

All overridable, because the KIND target holds host ports too:

| | default | override |
| --- | --- | --- |
| gateway | 8080 | `GATEWAY_PORT` |
| Anthropic | 8083 | `ANTHROPIC_PORT` |
| OpenRouter | 8084 | `OPENROUTER_PORT` |
| admin and `/metrics` | 9901 | `ADMIN_PORT` |
| Perses | 8081 | `PERSES_PORT` |
| Prometheus | 9090 | `PROMETHEUS_PORT` |
| Tempo | 3200 | `TEMPO_PORT` |

`./demo-podman` reads the same variables, so overrides move the listeners and their
printed links together.

</details>

<details>
<summary>Where the API key is stored</summary>

In order of preference, and nothing is ever written inside this repository:

1. **Already exported** — used as-is, never stored.
2. **OS keychain** — macOS Keychain, or libsecret on Linux.
3. **`$XDG_CONFIG_HOME/praxis-ai-gateway/env`**, mode 600, when neither exists.

`DEMO_CRED_BACKEND=file` forces the third. The key is passed to the gateway
container and nothing else, and the gateway strips whatever credential the
client sent before substituting it.

</details>

<details>
<summary>podman or docker, and reaching Ollama on the host</summary>

`./demo-docker` is a two-line wrapper that runs `./demo-podman` with the engine
set. The engine is never guessed from what happens to be installed: on a machine
with both, starting the stack on one and looking for it on the other is a
frustrating ten minutes. Both drive the same compose project, so pick one and
stay with it for a given stack.

On macOS `podman compose` delegates to the installed provider, so the full
compose spec applies — the Python `podman-compose` reimplementation has known
gaps and is not tested here. `podman build` produces an OCI image, which has no
`HEALTHCHECK` field; nothing here needs one, but `--format docker` restores
parity.

Containers resolve `host.docker.internal` natively on podman, Docker Desktop and
Rancher Desktop. Native Linux docker does not, which is what
`compose.linux.yaml` is for. **Do not** fold that entry into `compose.yaml`: on
Rancher Desktop it resolves to the VM instead of the host and the gateway
answers 502.

Ollama stays on its default `127.0.0.1` binding throughout.

</details>

<details>
<summary>Why the OTel collector is pinned to 0.108.0</summary>

To match the KIND target. From 0.123.0 it renames its self-metrics and replaces
`telemetry.metrics.address` with a `readers` array, either of which blanks the
export-health panels on one target only. Upgrade both together or not at all.

</details>
