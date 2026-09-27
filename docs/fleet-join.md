# Fleet join

This document designs how a machine joins, leaves, and is removed from an st fleet, and how the
replication worker reaches other members. It is written for the people and agents who implement
and review it. The exchange of signed immutable envelopes described in
[Fleet replication](st3/replication.md) keeps its format. This design adds member signatures to
it and one admission rule, the writer fence.

`st` and `st3` are the same program. This document uses `st`.

## Why

Adding a third machine to a two-machine fleet took all of this:

1. No release bundle existed, so the binaries were copied from another machine.
2. The fleet ID and the fleet secret were copied by hand.
3. The new machine had to be added to an existing member's `config.toml`.
4. `st service install` had to run again on that member, because the service files bake
   `--peer` arguments in at install time.
5. The new machine is a laptop behind a firewall. `st` accepts only loopback peer URLs, so a
   hand-written script kept a Fabric dial alive, with its own launchd agent.
6. The member refuses exchanges from any node it does not list, and every listed peer needs a
   URL. So the member lists the laptop at a port nothing serves, keeps dialing it, and reports it
   down.

This design replaces each step:

| Step today | After this design |
|---|---|
| Copy binaries | Install a release bundle ([binary releases](st3/binary-releases.md)) |
| Copy the fleet ID and secret | `st fleet invite` prints a single-use code; `st fleet join` exchanges it for the secret over an encrypted handshake |
| Edit another member's config | Membership is a set of claims in the graph; every member learns it by replication |
| Reinstall services to change peers | Service files carry no peers; the worker reads membership at run time |
| A helper keeps a Fabric dial alive | The worker dials Fabric itself; Tailscale needs no dial at all |
| A laptop is listed at a dead port and reported down | A dial-out member is never dialed and never reported down |

## What a person types

On any listening member, or on a machine that is not in a fleet yet (which then founds one):

```sh
st fleet invite laptop
```

On the new machine, after installing a release:

```sh
st fleet join --dial-out
```

`join` asks for the code, receives the fleet secret, installs the services, and waits until the
new machine has the fleet's full history. `--dial-out` is for a machine that is often asleep or
offline, such as a laptop. Leave it out for a machine that stays on.

When both machines are Fabric peers, the code never has to appear on a screen. `fabric exec`
gives the remote command no standard input, so the code goes in as an argument:

```sh
fabric exec laptop -- st fleet join --dial-out "$(st fleet invite laptop --code-only)"
```

To take a machine out:

```sh
st fleet remove laptop --reason "wiped for reinstall"   # on any other member
st uninstall                                            # on the machine itself, if it still runs
```

## Terms

- **Member**: a node admitted to the fleet by a membership claim. Every member holds the fleet
  secret and its own member key.
- **Member key**: an Ed25519 key pair that each member generates for itself. The public half is
  in the member's membership claims. The private half never leaves the machine.
- **Incarnation**: one member key for one node name. A machine that is wiped and joins again under
  the same name is a new incarnation.
- **Listening member**: a member that accepts connections and advertises at least one endpoint.
- **Dial-out member**: a member that accepts no connections. It dials listening members.
- **Sponsor**: the listening member that issued an invite. Only the sponsor can redeem it.
- **Invite**: a single-use permission, held by the sponsor, to admit one member key.
- **Join code**: the text that carries an invite to the new machine. It does not contain the
  fleet secret.
- **Config peer**: a peer listed in `[[peers]]` in `config.toml` or passed with `--peer`. This is
  how the running fleet is configured today. It stays supported.
- **Writer**: the node name that authored a replicated batch (`origin`). A node's writer name is
  its node name.

## Trust model

- Members are fully trusted, as they are today. A member can write any claim, and it runs agents
  with shell access on its machine. Membership decides who may exchange; it does not limit what a
  member may write.
- The fleet secret authenticates fleet traffic (HMAC-SHA256, unchanged). A member key identifies
  one member. After migration, a peer must prove both.
- A removed machine keeps its copy of the fleet secret. The member key is what makes removal
  enforceable: other members refuse a removed key, and the removed machine cannot sign as any
  other member.
- The fleet secret is never in a join code, a claim, a document, a log line, an error message,
  a command line, or a service file. It exists only in the secret file on each member and, during
  a join, inside one sealed handshake response.
- Tailscale or Fabric encrypts traffic between machines. The replication protocol is plain HTTP
  and is never exposed on another network interface.

## Files on each machine

`st fleet` commands write fleet state under the state directory, not into the person's
`config.toml`:

