#!/usr/bin/env bash
# Deploys the token-rate-limit-per-app-budgets demo to a kind cluster.
#
# Prereqs:
#   - A kind cluster already exists (KIND_EXPERIMENTAL_PROVIDER=podman kind
#     create cluster --name trl-demo) and images are loaded into it -- see
#     ../README.md's "Run on Kubernetes (kind)" section.
#   - kubectl context points at that cluster.
set -euo pipefail
here="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
demo_root="$(dirname "$here")"

kubectl apply -f "$here/00-namespace-rbac.yaml"

kubectl -n trl-demo create configmap gateway-config \
  --from-file=config.yaml="$demo_root/config.yaml" \
  --dry-run=client -o yaml | kubectl apply -f -

kubectl -n trl-demo create configmap dashboard-assets \
  --from-file=index.html="$demo_root/dashboard/index.html" \
  --from-file=nginx.conf="$demo_root/dashboard/nginx.conf" \
  --dry-run=client -o yaml | kubectl apply -f -

kubectl apply -f "$here/01-valkey.yaml"
kubectl apply -f "$here/02-backend.yaml"
kubectl apply -f "$here/03-gateways.yaml"
kubectl apply -f "$here/04-apps.yaml"
kubectl apply -f "$here/05-dashboard.yaml"

kubectl -n trl-demo rollout status deployment/valkey --timeout=60s
kubectl -n trl-demo rollout status deployment/backend --timeout=60s
kubectl -n trl-demo rollout status deployment/gateway-a --timeout=60s
kubectl -n trl-demo rollout status deployment/gateway-b --timeout=60s
kubectl -n trl-demo rollout status deployment/app-a --timeout=60s
kubectl -n trl-demo rollout status deployment/app-b --timeout=60s
kubectl -n trl-demo rollout status deployment/app-c --timeout=60s
kubectl -n trl-demo rollout status deployment/dashboard --timeout=60s

echo ""
echo "All pods ready:"
kubectl -n trl-demo get pods -o wide
