#!/usr/bin/env bash
set -Eeuo pipefail

# Live cloud-burst qualification runner. This is intentionally a diagnostic
# script: it changes only the simulator replicas/metrics through the existing
# demo controls and preserves every observed result. It never calls Grid
# reconciliation and never retries a request.

ROOT="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
KUBECONFIG_PATH="${KUBECONFIG:-/tmp/rhoai-sim-validation.kubeconfig}"
NS="${GRID_NAMESPACE:-grid-system}"
UI_NS="${UI_NAMESPACE:-praxis-tracing-cloud-burst}"
UI_URL="${UI_URL:?set UI_URL to the deployed cloud-burst UI URL}"
UI_USER="${UI_USER:?set UI_USER to the deployed UI username}"
UI_PASSWORD="${UI_PASSWORD:?set UI_PASSWORD to the deployed UI password}"
NETWORK="${GRID_NETWORK:-grid-cloud-burst-rhoai}"
OVERLAY_CM="${GRID_OVERLAY_CONFIGMAP:-grid-overlay-grid-cloud-burst-rhoai-consumer-east}"
RATE="${RATE_PER_SECOND:-2}"
WINDOW="${SCENARIO_SECONDS:-10}"
REQUEST_TIMEOUT="${REQUEST_TIMEOUT_SECONDS:-20}"
EVIDENCE_ROOT="${EVIDENCE_ROOT:-$ROOT/evidence}"
STAMP="$(date -u +%Y%m%dT%H%M%SZ)"
EVIDENCE="$EVIDENCE_ROOT/qualification-$STAMP"
RAW="$EVIDENCE/raw"
mkdir -p "$RAW"

LOCAL_PROVIDERS=(llm-d-east-1 llm-d-east-2 llm-d-west-1 llm-d-west-2)
declare -A PROVIDER_TO_SIM=(
  [llm-d-east-1]=llm-d-east-1
  [llm-d-east-2]=llm-d-east-2
  [llm-d-west-1]=llm-d-west-1
  [llm-d-west-2]=llm-d-west-2
)
SCENARIO="preflight"
SAMPLER_PID=""
TRAFFIC_PID=""

log() { printf '[%s] %s\n' "$(date -u +%FT%TZ)" "$*" | tee -a "$EVIDENCE/runner.log" >&2; }
kc() { kubectl --kubeconfig "$KUBECONFIG_PATH" -n "$NS" "$@"; }
ui_curl() { curl -ksS --connect-timeout 5 --max-time "$REQUEST_TIMEOUT" -u "$UI_USER:$UI_PASSWORD" "$@"; }

sanitize() {
  sed -E 's/(Authorization:|authorization:|X-Api-Key:|x-api-key:|api[_-]?key[=:])[^[:space:]]+/\1 [REDACTED]/Ig; s/(password|secret|token)[=:][^,;[:space:]]+/\1=[REDACTED]/Ig'
}

cleanup() {
  set +e
  for child in $(jobs -pr); do kill "$child" 2>/dev/null || true; done
  if [[ -n "$TRAFFIC_PID" ]]; then kill "$TRAFFIC_PID" 2>/dev/null || true; fi
  if [[ -n "$SAMPLER_PID" ]]; then kill "$SAMPLER_PID" 2>/dev/null || true; fi
  log "cleanup: restoring simulator metrics and replicas"
  ui_curl -X POST -H 'content-type: application/json' \
    -d '{"on":false,"mode":"sim"}' "$UI_URL/api/v1/cloud-burst/load" \
    >"$RAW/cleanup-pressure.json" 2>&1 || true
  for provider in "${LOCAL_PROVIDERS[@]}"; do
    ui_curl -X POST -H 'content-type: application/json' \
      -d "{\"provider\":\"$provider\",\"disabled\":false}" \
      "$UI_URL/api/v1/cloud-burst/provider" >"$RAW/cleanup-$provider.json" 2>&1 || true
  done
  log "cleanup: waiting for simulator deployments to report one ready replica"
  for _ in {1..60}; do
    ready=0
    for provider in "${LOCAL_PROVIDERS[@]}"; do
      sim="${PROVIDER_TO_SIM[$provider]}"
      replicas="$(kc get deploy "$sim" -o jsonpath='{.status.readyReplicas}' 2>/dev/null || true)"
      [[ "$replicas" == 1 ]] && ready=$((ready + 1))
    done
    [[ "$ready" == 4 ]] && break
    sleep 1
  done
  capture_logs
  rmdir "$EVIDENCE/state-samples.lock.d" "$EVIDENCE/requests.lock.d" 2>/dev/null || true
  sample_state "cleanup-final" || true
  log "cleanup complete; evidence=$EVIDENCE"
}
trap cleanup EXIT INT TERM

