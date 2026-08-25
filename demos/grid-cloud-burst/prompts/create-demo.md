# Prompt: create a narrated cloud-burst demo

Create a single professional narrated video from a live Kubernetes routing recording.

Requirements:

- Use four parts: title card, “what the demo proves” card, live recording, and outro card.
- Add one second of silence at the beginning and one second at the end.
- Narrate the live recording itself; do not treat it as silent B-roll.
- Review the recording closely and write narration that follows the visible UI state,
  including local routing, application quotas, queue pressure, external-provider burst,
  and recovery.
- Keep the architecture accurate: the control plane observes and publishes a versioned
  snapshot; the request gateway authenticates, applies token policy, preserves valid
  affinity, and selects locally from the accepted snapshot.
- Explain that quota admission happens before provider routing and that the shared ledger
  remains continuous across gateways and provider changes.
- Say “Kubernetes,” “local inference provider,” and “external provider.” Do not name cloud,
  model, or infrastructure vendors unless the recording explicitly requires it.
- Do not invent token-type usage, cost, routing, or provider evidence. Say that the value
  is unavailable when the UI does not expose it.
- Use card-based slides with a clean light theme, dark readable text, green local state,
  amber governance state, and red pressure/overflow state.
- Generate speech with the `tts-1` model and `alloy` voice. Keep the live-demo narration
  within the source recording duration; pad or trim only at the segment boundary.
- Use Playwright to render and inspect each card. Validate the final video duration,
  audio presence, silent lead-in/tail, and narration alignment against the visible scene
  changes before publishing.

Deliver:

1. slide HTML files;
2. narration source text;
3. generated voice audio and captions;
4. a reproducible assembly script;
5. a README documenting the evidence claims and validation results.
