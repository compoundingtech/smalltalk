# Fabric carrier

This local Expo module is the phone's opt-in fabric carrier: an app-owned iroh endpoint and a temporary loopback HTTP listener. Each accepted TCP connection opens a fresh direct service-ALPN fabric session. The ordinary TypeScript client then uses that listener without a transport change. Normal gateway entry still rejects loopback, and a listener, a pairing credential or gateway configuration is never saved.

It is opt-in at build time and at run time. Normal pod installation compiles `StFabricDisabled.swift` and needs neither Rust nor an XCFramework; `ST3_FABRIC=1` (older name `ST3_FABRIC_PROOF=1`) links the bridge. A linked bridge does nothing until the person turns Fabric on in the app (see [the app README](../../README.md#fabric-an-opt-in-carrier)); it makes no identity and starts no network monitor before then. The path monitor runs only while a bridge is open. The Debug entry `com.compoundingtech.smalltalk.starter://fabric-proof` stays development-only and is separate from ordinary pairing.

## Build

Install Rust targets `aarch64-apple-ios` and `aarch64-apple-ios-sim`, then from `apps/ios`:

```sh
sh modules/st-fabric/build.sh
pnpm exec expo prebuild --platform ios --no-install
ST3_FABRIC=1 pnpm run pods
```

Build Debug for an arm64 simulator of your own (`ARCHS=arm64 ONLY_ACTIVE_ARCH=YES`); the proof framework has no x86_64 simulator slice. The script targets iOS 16.4, including its C/assembly dependencies, and writes the XCFramework under `ios/build` inside the pod root. On a shared build host, wait until `pgrep -x xcodebuild` finds no process before starting Xcode. Generated static libraries and the device/simulator XCFramework are ignored; do not commit them. To restore the default build, reinstall pods without `ST3_FABRIC` (or its older name `ST3_FABRIC_PROOF`).

## Isolated proof

Use the installed `st` and a pinned **fabric 0.2.41+84c3208** binary, even if the host's default fabric is another version; the helper grants the phone inbound-only, which needs 0.2.41 or later. From the repository root, keep this helper running in its own terminal:

```sh
node apps/ios/modules/st-fabric/proof-member.mjs /path/to/st /path/to/fabric
```

It starts a test member and fabric daemon under a fresh `/tmp/st-fabric-proof-*` directory, exposes only `client.sock` as `demo-client/0`, and prints a descriptor path. Open its `identity-link.txt` in the Debug app to read the public phone NodeID. You can also set `EXPO_PUBLIC_ST3_FABRIC_PROOF_LINK` when starting a local Metro server for a headless simulator launch. Then grant only that service, inbound-only, and create a test pairing challenge:

```sh
node apps/ios/modules/st-fabric/proof-member.mjs grant /tmp/st-fabric-proof-example/member.json PHONE_NODE_ID
```

The grant is `fabric add PHONE_NODE_ID demo-phone --inbound-only --allow demo-client/0`: the member serves the phone when it connects and never dials, probes or announces to it, and `fabric status` shows it as `inbound-only`. Start the helper with `FABRIC_PROOF_APP=separate` to write links for the separate offline Debug app.

Open the privately written `pair-link.txt` in the app. The temporary `St3Client` completes pairing and reads capabilities through fabric. Pairing uses its own temporary device key value, independent of the iroh identity; no action-signing key or bearer is reused as a transport key. Do not publish the link or put its code in logs. Use an isolated, localhost Metro server if injecting a pairing link through an environment variable.

After verification, remove `demo-phone` using `fabric --home TEST_HOME remove demo-phone`, run `reload-peers`, and stop the helper. At the proof's pinned fabric 0.2.41+84c3208, a successful reload ends the admitted sessions whose grant or peer was removed, attached or detached ([trust after admission](https://github.com/compoundingtech/fabric/blob/v0.2.41/docs/tunnel-wire.md#trust-after-admission)), closing direct connections with code 403 and the corresponding admission refusal reason. Treat that as a refusal; a failed reload ends no sessions. Stopping the app bridge and the isolated daemon closes anything left. The test daemon uses a 30 second detached-session TTL, while the pinned daemon's default is 15 minutes.

Revoke the temporary paired device after the trial, remove any temporary service exposure and verify peer reload, and delete one-time pairing links. Keep the ordinary app's saved gateway and action-signing key. For a live-member or physical-phone trial, use only the chosen conversation and explicitly authorized test messages.

## Wire and limits

Fabric is pinned to tag `v0.2.41`, commit `84c3208b3d1c82e4a036640aed330473a74cf105`, whose [`docs/tunnel-wire.md`](https://github.com/compoundingtech/fabric/blob/v0.2.41/docs/tunnel-wire.md) describes this wire; its frame bytes are those of v0.2.30. iroh is pinned to `1.0.2`, the version fabric v0.2.41 itself locks, with this crate's separate Cargo lock. See the maintained [iOS Fabric protocol](../../../../docs/st3/ios-fabric-protocol.md) for framing, admission and error codes, [`rust/src/wire.rs`](rust/src/wire.rs) for the codec and [`rust/src/bridge.rs`](rust/src/bridge.rs) for admission. A refusal is close code 403; since v0.2.41 its reason, and an Error frame's text, start with a `fabric:<code> ` token, and the bridge records that code (never the rest, which can name a member path) beside each attempt's result in its measurements: `refused` with `peer_not_in_allow_list` or `peer_not_permitted_for_service`, `rejected` with a fault such as `service_unavailable`. A change to direct exposure ALPN, Hello/Data/Ack/Close bytes, acknowledgment semantics, trust/grant checks or error codes can break the adapter. This does not implement `fabric/mux/2` or session resume.

The Keychain holds a random 32 byte iroh secret under a separate service/account with `WhenUnlockedThisDeviceOnly` accessibility. JavaScript receives only its public NodeID. Native dial validates that an optional EndpointAddr hint matches the requested NodeID. The only accepted loopback URL is the newly returned native listener; user-entered and saved loopback addresses remain invalid.

There are at most 16 concurrent local sessions. Writes stop at the pinned 4 MiB unacknowledged window (with one 8 KiB read overshoot); received bytes are acknowledged after delivery. Half-closes carry final offsets, and orderly completion sends the final Ack before closing QUIC. Stop attempts a bounded graceful drain, then forces teardown if the peer or local consumer stalls. A transport loss closes the local socket and never retries a mutation.

The two local vendored patches make otherwise macOS-only networking code compile for iOS; their `ST-PATCH.md` files describe the changes and limitations. iOS enumerates interfaces via netdev, and Swift's `NWPathMonitor` triggers `Endpoint.network_change()`. Default-route and home-router metadata are unavailable in this proof. Simulator success does not establish device background behavior, route migration, NAT traversal, or production readiness.

## Checks

```sh
cargo test --manifest-path apps/ios/modules/st-fabric/rust/Cargo.toml --locked
FABRIC_BIN=/path/to/fabric cargo test --manifest-path apps/ios/modules/st-fabric/rust/Cargo.toml --locked -- --ignored
cargo clippy --manifest-path apps/ios/modules/st-fabric/rust/Cargo.toml --locked --all-targets -- -D warnings
```

The independent byte test checks framing and malformed input. The pinned-daemon check grants the phone inbound-only and verifies unknown-node and missing-grant denial with their codes, an unreachable service's `service_unavailable` Error frame, a removed grant ending an attached session as a refusal, a half-closed request, a 5 MiB response that would stall without Acks, explicit stop delivering EOF to an idle upstream, and, with the member probing every second, probes of an ordinarily granted peer but none of the inbound-only phone. It needs fabric 0.2.41 or later; set `FABRIC_EXPECTED_VERSION` for a build other than `0.2.41+84c3208`. The real-member check, `real_member_typed_errors`, runs against a helper started as described under [Isolated proof](#isolated-proof): set `ST_FABRIC_PROOF_MEMBER` to its `member.json` and run `cargo test --manifest-path apps/ios/modules/st-fabric/rust/Cargo.toml --locked real_member_typed_errors -- --ignored --nocapture`. It grants a fresh phone inbound-only, reads capabilities and st's refusal of an unpaired request through the bridge, and prints the raw 403 close reasons, an Error frame's code and the `(access revoked)` close from that member, then removes its grant. After the root frozen install, run `pnpm typecheck` and `pnpm test` in `apps/ios` for the app checks.

## Several members

Fleet › carrier keeps an ordered list of members. The app walks it in that order, 4 seconds per member and 10 seconds in all; a member that dropped is redialed after 1, 2 and 5 seconds before the walk moves on. A refusal stops the walk at that member, names it by its saved name and says what was refused; nothing after it is dialed until the person picks another member. Each member other than the device's own is paired on its own and gets only its own credential; the walk skips a member not paired yet. See [several members](../../../../docs/st3/ios-fabric-protocol.md#several-members-an-ordered-list) for the rules and the older-build compatibility.

## Full-client phone trial

Add `client=1` to the Debug proof pairing link to use the normal Now, conversation and other screens with a temporary, in-memory client. The ordinary store is unmounted during this trial. No loopback URL, proof credential or projection is saved, and the ordinary signing key is not used or replaced. The temporary pairing uses the gateway's legacy unsigned-device path; it does not prove enrolled action signing. Close the trial to restore the ordinary saved gateway. A full-client pairing needs the message scope (`devices pair --full-control` on the test member). For a phone trial, append `auto`, `direct` or `relay` to the helper's `grant` command. That creates a full-control challenge and writes a separate-app link with `client=1` and the requested path mode. The helper rejects fabric versions other than its pin.

Use `mode=auto`, `mode=direct` or `mode=relay` on the link. Direct mode removes relay transports; relay mode removes IP transports. Measurements show the actually selected path, not an inference from the requested mode. Native `handshakeMs` measures the complete `Endpoint.connect` wait (including discovery/path setup), excluding endpoint creation and HTTP admission; authenticated setup and message acknowledgement have separate timings. The message timing includes its fence and any existing idempotent retry, not an agent's answer.

Backgrounding stops the endpoint and pauses the feed. Foregrounding creates a fresh native listener, authenticates with the retained in-memory bearer and then opens the feed. It does not resume a fabric session or replay pairing. An unanswered pairing needs a fresh proof link. Leaving the app inactive during a Wi-Fi switch can therefore test reconnect rather than continuous QUIC migration; record which occurred.

The Measurements panel samples native paths and estimated QUIC RTT, process CPU/peak resident memory, interface type, thermal state, battery fraction and lifecycle/window/message events. It uses no sampling timer. History is bounded and contains no credentials, addresses, NodeIDs or message content. Battery fraction is coarse; charging prevents attributing drain. Compare an equal-duration idle and active trial under the same conditions and report the limitation.

For a cellular trial without Metro, regenerate the ignored native project and explicitly prepare a separate offline Debug app:

```sh
cd apps/ios
pnpm exec expo prebuild --platform ios --clean --no-install
node modules/st-fabric/prepare-phone.mjs --offline-debug
# Build the Rust framework, then install enabled pods as described above.
ST3_FABRIC=1 pnpm run pods
# Use Debug, FORCE_BUNDLING=1, and leave SKIP_BUNDLING unset.
```

This local preparation uses `com.compoundingtech.smalltalk.fabricproof` and its own URL scheme, so installation does not replace the ordinary app. Its proof link starts `com.compoundingtech.smalltalk.fabricproof://fabric-proof`. The generated local override forces Debug bundling and selects `ST3_FABRIC_OFFLINE_DEBUG=1`; Metro then omits only Expo's devtools message socket, which rejects embedded Debug bundles. Native and JavaScript Debug guards stay enabled; Release still rejects the carrier. Signing and provisioning remain local.
