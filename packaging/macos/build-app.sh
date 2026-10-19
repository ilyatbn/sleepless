#!/bin/sh
# Wraps a built sleepless binary into Sleepless.app.
#   packaging/macos/build-app.sh <path/to/sleepless> <version> <out-dir>
set -eu
bin="$1"; version="$2"; out="$3"
here="$(cd "$(dirname "$0")" && pwd)"
root="$here/../.."
app="$out/Sleepless.app"

rm -rf "$app"
mkdir -p "$app/Contents/MacOS" "$app/Contents/Resources"
cp "$bin" "$app/Contents/MacOS/sleepless"
chmod +x "$app/Contents/MacOS/sleepless"
cp "$root/assets/icon.icns" "$app/Contents/Resources/sleepless.icns"
sed "s/@VERSION@/$version/g" "$here/Info.plist" > "$app/Contents/Info.plist"
printf 'APPL????' > "$app/Contents/PkgInfo"

# Ad-hoc signature: not a Developer ID (so Gatekeeper still needs the
# xattr step from the README), but Apple Silicon refuses to run a bundle
# with no signature at all.
if command -v codesign >/dev/null 2>&1; then
    codesign --force --deep --sign - "$app"
fi
echo "Built $app"
