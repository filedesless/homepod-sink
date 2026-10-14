#!/usr/bin/env bash
# Writes <distdir>/manifest, which arch/PKGBUILD reads from
# the "latest" release to learn the current version and checksums, from the
# .sha256 files scripts/dist.sh wrote for each arch:
#
#   pkgver=r30.7bff20d
#   sha256_x86_64=<64 hex>
#   sha256_aarch64=<64 hex>
#
#   scripts/manifest.sh r30.7bff20d dist
set -euo pipefail

version=$1 distdir=$2
{
    echo "pkgver=$version"
    for arch in x86_64 aarch64; do
        echo "sha256_$arch=$(cut -d' ' -f1 "$distdir/homepod-sink-$version-$arch.tar.gz.sha256")"
    done
} > "$distdir/manifest"
