#!/usr/bin/env bash
# Launches homepod-sink: one PipeWire virtual sink per discovered AirPlay 2
# device. Pick which one is active from your system's audio output picker
# (e.g. Noctalia) - there's no target to configure here.
set -euo pipefail

# Prefer a sibling target/release build (repo checkout, dev workflow) over
# the packaged install location if both exist.
BIN_DIR="$(dirname "$(readlink -f "$0")")/.."
if [ -x "$BIN_DIR/target/release/homepod-sink" ]; then
    SINK="$BIN_DIR/target/release/homepod-sink"
else
    SINK="/usr/lib/homepod-sink/homepod-sink"
fi

: "${HOMEPOD_SAMPLE_RATE:=48000}"

exec "$SINK" --sample-rate "$HOMEPOD_SAMPLE_RATE"
