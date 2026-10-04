# iOS fabric transport proof: client protocol

This is a source-derived contract for a development-only transport proof. The phone
owns an iroh identity and reaches one member’s paired client gateway through an
app-owned loopback listener. The existing TypeScript client continues to speak
HTTP and WebSocket. Tailscale remains available, and fabric never becomes the
default as part of this proof.

This document specifies the adapter; it does **not** claim an iOS build, a device
connection, or a working embedded dialer. No member configuration was changed for
this protocol step. All example names and IDs below are invented.

## Version and evidence

Read fabric at commit
[`8bd9017a79f4aaa2b25a33321bd7daed6a0acaa6`](https://github.com/compoundingtech/fabric/tree/8bd9017a79f4aaa2b25a33321bd7daed6a0acaa6),
tag/release `v0.2.30`. The installed binary inspected for this step reports
`0.2.30+8bd9017`. Its [Cargo manifest and lock](https://github.com/compoundingtech/fabric/blob/8bd9017a79f4aaa2b25a33321bd7daed6a0acaa6/Cargo.lock)
select **iroh 1.0.2**. Pin `=1.0.2`, the fabric source revision, and the adapter’s
own lockfile for the executable proof. A version banner alone is not an artifact
checksum; record the built adapter and tested member binary hashes when testing.

Small Talk references were inspected at commit
`0c6b10d044f6a8a3b051639b55f53522cc9a9b11`. Relative links describe that checkout;
recheck them when implementing against a newer main.

| Contract | Source at the fabric pin |
| --- | --- |
| Identity, peer representation, exact grants | [`src/config.rs`](https://github.com/compoundingtech/fabric/blob/8bd9017a79f4aaa2b25a33321bd7daed6a0acaa6/src/config.rs): `load_or_create_identity`, `Peer`, `PeerBook::may` |
| Endpoint setup, transport trust, service admission | [`src/daemon.rs`](https://github.com/compoundingtech/fabric/blob/8bd9017a79f4aaa2b25a33321bd7daed6a0acaa6/src/daemon.rs): `build_daemon_endpoint`, `AllowListHook`, `handle_mux_connection`, `handle_mux_stream` |
| Connection ALPN, generation preface, stream routing | [`src/mux.rs`](https://github.com/compoundingtech/fabric/blob/8bd9017a79f4aaa2b25a33321bd7daed6a0acaa6/src/mux.rs): `MUX_ALPN`, `PeerConnections`, `MuxStreamHeader`, `read_admission` |
| Generic exposure framing and resumable byte delivery | [`src/tunnel.rs`](https://github.com/compoundingtech/fabric/blob/8bd9017a79f4aaa2b25a33321bd7daed6a0acaa6/src/tunnel.rs): `Frame`, `write_frame`, `decode_frame`, `attach_stream_inner`, `serve_connection`, `TunnelSession::run_attach`, `write_attach_loop`, `read_attach_loop`, `ServerSessionStore::get_or_create` |
| Service-side interface, distinct from a dialer | [`fabric-service-api`](https://github.com/compoundingtech/fabric/blob/8bd9017a79f4aaa2b25a33321bd7daed6a0acaa6/crates/fabric-service-api/src/lib.rs): `Service`, `PeerStream`, `Access` |

## Identity, addresses, and discovery

Fabric uses `iroh::SecretKey::generate()` and its public key as an `EndpointId`
(also called NodeID). The peer file serializes the ID as 64 hexadecimal
characters. The handshake proves `connection.remote_id()`; names are local
labels and cannot confer permission. The phone must generate its own stable
secret, keep it in device-only secure storage, and show/export only the public
NodeID for the member’s grant. Re-generating the secret changes the phone’s ID
and requires a new grant. This key is separate from both the paired gateway
bearer credential and the app’s existing action-signing key.

The app must be configured with the expected member NodeID and fixed service
name, for example `demo-client/0`. Build an iroh endpoint with
`Endpoint::builder(presets::N0).secret_key(phone_key).bind()`; the daemon uses this
same preset. The phone is an outbound client and needs no fabric control socket,
Unix dial declaration, fleet membership, shell, exec, sync companion, or inbound
service registration.

In the locally inspected iroh 1.0.2 source, `endpoint/presets.rs::N0` configures
Number 0’s default relays, a pkarr address publisher, DNS lookup through
`iroh.link` outside browsers, and a TLS crypto provider. Dial
`EndpointAddr::new(member_id)` to use NodeID discovery. An optional saved
`EndpointAddr` may carry relay/IP hints, but its ID must match the configured
member ID. Hints locate a peer; they do not replace identity verification.
Fabric’s normal setup omits address hints so discovery can follow roaming peers.

Iroh selects direct or relayed paths and can migrate them under a live connection.
The relay carries the encrypted transport; it does not grant a fabric service.
`endpoint.online()` only establishes relay reachability, not member reachability
or service admission. Use bounded connect/admission/Hello attempts and report
the phase that failed. Network changes and suspension still need a device proof;
source inspection alone does not establish iOS viability.

## Member exposure and authorization

Expose exactly the **paired-only** gateway socket, whose router is
[`fabric_router`](../../crates/st3/src/api.rs) and whose configured path is
`client_gateway_socket` (normally `st3-client.sock`). Do not expose `st3.sock`:
that is the privileged local API. A generic socket exposure is sufficient; no
new server-side Rust service is needed.

Illustrative member commands, with host-local paths and a real public ID supplied
only at setup time, through the member’s owner after the real-device pairing
plan has been agreed. An isolated dev daemon with a throwaway `FABRIC_HOME` can
exercise the protocol before live provisioning. Prove it against that isolated
daemon first, then arrange the one member exposure and phone grant; remove the
phone grant after the proof:

```sh
fabric expose demo-client/0 --socket /path/to/private/st3-client.sock --ephemeral
fabric peers
# Choose an unused peer name before adding the phone.
fabric add PHONE_NODE_ID demo-phone --allow demo-client/0
fabric reload-peers
```

For a new phone entry, the equivalent authoritative `peers.toml` content is:

```toml
format = 2

[[peers]]
id = "0123456789abcdef0123456789abcdef0123456789abcdef0123456789abcdef"
name = "demo-phone"
allow = ["demo-client/0"]
```

The example ID is a placeholder. Inspect the resulting entry, particularly if
updating an existing peer: `PeerBook::add_with_allow` preserves existing grants
when the allow argument is omitted. It also replaces any other peer with the same
name: choose an unused alias after inspecting `fabric peers`. A new entry has
`roaming = false`; `fabric add` has no roaming flag. If the isolated experiment
needs `roaming = true`, edit that field explicitly and reload. Do not silently
widen a phone’s grant. The member-side scope is one exposure and this phone’s exact grant only; broader
changes go through operations. `--ephemeral` avoids persisting the exposure;
peer grants still persist and require deliberate cleanup after the proof.

There are two fabric checks before the target socket is opened:

1. `AllowListHook::after_handshake` checks inbound `remote_id()` against the
   loaded peer IDs. An unknown phone is rejected with application code `403`
   and `node is not in fabric allow-list`.
2. On a direct exposure connection, `process_incoming_iroh` calls
   `PeerBook::may(remote_id, service_name)` before accepting the tunnel stream.
   Mux streams perform the same check in `handle_mux_stream`. Generic exposures
   use their full protocol string, here `demo-client/0`, as the permission name.
   Matching is exact. An absent/empty `allow` grants no service. The phone’s
   entry does not grant `shell`, `exec`, `sync`, `echo`, or other exposures.

Admission of `fabric/mux/2` is transport trust, not a grant to every service.
Registered built-ins may map protocol versions to a short service name, and
`Access::Grants` services perform narrower request-level checks; neither exception
applies to this generic gateway exposure. Format 2’s generated `allow_shell` and
`allow_exec` mirrors are not policy. Run `fabric reload-peers` and check its result after changing peer policy.
At this pin, a `PeerBook::load` failure clears the in-memory peer book and
transport allow-list. A later `SyncBook` validation failure occurs before the
new book is installed. Do not infer successful admission from an edited file.
At the pinned v0.2.30, `reload-peers` does not close already attached connections or sessions. Removing
the grant prevents new connections and resumes, but does not cut an already
admitted stream. Close the app’s active carrier sessions as part of proof cleanup;
verify detached sessions drain, expire, or are evicted on the isolated daemon.

Since [v0.2.31](https://github.com/compoundingtech/fabric/blob/v0.2.31/docs/tunnel-wire.md#trust-after-admission),
a successful peer reload ends attached and detached sessions that the new policy
no longer permits. Direct connections close with code 403 and the same admission
reason: `not permitted for service` for a removed grant, or
`node is not in fabric allow-list` for a removed peer. Treat these as refusals,
not transport losses; a failed reload ends no sessions. The wire bytes are
unchanged. This behavior note does not advance the isolated proof's v0.2.30 pin.

After fabric admits the stream, **st still checks pairing and client scopes**.
Fabric does not synthesize a person, device grant, or bearer credential. Complete
normal short-lived gateway pairing through this same carrier if needed. The
native carrier must not insert a privileged credential or bypass gateway checks.

## Direct connection: the selected app dialer

The fabric owner reviewed the source pin and recommends **direct service ALPN**
for this single-service client. This path is implemented by the pinned daemon,
although its own daemon-to-daemon dialer prefers mux. The application ALPN is
exactly the UTF-8 exposure name, here **`demo-client/0`**:

1. Connect to `EndpointAddr::new(member_id)` with ALPN `b"demo-client/0"`.
   An unexposed protocol is absent from the daemon’s advertised ALPNs and fails
   negotiation. Verify the expected peer identity through iroh.
2. The member checks transport trust and the exact service grant after the
   authenticated handshake. A trusted phone missing this grant is closed with
   application code `403` and a reason containing `not permitted for service`.
   This refusal marker is a documented wire contract. Report it as an admission
   problem instead of retrying forever as though the member were offline.
3. Open **one bidirectional stream** for this connection and immediately send a
   tunnel Hello. The member’s `accept_bi()` becomes observable when bytes arrive;
   waiting for the server to send first would deadlock.
4. Read the matching server Hello or Error, then carry HTTP in tunnel Data frames.
   There is no mux generation, stream service header, or mux admission prefix on
   this path. The connection ALPN already names the service.

Map each loopback TCP connection to one direct iroh connection and one tunnel
session. Reuse the app’s **Endpoint**, not a direct connection containing several
logical service streams: this generic direct handler accepts one bidirectional
stream. The proof can close local connections on fabric loss and reconnect at
HTTP/WebSocket client level; it need not implement daemon generation arbitration.
Each new local TCP connection costs a QUIC handshake and a member `tunnel_accept`
log entry, so preserve HTTP keep-alive instead of reconnecting per request.

A trusted phone still receives the daemon’s ordinary peer behavior at this pin:
health probes every 20 seconds while online, backoff while away, and announcements
on peer reload/network change. An outbound-only phone speaking neither
`fabric/mux/2` nor `fabric/echo/0` cannot answer, so it is marked unreachable and
probed on backoff even while its gateway tunnels work. This bounded cost is
accepted for the proof, but measure it. The fabric owner proposes an inbound-only
peer kind, without probing/announcements, before this becomes a lasting client
surface; it does not exist in v0.2.30.

## Mux wire reference: daemon-to-daemon routing

For readers tracing `fabric dial`, the normal daemon connection ALPN is the exact
byte string **`fabric/mux/2`**. On this path the service travels in each logical
stream. This is reference material; the selected app adapter does not emit it.

1. Connect with `fabric/mux/2`.
2. On the **first bidirectional stream**, send the endpoint generation as `u64`
   big-endian. Read the member’s `u64` big-endian generation, then finish the
   preface send half. There is no service header on this preface. Fabric bounds
   the generation exchange at three seconds.
3. On each subsequent bidirectional stream, write
   `[u16 BE service_byte_length][UTF-8 service bytes]`. Valid headers contain
   1–255 bytes. `demo-client/0` is 13 bytes, so its prefix is `00 0d`.
4. Read `[u8 status][u16 BE message_length][message bytes]`. Success is exactly
   `00 00 00`. Status 1 is denial with a reason of at most 4096 bytes; the server
   finishes that send half. Reject malformed responses. The opener bounds the
   answer at three seconds; the server bounds header input at ten seconds.
5. After admission perform the same tunnel Hello and framing as the direct path.

Fabric persists monotonically increasing endpoint generations per bind/recycle,
exchanges both owners’ generations, and replaces stale cached connections. Equal
generations use a NodeID-order tie-break for simultaneous opens. Omitting this
preface or arbitration from a mux client could close duplicate/stale connections
or misroute the first stream. This is why the owner recommends direct ALPN here.
The daemon dialer’s fallback to direct ALPN happens only after explicit rejection
of the mux ALPN, not after denial, timeout, or general connection failure.

## Tunnel framing for a generic exposure

Both direct generic exposures and admitted mux exposures carry a framed tunnel. Every frame is:

```text
[u8 kind][u32 BE payload_length][payload]
```

The maximum payload length is 1,048,576 bytes, including any offset fields. Read
exactly the declared payload; QUIC reads, TCP reads, and WebSocket messages are
not fabric frame boundaries. All integers below are unsigned and big-endian.

| Kind | Payload | Meaning |
| --- | --- | --- |
| 1 Hello | 16-byte session ID + u64 `recv_next` + u8 `resume` | Attach this session and advertise how many peer bytes this side has delivered |
| 2 Data | u64 `offset` + bytes | Bytes starting at this direction’s absolute byte offset |
| 3 Ack | u64 `recv_next` | Cumulative acknowledgement of peer bytes delivered locally |
| 4 Close | u64 `offset` | This direction ends after byte `offset`; drain those bytes before half-closing locally |
| 5 Error | UTF-8 message bytes | Server rejection of Hello/session attachment |

For a fresh local TCP connection generate a random 16-byte session ID. Send
Hello with `recv_next = 0`, `resume = 0` (25-byte payload, header `01 00 00 00 19`).
The server opens the target, then replies with the same session ID, its
`recv_next`, and `resume = 0`. Validate the ID and wait for that reply before
forwarding HTTP. The decoder also accepts an older 24-byte Hello without the
resume byte; the pinned encoder emits 25 bytes.

Offsets start at zero independently in both directions. Advance `recv_next` only
after writing delivered bytes to the local socket. The writer sends an Ack at the
start of each attach and whenever `recv_next` changes, with no delayed-Ack timer:
expect **Ack{0} immediately after the server Hello**. Ack promptly after each local
delivery. Retain unacknowledged outgoing bytes, discard duplicate received
prefixes, and reject gaps (`offset > recv_next`). Older Acks are ignored; the
pinned implementation clamps Acks beyond `send_next` instead of rejecting them.

Acks provide flow control: the member stops reading the target when 4 MiB is
unacknowledged. It checks before reads of up to 8192 bytes, so the threshold can
be exceeded by one read. Preserve bounded backpressure in the adapter. Never
inject reconnect notices or diagnostics into an HTTP/WebSocket byte stream.

On local EOF or a local read error send Close at the final outgoing offset,
**after all Data through that offset**, and keep draining the reverse direction.
Fabric emits Close after its final Data on that attach and re-sends Close on
reattach. Its receiver tolerates an early Close by recording the pending offset
and deferring local write-half shutdown until `recv_next` reaches it; the adapter
should tolerate that too.

A second Hello on an attached stream is a protocol error. Error is sent in answer
to Hello for cases such as failed target connect, a session limit, or an expired
resume; the server finishes the stream and waits up to one second. Its text may
include a host-local socket path. Treat it as a private diagnostic, never as
forwarded HTTP content or uploaded user data.

### Graceful teardown and abandoned sessions

A session completes only after both Close frames have been exchanged and all
data has been acknowledged. Continue reading and acknowledging delivered bytes
until the member Close arrives and the phone’s outgoing data is fully Acked.
Send the final Ack before finishing the send stream. Do not call
`connection.close()` before that Ack has gone out: a QUIC close discards unsent
stream data. Check this behavior against `TunnelSession::run_attach` and its
reader/writer loops, not just the frame encoder.

Fabric loss, suspension, or premature connection closure can leave the session
detached on the member. The original `st3-client.sock` connection remains open
and gateway output can accumulate to the 4 MiB threshold plus one read until the
900-second TTL or eviction. A framing gap also ends the attach and leaves it
detached. A failed local target write is `LocalEndpointGone`, which instead
causes the server to remove the session immediately.

At the default limits, a phone has at most 16 concurrent sessions and the machine
has 64 total, shared with other peers’ tunnels and resumable shell/exec sessions.
A new session evicts the oldest detached session when needed, preferring this
phone’s own detached sessions first; it refuses admission if all relevant slots
are attached. Sixteen abandoned phone sessions can therefore retain roughly
64 MiB plus up to one 8 KiB read per session on the gateway. A gateway WebSocket
writer may keep writing until the abandoned tunnel’s buffer fills. Measure
abandoned session count, retained bytes, eviction, and target socket cleanup in
the isolated proof, especially for the no-resume adapter.

Full fabric resumption reconnects with the **same service ALPN** (or reopens a
logical mux stream to the same service), then sends the same session ID, current receive offset, and `resume = 1`. Each side
replays unacknowledged bytes from the other side’s advertised receive offset. The
member keeps the original target socket, checks session ownership by NodeID, and
rejects a resume after expiry/eviction. A resume rejection must not become a fresh
HTTP transaction with old bytes. Default server limits are 64 sessions total,
16 per peer, and a 900-second (15-minute) detached TTL; deployed configuration can
differ.

The smallest first adapter can implement fresh Hello/Data/Ack/Close/Error only,
then close the affected local TCP connection on fabric loss and let the existing
client reconnect. State this limitation in proof results: it does not preserve
an open HTTP connection across fabric failure. Do not automatically resubmit
mutating HTTP requests when a response is lost. Full tunnel resumption is a
separate addition with replay/half-close/expiry tests.

## Smallest Rust surface to embed

Embed **iroh + an outbound fabric dialer + a loopback TCP byte bridge**, driven by
one app-owned Tokio runtime. The native Swift/Expo boundary needs operations like
`start(member_node_id, service) -> { loopback_url, phone_node_id }`, `stop()`, and
connection/error state. These are proposed adapter operations, not APIs currently
exported by fabric.

The dialer owns a stable-key endpoint, direct service-ALPN connections,
identity/grant failure reporting, and the tunnel codec/session state. It presents
each admitted tunnel as `AsyncRead + AsyncWrite` (or copies it to a local `TcpStream`) so the bridge
can preserve bidirectional bytes and half-closes. With the first adapter’s
no-resume limitation it needs no daemon recovery loop or persistent replay
session beyond that connection.

The pinned repository has **no small public dialer crate**. `fabric::mux` exposes
`PeerConnections::open_stream`, but it is coupled to fabric presence/daemon
behavior. `tunnel` is a private module and its `Frame` codec is private.
`fabric-service-api` is the inbound service interface, not an outbound dialer.
Depending on the whole fabric package also pulls daemon, Unix/process/service,
PTY, updater, and configuration code; it is not evidence of a supported iOS
library surface.

The fabric owner recommends building the small direct-ALPN wire dialer on an
app-owned iroh Endpoint. A proof-specific codec can be written against this pin
with explicit provenance and compatibility fixtures. Inform the owner when the
app begins to depend on the wire so it can be treated as a compatibility surface
and future changes can be recorded in fabric’s CHANGELOG. Do not label such a
codec a stable fabric SDK. The iroh 1.0.2 target,
TLS provider, getrandom support, and final static library/XCFramework must be
built and linked for both simulator and device by the existing iOS builders.
The source-only step establishes neither cross-compilation nor signing support.

## Using the existing TypeScript client

Bind the adapter to **`127.0.0.1:0`** on the phone and return its actual port.
For each TCP connection, open one admitted fabric tunnel to the fixed member and
fixed service and forward the entire HTTP/1.1 byte stream, including WebSocket
upgrade and subsequent frames. A byte bridge needs no HTTP parser, route
translation, JSON conversion, or WebSocket termination. It must handle multiple
concurrent connections (ordinary requests, uploads, the collections socket) and
keep bounded buffers. Do not bind a LAN address or let request URLs choose an
arbitrary fabric service.

[`store.tsx`](../../apps/ios/store.tsx) already constructs `St3Client` with a
`baseUrl`, a secure-store credential callback, and `gatewayFetch()`. It also has
an upload client using Expo fetch. When the development fabric setting is enabled,
wait for native start and give **both** clients the returned loopback URL. Keep
the saved paired member route and its credential association separate from the
ephemeral port. Disabling the setting restores the saved Tailscale/HTTPS route;
never send one member’s credential to a different member.

[`Client.generated.ts`](../../clients/typescript/st3-client/Client.generated.ts)
sends `Authorization: Bearer …` on fetch and React Native WebSocket handshakes,
converts `http:` to `ws:`, and retains protocol headers. Forward all these bytes,
including idempotency keys, attachment content, upgrade headers, and terminal
capability subprotocols. [`feed.ts`](../../apps/ios/feed.ts) holds the single
collections WebSocket for attention/missions/agents and the open conversation
and terminal; it reconnects and resubscribes in the foreground.

Two existing policies need narrow, development-only integration:

- [`gatewayUrl.ts`](../../apps/ios/gatewayUrl.ts) currently accepts HTTPS or HTTP
  for Tailscale/private-LAN hosts and rejects `http://127.0.0.1`. Select the native
  adapter’s returned URL through a separate development transport path. Accept
  only the exact address of the currently running native bridge; a user-typed
  loopback URL or a stale saved port must never select or pair with another local
  listener. Keep the ordinary gateway input’s existing HTTP policy.
- [`app.json`](../../apps/ios/app.json) declares ATS exceptions for tailnet/LAN
  ranges and `.local`, with no explicit loopback exception. Have the iOS builder
  verify actual loopback HTTP **and** WebSocket behavior in the development build
  and add the narrow build-specific configuration if required. Do not assume
  arbitrary HTTP or release-wide ATS relaxation is necessary.

On background/disable, stop accepting local connections and close carrier state
with the app’s existing foreground lifecycle. On foreground, restart the adapter
before opening the feed and refresh its ephemeral URL. No polling, fabricated
HTTP replies, cached mutation queue, or secret logging is needed.

## What must be rechecked when fabric changes

| Changed surface | What would break |
| --- | --- |
| iroh API/preset/version, discovery or relay defaults | Endpoint binding, address lookup, identity/address serialization, or iOS linkage |
| `fabric/mux/2` or generation preface | Connection negotiation; treating the first stream as a service would corrupt admission |
| Stream header/admission encoding | Routing or the first tunnel bytes; reason strings must not become HTTP content |
| Generic exposure framing, Hello shape, offsets, Ack/Close semantics | All HTTP/WebSocket transport or replay/half-close correctness |
| Peer-file format, `may`, exposed-name mapping | The exact phone grant could stop admitting the gateway or change scope |
| Session limits, lifetime, ownership/replay behavior | Concurrency and reconnect behavior; a fresh connection cannot impersonate a resume |
| Extracted dialer API or its build targets | Native ABI/lifecycle and device/simulator builds |

A release bump with the same ALPN is not proof of compatibility: this experimental
wire format has no separate negotiated tunnel version. Re-read the affected
source and run independent fixture and interoperability checks against the exact
member artifact before advancing the pin.

Owner review: the fabric owner confirmed the pin, direct-ALPN path, exact grant
checks, Hello-first ordering, and 15-minute default TTL. Its subsequent review of
the document confirmed the framing/Hello/admission bytes and supplied corrections
for teardown/reaping, Ack cadence/backpressure, Close ordering, peer creation,
ongoing probes, and existing-session revocation. Those corrections are included
here. The stui owner also required bridge-owned loopback selection and an
isolated-daemon proof before member provisioning.

The executable proof should establish: unknown NodeID refusal; trusted phone
refusal for an ungranted exposure; exact gateway admission; st refusal without a
paired credential; pairing/capabilities and collections over the carrier; uploads
and WebSocket/terminal capability forwarding; half-close and concurrent requests;
network loss/foreground recovery without replaying actions; abandoned session
limits/reaping and final-Ack teardown; member probe cost; a recorded
relay/direct path; and Tailscale working after disabling the development setting.
Keep these results separate from this source-derived protocol document.
