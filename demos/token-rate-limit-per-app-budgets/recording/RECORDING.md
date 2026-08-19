# Recording spike: token-rate-limit-per-app-budgets via traffic-theater

> Local spike only. Not committed/pushed anywhere -- this directory exists
> only in this working tree so the video and evidence are easy to review.
> Nothing here has been posted to GitHub.

## What this is

A narrated, evidence-backed recording of the per-app token budget scenario
(see `../README.md`), produced with
[nerdalert/traffic-theater](https://github.com/nerdalert/traffic-theater)
(`feat/reusable-recording-toolkit` branch), driving a real browser against a
live two-gateway + Valkey docker-compose stack -- not a mockup or a scripted
screen capture.

**Watch:** `output/final.mp4` (1920x1080, h264/aac, ~48s)

## What it proves (evidence-manifest.json)

Five live HTTP assertions (`requireGate` in `playwright/record.mjs`), each
checked against the real running gateways during the recording, not after
the fact:

1. app-a admitted on Gateway A (`200`).
2. app-a denied on Gateway B, the *other* process (`429`,
   `X-RateLimit-Remaining-Tokens: 0`) -- proves the shared Valkey ledger,
   not per-replica state.
3. app-b admitted on Gateway B (`200`) immediately after -- proves per-app
   isolation from app-a's exhaustion.
4. app-c admitted on Gateway A (`200`) on its own untouched budget -- a
   third independent app, same Valkey namespace.
5. app-a, previously denied, admitted again on Gateway A (`200`) once the
   10s sliding window ages its earlier reservation out -- proves the
   window *slides* (budget recovers continuously as usage ages out) rather
   than requiring a manual reset or a fixed reset boundary.

This is the exact same causal sequence as the curl walkthrough in
`../README.md`, driven through a browser dashboard (`../dashboard/`) instead
of curl, so it can be recorded.

## Why a dashboard exists

`traffic-theater`'s `scripts/record.sh` hard-requires a Playwright script
driving a real browser against a `baseUrl` web app -- there is no
terminal/CLI recording path in the toolkit as shipped. This demo had no
browser surface, so `dashboard/` (a static page + nginx same-origin reverse
proxy, see `../dashboard/nginx.conf`) was added purely for recording. It
wraps the identical HTTP contract as the curl walkthrough; it is not part of
the Praxis AI filter chain and does not change anything about the filter
under test.

`exact_trace`, the evidence gate every one of `traffic-theater`'s three
shipped examples requires, was deliberately **not** used here: this scenario
is a single-hop admission decision (no cross-service routing), so there is
no distributed trace to correlate. `evidence.required` in `production.yaml`
was scoped instead to `cross_instance_shared_budget`, `per_app_isolation`,
`reservation_admission`, and `window_recovery` -- matched 1:1 to the
`requireGate` assertions above.

## What "window recovery" means, and what it doesn't

The window is `10s` in this recording specifically so the recovery is
watchable; a production deployment would use a much larger value (the
demo's own `../config.yaml` default is commented accordingly). What's
demonstrated is the *sliding* part of "sliding window": there is no reset
event, no restart, no cron job -- app-a's budget recovers purely because
its earlier reservation ages past the trailing 10s window as real time
elapses. This is different from a *fixed/tumbling* window (e.g. resets at
the top of the hour) or a *token bucket* (continuous drip up to a
capacity); see `../README.md`'s "Open design questions" section for why
the choice between these algorithms is still open upstream.

## Known deviations from the toolkit's default pipeline

`scripts/generate-narration.sh` and `scripts/generate-captions.sh` both
hard-require `OPENAI_API_KEY` (real OpenAI TTS + Whisper transcription).
**No such key was available in this environment**, so:

- `narration/narration.wav` was generated with a local, offline
  substitute -- macOS `say -v Samantha`, converted to WAV via `ffmpeg` --
  instead of the toolkit's OpenAI TTS call.
- `narration/narration.srt` caption timing is an **approximation**
  (proportional to sentence character count over the measured audio
  duration), not a real Whisper word-level transcription. See
  `/tmp/trl-demo-spike/gen-approx-captions.mjs` for the substitute script
  used (not part of the upstream toolkit).

Everything else -- the production schema, the live browser recording, the
`requireGate` evidence assertions, the ffmpeg assembly, and the final media
validation (`src/validation/validate-media.js`) -- used the toolkit exactly
as shipped, unmodified, against a genuinely live stack.

**To get the toolkit's canonical pipeline output** (real OpenAI TTS +
Whisper captions), re-run with `OPENAI_API_KEY` set:

```bash
export OPENAI_API_KEY=...
./scripts/generate-narration.sh examples/token-rate-limit-per-app-budgets
./scripts/generate-captions.sh examples/token-rate-limit-per-app-budgets
./scripts/assemble.sh examples/token-rate-limit-per-app-budgets
```

## NDA scrubbing

App names throughout (`app-a`, `app-b`, `app-c`) are placeholders. The real
customer scenario's application names are excluded here and enforced via
`production.yaml`'s `redaction.deny` list.

## Reproduce from scratch

Self-contained -- assumes nothing already checked out locally. Requires
Docker or Podman with Compose, Git, Node.js/npm, and ffmpeg/ffprobe on
`PATH` (`traffic-theater`'s own `scripts/doctor.sh` checks these).

```bash
# 1. Clone this repo and the filter's source branch as siblings, then bring
#    up the demo stack (see ../README.md for the full walkthrough)
git clone https://github.com/praxis-proxy/experimental.git
git clone --branch jordigilh/token-rate-limit-per-app-budgets \
  https://github.com/jordigilh/praxis-ai.git praxis-ai-trl-demo

cd experimental/demos/token-rate-limit-per-app-budgets
export PRAXIS_AI_SRC=../../../praxis-ai-trl-demo
podman compose up --build -d   # or: docker compose up --build -d
podman exec token-rate-limit-per-app-budgets-valkey-1 valkey-cli FLUSHALL

# 2. Clone traffic-theater and install deps (one-time)
git clone --depth 1 -b feat/reusable-recording-toolkit \
  https://github.com/nerdalert/traffic-theater.git
cd traffic-theater
npm install
npx playwright install chromium

# 3. Copy this example's recording assets into the clone's examples/ dir
mkdir -p examples/token-rate-limit-per-app-budgets
cp -r ../recording/{production.yaml,slides,narration,playwright} \
  examples/token-rate-limit-per-app-budgets/

# 4. Validate, record, assemble, validate media
node src/validation/validate-production.js \
  examples/token-rate-limit-per-app-budgets/production.yaml
node examples/token-rate-limit-per-app-budgets/playwright/record.mjs
./scripts/assemble.sh examples/token-rate-limit-per-app-budgets
node src/validation/validate-media.js \
  examples/token-rate-limit-per-app-budgets/output/final.mp4

# 5. Tear down the demo stack when done
cd ../../experimental/demos/token-rate-limit-per-app-budgets
podman compose down -v
```

## Confidence

High (>=90%) on everything demonstrated: every HTTP status/header claim in
the video was asserted live against the real stack, not staged, and the
final media passed the toolkit's own validation gate. The two flagged
deviations (local TTS, approximate captions) are cosmetic/narration-layer,
not evidentiary -- they don't affect what was actually proven about the
filter's behavior.
