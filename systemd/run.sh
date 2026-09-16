#!/usr/bin/env bash
# Pipes the PipeWire virtual-sink capture process into the AirPlay sender.
# Split into two processes deliberately: PipeWire's rtkit-granted real-time
# thread can preempt a sender thread sharing the same process unpredictably.
set -euo pipefail

BIN_DIR="$(dirname "$(readlink -f "$0")")/.."
CAPTURE="$BIN_DIR/target/release/capture"
SINK="$BIN_DIR/target/release/homepod-sink"

: "${HOMEPOD_IP:?HOMEPOD_IP must be set (see homepod-sink.env)}"
: "${HOMEPOD_SINK_NAME:=HomePod}"
: "${HOMEPOD_PORT:=7000}"
: "${HOMEPOD_SAMPLE_RATE:=48000}"

"$CAPTURE" --sink-name "$HOMEPOD_SINK_NAME" --sample-rate "$HOMEPOD_SAMPLE_RATE" \
    | "$SINK" --ip "$HOMEPOD_IP" --port "$HOMEPOD_PORT" --sample-rate "$HOMEPOD_SAMPLE_RATE"
