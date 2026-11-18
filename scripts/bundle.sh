#!/bin/zsh
# Packs the release build into target/release/wave.app: both binaries, Info.plist with the crate's
# version, and the icon built from assets/icon.png. Then signs the app.
#
#   cargo build --release && scripts/bundle.sh
#   target/release/wave.app/Contents/MacOS/wave service restart
#
# Signing uses the local code-signing certificate named by WAVE_SIGN_IDENTITY (default "wave"),
# so macOS keeps wave's Accessibility and Screen Recording permissions across rebuilds. Without
# that certificate the app is signed ad hoc, and macOS asks for both permissions after each build.
# The README's Develop section shows how to make the certificate.
set -e
cd "${0:A:h}/.."
app=target/release/wave.app
identity=${WAVE_SIGN_IDENTITY:-wave}
version=$(sed -n 's/^version = "\(.*\)"/\1/p' Cargo.toml | head -1)

rm -rf $app
mkdir -p $app/Contents/MacOS $app/Contents/Resources
cp target/release/wave target/release/wave-cli $app/Contents/MacOS/
sed "s/@VERSION@/$version/g" assets/Info.plist > $app/Contents/Info.plist

# iconutil wants every size from 16 to 512 points, at 1x and 2x.
iconset=$(mktemp -d)/wave.iconset
mkdir -p $iconset
for size in 16 32 128 256 512; do
  sips -z $size $size assets/icon.png --out $iconset/icon_${size}x${size}.png >/dev/null
  sips -z $((size * 2)) $((size * 2)) assets/icon.png --out $iconset/icon_${size}x${size}@2x.png >/dev/null
done
iconutil -c icns $iconset -o $app/Contents/Resources/wave.icns
rm -rf ${iconset:h}

if ! security find-identity -p codesigning | grep -q "\"$identity\""; then
  echo "no \"$identity\" certificate: signing ad hoc"
  identity=-
fi
codesign --force --sign $identity --identifier com.moontourist.wave-cli $app/Contents/MacOS/wave-cli
codesign --force --sign $identity --identifier com.moontourist.wave $app
echo "built $app ($version)"
