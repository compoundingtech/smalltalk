# Fleet join

A fleet is a set of trusted members that replicate one claim history. For the two-machine
walkthrough, see [Two machines](two-machines.md); [Fleet replication](st3/replication.md) explains
sync, digests and checkpoints. This page is the membership, invitation and transport reference.

## Terms

A **member** is a node admitted by membership claims. Each generates an Ed25519 **member key**;
its private key stays on that machine. An **incarnation** is one key for one node name. A wiped
machine rejoining under the same name gets a new incarnation. The **anchor** is the key every
member pins as the membership root. A **sponsor** is the listening member that issued an invite;
only that sponsor redeems it. A join code carries a single-use bearer token, never the fleet secret.
A **writer** is the node name that authored an envelope. Config peers are explicit `[[peers]]`
entries or `--peer` arguments; they remain a compatibility path while legacy exchanges are enabled.

## Trust model

Members are trusted to write claims and run agents with shell access. The fleet secret authenticates
traffic with HMAC-SHA256. Members additionally sign connections and envelopes with their own keys.
An envelope signature binds its writer incarnation wherever another member relays it. Membership
claims count only from authenticated current members or the pinned anchor.

The fleet secret stays in its private file and a sealed join response. It never belongs in a code,
claim, document, log, command line or service definition. Tailscale or Fabric provides encryption
between machines; plain network HTTP requires an explicit opt-out (below).

### What removal guarantees

After a member receives an incarnation's removal, it refuses connections from that incarnation
and envelopes above its removal `high_water`. A keyed writer's envelope must carry that writer's
signature, so knowing the shared secret cannot impersonate another keyed member.

### What removal does not guarantee

A partitioned member that has not received removal can still admit that incarnation's writes.
Previously admitted envelopes are not retrospectively removed; doctor exposes admission beyond
high water. A member that has not learned a writer's key can admit unsigned history, and legacy
writers cannot be authenticated individually. Disable legacy exchanges after every retained
writer is admitted and signed. Removing membership does not erase history, rotate the secret or
stop declared seats. See [Checkpoints](st3/checkpoints.md) before treating removal as an excusal.

## Files on each machine

`STATE` is the effective state directory, normally `~/.local/state/st3`.

| Path | Mode | Contents |
|---|---|---|
| `STATE/fleet/` | `0700` | Local fleet files |
| `STATE/fleet/fleet.toml` | `0600` | Fleet ID, anchor, mode, port, transports, key/secret paths, writer floor and legacy setting |
| `STATE/fleet/secret` | `0600` | Fleet secret |
| `STATE/fleet/node.key` | `0600` | Private member key |
| `STATE/fleet/join.json` | `0600` | Interrupted join progress, without secrets |
| `STATE/fleet/removal.json` | `0600` | Signed removal reporter, reason and time |

Fleet commands maintain `fleet.toml`; change it through those commands. Config loading uses the
effective state directory after `--state-dir`. A conflicting fleet ID in `config.toml` is refused.
Local invite tokens do not replicate; see [Data authority](st3/data-authority.md).

## Creating your first fleet

`st fleet create` creates local membership while retaining the store's history. `--name studio`
selects its name; `--port`, `--transports`, `--advertise-loopback` and `--no-service` choose local
listener setup. Start the daemon and replication worker as the command directs before issuing
invites. A dial-out member cannot sponsor because a joiner must reach the sponsor.

## Invite

```text
st fleet invite [NAME] [--expires DURATION] [--via auto|tailscale|fabric|loopback] [--migrate]
                [--send-fabric | --code-only | --code-file PATH] [--as PERSON]
st fleet invites [--all]
st fleet invites revoke fleet-invite/ID --reason TEXT
```

The optional name pins the joiner's name. Expiry defaults to 15 minutes and accepts 10 seconds to
24 hours. `auto` includes advertised endpoints; `--via` selects a transport. A sponsor must already
be a fleet member with a listener. Invites are person operations; an agent seat must provide an
explicit `--as person/NAME` where the command permits it.

