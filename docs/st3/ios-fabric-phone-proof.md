# iOS fabric phone proof

This development trial extends the simulator spike in [PR #1242](https://github.com/compoundingtech/smalltalk/pull/1242). Its decision is whether to develop a fabric carrier further. It is opt-in and does not replace Tailscale.

## Evidence and status

The earlier simulator reached a real isolated member's client Unix gateway, refused unpaired capabilities, paired, and read `st3.client.v0` capabilities. Its proven fabric pin remains v0.2.30 (`8bd9017a79f4aaa2b25a33321bd7daed6a0acaa6`), with iroh 1.0.2. Device and simulator static libraries and the XCFramework built. That spike is not a physical-device or network-migration result.

For this trial, fabric v0.2.32 is source-pinned at [`e31e53b976bc1f2adad123c8a235c73c72868c96`](https://github.com/compoundingtech/fabric/tree/e31e53b976bc1f2adad123c8a235c73c72868c96). Its lock still selects iroh 1.0.2, and its [direct wire document](https://github.com/compoundingtech/fabric/blob/v0.2.32/docs/tunnel-wire.md) is unchanged from v0.2.31. The isolated byte/admission/half-close/5 MiB flow-control/explicit-stop check passed against `0.2.32+e31e53b`; the large response used direct-only mode, and a separate half-closed request/reply selected the forced relay path. Both unknown-node and missing-grant refusals were recorded as admission failures. Successful grant revocation closes admitted sessions with 403 as described in the [protocol](ios-fabric-protocol.md). This does not yet establish compatibility on a physical phone.

Both default and enabled device Debug builds compile and embed identical offline JavaScript (SHA-256 `4700194d2ad5a2edd091450b4814800dddc2ab35d2295f4005e63e7b8c529bc9`). The default binary has no carrier C exports; the enabled binary has all six. The unsigned bundle totals are 102,521,237 and 131,636,429 bytes.

The full-client isolated simulator UI trial also passed: live Home, a conversation snapshot, a unique test message acknowledged by the member, background/foreground read authentication with the retained bearer and restored conversation, and exit to the ordinary pairing screen. That simulator message took 74.64 ms including its fence and any existing retry; it is not a phone latency result. A second UI trial removed the admitted peer and reloaded grants while the feed was live: the app reported an admission refusal and did not dial again after foregrounding. All five temporary test-device pairings were revoked, the exact peer removed, one-time links deleted, isolated daemons stopped, and the dedicated simulator shut down. App typecheck and 31 tests, Rust byte/direct/relay/admission checks and clippy pass.

A separate offline phone package has been signed with a local development profile and its complete code signature verified. No signing or provisioning material is committed.

No physical phone trial has run. The owner lifted the display hold on October 5; simulator UI work and phone trials may proceed. A phone run needs the owner's unlocked, available device; no production peer grant has been added during preparation.

| Measurement | Result |
| --- | --- |
| Phone Home and live conversation | Pending physical device |
| Phone message acknowledged by member | Pending physical device |
| Direct handshake/setup time | Pending physical device |
| Relayed handshake/setup time | Pending physical device |
| Wi-Fi/cellular switch | Pending physical device; distinguish foreground migration from reconnect after inactivity |
| Background/foreground cycle | Pending physical device; isolated simulator restored the live conversation, and unit checks cover interrupted pairing |
| Battery cost | Pending matched physical trials; battery fraction cannot establish energy attribution |
| Binary-size cost | Unsigned arm64 device Debug bundle: 97.77 MiB default, 125.54 MiB enabled; +27.77 MiB (29,115,192 bytes). Identical embedded JavaScript. This is an installed Debug footprint, not a Release or App Store download estimate. |

## Trial procedure

Use the [separate offline Debug app](../../apps/ios/modules/st-fabric/README.md#full-client-phone-trial). Keep signing material and populated pairing links out of the repository and logs. First verify the new build against an isolated daemon with a throwaway `FABRIC_HOME`. Then expose exactly the member's paired-only client Unix socket under one unused service name. Inspect peer names before adding the phone's public NodeID under an unused alias with only that exact service grant, and check `reload-peers` succeeds. Do not expose the privileged daemon socket.

Use a short-lived full-control pairing for this trial. Open a link with `client=1`. Verify Home's live snapshot, follow the owner's chosen test conversation, and send one explicitly authorized test message. Record the member acknowledgement once and inspect its idempotent action result. Avoid unrelated fleet controls. The proof uses the gateway's legacy unsigned-device pairing path; it does not enroll, read or replace the ordinary app's action-signing key.

Run separate direct-only and relay-only trials. The native path sample must confirm the requested path; a failed or unknown path is not a timing result. The native `handshakeMs` measures the whole `Endpoint.connect` wait, including discovery/path setup; it is a connection measurement rather than a pure TLS handshake measurement. Record multiple cold connection attempts and keep endpoint startup/authentication time separate from QUIC handshake time. Record median and range with sample count. Message timing includes the fresh fence and any existing idempotent retry and ends at member acknowledgement, not an agent reply.

Keep the app foregrounded for a network-switch trial when possible. Record interface and selected path before and after. If iOS reports inactive/background, the proof stops its endpoint; label the result a fresh reconnect, not seamless migration. Follow the same conversation after recovery, check for a new snapshot, and confirm no offline message was automatically sent. Separately background the paired app, then foreground it and check the new native listener and restored conversation.

Sample battery/CPU/thermal state before and after equal-duration idle and active trials. Keep brightness, connectivity, duration, charge state and workload comparable. Do not infer battery drain from a plugged-in build or simulator. Record USB charging and percentage granularity; use device energy instrumentation before making a production battery claim.

Afterward, close the proof, revoke the temporary paired device, remove the exact phone service grant and verify peer reload. Remove the ephemeral exposure and verify the active trial sessions end. Stop isolated daemons and delete one-time pairing links. Leave the ordinary Tailscale app and saved route available.

## Before replacing Tailscale

The decision needs measured physical direct/relay behavior, network recovery, background lifecycle and battery/size cost. Further work includes enrolled action signing for the temporary client, terminal and upload stress, interrupted/unknown mutation outcomes, physical stress of admission revocation while the full client is live, device key recovery and grant revocation, relay availability, session reaping under abrupt suspension, privacy of endpoint discovery, and the cost of treating the phone as an ordinary fabric peer. The direct ALPN, Hello/Data/Ack/Close framing and admission/window semantics remain a versioned compatibility dependency. Re-run byte and daemon interoperability checks for each member artifact before advancing its pin.
