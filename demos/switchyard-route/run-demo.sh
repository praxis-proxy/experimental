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
SERVER_PID=""
SERVER_LOG=/tmp/switchyard-demo-server.log
MOCKS_LOG=/tmp/switchyard-demo-mocks.log

cleanup() {
  kill "${SERVER_PID:-}" 2>/dev/null || true
  kill "${MOCKS_PID:-}" 2>/dev/null || true
}
trap cleanup EXIT

render_config() {
  local floor=$1
  sed -e "s|__JUDGE_ENDPOINT__|${JUDGE_ENDPOINT}|" \
    -e "s|__JUDGE_MODEL__|${JUDGE_MODEL}|" \
    -e "s|__SESSION_FLOOR__|${floor}|" \
    -e "/__JUDGE_AUTH_BLOCK__/d" \
    praxis.yaml.template > praxis.yaml
}

wait_for_gateway() {
  local _i
  for _i in $(seq 1 60); do
    if curl -sf -o /dev/null -m 2 -X POST "$GATEWAY" \
      -H 'content-type: application/json' \
      -d '{"model":"warmup","messages":[{"role":"user","content":"ping"}],"max_tokens":1}' 2>/dev/null; then
      return 0
    fi
    if [[ -n "${SERVER_PID}" ]] && ! kill -0 "$SERVER_PID" 2>/dev/null; then
      echo "server exited early; see server.log:" >&2
      tail -n 40 "$SERVER_LOG" >&2 || true
      exit 1
    fi
    sleep 0.5
  done
  echo "gateway did not become ready; see server.log" >&2
  tail -n 40 "$SERVER_LOG" >&2 || true
  exit 1
}

start_server() {
  local append=${1:-}
  if [[ "$append" == append ]]; then
    RUST_LOG="${RUST_LOG:-info,praxis_experimental_filters=debug}" \
      "$SERVER_BIN" >> "$SERVER_LOG" 2>&1 &
  else
    RUST_LOG="${RUST_LOG:-info,praxis_experimental_filters=debug}" \
      "$SERVER_BIN" > "$SERVER_LOG" 2>&1 &
  fi
  SERVER_PID=$!
  ln -sfn "$SERVER_LOG" server.log
  wait_for_gateway
}

stop_server() {
  if [[ -n "${SERVER_PID}" ]]; then
    kill "$SERVER_PID" 2>/dev/null || true
    sleep 0.2
    kill -9 "$SERVER_PID" 2>/dev/null || true
    wait "$SERVER_PID" 2>/dev/null || true
    SERVER_PID=""
  fi
  local _i
  for _i in $(seq 1 50); do
    if ! python3 -c "import socket;s=socket.socket();s.settimeout(0.2);s.connect(('127.0.0.1',18080))" 2>/dev/null; then
      return 0
    fi
    sleep 0.1
  done
}

echo "mode: local mocks (judge :18091, weak :18092, strong :18093)" >&2
python3 upstreams.py > "$MOCKS_LOG" 2>&1 &
MOCKS_PID=$!
sleep 0.3
if ! kill -0 "$MOCKS_PID" 2>/dev/null; then
  echo "mock servers failed; see ${MOCKS_LOG}" >&2
  exit 1
fi

FILTER_SRC_DIR="${REPO_ROOT}/crates/praxis-experimental-filters/src"
NEWER_SRC=$(find "$FILTER_SRC_DIR" -name '*.rs' -newer "$SERVER_BIN" -print -quit 2>/dev/null || true)
if [[ "${FORCE_REBUILD:-}" == "1" ]] \
  || [[ ! -x "$SERVER_BIN" ]] \
  || [[ -n "$NEWER_SRC" ]]; then
  echo "building praxis-experimental-server..." >&2
  (cd "$REPO_ROOT" && cargo build -p praxis-experimental-server)
fi

render_config enabled
pkill -f 'praxis-experimental-server' 2>/dev/null || true
sleep 0.2
start_server

