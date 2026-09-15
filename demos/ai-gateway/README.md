# AI Gateway demo

A Praxis AI gateway in front of a real model, enforcing per-tier token budgets,
with dashboards built from its own traces. Point a coding agent at it and watch
the requests land.

## Quickstart

**[docs/quickstart.md](docs/quickstart.md)** — two paths, picked by who can reach
the port. Three ports serve every provider:

| Port | Serves | Pick it by |
| --- | --- | --- |
| `:8080` | **Ollama and OpenAI** | the model you name |
| `:8081` | Anthropic | pointing at the port |
| `:8082` | OpenRouter | pointing at the port |

On your laptop, provider credentials are optional. The gateway variables must exist,
but empty values are enough for Ollama and keep real keys out of coding agents:

```bash
podman run -d --name praxis \
  -p 127.0.0.1:8080:8080 -p 127.0.0.1:8081:8081 \
  -p 127.0.0.1:8082:8082 -p 127.0.0.1:9901:9901 \
  -e OPENAI_API_KEY="${OPENAI_API_KEY:-}" \
  -e ANTHROPIC_API_KEY="${ANTHROPIC_API_KEY:-}" \
  -e OPENROUTER_API_KEY="${OPENROUTER_API_KEY:-}" \
  ghcr.io/praxis-proxy/experimental:main \
  -c /usr/share/praxis/demo/configs/laptop.yaml
```

`:8080` carries two providers at once because everything that differs between them
has a per-cluster form — the upstream `Host` (`http.authority`), the SNI, the key,
and which cluster a model name selects. Anthropic and OpenRouter need their own
ports for reasons `configs/laptop.yaml` spells out at the top: a single
`token_count provider:` per chain, no `openai_to_anthropic` translation filter,
and an unconditional `path_rewrite`.

Both configs make the gateway hold provider keys. On a laptop any placeholder
is accepted because loopback is the trust boundary. On a server, `policy` first
validates that bearer value as a caller JWT. The quickstart explains why
`basic_auth` is not a fit and why `ip_acl` is only an optional perimeter.

## Providers

There are **two configs**, one per deployment target, and each serves every
provider:

| | | |
| --- | --- | --- |
| `configs/laptop.yaml` | your own machine | gateway variables may be empty; callers use placeholders |
| `configs/server.yaml` | a shared host | the gateway holds the keys; callers present a JWT |

| Provider | Port | Needs | Wire format |
| --- | --- | --- | --- |
| Ollama | `:8080` | Ollama running locally. No key, no cost | OpenAI |
| OpenAI | `:8080` | `OPENAI_API_KEY` | OpenAI |
| Anthropic | `:8081` | `ANTHROPIC_API_KEY` | Anthropic `/v1/messages` |
| OpenRouter | `:8082` | `OPENROUTER_API_KEY` | OpenAI |

Adding a provider means one route and one cluster in each config — `laptop.yaml`
explains at the top which fields are per-cluster and which force a new chain.

Anthropic is the odd one: it is not OpenAI-shaped and praxis does not translate,
so the client has to speak Anthropic too. Claude Code does.

## Developer manuals

For changing configs, adding a provider, running the verification suite, or
deploying to Kubernetes:

| | Good for | Needs | |
| --- | --- | --- | --- |
| **podman**, or docker | editing and re-running quickly | ~1 GB | **[docs/podman.md](docs/podman.md)** |
| **KIND** via forge | closest to a real deployment | ~4 GB | **[docs/kind.md](docs/kind.md)** |

Both come down to two commands, and both can run at once because their ports
differ:

```bash
./demo-podman up && ./demo-podman verify
./demo-kind    up && ./demo-kind    verify
```

[docs/budgets.md](docs/budgets.md) explains the tiers, the reservations and
refunds, how to size a cap that means something, and why the limit only trips
under concurrent load.

---

<details>
<summary>What is running, and why</summary>

| Component | Why it is here |
| --- | --- |
| **praxis** | the gateway under test: budgets requests per tier, routes upstream, emits the traces |
| **the model** | Ollama or a hosted API, so token counts and latencies are real rather than mocked |
| **OTel collector** | what praxis exports spans to. Its own metrics drive the export-health panels, so removing it would blank them |
| **Tempo** | stores traces, and its metrics generator derives the span metrics the latency dashboards query |
| **Prometheus** | scrapes praxis and the collector, and receives Tempo's generated span metrics |
| **Perses** | one UI for both metrics and traces, and the same dashboards work on OpenShift through the Cluster Observability Operator |
| **Grafana** (KIND only) | the original dashboards, kept until the Perses ports are confirmed equivalent |

</details>

<details>
<summary>Resources it needs</summary>

Measured on this stack, idle after a demo run:

| | podman | KIND |
| --- | --- | --- |
| Memory | **~250 MB** across 5 containers | **~2.3 GB** for the node, 19 pods |
| Largest | Tempo 114 MB, Prometheus 46 MB | the kube-prometheus-stack |
| Images | ~850 MB pulled | the above plus the KIND node image |
| Startup | seconds | ~3 minutes, mostly Helm |

Give the container VM **2 GB** for podman and **6 GB** for KIND. Ollama runs on
the host; the demo model is 0.5 GB, and the 27B model the coding-agent examples
use wants about 20 GB.

</details>

<details>
<summary>Where things live</summary>

| Path | What it is |
| --- | --- |
| `compose/compose.quick.yaml` | the checkout-free stack, baked into the image as `/usr/share/praxis/demo/compose.yaml` |
| `demo-podman`, `demo-docker`, `demo-kind` | the developer entry points; `demo-docker` is a wrapper |
| `scripts/demo-lib.sh` | what they share: providers, credentials, image, links |
| `configs/laptop.yaml`, `configs/server.yaml` | ready to use per deployment target, four providers over three ports |
| `compose/` | the podman target: services, Tempo, Prometheus, Perses |
| `forge.yaml`, `manifests/` | the KIND target: cluster, stacks, Kubernetes objects |
| `observability/perses/` | Perses config, project, dashboards, per-target datasources |
| `scripts/verify.sh` | the checks, for either target |

</details>

## This is a demo configuration

Laptop mode has no caller authentication, and its container-internal admin endpoint
binds all interfaces so a loopback-only host publish can reach it. SSRF guards are
relaxed so the gateway can reach a model server on the host. Server mode
validates a JWT and binds admin to loopback. Do not expose the laptop
configuration to a network.
