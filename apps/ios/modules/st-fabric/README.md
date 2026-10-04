# Fabric development proof

This local Expo module provides a temporary loopback HTTP listener backed by an app-owned iroh endpoint. Each accepted TCP connection opens a fresh direct service-ALPN fabric session. The ordinary TypeScript client then uses that listener without a transport change. Normal gateway entry still rejects loopback, and the proof never saves its listener, pairing credential or gateway configuration.

The carrier is opt-in: normal pod installation compiles `StFabricDisabled.swift` and needs neither Rust nor an XCFramework. Even an enabled carrier rejects operations in Release. The Debug entry is a `com.compoundingtech.smalltalk.starter://fabric-proof` link, separate from ordinary pairing. Closing the proof returns to the saved gateway; entering the background stops its endpoint.

## Build

Install Rust targets `aarch64-apple-ios` and `aarch64-apple-ios-sim`, then from `apps/ios`:

```sh
sh modules/st-fabric/build.sh
npx expo prebuild --platform ios --no-install
ST3_FABRIC_PROOF=1 npm run pods
```

Build Debug for a simulator of your own. The script targets iOS 16.4, including its C/assembly dependencies, and writes the XCFramework under `ios/build` inside the pod root. On a shared build host, wait until `pgrep -x xcodebuild` finds no process before starting Xcode. Generated static libraries and the device/simulator XCFramework are ignored; do not commit them. To restore the default build, reinstall pods without `ST3_FABRIC_PROOF`.

## Isolated proof

Use the installed `st` and a pinned **fabric 0.2.30+8bd9017** binary, even if the host's default fabric has since been upgraded. From the repository root, keep this helper running in its own terminal:

```sh
node apps/ios/modules/st-fabric/proof-member.mjs /path/to/st /path/to/fabric
```

It starts a test member and fabric daemon under a fresh `/tmp/st-fabric-proof-*` directory, exposes only `client.sock` as `demo-client/0`, and prints a descriptor path. Open its `identity-link.txt` in the Debug app to read the public phone NodeID. You can also set `EXPO_PUBLIC_ST3_FABRIC_PROOF_LINK` when starting a local Metro server for a headless simulator launch. Then grant only that service and create a test pairing challenge:

```sh
node apps/ios/modules/st-fabric/proof-member.mjs grant /tmp/st-fabric-proof-example/member.json PHONE_NODE_ID
```

Open the privately written `pair-link.txt` in the app. The temporary `St3Client` completes pairing and reads capabilities through fabric. Pairing uses its own temporary device key value, independent of the iroh identity; no action-signing key or bearer is reused as a transport key. Do not publish the link or put its code in logs. Use an isolated, localhost Metro server if injecting a pairing link through an environment variable.

After verification, remove `demo-phone` using `fabric --home TEST_HOME remove demo-phone`, run `reload-peers`, and stop the helper. At the pinned v0.2.30, removing the grant blocks future admission but does not close sessions already attached; stopping the app bridge and the isolated daemon closes those. Since [v0.2.31](https://github.com/compoundingtech/fabric/blob/v0.2.31/docs/tunnel-wire.md#trust-after-admission), a successful reload also ends admitted sessions whose grant or peer was removed, closing direct connections with code 403 and the corresponding admission refusal reason. Treat that as a refusal; a failed reload ends no sessions. The wire bytes are unchanged, and this proof remains pinned to v0.2.30. The test daemon uses a 30 second detached-session TTL, while the pinned daemon's default is 15 minutes.

## Wire and limits

Fabric is pinned to tag `v0.2.30`, commit `8bd9017a79f4aaa2b25a33321bd7daed6a0acaa6`; iroh is pinned to `1.0.2` with this crate's separate Cargo lock. See `docs/st3/ios-fabric-protocol.md` for framing and admission. A change to direct exposure ALPN, Hello/Data/Ack/Close bytes, acknowledgment semantics, or trust/grant checks can break the adapter. This does not implement `fabric/mux/2` or session resume.

The Keychain holds a random 32 byte iroh secret under a separate service/account with `WhenUnlockedThisDeviceOnly` accessibility. JavaScript receives only its public NodeID. Native dial validates that an optional EndpointAddr hint matches the requested NodeID. The only accepted loopback URL is the newly returned native listener; user-entered and saved loopback addresses remain invalid.

There are at most 16 concurrent local sessions. Writes stop at the pinned 4 MiB unacknowledged window (with one 8 KiB read overshoot); received bytes are acknowledged after delivery. Half-closes carry final offsets, and orderly completion sends the final Ack before closing QUIC. Stop attempts a bounded graceful drain, then forces teardown if the peer or local consumer stalls. A transport loss closes the local socket and never retries a mutation.

The two local vendored patches make otherwise macOS-only networking code compile for iOS; their `ST-PATCH.md` files describe the changes and limitations. iOS enumerates interfaces via netdev, and Swift's `NWPathMonitor` triggers `Endpoint.network_change()`. Default-route and home-router metadata are unavailable in this proof. Simulator success does not establish device background behavior, route migration, NAT traversal, or production readiness.

## Checks

```sh
cargo test --manifest-path apps/ios/modules/st-fabric/rust/Cargo.toml --locked
FABRIC_BIN=/path/to/fabric cargo test --manifest-path apps/ios/modules/st-fabric/rust/Cargo.toml --locked -- --ignored
cargo clippy --manifest-path apps/ios/modules/st-fabric/rust/Cargo.toml --locked --all-targets -- -D warnings
```

The independent byte test checks framing and malformed input. The pinned-daemon check verifies unknown-node and missing-grant denial, a half-closed request, a 5 MiB response that would stall without Acks, and explicit stop delivering EOF to an idle upstream. Run `npm run typecheck` and `npm test` in `apps/ios` for the app checks.

## Recorded simulator result

On 2026-10-04, an iOS 27 simulator running the Debug native app reached a real isolated st member's paired-only Unix gateway through `demo-client/0`. The unchanged TypeScript client first received an unpaired refusal, then completed pairing and received `st3.client.v0` capabilities for a `person/demo` session. The screen reported fabric `0.2.30+8bd9017` and iroh `1.0.2`. The Keychain NodeID remained the same across app relaunches. Only the exact test-service grant was added; it was removed and peers reloaded after verification. The proof closed its endpoint after the response. The isolated daemon's short detached-session TTL bounds forced teardown during setup retries.

Both device and simulator static libraries and their XCFramework built. Rust fixtures, pinned-daemon checks and clippy passed; app typecheck and 32 tests passed. A physical-device run, network migration, relay/NAT behavior and the broader capabilities listed in the protocol document remain to be proved before adopting this carrier.
