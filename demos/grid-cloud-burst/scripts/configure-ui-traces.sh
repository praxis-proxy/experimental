#!/usr/bin/env bash
set -euo pipefail

: "${KUBECONFIG:?set KUBECONFIG to the target cluster}"
ui_namespace="${UI_NAMESPACE:-praxis-tracing-cloud-burst}"
deployment="${UI_DEPLOYMENT:-praxis-tracing-cloud-burst-ui}"
jaeger_url="${JAEGER_URL:-http://jaeger-query.grid-system.svc.cluster.local:16686}"

kubectl -n "$ui_namespace" set env "deployment/$deployment" \
  JAEGER_URL="$jaeger_url" \
  JAEGER_UI_URL="${JAEGER_UI_URL:-$jaeger_url}"
kubectl -n "$ui_namespace" rollout status "deployment/$deployment" --timeout=180s
