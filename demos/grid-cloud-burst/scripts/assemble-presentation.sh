#!/usr/bin/env bash
set -euo pipefail
ROOT=$(cd "$(dirname "$0")/.." && pwd)
SOURCE="$ROOT/source/grid-burst-sim.mp4"
OUT="$ROOT/output/grid-cloud-burst-narrated.mp4"
WORK="$ROOT/output/assembly"
mkdir -p "$WORK"
test -s "$SOURCE" || { echo "Missing source recording: $SOURCE" >&2; exit 1; }
SCENES=(title reactive-burst independent-policies soft-token-limits live-demo outro)
for scene in "${SCENES[@]}"; do test -s "$ROOT/output/audio/$scene.wav" || { echo "Missing audio for $scene; run generate-audio.sh" >&2; exit 1; }; done
for scene in title reactive-burst independent-policies soft-token-limits outro; do test -s "$ROOT/output/slides/$scene.png" || { echo "Missing rendered slide for $scene; run render-slides.sh" >&2; exit 1; }; done
duration=$(ffprobe -v error -show_entries format=duration -of csv=p=0 "$SOURCE")
ffmpeg -y -hide_banner -loglevel error -f lavfi -t 1 -i anullsrc=r=44100:cl=stereo -i "$ROOT/output/audio/title.wav" -filter_complex '[0:a][1:a]concat=n=2:v=0:a=1[a]' -map '[a]' -c:a pcm_s16le "$WORK/title-audio.wav"
ffmpeg -y -hide_banner -loglevel error -i "$ROOT/output/audio/live-demo.wav" -af "apad,atrim=duration=$duration" -c:a pcm_s16le "$WORK/live-audio.wav"
ffmpeg -y -hide_banner -loglevel error -i "$ROOT/output/audio/outro.wav" -f lavfi -t 1 -i anullsrc=r=44100:cl=stereo -filter_complex '[0:a][1:a]concat=n=2:v=0:a=1[a]' -map '[a]' -c:a pcm_s16le "$WORK/outro-audio.wav"
make_card() { local image=$1 audio=$2 output=$3; local length; length=$(ffprobe -v error -show_entries format=duration -of csv=p=0 "$audio"); ffmpeg -y -hide_banner -loglevel error -loop 1 -i "$image" -i "$audio" -t "$length" -map 0:v:0 -map 1:a:0 -r 30 -c:v libx264 -preset medium -crf 18 -pix_fmt yuv420p -c:a aac -ar 44100 -ac 2 -b:a 192k "$output"; }
make_card "$ROOT/output/slides/title.png" "$WORK/title-audio.wav" "$WORK/01-title.mp4"
make_card "$ROOT/output/slides/reactive-burst.png" "$ROOT/output/audio/reactive-burst.wav" "$WORK/02-reactive-burst.mp4"
make_card "$ROOT/output/slides/independent-policies.png" "$ROOT/output/audio/independent-policies.wav" "$WORK/03-independent-policies.mp4"
make_card "$ROOT/output/slides/soft-token-limits.png" "$ROOT/output/audio/soft-token-limits.wav" "$WORK/04-soft-token-limits.mp4"
ffmpeg -y -hide_banner -loglevel error -i "$SOURCE" -i "$WORK/live-audio.wav" -t "$duration" -map 0:v:0 -map 1:a:0 -r 30 -c:v libx264 -preset medium -crf 18 -pix_fmt yuv420p -c:a aac -ar 44100 -ac 2 -b:a 192k "$WORK/05-live-demo.mp4"
make_card "$ROOT/output/slides/outro.png" "$WORK/outro-audio.wav" "$WORK/06-outro.mp4"
printf "file '%s'\nfile '%s'\nfile '%s'\nfile '%s'\nfile '%s'\nfile '%s'\n" "$WORK/01-title.mp4" "$WORK/02-reactive-burst.mp4" "$WORK/03-independent-policies.mp4" "$WORK/04-soft-token-limits.mp4" "$WORK/05-live-demo.mp4" "$WORK/06-outro.mp4" > "$WORK/concat.txt"
ffmpeg -y -hide_banner -loglevel error -f concat -safe 0 -i "$WORK/concat.txt" -c:v libx264 -preset medium -crf 18 -pix_fmt yuv420p -c:a aac -ar 44100 -ac 2 -b:a 192k "$OUT"
actual=$(ffprobe -v error -show_entries format=duration -of csv=p=0 "$OUT")
echo "Source duration: $duration seconds"
echo "Final duration:  $actual seconds"
echo "Wrote $OUT"
