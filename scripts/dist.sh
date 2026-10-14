#!/usr/bin/env bash
# Packs an already-built release (cargo build --release --bin homepod-sink
# --bin play) into homepod-sink-<version>-<arch>.tar.gz under <outdir>, with
# everything arch/PKGBUILD installs, plus a .sha256 next to it.
#
#   scripts/dist.sh r27.2597d29 x86_64 dist
set -euo pipefail

version=$1 arch=$2 outdir=$3
root="$(dirname "$(readlink -f "$0")")/.."
name="homepod-sink-$version-$arch"
stage="$(mktemp -d)"
trap 'rm -rf "$stage"' EXIT

mkdir -p "$stage/$name" "$outdir"
install -m755 "$root/target/release/homepod-sink" "$root/target/release/play" \
    "$root/systemd/run.sh" "$stage/$name/"
install -m644 "$root/systemd/homepod-sink-installed.service" \
    "$stage/$name/homepod-sink.service"
install -m644 "$root/systemd/homepod-sink.env.example" \
    "$root/README.md" "$root/LICENSE" "$stage/$name/"

# Reproducible-ish: fixed owner and mtime, sorted entries.
tar --sort=name --owner=0 --group=0 --numeric-owner \
    --mtime="@$(git -C "$root" log -1 --format=%ct)" \
    -C "$stage" -czf "$outdir/$name.tar.gz" "$name"
(cd "$outdir" && sha256sum "$name.tar.gz" > "$name.tar.gz.sha256")
echo "$outdir/$name.tar.gz"
