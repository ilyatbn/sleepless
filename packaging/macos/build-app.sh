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

# Finder Quick Actions (right-click > Quick Actions). Sleepless copies these
# into ~/Library/Services the first time the app runs. UUIDs are fixed per
# action so rebuilding doesn't make every install look "changed".
services="$app/Contents/Resources/Services"
quick_action() {
    name="$1"; args="$2"; uuid="$3"
    wf="$services/$name.workflow/Contents"
    mkdir -p "$wf"
    sed "s/@NAME@/$name/g" "$here/quick-action/Info.plist" > "$wf/Info.plist"
    sed -e "s/@ARGS@/$args/g" \
        -e "s/@UUID1@/${uuid}1/g" -e "s/@UUID2@/${uuid}2/g" -e "s/@UUID3@/${uuid}3/g" \
        "$here/quick-action/document.wflow" > "$wf/document.wflow"
}
quick_action "Sleepless as root"           "--asroot"        "5D1E0A1B-7C2E-4F3A-9B10-00000000A00"
quick_action "Sleepless with lock"         "--lock"          "5D1E0A1B-7C2E-4F3A-9B10-00000000B00"
quick_action "Sleepless as root with lock" "--asroot --lock" "5D1E0A1B-7C2E-4F3A-9B10-00000000C00"

# Ad-hoc signature: not a Developer ID (so Gatekeeper still needs the
# xattr step from the README), but Apple Silicon refuses to run a bundle
# with no signature at all.
if command -v codesign >/dev/null 2>&1; then
    codesign --force --deep --sign - "$app"
fi
echo "Built $app"
