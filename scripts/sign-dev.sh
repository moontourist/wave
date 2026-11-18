#!/bin/zsh
# Signs the release build with a fixed local certificate, so macOS keeps wave's Accessibility and
# Screen Recording permissions across rebuilds. An unsigned build gets a new identity every time,
# and macOS asks for both permissions again.
#
#   cargo build --release && scripts/sign-dev.sh && target/release/wave service restart
#
# The certificate is a self-signed code-signing one in your login keychain, named by
# WAVE_SIGN_IDENTITY (default "wave dev"). The README's Develop section shows how to make one.
set -e
cd "${0:A:h}/.."
codesign --force --sign "${WAVE_SIGN_IDENTITY:-wave dev}" --identifier com.moontourist.wave target/release/wave
codesign --display --requirements - target/release/wave 2>&1 | grep designated
