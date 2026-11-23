#!/bin/bash
set -euo pipefail

app=${1:?Path to the signed Kanade.app is required}
output=artifacts/release/kanade-ime-aarch64-apple-darwin.pkg
test -x "$app/Contents/MacOS/kanade-installer"
codesign --verify --deep --strict "$app"
version=$(/usr/libexec/PlistBuddy -c 'Print :CFBundleShortVersionString' "$app/Contents/Info.plist")

mkdir -p "$(dirname "$output")"
stage=$(mktemp -d "$(dirname "$output")/.kanade-pkg.XXXXXXXX")
trap '/bin/rm -rf "$stage"' EXIT
mkdir "$stage/scripts"
install -m 755 packaging/macos/pkg/postinstall "$stage/scripts/postinstall"
ditto -c -k --keepParent "$app" "$stage/scripts/Kanade.zip"
pkgbuild --nopayload --identifier dev.kanade.preview --version "$version" \
  --scripts "$stage/scripts" "$stage/Kanade.pkg"
productbuild --distribution packaging/macos/pkg/Distribution.xml \
  --package-path "$stage" "$stage/preview.pkg"
test "$(installer -dominfo -pkg "$stage/preview.pkg")" = CurrentUserHomeDirectory
mv -f "$stage/preview.pkg" "$output"
