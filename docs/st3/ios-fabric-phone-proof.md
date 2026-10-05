# iOS fabric phone proof

This development trial extends the simulator spike in [PR #1242](https://github.com/compoundingtech/smalltalk/pull/1242). Its decision is whether to develop a fabric carrier further. It is opt-in and does not replace Tailscale.

## Evidence and status

The earlier simulator reached a real isolated member's client Unix gateway, refused unpaired capabilities, paired, and read `st3.client.v0` capabilities. Its proven fabric pin remains v0.2.30 (`8bd9017a79f4aaa2b25a33321bd7daed6a0acaa6`), with iroh 1.0.2. Device and simulator static libraries and the XCFramework built. That spike is not a physical-device or network-migration result.

For this trial, fabric v0.2.32 is source-pinned at [`e31e53b976bc1f2adad123c8a235c73c72868c96`](https://github.com/compoundingtech/fabric/tree/e31e53b976bc1f2adad123c8a235c73c72868c96). Its lock still selects iroh 1.0.2, and its [direct wire document](https://github.com/compoundingtech/fabric/blob/v0.2.32/docs/tunnel-wire.md) is unchanged from v0.2.31. The isolated byte/admission/half-close/5 MiB flow-control/explicit-stop check passed against `0.2.32+e31e53b`; the large response used direct-only mode, and a separate half-closed request/reply selected the forced relay path. Both unknown-node and missing-grant refusals were recorded as admission failures. Successful grant revocation closes admitted sessions with 403 as described in the [protocol](ios-fabric-protocol.md). The physical trial below also passed against this member artifact.

Both default and enabled device Debug builds compile and embed identical offline JavaScript (SHA-256 `4700194d2ad5a2edd091450b4814800dddc2ab35d2295f4005e63e7b8c529bc9`). The default binary has no carrier C exports; the enabled binary has all six. The unsigned bundle totals are 102,521,237 and 131,636,429 bytes.

The full-client isolated simulator UI trial also passed: live Home, a conversation snapshot, a unique test message acknowledged by the member, background/foreground read authentication with the retained bearer and restored conversation, and exit to the ordinary pairing screen. That simulator message took 74.64 ms including its fence and any existing retry; it is not a phone latency result. A second UI trial removed the admitted peer and reloaded grants while the feed was live: the app reported an admission refusal and did not dial again after foregrounding. All five temporary test-device pairings were revoked, the exact peer removed, one-time links deleted, isolated daemons stopped, and the dedicated simulator shut down. App typecheck and 31 tests, Rust byte/direct/relay/admission checks and clippy pass.

A separate offline phone package has been signed with a local development profile and its complete code signature verified. No signing or provisioning material is committed.

The physical Wi-Fi trial passed on October 5, 2026, on an iPhone Air running iOS 27.0.1, using the separate offline Debug app from [PR #1412](https://github.com/compoundingtech/smalltalk/pull/1412) (tested commit `585c3af44a9c1474231281714b6a56d51ffd78c6`, merged as `48bd002e405bfe7499346b4c0bf2fe94c779be04`). It first passed against an isolated member, then against one live member's paired-only client gateway. Home loaded, the conversation received a live snapshot, and one authorized message was acknowledged. The owner ended the apartment session and deferred cellular and unplugged energy measurements to a later coffee-shop trial controlled from the roaming laptop.

| Measurement | Physical Wi-Fi result |
| --- | --- |
| Home and live conversation | Passed on isolated and live members |
| Message acknowledged by member | 80.85 ms live; 183.70 ms isolated. Includes the fresh fence and existing idempotent retry; ends at member acknowledgement. One live message sample, not a latency distribution. |
| Direct connection setup | Fresh endpoints: 11, 7, 30 ms; median 11 ms, range 7–30 ms, n=3. Native selected path was direct. |
| Forced relay connection setup | Fresh endpoints: 172, 140, 137 ms; median 140 ms, range 137–172 ms, n=3. IP transports disabled and native selected path was relay. |
| Background/foreground | Retained bearer, new listener, read authentication and restored live conversation; two additional cycles passed in each direct/relay trial. |
| Wi-Fi off, then restored | No usable OS path while Wi-Fi was disconnected (`networkType=other`, `networkSatisfied=false`); reconnect and foreground retry failed. After Wi-Fi restoration, authentication took 310.46 ms and a new live feed and conversation snapshot arrived. No offline message was sent. Cellular success is unproven. |
| Battery cost | USB attached and charging throughout; observed 90–95%, nominal thermal state. No valid battery-drain or energy attribution. Matched unplugged measurements deferred. |
| Binary-size cost | Unsigned arm64 device Debug bundle: 97.77 MiB default, 125.54 MiB enabled; +27.77 MiB (29,115,192 bytes). Identical embedded JavaScript. Installed Debug footprint; Release and App Store download size remain unmeasured. |

Connection times measure the whole `Endpoint.connect` wait, including discovery and path setup, excluding endpoint binding and HTTP admission. These are fresh endpoint samples; OS discovery/DNS caches were not cleared. Each endpoint also made a second connection, whose warm timings are excluded from the table. Control Center made the app inactive during the network-switch diagnostic, so recovery was a fresh reconnect; seamless foreground migration was not proven. Cellular permission was not verified, and the unsatisfied OS path does not establish a fabric transport failure.

The phone VPN remained enabled, and the gateway's address hint included a tailnet address. The direct path samples do not identify the underlying IP route and therefore do not prove VPN independence. Forced relay does establish use of fabric's relay transport, while the underlying VPN remained possible. The coffee-shop follow-up must distinguish these facts before drawing a Tailscale replacement conclusion.

Cleanup was verified: the live phone peer and ephemeral exposure were removed, peer reload succeeded, all eight temporary live-device pairings and the isolated pairing were revoked, populated one-time links were deleted, both isolated daemons stopped, and Wi-Fi was restored. The proof process was terminated or absent. The ordinary app, saved Tailscale route and action-signing key were preserved. No further phone work is required for this completed Wi-Fi session.

Metadata-only evidence (no keys, bearers, node IDs, endpoint addresses or message content) is pinned in the mission graph at `doc/ios-fabric-phone-results-2026-10-05@fb76e62623680c7d6989899cf07b9841a9837a5efcab521a2a64059cda3beaaa`. It includes the native path samples, lifecycle events, message acknowledgement timing, network diagnostic and cleanup summary.

## Trial procedure

Use the [separate offline Debug app](../../apps/ios/modules/st-fabric/README.md#full-client-phone-trial). Keep signing material and populated pairing links out of the repository and logs. First verify the new build against an isolated daemon with a throwaway `FABRIC_HOME`. Then expose exactly the member's paired-only client Unix socket under one unused service name. Inspect peer names before adding the phone's public NodeID under an unused alias with only that exact service grant, and check `reload-peers` succeeds. Do not expose the privileged daemon socket.

Use a short-lived full-control pairing for this trial. Open a link with `client=1`. Verify Home's live snapshot, follow the owner's chosen test conversation, and send one explicitly authorized test message. Record the member acknowledgement once and inspect its idempotent action result. Avoid unrelated fleet controls. The proof uses the gateway's legacy unsigned-device pairing path; it does not enroll, read or replace the ordinary app's action-signing key.

Run separate direct-only and relay-only trials. The native path sample must confirm the requested path; a failed or unknown path is not a timing result. The native `handshakeMs` measures the whole `Endpoint.connect` wait, including discovery/path setup; it is a connection measurement rather than a pure TLS handshake measurement. Record multiple fresh endpoint connection attempts and keep endpoint startup/authentication time separate from QUIC handshake time. Record median and range with sample count. Message timing includes the fresh fence and any existing idempotent retry and ends at member acknowledgement, not an agent reply.

Keep the app foregrounded for a network-switch trial when possible. Record interface and selected path before and after. If iOS reports inactive/background, the proof stops its endpoint; label the result a fresh reconnect, not seamless migration. Follow the same conversation after recovery, check for a new snapshot, and confirm no offline message was automatically sent. Separately background the paired app, then foreground it and check the new native listener and restored conversation.

Sample battery/CPU/thermal state before and after equal-duration idle and active trials. Keep brightness, connectivity, duration, charge state and workload comparable. Do not infer battery drain from a plugged-in build or simulator. Record USB charging and percentage granularity; use device energy instrumentation before making a production battery claim.

Afterward, close the proof, revoke the temporary paired device, remove the exact phone service grant and verify peer reload. Remove the ephemeral exposure and verify the active trial sessions end. Stop isolated daemons and delete one-time pairing links. Leave the ordinary Tailscale app and saved route available.

## Before replacing Tailscale

The Wi-Fi results support further development. They do not yet support replacing Tailscale: cellular availability and recovery, VPN-independent routes, seamless foreground migration, matched unplugged energy cost and Release size remain unproven. Further work includes enrolled action signing for the temporary client, terminal and upload stress, interrupted/unknown mutation outcomes, physical stress of admission revocation while the full client is live, device key recovery and grant revocation, relay availability, session reaping under abrupt suspension, privacy of endpoint discovery, and the cost of treating the phone as an ordinary fabric peer. The direct ALPN, Hello/Data/Ack/Close framing and admission/window semantics remain a versioned compatibility dependency. Re-run byte and daemon interoperability checks for each member artifact before advancing its pin.
