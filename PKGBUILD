pkgname=srtui
pkgver=0.1.0
pkgrel=1
pkgdesc="Terminal UI for listening to Sveriges Radio with cava-style visualization and recording"
arch=('x86_64')
url="https://local/srtui"
license=('custom:none')
depends=('cava' 'ffmpeg' 'alsa-lib' 'glibc')
makedepends=('cargo')
options=('!lto')

build() {
  cd "$startdir"
  cargo build --release --locked --bin srtui
}

package() {
  cd "$startdir"
  install -Dm755 target/release/srtui "$pkgdir/usr/bin/srtui"
  install -Dm644 README.md "$pkgdir/usr/share/doc/srtui/README.md"
}
