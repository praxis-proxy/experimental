# Cloud-burst example manifests

Reference manifests that make the concepts in the [demo README](../README.md)
concrete. The demo README explains *why* each policy exists; these files show the
*exact configuration* that produces the behavior, copied from the working demo.

> These are illustrative references, not a turnkey installer. Some values are
> placeholders resolved at deploy time (see [Placeholders](#placeholders)).

## What each file demonstrates

| File | Kind | Demonstrates (user story) |
| --- | --- | --- |
| `01-local-sim-pools.yaml` | ConfigMap + Deployment + Service | Deterministic local providers whose **queue metric is controlled**, so admission/burst transitions are reproducible without real load. |
| `02-gridnetwork.yaml` | GridNetwork | **Policy as independent decisions**: `scoringPolicy` (admission signal), `admissionPolicy` (stabilized enter/exit band), `selectionPolicy` + `placementPolicy` (weighted distribution), `routingPolicy` (preferred vs fallback tier). Bursting emerges from composing these — there is no "burst" switch. |
| `03-grid-sites-and-providers.yaml` | GridSite + InferenceProvider | **Truthful topology** + **burst destination**: a local pool (`backendKind: local`) and an OpenAI overflow (`backendKind: api_provider`, `auth.manual: true`). Distinct backend class keeps the overflow in a separate fallback group. |
| `04-bedrock-overflow.yaml` | GridSite + InferenceProvider | **Burst amount is independent from burst destination**: a second, genuinely distinct overflow provider (Bedrock) as a generic HTTP hop. |
| `05-consumer-praxis-soft-quota.yaml` | Praxis config | **Soft token governance independent from routing**: `token_rate_limit` with `enforcement: soft` (over-budget requests are served, not 429'd) plus `intelligent_route` reading the local snapshot. |
| `06-provider-praxis.yaml` | Praxis config | **Security at the provider boundary**: `peer_identity_trust` (fail-closed), `provider_route`, and `credential_inject` replace the caller credential before the upstream hop. |

## The burst behavior, end to end

1. `scoringPolicy: queueDepth` makes admission track each local provider's live
   queue depth (from `01`'s controllable metric).
2. Under the `admissionPolicy` stabilized band, a local provider whose queue
   crosses `enterThreshold` (0.85) becomes `existing_only` and stops accepting
   new traffic; it recovers only after dropping below `exitThreshold` (0.70).
3. `selectionPolicy: weightedRandom` + `placementPolicy: static` split traffic
   within the active group by each provider's `capacityWeight`.
4. Because the overflow providers use a different **backend class**
   (`api_provider`), Grid keeps them in a separate fallback group. They serve new
   traffic only once **every** local provider is `existing_only`. Metric-less
   elastic providers stay admissible, so overflow is always ready.
5. `token_rate_limit` runs independently on the consumer; a user over their soft
   allocation is still served and simply flagged `over_allocation`.

## Apply order

```sh
kubectl create namespace grid-system   # if not present
kubectl apply -f 01-local-sim-pools.yaml
kubectl apply -f 02-gridnetwork.yaml
kubectl apply -f 03-grid-sites-and-providers.yaml
kubectl apply -f 04-bedrock-overflow.yaml     # optional: second overflow cloud
# 05 / 06 are Praxis gateway configs, mounted into the consumer / provider gateways.
```

The Grid operator reconciles `02`–`04` into a routing overlay that the consumer
gateway (`05`) consumes. The provider gateway (`06`) terminates the provider hop.

## Prerequisites

- A Kubernetes cluster (the demo uses kind) with the Grid operator installed.
- Images: `ghcr.io/llm-d/llm-d-inference-sim:v0.10.2` and the Grid/Praxis gateway
  images used by the demo.
- For the live OpenAI overflow, a secret `openai-api-key` in `grid-system`
  (key `token`); referenced via `auth.manual: true`. Never commit the key.

## Placeholders

Copied verbatim from the working demo, some files contain values resolved at
deploy time:

- `03`/`04` use `__SITE__`, `__GATEWAY_IP__`, `__WEST_FINGERPRINT__`, `__WEIGHT__`,
  `__OPENAI_WEIGHT__`, `__BEDROCK_GATEWAY_IP__` — rendered per-cluster by the
  demo's `scripts/apply-static-grid-resources.sh` (gateway IPs and the trust
  fingerprint are discovered from the running cluster; the templates carry no
  credentials).
- `05`/`06` are Praxis config templates using `{{ cluster.name }}` /
  `{{ captures.*.provider-gateway-ip }}` and `SITE_PLACEHOLDER` /
  `CANDIDATE_ID_PLACEHOLDER`, rendered by the demo's deployment tooling.

Replace the placeholders with real values (or use the demo's render scripts)
before applying.

## Regional resilience topology used by the v2.1 recording

Files `08` through `14` are a separate, larger topology. They model two
consumer gateways, east and west provider sites, four local inference
simulators, and an Azure OpenAI/OpenAI external provider group.

| File | Purpose |
| --- | --- |
| `08-regional-local-pools.yaml` | Four independently controllable local simulator Deployments and Services. |
| `09-regional-inference-providers.yaml` | Local providers with health and queue-depth admission signals. |
| `10-openai-overflow-providers.yaml` | East and west OpenAI overflow candidates. |
| `11-azure-overflow-providers.yaml` | Azure OpenAI provider templates with Secret references and endpoint placeholders. |
| `12-azure-provider-gateways.yaml` | Dedicated Azure gateway Services and Deployments with mTLS and Entra token injection. |
| `13-consumer-east-praxis.yaml` | East consumer pipeline with shared soft quota and all provider-hop clusters. |
| `14-consumer-west-praxis.yaml` | West consumer equivalent of the same identity, quota, and routing contract. |
| `10-observability-jaeger.yaml` | Optional Jaeger deployment for request/route inspection. |

The numeric overlap at `10` is intentional: observability is optional and does
not participate in provider ordering.

This set extends an already installed Grid and Praxis environment. It does not
create trust material, external-provider Secrets, Grid operators, overlay sync,
or the base consumer/provider gateway deployments. Apply the local simulator
and provider resources first, configure the gateway routes, then add Azure only
after its direct and provider-boundary validation passes.

Do not apply the `01`-`06` and `08`-`14` GridNetwork/provider sets together
without reconciling their network names, sites, and provider policies. The
former is the compact adaptive-burst reference; the latter reproduces the
regional resilience recording.
