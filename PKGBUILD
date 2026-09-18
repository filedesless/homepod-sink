# Maintainer: filedesless <filedesless@gmail.com>
pkgname=homepod-sink-git
pkgver=r0.0000000
pkgrel=1
pkgdesc="Turns a HomePod into a PipeWire audio output via AirPlay 2"
arch=('x86_64' 'aarch64')
url="https://github.com/filedesless/homepod-sink"
license=('GPL-3.0-or-later')
depends=('pipewire')
makedepends=('cargo' 'git' 'clang' 'pkgconf' 'libpipewire')
provides=('homepod-sink')
conflicts=('homepod-sink')
backup=('etc/homepod-sink/homepod-sink.env.example')
source=("$pkgname::git+$url.git")
sha256sums=('SKIP')

pkgver() {
    cd "$srcdir/$pkgname"
    printf "r%s.%s" "$(git rev-list --count HEAD)" "$(git rev-parse --short HEAD)"
}

build() {
    cd "$srcdir/$pkgname"
    cargo build --release --locked --bin homepod-sink --bin capture --bin play
}

check() {
    cd "$srcdir/$pkgname"
    cargo test --release --locked
}

package() {
    cd "$srcdir/$pkgname"

    install -Dm755 target/release/homepod-sink "$pkgdir/usr/lib/homepod-sink/homepod-sink"
    install -Dm755 target/release/capture "$pkgdir/usr/lib/homepod-sink/capture"
    install -Dm755 target/release/play "$pkgdir/usr/lib/homepod-sink/play"
    install -Dm755 systemd/run.sh "$pkgdir/usr/lib/homepod-sink/run.sh"

    install -Dm644 systemd/homepod-sink-installed.service \
        "$pkgdir/usr/lib/systemd/user/homepod-sink.service"
    install -Dm644 systemd/homepod-sink.env.example \
        "$pkgdir/etc/homepod-sink/homepod-sink.env.example"

    install -Dm644 README.md "$pkgdir/usr/share/doc/$pkgname/README.md"
    install -Dm644 LICENSE "$pkgdir/usr/share/licenses/$pkgname/LICENSE"
}