Prefer the no-echo prompt, a pipe, `--code-file` or `--send-fabric` to a code in argv or a transcript.
The Fabric form creates a private temporary code file, sends it and deletes the local copy;
`st fleet join --fabric-inbox` reads and removes the matching inbox file.

### Expiry and single use

The sponsor's clock decides expiry. First redemption binds the token to one member key and name;
that same key/name can redeem again until expiry to recover a lost response, without another
admission claim. Another key is refused. Five failed proofs revoke the invite. Explicit revocation
on another member takes effect when it reaches the sponsor. `st fleet invites` shows redemption,
member identity and sponsor acknowledgement; `--all` includes ended invites.

Join refusals use generic `invite-invalid`. If an admitted joiner's response is lost and the code
expires, remove the stranded member and issue a new invite. The join route is available only while
an unexpired invite exists and admits a body of at most 4 KiB.

## Join

```text
st fleet join [CODE | - | --code-file PATH | --fabric-inbox] [--name NAME] [--dial-out]
              [--via ROUTE] [--port PORT] [--transports LIST] [--advertise-loopback]
              [--no-service] [--no-wait] [--as PERSON]
```

No code argument prompts without echo; `-` reads stdin. Successfully redeemed code files are
removed. Name selection uses `--name`, the pinned invite name, then configured/stable node identity;
it does not query `scutil` during join. The reserved name `local` is refused.

Join persists its member key before redeeming, stops the local daemon before reading its writer
head, writes the sealed response into private fleet files, records the writer floor, and starts
or refreshes the daemon/worker. Native seats retain their provider processes across the daemon
restart. A joiner with unreachable inbound ports can still sync both directions by dialing out.

### Interrupted join

Before redemption, retry with the same code and member key. After `redeemed` is persisted, running
join without a code resumes service setup and waiting. If the answer was lost, the same key redeems
again until expiry. Losing the key or letting that invite expire requires removal and a new invite.
`join.json` is the progress record; do not replace the key to work around a refusal.

### Reusing a name

A current member or a writer with unended history must be removed first. A new key with no local
writer history can reuse an ended name: its writer floor is the highest sequence or ended high
water the sponsor knows, and its window begins above that floor. A store that already wrote under
that name cannot use this rejoin path; choose a new name or a separately reviewed reset.
Seats remain declared on the node name and can resume when a member rejoins under that name.

## Who may exchange

HMAC authenticates both directions first; member-key signatures then select the acceptance below.
The dialer applies the same rules to the response, checking the peer it intended to reach.

| Sender name | Member signature | Result |
|---|---|---|
| Current member | Valid for its current key | Accept |
| Current member | Missing or invalid, `legacy_peers = true`, and the name is a local config peer | Accept, counted as a legacy exchange in status |
| Current member | Missing or invalid, otherwise | Refuse, 401 `member-signature-required` |
| Ended incarnation | The ended key, or no key for an ended legacy name | Refuse, 403 `member-removed` (or `member-left`) |
| Conflicted | Any | Refuse, 409 `member-conflicted` |
| Not a member, listed in local `[[peers]]`, on a node with no `fleet.toml` or with `legacy_peers = true` | Not checked | Accept as a legacy exchange |
| Anything else, including a key this node has not seen admitted | Any | Refuse, 403 `not-a-member` |


A signed removal refusal names the ended key. A node treats it as its own removal only when that
key is its current key; a refusal naming an older incarnation remains transient. Removal is kept
in `fleet.toml` and `removal.json`; the node stops dialing and doctor names leave/uninstall recovery.

### Signed envelopes and writer fences

An admitted keyed writer must sign every envelope. Admission uses the incarnation's sequence
window and removal high water, independently of which member relayed it. Old-key or unsigned
writes cannot enter a new incarnation just because their sequence matches. Unknown/unsigned or
fenced records remain visible as admission state, not silently accepted graph authority.

