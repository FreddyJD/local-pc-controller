#!/bin/sh
set -eu
cd "$(dirname "$0")/.."
export MACOSX_DEPLOYMENT_TARGET=12.0
cargo build --release --locked
APP="dist/Localnet.app"
mkdir -p "$APP/Contents/MacOS" "$APP/Contents/Resources"
cp target/release/localnet "$APP/Contents/MacOS/localnet"
cp packaging/macos/Info.plist "$APP/Contents/Info.plist"
if [ -n "${LOCALNET_SIGNING_IDENTITY:-}" ]; then
    codesign --force --options runtime --timestamp --sign "$LOCALNET_SIGNING_IDENTITY" "$APP"
else
    codesign --force --sign - "$APP"
fi
printf 'Built %s. Grant Screen Recording and Accessibility after opening it.\n' "$APP"