| Path | Mode | Contents |
|---|---|---|
| `STATE/fleet/` | `0700` | The directory below |
| `STATE/fleet/fleet.toml` | `0600` | Fleet ID, mode, port, transports, Fabric protocol, secret and key paths, legacy setting, writer floor |
| `STATE/fleet/secret` | `0600` | 32 random bytes |
| `STATE/fleet/node.key` | `0600` | The member key (Ed25519, PKCS#8) |
| `STATE/fleet/join.json` | `0600` | Join progress checkpoints; no secrets |

`STATE` is the daemon's state directory, `~/.local/state/st3` by default.

`fleet.toml` example:

```toml
# Written by st fleet. Change it with st fleet commands.
fleet_id = "5b0c1d8e-6a44-4f0e-9d51-2f7f3c9a0b12"
secret_file = "secret"               # relative to STATE/fleet
node_key_file = "node.key"
mode = "listening"                   # or "dial-out"
port = 31313                         # the loopback and tailnet listeners share this port
transports = ["tailscale", "fabric"] # what this node listens on and dials with
fabric_protocol = "st3/fleet/5b0c1d8e-6a44-4f0e-9d51-2f7f3c9a0b12"
legacy_peers = false                 # true only during a migration
# advertise_loopback = false         # publish the loopback endpoint too (tests, tunnels)
# fabric = "/path/to/fabric"         # optional executable overrides; tests set these
# tailscale = "/path/to/tailscale"
```

`Config::load` merges `fleet.toml` when it exists, reading it from the effective state directory
after command-line overrides such as `--state-dir`. The legacy fields in `config.toml`
(`fleet_id`, `shared_secret_file`, `peer_listen`, `[[peers]]`) keep working. If both files name a
fleet ID, the IDs must be equal, or the daemon refuses to start and says which files disagree.

The member key lives in the state directory on purpose. `st service reset` erases the store, and
with it the writer history; the reset machine must then join again as a new incarnation.
`st service reset` on a member therefore refuses unless `--leave` or `--offline` is given, with
the same meaning as `st fleet leave`.

## Invite

### Command

```text
st fleet invite [NAME] [--expires DURATION] [--via auto|tailscale|fabric|loopback]
                [--code-only] [--code-file PATH] [--as PERSON]
st fleet invites [--all]
st fleet invites revoke fleet-invite/ID --reason TEXT
```

- `NAME` pins the node name the new machine must use. Without it, the joiner chooses a name.
- `--expires` defaults to 15 minutes. It accepts 10 seconds to 24 hours.
- `--via` chooses which of the sponsor's endpoints go into the code. `auto` includes every
  endpoint the sponsor advertises. `loopback` adds the loopback endpoint, for tests on one machine
  and for operator tunnels.
- `--code-only` prints only the code, for command substitution or a pipe. `--code-file` writes
  it to a new `0600` file instead of standard output.
- Invites are person operations. The CLI uses the configured person, and refuses inside an agent
  seat unless `--as person/NAME` is explicit.

The default output:

```text
Invite fleet-invite/q3m7k2 for laptop expires at 14:32 (in 15 minutes).
On laptop, run this and paste the code when asked:
  st fleet join
Code:
  stj1-...
Or, if laptop is a Fabric peer of this machine:
  fabric exec laptop -- st fleet join "$(st fleet invite laptop --code-only)"
```

A dial-out member cannot sponsor, because nothing can connect to it. `st fleet invite` there
fails and names the listening members.

On a machine that is not in a fleet yet, `st fleet invite` first founds one; see
[Founding a fleet](#founding-a-fleet).

### What the sponsor stores

- A `fleet.invite-created` claim on `fleet-invite/ID` records the invite ID, sponsor, pinned name,
  expiry, transports, and person. It contains no token and no verifier. Every member sees it, so
  `st fleet invites` works anywhere.
- The token lives only in the sponsor's local table `fleet_invite_tokens`: invite ID, token,
  expiry, bound member key, and failed-attempt count. This table does not replicate. It is the
  same class as the local `capabilities` table in [data authority](st3/data-authority.md). The
  sponsor deletes a row when the invite is redeemed, revoked, or expired.

### Code contents

A code is `stj1-` followed by lowercase RFC 4648 base32, without padding, of this CBOR map and a
4-byte checksum (the first 4 bytes of the SHA-256 of the map):

| Key | Bytes | Meaning | Secret |
|---|---|---|---|
| `v` | 1 | Code version, `1` | no |
| `f` | 16 | Fleet ID | no |
| `i` | 16 | Invite ID, random | no |
| `t` | 16 | Invite token, random | yes, until redeemed or expired |
| `k` | 16 | Sponsor fingerprint: the first 16 bytes of SHA-256 of the sponsor's member public key | no |
| `x` | 8 | Expiry, Unix seconds, for messages only | no |
| `n` | up to 63 | Pinned node name, optional | no |
| `e` | varies | Sponsor endpoints: tailnet address and port; Fabric node ID (the protocol is derived from `f` unless the sponsor overrides it); loopback address when `--via loopback` | no |

A code is about 280 characters. It is meant to be pasted or piped, not typed. The checksum turns
a damaged paste into "this code is damaged" instead of a failed handshake.

The token is the only secret in a code. It is not the fleet secret, and it cannot be used to
derive the fleet secret: the secret is sealed with a key that also needs the joiner's ephemeral
private key (below).

### Expiry and single use

- The sponsor's clock alone decides expiry. The joiner reports the code's `x` in messages but
  does not refuse on it, so a joiner with a skewed clock still works.
- An invite binds to the first member key that proves the token. After that, only the same key
  can redeem it again, and only until it expires. A retry by the same key is how an interrupted
  join resumes. Any other key is refused.
- Five failed proofs burn the invite. The sponsor records `fleet.invite-revoked` with reason
  `too-many-failures`.
- `st fleet invites revoke` works on any member. It takes effect when the claim reaches the
  sponsor, which then deletes the token. `st fleet invites` shows whether the sponsor has
  acknowledged it.
- The join route answers with one generic refusal, `invite-invalid`, for every failure: unknown
  invite, expired, revoked, wrong proof, or bound to another key. It never says which.
- The join route exists only while the sponsor holds at least one unexpired invite. Otherwise it
  returns 404 like any unknown path. Its body is limited to 4 KiB, and it accepts at most 10
  requests a minute across all invites.

## Join

### Handshake

Notation: `i` is the invite ID, `t` the token, `Ks` the sponsor's member key, `Kj` the joiner's
member key, `ej` and `es` fresh X25519 key pairs for this handshake, and `T1` the canonical
serialization of the request fields listed below (sorted-key JSON without `proof` and
`signature`).

The joiner sends one request to the sponsor, `POST /v1/fleet/join` on the peer listener:

```json
{
  "protocol": "st3-join-v1",
  "invite": "i",
  "name": "laptop",
  "mode": "dial-out",
  "member_key": "Kj.public",
  "ephemeral": "ej.public",
  "writer_head": {"sequence": 0, "hash": null},
  "build": "0.3.1",
  "proof": "HMAC-SHA256(t, T1)",
  "signature": "Ed25519(Kj, T1)"
}
```

`writer_head` is the joiner's own highest batch for the chosen name, or empty if its store has
never written under that name.

The sponsor checks, in this order: body size and protocol; an unexpired, unrevoked local token
row for `i`; `proof` in constant time; `signature` against `Kj`; the pinned name; the name rules
in [Reusing a name](#reusing-a-name); and last the bind (`member_key IS NULL OR member_key = Kj`,
in one statement). The first successful bind appends `fleet.invite-redeemed` and
`fleet.member-admitted` in the same store transaction. A retry by the same key reuses those
claims.

The worker serves the route. It passes the proof to the main daemon
(`POST /v1/internal/fleet/redeem`), which holds the token table and writes the claims. The worker
then seals and signs the answer, because it holds the secret and the member key.

The sponsor answers:

```json
{
  "protocol": "st3-join-v1",
  "sponsor": "studio",
  "sponsor_key": "Ks.public",
  "ephemeral": "es.public",
  "nonce": "12 random bytes",
  "sealed": "ChaCha20-Poly1305(k, nonce, aad, {fleet_id, secret, writer_floor, fabric_protocol, admitted_claim})",
  "signature": "Ed25519(Ks, T1 || proof || sponsor_key || ephemeral || nonce || sealed)"
}
```

where `aad = SHA-256(T1 || Ks.public || es.public)` and
`k = HKDF-SHA256(ikm = X25519(es, ej), salt = t, info = "st3-join-v1 seal" || aad)`.

The joiner checks that SHA-256 of `sponsor_key` starts with the code's fingerprint, checks the
signature, derives `k`, and opens `sealed`.

What this gives:

- The fleet secret travels only inside `sealed`. Opening it takes both the token and the private
  half of `ej`, which exists only in the joiner's memory.
- Someone who records the traffic learns nothing, even if the token leaks later.
- Someone without the token cannot produce `proof`.
- Someone who replays a recorded request gets a new sealed answer to the recorded `ej`, which
  they cannot open. The bind is unchanged, because the key is the same.
- Someone who answers in the sponsor's place cannot sign as `Ks`, so the joiner refuses them,
  even if they know the token. The joiner never sends its local history to an impostor.
- The joiner proves it holds `Kj`, so nobody can register another machine's public key.

`ring` provides Ed25519, X25519, HKDF, and ChaCha20-Poly1305. It is already in the build through
`rustls`, so this adds no new crate. `ciborium` and `data-encoding` are already in `Cargo.lock`.

The design does not add timestamps or nonces to requests. Every peer operation is idempotent or
fenced, and a replay would first have to break Tailscale or Fabric encryption. Leaving time out
keeps clock skew out of authentication.

### What join does on the new machine

```text
st fleet join [CODE | -] [--name NAME] [--dial-out] [--via auto|tailscale|fabric|URL]
              [--no-service] [--wait DURATION] [--as PERSON]
```

With no `CODE`, `join` prompts for it without echo. `-` reads it from standard input. A code on
the command line works too; it is short-lived and single-use, and `join` does not log it.

Each step records a checkpoint in `STATE/fleet/join.json`, so running `st fleet join` again
continues from the last completed step:

1. **Check.** Parse the code. Refuse if this store is bound to another fleet, or this machine is
   already a member. Choose the name: `--name`, else the code's pinned name, else the configured
   node name, else the short host name (`scutil --get LocalHostName` on macOS). Refuse the name
   `local`. Names match `[A-Za-z0-9][A-Za-z0-9._-]{0,62}`.
2. **Key.** Create `STATE/fleet/node.key` if it does not exist, and sync it to disk before any
   request. Checkpoint `key-created`.
3. **Route.** Pick the sponsor endpoint: `--via URL` if given (loopback only), else tailnet if this
   machine's Tailscale is up, else Fabric if `fabric probe` reports the sponsor serves the fleet
   protocol, else an advertised loopback endpoint. If Fabric is the only route and the probe says
   `unsupported`, print the exact grant the sponsor needs:
   `add "st3/fleet/ID" to the allow list for NodeID ... in the sponsor's Fabric peers.toml`.
4. **Stop.** Stop the local daemon if it runs, so no local batch is written during the join. Seats
   keep running, as they do across any daemon restart. Read this store's own writer head
   (highest sequence and hash for the chosen name).
5. **Redeem.** Run the handshake. Write the secret to a temporary `0600` file, sync it, and rename
   it to `STATE/fleet/secret`. Write `fleet.toml`. Checkpoint `redeemed`. From here the code is no
   longer needed.
6. **Start.** Install or refresh the services (`--no-service` prints the two foreground commands
   instead). The daemon binds the store to the fleet ID and applies the writer floor before it
   writes anything. Checkpoint `started`.
7. **Sync.** Wait until the sponsor reports this member up and the authority digests match, and
   print progress (`received 12,480 envelopes; 3 exchanges left`). `--wait` defaults to 10
   minutes. If it runs out, `join` says the machine is a member and still syncing, and exits 0.
   Checkpoint `synced`.

Until its first exchange brings the membership claims, the new member knows only the sponsor. It
checks the sponsor's responses against the key from the handshake, which `join.json` keeps.

The new member then dials every listening member it can route to. A member that has not yet
received the admission claim refuses with `not-a-member`; the new member treats that as transient
for 10 minutes after its admission and does not record it as a failure.

### Interrupted join

| Interrupted after | State | Recovery |
|---|---|---|
| Step 1 to 3 | Nothing on the sponsor | Run `join` again with the same code |
| Step 5, before the answer arrives | Invite bound to this key; member admitted | Run `join` again with the same code; the same key redeems again |
| Step 5, after `redeemed` | Secret stored | Run `join` again without a code |
| Step 6 or 7 | Member, services may be down | Run `join` again; it starts the services and waits |

If the machine loses its key before `redeemed` (for example, it is wiped), the invite is bound to
a key that no longer exists. The admitted member never appears. `st fleet status` on any member
shows it as `admitted, never seen`. Remove it with `st fleet remove` and issue a new invite.

### Reusing a name

A machine that is wiped and joins again under its old name must not reuse writer sequence
numbers the fleet already holds. The fleet must also be able to tell its new batches from
anything the old incarnation wrote after it was removed, which another member may hold and the
sponsor may never have seen.

A name has **history** when the sponsor holds any envelope from that writer or any membership
claim for that name. At redemption, the sponsor applies these rules:

- The name has a current member, or it has history and no ended incarnation (a config peer that
  was never removed): refuse. Remove it first.
- The name has no history: admit with no floor. The new incarnation's window starts at 1.
- The name has history, and the joiner's `writer_head` is empty: admit with
  `writer_floor = H + 2^32`, where `H` is the highest of every sequence the sponsor holds for that
  writer, admitted or fenced, and every `high_water` in the name's removal and leave claims.
- The name has history, and the joiner's `writer_head` is not empty: refuse. This store has
  already written as that name; reset it or choose another name.

The joiner's daemon stores the floor in `meta` before its first batch, and
`next_replica_sequence` uses the larger of the floor and its local maximum. The joiner stops its
daemon before it reads its head (step 4), so no batch is written between the check and the floor.

The gap of 2^32 sequences separates the two incarnations. Everything the old incarnation wrote
after its `high_water` falls in the gap and is fenced (see [Writer fence](#writer-fence)), unless
it wrote more than four billion batches beyond `H`. A proof that each new envelope chains back to
the new incarnation's first batch would remove even that bound, but envelopes arrive sparsely, so
admission would have to hold each one until its predecessors arrived. Inventory ranges are built
only from envelopes that exist, so the gap costs nothing in an exchange.

The same store rejoining after its removal is refused on purpose. Its writes after the removal
were not accepted, and resetting it or choosing a new name keeps one rule for every rejoin.

Runtime ownership follows the node name. Seats declared on host `laptop` belong to whichever
machine is currently `laptop`. That is why `st fleet remove` asks what to do with a machine's
seats first.

### Founding a fleet

`st fleet invite` on a machine that is not in a fleet founds one, and says so:

1. Generate a random fleet ID (UUID v4), a 32-byte secret, and the member key.
2. Write `STATE/fleet/`, bind the store to the fleet ID, and append `fleet.member-admitted` with
   `via = founder` for this node, then its `fleet.member-endpoints`.
3. Detect the transports. Tailscale is used if `tailscale ip` works. Fabric is used if `fabric id`
   works. Pick port 31313, or the next free port.
4. Install or refresh the replication service if the daemon runs as a service. Otherwise print
   the foreground worker command. The worker serves the join route, so a code works once the
   worker runs.
5. Continue with the invite.

A local-only store keeps its history when it founds a fleet, and that history replicates to
every member that joins.

## Membership

### Claims and write policy

Membership lives on the existing `host/NAME` subjects. Invites use a new subject family,
`fleet-invite/ID` ("A single-use fleet join invite.").

| Kind | Subject | Written by | Main fields |
|---|---|---|---|
| `fleet.member-admitted` | `host/NAME` | The sponsor during a join; the member itself when founding or migrating | `fleet_id`, `member_key`, `via` (`invite`, `founder`, `migration`), `sponsor`, `invite`, `mode`, `writer_floor`, `admitted_by` |
| `fleet.member-endpoints` | `host/NAME` | The member's own daemon when its endpoints or mode change | `member_key`, `mode`, `endpoints`, `build` |
| `fleet.member-removed` | `host/NAME` | Any member, for a person, through `st fleet remove` | `member_key` (absent for a config peer that was never a member), `high_water`, `reason`, `removed_by` |
| `fleet.member-left` | `host/NAME` | The member itself, through `st fleet leave` | `member_key`, `high_water` |
| `fleet.invite-created` | `fleet-invite/ID` | The sponsor | `sponsor`, `name`, `expires_at_unix_ms`, `transports`, `created_by` |
| `fleet.invite-redeemed` | `fleet-invite/ID` | The sponsor | `name`, `member_key` |
| `fleet.invite-revoked` | `fleet-invite/ID` | Any member | `reason`, `revoked_by` |

Endpoint objects are `{transport: "tailscale", address: "100.101.102.103:31313", dns:
"studio.example-tailnet.ts.net"}`, `{transport: "fabric", node: "64 hex", protocol: "..."}`, and
`{transport: "loopback", address: "127.0.0.1:31313"}`. A dial-out member publishes
`mode = "dial-out"` and no endpoints.

Every kind has `WritePolicy::SystemOnly` and `Cardinality::Append`. None wakes the reconciler;
the worker rereads membership on every wake and every 30 seconds. The public claim API refuses
them with `claim-write-forbidden`. Only the dedicated `/v1/fleet/*`
operations on the local Unix API write them, and those require a concrete person, as device
pairing does. Replicated admission checks them against the schema, as it does every claim.

An older build does not know these kinds. It keeps them as `unknown` records and admits them
after it is upgraded, as [Fleet replication](st3/replication.md) already describes.

### Reducer

A new projection, `fleet_members`, folds these claims per node name. The fold must give the same
answer in any receipt order, so it uses sets and writer sequences, never timestamps:

- An incarnation is `(name, member_key)`. It exists once any `fleet.member-admitted` names it.
- An incarnation is **ended** once any `fleet.member-removed` or `fleet.member-left` names its key.
  Ending is permanent. A stale admission from a partitioned member cannot undo it, because it
  names the same key. Joining again means a new key, which is a new incarnation.
- `fleet.member-endpoints` counts only when the claim's origin is `name`. The latest one by that
  writer's sequence wins. Only a member can say where it listens.
- `fleet.member-left` counts only when the claim's origin is `name`.
- A removal without `member_key` ends the name's legacy incarnation: the config peer that was
  never a member.
- The **current member** for a name is its one incarnation that has not ended. Two such
  incarnations mean the name is **conflicted**; every member refuses both keys until a person
  removes one. This happens only if two machines are joined or migrated under one name.

The data authority table gains `fleet_members` (projection of `fleet.*` claims) and
`fleet_invite_tokens` (local short-lived authority).

### Who may exchange

Every request and response on the peer listener carries the existing HMAC headers. New builds add
two headers:

- `x-st3-member-key`: the sender's member public key, base64url.
- `x-st3-member-signature`: Ed25519 over `"st3-member-v1\n"` followed by the same canonical string
  the HMAC signs.

Old builds ignore these headers. After the HMAC check passes, the receiver decides:

| Sender name | Member signature | Result |
|---|---|---|
| Current member | Valid for its current key | Accept |
| Current member | Missing or invalid, `legacy_peers = true`, and the name is a local config peer | Accept, counted as a legacy exchange in status |
| Current member | Missing or invalid, otherwise | Refuse, 401 `member-signature-required` |
| Ended incarnation | The ended key, or no key for an ended legacy name | Refuse, 403 `member-removed` |
| Conflicted | Any | Refuse, 409 `member-conflicted` |
| Not a member, listed in local `[[peers]]`, on a node with no `fleet.toml` or with `legacy_peers = true` | Not checked | Accept (today's behavior) |
| Anything else, including a key this node has not seen admitted | Any | Refuse, 403 `not-a-member` |

The dialer applies the same rules to the response and its signature, so it knows it reached the
member it meant to reach.

The `member-removed` refusal is a signed response whose body names the removal claim, the key it
ended, and the person who removed it. A node treats it as about itself only when that key is its
own current key. It then records `removed` in `fleet.toml`, stops dialing, and `st doctor` fails
with the next command to run: `st uninstall`, or `st fleet leave --offline` to keep the local
store. A refusal that names an older key reaches a machine that joined again under the same name
from a member that has not yet received the new admission; the node treats it as transient, like
`not-a-member`.

### Writer fence

A removed machine that still holds the secret cannot connect to a member that knows about the
removal. But a member that has not received the removal yet, because it is partitioned, would
still accept the removed machine and relay its new envelopes to the rest of the fleet. The writer
fence stops that relay.

Each incarnation of a name owns a window of that writer's sequences:

- An incarnation admitted with `writer_floor = F` starts after `F`. Otherwise it starts at 1.
- An ended incarnation stops at the `high_water` in its removal or leave claim: the highest
  sequence of that writer that the author of the claim held. A current incarnation has no end.
- A config peer that was never a member has one legacy window from 1 with no end, until a removal
  without a key sets its end.

Admission refuses an envelope whose writer has at least one ended incarnation and whose sequence
is in no window. The envelope stays in `replica_envelopes`, and its record gets the new state
`fenced`. Admission reconsiders fenced records whenever membership changes, as it already
reconsiders `unknown` records. Fence evaluation uses membership from unfenced claims only, so a
writer's own claims cannot lift its fence. Local writes are never fenced.

Removal therefore means: the fleet stops accepting new authority from that incarnation. Writes it
made before the removal but had not synced are not accepted either. `st fleet remove` says so.

A wiped machine that joins again under the old name starts its window 2^32 sequences above
everything the sponsor knows for that writer (see [Reusing a name](#reusing-a-name)), so the old
incarnation's late envelopes fall in the gap and stay fenced.

### Peers derived from membership

The worker computes its peers at run time and recomputes them on every wake and at least every
30 seconds:

- **Dial set**: every current listening member other than this node that this node has a route to,
  plus every config peer that is not a member. A dial-out member dials only listening members.
- **Accept set**: the table above.
- **Config peers become local overrides.** A `[[peers]]` entry whose name is a current member
  becomes that member's loopback route from this machine. A `[[peers]]` entry whose name is
  removed or dial-out is ignored, and `st fleet status` says to delete it.

The worker reads membership from the main daemon through a new internal endpoint,
`GET /v1/internal/fleet/membership`, which returns members, keys, modes, endpoints, ended
incarnations, and local config peers. The main daemon stays the only database writer.

The main daemon uses the same view where it now uses `configured_peers`: `st doctor`,
`st replication status`, the machines view, and the client relay for remote terminal reads.

Every member dials out to every listening member. So when a listening member's address changes
while it is offline, it publishes its new endpoints the next time it dials anyone, and the fleet
converges without a person.

A full mesh sends one exchange per pair every 30 seconds when idle. That is fine for the ten or so
machines one person or a small team runs. A larger fleet would need designated hubs; that is not
in this design.

### Service units

For a node with `fleet.toml`, `st service install` writes units with no peer, fleet, secret, or
listener arguments:

```text
st3 up --node studio --state-dir STATE --socket SOCKET --client-gateway-socket GATEWAY
st3 replication-worker --node studio --state-dir STATE --socket SOCKET
```

The worker unit is installed whenever the node is in a fleet. Changing membership changes
nothing on disk and needs no restart. `st fleet join`, founding, `st fleet mode`, `st fleet leave`,
and `st fleet migrate` refresh the units themselves when the daemon runs as a service.

Units installed by an older build keep their baked `--peer` arguments and keep working until
`st fleet migrate` or `st service install` rewrites them. The arguments stay accepted.

## Dial-out members

A dial-out member is for a machine that is often asleep or away: a laptop.

- It has no listener and publishes no endpoints. No member ever dials it.
- It dials every listening member it has a route to, on the usual schedule: a wake on each local
  change, and anti-entropy every 30 seconds. One exchange moves data in both directions, so it
  sends and receives everything through its own connections.
- It never appends `transport.observed` claims. Its failed dials stay in its local
  `replication_peers` rows. A closed lid therefore never publishes "the server is down" into the
  fleet when it wakes.
- Listening members record its exchanges in their local `replication_peers` rows, not as
  `transport.observed` claims.
- `st machines`, `st doctor`, and `st fleet status` show it as `dial-out` with a local
  `last contact` time. It is never `unreachable`, and it never causes a doctor warning, however
  long it has been away. The machines view ignores older `transport.observed` claims about a name
  whose current member is dial-out.
- Other machines cannot open remote terminal reads for its seats, because the relay needs to
  connect to the owner. Its seats are visible in the graph as usual.

`st fleet mode dial-out` and `st fleet mode listening` switch an existing member. The switch
publishes new endpoints, starts or stops the listeners, and adds or removes the Fabric exposure.

A listening member that is offline is reported down, as today. That is correct for a machine that
is meant to stay on.

## Remove, leave, and uninstall

### `st fleet remove NAME`

```text
st fleet remove NAME --reason TEXT [--stop-seats] [--force] [--as PERSON]
```

Run on any member, for another member or a config peer.

1. Refuse to remove this node. Use `st fleet leave`.
2. List the seats whose desired state is running on host `NAME`, and the active mission runs whose
   runtime owner is `NAME`. Refuse if there are any, unless `--stop-seats` (publish a stop
   declaration for each seat as the person) and no active runs remain, or `--force` (leave them;
   they resume if a machine joins again under `NAME`).
3. Append `fleet.member-removed` for each current key of `NAME`, or without a key for a config peer,
   with `high_water` set to the highest sequence this member holds for writer `NAME`.
4. Revoke every open invite that `NAME` sponsored.
5. Print what happens next: the removal takes effect on each member when it receives this claim;
   the removed machine should run `st uninstall`; unsynced writes on it are not accepted.

If some members still accept legacy exchanges, removal cannot stop a machine that keeps the
secret from posing as one of those legacy peers. `st fleet remove` warns about it and names the
members that still have `legacy_peers = true`.

### `st fleet leave`

```text
st fleet leave [--offline] [--stop-seats] [--force]
```

Run on the member that is leaving.

1. Refuse while local seats run, unless `--stop-seats` or `--force`, for the same reason as remove.
2. Append `fleet.member-left` with `high_water` set to the sequence of the batch that holds the
   claim itself.
3. Exchange until a listening member reports that it holds this writer through `high_water`:
   the range digests covering those sequences match.
4. Stop the worker, remove the Fabric exposure, and delete `STATE/fleet/`.
5. Restart the daemon local-only. The store keeps its history and stays bound to the fleet ID.
   Joining the same fleet later works. Joining another fleet needs `st service reset` first.

`--offline` skips steps 2 and 3 when no listening member is reachable, and prints the
`st fleet remove` command to run on another member.

### `st uninstall`

```text
st uninstall [--dry-run] [--yes] [--offline] [--keep-binaries] [--erase-local-graph]
```

Removes everything st put on the machine. `--dry-run` lists what it would remove. Without `--yes`
it asks first.

1. If this node is a member, run `st fleet leave` (with `--offline` if given).
2. Stop owned runtimes, as `st service reset` does.
3. Remove the services (`st service uninstall`).
4. Remove the Fabric exposure (`fabric unexpose PROTOCOL`), if this node made one.
5. For each workspace of a seat declared on this host, remove the files `render.rs` generated under
   `.st3/` and the Git exclude lines it added. The list comes from the local graph before step 7.
6. Remove the st3 Claude channel marketplace and plugin registration.
7. Delete `$XDG_STATE_HOME/st3`, `$XDG_CONFIG_HOME/st3`, `$XDG_DATA_HOME/st3`, and the sockets in
   `$XDG_RUNTIME_DIR`.
8. Delete `$XDG_STATE_HOME/st2/hooks` only if no st2 state exists, because st2 shares it.
9. Delete the installed binaries listed in the release installer's manifest,
   `$XDG_DATA_HOME/st3/install.json`, unless `--keep-binaries`. The release installer gains that
   manifest. A Nix or source install has no manifest; `st uninstall` prints how to remove it.
10. Print anything that needs root, such as the managed Claude Code policy file, and anything it
    could not remove. Exit non-zero if anything it owns remains.

A local-only store, or a member leaving with `--offline`, may hold the only copy of its graph.
Then `st uninstall` also requires `--erase-local-graph`.

## Transports

The worker owns all three transports. There is no helper process and no extra service.

### Listening

A listening member runs:

- a loopback listener on `127.0.0.1:PORT`, always. Fabric exposes this one;
- a tailnet listener on this machine's Tailscale addresses, same `PORT`, when `tailscale` is in
  `transports`.

The worker publishes `fleet.member-endpoints` with every endpoint that is actually up. It checks
every 60 seconds and republishes only on change.

### Dialing order

For each peer in the dial set, the worker tries routes in this order and remembers the one that
worked:

1. A local override: a `[[peers]]` loopback URL for that name.
2. Tailscale: this machine's Tailscale is up and the member advertises a tailnet endpoint.
3. Fabric: this machine runs Fabric and the member advertises a Fabric endpoint.
4. Loopback: the member advertises a loopback endpoint.

A failed exchange moves to the next route on the next attempt. `st fleet status` shows the route
in use for each member.

### Tailscale

- The worker finds the addresses with `tailscale ip -4` and `tailscale ip -6`, using the
  `tailscale` override, else `PATH` from the login environment, else
  `/Applications/Tailscale.app/Contents/MacOS/Tailscale` on macOS.
- It binds only addresses that are both reported by Tailscale and inside `100.64.0.0/10` or
  `fd7a:115c:a1e0::/48`, and present on a local interface. It never binds `0.0.0.0`.
- If Tailscale starts after the worker, or the address changes, the worker binds the new
  address within 60 seconds and republishes its endpoints.
- Dialers use the IP address, not the MagicDNS name, so a DNS problem does not stop replication.
  The name is in the endpoint for display.
- Tailscale in userspace-networking mode has no local address to bind. The worker reports the
  tailnet transport as unavailable, with that reason.
- Tailscale ACLs must allow the port between members. `st fleet status` reports a refused
  connection on the tailnet as `tailnet refused`, and the README says to check ACLs.

### Fabric

- **Protocol name.** `st3/fleet/FLEET_ID` by default, so a throwaway fleet never collides with a
  real one on the same machines. `fabric_protocol` overrides it; a migrated fleet can keep the
  exposure name it already uses.
- **Inbound.** The worker runs `fabric expose PROTOCOL --tcp 127.0.0.1:PORT --ephemeral` at start
  and every 60 seconds. The exposure is not persisted in Fabric's configuration, so a crash leaves
  nothing behind after Fabric restarts. `st fleet leave`, `st fleet mode dial-out`, and
  `st uninstall` run `fabric unexpose PROTOCOL`.
- **Outbound.** Before an exchange, when it has no working socket for a member, the worker runs
  `fabric dial NODE_ID PROTOCOL`. Fabric's daemon creates or reuses a local Unix socket and the
  command prints its path. The worker sends the exchange as HTTP over that socket, with the same
  hyper Unix-socket client the local API uses. After a connection failure, it discards the socket
  and dials again.
- **Trust and grants.** Fabric trust and grants are Fabric's business; st never edits
  `peers.toml`. The exposing machine must list the dialer's NodeID with the protocol in its
  `allow` list. When the grant is missing, `fabric probe` answers `unsupported`, and
  `st fleet join` and `st fleet status` print the exact entry to add.
- **Which Fabric.** The `fabric` override, else `PATH` from the login environment.

### Loopback

Loopback covers everything else: two nodes on one machine, which is how the integration tests run,
and any tunnel a person runs themselves, such as `ssh -L`. It has two forms:

- a `[[peers]]` entry with a loopback URL, which is a local override for one name;
- an advertised `loopback` endpoint, which means "this port on the dialer's own loopback reaches
  me". A node publishes it only when it founded the fleet or joined with `--via loopback`, or has
  `advertise_loopback = true`.

### No plain network HTTP

The protocol authenticates but does not encrypt. So the worker binds only loopback and tailnet
addresses, and dials only loopback, tailnet, and Fabric routes. A LAN address is refused, with a
message that names the three transports.

## Compatibility and migration

### During a rollout

| Sender | Receiver | Works because |
|---|---|---|
| Old build | Old build | Unchanged |
| New build, config peers only | Old build | The HMAC headers are unchanged; the old build ignores the member headers |
| Old build | New build | The old build is a config peer that is not a member, so HMAC alone is accepted |
| New member | Old build | The old build lists it as a config peer, so it accepts HMAC alone |
| Newly joined member | Old build | Refused: the old build does not list it. Its data still reaches the old build through new-build members, because every exchange relays every writer's envelopes |

An old build keeps `fleet.*` claims as `unknown` records until it is upgraded, and its
`st doctor` warns about unknown records meanwhile. Upgrade every listening member before inviting
new machines.

### Migrating a config-peer fleet

An existing fleet moves to membership without joining again. It keeps its fleet ID, secret, store,
and history. On each machine, after installing the new build:

```sh
st service restart
st fleet migrate                    # on a machine that stays on
st fleet migrate --dial-out         # on a laptop
```

`st fleet migrate`:

1. Refuses if the store is not bound to the configured fleet ID, or the name has a removal.
2. Creates the member key.
3. Writes `fleet.toml` from the legacy values, with `legacy_peers = true` and `port` taken from
   `peer_listen`. The secret file stays where it is; `secret_file` points at it. `--fabric-protocol NAME` records an existing Fabric
   exposure name, so the worker keeps using it.
4. Appends `fleet.member-admitted` with `via = migration` for this node, and its endpoints. No floor
   is set: the incarnation continues its own writer chain.
5. Rewrites the service units without peer arguments and restarts them.

Now every migrated machine exchanges with member signatures, and legacy exchanges still work, so
old and new builds keep replicating. When `st fleet status` shows that every config peer is
either removed or a member that has signed an exchange with this node:

```sh
st fleet migrate --finish
```

It checks that condition, sets `legacy_peers = false`, and prints the `[[peers]]`,
`peer_listen`, `fleet_id`, and `shared_secret_file` lines to delete from `config.toml`. After
`--finish` on every member, removal is enforced by member keys alone.

A hand-written Fabric dial helper and its launchd agent or systemd unit are no longer needed once
`st fleet status` shows the Fabric route in use for that member. Remove them then.

A config peer that is a laptop listed at an unused port becomes a dial-out member with
`st fleet migrate --dial-out` on the laptop. Every other member then stops dialing it, and the
`[[peers]]` entry is ignored until it is deleted.

### Rollback

Until `--finish`, a migrated machine can go back to the old build: the old build ignores
`fleet.toml`, uses the legacy fields that are still in `config.toml`, and other members accept its
HMAC-only exchanges because `legacy_peers` is still true. After `--finish`, rolling one machine
back requires `legacy_peers = true` on the others again (`st fleet migrate --unfinish`).

## Status and diagnostics

`st fleet status`:

```text
FLEET  5b0c1d8e-6a44-4f0e-9d51-2f7f3c9a0b12   this node: studio (listening)
MEMBER   MODE       ROUTE      LAST CONTACT   STATE
server   listening  tailscale  4s ago         up
laptop   dial-out   -          2h ago         dial-out
old-box  -          -          -              removed 3 days ago by person/ada

INVITES  fleet-invite/q3m7k2 for tablet, expires in 11 minutes (sponsor: studio)
WARNINGS
  config peer old-box is removed; delete its [[peers]] entry
```

New `st doctor` checks:

- `fleet-membership`: a conflicted name; this node removed; a member admitted but never seen.
- `fleet-transports`: a configured transport that is not up; a missing Fabric grant; a tailnet
  refusal.
- `fleet-legacy`: `legacy_peers = true`, and config peers that duplicate members.

`st replication status` lists members and config peers from the fleet view instead of
`configured_peers`, with each one's mode and route.

## Failure cases

- **A leaked code.** Whoever redeems it first, before it expires, becomes a member. The person
  sees the unexpected `fleet.member-admitted` in `st fleet status` and `st fleet invites`, and the
  rightful joiner gets `invite-invalid`. `st fleet remove` ends it. To keep codes off screens and
  out of transcripts, pass them by command substitution
  (`fabric exec laptop -- st fleet join "$(st fleet invite laptop --code-only)"`) or write them to
  a `0600` file with `--code-file`.
- **A replayed code or request.** A code is bound to one member key at first use. A replayed
  request gets an answer only the original joiner can open.
- **A fake sponsor.** It cannot sign with the fingerprinted key, so the joiner refuses its answer.
  The joiner has sent only its request, which reveals neither the token nor any history.
- **A join interrupted halfway.** See [Interrupted join](#interrupted-join).
- **A member offline for weeks.** A dial-out member stays `dial-out` and catches up when it
  returns: one exchange moves up to 512 envelopes each way and repeats at once while envelopes
  move. A listening member is reported down while it is away, then publishes new endpoints if its
  address changed. If it was removed meanwhile, its first exchange gets `member-removed`.
- **Clock skew.** Only the sponsor's clock decides invite expiry. Membership, the writer fence, and
  authentication use keys and writer sequences, never time. Skew affects only displayed times.
- **A removed member comes back.** It gets `member-removed` from every member that has the
  removal. A partitioned member may accept it and relay its envelopes, but the writer fence makes
  every other member refuse them, and the partitioned member refuses them too once it has the
  removal. Joining again needs a new invite and gets a new incarnation.
- **Two machines under one name.** The name becomes conflicted, and every member refuses both keys
  until a person removes one.
- **The sponsor is removed while its invite is open.** `st fleet remove` revokes the invite. The
  token dies with the sponsor.
- **Nobody listens.** If a removal would leave no listening member, `st fleet remove` warns, and
  `st fleet status` warns while it lasts.

## Test plan

Every test uses isolated nodes: temporary `HOME` and XDG directories, relative socket paths short
enough for `SUN_LEN`, ports from binding `127.0.0.1:0`, a new fleet ID, stub `pty`, and `fabric`
and `tailscale` overrides that point at test shims. No test touches the real fleet's state,
services, ports, or Fabric protocol names. The integration harness refuses to start a node whose
`fabric` or `tailscale` override is not a shim.

The worker gains hidden settings for tests: `--anti-entropy-interval-ms` and
`--wake-coalesce-ms`. Their defaults stay 30 seconds and 1 second.

### Unit tests

`crates/st3/src/config.rs`

- `fleet_toml_merges_with_config_and_rejects_different_fleet_ids`
- `a_membership_node_needs_no_config_peers_and_a_dial_out_node_needs_no_listener`
- `advertised_endpoints_accept_only_loopback_tailnet_or_fabric`
- `a_legacy_config_peer_fleet_validates_unchanged`

`crates/st3/src/fleet/code.rs`

- `a_join_code_round_trips`
- `a_damaged_code_fails_its_checksum`
- `an_unknown_code_version_is_refused`
- `a_join_code_never_contains_the_fleet_secret` (property test: the secret's raw, hex, base32,
  and base64 forms never occur in any code)

`crates/st3/src/fleet/handshake.rs`

- `the_handshake_delivers_the_secret_to_the_token_holder`
- `a_wrong_token_gets_the_same_refusal_as_an_unknown_invite`
- `a_replayed_request_gets_an_answer_the_replayer_cannot_open`
- `the_joiner_refuses_a_sponsor_key_that_does_not_match_the_fingerprint`
- `a_second_member_key_cannot_redeem_a_bound_invite`
- `the_same_member_key_can_redeem_again_until_expiry`
- `the_sponsor_clock_decides_expiry`
- `five_failed_proofs_burn_the_invite`
- `a_request_signed_with_another_key_is_refused`

`crates/st3/src/store.rs` (membership and fence)

- `membership_folds_admission_endpoints_removal_and_leave_per_incarnation`
- `the_fold_is_the_same_in_every_receipt_order` (property test over permutations)
- `a_stale_admission_cannot_undo_a_removal_of_the_same_key`
- `joining_again_with_a_new_key_is_a_new_incarnation`
- `endpoints_and_leave_count_only_from_the_members_own_writer`
- `two_current_keys_for_one_name_are_conflicted`
- `a_removed_writers_envelopes_outside_every_window_are_fenced_not_dropped`
- `a_writers_own_claims_cannot_lift_its_fence`
- `fenced_records_are_reconsidered_when_membership_changes`
- `the_writer_floor_puts_a_rejoined_writer_above_its_old_chain`
- `a_used_name_can_be_joined_again_only_by_an_empty_store_after_removal`
- `the_floor_counts_fenced_envelopes_and_every_high_water`
- `fleet_claims_are_refused_on_the_public_claim_api`
- `an_old_build_keeps_fleet_claims_as_unknown_records` (the unknown-kind admission path)

`crates/st3/src/peer.rs`

- `a_member_request_needs_a_valid_member_signature`
- `a_legacy_config_peer_is_accepted_with_hmac_alone`
- `a_member_cannot_fall_back_to_hmac_alone_after_finish`
- `a_removed_member_gets_a_signed_member_removed_refusal`
- `a_node_that_learns_it_was_removed_stops_dialing`
- `a_member_removed_refusal_for_an_older_key_is_transient`
- `an_unknown_node_is_refused`
- `responses_carry_a_member_signature_the_dialer_checks`
- `the_dial_set_comes_from_membership_and_skips_dial_out_members`
- `routes_are_tried_override_then_tailscale_then_fabric_then_loopback`
- `a_dial_out_member_never_appends_transport_observations`
- `a_listening_member_records_a_dial_out_exchange_only_locally`
- `the_fabric_route_dials_through_the_cli_and_dials_again_after_a_lost_socket` (shim)
- `the_fabric_exposure_is_ephemeral_and_removed_on_leave` (shim)
- `the_tailnet_listener_binds_only_tailscale_addresses_in_the_tailnet_ranges` (injected
  interface and command output)
- `the_join_route_is_absent_without_invites_and_bounds_body_size_and_rate`
- `the_secret_and_token_never_appear_in_errors` (every error path of `FleetAuth`, the handshake,
  and the worker, formatted with `{:#}`)

`crates/st3/src/service.rs`

- `membership_units_carry_no_peer_fleet_or_secret_arguments`
- `legacy_units_with_peer_arguments_still_parse`
- `uninstall_lists_every_owned_path` (the `--dry-run` inventory against a fake home)

`crates/st3/src/main.rs`

- `fleet_help_lists_invite_invites_join_remove_leave_mode_migrate_status`
- `fleet_commands_refuse_inside_a_seat_without_an_explicit_person`
- `fleet_join_reads_a_code_from_standard_input`
- `uninstall_requires_erase_local_graph_for_a_local_only_store`

`crates/st3/src/api/client_v0.rs`

- `a_dial_out_member_is_dial_out_with_last_contact_not_unreachable`
- `a_removed_member_is_history_not_current`

### Integration tests: `crates/st3/tests/fleet.rs`

Each test starts real `st3 up` and `st3 replication-worker` processes from `CARGO_BIN_EXE_st3`,
in the foreground, and drives them with the `st` CLI. For each test, the line after "fails if"
is the assertion that would catch a broken feature.

1. `invite_and_join_sync_full_history`: A holds 2,000 claims and a blob-backed document. B joins
   with a code from A over loopback; then C joins with a code from B. Fails if the three
   authority digests differ, a write on any node is missing on another, or any node has a
   `[[peers]]` entry or `--peer` argument.
2. `a_dial_out_member_is_caught_up_and_never_reported_down`: A listens, L joins with `--dial-out`.
   Stop L, write 600 claims on A (more than one exchange carries), wait three anti-entropy
   periods, start L. Fails if L lacks any of them, if any node has a `transport.observed` claim
   with status `down` for `host/L`, or if A's status shows an outbound attempt to L.
3. `a_removed_member_is_refused`: remove B on A. Fails if B's next exchange is accepted, if B keeps
   dialing, or if a claim B writes after the removal reaches A.
4. `a_partitioned_member_cannot_relay_a_removed_writer`: C cannot reach A. Remove B on A. B
   exchanges with C and writes. Reconnect C. Fails if B's new envelopes are admitted on A, if
   they are not kept on A as `fenced`, or if C still admits new ones after it has the removal.
5. `a_removed_member_cannot_pose_as_another_member`: B, removed, signs an HMAC-valid exchange as
   C with its own key, and with no member key. Fails if A accepts either.
6. `an_interrupted_join_resumes_with_the_same_code`: the joiner exits after the sponsor binds
   the invite and before it stores the secret (a test-only fault point). Fails if running `join`
   again with the same code does not complete, or if a different machine can redeem that code.
7. `expired_revoked_and_used_codes_are_refused`: a 10-second invite after it expires, a revoked
   invite, and a used invite. Fails if any is redeemed, or if the refusals differ.
8. `a_wiped_member_joins_again_under_its_old_name`: B joins and writes; C is cut off from A. A
   removes B. B keeps writing and exchanges only with C. B's state is deleted, and it joins again
   as B through A; then C reconnects. Fails if any two admitted batches share `(origin B,
   sequence)`, if B's history from before the removal is missing, or if anything the old B wrote
   after the removal is admitted on any node. A second store that has already written as B must
   be refused.
9. `leave_drains_local_writes_first`: B writes while A is stopped, starts A, and runs
   `st fleet leave`. Fails if leave finishes before A holds B's writes and the leave claim, or if
   B's secret or key remains.
10. `uninstall_leaves_nothing_behind`: snapshot the isolated home before B is installed; join,
    write, `st uninstall --yes --keep-binaries`. Fails if the tree differs from the snapshot, if
    the Fabric shim still has an exposure, or if A does not show B as left.
11. `the_fabric_transport_works_through_the_worker_alone`: nodes advertise only Fabric endpoints
    through the shim, which records `expose` calls and prints harness-owned Unix sockets that proxy
    to each node's loopback port. Fails if sync does not converge, or if any process other than
    the worker invoked the shim.
12. `the_secret_never_leaves_its_file`: run a join and a dial-out catch-up as in tests 1 and 2,
    then search every file under every isolated home except the secret files, every process's
    arguments, every claim, and every document for the secret's raw, hex, base32, and base64
    forms. Fails on any match.
13. `a_config_peer_fleet_migrates_to_membership`: A and B start with legacy `[[peers]]` and baked
    arguments, as the running fleet does, and exchange data. Migrate both and finish. Fails if the
    store's fleet binding or history changes, if replication pauses during the migration, or if
    exchanges after `--finish` lack member signatures. A third node L, listed on A at a port
    nothing serves, migrates with `--dial-out`; fails if A dials L afterwards or records it down.
14. `an_old_build_config_peer_replicates_with_new_members` (ignored unless `ST3_COMPAT_BIN` is set):
    O runs the v0.3.0 release binary and lists N1 as a config peer. N1 is a new-build member that
    lists O as a config peer. N2 joins N1 through an invite, so its writes reach O only through
    N1. Fails if a write on any node is missing on another, or if O holds any `invalid` record
    (its `fleet.*` records must be `unknown`).

The docs step adds `readme_multi_machine_section_runs` to this file. It extracts the commands from
the README section on running st on more than one machine and runs them against isolated nodes.

### CI

- **Nix** (existing `check-x86_64-linux (st3)` and `check-aarch64-darwin`): runs every unit test
  and every `fleet.rs` test except 14, on Linux and macOS, on every pull request.
- **`fleet-compat`** (new workflow `.github/workflows/fleet.yml`, on `ubuntu-22.04` and
  `macos-15`, for pull requests that touch fleet code): downloads the v0.3.0 bundle for the runner,
  verifies its checksum, sets `ST3_COMPAT_BIN`, and runs test 14 with `--ignored --exact`. It fails
  if the variable is empty or the output does not report exactly one test passed.
- **`fleet-e2e`** (new job in the tag release workflow, also run on pull requests that touch fleet
  code, on `ubuntu-22.04` x86_64 and `macos-15` arm64): installs from the bundle the same run just
  built, never from cargo, and runs `scripts/fleet-e2e --bin-dir DIR`. The script runs node A as a
  user service (launchd on macOS; systemd with lingering enabled on Linux) and node B in the
  foreground, both isolated. It invites, joins, writes on each, waits until both writes are on
  both, removes B, uninstalls both, and checks that no unit, plist, state, config, or socket
  remains. If the runner cannot provide a user service manager, the job fails; it never skips.
  Publication `needs` this job.

"Fleet code" is `crates/st3/src/{peer,config,service}.rs`, `crates/st3/src/fleet/**`,
`crates/st3/tests/fleet.rs`, `crates/st3-schema/**`, `scripts/fleet-*`, and the workflow files
themselves.

### Live test: `scripts/fleet-live-test`

It runs from the Linux maintainer machine against a macOS machine reached with `fabric exec`, on a
throwaway fleet. Machine names are arguments, never committed.

```sh
scripts/fleet-live-test --remote REMOTE --release v0.3.1
```

Setup and isolation:

- Download the release bundles for both machines, verify their checksums, and install them under
  a temporary root on each machine (short paths, for `SUN_LEN`). Check `st --version`.
- Found a new fleet, so the fleet ID and the Fabric protocol are new. Node names are
  `live-a-RUN` and `live-b-RUN`. Ports are free ports above 40000 that no local `config.toml`
  names.
- Run every node as a foreground process with a pid file; on the remote machine, start it with
  `nohup` through `fabric exec`. Never install a service.
- Before starting, record hashes of each machine's real st3 unit files or plists and of Fabric's
  `peers.toml`, and check that the real st3 services are running. The Fabric phase adds a grant
  for the throwaway protocol to `peers.toml` on each machine and reloads Fabric; the teardown
  restores the file byte for byte.
- Check that Tailscale is up on both machines and the port is allowed. Fail with the reason if
  not; never skip a phase.

Phases:

1. **Tailscale.** A founds the fleet and invites `--via tailscale`; B joins. Write a document on
   each; both must see both within 60 seconds.
2. **Dial-out.** `st fleet mode dial-out` on B. Stop B, write 600 claims on A, start B. B must
   catch up, A must hold no `down` observation for B, and A's status must show no outbound
   attempt to B.
3. **Remove and uninstall.** Remove B on A; B's next exchange must be refused and B must stop
   dialing. `st uninstall --yes --keep-binaries` on B; nothing of B's may remain.
4. **Fabric.** A new B joins with `--via fabric`, and phases 1 to 3 repeat over Fabric.
5. **Teardown.** Stop every process, run `st uninstall` on A, restore the Fabric files, delete both
   temporary roots, compare the recorded hashes, and check that the real st3 services still run.
   Any difference fails.

The script traps every exit to tear down and exits non-zero on any failure.

### Gates per pull request

Merge `origin/main` in before testing and again before merging. A pull request merges when every
check passes: the Nix checks, `fleet-compat` and `fleet-e2e` when fleet code changed, and the
existing checks. The live test runs before v0.3.1 is announced, not per pull request.

## README section

The docs step adds "Run st on more than one machine" to the README, in this order, with invented
names:

1. Install from a release on each machine.
2. Join over Tailscale: `st fleet invite NAME` on a machine that stays on, `st fleet join` on the
   new one, and `st fleet status` on both. Include the tailnet ACL note.
3. Join over Fabric: the one-time Fabric trust and the per-fleet grant, then the same two commands,
   with the `fabric exec ... "$(...)"` form.
4. A laptop that is often offline: `--dial-out`, what `dial-out` means in `st machines`, and
   `st fleet mode`.
5. Removing a machine: `st fleet remove` on another member, then `st uninstall` on the machine.
6. Moving a fleet configured with `[[peers]]` to membership: `st fleet migrate`, then `--finish`.

## Pull requests

Small pull requests, in order, each from a branch off `origin/main`:

1. **Membership in the graph.** Schema kinds and the `fleet-invite` family, the `fleet_members`
   projection, the writer fence and `fenced` state, the data authority rows, regenerated schema
   docs and clients. Nothing writes the claims yet.
2. **Member keys on the wire.** Node key files, the member headers on requests and responses, and
   the acceptance table with legacy acceptance.
3. **Peers from membership.** `fleet.toml`, config validation, the membership endpoint, the dial
   and accept sets, dial-out mode and its observation rules, units without peer arguments, and
   the status, doctor, and machines views.
4. **Transports.** The tailnet listener and discovery, Fabric expose and dial through the CLI,
   advertised loopback endpoints, and route order.
5. **Invite and join.** The code, the handshake, founding, `st fleet invite`, `invites`, and `join`,
   the integration harness, and `fleet.rs` tests 1, 2, 6, 7, 11, and 12.
6. **Remove, leave, and uninstall.** Those commands, the installer manifest, and tests 3, 4, 5,
   8, 9, and 10.
7. **Migration.** `st fleet migrate`, `--finish`, `--unfinish`, test 13, test 14, and the
   `fleet-compat` workflow.

The docs, release-e2e, and live steps follow with the README section, `fleet-e2e`, and
`scripts/fleet-live-test`.

## Not in this design

- **Rotating the fleet secret.** Member keys already make removal enforceable, so the secret is a
  second factor. Rotation can later deliver a new secret sealed to each member key.
- **Confirming a join on the sponsor** with a short code shown on both screens. Expiry, single use,
  pinned names, and delivery by command substitution cover the cases this design targets, and a
  second interactive step would work against scripted joins.
- **Signing envelopes per writer.** Members are fully trusted.
- **Hubs** for fleets much larger than ten machines.
- **Prefix grants in Fabric** (`st3/fleet/*`), which would make the per-fleet grant a one-time
  step. That is a Fabric change.
- **Remote terminal reads of a dial-out member's seats.**
- **Windows.**
