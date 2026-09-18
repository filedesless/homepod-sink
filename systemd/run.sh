#!/usr/bin/env bash
# Pipes the PipeWire virtual-sink capture process into the AirPlay sender.
# Split into two processes deliberately: PipeWire's rtkit-granted real-time
# thread can preempt a sender thread sharing the same process unpredictably.
set -euo pipefail

# Prefer a sibling target/release build (repo checkout, dev workflow) over
# the packaged install location (/usr/lib/homepod-sink - not on PATH, since
# "capture" is too generic a name to put there) if both exist.
BIN_DIR="$(dirname "$(readlink -f "$0")")/.."
if [ -x "$BIN_DIR/target/release/capture" ]; then
    CAPTURE="$BIN_DIR/target/release/capture"
    SINK="$BIN_DIR/target/release/homepod-sink"
else
    CAPTURE="/usr/lib/homepod-sink/capture"
    SINK="/usr/lib/homepod-sink/homepod-sink"
fi

: "${HOMEPOD_IP:=}"
: "${HOMEPOD_NAME:=}"
: "${HOMEPOD_SINK_NAME:=HomePod}"
: "${HOMEPOD_PORT:=7000}"
: "${HOMEPOD_SAMPLE_RATE:=48000}"

# Neither HOMEPOD_IP nor HOMEPOD_NAME set: fall back to auto-discovery,
# which only works unattended if exactly one AirPlay device is ever on the
# network (there's no one here to resolve an ambiguous list interactively).
sink_target_args=()
if [ -n "$HOMEPOD_IP" ]; then
    sink_target_args=(--ip "$HOMEPOD_IP")
elif [ -n "$HOMEPOD_NAME" ]; then
    sink_target_args=(--name "$HOMEPOD_NAME")
fi

"$CAPTURE" --sink-name "$HOMEPOD_SINK_NAME" --sample-rate "$HOMEPOD_SAMPLE_RATE" \
    | "$SINK" "${sink_target_args[@]}" --port "$HOMEPOD_PORT" --sample-rate "$HOMEPOD_SAMPLE_RATE"
