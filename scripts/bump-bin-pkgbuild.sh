#!/usr/bin/env bash
# Points arch/homepod-sink-bin/PKGBUILD at <version>, taking each arch's
# checksum from the .sha256 files scripts/dist.sh wrote to <distdir>.
#
#   scripts/bump-bin-pkgbuild.sh r27.2597d29 dist
set -euo pipefail

version=$1 distdir=$2
pkgbuild="$(dirname "$(readlink -f "$0")")/../arch/homepod-sink-bin/PKGBUILD"

sed -i -e "s/^pkgver=.*/pkgver=$version/" -e "s/^pkgrel=.*/pkgrel=1/" "$pkgbuild"
for arch in x86_64 aarch64; do
    sum=$(cut -d' ' -f1 "$distdir/homepod-sink-$version-$arch.tar.gz.sha256")
    sed -i "s/^sha256sums_$arch=.*/sha256sums_$arch=('$sum')/" "$pkgbuild"
done
