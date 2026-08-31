#!/usr/bin/env bash
set -euo pipefail

# Provider-boundary smoke test for the optional Azure overflow gateway.
# This is not the qualifying Grid-to-consumer test. It validates one tiny,
# authenticated provider hop using the live route map and mTLS Secret.

: "${KUBECONFIG:?set KUBECONFIG to the target cluster}"
kube_bin="${KUBE_BIN:-kubectl}"
namespace="${GRID_NAMESPACE:-grid-system}"
region="${AZURE_REGION:-east}"
configmap="${AZURE_CONFIGMAP:-azure-${region}-config}"
provider_service="${AZURE_PROVIDER_SERVICE:-azure-${region}.${namespace}.svc.cluster.local}"
server_name="${AZURE_SERVER_NAME:-provider-${region}.${namespace}.svc}"
curl_image="${CURL_IMAGE:-curlimages/curl:8.10.1}"
probe_pod="${AZURE_PROBE_POD:-azure-provider-boundary-smoke}"

case "$region" in east|west) ;; *) echo "AZURE_REGION must be east or west" >&2; exit 2 ;; esac

config="$($kube_bin -n "$namespace" get configmap "$configmap" -o jsonpath='{.data.praxis\.yaml}')"
candidate_id="$(printf '%s\n' "$config" | sed -nE 's/^[[:space:]]*-[[:space:]]*candidate_id:[[:space:]]*([^[:space:]]+).*$/\1/p' | head -n 1)"
route_model="$(printf '%s\n' "$config" | sed -nE 's/^[[:space:]]*model:[[:space:]]*([^[:space:]]+).*$/\1/p' | head -n 1)"
route_revision="$(printf '%s\n' "$config" | sed -nE 's/^[[:space:]]*overlay_revision:[[:space:]]*([^[:space:]]+).*$/\1/p' | head -n 1)"
if [[ -z "$candidate_id" || -z "$route_model" ]]; then
  echo "provider route in $configmap has no candidate_id/model" >&2; exit 1
fi

request_id="azure-boundary-$(date -u +%Y%m%dT%H%M%SZ)-$$"
revision_header=''
if [[ -n "$route_revision" ]]; then
  revision_header="x-ai-routing-revision: $route_revision"
fi

read -r -d '' probe_command <<'PROBE' || true
set -eu
headers=$(mktemp); body=$(mktemp)
trap 'rm "$headers" "$body"' EXIT
set --
if [ -n "${REVISION_HEADER}" ]; then set -- -H "${REVISION_HEADER}"; fi
status=$(curl --fail-with-body -sS --connect-timeout 10 --max-time 45 \
  --cacert /tls/ca.crt --cert /tls/tls.crt --key /tls/tls.key \
  --connect-to "${SERVER_NAME}:8443:${PROVIDER_SERVICE}:8443" \
  -H "Content-Type: application/json" -H "X-Model: ${ROUTE_MODEL}" \
  -H "x-ai-routing-candidate: ${CANDIDATE_ID}" \
  -H "x-ai-routing-request-id: ${REQUEST_ID}" "$@" \
  -X POST "https://${SERVER_NAME}:8443/v1/chat/completions" \
  --data "{\"model\":\"${ROUTE_MODEL}\",\"messages\":[{\"role\":\"user\",\"content\":\"Say hi\"}],\"max_tokens\":1}" \
  -D "$headers" -o "$body" -w "%{http_code}")
printf "http_status=%s\\n" "$status"
gateway=$(awk "tolower(\$1)==\"x-ai-demo-provider-gateway:\" {print \$2}" "$headers" | tr -d "\\r")
backend=$(awk "tolower(\$1)==\"x-ai-inference-provider:\" {print \$2}" "$headers" | tr -d "\\r")
printf "x-ai-demo-provider-gateway=%s\\n" "$gateway"
printf "x-ai-inference-provider=%s\\n" "$backend"
if [ "$status" != 200 ] || [ "$gateway" != "$EXPECTED_GATEWAY" ] || [ "$backend" != "azure-upstream" ]; then
  echo "unexpected Azure provider-boundary result" >&2; exit 1
fi
PROBE

overrides="$(jq -n --arg image "$curl_image" --arg command "$probe_command" \
  '{spec:{restartPolicy:"Never",containers:[{name:"probe",image:$image,command:["sh","-ceu",$command],env:[
    {name:"CANDIDATE_ID",value:""},{name:"ROUTE_MODEL",value:""},{name:"ROUTE_REVISION",value:""},
    {name:"REQUEST_ID",value:""},{name:"SERVER_NAME",value:""},{name:"PROVIDER_SERVICE",value:""},
    {name:"EXPECTED_GATEWAY",value:""},{name:"REVISION_HEADER",value:""}],
    volumeMounts:[{name:"gateway-tls",mountPath:"/tls",readOnly:true}]}],
    volumes:[{name:"gateway-tls",secret:{secretName:"gateway-tls"}}]}}' | jq \
  --arg candidate "$candidate_id" --arg model "$route_model" --arg revision "$route_revision" \
  --arg request "$request_id" --arg server "$server_name" --arg service "$provider_service" \
  --arg expected "azure-${region}" --arg revision_header "$revision_header" \
  '.spec.containers[0].env[0].value=$candidate | .spec.containers[0].env[1].value=$model |
   .spec.containers[0].env[2].value=$revision | .spec.containers[0].env[3].value=$request |
   .spec.containers[0].env[4].value=$server | .spec.containers[0].env[5].value=$service |
   .spec.containers[0].env[6].value=$expected | .spec.containers[0].env[7].value=$revision_header')"

echo "validating Azure provider boundary: region=$region configmap=$configmap candidate=$candidate_id"
if [[ -n "$route_revision" ]]; then echo "sealed route revision will be forwarded"; else echo "no sealed route revision configured"; fi
cleanup() { "$kube_bin" -n "$namespace" delete pod "$probe_pod" --ignore-not-found >/dev/null 2>&1 || true; }
trap cleanup EXIT
"$kube_bin" -n "$namespace" delete pod "$probe_pod" --ignore-not-found >/dev/null 2>&1 || true
"$kube_bin" -n "$namespace" run "$probe_pod" --image="$curl_image" --restart=Never --rm -i --quiet --overrides="$overrides"
