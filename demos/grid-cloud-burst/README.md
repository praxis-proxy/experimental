# Grid Cloud-Burst — narrated recording

[Recorded demo](https://github.com/user-attachments/assets/6cb33a69-288d-4195-8f80-c6bc537f2d41)

This demo presents Praxis Grid intelligent overflow routing for Kubernetes:

- local-first provider routing with queue-aware admission;
- independent admission, grouping, placement, burst, and overflow policies;
- soft token governance with a distributed quota ledger;
- quota continuity across gateways, sites, regions, and Kubernetes clusters;
- OpenTelemetry-backed route and provider attribution; and
- recovery from external capacity back to on-prem inference providers.

## Presentation flow

1. **Architecture** — control-plane policy computation and the fast request path.
2. **Reactive burst** — local headroom and controlled external overflow.
3. **Independent policies** — burst amount and overflow destination remain separate.
4. **Soft token limits** — over-allocation is reported without forced denial.
5. **Live recording** — normal traffic, pressure, burst, and recovery.
6. **Outro** — Grid remains off the request hot path.

## Architecture shown

```text
Kubernetes provider health and metrics
                |
                v
          Grid control plane
  discover -> observe -> admit -> group
       -> place -> burst -> publish
                |
                v
       versioned routing snapshot
                |
                v
        request gateway
 authenticate -> token policy -> snapshot
       -> affinity -> local selection
                |
                v
        selected provider gateway
                |
                v
          inference backend
```

Grid computes policy asynchronously. The request gateway uses the accepted snapshot
locally; there is no request-time call to Grid, Kubernetes, metrics systems, or
inference schedulers.

The recording is backed by a Kubernetes deployment with consumer gateways, local
inference-simulation providers, shared token state, external capacity, and observed
OpenTelemetry routing evidence.

## Generation assets

The slides, narration, recording, TTS helper, assembly scripts, and Playwright review
are maintained in the Traffic Theater implementation:

[Traffic Theater demo implementation](https://github.com/nerdalert/traffic-theater/tree/feat/reusable-recording-toolkit/examples/grid-cloud-burst)

That directory contains the reproducible production manifest and the generated-video
workflow. This Experimental directory intentionally contains only this README and the
recorded-demo link.

## Build inputs

The composed deployment used for the recording is built from these branches:

- **AI** — [`nerdalert/ai`](https://github.com/nerdalert/ai/tree/burst-routing-v1),
  branch `burst-routing-v1`.
- **Grid** — [`nerdalert/grid`](https://github.com/nerdalert/grid/tree/burst-routing-v1),
  branch `burst-routing-v1`.
- **Praxis identity** —
  [`nerdalert/praxis`](https://github.com/nerdalert/praxis/tree/poc/authenticated-principal-metadata),
  branch `poc/authenticated-principal-metadata`.
- **Praxis authority** —
  [`nerdalert/praxis`](https://github.com/nerdalert/praxis/tree/ai-grid-cluster-authority-override),
  branch `ai-grid-cluster-authority-override`.
- **Tracing UI** —
  [`nerdalert/praxis-tracing`](https://github.com/nerdalert/praxis-tracing/tree/burst-routing-v1),
  branch `burst-routing-v1`.

The two Praxis feature branches are composed into the gateway build as required by
the selected AI branch. Do not replace these pins with unrelated main-branch builds.
