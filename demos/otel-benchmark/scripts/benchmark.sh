#!/usr/bin/env bash
set -euo pipefail

for cmd in vegeta kubectl kind; do
  command -v "$cmd" >/dev/null 2>&1 || { echo "Error: $cmd not found"; exit 1; }
done

SCRIPT_DIR="$(cd "$(dirname "$0")" && pwd)"
DEMO_DIR="$(cd "${SCRIPT_DIR}/.." && pwd)"

CLUSTER_NAME="${CLUSTER_NAME:-otel-bench-local}"
CTX="kind-${CLUSTER_NAME}"
GATEWAY_URL="http://localhost:18080"

DURATION="${DURATION:-30s}"
RUNS="${RUNS:-3}"

# ---- Argument parsing ----
SCENARIO="ai"
while [[ $# -gt 0 ]]; do
  case "$1" in
    --scenario)
      SCENARIO="${2:?missing value for --scenario}"
      shift 2
      ;;
    --scenario=*)
      SCENARIO="${1#*=}"
      shift
      ;;
    -h|--help)
      echo "Usage: $0 [--scenario core|ai]"
      exit 0
      ;;
    *)
      echo "Unknown argument: $1" >&2
      echo "Usage: $0 [--scenario core|ai]" >&2
      exit 1
      ;;
  esac
done

# ---- vegeta target generators ----
core_target() {
  echo "GET ${GATEWAY_URL}/"
}

ai_target() {
  printf 'POST %s/v1/chat/completions\nContent-Type: application/json\n@%s\n' \
    "${GATEWAY_URL}" "${SCRIPT_DIR}/ai-payload.json"
}

# ---- Scenario parameters ----
case "${SCENARIO}" in
  core)
    RATE="${RATE:-2000}"
    CONNECTIONS=200
    WARMUP_RATE=500
    CONFIGS=(baseline baseline otel-full)
    LABELS=(baseline otel-noop otel-full)
    RESULTS_PREFIX=""
    TARGET_FN=core_target
    SCENARIO_TITLE=""
    REPORT_SCRIPT="report.sh"
    ;;
  ai)
    RATE="${RATE:-500}"
    CONNECTIONS=100
    WARMUP_RATE=100
    CONFIGS=(ai-baseline ai-baseline ai-otel-full)
    LABELS=(ai-baseline ai-otel-noop ai-otel-full)
    RESULTS_PREFIX="ai-"
    TARGET_FN=ai_target
    SCENARIO_TITLE="AI "
    REPORT_SCRIPT="report.sh"
    ;;
  *)
    echo "Usage: $0 [--scenario core|ai]" >&2
    exit 1
    ;;
esac

RESULTS_DIR="${DEMO_DIR}/results/${RESULTS_PREFIX}$(date +%Y%m%d-%H%M%S)"
mkdir -p "${RESULTS_DIR}"


echo "=== Praxis ${SCENARIO_TITLE}OTel Benchmark ==="
echo "Rate: ${RATE} RPS | Duration: ${DURATION} | Runs: ${RUNS}"
echo "Results: ${RESULTS_DIR}"
echo ""

run_vegeta() {
  local label="$1"
  local run="$2"
  echo "--- ${label} run ${run}/${RUNS} ---"
  "${TARGET_FN}" | \
    vegeta attack -rate="${RATE}" -duration="${DURATION}" -connections="${CONNECTIONS}" | \
    tee "${RESULTS_DIR}/${label}-run${run}.bin" | \
    vegeta report -type=json > "${RESULTS_DIR}/${label}-run${run}.json"
  vegeta report < "${RESULTS_DIR}/${label}-run${run}.bin"
  echo ""
}

# ---- Run A/B/C definitions ----
RUN_LETTERS=(A B C)
RUN_DESCRIPTIONS=(
  "${SCENARIO_TITLE}Baseline (praxis-experimental:dev, no OTel)"
  "${SCENARIO_TITLE}OTel noop (praxis-experimental:dev-otel, no endpoint)"
  "${SCENARIO_TITLE}OTel full (praxis-experimental:dev-otel, exporting)"
)
IMAGES=(dev dev-otel dev-otel)