sample_state() {
  local label="$1" now overlay_json grid_json
  local lockdir="$EVIDENCE/state-samples.lock.d"
  while ! mkdir "$lockdir" 2>/dev/null; do sleep 0.01; done
  now="$(date -u +%FT%TZ)"
  overlay_json="$(kc get configmap "$OVERLAY_CM" -o json 2>/dev/null | jq -c '
    .data["routing-overlay.json"] // .data["routing-config.json"] // "" | fromjson? |
    {revision:(.revision // null), candidates:((.overlay.candidates // .candidates // []) |
      map({name:(.name // .model // null),site:(.site // null),cluster:(.cluster // null),group:(.selection_group // .group // null),admission:(.admission_state // .admission // null),tier:(.selection_tier // .tier // null),weight:(.traffic_weight // .weight // null),backend_kind:(.backend_kind // null)}))}' 2>/dev/null || echo '{"revision":null,"candidates":[]}' )"
  grid_json="$(kc get gridnetwork "$NETWORK" -o json 2>/dev/null | jq -c '{phase:(.status.phase // null),conditions:(.status.conditions // [] | map({type,reason,status,message})),generation:.metadata.generation}' 2>/dev/null || echo '{"phase":null,"conditions":[]}' )"
  {
    printf '{"at":"%s","scenario":"%s","label":"%s","deployments":{' "$now" "$SCENARIO" "$label"
    local first=1 provider sim desired ready
    for provider in "${LOCAL_PROVIDERS[@]}"; do
      sim="${PROVIDER_TO_SIM[$provider]}"
      desired="$(kc get deploy "$sim" -o jsonpath='{.spec.replicas}' 2>/dev/null || true)"
      ready="$(kc get deploy "$sim" -o jsonpath='{.status.readyReplicas}' 2>/dev/null || true)"
      [[ $first == 0 ]] && printf ','; first=0
      printf '%q' "$provider" >/dev/null
      printf '"%s":{"desired":%s,"ready":%s}' "$provider" "${desired:-null}" "${ready:-null}"
    done
    printf '},"services":{'
    first=1
    for provider in "${LOCAL_PROVIDERS[@]}"; do
      sim="${PROVIDER_TO_SIM[$provider]}"
      local endpoints
      endpoints="$(kc get endpoints "$sim" -o json 2>/dev/null | jq -c '[.subsets[]?.addresses[]?.ip] // []' 2>/dev/null || echo '[]')"
      [[ $first == 0 ]] && printf ','; first=0
      printf '"%s":%s' "$provider" "$endpoints"
    done
    printf '},"overlay":%s,"gridnetwork":%s}\n' "$overlay_json" "$grid_json"
  } >>"$EVIDENCE/state-samples.jsonl"
  rmdir "$lockdir"
}

sample_loop() {
  while :; do sample_state "periodic"; sleep 1; done
}

start_sampling() {
  sample_loop & SAMPLER_PID=$!
}

capture_provenance() {
  git -C "$ROOT/../.." rev-parse HEAD >"$RAW/experimental-head.txt" 2>/dev/null || true
  git -C "$ROOT/../.." status --short >"$RAW/experimental-status.txt" 2>/dev/null || true
  kc get deploy -o json | jq '[.items[] | {name:.metadata.name, images:[.spec.template.spec.containers[]?.image]}]' \
    >"$RAW/deployment-images.json" 2>/dev/null || true
  kc get gridnetwork "$NETWORK" -o json | jq '{metadata:{name:.metadata.name,generation:.metadata.generation},status:.status}' \
    >"$RAW/gridnetwork-provenance.json" 2>/dev/null || true
  kc get configmap "$OVERLAY_CM" -o json | jq 'del(.metadata.managedFields,.metadata.annotations)' \
    >"$RAW/overlay-configmap.json" 2>/dev/null || true
}

capture_logs() {
  local workload
  for workload in consumer-east consumer-west provider-east provider-west azure-east azure-west; do
    kc logs "deploy/$workload" --since=15m 2>/dev/null | sanitize >"$RAW/logs-$workload.txt" || true
  done
  kc get events --sort-by='.lastTimestamp' -o json 2>/dev/null | jq '[.items[] | {lastTimestamp:.lastTimestamp,reason:.reason,type:.type,involvedObject:.involvedObject.name,message:.message}]' \
    >"$RAW/events.json" || true
}

record_response() {
  local scenario="$1" consumer="$2" app="$3" request_id="$4" header_file="$5" body_file="$6" start="$7" end="$8" http="$9"
  local lockdir="$EVIDENCE/requests.lock.d"
  while ! mkdir "$lockdir" 2>/dev/null; do sleep 0.01; done
  local headers_json='{}' response_json='null' record_json='null' actual_status='null'
  if [[ -f "$header_file" ]]; then
    headers_json="$(awk 'BEGIN{IGNORECASE=1} /^[[:space:]]*[^:]+:[[:space:]]/ { sub(/^[^:]+:[[:space:]]*/, ""); key=$0; sub(/:.*/, "", key); value=$0; sub(/^[^:]+:[[:space:]]*/, "", value); printf "%s\t%s\n", tolower(key), value }' "$header_file" | jq -Rn '[inputs | split("\t") | { (.[0]): .[1] }] | add // {}' 2>/dev/null || echo '{}')"
  fi
  response_json="$(jq -c . "$body_file" 2>/dev/null || echo 'null')"
  record_json="$(jq -c '.record // null' <<<"$response_json" 2>/dev/null || echo 'null')"
  actual_status="$(jq -r '.record.http.status // .record.status // .record.http_status // empty' <<<"$response_json" 2>/dev/null || true)"
  [[ "$actual_status" =~ ^[0-9]+$ ]] || actual_status=null
  jq -nc --arg scenario "$scenario" --arg consumer "$consumer" --arg app "$app" \
    --arg request_id "$request_id" --arg started "$start" --arg ended "$end" \
    --arg wrapper_status "$http" --argjson actual_status "$actual_status" \
    --argjson headers "$headers_json" --argjson record "$record_json" \
    --arg body "$(sanitize <"$body_file" | head -c 4000)" \
    '{scenario:$scenario,consumer:$consumer,application:$app,request_id:$request_id,started_at:$started,ended_at:$ended,wrapper_http_status:($wrapper_status|tonumber?),http_status:$actual_status,headers:$headers,record:$record,body:$body}' \
    | { cat >>"$EVIDENCE/requests.jsonl"; rmdir "$lockdir"; }
}

send_request() {
  local consumer="$1" app="$2" scenario="${3:-$SCENARIO}" request_id start end header_file body_file http unique
  unique="${BASHPID}-$(date -u +%s%N)"
  request_id="cbq-${STAMP}-${unique}-${consumer}-${app}"
  header_file="$RAW/response-${unique}.headers"
  body_file="$RAW/response-${unique}.body"
  start="$(date -u +%FT%T.%3NZ)"
  set +e
  http="$(ui_curl -D "$header_file" -o "$body_file" -w '%{http_code}' \
    -X POST -H 'content-type: application/json' -H "X-Request-ID: $request_id" \
    -d "{\"consumer\":\"$consumer\",\"app\":\"$app\"}" \
    "$UI_URL/api/v1/token-rate-limit/requests" 2>"$RAW/response-${unique}.curlerr")"
  local curl_rc=$?
  set -e
  [[ "$curl_rc" == 0 && "$http" =~ ^[0-9]{3}$ ]] || http=000
  end="$(date -u +%FT%T.%3NZ)"
  record_response "$scenario" "$consumer" "$app" "$request_id" "$header_file" "$body_file" "$start" "$end" "$http"
}

traffic_for() {
  local consumer="$1" seconds="${2:-$WINDOW}" deadline pid
  local -a request_pids=()
  deadline=$((SECONDS + seconds))
  while (( SECONDS < deadline )); do
    send_request "$consumer" "app1" "$SCENARIO" &
    request_pids+=("$!")
    sleep "$(awk "BEGIN {print 1/$RATE}")"
  done
  for pid in "${request_pids[@]}"; do wait "$pid" || true; done
}

set_provider_disabled() {
  local provider="$1" disabled="$2"
  ui_curl -X POST -H 'content-type: application/json' \
    -d "{\"provider\":\"$provider\",\"disabled\":$disabled}" \
    "$UI_URL/api/v1/cloud-burst/provider" | sanitize >"$RAW/${SCENARIO}-${provider}-${disabled}.json"
}

set_pressure() {
  local on="$1"
  ui_curl -X POST -H 'content-type: application/json' \
    -d "{\"on\":$on,\"mode\":\"sim\"}" "$UI_URL/api/v1/cloud-burst/load" \
    | sanitize >"$RAW/${SCENARIO}-pressure-${on}.json"
}

wait_local_ready() {
  local expected=4 ready sim provider
  for _ in {1..90}; do
    ready=0
    for provider in "${LOCAL_PROVIDERS[@]}"; do
      sim="${PROVIDER_TO_SIM[$provider]}"
      [[ "$(kc get deploy "$sim" -o jsonpath='{.status.readyReplicas}' 2>/dev/null || true)" == 1 ]] && ready=$((ready + 1))
    done
    [[ "$ready" == "$expected" ]] && return 0
    sleep 1
  done
  log "WARN local readiness timeout: ready=$ready expected=$expected"
  return 1
}

restore_all_local() {
  local provider
  for provider in "${LOCAL_PROVIDERS[@]}"; do
    set_provider_disabled "$provider" false
  done
  wait_local_ready || true
}

preflight() {
  log "preflight: checking UI, four simulators, overlay, consumers, and baseline"
  local status
  status="$(ui_curl -o "$RAW/preflight-status.json" -w '%{http_code}' "$UI_URL/api/status")"
  [[ "$status" == 200 ]] || { log "FAIL preflight UI status HTTP $status"; return 1; }
  ui_curl "$UI_URL/api/v1/cloud-burst" >"$RAW/preflight-cloud-burst.json"
  ui_curl "$UI_URL/api/v1/token-rate-limit" >"$RAW/preflight-token-status.json"
  for provider in "${LOCAL_PROVIDERS[@]}"; do
    sim="${PROVIDER_TO_SIM[$provider]}"
    [[ "$(kc get deploy "$sim" -o jsonpath='{.spec.replicas}' | tr -d '\n')" == 1 ]] || { log "FAIL $sim desired replicas"; return 1; }
    [[ "$(kc get deploy "$sim" -o jsonpath='{.status.readyReplicas}' | tr -d '\n')" == 1 ]] || { log "FAIL $sim ready replicas"; return 1; }
  done
  sample_state "preflight"
  traffic_for a 10
  traffic_for b 10
  local failures
  failures="$(jq -s '[.[] | select(.scenario=="preflight" and ((.http_status // 0) < 200 or (.http_status // 0) >= 300))] | length' "$EVIDENCE/requests.jsonl")"
  [[ "$failures" == 0 ]] || { log "FAIL preflight baseline request failures=$failures"; return 1; }
  log "PASS preflight"
}

write_summary() {
  jq -s 'group_by(.scenario) | map({scenario:.[0].scenario,requests:length,wrapper_statuses:(map(.wrapper_http_status)|group_by(.)|map({status:.[0],count:length})),http_statuses:(map(.http_status)|group_by(.)|map({status:.[0],count:length})),admissions:(map(.record.admission // null)|group_by(.)|map({value:.[0],count:length})),providers:(map(.record.inference_provider // null)|group_by(.)|map({value:.[0],count:length})),provider_gateways:(map(.record.route.provider_gateway // null)|group_by(.)|map({value:.[0],count:length})),five_xx:(map(select((.http_status // 0)>=500 and (.http_status // 0)<600))|length),transition_502s:(map(select(.http_status==502))|length)})' \
    "$EVIDENCE/requests.jsonl" >"$EVIDENCE/scenario-summary.json"
  jq -s '[.[] | select((.http_status // 0) >= 500 and (.http_status // 0) < 600)]' \
    "$EVIDENCE/requests.jsonl" >"$EVIDENCE/failures.json"
  jq -s 'map(select(.http_status==502)) | length' "$EVIDENCE/requests.jsonl" >"$EVIDENCE/transition-502-count.txt"
  cp "$EVIDENCE/state-samples.jsonl" "$EVIDENCE/state-timeline.jsonl"
  jq -R -s 'split("\n") | map(select(length>0) | fromjson) | map({at,scenario,label,gridnetwork_phase:.gridnetwork.phase,overlay_revision:.overlay.revision})' \
    "$EVIDENCE/state-samples.jsonl" >"$EVIDENCE/revision-timeline.json" 2>/dev/null || true
}

run_scenario() {
  local name="$1" action="$2"
  SCENARIO="$name"
  log "scenario start: $name"
  sample_state "scenario-start"
  eval "$action"
  sample_state "scenario-end"
  log "scenario complete: $name"
}

baseline() { traffic_for a "$WINDOW"; traffic_for b "$WINDOW"; }
one_east_down() { set_provider_disabled llm-d-east-1 true; traffic_for a "$WINDOW"; }
restore_east() { set_provider_disabled llm-d-east-1 false; traffic_for a "$WINDOW"; }
both_east_down() { set_provider_disabled llm-d-east-1 true; set_provider_disabled llm-d-east-2 true; traffic_for a "$WINDOW"; }
one_west_down() { set_provider_disabled llm-d-west-1 true; traffic_for b "$WINDOW"; }
restore_west() { set_provider_disabled llm-d-west-1 false; traffic_for b "$WINDOW"; }
both_west_down() { set_provider_disabled llm-d-west-1 true; set_provider_disabled llm-d-west-2 true; traffic_for b "$WINDOW"; }
all_local_down() { for p in "${LOCAL_PROVIDERS[@]}"; do set_provider_disabled "$p" true; done; traffic_for a "$WINDOW"; traffic_for b "$WINDOW"; }
pressure_burst() { restore_all_local; set_pressure true; traffic_for a "$WINDOW"; traffic_for b "$WINDOW"; }
pressure_recovery() { set_pressure false; traffic_for a "$WINDOW"; traffic_for b "$WINDOW"; }
combined_degradation() { restore_all_local; set_provider_disabled llm-d-east-1 true; set_provider_disabled llm-d-west-1 true; set_pressure true; traffic_for a "$WINDOW"; traffic_for b "$WINDOW"; }

main() {
  cat >"$EVIDENCE/metadata.json" <<EOF
{"started_at":"$(date -u +%FT%TZ)","kubeconfig":"$KUBECONFIG_PATH","namespace":"$NS","ui_namespace":"$UI_NS","ui_url":"$UI_URL","network":"$NETWORK","overlay_configmap":"$OVERLAY_CM","rate_per_second":$RATE,"scenario_seconds":$WINDOW,"retries":false}
EOF
  capture_provenance
  start_sampling
  preflight
  run_scenario healthy-baseline baseline
  run_scenario disable-one-east one_east_down
  run_scenario restore-one-east restore_east
  run_scenario disable-both-east both_east_down
  run_scenario restore-east-and-disable-one-west 'restore_east; one_west_down'
  run_scenario restore-one-west restore_west
  run_scenario disable-both-west both_west_down
  run_scenario restore-west restore_west
  run_scenario all-local-unavailable all_local_down
  run_scenario pressure-burst pressure_burst
  run_scenario pressure-recovery pressure_recovery
  run_scenario combined-degradation combined_degradation
  write_summary
  log "all scenarios completed; cleanup will restore the cluster"
}

main "$@"
