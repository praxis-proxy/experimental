#!/usr/bin/env bash
set -euo pipefail

# switchyard_route local mock demo.
# Optional: JUDGE_ENDPOINT, JUDGE_MODEL, FORCE_REBUILD=1

cd "$(dirname "$0")"

GATEWAY=http://127.0.0.1:18080/v1/chat/completions
REPO_ROOT="$(cd ../.. && pwd)"
export CARGO_TARGET_DIR="${REPO_ROOT}/target"
SERVER_BIN="${CARGO_TARGET_DIR}/debug/praxis-experimental-server"
JUDGE_ENDPOINT="${JUDGE_ENDPOINT:-http://127.0.0.1:18091/v1/chat/completions}"
JUDGE_MODEL="${JUDGE_MODEL:-mock-switchyard-judge}"
MOCKS_PID=""

cleanup() {
  kill "${SERVER_PID:-}" 2>/dev/null || true
  kill "${MOCKS_PID:-}" 2>/dev/null || true
}
trap cleanup EXIT

echo "mode: local mocks (judge :18091, weak :18092, strong :18093)" >&2
python3 upstreams.py > /tmp/switchyard-demo-mocks.log 2>&1 &
MOCKS_PID=$!
sleep 0.3
if ! kill -0 "$MOCKS_PID" 2>/dev/null; then
  echo "mock servers failed; see /tmp/switchyard-demo-mocks.log" >&2
  exit 1
fi

FILTER_SRC="${REPO_ROOT}/crates/praxis-experimental-filters/src/switchyard_route.rs"
if [[ "${FORCE_REBUILD:-}" == "1" ]] \
  || [[ ! -x "$SERVER_BIN" ]] \
  || [[ "$FILTER_SRC" -nt "$SERVER_BIN" ]]; then
  echo "building praxis-experimental-server..." >&2
  (cd "$REPO_ROOT" && cargo build -p praxis-experimental-server)
fi

sed -e "s|__JUDGE_ENDPOINT__|${JUDGE_ENDPOINT}|" \
    -e "s|__JUDGE_MODEL__|${JUDGE_MODEL}|" \
    -e "/__JUDGE_AUTH_BLOCK__/d" \
    praxis.yaml.template > praxis.yaml

pkill -f 'praxis-experimental-server' 2>/dev/null || true
sleep 0.2

RUST_LOG="${RUST_LOG:-info,praxis_experimental_filters=debug}" \
  "$SERVER_BIN" > /tmp/switchyard-demo-server.log 2>&1 &
SERVER_PID=$!
ln -sfn /tmp/switchyard-demo-server.log server.log

for _ in $(seq 1 60); do
  if curl -sf -o /dev/null -m 10 -X POST "$GATEWAY" \
       -H 'content-type: application/json' \
       -d '{"model":"warmup","messages":[{"role":"user","content":"ping"}],"max_tokens":1}' 2>/dev/null; then
    break
  fi
  if ! kill -0 "$SERVER_PID" 2>/dev/null; then
    echo "server exited early; see server.log:" >&2
    tail -n 40 /tmp/switchyard-demo-server.log >&2 || true
    exit 1
  fi
  sleep 0.5
done

ask() {
  local label=$1 prompt=$2 tmp body http
  tmp=$(mktemp)
  body=$(PROMPT="$prompt" python3 - <<'PY'
import json, os
print(json.dumps({
    "model": "agent-default",
    "messages": [{"role": "user", "content": os.environ["PROMPT"]}],
    "max_tokens": 64,
    "stream": False,
}))
PY
)
  echo "--- ${label} ---"
  echo "prompt: ${prompt}"
  http=$(curl -sS -m 60 -o "$tmp" -w '%{http_code}' -X POST "$GATEWAY" \
    -H 'content-type: application/json' -d "$body" || true)
  echo "HTTP ${http}"
  if [[ -s "$tmp" ]]; then
    python3 -c 'import json,sys; r=json.load(open(sys.argv[1])); print(r["choices"][0]["message"]["content"][:200])' "$tmp" 2>/dev/null \
      || cat "$tmp"
    echo
  else
    echo "(empty body — see server.log)"
  fi
  rm -f "$tmp"
}

echo "=== easy (expect weak) ==="
ask easy1 'What is 2+2?'
ask easy2 'What is the capital of France?'
ask easy3 'Translate hello into Spanish. One word only.'

echo "=== hard (expect strong) ==="
ask hard1 'Reverse-engineer an undocumented legacy billing service with no harness.'
ask hard2 'From a blurry whiteboard photo with no image or OCR, recover every equation.'
ask hard3 'Reproduce undocumented acme-vision tensor layouts with no golden files.'

echo
echo "routing decisions (ignore warmup):"
grep -E 'switchyard_route: (judge verdict|routed|routing failed|fail-open)' /tmp/switchyard-demo-server.log || true
echo
echo "upstreams (4× weak = warmup + 3 easy, then 3× strong):"
grep -nE 'weak-upstream|strong-upstream' /tmp/switchyard-demo-mocks.log || true
