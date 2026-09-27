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

When both machines are Fabric peers, the code never has to appear on a screen or in a command
line. `st` sends it as a file to the new machine's Fabric inbox, and `join` reads and deletes it:

```sh
st fleet invite laptop --send-fabric
fabric exec laptop -- st fleet join --dial-out --fabric-inbox
```

To take a machine out:

```sh
st fleet remove laptop --reason "wiped for reinstall"   # on any other member
st uninstall                                            # on the machine itself, if it still runs
```

## Design review responses

The design review of the first version of this document raised seven findings. Each is resolved in this revision:

| Finding | Resolution |
|---|---|
| 1. Critical: removal bypassable through an uninformed relay | Every keyed writer signs its envelopes, and admission checks the signature wherever the envelope came from ([Signed envelopes](#signed-envelopes)). Membership claims count only when signed by a current member or the pinned anchor. [What removal guarantees](#what-removal-guarantees) and [what it does not](#what-removal-does-not-guarantee) state the promise and the partition interval exactly. Test 4 forges and relays envelopes and a membership claim through an uninformed member. |
| 2. High: invite deletion contradicts retry | The sponsor keeps the token, bound to one key and name, until expiry; each retry gets a freshly sealed answer and appends nothing ([Expiry and single use](#expiry-and-single-use)). Test 6 covers a lost answer, an immediate retry, a retry after a sponsor restart, and a retry after expiry. |
| 3. High: leave can fence writes it promises to drain | `leave` stops local writes, drains to an exact condition, and only then appends the leave claim as the last envelope ([`st fleet leave`](#st-fleet-leave)). Test 10 drains 1,500 envelopes across sparse ranges with an interrupted confirmation and a peer restart. |
| 4. High: CI filter skips core fleet changes | `fleet-compat` and `fleet-e2e` have no path filter and run on every pull request once added; a test fails if a filter appears; publication needs both platform legs of `fleet-e2e` ([CI](#ci)). |
| 5. Medium: the v0.3.0 baseline may not exist | The baseline is pinned by checksum in `.github/fleet-compat-baseline.json`; the pull request that adds `fleet-compat` waits for the release and nothing skips or falls back. |
| 6. Medium: the Fabric command exposes the code | `st fleet invite --send-fabric` and `st fleet join --fabric-inbox` move the code as a Fabric file, never in argv. The argv form remains, with its exposure stated. Test 8 checks that a leaked code is visible and revocable; tests 12 and 13 check argument lists. |
| 7. Medium: rejoin separation is bounded | Incarnations are told apart by the key that signed each envelope, not by a sequence gap, so the floor is `H` and the separation is exact ([Reusing a name](#reusing-a-name)). Test 9 covers envelopes at and above the floor and relays after the new incarnation is admitted. |

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
- **Anchor**: the member whose key every member pins as the root of membership: the machine that
  founded the fleet, or the first machine of an existing fleet to migrate.

## Trust model

- Members are fully trusted while they are members, as they are today. A member can write any
  claim, and it runs agents with shell access on its machine.
- The fleet secret authenticates fleet traffic (HMAC-SHA256, unchanged). A removed machine keeps
  its copy, so the secret alone cannot enforce a removal.
- Each member has its own member key. It signs every connection (request and response) and every
  envelope the member writes, including envelopes written before it had the key. A signature binds
  an envelope to one incarnation, wherever the envelope travels afterwards. See
  [Signed envelopes](#signed-envelopes).
- Membership claims count only when they are in an envelope signed by a current member, or by the
  fleet's anchor (the founder, or the first migrated member). Every member pins the anchor's key
  when it joins, from the authenticated handshake.
- The fleet secret is never in a join code, a claim, a document, a log line, an error message,
  a command line, or a service file. It exists only in the secret file on each member and, during
  a join, inside one sealed handshake response.
- Tailscale or Fabric encrypts traffic between machines. The replication protocol is plain HTTP
  and is never exposed on another network interface.

### What removal guarantees

Once a member has received the removal of an incarnation, it:

- refuses every connection from that incarnation;
- refuses every envelope signed by that incarnation's key with a sequence above the removal's
  `high_water`, whichever member relays it;
- refuses every envelope under any keyed writer that is not signed by that writer's key, so a
  removed machine cannot write as another member.

A holder of the fleet secret can therefore get nothing past a member that has the removal and the
admissions of the writers involved.

### What removal does not guarantee

These gaps are deliberate and have tests that pin down their exact extent:

- **The partition interval.** A member that has not yet received a removal still treats the
  removed incarnation as a member. It accepts that machine's connections and admits its new signed
  envelopes, and it relays them. Every member that has the removal refuses them. The uninformed
  member keeps what it admitted before it learned; `st doctor` on that member reports each such
  envelope as `admitted beyond high water`, so the difference is visible. Removing those envelopes
  from its graph after the fact is not in this design.
- **Writers a member does not yet know are keyed.** Until a member has a writer's admission, it
  admits that writer's unsigned envelopes as today. After it has the admission, it refuses them.
  `st doctor` reports unsigned envelopes it admitted earlier from a writer it now knows is keyed.
- **Legacy writers.** A writer that never had a member key cannot be authenticated. Anyone with
  the secret can inject envelopes under a legacy writer's name at a member that accepts the
  connection. For a legacy writer that was removed, members refuse its envelopes above the
  removal's `high_water`, but still accept unsigned candidates at or below it. Migration closes
  this gap for each writer as it gets a key.
- **Old builds** authenticate nothing beyond the secret.

For a machine that was lost or stolen: remove it on every member you can reach, and finish
migration (`st fleet migrate --finish`) on every member, so that no member accepts a legacy
exchange. Rotating the secret is not in this design; once every writer is keyed, the secret is no
longer what keeps anyone out.

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
st fleet invite [NAME] [--expires DURATION] [--via auto|tailscale|fabric|loopback] [--migrate]
                [--send-fabric | --code-only | --code-file PATH] [--as PERSON]
st fleet invites [--all]
st fleet invites revoke fleet-invite/ID --reason TEXT
```

- `NAME` pins the node name the new machine must use. Without it, the joiner chooses a name.
- `--expires` defaults to 15 minutes. It accepts 10 seconds to 24 hours.
- `--via` chooses which of the sponsor's endpoints go into the code. `auto` includes every
  endpoint the sponsor advertises. `loopback` adds the loopback endpoint, for tests on one machine
  and for operator tunnels.
- `--send-fabric` writes the code to a new `0600` temporary file, sends it with
  `fabric send-file NAME FILE --as st-fleet-join-ID.code`, and deletes the local file. The code
  is never printed and never on a command line. `st fleet join --fabric-inbox` on the other
  machine finds the one `st-fleet-join-*.code` file in its Fabric inbox, reads it, and deletes it.
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
Or, if laptop is a Fabric peer of this machine, send the code instead of showing it:
  st fleet invite laptop --send-fabric
  fabric exec laptop -- st fleet join --fabric-inbox
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
  expiry, bound member key, bound name, failed-attempt count, and the admission claim once there
  is one. This table does not replicate and survives a sponsor restart. It is the same class as
  the local `capabilities` table in [data authority](st3/data-authority.md).
- The row keeps the token until the invite expires or is revoked, even after redemption, so that
  the bound key can redeem again (see below). Then the sponsor erases the token and keeps the row
  with the token column cleared, so a late request still gets `invite-invalid`. The sponsor sweeps
  expired rows at startup and every minute.

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
- An invite binds to the first member key that proves the token, together with the name it asked
  for. Until the invite expires, that key can redeem it again with the same name, as often as it
  needs to. Each redemption returns a freshly sealed answer. Only the first appends claims; later
  ones append nothing. Any other key is refused. This is how an interrupted join resumes.
- After expiry, nothing can redeem the invite. A joiner that never received its answer is then an
  admitted member that is never seen. `st fleet status` shows it as `admitted, never seen`, and
  the recovery is `st fleet remove NAME` followed by a new invite.
- Five failed proofs burn the invite. The sponsor records `fleet.invite-revoked` with reason
  `too-many-failures` and erases the token.
- `st fleet invites revoke` works on any member. It takes effect when the claim reaches the
  sponsor, which then erases the token. `st fleet invites` shows whether the sponsor has
  acknowledged it.
- `st fleet invites` shows, for each invite, whether it was redeemed, when, by which name and key
  fingerprint, and whether that member has been seen since. A redemption appears on the sponsor
  at once and everywhere else within one exchange.
- The join route answers with one generic refusal, `invite-invalid`, for every failure: unknown
  invite, expired, revoked, wrong proof, or bound to another key. It never says which. The joiner
  then prints: "If you did not use this code before, someone else may have. Check
  `st fleet invites` on a member."
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
`fleet.member-admitted` in the same store transaction. A retry by the same key and name appends
nothing and gets a freshly sealed answer.

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
  "sealed": "ChaCha20-Poly1305(k, nonce, aad, {fleet_id, secret, anchor_key, writer_floor, fabric_protocol, admitted_claim})",
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
st fleet join [CODE | - | --code-file PATH | --fabric-inbox] [--name NAME] [--dial-out]
              [--via auto|tailscale|fabric|URL] [--no-service] [--wait DURATION] [--as PERSON]
```

With no `CODE`, `join` prompts for it without echo. `-` reads it from standard input.
`--code-file PATH` and `--fabric-inbox` read it from a file and delete the file after a successful
redemption.

A code on the command line works too, but then it is visible in the process list of that machine
while `join` runs, and it may be kept by whatever ran the command, such as shell history or an
agent transcript. It is a bearer credential until it expires or is redeemed, though not the fleet
secret. Prefer the prompt, a file, or `--fabric-inbox`. `join` never logs it.

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
checks the sponsor's responses against the key from the handshake, which `join.json` keeps. It
writes the anchor key from the handshake to `fleet.toml` as `anchor_key`, and it signs every
envelope it already holds under its own name (see [Signed envelopes](#signed-envelopes)).

The new member then dials every listening member it can route to. A member that has not yet
received the admission claim refuses with `not-a-member`; the new member treats that as transient
for 10 minutes after its admission and does not record it as a failure.

### Interrupted join

| Interrupted after | State | Recovery |
|---|---|---|
| Step 1 to 3 | Nothing on the sponsor | Run `join` again with the same code |
| Step 5, the answer lost | Invite bound to this key; member admitted | Run `join` again with the same code before it expires; the same key redeems again, also after a sponsor restart |
| Step 5, the answer lost, then the code expired | Member admitted, never seen | `st fleet remove NAME` on a member, then a new invite |
| Step 5, after `redeemed` | Secret stored | Run `join` again without a code |
| Step 6 or 7 | Member, services may be down | Run `join` again; it starts the services and waits |

If the machine loses its key before `redeemed` (for example, it is wiped), the invite is bound to
a key that no longer exists. It is the same as an expired, lost answer: remove the stranded member
and issue a new invite.

### Reusing a name

A machine that is wiped and joins again under its old name is a new incarnation with a new key.
The fleet tells its envelopes from the old incarnation's by the key that signed them, not by
their sequence numbers (see [Writer fence](#writer-fence)). The floor only keeps the new
incarnation from reusing a sequence number that an admitted envelope already has.

A name has **history** when the sponsor holds any envelope from that writer or any membership
claim for that name. At redemption, the sponsor applies these rules:

- The name has a current member, or it has history and no ended incarnation (a config peer that
  was never removed): refuse. Remove it first.
- The name has no history: admit with no floor. The new incarnation's window starts at 1.
- The name has history, and the joiner's `writer_head` is empty: admit with
  `writer_floor = H`, where `H` is the highest of every sequence the sponsor holds for that
  writer, admitted or not, and every `high_water` in the name's removal and leave claims. The new
  incarnation's window starts at `H + 1`.
- The name has history, and the joiner's `writer_head` is not empty: refuse. This store has
  already written as that name; reset it or choose another name.

The joiner's daemon stores the floor in `meta` before its first batch, and
`next_replica_sequence` uses the larger of the floor and its local maximum. The joiner stops its
daemon before it reads its head (step 4), so no batch is written between the check and the floor.

Another member may hold envelopes the old incarnation wrote after its removal, with sequences
above `H`. They are signed with the old key, or not signed at all if the old incarnation was a
legacy writer. Either way they fall outside the old incarnation's window and are not signed by the
new key, so no member admits them, even where their sequence numbers equal the new incarnation's.

The same store rejoining after its removal is refused on purpose. Its writes after the removal
were not accepted, and resetting it or choosing a new name keeps one rule for every rejoin.

Runtime ownership follows the node name. Seats declared on host `laptop` belong to whichever
machine is currently `laptop`. That is why `st fleet remove` asks what to do with a machine's
seats first.

### Founding a fleet

`st fleet invite` on a machine that is not in a fleet founds one, and says so:

1. Generate a random fleet ID (UUID v4), a 32-byte secret, and the member key.
2. Write `STATE/fleet/` with `anchor_key` set to this node's own key, bind the store to the fleet
   ID, sign every envelope this store already holds under its own name, and append
   `fleet.member-admitted` with `via = anchor` for this node, then its `fleet.member-endpoints`.
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
| `fleet.member-admitted` | `host/NAME` | The sponsor during a join or a migration; the anchor itself, once | `fleet_id`, `member_key`, `via` (`anchor`, `invite`, `migration`), `sponsor`, `invite`, `mode`, `writer_floor`, `admitted_by` |
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
answer in any receipt order, so it uses sets, keys, and writer sequences, never timestamps:

- An incarnation is `(name, member_key)`. It exists once an authorized `fleet.member-admitted`
  names it.
- An incarnation is **ended** once an authorized `fleet.member-removed` or `fleet.member-left`
  names its key. Ending is permanent. A stale admission from a partitioned member cannot undo it,
  because it names the same key. Joining again means a new key, which is a new incarnation.
- `fleet.member-endpoints` counts only when the claim's writer is `name`. The latest one by that
  writer's sequence wins. Only a member can say where it listens.
- `fleet.member-left` counts only when the claim's writer is `name`.
- A removal without `member_key` ends the name's legacy incarnation: the config peer that was
  never a member.
- The **current member** for a name is its one incarnation that has not ended. Two such
  incarnations mean the name is **conflicted**; every member refuses both keys until a person
  removes one. This happens only if two machines are joined or migrated under one name.

A membership claim is **authorized** when the envelope that carries it was admitted as signed by
an incarnation whose window contains that envelope's sequence (see
[Signed envelopes](#signed-envelopes)). The one exception is the anchor's own admission: a
`fleet.member-admitted` with `via = anchor` is authorized only when its `member_key` equals this
node's pinned `anchor_key` and the envelope is signed by that key. Every other anchor claim is
ignored.

Admission and the fold depend on each other: which envelopes are admitted depends on the known
keys, and the known keys come from admitted claims. Each node computes both to a fixed point,
starting from its pinned anchor. It repeats admission and the fold until neither changes, which
takes at most one round per member. A node recomputes only when a new `fleet.*` claim or a new
signature arrives, and the fleet has a handful of such claims, so this is cheap.

The data authority table gains `fleet_members` (projection of `fleet.*` claims),
`fleet_invite_tokens` (local short-lived authority), and `replica_envelope_signatures`
(replicated authority; see below).

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
| Ended incarnation | The ended key, or no key for an ended legacy name | Refuse, 403 `member-removed` (or `member-left`) |
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

### Signed envelopes

Connection signatures prove who is on the other end of one exchange. They say nothing about who
wrote the envelopes inside it, because every exchange relays every writer's envelopes. A member
that has not yet heard of a removal would otherwise relay forged envelopes from a removed
machine to members that have. So each keyed writer signs its own envelopes, and admission checks
those signatures wherever the envelope came from.

**Signing.** A member signs each envelope of its own writer:

```text
signature = Ed25519(member key, "st3-envelope-v1\n" || fleet_id || "\n" || writer || "\n" ||
                    sequence || "\n" || envelope_hash)
```

The envelope hash already covers the writer, sequence, previous hash, accept time, and payload.
The main daemon holds the member key and signs when it appends a batch. When a node gets its key
(founding, joining, migrating), it also signs every envelope it already holds under its own name,
so its whole history is signed. At every start it signs any envelope of its own writer that has no
signature, which covers batches written while it ran an older build. Ed25519 signing and checking take tens of microseconds, and one
exchange carries at most 512 envelopes.

**Storage.** A new table, `replica_envelope_signatures(writer, sequence, envelope_hash,
member_key, signature)`, holds each signature beside its envelope. It is replicated authority:
receipt stores a signature, admission checks it, and nothing rewrites it.

**On the wire.** `ReplicaEnvelope` gains two optional fields, `member_key` and `signature`.
`ReplicationExchange` gains `signature_requests` (identities of envelopes the sender holds but
cannot admit for want of a signature) and `signatures` (signatures for envelopes the other side
already holds). Old builds ignore unknown fields, and when an old build relays an envelope, the
signature is lost. The receiver then holds the envelope unsigned, and asks for the signature in
its next exchange with a new build. Every new build answers with the signatures it has, and the
writer itself has all of its own.

**Admission.** For each envelope from writer `W` at sequence `s`:

1. If this node knows no keyed incarnation of `W`, admit it as today (legacy), unless a removal
   without a key ended `W`'s legacy window below `s`; then it is fenced.
2. Otherwise, find the incarnation of `W` whose window contains `s`. If it is `W`'s legacy window
   (from before `W` had any key), admit the envelope as in rule 1. If it is a keyed incarnation,
   admit the envelope only when it carries a valid signature by that incarnation's key. Without a
   signature it waits in the new admission state `unsigned`. With a wrong signature it is
   `invalid`.
3. If no window contains `s`, the envelope is `fenced`.

`unsigned` and `fenced` records are retried whenever a signature arrives or membership changes,
as `unknown` records already are. Local writes are signed as they are written and never wait.

A decision to admit is not revisited. An envelope admitted under rule 1 before this node learned
that `W` is keyed stays admitted. So does an envelope from an incarnation this node still thought
current. `st doctor` reports both kinds (see [What removal does not guarantee](#what-removal-does-not-guarantee)).

### Writer fence

Each incarnation of a name owns a window of that writer's sequences:

- An incarnation admitted with `writer_floor = F` starts at `F + 1`. Otherwise it starts at 1.
- An ended incarnation stops at the `high_water` in its removal or leave claim: the highest
  sequence of that writer that the claim's author held. A current incarnation has no end.
- A config peer that was never a member has one legacy window from 1 with no end, until a removal
  without a key sets its end.

Windows of one name never overlap, because a new incarnation's floor is at or above every earlier
incarnation's end (see [Reusing a name](#reusing-a-name)). And because a keyed window admits only
envelopes signed by its own key, an old incarnation's late envelopes are refused even where their
sequence numbers fall inside the new incarnation's window.

Removal therefore means: from the moment a member has the removal, it admits nothing more from
that incarnation, from any relay. Writes the machine made before the removal but had not synced
are not accepted either. `st fleet remove` says so.

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

Run on any member that has a member key, for another member or a config peer. The removal claim
counts only because a current member signed it, so a node that has not migrated cannot remove.

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

Run on the member that is leaving. The leave claim must be the last thing this member writes, and
it must reach the fleet after everything else, because a member that admits it refuses this
incarnation from then on.

1. **Quiesce.** Refuse while local seats run, unless `--stop-seats` or `--force`, for the same
   reason as remove. Then put the daemon in `leaving` mode: it refuses every mutating API request
   with `fleet-leaving`, and it stops its own background writes (transport observations, daemon
   heartbeats, lease renewals). From here the local writer's head does not move.
2. **Drain.** Exchange with a listening member, as many rounds as it takes, until that member
   holds every envelope of this writer. The condition is exact: the member's inventory, returned
   in the last exchange, has range digests for this writer equal to ours, and every one of our
   envelopes is admitted there (its response lists none of them in `signature_requests`).
3. **Leave.** Append `fleet.member-left` with `high_water` set to the sequence of the batch that
   holds it. This is the writer's last envelope.
4. **Confirm.** Exchange that envelope. It is confirmed when the member's inventory lists it with
   the same hash, or when the member refuses the next exchange with a signed `member-left` that
   names this key. Either one is proof that the member admitted it. If the exchange is cut off,
   repeat step 4; the drain in step 2 means there is nothing else left to lose.
5. **Clean up.** Stop the worker, remove the Fabric exposure, and delete `STATE/fleet/`.
6. **Restart** the daemon local-only. The store keeps its history and stays bound to the fleet ID.
   Joining the same fleet later works. Joining another fleet needs `st service reset` first.

`leave` keeps a checkpoint in `STATE/fleet/leave.json`, so running it again after an interruption
resumes at the same step. If `leaving` mode is interrupted before step 3, `st fleet leave --cancel`
returns the daemon to normal.

`--offline` skips steps 2 to 4 when no listening member is reachable, and prints the
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
and history. Every node keeps replicating throughout. After installing the new build everywhere
and restarting the services:

```sh
# On one machine that stays on. It becomes the anchor.
st fleet migrate --anchor

# For each other machine, on any migrated member:
st fleet invite server --migrate --send-fabric
# and on that machine:
st fleet migrate --fabric-inbox          # add --dial-out on a laptop
```

A migration code works like a join code, and it can be pasted or read from a file the same ways.
The handshake is the same, except that the sealed answer carries no secret, because the node
already has it.

`st fleet migrate`, on every node:

1. Refuses if the store is not bound to the configured fleet ID, or the name has a removal.
2. Creates the member key.
3. Writes `fleet.toml` from the legacy values, with `legacy_peers = true` and `port` taken from
   `peer_listen`. The secret file stays where it is; `secret_file` points at it.
   `--fabric-protocol NAME` records an existing Fabric exposure name, so the worker keeps using it.
4. Pins the anchor key. With `--anchor`, that is its own key. Otherwise it comes from the
   handshake.
5. Signs every envelope it holds under its own name.
6. With `--anchor`, appends `fleet.member-admitted` with `via = anchor` for itself. Otherwise the
   sponsor has already appended `fleet.member-admitted` with `via = migration` during the
   handshake. Either way there is no floor: the incarnation's window starts at 1 and covers the
   node's whole history, which it has just signed.
7. Publishes its endpoints, rewrites the service units without peer arguments, and restarts them.

A node must be migrated by a code from an existing member, except the anchor. Otherwise anyone
with the secret could mint a member key for any name.

Now every migrated machine exchanges with member signatures, and legacy exchanges still work, so
old and new builds keep replicating. When `st fleet status` shows that every config peer is
either removed or a member that has signed an exchange with this node:

```sh
st fleet migrate --finish
```

It checks that condition, sets `legacy_peers = false`, and prints the `[[peers]]`,
`peer_listen`, `fleet_id`, and `shared_secret_file` lines to delete from `config.toml`. After
`--finish` on every member, no member accepts a legacy exchange.

A hand-written Fabric dial helper and its launchd agent or systemd unit are no longer needed once
`st fleet status` shows the Fabric route in use for that member. Remove them then.

A config peer that is a laptop listed at an unused port becomes a dial-out member when it migrates
with `--dial-out`. Every other member then stops dialing it, and the `[[peers]]` entry is ignored
until it is deleted.

A config peer that will be wiped does not need to migrate. Remove it with `st fleet remove NAME`
on a migrated member, wipe it, and join it again with a normal invite.

### Rollback

Until `--finish`, a migrated machine can go back to the old build. The old build ignores
`fleet.toml` and uses the legacy fields still in `config.toml`, and the other members accept its
HMAC-only exchanges because `legacy_peers` is still true. Its new envelopes are unsigned, so
members that know its key hold them as `unsigned` until it runs a new build again and signs them.
Nothing is lost. After `--finish`, rolling one machine back requires `legacy_peers = true` on the
others again (`st fleet migrate --unfinish`).

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
  rightful joiner gets `invite-invalid` and a hint to check `st fleet invites`, which shows the
  redeeming name, key fingerprint, and time. `st fleet remove` ends the stray member, and
  `st fleet invites revoke` kills a code that leaked before use. To keep codes off screens,
  command lines, and transcripts, use `--send-fabric` with `--fabric-inbox`, or `--code-file`.
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
  removal. A partitioned member may accept it and relay its new envelopes, but they are signed by
  an ended incarnation above its `high_water`, so every member that has the removal refuses them.
  It cannot write as anyone else, because it lacks their keys. The partitioned member refuses it
  too once it has the removal, and `st doctor` there reports what it admitted in between. Joining
  again needs a new invite and gets a new incarnation.
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
- `send_fabric_leaves_no_local_code_file_and_fabric_inbox_deletes_the_received_one` (shim)

`crates/st3/src/fleet/handshake.rs`

- `the_handshake_delivers_the_secret_to_the_token_holder`
- `a_wrong_token_gets_the_same_refusal_as_an_unknown_invite`
- `a_replayed_request_gets_an_answer_the_replayer_cannot_open`
- `the_joiner_refuses_a_sponsor_key_that_does_not_match_the_fingerprint`
- `a_second_member_key_cannot_redeem_a_bound_invite`
- `the_same_member_key_can_redeem_again_until_expiry_without_new_claims`
- `a_bound_invite_survives_a_sponsor_restart`
- `an_expired_bound_invite_erases_its_token_and_refuses_the_bound_key`
- `a_bound_key_cannot_redeem_under_another_name`
- `the_sealed_answer_carries_the_anchor_key`
- `the_sponsor_clock_decides_expiry`
- `five_failed_proofs_burn_the_invite`
- `a_request_signed_with_another_key_is_refused`

`crates/st3/src/store.rs` (signed envelopes)

- `local_batches_are_signed_with_the_member_key`
- `keying_signs_every_envelope_already_held_under_the_own_writer`
- `startup_signs_own_envelopes_written_without_a_signature`
- `a_keyed_writers_envelope_needs_the_signature_of_the_incarnation_whose_window_holds_it`
- `an_unsigned_envelope_of_a_keyed_writer_waits_as_unsigned_and_is_admitted_when_its_signature_arrives`
- `a_wrong_signature_is_invalid`
- `an_envelope_signed_by_another_members_key_is_invalid`
- `an_envelope_outside_every_window_is_fenced`
- `an_old_incarnations_envelope_at_a_new_incarnation_sequence_is_refused`
- `envelopes_at_the_floor_and_just_above_it_are_placed_in_the_right_window`
- `a_legacy_writer_is_admitted_unsigned_until_a_keyless_removal_ends_its_window`
- `signature_requests_are_answered_with_held_signatures`
- `admitted_envelopes_are_not_revisited_and_doctor_reports_them`

`crates/st3/src/store.rs` (membership and fence)

- `membership_folds_admission_endpoints_removal_and_leave_per_incarnation`
- `the_fold_is_the_same_in_every_receipt_order` (property test over permutations)
- `a_stale_admission_cannot_undo_a_removal_of_the_same_key`
- `membership_claims_count_only_when_signed_by_a_current_incarnation_in_its_window`
- `only_the_pinned_anchor_key_can_self_admit`
- `admission_and_membership_reach_the_same_fixed_point_in_every_order` (property test)
- `joining_again_with_a_new_key_is_a_new_incarnation`
- `endpoints_and_leave_count_only_from_the_members_own_writer`
- `two_current_keys_for_one_name_are_conflicted`
- `a_removed_writers_envelopes_outside_every_window_are_fenced_not_dropped`
- `a_writers_own_claims_cannot_lift_its_fence`
- `fenced_records_are_reconsidered_when_membership_changes`
- `the_writer_floor_puts_a_rejoined_writer_above_its_old_chain`
- `a_used_name_can_be_joined_again_only_by_an_empty_store_after_removal`
- `the_floor_counts_every_held_envelope_and_every_high_water`
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
- `leaving_mode_refuses_mutating_requests_and_stops_background_writes`
- `leave_appends_the_leave_claim_only_after_the_drain_condition_holds`
- `leave_resumes_from_its_checkpoint_and_cancel_restores_normal_mode`
- `a_signed_member_left_refusal_confirms_the_leave`

`crates/st3/src/api/client_v0.rs`

- `a_dial_out_member_is_dial_out_with_last_contact_not_unreachable`
- `a_removed_member_is_history_not_current`

### Integration tests: `crates/st3/tests/fleet.rs`

Each test starts real `st3 up` and `st3 replication-worker` processes from `CARGO_BIN_EXE_st3`,
in the foreground, and drives them with the `st` CLI. Tests that need a forged or replayed message
use a small harness client that signs with keys and the secret taken from the isolated nodes'
files. For each test, the "fails if" clause is the assertion that would catch a broken feature.

1. `invite_and_join_sync_full_history`: A holds 2,000 claims and a blob-backed document. B joins
   with a code from A over loopback; then C joins with a code from B. Fails if the three
   authority digests differ, a write on any node is missing on another, any node has a
   `[[peers]]` entry or `--peer` argument, or any admitted envelope of A, B, or C lacks a valid
   signature on any node.
2. `a_dial_out_member_is_caught_up_and_never_reported_down`: A listens, L joins with `--dial-out`.
   Stop L, write 600 claims on A (more than one exchange carries), wait three anti-entropy
   periods, start L. Fails if L lacks any of them, if any node has a `transport.observed` claim
   with status `down` for `host/L`, or if A's status shows an outbound attempt to L.
3. `a_removed_member_is_refused`: remove B on A. Fails if B's next exchange is accepted, if B keeps
   dialing, or if a claim B writes after the removal reaches A.
4. `a_forged_envelope_relayed_by_an_uninformed_member_is_refused`: A, B, C, and R are members; C is
   cut off from A. Remove R on A. R then gives C three things: an envelope under writer A signed
   with R's key, the same envelope unsigned, and an envelope under its own name above its
   `high_water`, signed with its own key. R also gives C a `fleet.member-admitted` claim that
   admits a fresh key under the name `ghost`. Reconnect C to A and B. Fails if A or B admits any
   of them, if `ghost` is a member anywhere, or if C admits the first two. C admits the third,
   since it did not know of the removal; fails if `st doctor` on C does not report it as
   `admitted beyond high water`, or if C admits anything more from R after it has the removal.
5. `a_removed_member_cannot_pose_as_another_member`: R, removed, sends A an HMAC-valid exchange
   signed as C with R's key, and one with no member key. Fails if A accepts either.
6. `an_interrupted_join_resumes_with_the_same_code`: a test-only fault point drops the sponsor's
   answer after it binds the invite. Fails unless: an immediate retry completes; a retry after
   the sponsor restarts completes; a second retry appends no claims; a different key with the
   same code is refused; and after expiry the bound key is refused and `st fleet status` shows
   the member as `admitted, never seen`.
7. `expired_revoked_and_used_codes_are_refused`: a 10-second invite after it expires, a revoked
   invite, and a used invite. Fails if any is redeemed, or if the refusals differ.
8. `a_leaked_code_is_visible_and_can_be_revoked`: a stranger redeems a code first. Fails if
   `st fleet invites` on the sponsor and on another member does not show the stranger's name, key
   fingerprint, and time within one exchange, if the rightful joiner's error does not point at
   `st fleet invites`, or if the stranger still exchanges after `st fleet remove`. A second
   leaked code is revoked before use; fails if it can be redeemed.
9. `a_wiped_member_joins_again_under_its_old_name`: B joins and writes; C is cut off from A. A
   removes B. B keeps writing and exchanges only with C, so C holds envelopes of the old B above
   A's `H`. B's state is deleted, and it joins again as B through A, writing at `H + 1` and above;
   then C reconnects. Fails if any node admits an old-B envelope above its `high_water`, if any two
   admitted batches share `(origin B, sequence)`, if a new-B envelope at `H + 1` is refused, or if
   B's history from before the removal is missing. A second store that has already written as B
   must be refused.
10. `leave_drains_everything_before_it_leaves`: B writes 1,500 claims while A is stopped, so the
    drain needs at least three exchanges and several sparse ranges; then A starts and B runs
    `st fleet leave`. The first confirming exchange is cut off by a fault point, and A restarts
    once during the drain. Fails if the leave claim reaches A before every other B envelope, if
    any B envelope is missing on A afterwards, if leave does not complete, or if B's secret or key
    remains.
11. `uninstall_leaves_nothing_behind`: snapshot the isolated home before B is installed; join,
    write, `st uninstall --yes --keep-binaries`. Fails if the tree differs from the snapshot, if
    the Fabric shim still has an exposure, or if A does not show B as left.
12. `the_fabric_transport_works_through_the_worker_alone`: nodes advertise only Fabric endpoints
    through the shim, which records `expose` calls and prints harness-owned Unix sockets that proxy
    to each node's loopback port. The code travels with `--send-fabric` and `--fabric-inbox`.
    Fails if sync does not converge, if any process other than the worker and `st fleet` invoked
    the shim, or if the code appears in any recorded argument list.
13. `the_secret_never_leaves_its_file`: run a join and a dial-out catch-up as in tests 1 and 2,
    recording every process's arguments throughout. Then search those arguments, every file under
    every isolated home except the secret files, every claim, and every document for the secret's
    raw, hex, base32, and base64 forms. Fails on any match.
14. `a_config_peer_fleet_migrates_to_membership`: A and B start with legacy `[[peers]]` and baked
    arguments, as the running fleet does, and exchange data. A migrates with `--anchor`; B
    migrates with a code from A; both finish. Fails if the store's fleet binding or history
    changes, if replication pauses during the migration, if any envelope of A or B lacks a valid
    signature afterwards, or if exchanges after `--finish` lack member signatures. A third node L,
    listed on A at a port nothing serves, migrates with `--dial-out`; fails if A dials L afterwards
    or records it down. A fourth node that self-admits without a code must not become a member.
15. `an_old_build_config_peer_replicates_with_new_members` (ignored unless `ST3_COMPAT_BIN` is set):
    O runs the baseline release binary and lists N1 and N3 as config peers. N1 and N3 are
    new-build members that list O. N2 joins N1 through an invite. N3 is cut off from N1, so N1's
    and N2's writes reach N3 only through O, which drops their signatures. Fails if a write on any
    node is missing on another, if N3 does not end up admitting N1's and N2's envelopes through
    signature requests, or if O holds any `invalid` record (its `fleet.*` records must be
    `unknown`).

The docs step adds `readme_multi_machine_section_runs` to this file. It extracts the commands from
the README section on running st on more than one machine and runs them against isolated nodes.

### CI

- **Nix** (existing `check-x86_64-linux (st3)` and `check-aarch64-darwin`): runs every unit test
  and every `fleet.rs` test except 15, on Linux and macOS, on every pull request.
- **`fleet-compat`** (new workflow `.github/workflows/fleet.yml`, on `ubuntu-22.04` and
  `macos-15`, on every pull request with no path filter): downloads the baseline bundle for the
  runner, verifies it against the pinned checksum, sets `ST3_COMPAT_BIN`, and runs test 15 with
  `--ignored --exact`. It fails if the download or checksum fails, if the variable is empty, or if
  the output does not report exactly one test passed. It never skips.
- **`fleet-e2e`** (new job in the tag release workflow, also run on every pull request with no path
  filter, on `ubuntu-22.04` x86_64 and `macos-15` arm64): installs from the bundle the same run just
  built, never from cargo, and runs `scripts/fleet-e2e --bin-dir DIR`. The script runs node A as a
  user service (launchd on macOS; systemd with lingering enabled on Linux) and node B in the
  foreground, both isolated. It invites, joins, writes on each, waits until both writes are on
  both, removes B, uninstalls both, and checks that no unit, plist, state, config, or socket
  remains. If the runner cannot provide a user service manager, the job fails; it never skips.
  The publish job `needs` both platform legs of this job, so no release is published unless the
  installed-bundle join passed on macOS and on Linux.

Neither job has a path filter, so no change in this series can skip them. A unit test in
`crates/st3/tests/fleet.rs`, `fleet_workflows_have_no_path_filter`, parses both workflow files
and fails if a `paths` or `paths-ignore` key appears on the jobs' pull request triggers, or if the
publish job stops needing `fleet-e2e`.

**The compatibility baseline.** The old build is the `v0.3.0` release bundle, the first release
the tag workflow publishes. `.github/fleet-compat-baseline.json` pins its tag, source commit, and
the SHA-256 of each platform's archive. The pull request that adds `fleet-compat` (the last one in
this series) cannot merge until that release exists; if it does not exist yet, that pull request
waits for it. Nothing falls back to building from source or to skipping.

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
4. **Fabric.** A new B joins with `--via fabric`, its code delivered with `--send-fabric` and
   `--fabric-inbox`, and phases 1 to 3 repeat over Fabric.
5. **Teardown.** Stop every process, run `st uninstall` on A, restore the Fabric files, delete both
   temporary roots, compare the recorded hashes, and check that the real st3 services still run.
   Any difference fails.

The script traps every exit to tear down and exits non-zero on any failure.

### Gates per pull request

Merge `origin/main` in before testing and again before merging. A pull request merges when every
check passes: the Nix checks, and `fleet-compat` and `fleet-e2e` from the pull requests that add
them onward, on every pull request. The live test runs before v0.3.1 is announced, not per pull
request.

## README section

The docs step adds "Run st on more than one machine" to the README, in this order, with invented
names:

1. Install from a release on each machine.
2. Join over Tailscale: `st fleet invite NAME` on a machine that stays on, `st fleet join` on the
   new one, and `st fleet status` on both. Include the tailnet ACL note.
3. Join over Fabric: the one-time Fabric trust and the per-fleet grant, then
   `st fleet invite NAME --send-fabric` and `fabric exec NAME -- st fleet join --fabric-inbox`.
4. A laptop that is often offline: `--dial-out`, what `dial-out` means in `st machines`, and
   `st fleet mode`.
5. Removing a machine: `st fleet remove` on another member, then `st uninstall` on the machine.
6. Moving a fleet configured with `[[peers]]` to membership: `st fleet migrate`, then `--finish`.

## Pull requests

Small pull requests, in order, each from a branch off `origin/main`:

1. **This design**, revised for the design review.
2. **Membership and admission.** Schema kinds and the `fleet-invite` family, the `fleet_members`
   projection with anchor authorization, `replica_envelope_signatures`, and the admission rules:
   windows, signature checks, and the `unsigned` and `fenced` states. The data authority rows,
   regenerated schema docs and clients. Nothing writes keys or claims yet, so every node behaves as
   before.
3. **Member keys.** Key files, signing local batches and a node's own history, connection
   signatures, the acceptance table with legacy acceptance, and signature sync in the exchange.
   This pull request also adds the `fleet-compat` workflow, with test 15 in the form the code
   supports so far, so every later pull request runs it. It waits for the `v0.3.0` baseline.
4. **Peers from membership.** `fleet.toml`, config validation, the membership endpoint, the dial
   and accept sets, dial-out mode and its observation rules, units without peer arguments, and
   the status, doctor, and machines views.
5. **Transports.** The tailnet listener and discovery, Fabric expose and dial through the CLI,
   advertised loopback endpoints, and route order.
6. **Invite and join.** The code, the handshake, founding and the anchor, `st fleet invite`,
   `invites`, and `join`, code delivery through the Fabric inbox, the integration harness,
   `fleet.rs` tests 1, 2, 6, 7, 8, 12, and 13, and `scripts/fleet-e2e` with the `fleet-e2e` job
   (this needs the tag workflow from pull request 585 on `main`).
7. **Remove, leave, and uninstall.** Those commands, leaving mode, the installer manifest, and
   tests 3, 4, 5, 9, 10, and 11.
8. **Migration.** `st fleet migrate` with `--anchor`, `--migrate` codes, `--finish`, `--unfinish`,
   test 14, and test 15 in full.

The docs, release-e2e, and live steps follow with the README section, the `v0.3.1` tag, and
`scripts/fleet-live-test`.

## Not in this design

- **Rotating the fleet secret.** Once every writer is keyed and every member has finished
  migration, the secret no longer keeps anyone out on its own. Rotation can later deliver a new
  secret sealed to each member key.
- **Removing envelopes that a member admitted during a partition interval.** `st doctor` reports
  them; excluding admitted claims from a graph after the fact is a separate design.
- **Confirming a join on the sponsor** with a short code shown on both screens. Expiry, single use,
  pinned names, delivery through the Fabric inbox, and prompt visibility of redemptions cover the
  cases this design targets, and a second interactive step would work against scripted joins.
- **Hubs** for fleets much larger than ten machines.
- **Prefix grants in Fabric** (`st3/fleet/*`), which would make the per-fleet grant a one-time
  step. That is a Fabric change.
- **Remote terminal reads of a dial-out member's seats.**
- **Windows.**
