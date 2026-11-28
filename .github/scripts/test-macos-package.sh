#!/bin/bash
# Build both formats on a disposable Mac. Only stable uses Developer ID.
set -euo pipefail
test "${RUNNER_ENVIRONMENT:-}" = github-hosted
folder=$(mktemp -d "${RUNNER_TEMP:?}/kanade-signing.XXXXXX")
keychain="$folder/ci.keychain-db"
password=$(openssl rand -hex 24)
identity='Kanade installer CI'
# The signing key lives only on this runner. Preview downloads can outlive it.
openssl req -x509 -newkey rsa:2048 -nodes -days 3650 -subj "/CN=$identity" \
  -addext 'keyUsage=critical,digitalSignature' -addext 'extendedKeyUsage=critical,codeSigning' \
  -keyout "$folder/key.pem" -out "$folder/cert.pem"
openssl pkcs12 -export -inkey "$folder/key.pem" -in "$folder/cert.pem" \
  -keypbe PBE-SHA1-3DES -certpbe PBE-SHA1-3DES -macalg sha1 \
  -out "$folder/ci.p12" -passout "pass:$password"
security create-keychain -p "$password" "$keychain"
security set-keychain-settings -lut 7200 "$keychain"
security unlock-keychain -p "$password" "$keychain"
security import "$folder/ci.p12" -k "$keychain" -P "$password" -T /usr/bin/codesign
security set-key-partition-list -S apple-tool:,apple:,codesign: -s -k "$password" "$keychain" >/dev/null
security list-keychains -d user -s "$keychain" login.keychain-db
sudo security add-trusted-cert -d -r trustRoot -p codeSign -k /Library/Keychains/System.keychain "$folder/cert.pem"
CODESIGN_IDENTITY="$identity" KANADE_NOTARIZE=0 bash .github/scripts/package-macos.sh
bash .github/scripts/package-macos-preview.sh artifacts/macos.noindex/Kanade.app
