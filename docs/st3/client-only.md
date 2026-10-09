# Terminal UI on a device without a daemon

A laptop can run `st` as a paired client device. It holds a private read-only display cache and
device credentials, connects to a member's client gateway, and sends confirmed actions through
that member. It runs no st daemon or replication worker, holds no graph replica, and needs no
fleet membership, member key, peer exposure, or fleet sync secret. Members continue to sync as
described in [replication](replication.md) and [Fleet join](../fleet-join.md).

The [iOS fabric transport proof](ios-fabric-protocol.md) documents a pinned,
development-only direct iroh carrier and app-owned loopback adapter for the same
paired gateway. It is a protocol specification; device interoperability is still
to be proven.

## Pair once

First publish the member's **paired-only** `st3-client.sock` through an existing LAN or tailnet
carrier. Use the [client gateway setup](client-v0/README.md#tailnet-carrier), including the
unauthenticated `403` check. Never forward the privileged `st3.sock`. Use HTTPS, or HTTP over an
encrypted tailnet; HTTP on a shared LAN exposes the bearer credential to observers.

On the member, create a short-lived, single-use challenge for the intended person:

```sh
st devices --as person/avery pair --full-control "Demo laptop"
```

This returns a `pairing_id`, a code, and its expiration time. `--full-control` grants the actions
the terminal UI offers; omit it for the API's limited read/terminal/attention/launch scopes, or pass
`--read-only` for a device that may only observe projections, glasses, and terminal output.
The terminal UI cannot expand the member's grant.

On the laptop, install Smalltalk and run one setup command:

```sh
st devices complete https://member.example pairing/CHALLENGE_ID --fingerprint sha256:FINGERPRINT
```

Obtain the member fingerprint through a trusted channel. Enter the code at the prompt. Input is hidden, and the code and credential are never printed.
Automation can provide the code on stdin; keep it out of shell arguments and history. Pairing
does not require a local daemon or `ST3_PERSON`. The member's grant determines the person and
session actor; local actor settings cannot change that authority.

Then run `st`. A saved profile selects client-only mode automatically. `st ui --client` requires
a saved pairing; `st ui --local` explicitly selects the usual local Unix socket and configured
person. `st ui --help` lists these choices.

## Several members and reconnecting

Repeat `st devices complete URL PAIRING_ID --fingerprint sha256:FINGERPRINT` for another member to add its route and its own device grant.
All routes in one profile must delegate the same person; use separate `XDG_CONFIG_HOME`
directories for different people. Each credential goes only to the member it was paired with.
The profile is read at startup, so restart the terminal UI after adding a route. A reachable member stays
selected until its connection drops. The terminal UI tries the other saved routes before waiting again,
and uses the selected member for both live projections and actions.

When none answers, the header says **offline** and the footer says **Last connected at** a UTC
time (or **No member reachable** before the first connection). The last lists and conversation
stay visible. The display cache also survives restarting the terminal UI while offline. Mutations require
a live projection and fresh server fences; offline input queues no mutation and reconnecting
does not replay it. An action whose response was lost is not automatically submitted again.

Connection attempts have a five-second deadline per route. Retries grow from one second to
thirty seconds, with up to half a second of jitter, and reset when the member answers. Device
gateways have no member-online announcement channel, so this bounded retry also discovers a
member's return without requiring inbound connections to the laptop. A fifteen-second bounded
capability probe detects an idle network blackhole. Press `r` while offline to retry immediately;
reconnecting normally requires no keys or commands. Opening a new connection resubscribes the
live windows and open conversation and reattaches a terminal when its incarnation still matches.

## Credentials and cache

The profile is `$XDG_CONFIG_HOME/st3/stui-devices.json`, defaulting to
`~/.config/st3/stui-devices.json`. It stores scoped bearer credentials in a `0600` file under a
`0700` directory, replaced atomically. The terminal UI refuses profiles readable by other users, symlinks,
and files owned by another user. Treat this file as a secret: do not commit, share, or paste it.
The client cache uses the normal private `$XDG_CACHE_HOME/st3/stui` directory and is scoped to
the paired person, routes, and device IDs. It contains display data, never graph authority, and
expires after seven days.

Credentials expire according to the member's API policy (currently thirty days). Create a new
challenge and run the pairing command again to replace a route's grant. List and revoke devices
on the member with `st devices --as person/avery ls` and
`st devices --as person/avery revoke device/DEVICE_ID --reason "Retired laptop grant"`.
Re-pairing creates a new grant; revoke any old grant that is no longer needed. Removing the
local profile makes the terminal UI select local mode again but does not revoke the server-side grants.

## Isolated verification

```sh
cargo test -p st3 --locked --test integration client_only
cargo run -p st3-client-codegen --locked -- --check
```

The `client_only` end-to-end test launches the actual st binary in a PTY with an isolated
client home and deliberately absent local socket. Independent member routers and paired-only
network carriers prove pairing, a confirmed remote action, loss of the gateway, retained data
and a last-connected time, no queued action, offline cache after a client restart, automatic
recovery, an idle network blackhole, and selection and control of another reachable member,
including failover while the terminal UI is running. It touches no shared daemon.