## Listener capability and absence

Listening members advertise endpoints; dial-out members advertise none. `st fleet mode listening`
and `st fleet mode dial-out` change that capability, endpoints and Fabric exposure. Either mode
uses the same sync and network-recovery policy. A dial-out member can replicate while its live
terminal is unavailable remotely because no route reaches the owning machine.

Loss of contact does not remove membership. Status distinguishes current membership from last
successful contact; authentication, admission, membership and projection failures need investigation.

## Transports

The worker dials listening members using local overrides, Tailscale, Fabric, then loopback routes.
Successful inbound exchanges suppress redundant reverse dials. Graph/network wakes request prompt
sync; unreachable peers use adaptive network backoff. This is connection recovery, not a retry of
broken local work. A connection pushes and pulls history while its backlog drains.

### Changing an existing member's transports

```text
st fleet transports add fabric [--fabric-protocol PROTOCOL] [--no-service]
st fleet transports remove fabric [--no-service]
```

Run these commands on the member whose transports should change. They update the existing
`fleet.toml`, publish `fleet.member-endpoints` immediately, and refresh the service using the
same path as `st fleet mode`. Run them with the local daemon running. The member's name,
identity, secret, signing key and graph history stay intact.
The command currently supports Fabric; changing Tailscale is outside this command's scope.

Adding Fabric requires a listening member with a configured replication port and a working
local `fabric` command. Use `st fleet mode listening` first for a dial-out member. Add exposes
`127.0.0.1:PORT` before saving or announcing the Fabric endpoint; if exposure fails, settings
and endpoints are unchanged. The protocol defaults to the saved `fabric_protocol`, or
`st3/fleet/FLEET_ID`. Re-adding an enabled transport changes nothing; to change its protocol,
remove it and add it with `--fabric-protocol PROTOCOL`. Configure Fabric trust and grants on
the peers as described below.

Removing Fabric first publishes the endpoint list without Fabric, then unexposes the saved
protocol. It refuses an absent configured Fabric transport, the last configured transport,
or a listening member with no usable remaining configured endpoint. The saved protocol is
retained for a later add. If unexposure fails, the endpoint stays withdrawn and the command
reports the exact `fabric unexpose PROTOCOL` cleanup command. Withdrawal succeeds even when
the Fabric executable or daemon is unavailable, so a broken carrier can be removed; the
reported cleanup remains to be done when Fabric is available.

If endpoint publication fails after the settings write, the command reports that partial
result and keeps the exposure reachable: a lost response can follow a committed claim.
Restart the worker to publish the saved settings; after a failed remove publication, then
run `fabric unexpose PROTOCOL` to finish cleanup.

With `--no-service`, or when no service is installed, restart `st3 replication-worker` yourself
to apply its new transport selection. Until that restart, the old worker cannot re-expose
Fabric or republish its previous endpoint list after a settings change. The worker restores
the configured exposure from `fleet.toml` when it starts. `st fleet status` shows the local
configured transports and enabled Fabric protocol, including before the worker restart.
A local settings read or lock failure stops the worker with an error. Repair that settings
problem and restart the worker to restore publication and exposure.

The endpoint claim's shape is unchanged. Older membership-capable builds read the new claim
through their existing membership fold and derive routes from its complete endpoint list:
they can dial an added Fabric route when Fabric is locally available and stop choosing a
removed route once the claim replicates. A peer that still holds the earlier claim may try
the withdrawn route until it receives the update; its existing network route retry behavior
can continue over the remaining carrier. Builds that only support config peers keep using
their configured routes and need the existing migration workflow to consume membership.

### Tailscale

The worker discovers local Tailscale addresses, binds only assigned tailnet addresses and publishes
changes. Userspace networking without a bindable local address is reported unavailable. ACLs must
allow the peer port. See [Tailscale](st3/tailscale.md) for setup.

### Fabric

