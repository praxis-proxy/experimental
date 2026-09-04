#!/usr/bin/env bash
#
# Drive the gateway hard enough that the free tier's token budget is genuinely
# reached, then show the premium tier absorbing the same load.
#
# The budgets in configs/token-budget.yaml are sized for this to be a real
# limit rather than a trick: the free tier's 5,000 tokens/min is reached by
# ordinary traffic, the way a production "protect the GPU" limit would be.
set -euo pipefail

for cmd in curl python3; do
  command -v "$cmd" >/dev/null 2>&1 || { echo "Error: $cmd not found"; exit 1; }
done

GATEWAY="${GATEWAY:-http://localhost:38080}"
# A SMALL model on purpose. The rate limiter makes its decision at admission,
# before the upstream is called, so the model is irrelevant to what this
# demonstrates -- but it must be fast enough that the burst lands inside the
# 1m sliding window. Measured on an M4 Max: qwen3.5:0.8b answers in ~0.3s warm,
# while qwen3.8:27b takes 74-78s under 4-way concurrency, so 40 requests would
# span ~12 minutes and the budget would age out faster than it was consumed --
# the limit would never be reached.
#
# Use qwen3.8:27b for the quality demo in step 7, not for this.
MODEL="${MODEL:-qwen3.5:0.8b}"
# Sized to actually exhaust the free tier. Reservations reconcile against real
# usage, so sustained throughput is governed by actual tokens (~68 per request
# here), not by the 200-token estimate: 5,000 / 68 admits roughly 70 requests
# in the window. 40 requests never reach the limit.
REQUESTS="${REQUESTS:-100}"
CONCURRENCY="${CONCURRENCY:-8}"
# Ollama serializes requests per model unless OLLAMA_NUM_PARALLEL is raised.
# Denied requests are rejected at admission and return instantly; only admitted
# ones reach the model.
TIMEOUT="${TIMEOUT:-60}"

# qwen3.8 is a reasoning model: it emits reasoning tokens before content, and
# they count toward completion_tokens. A small max_tokens spends the whole
# budget on reasoning and returns empty content, which looks like a failure
# but is not.
MAX_TOKENS="${MAX_TOKENS:-60}"

send() {
  local tier="$1" idx="$2"
  # The free tier is the catch-all rule: it matches by NOT carrying the premium
  # header, so sending a placeholder header would imply a match that no rule
  # makes. An empty array expands to "unbound" under `set -u` on bash 3.2,
  # which is what macOS ships, hence the ${arr[@]+...} guard at the call site.
  local tier_header=()
  if [[ "${tier}" == "premium" ]]; then
    tier_header=(-H "X-Tier: premium")
  fi
  curl -s -o /dev/null -w '%{http_code}\n' --max-time "${TIMEOUT}" \
    "${GATEWAY}/v1/chat/completions" \
    -H 'Content-Type: application/json' ${tier_header[@]+"${tier_header[@]}"} \
    -d "{\"model\":\"${MODEL}\",\"messages\":[{\"role\":\"user\",\"content\":\"reply with the number ${idx}\"}],\"max_tokens\":${MAX_TOKENS}}"
}

run_tier() {
  local tier="$1"
  echo "=========================================="
  echo "  ${REQUESTS} requests as tier: ${tier}"
  echo "=========================================="
  local tmp
  tmp="$(mktemp)"
  # Throttle with xargs -P rather than `wait -n`: `wait -n` is bash 4.3+, and
  # macOS ships bash 3.2, where it fails silently and the cap never applies.
  export -f send
  export GATEWAY MODEL MAX_TOKENS TIMEOUT
  # shellcheck disable=SC2016  # $1/$2 are for the inner bash, not this shell
  seq 1 "${REQUESTS}" | xargs -P "${CONCURRENCY}" -I{} bash -c 'send "$1" "$2"' _ "${tier}" {} >> "${tmp}"

  python3 - "${tmp}" "${tier}" <<'PY'
import collections, sys
codes = collections.Counter(l.strip() for l in open(sys.argv[1]) if l.strip())
total = sum(codes.values())
ok, limited = codes.get("200", 0), codes.get("429", 0)
print(f"  200 OK          {ok:>4}")
print(f"  429 rate limited{limited:>4}")
other = {k: v for k, v in codes.items() if k not in ("200", "429")}
if other:
    print(f"  other           {other}")
if total:
    print(f"  -> {limited / total:.0%} of requests hit the {sys.argv[2]} tier's token budget")
PY
  rm -f "${tmp}"
  echo ""
}

echo "Gateway: ${GATEWAY}   model: ${MODEL}"
echo ""
run_tier free
echo "Waiting 65s for the sliding window to age out before the premium run..."
sleep 65
run_tier premium

cat <<'EOF'
==========================================
  Where to look
==========================================
  Praxis AI Gateway Overview
    http://localhost:33000/d/praxis-ai-gateway-overview
      "Admitted vs Denied, per Tier"  - the two rules side by side
      "Denial Rate per Tier"          - free non-zero, premium at zero
      "Token Consumption Rate"        - actual tokens, reconciled

  Budget state straight from the gateway:
    kubectl exec -n default deploy/praxis-proxy -- \
      wget -qO- http://127.0.0.1:9901/metrics | grep token_rate_limit
EOF
