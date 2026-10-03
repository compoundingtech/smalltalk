#!/bin/sh
set -eu
cd "$(dirname "$0")"
# Match Expo's iOS minimum, including C/assembly dependencies built by cc-rs.
export IPHONEOS_DEPLOYMENT_TARGET=16.4
mkdir -p build/headers ios/build
cp ios/StFabricBridge.h build/headers/
cat > build/headers/module.modulemap <<'MAP'
module StFabricRust {
  header "StFabricBridge.h"
  export *
}
MAP
for target in aarch64-apple-ios aarch64-apple-ios-sim; do
  cargo build --manifest-path rust/Cargo.toml --release --locked --target "$target"
done
# Other native projects share this host. Never overlap their xcodebuild process.
while pgrep -x xcodebuild >/dev/null; do sleep 10; done
rm -rf ios/build/StFabricRust.xcframework
xcodebuild -create-xcframework \
  -library rust/target/aarch64-apple-ios/release/libst_fabric_bridge.a -headers build/headers \
  -library rust/target/aarch64-apple-ios-sim/release/libst_fabric_bridge.a -headers build/headers \
  -output ios/build/StFabricRust.xcframework > build/xcframework.log 2>&1
