#!/usr/bin/env bash
# Verify a running ai-gateway demo, on either target.
#
#   compose: ./verify.sh http://localhost:8080 http://localhost:9090 http://localhost:3200
#   KIND:    ./verify.sh http://localhost:38080 http://localhost:39090
#
# Prometheus and Tempo are optional: pass them to check the observability path
# too. Exits non-zero on the first failed check.
set -euo pipefail

GATEWAY="${1:-${GATEWAY:-http://localhost:8080}}"
PROMETHEUS="${2:-${PROMETHEUS:-}}"
TEMPO="${3:-${TEMPO:-}}"

# The CI backend (grid-mock-providers) serves chat completions but not the model
# list, so that one check is skippable.
SKIP_MODELS="${SKIP_MODELS:-0}"
# The budget probe deliberately floods the gateway until it refuses. Against a
# paid provider every admitted request costs money, so it is skippable.
SKIP_BUDGET="${SKIP_BUDGET:-0}"
# Scrape job to assert on. Both targets use this name: compose sets it in
# compose/prometheus.yaml, and on KIND the Prometheus Operator derives it from
# manifests/servicemonitor.yaml.
PRAXIS_JOB="${PRAXIS_JOB:-praxis-proxy}"
MODEL="${MODEL:-qwen3.5:0.8b}"
# Which wire format the upstream speaks. Anthropic serves /v1/messages with its
# own body shape rather than OpenAI's /v1/chat/completions, and praxis does not
# translate between them, so the probe has to match the provider.
WIRE="${WIRE:-openai}"
# Enough concurrent load to exhaust the free tier's per-minute token budget.
#
# Sequential requests never trigger it: token_rate_limit reserves an estimate at
# admission and refunds the unused part when the response completes, so one
# request at a time is refunded faster than the window fills. Concurrency is what
# keeps several reservations outstanding at once. These mirror rate-limit-demo.sh.
BUDGET_REQUESTS="${BUDGET_REQUESTS:-60}"
BUDGET_CONCURRENCY="${BUDGET_CONCURRENCY:-8}"
BUDGET_MAX_TOKENS="${BUDGET_MAX_TOKENS:-60}"

pass() { printf '  ok    %s\n' "$1"; }
fail() { printf '  FAIL  %s\n' "$1" >&2; exit 1; }

# Waits for an endpoint to answer, so callers do not have to sleep first.
wait_for() {
  local url="$1" name="$2" tries="${3:-60}"
  for _ in $(seq 1 "${tries}"); do
    # Any HTTP response means the listener is up. --fail would reject the 401 a
    # hosted provider returns for GET /, which is not a readiness signal.
    if [ "$(curl --silent --max-time 3 --output /dev/null --write-out '%{http_code}' "${url}" 2>/dev/null)" != "000" ]; then
      pass "${name} is up"
      return 0
    fi
    sleep 2
  done
  fail "${name} did not become ready: ${url}"
}

echo "gateway: ${GATEWAY}"
wait_for "${GATEWAY}/" "gateway"

# Timestamp taken before any request, so the Tempo check below can require a
# trace from THIS run rather than matching one left by a previous one. One second
# of slack absorbs clock rounding between here and Tempo.
RUN_START="$(( $(date +%s) - 1 ))"

# 1. Model list. Anthropic returns {"data":[...]}, OpenAI an "object" list.
if [ "${SKIP_MODELS}" = "1" ]; then
  echo "  skip  model list (SKIP_MODELS=1)"
else
  models_key='"object"'
  if [ "${WIRE}" = "anthropic" ]; then
    models_key='"data"'
  fi
  if curl --fail --silent --max-time 10 "${GATEWAY}/v1/models" | grep -q "${models_key}"; then
    pass "model list"
  else
    fail "model list did not return an object list"
  fi
fi

# 2. A chat completion, which is also what puts a trace in Tempo.
chat() {
  if [ "${WIRE}" = "anthropic" ]; then
    curl --silent --max-time 180 --output "$2" --write-out '%{http_code}' \
      "${GATEWAY}/v1/messages" \
      --header 'Content-Type: application/json' \
      --header 'anthropic-version: 2023-06-01' \
      ${3:+--header "$3"} \
      --data "{\"model\":\"${MODEL}\",\"max_tokens\":16,\"messages\":[{\"role\":\"user\",\"content\":\"say hi\"}]}"
  else
    curl --silent --max-time 180 --output "$2" --write-out '%{http_code}' \
      "${GATEWAY}/v1/chat/completions" \
      --header 'Content-Type: application/json' \
      ${3:+--header "$3"} \
      --data "{\"model\":\"${MODEL}\",\"messages\":[{\"role\":\"user\",\"content\":\"say hi\"}],\"max_tokens\":16}"
  fi
}

