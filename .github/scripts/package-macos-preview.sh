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
# Check the domains embedded by productbuild without starting an Installer session
# on the signing machine. CI installs this exact product on a fresh Mac.
(
  cd "$stage"
  /usr/bin/xar -xf preview.pkg Distribution
  domains='/installer-gui-script/domains'
  valid="count($domains) = 1 and $domains[@enable_currentUserHome='true' and @enable_localSystem='false' and @enable_anywhere='false']"
  if [ "$(/usr/bin/xmllint --xpath "$valid" Distribution)" != true ]; then
    echo "The preview package must offer only the current user's home." >&2
    exit 1
  fi
)
mv -f "$stage/preview.pkg" "$output"
echo "Verified the current-user distribution in $output"
