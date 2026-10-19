#!/bin/sh
# Installs sleepless for the current user (~/.local), or system-wide when
# run as root (/usr/local). Run from the extracted release folder.
set -eu
here="$(cd "$(dirname "$0")" && pwd)"
if [ "$(id -u)" -eq 0 ]; then prefix=/usr/local; else prefix="$HOME/.local"; fi

install -Dm755 "$here/sleepless" "$prefix/bin/sleepless"
install -Dm644 "$here/sleepless.png" "$prefix/share/icons/hicolor/512x512/apps/sleepless.png"
install -Dm644 "$here/sleepless.desktop" "$prefix/share/applications/sleepless.desktop"
command -v gtk-update-icon-cache >/dev/null 2>&1 && gtk-update-icon-cache -q "$prefix/share/icons/hicolor" 2>/dev/null || true
echo "Installed to $prefix (make sure $prefix/bin is on your PATH)"
