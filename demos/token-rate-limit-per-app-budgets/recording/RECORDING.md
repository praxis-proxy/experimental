# Recording spike: token-rate-limit-per-app-budgets via traffic-theater

## What this is

A narrated, evidence-backed recording of the per-app token budget scenario
(see `../README.md`), produced with
[nerdalert/traffic-theater](https://github.com/nerdalert/traffic-theater)
(`feat/reusable-recording-toolkit` branch), driving a real browser against a
live two-gateway + Valkey docker-compose stack -- not a mockup or a scripted
screen capture. This recording covers the **mixed-algorithm** scenario:
`gold-tier` (`sliding_window`) and `silver-tier` (`token_bucket`), matched by
an `x-tier` header, per ai#789/praxis#551.

**Watch:** `output/mixed-algorithms-token-rate-limit.mp4` (1920x1080,
h264/aac, ~79s)

An earlier, single-algorithm (`sliding_window`-only) recording of this same
demo existed before the mixed-algorithm scenario landed; it has been
superseded and removed since it no longer matches the current
`config.yaml`/`dashboard/` (which only support the two-rule, `x-tier`-matched
scenario).

## Dashboard v2: live per-app gauges/charts + narration-paced timeline

The first mixed-algorithm cut fired all 7 requests within the first ~15s
after `dashboard/`'s "Run scenario" button was clicked, then held on a
static log for the remaining ~60s of the ~77s narration -- correct evidence,
but visually inert for most of the clip, and it gave viewers no way to see
*how* the two algorithms differ, only their end states (200/429). Fixed by:

- **Narration-paced offsets.** Each request now fires at an offset (from
  `dashboard/index.html`'s `runScenario()`) chosen against
  `narration/narration.srt`'s actual cue timestamps, so the dashboard keeps
  producing new, narrated action for the full ~74s of the scenario instead
  of finishing in the first 15s. A short non-request "algorithm assignment"
  beat and a closing recap beat (which checks off the "what this proves"
  list live) were added so there's always something happening on screen,
  not just a static end state.
- **Live per-app budget gauges + 30s rolling sparklines.** A client-side
  `BudgetModel` per app replays the *same* arithmetic the Lua
  reserve/reconcile scripts perform server-side --
  `sliding_window`: capacity minus reservations still inside the trailing
  window; `token_bucket`: continuous linear refill up to capacity, drained
  per reservation -- driven off each request's real timestamp and cost.
  A `requestAnimationFrame` loop samples it every frame, so gold-tier's
  budget visibly sits flat at zero until the window slides (a step
  function/"cliff"), while silver-tier's visibly ramps upward continuously
  as it refills -- the exact visual contrast between the two algorithms
  that a plain admit/deny log couldn't show. This needs no extra requests
  to the gateways (which would themselves cost budget); every *transition*
  the model predicts is still cross-checked against the real HTTP
  status/headers of each actual request via `requireGate`, so the model is
  a visualization layer on top of the live evidence, not a replacement for
  it.
- **A caught timing bug, found by dry-running the new offsets first.**
  The original gap between silver-tier's exhaustion (its first admitted
  request) and the cross-instance denial check that was supposed to prove
  it (5.3s) was *longer* than `token_bucket`'s exact full-refill time
  (`capacity: 10 / refill_rate: 2` = 5.0s) -- so by the time of the check,
  the bucket had already silently refilled and the "expected 429" request
  returned 200 instead. Caught with a plain curl replay of the new offsets
  against the live stack *before* spending an actual browser recording run
  on it (see the dry-run transcript below); fixed by tightening the gap to
  2.0s (4/10 tokens refilled at that point, comfortably below the 10
  needed for admission).

```
$ curl replay of the new offsets, live stack, before re-recording:
t=9732ms   app-a@a (gold)   -> 200   (admitted)
t=13166ms  app-a@b (gold)   -> 429   (denied, cross-instance)
t=20297ms  app-c@a (gold)   -> 200   (admitted, own untouched budget)
t=25328ms  app-b@a (silver) -> 200   (admitted)
t=30658ms  app-b@b (silver) -> 200   (BUG: expected 429, bucket already refilled)
t=44687ms  app-a@a (gold)   -> 200   (recovered)
t=51718ms  app-b@b (silver) -> 200   (recovered)

after tightening the exhaustion->check gap from 5.3s to 2.0s:
t=...      app-a@a (gold)   -> 200
t=...      app-a@b (gold)   -> 429
t=...      app-c@a (gold)   -> 200
t=...      app-b@a (silver) -> 200
t=...      app-b@b (silver) -> 429  (fixed)
t=...      app-a@a (gold)   -> 200
t=...      app-b@b (silver) -> 200
```

## What it proves (evidence-manifest.json)

Seven live HTTP assertions (`requireGate` in `playwright/record.mjs`), each
checked against the real running gateways during the recording, not after
the fact:

1. app-a (`gold-tier`, `sliding_window`) admitted on Gateway A (`200`).
2. app-a denied on Gateway B, the *other* process (`429`,
   `X-RateLimit-Remaining-Tokens: 0`) -- proves the shared Valkey ledger,
   not per-replica state.
3. app-c (`gold-tier`, `sliding_window`) admitted on Gateway A (`200`) on
   its own untouched budget -- a third independent app, same rule, same
   Valkey namespace.
4. app-b (`silver-tier`, `token_bucket`) admitted on Gateway A (`200`) -- a
   different rule entirely, matched purely on `x-tier`.
5. app-b denied on Gateway B (`429`) -- proves the shared-Valkey,
   cross-instance guarantee holds for `token_bucket` too, not just
   `sliding_window`.
6. app-a, previously denied, admitted again on Gateway A (`200`) once the
   10s sliding window ages its earlier reservation out -- proves the
   window *slides* (budget recovers continuously as usage ages out) rather
   than requiring a manual reset or a fixed reset boundary.
7. app-b, previously denied, admitted again on Gateway B (`200`) once
   `silver-tier`'s bucket refilled continuously past its `estimate_tokens`
   cost -- a *different* recovery mechanism than app-a's window slide, also
   with no manual reset.

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
was scoped instead to `per_rule_algorithm_choice`,
`cross_instance_shared_budget`, `per_app_isolation`, `reservation_admission`,
`window_recovery`, and `token_bucket_recovery` -- matched 1:1 to the
`requireGate` assertions above.

## A finding this recording surfaced: the two algorithms reconcile differently

Both rules use reservation-based admission (reserve an `estimate_tokens`
cost up front, reconcile against the stub backend's actual
`usage.total_tokens: 10` once the response is known -- see
`../README.md`'s "Validate the request flow"). While tuning this recording's
parameters, reconciliation turned out to behave differently per algorithm,
not just per the design docs:

- **`token_bucket` (`silver-tier`) credits any estimate/actual gap straight
  back into the bucket.** With the tempting choice of `capacity: 40,
  estimate_tokens: 40` (matching `gold-tier`'s numbers, and the values
  originally used before this was discovered), the first reservation
  reserves 40, the response settles at the stub's real usage of 10, and the
  unused 30 is refunded back into the bucket -- once the background
  reconcile worker processes it (asynchronous, not on the response's own
  request path). In practice that refund plus a small amount of continuous
  refill is often enough to admit a *second* request well before any
  "real" capacity has been exhausted, which defeats the demo's own premise.
  Fixed here by setting `silver-tier`'s `estimate_tokens`/`capacity` to `10`
  -- equal to the stub's fixed real usage -- so the refund is always ~0 and
  exhaustion is deterministic regardless of when the async worker runs.
- **`sliding_window` (`gold-tier`) does not do this.** With
  `capacity: 40, estimate_tokens: 40`, the same 30-token refund happens
  (the reconcile math is shared), but it does not retroactively shrink the
  amount already counted against the trailing window -- the window tracks
  reservations against the estimate at admission time, not a "live"
  spendable balance the way a bucket does. That's why the original
  (single-algorithm) recording's `capacity: 40, estimate_tokens: 40` numbers
  worked correctly for `sliding_window` and needed no change here.

This is a real, previously-undocumented asymmetry between the two
algorithms' reservation/reconciliation semantics, not a demo-only artifact
-- see `../README.md`'s "Open design questions" for the follow-up this
raises upstream (should both algorithms free reconciled-away capacity
immediately, or should neither?).

## What "window recovery" and "bucket recovery" mean, and what they don't

`gold-tier`'s `window: 10s` and `silver-tier`'s `refill_rate: 2` tokens/sec
are both deliberately small so recovery is watchable; a production
deployment would use much larger/slower values (the demo's own
`../config.yaml` comments this accordingly). What's demonstrated is each
algorithm's *own* recovery mechanic:

- **`sliding_window`**: no reset event, no restart, no cron job -- app-a's
  budget recovers purely because its earlier reservation ages past the
  trailing 10s window as real time elapses.
- **`token_bucket`**: no reset event either, but the mechanism is
  different -- app-b's bucket refills continuously at a fixed rate
  (2 tokens/sec here) up to `capacity`, so it can become admissible again
  well before a fixed window would elapse.

See `../README.md`'s "Open design questions" section for why the choice
between these algorithms (and which one, if either, should be the default)
is still open upstream.

## Known deviations from the toolkit's default pipeline

`scripts/generate-narration.sh` and `scripts/generate-captions.sh` both
hard-require `OPENAI_API_KEY` (real OpenAI TTS + Whisper transcription).
**No such key was available in this environment**, so:

- `narration/narration.wav` was generated with a local, offline
  substitute -- macOS `say -v Samantha`, converted to WAV via `ffmpeg` --
  instead of the toolkit's OpenAI TTS call.
- `narration/narration.srt` caption timing is an **approximation**
  (proportional to sentence character count over the measured audio
  duration), not a real Whisper word-level transcription, via a small ad
  hoc substitute script (not part of the upstream toolkit).

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

## Recording environment

This mixed-algorithm recording was produced on a remote RHEL 9 lab host
(Podman + Compose, not Docker Desktop) rather than the local macOS spike
environment used for the original single-algorithm recording -- macOS
Podman machine networking was unreliable in this session's local
environment. Two host-specific things came up that aren't part of the
demo itself:

- No `ffmpeg`/`ffprobe` package is available via `dnf` on this host (no
  RPM Fusion/EPEL configured); a static build from
  [johnvansickle.com](https://johnvansickle.com/ffmpeg/) was used instead.
- Podman on an SELinux-enforcing host denies containers read access to
  bind-mounted files that aren't labeled for container access. Rather than
  changing the repo's `docker-compose.yml` (which would add an SELinux-only
  `:z`/`:Z` mount flag that other contributors on non-SELinux hosts don't
  need), the bind-mounted files were relabeled on the host directly:
  `chcon -Rt container_file_t config.yaml dashboard/nginx.conf
  dashboard/index.html`. Podman/Docker on non-SELinux hosts (macOS, most
  default Linux distros) need neither of these workarounds.

## NDA scrubbing

App names throughout (`app-a`, `app-b`, `app-c`) are placeholders. The real
customer scenario's application names are excluded here and enforced via
`production.yaml`'s `redaction.deny` list.

## Reproduce from scratch

Self-contained -- assumes nothing already checked out locally. Requires
Docker or Podman with Compose, Git, Node.js/npm, and ffmpeg/ffprobe on
`PATH` (`traffic-theater`'s own `scripts/doctor.sh` checks these). On an
SELinux-enforcing host, see "Recording environment" above for the extra
`chcon` step.

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
final media passed the toolkit's own validation gate. The flagged
deviations (local TTS, approximate captions, the lab-host ffmpeg/SELinux
workarounds) are cosmetic/environment-layer, not evidentiary -- they don't
affect what was actually proven about the filter's behavior. The
reconciliation-asymmetry finding above is a genuine discovery from tuning
this recording against the live stack, not a guess -- it's now called out
explicitly for upstream review rather than silently worked around.