The default protocol is `st3/fleet/FLEET_ID`; `fabric_protocol` can retain an existing grant name.
The worker exposes its loopback listener with `fabric expose PROTOCOL --tcp 127.0.0.1:PORT` and
obtains an outbound tunnel with `fabric dial NODE_ID PROTOCOL --tcp 127.0.0.1:0`. `st fleet transports remove fabric`, leave, dial-out
mode and uninstall remove the exposure. st does not edit Fabric trust: the exposing member must
allow the dialer's NodeID and protocol. Missing grants are reported with the entry to add.

### Loopback and plain HTTP

Loopback advertisements are explicit for tests and operator tunnels. Peer HTTP listeners normally
bind loopback or Tailscale addresses; `peer_listen_allow_plain_http = true` (or
`--peer-listen-allow-plain-http`) permits unencrypted network HTTP. Use actual routes from
`st fleet status --json`, rather than inventing a tunnel address.

## Remove, leave and uninstall

`st fleet remove NAME --reason TEXT [--as PERSON]` ends membership. It does not inspect or stop
seats: stop them first with `st agents stop` if needed. Their declarations remain on host NAME.

`st fleet leave [--offline] [--force] [--cancel] [--wait DURATION] [--no-service] [--as PERSON]`
refuses running local seats unless forced. It records leaving mode in the store, stops local
mutations, drains history to a listener, writes the leave claim, and confirms the same authority
digest before and after that claim or a signed `member-left` naming its key. Interrupted leaving
resumes; `--cancel` restores normal mode before the claim is written. Offline leave skips the peer
proof and prints the removal command for another member. The local history remains fleet-bound;
joining a different fleet needs a separately authorized reset.

`st uninstall [--dry-run] [--yes] [--offline] [--keep-binaries] [--erase-local-graph] [--no-service] [--as PERSON]`
leaves the fleet, stops owned runtimes, removes services/exposure and st's Claude plugin/marketplace,
and deletes configured local state and manifest-owned binaries. It does not remove workspace Git
exclude lines. Nix/source installations without a manifest print their removal instructions.
A sole local graph copy also requires `--erase-local-graph`. Inspect `--dry-run` before deleting it.

## Status and diagnostics

`st fleet status` lists membership, listener mode and local configured transports. `--json` also
gives addresses, local `transports`, the enabled `fabric_protocol`, and Fabric node/protocol fields. `current` means admitted, not online. Use `st replication status`
for exchange age, backlog and divergence, and `st doctor` for authentication/admission problems.
For a leaked code, revoke it before use or remove an unexpected admitted member, then issue a new
invite. For a wrong-fleet refusal, inspect the binding before considering any reset.

## Move an existing fleet off local dial helpers

Retain names, fleet ID, member keys and history. Use the same current build on all members. Keep
original config, secrets and helper definitions until native signed exchanges have been verified.

1. On the chosen anchor, run `st fleet migrate --anchor --transports tailscale,fabric` with the
   already granted `--fabric-protocol` when needed.
2. Invite each existing member with `st fleet invite NAME --migrate` over its reachable transport.
   Redeem with `st fleet migrate --fabric-inbox` or `--via ROUTE`; the worker obtains its own tunnel.
3. Inspect status and require current membership plus successful signed exchanges over native
   routes. On every member, run `st fleet migrate --finish`, delete exactly the config lines it
   prints, and refresh services to use membership settings.
4. Disable the old helper and verify exchanges in both directions and after a Fabric restart.
   Only then remove its unit/executable and unneeded grants; retain the advertised member protocol.

### Rollback

Before finish, restore the saved config/helpers. After finish, every member that must accept an old
build needs its saved legacy peer entries and `st fleet migrate --unfinish`, followed by a worker
restart, before restoring that old build. An old build's unsigned envelopes remain held once the
writer is known to be keyed. Preserve the current store, keys and history throughout; do not reset
them to make rollback or diagnostics appear successful.
