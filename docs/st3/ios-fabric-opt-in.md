# iOS over Fabric as an opt-in, and phone SLOs

Decided with the owner on 2026-10-10: take the fabric phone proof ([protocol](ios-fabric-protocol.md),
[phone proof](ios-fabric-phone-proof.md)) into the ordinary iOS app as an opt-in setting, opt the
owner's phone in, and have the app measure and report service-level objectives from the phone's
side. Tailscale and the LAN keep working for everyone else. The owner accepts re-pairing the phone or
reinstalling the app.

## What exists

The fabric carrier is on `main` as a separate offline Debug app. A native module (`st-fabric`: an
iroh endpoint, a loopback HTTP listener, the pinned direct wire) opens a fresh fabric session for
each local TCP connection, and the unchanged TypeScript client talks to that listener. It passed on
an isolated member and on a live member over Wi-Fi: Home, a conversation, a message
acknowledged, background and foreground, grant refusal.

It is not in the ordinary app:

- it is compiled out of the default build (`StFabricDisabled.swift`), and both the native module and
  `dialFabric` refuse to run outside Debug (`__DEV__`);
- it is entered by a one-off `fabric-proof` link, keeps its credential in memory, and uses the
  gateway's legacy unsigned-device pairing path, so it never enrolled action signing;
- the member side is set up by hand (`fabric expose`, `fabric add PHONE_NODE_ID … --allow`);
- cellular, VPN-independence, seamless migration, matched energy cost and Release size are unproven.

## What changes

### 1. An opt-in setting in the ordinary app

Fleet › connection gets a **Carrier** choice: *Tailscale or LAN* (today, the default) or *Fabric
(experimental)*. Opting in saves a fabric target beside the saved gateway: the member's node ID, the
service name and an optional address hint. It never saves a loopback URL. The app accepts only the
exact address of its own running bridge, as the proof does.

- **A device already paired to that member keeps its bearer and signing key.** The fabric listener
  reaches the same paired-only client gateway, so opting in is one setting and a grant, not a new
  pairing. A new pairing over Fabric uses the same pairing form with a fabric target in the link,
  and enrolls action signing as an ordinary pairing does.
- **Fallback is explicit.** Fabric first; if Fabric cannot connect (no grant, member down, no route)
  the app uses the saved Tailscale or LAN route and says which route is live in Fleet › connection,
  as stui does. A refusal by the member (the grant is gone, 403) is a refusal and is shown as one.
- **Lifecycle.** A fresh native listener per foreground, stopped on background, retained bearer,
  never a replayed mutation, as in the proof. A transport loss closes the connection and never retries
  a mutation.
- **Revocation and identity.** The phone's iroh identity stays in the Keychain, this device only,
  separate from the bearer and signing keys. Removing the phone's grant on a member ends its admitted
  sessions (fabric 0.2.31 and later).

### 2. The native module in every build

The module is linked in the ordinary build and the Debug/Release guards go; the setting is the
switch. The Rust XCFramework (device and simulator slices) builds from `modules/st-fabric/build.sh`
on the Apple host. The debug bundle grew 27.8 MiB with the carrier. Measured Release (dev-signed, not
App Store thinned): 49,625,899 bytes with the bridge against 31,856,010 without, so the bridge adds
17,769,889 bytes (about 16.9 MiB); zipped, it adds 5,145,126 bytes (about 4.9 MiB). The member-side fabric pin stays explicit
(0.2.32+e31e53b today); a change to the direct wire needs the byte and daemon interoperability
checks again.

### 3. Members

Each member that should serve phones exposes only its paired-only client socket
(`client_gateway_socket`, never `st3.sock`) as one fabric service and grants the phone's public node
ID that one service. Operations does this the way it does the terminal exposure: one host at a time,
a script with a revoke counterpart, a receipt per host. The phone shows its node ID in Fleet ›
connection for the owner to send.

## Phone SLOs

The phone measures what the person feels, from the phone, and the daemon keeps it with the daemon's
own SLOs. The daemon already has `slo/targets.toml`, 1-minute, 5-minute and 1-hour windows on
`GET /v1/client/request-latency`, and `slo/NAME` lines in `st doctor`; client-observed targets join
them rather than a second system.

| Target | What it times | First aim |
| --- | --- | --- |
| `ios-open-to-live` | foreground to the first live Home snapshot | p99 3 s |
| `ios-connect` | opening the carrier to authenticated (Fabric: the whole `Endpoint.connect` and admission; Tailscale: first authenticated read) | p99 1.5 s |
| `ios-message-ack` | send pressed to the member's acknowledgement, including the fence | p99 500 ms |
| `ios-conversation-open` | open a conversation to its first snapshot | p99 1 s |
| `ios-terminal-open` | open a terminal to its first screen | p99 1.5 s |
| `ios-recover` | the connection dropping (network change, resume) to a live feed again | p99 10 s |
| `ios-live-share` | share of foreground time with a live feed | at least 99% |

These first aims are starting points for the owner to tighten once a week of real numbers exists.
Every sample carries the carrier (`fabric` or `tailscale`) and, for Fabric, the selected path
(`direct` or `relay`), so the two routes can be compared on the same phone.

**Recording.** A small tested module times the points above where they already happen in the app (the
feed, the store's send, conversation and terminal opens, the foreground gate). It keeps a bounded
ring on the device: counts and a sparse histogram per target, per route, per hour. It holds no
message content, no addresses, no node IDs and no credentials, and it costs no timer of its own.

**Reporting.** On foreground, on background, and at most once an hour while the app stays open, the
app sends its unreported hours to the member it is connected to, as one authenticated client action
(`client.observations`, the same signing and idempotency as any action). The daemon merges the
samples into the same windows as its own targets under `client/ios/NAME`, shows them in `st doctor`
and in `request-latency`, and appends one line per report to a bounded file in its state directory
(`client-observations.jsonl`) so an hourly report can read the history after a restart. The phone
keeps an unreported hour until a member accepts it, and drops it after seven days. A member that
refuses or cannot be reached never blocks the app.

**Retrieval.** `st doctor` and `GET /v1/client/request-latency` for the live windows; the JSONL file
for history; the factory's hourly SLO report reads the same endpoint it reads for the daemon's own
targets.

## Order of work, and who does what

1. This design, and the SLO targets in `slo/targets.toml` with the daemon's ingest (`stui` lane;
   server side and tests on Linux).
2. The app: the opt-in setting, the saved fabric target, the carrier selection in the store, the
   recorder and the reporter, with their tests (`stui` lane; Node tests on Linux).
3. The Apple host: build the XCFramework, link it in the ordinary build, measure the Release size,
   install on the owner's phone (the Apple-host app builders; the owner reinstalls).
4. Operations: expose the client gateway and grant the owner's phone on the members the owner chooses.
5. The owner: send the phone's node ID, opt in, and run the cellular and Wi-Fi trials.

## What is still unproven

Cellular, a route that does not run over Tailscale, continuous migration across a network change,
matched unplugged energy cost, Release and download size, relay availability, and interrupted or
unknown mutation outcomes. The phone SLOs exist so that the owner's daily use answers these with
numbers instead of a trial.
