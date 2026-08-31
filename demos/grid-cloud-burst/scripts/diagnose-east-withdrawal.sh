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
E="$EVIDENCE_ROOT/east-withdrawal-$STAMP"
mkdir -p "$E/raw"
SCENARIO=focused-east-withdrawal
PROVIDER=llm-d-east-1
TRAFFIC_PID=""

log() { printf '[%s] %s\n' "$(date -u +%FT%T.%3NZ)" "$*" | tee -a "$E/runner.log" >&2; }
kc() { kubectl --kubeconfig "$K" -n "$NS" "$@"; }
ui() { curl -ksS --connect-timeout 5 --max-time 20 -u "$USER:$PASS" "$@"; }

cleanup() {
  set +e
  if [[ -n "$TRAFFIC_PID" ]]; then kill "$TRAFFIC_PID" 2>/dev/null || true; fi
  ui -X POST -H 'content-type: application/json' -d "{\"provider\":\"$PROVIDER\",\"disabled\":false}" "$UI/api/v1/cloud-burst/provider" >"$E/raw/restore-provider.json" 2>&1 || true
  for _ in {1..90}; do
    ready="$(kc get deploy "$PROVIDER" -o jsonpath='{.status.readyReplicas}' 2>/dev/null || true)"
    [[ "$ready" == 1 ]] && break
    sleep 1
  done
  capture_state cleanup-final || true
  kc logs deploy/consumer-east --since=20m 2>/dev/null | sed -E 's/(Authorization:|authorization:|api[_-]?key[=:])[^[:space:]]+/\1 [REDACTED]/Ig' >"$E/raw/consumer-east.log" || true
  log "cleanup complete; evidence=$E"
}
trap cleanup EXIT INT TERM

capture_state() {
  local label="$1" now desired ready endpoints overlay phase
  now="$(date -u +%FT%T.%3NZ)"
  desired="$(kc get deploy "$PROVIDER" -o jsonpath='{.spec.replicas}' 2>/dev/null || true)"
  ready="$(kc get deploy "$PROVIDER" -o jsonpath='{.status.readyReplicas}' 2>/dev/null || true)"
  endpoints="$(kc get endpoints "$PROVIDER" -o json 2>/dev/null | jq -c '[.subsets[]?.addresses[]?.ip] // []' || echo '[]')"
  overlay="$(kc get cm grid-overlay-grid-cloud-burst-rhoai-consumer-east -o json 2>/dev/null | jq -c '.data["routing-overlay.json"] | fromjson | {revision:.revision, candidates:(.overlay.candidates|map({cluster,stable_id,selection_group,admission_state}))}' 2>/dev/null || echo '{"revision":null,"candidates":[]}' )"
  phase="$(kc get gridnetwork grid-cloud-burst-rhoai -o jsonpath='{.status.phase}' 2>/dev/null || true)"
  jq -nc --arg at "$now" --arg scenario "$SCENARIO" --arg label "$label" --arg desired "$desired" --arg ready "$ready" --arg phase "$phase" --argjson endpoints "$endpoints" --argjson overlay "$overlay" \
    '{at:$at,scenario:$scenario,label:$label,deployment:{desired:($desired|tonumber?),ready:($ready|tonumber?)},endpoints:$endpoints,overlay:$overlay,gridnetwork_phase:$phase}' >>"$E/state-samples.jsonl"
}

traffic_loop() {
  local seq=0 body status rid at
  while :; do
    seq=$((seq+1)); rid="east-withdraw-${STAMP}-${seq}"
    at="$(date -u +%FT%T.%3NZ)"
    body="$(ui -X POST -H 'content-type: application/json' -H "X-Request-ID: $rid" -d '{"consumer":"a","app":"app1"}' "$UI/api/v1/token-rate-limit/requests" 2>"$E/raw/request-${seq}.err" || true)"
    status="$(jq -r '.record.http.status // .record.status // 0' <<<"$body" 2>/dev/null || echo 0)"
    jq -nc --arg at "$at" --arg id "$rid" --argjson status "${status:-0}" --argjson body "$(jq -c '.record // null' <<<"$body" 2>/dev/null || echo null)" \
      '{at:$at,request_id:$id,http_status:$status,record:$body}' >>"$E/requests.jsonl"
    sleep 0.5
  done
}

main() {
  printf '{"started_at":"%s","scenario":"%s","rate_per_second":2,"provider":"%s","no_retries":true}\n' "$(date -u +%FT%TZ)" "$SCENARIO" "$PROVIDER" >"$E/metadata.json"
  capture_state before
  [[ "$(kc get deploy "$PROVIDER" -o jsonpath='{.spec.replicas}')" == 1 ]] || { log "precondition failed: desired replicas"; exit 1; }
  [[ "$(kc get deploy "$PROVIDER" -o jsonpath='{.status.readyReplicas}')" == 1 ]] || { log "precondition failed: ready replicas"; exit 1; }
  traffic_loop & TRAFFIC_PID=$!
  sleep 5
  log "disabling $PROVIDER while traffic continues"
  ui -X POST -H 'content-type: application/json' -d "{\"provider\":\"$PROVIDER\",\"disabled\":true}" "$UI/api/v1/cloud-burst/provider" >"$E/raw/disable-provider.json"
  old_revision="$(jq -r '.overlay.revision.value // empty' "$E/state-samples.jsonl" | head -1)"
  withdrawal_revision=""
  stable=0
  while :; do
    capture_state polling
    latest="$(tail -1 "$E/state-samples.jsonl")"
    current_revision="$(jq -r '.overlay.revision.value // empty' <<<"$latest")"
    candidate_present="$(jq -r --arg p "$PROVIDER" '[.overlay.candidates[]|select(.cluster==$p)]|length' <<<"$latest")"
    if [[ -n "$current_revision" && "$current_revision" != "$old_revision" && "$candidate_present" == 0 ]]; then
      [[ -n "$withdrawal_revision" ]] || { withdrawal_revision="$current_revision"; log "withdrawal revision observed: $withdrawal_revision"; }
      stable=$((stable+1))
    else
      stable=0
    fi
    [[ "$stable" -ge 30 ]] && break
    sleep 1
  done
  jq --arg revision "$withdrawal_revision" '. + {withdrawal_revision:$revision}' "$E/metadata.json" >"$E/metadata.tmp" && mv "$E/metadata.tmp" "$E/metadata.json"
  log "withdrawal revision served for 30 consecutive state samples"
}
main "$@"