ask() {
  local label=$1 prompt=$2 session=${3:-} tmp body http curl_args
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
  curl_args=(-sS -m 60 -o "$tmp" -w '%{http_code}' -X POST "$GATEWAY" \
    -H 'content-type: application/json')
  if [[ -n "$session" ]]; then
    echo "session: ${session}"
    curl_args+=(-H "x-switchyard-session-id: ${session}")
  fi
  http=$(curl "${curl_args[@]}" -d "$body" || true)
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

# Newest matching lines from the gateway log, copied as written.
print_route_logs() {
  local count=$1
  echo "from ${SERVER_LOG}:"
  grep -E 'switchyard_route: (judge verdict|routed|floor_skip|reuse|default_strong)' "$SERVER_LOG" \
    | tail -n "$count" || true
}

# Newest mock-judge line for this prompt, if the judge was called.
print_judge_preview() {
  local needle=${1:0:60}
  echo "searching ${MOCKS_LOG} for preview='${needle}':"
  grep -F "preview='${needle}'" "$MOCKS_LOG" | tail -n 1 \
    || echo "(none — judge was not called for this prompt)"
}

set_judge() {
  local state=$1
  curl -sS -m 5 -o /dev/null -X POST "http://127.0.0.1:18091/control/${state}"
  echo "judge: ${state}"
}

echo "=== easy (expect weak) ==="
ask easy1 'What is 2+2?'
ask easy2 'What is the capital of France?'
ask easy3 'Translate hello into Spanish. One word only.'

echo "=== hard (expect strong) ==="
ask hard1 'Reverse-engineer an undocumented legacy billing service with no harness.'
ask hard2 'From a blurry whiteboard photo with no image or OCR, recover every equation.'
ask hard3 'Reproduce undocumented acme-vision tensor layouts with no golden files.'

echo "=== session floor, judge healthy (expect Weak then Strong then floor_skip Strong) ==="
ask floor-easy 'Count to three.' demo-floor
echo "logs:"
print_route_logs 2
print_judge_preview 'Count to three.'
ask floor-hard 'Reverse-engineer an undocumented legacy billing service with no harness.' demo-floor
echo "logs:"
print_route_logs 2
print_judge_preview 'Reverse-engineer an undocumented legacy billing service with no harness.'
ask floor-stay 'Thanks, just say ok.' demo-floor
echo "logs:"
print_route_logs 1
print_judge_preview 'Thanks, just say ok.'

echo "=== mid-session judge down (expect reuse Weak, then default Strong) ==="
ask reuse-weak-seed 'Count to four.' demo-reuse-weak
set_judge down
ask reuse-weak-after 'What is 2+2?' demo-reuse-weak
ask empty-store-while-down 'What is the capital of France?' demo-empty-store
set_judge up

echo "=== session_floor disabled (restart; expect Strong then Weak on the same session) ==="
stop_server
render_config disabled
echo "session_floor: disabled" >&2
start_server append
ask disabled-hard 'Reverse-engineer an undocumented legacy billing service with no harness.' demo-no-floor
ask disabled-easy 'What colour is the sky?' demo-no-floor

echo
echo "routing decisions (ignore warmup):"
grep -E 'switchyard_route: (judge verdict|routed|floor_skip|reuse|default_strong|routing failed|fail-open)' \
  "$SERVER_LOG" || true
echo
echo "upstreams:"
grep -nE 'weak-upstream|strong-upstream' "$MOCKS_LOG" || true

missing=0
for token in floor_skip reuse default_strong; do
  if ! grep -q "switchyard_route: ${token}" "$SERVER_LOG"; then
    echo "missing expected log: switchyard_route: ${token}" >&2
    missing=1
  fi
done
if grep -qF "preview='Thanks, just say ok.'" "$MOCKS_LOG"; then
  echo "judge was called on the floor-stay prompt; expected floor_skip" >&2
  missing=1
fi
if ! grep -qF "preview='What colour is the sky?'" "$MOCKS_LOG"; then
  echo "judge was not called on the disabled-easy prompt; expected a fresh Weak verdict" >&2
  missing=1
fi
if [[ "$missing" -ne 0 ]]; then
  exit 1
fi