body="$(mktemp)"
trap 'rm -f "${body}"' EXIT
code="$(chat "${MODEL}" "${body}")"
[ "${code}" = "200" ] || fail "chat completion returned ${code}: $(head -c 200 "${body}")"
pass "chat completion"

# 3. The free tier runs out of tokens and is refused, while premium is not.
#    This is the token_rate_limit filter doing its job.
if [ "${SKIP_BUDGET}" = "1" ]; then
  echo "  skip  token budget probe (SKIP_BUDGET=1)"
else
send_one() {
  curl --silent --max-time 60 --output /dev/null --write-out '%{http_code}\n' \
    "${GATEWAY}/v1/chat/completions" \
    --header 'Content-Type: application/json' \
    --data "{\"model\":\"${MODEL}\",\"messages\":[{\"role\":\"user\",\"content\":\"reply with a number\"}],\"max_tokens\":${BUDGET_MAX_TOKENS}}"
}
export -f send_one
export GATEWAY MODEL BUDGET_MAX_TOKENS

codes="$(mktemp)"
trap 'rm -f "${body}" "${codes}"' EXIT
# xargs exits 123 when a child fails and curl fails on a dropped connection;
# neither invalidates the run, so the status is deliberately ignored.
seq 1 "${BUDGET_REQUESTS}" \
  | xargs -P "${BUDGET_CONCURRENCY}" -I{} bash -c 'send_one' _ > "${codes}" 2>/dev/null || true

refused="$(grep -c '^429$' "${codes}" || true)"
served="$(grep -c '^200$' "${codes}" || true)"
[ "${refused}" -gt 0 ] || fail "free tier never hit its token budget in ${BUDGET_REQUESTS} requests (${served} served)"
pass "free tier is rate limited (${refused} refused, ${served} served)"

# Require 200, not merely "not 429": a 401, a 500 or a dropped connection all
# used to pass this check silently.
premium_code="$(chat "${MODEL}" /dev/null 'X-Tier: premium')"
case "${premium_code}" in
  200) pass "premium tier is served while the free tier is limited" ;;
  429) fail "premium tier was rate limited" ;;
  *) fail "premium tier returned ${premium_code}, expected 200" ;;
esac
fi

# 4. Traces reached Tempo.
if [ -n "${TEMPO}" ]; then
  # Time-bounded: an unbounded search matches traces from a previous run, so the
  # check passed even when this run exported nothing. RUN_START is set before the
  # first request above.
  found=0
  for _ in $(seq 1 15); do
    now="$(date +%s)"
    if curl --fail --silent --max-time 5 --get \
      --data-urlencode "start=${RUN_START}" \
      --data-urlencode "end=${now}" \
      --data-urlencode "limit=5" \
      "${TEMPO}/api/search" | grep -q '"rootServiceName"'; then
      found=1
      break
    fi
    sleep 2
  done
  [ "${found}" = "1" ] || fail "no traces from this run reached Tempo at ${TEMPO} (searched from ${RUN_START})"
  pass "traces from this run are in Tempo"
fi

# 5. Prometheus is scraping the gateway.
if [ -n "${PROMETHEUS}" ]; then
  up=0
  for _ in $(seq 1 15); do
    # The VALUE must be 1. Grepping for the string "value" also matched an
    # up=0 response, so a scrape target that was down reported as healthy.
    if curl --fail --silent --max-time 5 --get \
      --data-urlencode "query=up{job=\"${PRAXIS_JOB}\"}" \
      "${PROMETHEUS}/api/v1/query" \
      | grep -qE '"value":\[[0-9.]+,"1(\.0+)?"\]'; then
      up=1
      break
    fi
    sleep 2
  done
  [ "${up}" = "1" ] || fail "Prometheus reports up{job=\"${PRAXIS_JOB}\"} != 1 at ${PROMETHEUS}"
  pass "Prometheus scrapes the gateway (job ${PRAXIS_JOB})"
fi

echo "all checks passed"
