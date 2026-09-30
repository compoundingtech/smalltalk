# Tailscale setup for fleet replication

The replication protocol uses plain HTTP with fleet-secret authentication and member
signatures. Tailscale encrypts traffic between machines; do not expose the listener on
`0.0.0.0`, a LAN address, or the public Internet. The allowed listener and peer IP ranges
are `100.64.0.0/10` and `fd7a:115c:a1e0::/48`, plus loopback for local tunnels. Grant the
chosen TCP port (default 31313) between fleet members in your Tailscale ACL.

## Join two machines

1. Install and connect Tailscale on both. Run `tailscale ip -4` on the listening host
   and confirm the reported address is assigned to a local interface. Check the ACL
   permits the other host to reach its TCP port.
2. Install `st` on both hosts. On the listening host, run `st fleet create` once and
   start `st up` plus `st replication-worker` (or install the st services). The worker
   binds the local loopback and detected Tailscale addresses; `st fleet status` shows
   the advertised endpoints.
3. On that host run `st fleet invite NEW_NAME --via tailscale`. Transfer the one-time
   code privately to the new host. On the new host, with its daemon stopped, run
   `st fleet join` and enter the code at the prompt. Add `--dial-out` on a laptop that
   should not accept inbound connections. Without installed services, start `st up`
   and `st replication-worker` there after the join.
4. Run `st fleet wait` on the new host, then `st replication status` and
   `st fleet status` on both hosts. The authority digests should agree after sync.

Membership and peer routes come from signed claims; a joined node does not need
`[[peers]]` entries or a copied fleet secret. `st fleet` stores its secret and private
member key under the state directory, outside the Nix store. Never put the code, secret,
or key in a command-line argument, committed config, or log.

For manually configured legacy peers instead of fleet membership, set `peer_listen`
to the listener's literal Tailscale IP and set each `[[peers]].url` to
`http://TAILSCALE_IP:PORT` (bracket IPv6 addresses). Both sides must use the same
fleet ID and a protected shared secret file. A peer can be named without a URL when
it only dials this node. See [replication](replication.md) and [fleet join](../fleet-join.md).

If the worker cannot bind, check `tailscale ip -4`, local interface assignment, and
`st fleet status`; Tailscale userspace networking has no host interface to bind.
For refused connections, check the tailnet ACL and the worker's advertised port.
For a stalled join, inspect `st replication status` on both machines and verify the
worker is running. MagicDNS is not required: replication dials literal IP addresses.
