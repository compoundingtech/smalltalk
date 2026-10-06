# Fabric development proof

This local Expo module provides a temporary loopback HTTP listener backed by an app-owned iroh endpoint. Each accepted TCP connection opens a fresh direct service-ALPN fabric session. The ordinary TypeScript client then uses that listener without a transport change. Normal gateway entry still rejects loopback, and the proof never saves its listener, pairing credential or gateway configuration.

The carrier is opt-in: normal pod installation compiles `StFabricDisabled.swift` and needs neither Rust nor an XCFramework. Even an enabled carrier rejects operations in Release. The Debug entry is a `com.compoundingtech.smalltalk.starter://fabric-proof` link, separate from ordinary pairing. Closing the proof returns to the saved gateway; entering the background stops its endpoint.

## Build

Install Rust targets `aarch64-apple-ios` and `aarch64-apple-ios-sim`, then from `apps/ios`:

```sh
sh modules/st-fabric/build.sh
pnpm exec expo prebuild --platform ios --no-install
ST3_FABRIC_PROOF=1 pnpm run pods
```

Build Debug for an arm64 simulator of your own (`ARCHS=arm64 ONLY_ACTIVE_ARCH=YES`); the proof framework has no x86_64 simulator slice. The script targets iOS 16.4, including its C/assembly dependencies, and writes the XCFramework under `ios/build` inside the pod root. On a shared build host, wait until `pgrep -x xcodebuild` finds no process before starting Xcode. Generated static libraries and the device/simulator XCFramework are ignored; do not commit them. To restore the default build, reinstall pods without `ST3_FABRIC_PROOF`.

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

The independent byte test checks framing and malformed input. The pinned-daemon check verifies unknown-node and missing-grant denial, a half-closed request, a 5 MiB response that would stall without Acks, and explicit stop delivering EOF to an idle upstream. After the root frozen install, run `pnpm typecheck` and `pnpm test` in `apps/ios` for the app checks.

## Recorded simulator result

On 2026-10-04, an iOS 27 simulator running the Debug native app reached a real isolated st member's paired-only Unix gateway through `demo-client/0`. The unchanged TypeScript client first received an unpaired refusal, then completed pairing and received `st3.client.v0` capabilities for a `person/demo` session. The screen reported fabric `0.2.30+8bd9017` and iroh `1.0.2`. The Keychain NodeID remained the same across app relaunches. Only the exact test-service grant was added; it was removed and peers reloaded after verification. The proof closed its endpoint after the response. The isolated daemon's short detached-session TTL bounds forced teardown during setup retries.

Both device and simulator static libraries and their XCFramework built. Rust fixtures, pinned-daemon checks and clippy passed; app typecheck and 32 tests passed. A physical-device run, network migration, relay/NAT behavior and the broader capabilities listed in the protocol document remain to be proved before adopting this carrier.

## Full-client phone trial

Add `client=1` to the Debug proof pairing link to use the normal Home, conversation and other screens with a temporary, in-memory client. The ordinary store is unmounted during this trial. No loopback URL, proof credential or projection is saved, and the ordinary signing key is not used or replaced. The temporary pairing uses the gateway's legacy unsigned-device path; it does not prove enrolled action signing. Close the trial to restore the ordinary saved gateway. A full-client pairing needs the message scope (`devices pair --full-control` on the test member). For the v0.2.32 phone trial, start the isolated helper with `FABRIC_EXPECTED_VERSION=0.2.32+e31e53b`, then append `auto`, `direct` or `relay` to its `grant` command. That creates a full-control challenge and writes a separate-app link with `client=1` and the requested path mode. Other versions are rejected.

Use `mode=auto`, `mode=direct` or `mode=relay` on the link. Direct mode removes relay transports; relay mode removes IP transports. Measurements show the actually selected path, not an inference from the requested mode. Native `handshakeMs` measures the complete `Endpoint.connect` wait (including discovery/path setup), excluding endpoint creation and HTTP admission; authenticated setup and message acknowledgement have separate timings. The message timing includes its fence and any existing idempotent retry, not an agent's answer.

Backgrounding stops the endpoint and pauses the feed. Foregrounding creates a fresh native listener, authenticates with the retained in-memory bearer and then opens the feed. It does not resume a fabric session or replay pairing. An unanswered pairing needs a fresh proof link. Leaving the app inactive during a Wi-Fi switch can therefore test reconnect rather than continuous QUIC migration; record which occurred.

The Measurements panel samples native paths and estimated QUIC RTT, process CPU/peak resident memory, interface type, thermal state, battery fraction and lifecycle/window/message events. It uses no sampling timer. History is bounded and contains no credentials, addresses, NodeIDs or message content. Battery fraction is coarse; charging prevents attributing drain. Compare an equal-duration idle and active trial under the same conditions and report the limitation.

For a cellular trial without Metro, regenerate the ignored native project and explicitly prepare a separate offline Debug app:

```sh
cd apps/ios
pnpm exec expo prebuild --platform ios --clean --no-install
node modules/st-fabric/prepare-phone.mjs --offline-debug
# Build the Rust framework, then install enabled pods as described above.
ST3_FABRIC_PROOF=1 pnpm run pods
# Use Debug, FORCE_BUNDLING=1, and leave SKIP_BUNDLING unset.
```

This local preparation uses `com.compoundingtech.smalltalk.fabricproof` and its own URL scheme, so installation does not replace the ordinary app. Its proof link starts `com.compoundingtech.smalltalk.fabricproof://fabric-proof`. The generated local override forces Debug bundling and selects `ST3_FABRIC_OFFLINE_DEBUG=1`; Metro then omits only Expo's devtools message socket, which rejects embedded Debug bundles. Native and JavaScript Debug guards stay enabled; Release still rejects the carrier. Signing and provisioning remain local. See [the phone proof report](../../../../docs/st3/ios-fabric-phone-proof.md) for measured results and pending device trials.
