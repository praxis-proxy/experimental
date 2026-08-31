#!/usr/bin/env bash
set -Eeuo pipefail

ROOT="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
K="${KUBECONFIG:-/tmp/rhoai-sim-validation.kubeconfig}"
NS=grid-system
UI="${UI_URL:?set UI_URL to the deployed cloud-burst UI URL}"
E="$ROOT/evidence/request-stall-$(date -u +%Y%m%dT%H%M%SZ)"
mkdir -p "$E/raw"
PROVIDER=llm-d-east-1
TRAFFIC_PID=""
STARTED="$(date -u +%FT%TZ)"

kc() { kubectl --kubeconfig "$K" -n "$NS" "$@"; }
: "${UI_USER:?set UI_USER to the tracing UI username}"
: "${UI_PASSWORD:?set UI_PASSWORD to the tracing UI password}"
ui() { curl -ksS --connect-timeout 5 --max-time 35 -u "$UI_USER:$UI_PASSWORD" "$@"; }
log() { printf '[%s] %s\n' "$(date -u +%FT%T.%3NZ)" "$*" | tee -a "$E/runner.log" >&2; }

snapshot() {
  local label="$1" now overlay phase endpoint_json
  now="$(date -u +%FT%T.%3NZ)"
  endpoint_json="$(kc get endpointslice -l kubernetes.io/service-name="$PROVIDER" -o json 2>/dev/null | jq -c '[.items[].endpoints[]?|{addresses,conditions}]' || echo '[]')"
  overlay="$(kc get cm grid-overlay-grid-cloud-burst-rhoai-consumer-east -o json 2>/dev/null | jq -c '.data["routing-overlay.json"]|fromjson|{revision:.revision,candidates:(.overlay.candidates|map({cluster,stable_id,selection_group,admission_state}))}' 2>/dev/null || echo '{"revision":null,"candidates":[]}' )"
  phase="$(kc get gridnetwork grid-cloud-burst-rhoai -o jsonpath='{.status.phase}' 2>/dev/null || true)"
  jq -nc --arg at "$now" --arg label "$label" --arg phase "$phase" --argjson endpoints "$endpoint_json" --argjson overlay "$overlay" \
    '{at:$at,label:$label,endpoints:$endpoints,gridnetwork_phase:$phase,overlay:$overlay}' >>"$E/state.jsonl"
}

traffic() {
  local n=0 id start end body_file timing_file status
  while :; do
    n=$((n+1)); id="stall-${STARTED}-${n}"; start="$(date -u +%FT%T.%3NZ)"
    body_file="$E/raw/response-$n.body"; timing_file="$E/raw/response-$n.timing.json"
    set +e
    ui -o "$body_file" -D "$E/raw/response-$n.headers" -w '%{json}' \
      -X POST -H 'content-type: application/json' -H "X-Request-ID: $id" \
      -d '{"consumer":"a","app":"app2"}' "$UI/api/v1/token-rate-limit/requests" >"$timing_file" 2>"$E/raw/response-$n.curlerr"
    local rc=$?
    set -e
    end="$(date -u +%FT%T.%3NZ)"
    status="$(jq -r '.record.http.status // .record.status // 0' "$body_file" 2>/dev/null || echo 0)"
    jq -nc --arg id "$id" --arg started "$start" --arg ended "$end" --argjson rc "$rc" \
      --argjson timing "$(jq -c '. // {}' "$timing_file" 2>/dev/null || echo '{}')" \
      --argjson status "${status:-0}" --argjson record "$(jq -c '.record // null' "$body_file" 2>/dev/null || echo null)" \
      '{request_id:$id,started_at:$started,ended_at:$ended,curl_exit:$rc,http_status:$status,curl_timing:$timing,record:$record}' >>"$E/requests.jsonl"
    sleep 1
  done
}

cleanup() {
  set +e
  if [[ -n "$TRAFFIC_PID" ]]; then kill "$TRAFFIC_PID" 2>/dev/null || true; fi
  ui -X POST -H 'content-type: application/json' -d "{\"provider\":\"$PROVIDER\",\"disabled\":false}" "$UI/api/v1/cloud-burst/provider" >"$E/raw/restore.json" 2>&1 || true
  for _ in {1..90}; do
    [[ "$(kc get deploy "$PROVIDER" -o jsonpath='{.status.readyReplicas}' 2>/dev/null || true)" == 1 ]] && break
    sleep 1
  done
  snapshot cleanup-final || true
  for workload in consumer-east consumer-west provider-east provider-west; do
    kc logs "deploy/$workload" --since-time="$STARTED" 2>/dev/null | sed -E 's/(Authorization:|authorization:|api[_-]?key[=:])[^[:space:]]+/\1 [REDACTED]/Ig' >"$E/raw/$workload.log" || true
  done
  log "cleanup complete; evidence=$E"
}
trap cleanup EXIT INT TERM

main() {
  printf '{"started_at":"%s","provider":"%s","rate_per_second":1,"retries":false}\n' "$STARTED" "$PROVIDER" >"$E/metadata.json"
  for p in llm-d-east-1 llm-d-east-2 llm-d-west-1 llm-d-west-2; do
    [[ "$(kc get deploy "$p" -o jsonpath='{.spec.replicas}')" == 1 && "$(kc get deploy "$p" -o jsonpath='{.status.readyReplicas}' 2>/dev/null || true)" == 1 ]] || { log "preflight failed: $p"; exit 1; }
  done
  snapshot preflight
  traffic & TRAFFIC_PID=$!
  sleep 5
  log "disabling $PROVIDER while one request per second continues"
  ui -X POST -H 'content-type: application/json' -d "{\"provider\":\"$PROVIDER\",\"disabled\":true}" "$UI/api/v1/cloud-burst/provider" >"$E/raw/disable.json"
  for _ in {1..45}; do snapshot polling; sleep 1; done
}
main "$@"
