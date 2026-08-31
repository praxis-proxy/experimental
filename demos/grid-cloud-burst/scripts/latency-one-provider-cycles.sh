#!/usr/bin/env bash
set -Eeuo pipefail

ROOT="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
K="${KUBECONFIG:-/tmp/rhoai-sim-validation.kubeconfig}"
NS="${GRID_NAMESPACE:-grid-system}"
UI="${UI_URL:?set UI_URL to the deployed cloud-burst UI URL}"
USER="${UI_USER:?set UI_USER to the deployed UI username}"
PASS="${UI_PASSWORD:?set UI_PASSWORD to the deployed UI password}"
EVIDENCE_ROOT="${EVIDENCE_ROOT:-$ROOT/evidence}"
STAMP="$(date -u +%Y%m%dT%H%M%SZ)"
E="$EVIDENCE_ROOT/latency-one-provider-$STAMP"
mkdir -p "$E/raw"
TRAFFIC_PID=""
SCENARIO="preflight"
LOCAL=(llm-d-east-1 llm-d-east-2 llm-d-west-1 llm-d-west-2)

log() { printf '[%s] %s\n' "$(date -u +%FT%T.%3NZ)" "$*" | tee -a "$E/runner.log" >&2; }
kc() { kubectl --kubeconfig "$K" -n "$NS" "$@"; }
ui() { curl -ksS --connect-timeout 5 --max-time 20 -u "$USER:$PASS" "$@"; }

snapshot() {
  local label="$1" now overlay phase
  now="$(date -u +%FT%T.%3NZ)"
  overlay="$(kc get cm grid-overlay-grid-cloud-burst-rhoai-consumer-east -o json 2>/dev/null | jq -c '.data["routing-overlay.json"]|fromjson|{revision:.revision,candidates:(.overlay.candidates|map({cluster,stable_id,selection_group,admission_state}))}' 2>/dev/null || echo '{"revision":null,"candidates":[]}' )"
  phase="$(kc get gridnetwork grid-cloud-burst-rhoai -o jsonpath='{.status.phase}' 2>/dev/null || true)"
  jq -nc --arg at "$now" --arg scenario "$SCENARIO" --arg label "$label" --arg phase "$phase" --argjson overlay "$overlay" \
    '{at:$at,scenario:$scenario,label:$label,gridnetwork_phase:$phase,overlay:$overlay}' >>"$E/state-samples.jsonl"
}

set_disabled() {
  local provider="$1" disabled="$2"
  ui -X POST -H 'content-type: application/json' -d "{\"provider\":\"$provider\",\"disabled\":$disabled}" \
    "$UI/api/v1/cloud-burst/provider" >"$E/raw/${SCENARIO}-${provider}-${disabled}.json"
}

restore_all() {
  local provider
  ui -X POST -H 'content-type: application/json' -d '{"on":false,"mode":"sim"}' "$UI/api/v1/cloud-burst/load" >"$E/raw/${SCENARIO}-pressure-off.json" 2>&1 || true
  for provider in "${LOCAL[@]}"; do set_disabled "$provider" false; done
  for _ in {1..90}; do
    local ready=0
    for provider in "${LOCAL[@]}"; do
      [[ "$(kc get deploy "$provider" -o jsonpath='{.status.readyReplicas}' 2>/dev/null || true)" == 1 ]] && ready=$((ready+1))
    done
    [[ "$ready" == 4 ]] && return 0
    sleep 1
  done
  return 1
}

