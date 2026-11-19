#!/bin/zsh
# Builds the zip for a GitHub release from the current checkout:
# dist/wave-<version>-macos-arm64.zip.
#
# Unlike a development build, it leaves nothing personal in the download. Source paths are
# rewritten (no /Users/<you>/... in panic messages), debug info is stripped, and the app is signed
# ad hoc, not with a local certificate. It builds in target/dist, so the development build in
# target/release stays as it is.
set -e
cd "${0:A:h}/.."
version=$(sed -n 's/^version = "\(.*\)"/\1/p' Cargo.toml | head -1)
export CARGO_TARGET_DIR=target/dist
export CARGO_PROFILE_RELEASE_DEBUG=0 CARGO_PROFILE_RELEASE_STRIP=true
export RUSTFLAGS="--remap-path-prefix=$HOME/.cargo=/cargo --remap-path-prefix=$HOME/.rustup=/rustup --remap-path-prefix=$PWD=/wave"
cargo build --release
WAVE_SIGN_IDENTITY=- scripts/bundle.sh

app=$CARGO_TARGET_DIR/release/wave.app
if strings -a $app/Contents/MacOS/* | grep -q "$HOME"; then
  echo "the build still contains $HOME; not packaging it" >&2
  exit 1
fi
mkdir -p dist
zip=dist/wave-$version-macos-arm64.zip
rm -f $zip
ditto -c -k --keepParent $app $zip
echo "wrote $zip"
