#!/usr/bin/env bash
set -euo pipefail

# Runs the switchyard_route demo against a REAL OpenAI-compatible judge.
#
# Keyless local judge (Ollama / vLLM / LM Studio):
#
#   JUDGE_ENDPOINT=http://127.0.0.1:11434/v1/chat/completions \
#   JUDGE_MODEL=qwen3:8b \
#   ./run-demo.sh
#
# Hosted judge that needs a bearer token (OpenAI, Together, Fireworks, …):
#
#   JUDGE_ENDPOINT=https://api.openai.com/v1/chat/completions \
#   JUDGE_MODEL=gpt-4o-mini \
#   OPENAI_API_KEY=sk-... \
#   JUDGE_KEY_ENV=OPENAI_API_KEY \
#   ./run-demo.sh
#
# JUDGE_KEY_ENV names the environment variable holding the token; the filter
# reads that variable at startup, so the secret never touches praxis.yaml. Leave
# it unset for a keyless judge and the auth block is dropped entirely.
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

# Build the judge auth block only when JUDGE_KEY_ENV is set. When it is, verify
# the named variable actually holds a value now, so a typo fails here with a
# clear message instead of later as a fail-open judge callout.
if [[ -n "${JUDGE_KEY_ENV:-}" ]]; then
  if [[ -z "${!JUDGE_KEY_ENV:-}" ]]; then
    echo "JUDGE_KEY_ENV=${JUDGE_KEY_ENV} but that variable is unset or empty" >&2
    exit 1
  fi
  AUTH_BLOCK="          auth:"$'\n'"            value_env: ${JUDGE_KEY_ENV}"
  # The server reads the credential from its own environment, so export it in
  # case the caller passed it as a plain (unexported) shell variable.
  export "${JUDGE_KEY_ENV?}"
  echo "judge auth: sending the value of \$${JUDGE_KEY_ENV} as 'authorization: Bearer ...'" >&2
else
  AUTH_BLOCK=""
  echo "judge auth: none (keyless judge)" >&2
fi

# Render the template. The auth-block substitution reads from a file to keep the
# secret's *name* (never its value) out of the sed program text, and to carry
# the block's newline cleanly.
printf '%s\n' "$AUTH_BLOCK" > .auth-block.yaml
sed -e "s|__JUDGE_ENDPOINT__|${JUDGE_ENDPOINT}|" \
    -e "s|__JUDGE_MODEL__|${JUDGE_MODEL}|" \
    -e "/__JUDGE_AUTH_BLOCK__/{
           r .auth-block.yaml
           d
         }" \
    praxis.yaml.template > praxis.yaml
rm -f .auth-block.yaml

python3 upstreams.py &
UPSTREAMS_PID=$!
# Raise the filter to debug so successful routing decisions (logged at debug!)
# are visible in server.log, not just the warn-level fail-open path. Honour a
# caller-supplied RUST_LOG if they want something else.
RUST_LOG="${RUST_LOG:-info,switchyard_filters=debug}" "$SERVER_BIN" > server.log 2>&1 &
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
echo "filter decisions and fail-open reasons: grep switchyard_route server.log"
