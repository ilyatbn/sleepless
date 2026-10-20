#!/bin/sh
# Wraps built sleepless-gui and sleepless binaries into "Sleepless GUI.app".
# The CLI goes next to the GUI inside the bundle, which is where the menu
# bar app looks for it.
#   packaging/macos/build-gui-app.sh <path/to/sleepless-gui> <path/to/sleepless> <version> <out-dir>
set -eu
gui="$1"; cli="$2"; version="$3"; out="$4"
here="$(cd "$(dirname "$0")" && pwd)"
root="$here/../.."
app="$out/Sleepless GUI.app"

rm -rf "$app"
mkdir -p "$app/Contents/MacOS" "$app/Contents/Resources"
cp "$gui" "$app/Contents/MacOS/sleepless-gui"
cp "$cli" "$app/Contents/MacOS/sleepless"
chmod +x "$app/Contents/MacOS/sleepless-gui" "$app/Contents/MacOS/sleepless"
cp "$root/assets/icon.icns" "$app/Contents/Resources/sleepless.icns"
sed "s/@VERSION@/$version/g" "$here/gui/Info.plist" > "$app/Contents/Info.plist"
printf 'APPL????' > "$app/Contents/PkgInfo"

# Ad-hoc signature, see build-app.sh.
if command -v codesign >/dev/null 2>&1; then
    codesign --force --deep --sign - "$app"
fi
echo "Built $app"