request_one() {
  local consumer="$1" app="app1" id start end body status latency provider gateway
  id="lat-${STAMP}-${SCENARIO}-$$-$(date -u +%s%N)"
  start="$(date -u +%FT%T.%3NZ)"; start_ns="$(date +%s%N)"
  body="$(ui -X POST -H 'content-type: application/json' -H "X-Request-ID: $id" -d "{\"consumer\":\"$consumer\",\"app\":\"$app\"}" "$UI/api/v1/token-rate-limit/requests" 2>/dev/null || true)"
  end="$(date -u +%FT%T.%3NZ)"; end_ns="$(date +%s%N)"
  latency="$(awk -v s="$start_ns" -v e="$end_ns" 'BEGIN{printf "%.3f",(e-s)/1000000}')"
  status="$(jq -r '.record.http.status // .record.status // 0' <<<"$body" 2>/dev/null || echo 0)"
  provider="$(jq -r '.record.inference_provider // ""' <<<"$body" 2>/dev/null || true)"
  gateway="$(jq -r '.record.route.provider_gateway // ""' <<<"$body" 2>/dev/null || true)"
  jq -nc --arg scenario "$SCENARIO" --arg consumer "$consumer" --arg id "$id" --arg started "$start" --arg ended "$end" \
    --arg latency "$latency" --argjson status "${status:-0}" --arg provider "$provider" --arg gateway "$gateway" \
    '{scenario:$scenario,consumer:$consumer,request_id:$id,started_at:$started,ended_at:$ended,latency_ms:($latency|tonumber),http_status:$status,inference_provider:$provider,provider_gateway:$gateway,record:($body.record//null)}' \
    --argjson body "$(jq -c '. // {}' <<<"$body" 2>/dev/null || echo '{}')" >>"$E/requests.jsonl"
}

traffic_loop() {
  local n=0
  while :; do
    n=$((n+1)); request_one "$([[ $((n%2)) == 0 ]] && echo b || echo a)" &
    sleep 0.5
    wait || true
  done
}

sequential_20() {
  local n
  for n in {1..20}; do request_one "$([[ $((n%2)) == 0 ]] && echo b || echo a)"; done
}

cycle() {
  local keep="$1" provider
  SCENARIO="cycle-${keep}"
  log "starting $SCENARIO; survivor=$keep"
  restore_all
  snapshot cycle-start
  traffic_loop & TRAFFIC_PID=$!
  sleep 5
  for provider in "${LOCAL[@]}"; do
    [[ "$provider" == "$keep" ]] && continue
    log "disabling $provider; traffic remains at 2 requests/second"
    set_disabled "$provider" true
    snapshot "disabled-$provider"
    sleep 30
  done
  log "collecting 20 sequential requests with only $keep enabled"
  snapshot one-provider-start
  sequential_20
  snapshot one-provider-end
  for provider in "${LOCAL[@]}"; do
    [[ "$provider" == "$keep" ]] && continue
    log "restoring $provider while traffic continues"
    set_disabled "$provider" false
    snapshot "restored-$provider"
    sleep 10
  done
  kill "$TRAFFIC_PID" 2>/dev/null || true; wait "$TRAFFIC_PID" 2>/dev/null || true; TRAFFIC_PID=""
  snapshot cycle-end
}

cleanup() {
  set +e
  if [[ -n "$TRAFFIC_PID" ]]; then kill "$TRAFFIC_PID" 2>/dev/null || true; fi
  restore_all >/dev/null 2>&1 || true
  snapshot cleanup-final || true
  kc logs deploy/consumer-east --since=30m 2>/dev/null >"$E/raw/consumer-east.log" || true
  kc logs deploy/consumer-west --since=30m 2>/dev/null >"$E/raw/consumer-west.log" || true
  log "cleanup complete; evidence=$E"
}
trap cleanup EXIT INT TERM

main() {
  printf '{"started_at":"%s","rate_per_second":2,"cycles":["llm-d-east-1","llm-d-west-1","llm-d-east-2"],"retries":false}\n' "$(date -u +%FT%TZ)" >"$E/metadata.json"
  restore_all
  for provider in "${LOCAL[@]}"; do
    [[ "$(kc get deploy "$provider" -o jsonpath='{.spec.replicas}')" == 1 && "$(kc get deploy "$provider" -o jsonpath='{.status.readyReplicas}')" == 1 ]] || { log "preflight failed: $provider"; exit 1; }
  done
  snapshot preflight
  cycle llm-d-east-1
  cycle llm-d-west-1
  cycle llm-d-east-2
}
main "$@"
