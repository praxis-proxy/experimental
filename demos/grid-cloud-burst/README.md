# Grid Cloud-Burst — narrated recording

[Recorded demo](https://github.com/user-attachments/assets/6cb33a69-288d-4195-8f80-c6bc537f2d41)

This example assembles one presentation from professional intro cards, the recorded
live demo, and an outro card. It demonstrates local-first routing, token governance,
queue-pressure admission, external burst routing, and recovery.

The recording uses a Kubernetes deployment with multiple consumer gateways, local
inference-simulation providers, a shared token ledger, and an external-provider
fallback path. The narration avoids vendor-specific names and describes only the
routing and quota behavior visible in the recording.

## Presentation flow

1. **Architecture card** — Praxis Grid Intelligent Overflow Routing and the separation
   between the control plane and the fast request path.
2. **Reactive burst card** — rebalance local capacity before using external capacity.
3. **Independent policies card** — separate admission, grouping, placement, burst
   amount, and overflow destination.
4. **Soft token limits card** — distributed quota governance independent from routing.
5. **Live recording** — the supplied `grid-burst-sim.mp4`, with narration aligned
   to the visible UI sequence.
6. **Outro card** — the completed control-plane/data-plane story and the no-hot-path
   control-plane property.

The presentation has a one-second silent lead-in and one-second silent tail. The
generated narration is rendered with the `tts-1` speech model and `alloy` voice.

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

Grid computes policy asynchronously. The gateway uses an accepted immutable snapshot
on the request path. There is no request-time call to Grid, Kubernetes, metrics
systems, or inference schedulers.

The policy layers remain separate:

```text
hard constraints
  -> admission
  -> grouping and locality
  -> local placement weights
  -> burst amount
  -> external-provider distribution
  -> local request selection
```

Token governance is independent from routing. The shared ledger follows the principal
and logical service across consumer gateways and provider changes. Soft allocation can
report over-allocation while allowing a request to complete; hard enforcement remains
a separate policy choice.

## Build the presentation

From this example directory:

```bash
./scripts/render-slides.sh
./scripts/generate-audio.sh
./scripts/assemble-presentation.sh
```

The scripts require `ffmpeg`, a Chromium-compatible browser for rendering cards, and
`OPENAI_API_KEY` in the environment. The key is used only by the local audio-generation
script and is never written to the repository.

The final artifact is written to `output/grid-cloud-burst-narrated.mp4`.

For a reusable starting point, see [prompts/create-demo.md](prompts/create-demo.md).

Generated media is ignored by the repository's media rules. The source recording must
be supplied at `source/grid-burst-sim.mp4` before running the assembly scripts.

## Source recording

The source recording is kept at `source/grid-burst-sim.mp4`. It is intentionally
treated as the live-demo segment; the assembly script does not alter its content.

## Build inputs

The demo records behavior from a composed Grid and gateway deployment. The source
branches used to build that deployment are:

| Repository | Branch | Purpose |
| --- | --- | --- |
| [`nerdalert/ai`](https://github.com/nerdalert/ai/tree/burst-routing-v1) | `burst-routing-v1` | Gateway filters, weighted selection, token governance, and request-path behavior |
| [`nerdalert/grid`](https://github.com/nerdalert/grid/tree/burst-routing-v1) | `burst-routing-v1` | Provider discovery, admission, grouping, placement, and published routing snapshots |
| [`nerdalert/praxis`](https://github.com/nerdalert/praxis/tree/poc/authenticated-principal-metadata) | `poc/authenticated-principal-metadata` | Authenticated principal metadata used by token governance |
| [`nerdalert/praxis`](https://github.com/nerdalert/praxis/tree/ai-grid-cluster-authority-override) | `ai-grid-cluster-authority-override` | Gateway-to-gateway authority and external-provider routing support |
| [`nerdalert/praxis-tracing`](https://github.com/nerdalert/praxis-tracing/tree/burst-routing-v1) | `burst-routing-v1` | Tracing UI and narrated-demo capture surface |

The demo itself lives in `praxis-proxy/experimental` under this directory. Use the
repository branches above when rebuilding the gateway, operator, and tracing images;
compose the two Praxis feature branches into the gateway build as required by the
selected AI branch. Do not substitute local-only tags or unrelated main-branch builds.

## Validation

The assembly script checks the source duration, card dimensions, audio presence, final
duration, and the one-second silent lead-in/tail. Review the final video for narration
alignment before publishing the example.

The current local validation passed:

- Playwright rendered and reviewed all five cards and ten timestamps in the final
  video at 1920×1080.
- Source recording duration: 122.333 seconds.
- Live narration duration: 121.369 seconds, leaving a controlled cushion before the
  recovery segment ends.
- Final presentation duration: 462.553 seconds.
- The final file contains H.264 video and stereo AAC audio.
- The measured lead-in silence is approximately one second and the outro tail is
  approximately one second or longer.

The slide copy is preserved in [slides/deck-text.md](slides/deck-text.md), so the
spoken explanation and the visible bullet cards can be reviewed together.
