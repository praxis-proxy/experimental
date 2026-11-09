#!/usr/bin/env bash
set -euo pipefail

# Runs the two required external-metering outcomes without provider credentials:
# an external-budget 429 that reaches the client, then the same 429 with one
# free-model fallback. Requires Docker and a checkout of the pinned PriceTag source.

DEMO_DIR="$(cd -- "$(dirname -- "${BASH_SOURCE[0]}")" && pwd)"
readonly DEMO_DIR
readonly COMPOSE=(docker compose -f "${DEMO_DIR}/docker-compose.yml")
readonly GATEWAY_URL=http://127.0.0.1:18080/v1/chat/completions
readonly MOCK_URL=http://127.0.0.1:18090/counts
readonly PRICETAG_COMMIT=557ceb1cbe1316d93e0f682baa8301455956fe18

source_dir=${PRICETAG_METERING_SOURCE:-"${DEMO_DIR}/../../../pricetag-metering"}
keep_containers=${KEEP_CONTAINERS:-0}

cleanup() {
  if [[ "$keep_containers" != 1 ]]; then
    "${COMPOSE[@]}" down --volumes --remove-orphans
  fi
}
trap cleanup EXIT

die() {
  echo "error: $*" >&2
  exit 1
}

wait_for_port() {
  local port=$1 attempt
  for ((attempt = 1; attempt <= 120; attempt++)); do
    if (exec 3<>"/dev/tcp/127.0.0.1/${port}") 2>/dev/null; then
      exec 3>&-
      return
    fi
    sleep 1
  done
  die "port ${port} did not become ready"
}

request() {
  local response_file=$1
  curl --silent --show-error --max-time 20 --output "$response_file" --write-out '%{http_code}' \
    --request POST "$GATEWAY_URL" --header 'content-type: application/json' \
    --header 'x-tenant-model: budgeted-model' \
    --data '{"model":"client-requested-model","messages":[{"role":"user","content":"budget demo"}],"max_tokens":8}'
}

assert_counts() {
  local expected_weak=$1 expected_free=$2 counts
  counts=$(curl --fail --silent --show-error "$MOCK_URL")
  COUNTS_JSON="$counts" python3 - "$expected_weak" "$expected_free" <<'PY'
import json
import os
import sys

counts = json.loads(os.environ["COUNTS_JSON"])
assert counts == {"weak": int(sys.argv[1]), "strong": 0, "free": int(sys.argv[2])}, counts
PY
}

wait_for_usage_event() {
  local attempt rows
  for ((attempt = 1; attempt <= 60; attempt++)); do
    rows=$("${COMPOSE[@]}" exec -T postgres psql -U metering -d metering -Atqc \
      "SELECT count(*) FROM usage_events WHERE username = 'demo-user' AND model = 'budgeted-model' AND total_tokens = 5 AND cost_usd = 0.05 AND status_code = 200")
    if [[ "$rows" == 1 ]]; then
      return
    fi
    sleep 1
  done
  die 'PriceTag did not record the allowed request'
}

start_gateway() {
  local config=$1
  export GATEWAY_CONFIG="configs/${config}.yaml"
  "${COMPOSE[@]}" up --detach --build --force-recreate gateway
  wait_for_port 18080
}

[[ -d "$source_dir/.git" ]] || die "set PRICETAG_METERING_SOURCE to a checkout at ${PRICETAG_COMMIT}"
actual_commit=$(git -C "$source_dir" rev-parse HEAD)
[[ "$actual_commit" == "$PRICETAG_COMMIT" ]] || die "PriceTag checkout must be at ${PRICETAG_COMMIT}; fetch it with: git -C ${source_dir} fetch origin ${PRICETAG_COMMIT}"

export METERING_IMAGE
METERING_IMAGE=$("${DEMO_DIR}/build-metering" "$source_dir")
export GATEWAY_CONFIG=configs/no-fallback.yaml
"${COMPOSE[@]}" down --volumes --remove-orphans
"${COMPOSE[@]}" up --detach --build
wait_for_port 18080
wait_for_port 18090

first_response=$(mktemp)
second_response=$(mktemp)
third_response=$(mktemp)
trap 'rm -f "$first_response" "$second_response" "$third_response"; cleanup' EXIT

[[ $(request "$first_response") == 200 ]] || die 'the initial budgeted request was not allowed'
grep -q 'served_by=weak' "$first_response" || die 'the allowed request did not reach the Switchyard weak target'
wait_for_usage_event
assert_counts 1 0
echo 'PASS: allowed request reached Switchyard and PriceTag recorded USD spend'

# PriceTag caches the positive entitlement decision for 15 seconds. Wait one
# extra second before verifying that the first response exhausted the budget.
sleep 16
[[ $(request "$second_response") == 429 ]] || die 'expected an external-budget HTTP 429'
grep -q 'token budget exhausted' "$second_response" || die 'the denial was not emitted by external metering'
assert_counts 1 0
echo 'PASS: external PriceTag USD budget exhaustion returned 429; no model was called'

start_gateway with-free-fallback
[[ $(request "$third_response") == 200 ]] || die 'expected the configured free fallback to succeed'
grep -q 'served_by=free' "$third_response" || die 'the fallback response did not come from the free target'
assert_counts 1 1
echo 'PASS: external-budget 429 caused exactly one configured free-model attempt'
