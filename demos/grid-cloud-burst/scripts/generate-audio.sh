#!/usr/bin/env bash
set -euo pipefail

ROOT=$(cd "$(dirname "$0")/.." && pwd)
KEY_FILE=${OPENAI_KEY_FILE:-"$HOME/rhoai/oai-key"}
test -s "$KEY_FILE" || { echo "Missing OpenAI key file: $KEY_FILE" >&2; exit 1; }
command -v jq >/dev/null || { echo "jq is required" >&2; exit 1; }

mkdir -p "$ROOT/output/audio"
raw_key=$(<"$KEY_FILE")
raw_key=${raw_key#OPENAI_API_KEY=}
raw_key=${raw_key#export OPENAI_API_KEY=}
raw_key=${raw_key%$'\n'}
raw_key=${raw_key%$'\r'}
raw_key=${raw_key#\"}
raw_key=${raw_key%\"}

for scene in title reactive-burst independent-policies soft-token-limits live-demo outro; do
  input="$ROOT/narration/$scene.txt"
  output="$ROOT/output/audio/$scene.wav"
  speed=1
  [ "$scene" = live-demo ] && speed=1.26
  payload=$(jq -Rs --arg model "tts-1" --arg voice "alloy" --argjson speed "$speed" \
    '{model:$model, voice:$voice, speed:$speed, input:., response_format:"wav"}' < "$input")
  printf 'Authorization: Bearer %s\n' "$raw_key" | \
    curl --fail --silent --show-error https://api.openai.com/v1/audio/speech \
      -H @- -H 'Content-Type: application/json' \
      -d "$payload" -o "$output"
  echo "Generated $scene narration"
done
unset raw_key payload
