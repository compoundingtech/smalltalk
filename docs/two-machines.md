# Connect two machines

Your machines can share one fleet: each daemon keeps a local replica of the graph and catches up when it reconnects. This connects **your own machines**; each person currently runs their own fleet.

Finish [getting started](getting-started.md) on the first machine. Install Small Talk, set `person = "person/ada"`, and start its user service on the second machine too. Harnesses and their logins are needed only on machines that will run seats.

The v0.3.4 fresh-machine rehearsal found [invalid claim-signature warnings](https://github.com/compoundingtech/smalltalk/issues/1228): a standalone restart can seal grants unsigned, and later fleet claims depend on them. For a suspected affected store, [capture a bounded read-only audit before founding, joining, doctor or restart operations](st3/founder-signing-audit.md). The prevention fix is included in v0.3.5's source, but upgrading does not repair already-sealed unsigned history; the guide preserves that distinction and the existing rehearsal's limits.

## Join over SSH

This worked example calls the first machine **studio** and the second **beacon**. It uses SSH to encrypt a connection between their loopback listeners. You need SSH access from beacon to studio; use your actual SSH destination when prompted. If you already use Tailscale or Fabric, [fleet join](fleet-join.md#what-a-person-types) gives the shorter route with automatic transport discovery.

On **studio**, found a fleet and check its listener:

```sh
st fleet create --name studio --port 31313 --advertise-loopback
st fleet status
st service status
```

Wait until `status` shows studio's loopback endpoint. Creating the fleet preserves your existing graph. If studio already belongs to your fleet, skip `create`; use its existing Tailscale/Fabric route, or its advertised loopback endpoint and listening port for this example.

On **beacon**, open a second terminal and keep this SSH tunnel running:

```sh
printf 'SSH destination for studio (for example ada@studio): '
read -r studio_ssh
ssh -N -o ExitOnForwardFailure=yes \
  -L 127.0.0.1:31313:127.0.0.1:31313 "$studio_ssh"
```

On **studio**, issue a single-use invitation:

```sh
st fleet invite beacon --via loopback
```

Back in beacon's original terminal, join and paste the code when asked:

```sh
st fleet join --dial-out
st fleet status
st replication status
```

`--dial-out` leaves beacon's loopback port free for the tunnel. Beacon connects to studio, and each connection synchronizes **both directions**. Join installs the replication service and waits for first sync. Keep the tunnel running while using this route; Tailscale and Fabric manage their own connections instead.

If port 31313 is busy on beacon, choose another local tunnel port (such as 31314), and join using that route:

```sh
st fleet join --dial-out --via http://127.0.0.1:31314
```

## Check what arrived

On **both machines**:

```sh
st agents ls
st missions ls --all
st replication status
st replication status --json
st doctor
```

The same declared worker and first mission should appear. `replication status` reports the last successful exchange, backlog, authority digest, graph digest, and table digests. After both members have caught up and writes settle, matching authority digests mean they hold the same replicated history. With matching builds and no waiting claims, matching graph and table digests also confirm the same shared projections. During an upgrade, a reader may hold claims that need a newer build; update it before comparing projections.

If doctor reports invalid claim-signature warnings, retain the report and follow the [operator guidance](st3/founder-signing-audit.md); matching replication digests alone do not establish valid claim signatures.

To show that a write travels back from beacon, send a message **on beacon**:

```sh
st conversations send person/ada --from person/ada \
  --subject 'Replication check' --body 'Hello from beacon.'
```

Read it **on studio** after the next exchange:

```sh
st conversations ls person/ada
```

| Replicates | Stays on the owning machine |
| --- | --- |
| Declarations, missions, runs, step results, messages, reviews, and referenced graph documents/blobs | Checkouts and ordinary workspace files |
| Membership and durable agent state | Harness installations, credentials, private keys, configuration, and live terminal processes |

The graph records a seat's host. Replication does not copy its workspace or move its running harness. Clone a repository and log in locally before declaring a seat on another machine; start a mission run on the machine where its workspace exists.

## A sleeping machine is normal

Disconnect beacon's tunnel with **Ctrl+C**. On studio, inspect the fleet again after about 90 seconds:

```sh
st fleet status
st replication status
st doctor
```

Beacon becomes `last-seen`, with the time of the last exchange. Its absence does not remove it or make a healthy graph an error; transport diagnostics can retain the last failed connection. Authentication or admission failures are different and need investigation. Reopen the tunnel and beacon catches up automatically, with no new invitation. Retries back off, so reconnection can take a little time.

See [replication](st3/replication.md) for digest interpretation and recovery, and [fleet join](fleet-join.md) for Tailscale/Fabric routes, invitations, leaving, and removal.
