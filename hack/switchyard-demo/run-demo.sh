#!/usr/bin/env bash
set -euo pipefail

# Runs the switchyard_route demo against a REAL OpenAI-compatible judge.
#
#   JUDGE_ENDPOINT=http://127.0.0.1:11434/v1/chat/completions \
#   JUDGE_MODEL=qwen3:8b \
#   ./run-demo.sh
#
# Renders praxis.yaml from praxis.yaml.template, starts the two local echo
# upstreams (upstreams.py) and the composed switchyard-server, then runs a
# four-turn transcript demonstrating: easy->weak, hard->strong, the session
# floor holding strong on a later easy turn, and session isolation.
#
# See the sibling README.md and docs/switchyard-route.md.

cd "$(dirname "$0")"

: "${JUDGE_ENDPOINT:?set JUDGE_ENDPOINT to an OpenAI-compatible chat-completions URL}"
: "${JUDGE_MODEL:?set JUDGE_MODEL to the judge model id}"

GATEWAY=http://127.0.0.1:18080/v1/chat/completions
SERVER_BIN=../../target/debug/switchyard-server

if [[ ! -x "$SERVER_BIN" ]]; then
  echo "building switchyard-server..." >&2
  (cd ../.. && cargo build -p switchyard-server)
fi

sed -e "s|__JUDGE_ENDPOINT__|${JUDGE_ENDPOINT}|" \
    -e "s|__JUDGE_MODEL__|${JUDGE_MODEL}|" \
    praxis.yaml.template > praxis.yaml

python3 upstreams.py &
UPSTREAMS_PID=$!
"$SERVER_BIN" > server.log 2>&1 &
SERVER_PID=$!
cleanup() {
  kill "$SERVER_PID" "$UPSTREAMS_PID" 2>/dev/null || true
}
trap cleanup EXIT
sleep 2

ask() { # ask <session> <prompt>
  curl -s -m 120 -X POST "$GATEWAY" \
    -H 'content-type: application/json' \
    -H "x-switchyard-session-id: $1" \
    -d "{\"model\":\"agent-default\",\"messages\":[{\"role\":\"user\",\"content\":\"$2\"}]}"
  echo
}

echo "--- turn 1: easy question, session demo-A (expect weak) ---"
ask demo-A 'What is 2+2?'
echo "--- turn 2: hard question, session demo-A (expect strong) ---"
ask demo-A 'Prove that every finitely generated group acting freely on a tree is free, and generalize to graphs of groups.'
echo "--- turn 3: easy question again, session demo-A (the floor must HOLD strong) ---"
ask demo-A 'What is 3+3?'
echo "--- turn 4: easy question, fresh session demo-B (isolated; expect weak) ---"
ask demo-B 'What is 4+4?'
echo
echo "filter decisions and errors: grep switchyard server.log"
