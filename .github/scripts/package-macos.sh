#!/bin/bash
set -euo pipefail
case "${1:-}" in
  ""|--preview-pkg) ;;
  *) echo "Usage: $0 [--preview-pkg]" >&2; exit 2 ;;
esac
test "$(uname -m)" = arm64
: "${CODESIGN_IDENTITY:?A certificate identity is required}"
test "$CODESIGN_IDENTITY" != -
cargo xtask build-worker
cargo xtask bundle-macos
# Remap build paths in the relocatable installer as in the native bundle.
flags="${CARGO_ENCODED_RUSTFLAGS:-}"
if [ -n "$flags" ]; then flags+=$'\x1f'; fi
flags+="--remap-path-prefix=$HOME=/home/builder"$'\x1f'"--remap-path-prefix=$PWD=/workspace/kanade-ime"
CARGO_ENCODED_RUSTFLAGS="$flags" cargo build --release --locked -p xtask --bin kanade-installer
app=artifacts/macos.noindex/Kanade.app
cp target/release/kanade-installer "$app/Contents/MacOS/kanade-installer"
codesign --force --options runtime --timestamp --identifier dev.kanade.installer --sign "$CODESIGN_IDENTITY" "$app/Contents/MacOS/kanade-installer"
codesign --force --options runtime --timestamp --sign "$CODESIGN_IDENTITY" "$app"
codesign --verify --deep --strict "$app"
"$app/Contents/MacOS/kanade-imk" --check
"$app/Contents/MacOS/kanade-installer" install-macos --status

if [ "${1:-}" = --preview-pkg ]; then
  test "${KANADE_NOTARIZE:-0}" = 0
  exec bash .github/scripts/package-macos-preview.sh "$app"
fi

stage=artifacts/distribution/macos
rm -rf "$stage"
mkdir -p "$stage"
installer="$stage/Kanade インストーラー.app"
mkdir -p "$installer/Contents/MacOS" "$installer/Contents/Resources" "$installer/Contents/Helpers"
swift build --package-path packaging/macos/installer --scratch-path artifacts/swift-installer --configuration release --product KanadeInstaller -Xswiftc -warnings-as-errors -Xswiftc -gnone -Xswiftc -file-prefix-map -Xswiftc "$HOME=/home/builder" -Xswiftc -file-prefix-map -Xswiftc "$PWD=/workspace/kanade-ime"
cp artifacts/swift-installer/release/KanadeInstaller "$installer/Contents/MacOS/KanadeInstaller"
cp packaging/macos/Installer.plist "$installer/Contents/Info.plist"
version=$(/usr/libexec/PlistBuddy -c 'Print :CFBundleShortVersionString' "$app/Contents/Info.plist")
/usr/libexec/PlistBuddy -c "Set :CFBundleShortVersionString $version" "$installer/Contents/Info.plist"
/usr/libexec/PlistBuddy -c "Set :CFBundleVersion $version" "$installer/Contents/Info.plist"
cp "$app/Contents/Resources/Kanade.icns" "$installer/Contents/Resources/Kanade.icns"
payload="$installer/Contents/Helpers/Kanade.app"
ditto "$app" "$payload"
codesign --force --options runtime --timestamp --sign "$CODESIGN_IDENTITY" "$installer"
codesign --verify --deep --strict "$installer"
"$installer/Contents/MacOS/KanadeInstaller" --check

if [ "${KANADE_NOTARIZE:-0}" = 1 ]; then
  : "${NOTARY_KEY_FILE:?}" "${NOTARY_KEY_ID:?}" "${NOTARY_ISSUER:?}"
  # The copied payload and the launcher both need offline-verifiable tickets.
  ditto -c -k --keepParent "$installer" artifacts/notarization.zip
  xcrun notarytool submit artifacts/notarization.zip --key "$NOTARY_KEY_FILE" --key-id "$NOTARY_KEY_ID" --issuer "$NOTARY_ISSUER" --wait --timeout 30m
  for bundle in "$payload" "$installer"; do
    xcrun stapler staple "$bundle"
    xcrun stapler validate "$bundle"
    spctl --assess --type execute --verbose "$bundle"
  done
  codesign --verify --deep --strict "$installer"
fi

cp LICENSE "$stage/LICENSE"
cp docs/distribution.md "$stage/README.md"
mkdir -p artifacts/release
dmg=artifacts/release/kanade-ime-aarch64-apple-darwin.dmg
hdiutil create -quiet -volname 'Kanade IME' -srcfolder "$stage" -format UDZO -ov "$dmg"
codesign --force --timestamp --sign "$CODESIGN_IDENTITY" "$dmg"
codesign --verify --strict "$dmg"
hdiutil verify "$dmg"
if [ "${KANADE_NOTARIZE:-0}" = 1 ]; then
  xcrun notarytool submit "$dmg" --key "$NOTARY_KEY_FILE" --key-id "$NOTARY_KEY_ID" --issuer "$NOTARY_ISSUER" --wait --timeout 30m
  xcrun stapler staple "$dmg"
  xcrun stapler validate "$dmg"
  spctl --assess --type open --context context:primary-signature --verbose "$dmg"
fi
python3 .github/scripts/verify-macos-installer.py "$dmg" --version "$version"
rm -f artifacts/release/kanade-ime-aarch64-apple-darwin.tar.gz
