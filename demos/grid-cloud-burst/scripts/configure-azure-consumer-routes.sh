#!/usr/bin/env bash
set -euo pipefail

# Add the optional Azure provider gateways to the consumer's authenticated
# provider-hop contract. It is a validation bridge for the current installation;
# the normal install should render the same entries declaratively from Helm or
# the demo's gateway ConfigMap templates. It preserves local/OpenAI routes.

: "${KUBECONFIG:?set KUBECONFIG to the target cluster}"
kube_bin="${KUBE_BIN:-kubectl}"
namespace="${GRID_NAMESPACE:-grid-system}"
apply="${APPLY_AZURE_CONSUMER_ROUTES:-0}"

for configmap in consumer-east-config consumer-west-config; do
  current="$($kube_bin -n "$namespace" get configmap "$configmap" -o jsonpath='{.data.praxis\.yaml}')"
  updated="$(printf '%s\n' "$current" | yq -oy '
    (.filter_chains[].filters[] | select(.filter == "intelligent_route").provider_hop_clusters) |=
      (((. // []) + ["azure-east", "azure-west"]) | unique)
    | (.filter_chains[].filters[] | select(.filter == "load_balancer").clusters) |=
      (map(select(.name != "azure-east" and .name != "azure-west")) + [
        {"name": "azure-east", "tls": {"ca": {"ca_path": "/etc/praxis/tls/ca.crt"}, "client_cert": {"cert_path": "/etc/praxis/tls/tls.crt", "key_path": "/etc/praxis/tls/tls.key"}, "sni": "provider-east.grid-system.svc", "verify": true}, "endpoints": ["azure-east.grid-system.svc.cluster.local:8443"]},
        {"name": "azure-west", "tls": {"ca": {"ca_path": "/etc/praxis/tls/ca.crt"}, "client_cert": {"cert_path": "/etc/praxis/tls/tls.crt", "key_path": "/etc/praxis/tls/tls.key"}, "sni": "provider-west.grid-system.svc", "verify": true}, "endpoints": ["azure-west.grid-system.svc.cluster.local:8443"]}
      ])')"

  # Validate the generated configuration before touching the cluster.
  normalized_after="$(printf '%s\n' "$updated" | yq -o=json | jq -S '.')"
  local_openai_before="$(printf '%s\n' "$current" | yq -o=json '[.filter_chains[].filters[] | select(.filter == "load_balancer").clusters[] | select(.name | test("^(llm-d|openai)-"))]' | jq -S '.')"
  local_openai_after="$(printf '%s\n' "$updated" | yq -o=json '[.filter_chains[].filters[] | select(.filter == "load_balancer").clusters[] | select(.name | test("^(llm-d|openai)-"))]' | jq -S '.')"
  [[ "$local_openai_before" == "$local_openai_after" ]] || { echo "$configmap: existing local/OpenAI routes changed" >&2; exit 1; }
  printf '%s\n' "$updated" | yq -o=json '.filter_chains[].filters[] | select(.filter == "load_balancer").clusters[] | select(.name == "azure-east") | (.endpoints[0] == "azure-east.grid-system.svc.cluster.local:8443" and .tls.sni == "provider-east.grid-system.svc")' | grep -qx true || { echo "$configmap: azure-east endpoint/SNI validation failed" >&2; exit 1; }
  printf '%s\n' "$updated" | yq -o=json '.filter_chains[].filters[] | select(.filter == "load_balancer").clusters[] | select(.name == "azure-west") | (.endpoints[0] == "azure-west.grid-system.svc.cluster.local:8443" and .tls.sni == "provider-west.grid-system.svc")' | grep -qx true || { echo "$configmap: azure-west endpoint/SNI validation failed" >&2; exit 1; }
  repeated="$(printf '%s\n' "$updated" | yq -oy '
    (.filter_chains[].filters[] | select(.filter == "intelligent_route").provider_hop_clusters) |= (((. // []) + ["azure-east", "azure-west"]) | unique)
    | (.filter_chains[].filters[] | select(.filter == "load_balancer").clusters) |= (map(select(.name != "azure-east" and .name != "azure-west")) + [
      {"name": "azure-east", "tls": {"ca": {"ca_path": "/etc/praxis/tls/ca.crt"}, "client_cert": {"cert_path": "/etc/praxis/tls/tls.crt", "key_path": "/etc/praxis/tls/tls.key"}, "sni": "provider-east.grid-system.svc", "verify": true}, "endpoints": ["azure-east.grid-system.svc.cluster.local:8443"]},
      {"name": "azure-west", "tls": {"ca": {"ca_path": "/etc/praxis/tls/ca.crt"}, "client_cert": {"cert_path": "/etc/praxis/tls/tls.crt", "key_path": "/etc/praxis/tls/tls.key"}, "sni": "provider-west.grid-system.svc", "verify": true}, "endpoints": ["azure-west.grid-system.svc.cluster.local:8443"]}
    ])' | yq -o=json | jq -S '.')"
  [[ "$normalized_after" == "$repeated" ]] || { echo "$configmap: transformation is not idempotent" >&2; exit 1; }
  patch="$(jq -n --arg value "$updated" '{data:{"praxis.yaml":$value}}')"
  if [[ "$apply" == 1 ]]; then
    "$kube_bin" -n "$namespace" patch configmap "$configmap" --type merge -p "$patch" >/dev/null
    echo "configured $configmap for Azure provider hops"
  else
    echo "validated $configmap; dry-run only (set APPLY_AZURE_CONSUMER_ROUTES=1 to apply)"
  fi
done
