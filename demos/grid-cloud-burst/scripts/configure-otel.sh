#!/usr/bin/env bash
set -euo pipefail

# Configure the existing gateway ConfigMaps to export real gateway spans.
#
# SAFETY: require an explicit opt-in because this changes all four gateway
# ConfigMaps and causes a rollout. The current composed image includes the
# Praxis runtime fix and retains its tracing guard for the server lifetime.

: "${KUBECONFIG:?set KUBECONFIG to the target cluster}"
if [[ "${ALLOW_GATEWAY_OTEL_UNTIL_CORE_FIX:-}" != "1" ]]; then
  echo "gateway OTLP export is blocked: rerun with ALLOW_GATEWAY_OTEL_UNTIL_CORE_FIX=1 after verifying the fixed image" >&2
  exit 2
fi
namespace="${GRID_NAMESPACE:-grid-system}"
collector_endpoint="${OTLP_ENDPOINT:-http://jaeger-collector.grid-system.svc.cluster.local:4317}"

for configmap in consumer-east-config consumer-west-config provider-east-config provider-west-config; do
  current=$(kubectl -n "$namespace" get configmap "$configmap" -o jsonpath='{.data.praxis\.yaml}')
  if printf '%s\n' "$current" | grep -q '^telemetry:'; then
    echo "$configmap already has telemetry configuration"
    continue
  fi
  updated=$(printf '%s\ntelemetry:\n  otlp_endpoint: %s\n  sampling_rate: 1.0\n' "$current" "$collector_endpoint")
  patch=$(jq -n --arg value "$updated" '[{"op":"replace","path":"/data/praxis.yaml","value":$value}]')
  kubectl -n "$namespace" patch configmap "$configmap" --type=json -p "$patch" >/dev/null
  echo "configured $configmap"
done
