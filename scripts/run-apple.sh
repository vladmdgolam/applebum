#!/bin/bash
# Build, bundle, sign and open Applebum. MusicKit only works from a signed
# bundle whose App ID has MusicKit enabled.
set -euo pipefail
cd "$(dirname "$0")/.."
profile="${PROFILE:-release}"
export LIBCLANG_PATH="$(dirname "$(xcrun --find clang)")/../lib"
if [ "$profile" = release ]; then cargo build --release; else cargo build; fi
app="target/$profile/Applebum.app"
# Your own App ID, with MusicKit enabled. See the README. Kept out of the repo.
[ -f .applebum.env ] && . ./.applebum.env
identifier="${BUNDLE_ID:?Set BUNDLE_ID to your App ID, or put BUNDLE_ID=... in .applebum.env. See the README.}"
rm -rf "$app"
mkdir -p "$app/Contents/MacOS" "$app/Contents/Resources"
cp "target/$profile/spotifast" "$app/Contents/MacOS/Applebum"
plist="$app/Contents/Info.plist"
sed -e "s/__VERSION__/0.1/g" -e "s/__BUILD__/1/g" -e "s/__EXECUTABLE__/Applebum/g" \
    -e "s/__IDENTIFIER__/$identifier/g" packaging/macos/Info.plist > "$plist"
plutil -replace CFBundleName -string Applebum "$plist"
plutil -replace CFBundleDisplayName -string Applebum "$plist"
plutil -replace CFBundleIconFile -string applebum "$plist"
plutil -replace NSAppleMusicUsageDescription -string "Search and play Apple Music, and show your library." "$plist"
plutil -replace NSAudioCaptureUsageDescription -string "Lets the visualizers react to the song playing. Nothing is recorded." "$plist"
# Spotify's links mean nothing here; HomePods are found on the local network.
plutil -remove CFBundleURLTypes "$plist" 2>/dev/null || true
plutil -replace NSBonjourServices -json '["_airplay._tcp"]' "$plist"
plutil -replace NSLocalNetworkUsageDescription -string "Finds your HomePods so Applebum can play on them." "$plist"
iconset="$(mktemp -d)/applebum.iconset"
mkdir -p "$iconset"
for size in 16 32 128 256 512; do
    sips -z $size $size packaging/macos/icon-1024.png --out "$iconset/icon_${size}x${size}.png" >/dev/null
    double=$((size * 2))
    sips -z $double $double packaging/macos/icon-1024.png --out "$iconset/icon_${size}x${size}@2x.png" >/dev/null
done
iconutil -c icns "$iconset" -o "$app/Contents/Resources/applebum.icns"
identity="$(security find-identity -v -p codesigning | grep -m1 'Apple Development' | sed -E 's/.*"(.*)"/\1/')"
codesign --force --sign "$identity" "$app"
echo "$app"
[ "${NO_OPEN:-}" = 1 ] || open "$app"
