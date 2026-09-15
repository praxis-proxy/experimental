#!/usr/bin/env bash
set -euo pipefail

GRID_REPO=${GRID_REPO:?Set GRID_REPO to a checked-out Grid repository}
DEMO_DIR=$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)
RUN_ID=${RUN_ID:-weighted-demo-$(date -u +%Y%m%dT%H%M%SZ)}
EVIDENCE_DIR=${EVIDENCE_DIR:-"$DEMO_DIR/evidence/$RUN_ID"}

if [[ ! -f "$GRID_REPO/Cargo.toml" ]]; then
  echo "GRID_REPO is not a Grid checkout: $GRID_REPO" >&2
  exit 2
fi

export GRID_XTASK_IMAGE_PULL_POLICY=${GRID_XTASK_IMAGE_PULL_POLICY:-Never}
export GRID_XTASK_RUN_ID=${GRID_XTASK_RUN_ID:-$RUN_ID}

echo "Grid repository: $GRID_REPO"
echo "Run ID: $RUN_ID"
echo "Evidence: $EVIDENCE_DIR"
echo "Image pull policy: $GRID_XTASK_IMAGE_PULL_POLICY"

cd "$GRID_REPO"
exec cargo xtask env run-grid-llmd-pool-metrics-demo \
  --forge-config "$DEMO_DIR/forge.yaml" \
  --weighted --full \
  --evidence-dir "$EVIDENCE_DIR" \
  "$@"