# Record what this run actually exercised, so the report describes itself and
# cannot drift from the configs. Cargo features are read back off the image
# label rather than assumed, and the filter chain is read out of the configs.
{
  echo "scenario=${SCENARIO}"
  echo "rate=${RATE}"
  echo "duration=${DURATION}"
  echo "runs=${RUNS}"
  echo "connections=${CONNECTIONS}"
  for idx in 0 1 2; do
    img="praxis-experimental:${IMAGES[$idx]}"
    feats=$("${CONTAINER_ENGINE:-docker}" inspect --format \
      '{{index .Config.Labels "io.praxis.build.features"}}' "$img" 2>/dev/null || echo "unknown")
    echo "run.${RUN_LETTERS[$idx]}.label=${LABELS[$idx]}"
    echo "run.${RUN_LETTERS[$idx]}.image=${IMAGES[$idx]}"
    echo "run.${RUN_LETTERS[$idx]}.features=${feats:-none}"
    echo "run.${RUN_LETTERS[$idx]}.config=${CONFIGS[$idx]}.yaml"
    echo "run.${RUN_LETTERS[$idx]}.filters=$(grep -E '^      - filter:' \
      "${DEMO_DIR}/configs/${CONFIGS[$idx]}.yaml" | sed 's/.*filter: //' | paste -sd, -)"
    echo "run.${RUN_LETTERS[$idx]}.sampling=$(grep -E '^  sampling_rate:' \
      "${DEMO_DIR}/configs/${CONFIGS[$idx]}.yaml" | sed 's/.*sampling_rate: //' || true)"
  done
} > "${RESULTS_DIR}/scenario.env"

# Runs are INTERLEAVED (A B C, A B C, ...) rather than grouped (AAA BBB CCC).
# Grouped ordering aliases any drift over time -- thermal, page-cache warming,
# background load -- onto the config comparison, and the config that runs first
# absorbs all of it. That systematically biased the baseline slow and made OTel
# look free, or even negative. Interleaving gives every config the same
# distribution of machine states. The cost is RUNS x 3 pod rollouts instead of
# 3, which is the price of a comparison that means something.
deploy_config() {
  local idx="$1"
  kubectl --context "${CTX}" create configmap praxis-config \
    --from-file=praxis.yaml="${DEMO_DIR}/configs/${CONFIGS[$idx]}.yaml" \
    -n default --dry-run=client -o yaml | kubectl --context "${CTX}" apply -f - >/dev/null
  kubectl --context "${CTX}" set image deployment/praxis-proxy \
    praxis-proxy=praxis-experimental:"${IMAGES[$idx]}" -n default >/dev/null

  if [[ "${idx}" -eq 2 ]]; then
    # Set the OTLP endpoint BEFORE restarting, so the pod starts exporting.
    kubectl --context "${CTX}" set env deployment/praxis-proxy \
      OTEL_EXPORTER_OTLP_ENDPOINT=http://otel-collector.otel.svc:4317 -n default >/dev/null
  else
    kubectl --context "${CTX}" set env deployment/praxis-proxy \
      OTEL_EXPORTER_OTLP_ENDPOINT- -n default >/dev/null 2>&1 || true
  fi

  kubectl --context "${CTX}" scale deployment/praxis-proxy --replicas=0 -n default >/dev/null
  sleep 3
  kubectl --context "${CTX}" scale deployment/praxis-proxy --replicas=1 -n default >/dev/null
  kubectl --context "${CTX}" -n default wait --for=condition=Available \
    deployment/praxis-proxy --timeout 60s >/dev/null
  sleep 5

  # Every run is preceded by its own warmup, because every run follows a fresh
  # pod. Without this the first measurement of each config is a rollout
  # artifact rather than a measurement.
  "${TARGET_FN}" | vegeta attack -rate="${WARMUP_RATE}" -duration=10s > /dev/null 2>&1 || true
  sleep 2
}

for round in $(seq 1 "${RUNS}"); do
  echo "=========================================="
  echo "  Round ${round}/${RUNS}"
  echo "=========================================="
  for idx in 0 1 2; do
    echo "--- ${RUN_LETTERS[$idx]}: ${RUN_DESCRIPTIONS[$idx]} ---"
    deploy_config "${idx}"
    run_vegeta "${LABELS[$idx]}" "${round}"
    sleep 5
  done
done

echo "=========================================="
echo "  ${SCENARIO_TITLE}Benchmark complete"
echo "=========================================="
echo "Results in: ${RESULTS_DIR}"
echo ""
echo "Generate report:"
echo "  bash ${SCRIPT_DIR}/${REPORT_SCRIPT} ${RESULTS_DIR}"
