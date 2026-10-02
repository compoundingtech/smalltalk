#!/bin/bash
# Build StListen.app (st-listen, stui's speech helper) into OUT (default ./build), signed with
# ST_MACOS_SIGNING_IDENTITY or ad hoc. macOS 26 or later, with Xcode's Swift.
set -euo pipefail
here=$(cd "$(dirname "$0")" && pwd)
out=${1:-$here/build}
app=$out/StListen.app
rm -rf "$app"
mkdir -p "$app/Contents/MacOS"
cp "$here/Info.plist" "$app/Contents/Info.plist"
xcrun swiftc -O -target "$(uname -m)-apple-macos26.0" -o "$app/Contents/MacOS/st-listen" "$here/main.swift"
codesign --force --sign "${ST_MACOS_SIGNING_IDENTITY:--}" --identifier com.compoundingtech.smalltalk.listen \
  --options runtime --entitlements "$here/listen.entitlements" --timestamp=none "$app"
echo "$app"
