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

**Watch:** `output/k8s-real-pods-token-rate-limit.mp4` (1920x1080, h264/aac,
~143s)

`dashboard/index.html` uses a generic (unbranded) dark theme, deliberately
kept free of any company branding since this repo is meant for
community/contributor adoption. A Red Hat-branded skin variant (dark theme
using Red Hat's own brand palette/type, per
redhat.com/en/about/brand/standards) was also produced and recorded/
validated identically to this one, for the team to decide separately
whether a branded variant belongs in this demo at all; it is intentionally
not part of this PR.

An earlier, single-algorithm (`sliding_window`-only) recording of this same
demo existed before the mixed-algorithm scenario landed; it has been
superseded and removed since it no longer matches the current
`config.yaml`/`dashboard/` (which only support the two-rule, `x-tier`-matched
scenario).

## Update: real Kubernetes deployment, retuned token values, live cluster topology

The stack now runs on a real `kind` Kubernetes cluster (`k8s/`, `deploy.sh`)
instead of `docker-compose.yml` for recording purposes (the compose file is
kept in sync and still works for the `curl` walkthrough in `../README.md`,
but the video itself is produced against the K8s deployment). This
addressed feedback that the previous recording "read like a fake video":

- **Real pod identities everywhere**, not placeholder names: every
  `budget-card`, gateway-log panel, and the new namespace/pod-status strip
  at the top of the dashboard shows actual `metadata.name` pod names
  resolved live via the Kubernetes API (see `k8s/00-namespace-rbac.yaml`'s
  RBAC grant and `k8s/04-apps.yaml`'s `/whoami`, `/gateway-pod`, and
  `/cluster-pods` endpoints) -- app-a/b/c keep their role labels as the
  primary identifier (they're the real `x-app-id` rate-limit key), but the
  pod name shown alongside each is the literal, currently-scheduled pod.
- **Live gateway stdout**, streamed via Server-Sent Events from the
  Kubernetes `pods/log` subresource (`k8s/04-apps.yaml`'s `/gw-logs`
  endpoint), client-side reformatted for legibility and with `client_ip`
  resolved back to an app name via each app's Downward-API `pod_ip`.
- **Retuned `estimate_tokens`** for more visible traffic: `gold-tier`
  15/40 (was 40/40) and `silver-tier` 7/10 (was 10/10), so a single
  request no longer exhausts either budget outright -- see
  `../config.yaml`'s comment and `../README.md`'s "Validate the request
  flow" for the exact admit/deny/recover math this produces, and why
  `estimate_tokens` must still exactly match the stub backend's (now
  tier-aware, see `k8s/02-backend.yaml`) reported `usage.total_tokens`.
  The scenario grew from 7 to 8 requests: gold-tier now shows a genuine
  3-request burst (admit, admit, deny) instead of a 1-request cliff-edge,
  and silver-tier's recovery is demonstrably much faster (a couple of
  seconds) than gold-tier's (the full 10s window).
- **A live namespace/pod-status strip** (`#cluster-strip` in
  `dashboard/index.html`) shows every pod in the `trl-demo` namespace --
  not just the ones each card already links to -- fetched from the real
  Kubernetes API and polled every 5s, as the single strongest "this isn't
  a mock" signal available (the entire namespace's actual live state, not
  a curated subset).

All narration/timing (`narration/narration.txt`, `.srt`) and
`playwright/record.mjs`'s `requireGate` assertions were updated to match
the new 8-request scenario; see their headers/comments for specifics. The
"Seven live HTTP assertions" and per-algorithm numeric details in the
sections below describe the *original* mixed-algorithm recording's
original `capacity: 40, estimate_tokens: 40` / `capacity: 10,
estimate_tokens: 10` numbers -- they are not the current `estimate_tokens`
values (see this section and `../config.yaml` for those), and the
"reconciliation asymmetry" language they and the following section
originally used has since been corrected (see "Correction" section below).
The scenario is now 8 requests, not 7.

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

```text
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

## Correction: an earlier version of this doc claimed a false algorithm asymmetry

An earlier pass of this recording's tuning notes claimed that
`token_bucket` credits an estimate/actual refund back into its balance
while `sliding_window` does not retroactively shrink what's counted
against its window -- framed as a genuine, previously-undocumented
asymmetry between the two algorithms. **That claim was wrong** and has
been removed from this doc, `../README.md`, `../config.yaml`, and
`evidence-manifest.json`. Re-reading the source branch's actual reconcile
implementations (`token_rate_limit::ledger::Ledger::reconcile` and
`token_rate_limit::token_bucket_ledger::TokenBucketLedger::reconcile`)
shows both apply the identical `estimate`-vs-`actual` delta: `sliding_window`
records the *settled* usage entry at the actual token count (not the
estimate), and since window usage is summed fresh from settled + active
entries on every call, an overestimate genuinely does shrink what's counted
against the window -- there is a dedicated passing unit test for exactly
this,
`reconcile_releases_unused_tokens_on_overestimate` (`filters/src/token_rate_limit/tests.rs`).
Both algorithms reconcile symmetrically as currently implemented.

What actually needed fixing, and what the retuning above was really about:
this demo's stub backend originally reported a single **fixed**
`usage.total_tokens` value regardless of which tier's request it was
serving, which could mismatch whatever `estimate_tokens` a tier configured
-- a demo-configuration bug, not an algorithm-level design difference. It's
now fixed by making the stub backend tier-aware (see `../k8s/02-backend.yaml`
and `../docker-compose.yml`), so `estimate_tokens` always matches
`usage.total_tokens` exactly for both tiers and reconciliation's refund/
overage math nets to ~0 either way. There is no known open design question
here upstream; this section previously implied one that doesn't hold up
against the actual code.

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
affect what was actually proven about the filter's behavior. An earlier
pass of this doc also claimed a "reconciliation asymmetry" between the two
algorithms as a finding from tuning this recording; that claim did not hold
up against the source branch's actual reconcile code and tests, and has
been corrected above -- flagged here as a reminder that this doc's own
design-adjacent claims (as opposed to the live HTTP assertions) need the
same evidentiary bar, not just plausibility.
