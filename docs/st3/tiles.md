# Tiles

A tile is one agent as its own Unix user, in its own namespaces and its own root-owned cgroup.
Another tile cannot read its files, see or signal its processes, reach its sockets or loopback
ports, or pass for it. A tile is a process group and private directories on the host's kernel,
not a VM and not a full container image. Linux first; the approved design is the st document
`doc/fleet/smalltalk/agent-tiles/design`.

This document describes what is built: the first step of that design, launching a tile with a
user ID the launcher allocates. Pinned sekrets profiles for gh and git, one harness in a tile,
signed handoffs and network egress come next and are listed at the end.

## Registry

Root's registry, `/etc/st-tiles/tiles.toml`, is the only source of a tile's identity. Root owns
the file and every directory above it; st refuses one anyone else can change. Root names each
tile's agent, person, supervisor and ceilings; the launcher gives it a user ID.

```toml
state_root = "/var/lib/st-tiles"      # STATE/NAME/home and STATE/NAME/work, the tile's own
runtime_root = "/run/st-tiles"        # RUNTIME/NAME/endpoint and the tile's root mount point
read_only = ["/nix/store/...-tools"]  # root-controlled closures every tile sees read-only
path = ["/nix/store/...-tools/bin"]   # PATH inside a tile
sekrets_socket = "/run/st-sekrets/gateway.sock"  # optional: tiles reach the sekrets gateway
uids = { first = 80000, last = 89999 }           # the default; no account or group may use one

[tiles.atlas]
agent = "agent/fleet/fixture-example/atlas"
person = "person/ada"
supervisor_uid = 1000                 # whose st daemon serves the tile's endpoint
tasks_max = 512                       # ceilings, not reservations
memory_max = "8G"
cpu_weight = 100
```

A tile's name is up to 32 of `a-z`, `0-9` and `_`. A tile is never root, never a person and
never another tile's supervisor, and a read-only path never overlaps a tile's private state.

## User IDs

A tile is not an account. The first time root prepares or runs a tile, the launcher allocates
its user ID from the registry's `uids` range and records it in `STATE/uids.toml`. That file is
owned by root, and the daemon and the sekrets gateway read it alongside the registry. The tile's
group ID is its user ID, and inside the tile `/etc/passwd` calls it `tile`. This follows Nomad's
dynamic workload users and systemd's `DynamicUser=`. One difference: a tile's state outlives any
one launch, so a tile keeps its user ID for as long as its state lasts, not for one run.

- The launcher allocates the lowest free ID in the range. Free means no other tile has it on
  record, no account or group holds it, and it owns no tile directory in the state root (an entry,
  or its `home` or `work`). A tile writes nowhere else on the host, so an ID that still owns a
  tile's files is never handed out, even when no record remains.
- The launcher refuses a range that holds an account or a group. It also refuses one that
  overlaps a subordinate range in `/etc/subuid` or `/etc/subgid`, systemd's own dynamic users
  (61184-65519), systemd's container range (524288 and up), or 65534 and 65535. Every later
  launch refuses an ID that an account or a group has taken in the meantime.
- `sudo st tiles remove atlas` stops the tile, removes its state and runtime directories, and
  gives the ID back. It refuses while any process anywhere on the host runs as that ID, and the
  ID stays allocated while the state remains. Root's registry entry stays, so a later launch
  allocates a fresh ID, and nothing of the old tile is left for it to inherit.
- sekrets grants name the tile's agent, never its user ID. A released ID carries no grant, and
  a tile that comes back with a new ID is still the same agent.

A process is a tile's when the kernel says it runs as the tile's user ID inside the tile's
slice, `/st_tiles.slice/st_tiles-NAME.slice/`, which only root can place it in
(`Registry::identify`). What a tile says about itself, such as `ST_AGENT` or `--as`, counts for
nothing.

## Launching

```sh
sudo st tiles run atlas -- python3 -c 'print("hello")'   # wait for it; --pty for a terminal
sudo st tiles stop atlas                                  # end every process in it
sudo st tiles prepare atlas                               # allocate its ID, make its directories
sudo st tiles remove atlas                                # end it, remove its state, free its ID
```

`st tiles run` starts `st-tile-NAME-tools.service` in `st_tiles-NAME.slice` with the tile's
ceilings (`TasksMax`, `MemoryMax`, optional `MemoryHigh`, `CPUWeight`) and
`KillMode=control-group`. Only one runs per tile. systemd owns the cgroup: nothing is delegated,
so a tile cannot write to it, and the unit ends when its main process ends, for any reason. The
st executable runs as root in that unit, so root must control it and every directory above it;
install st where only root can change it.

The unit's main process, `st tiles enter`, checks it runs in that unit, makes new mount, PID,
IPC, network, UTS and cgroup namespaces, and forks the tile's first process, PID 1 of the new
PID namespace. PID 1 builds the tile's file system on an empty tmpfs:

| Path | What it is |
|---|---|
| each `read_only` path | bound read-only, without its submounts, no set-user-ID |
| `/tile/home`, `/tile/work` | `STATE/NAME/home` and `work`, owned by the tile, mode 0700 |
| `/tmp`, `/dev/shm` | private tmpfs |
| `/proc` | a fresh proc of the tile's PID namespace |
| `/dev` | `null`, `zero`, `full`, `random`, `urandom`, `tty`, a private `pts` |
| `/run/st` | the tile's endpoint directory, read-only |
| `/run/st-sekrets/gateway.sock` | the sekrets gateway's socket, when the registry names it |
| `/etc` | `passwd`, `group` and `hostname` for the tile alone |

Nothing else of the host is there: no `/sys`, no homes, no `/run/user`, no host sockets, no
`/var`. The root itself is read-only. The network namespace has only its loopback, so a tile's
own servers listen there and nothing else is reachable; abstract Unix sockets belong to it too.

PID 1 then closes every descriptor but standard input, output and error, drops the capability
bounding and ambient sets and its supplementary groups, becomes the tile's user and group, sets
no-new-privileges and installs a seccomp filter. The filter refuses with EPERM `unshare`,
`setns`, clone with a namespace flag, `mount`, `umount2`, `pivot_root`, `chroot` and the new
mount API, and the kernel surfaces an unprivileged process most often turns into root and tools
do not need: `bpf`, `userfaultfd`, `perf_event_open`, the keyring calls (`add_key`,
`request_key`, `keyctl`), `kcmp` and `process_vm_readv`/`writev`. `clone3` and io_uring fail
with ENOSYS, as on a kernel without them, so libc and runtimes fall back (libc to clone, whose
flags the filter reads). A call through another architecture's table ends the process. The
filter is a list of refusals, so a kernel interface added after it is reachable until it is
added here; it narrows the kernel a tile reaches and is not the boundary itself. PID 1 forks the command with a
clean environment (`HOME`, `USER`, `PATH`, `TMPDIR`, `ST_TILE`, `ST_AGENT`, `ST_TILE_ENDPOINT`,
`TERM`), every signal at its default, then reaps until the command exits, and exits with its
status.

## Ending

Every way a tile's launch ends leaves nothing behind. When the command exits, PID 1 exits and the
kernel ends every process left in the PID namespace, including ones that ignore SIGTERM or
double-forked into their own sessions. When PID 1 is killed, the same. When `st tiles enter`
ends for any reason, systemd stops the unit and kills every process in its cgroup: the cgroup is
the guarantee. PID 1 also gets SIGKILL from its parent-death signal, which only ends it sooner.
`st tiles stop` stops the unit directly. In each case the unit and its cgroup are
gone when `st tiles run` returns, no process of the tile's user remains on the host, and the
tile's mounts, which only ever existed in its own mount namespace, are gone with it. The tile
starts again cleanly.

## Endpoint

`RUNTIME/NAME/endpoint` is owned by the tile's supervisor and bound read-only at `/run/st`. It is
the only host directory where a tile can reach a socket. A server there learns the caller from
the kernel: `SO_PEERCRED` gives the Unix user and process, and the process's cgroup and the
registry give the tile and its agent.

The supervisor's st daemon serves `st.sock` there, the tile's restricted st endpoint, at
`/run/st/st.sock` inside the tile. The daemon follows root's registry and serves every tile
whose `supervisor_uid` is its own Unix user, from the moment root makes the tile's endpoint
directory (`st tiles run` or `st tiles prepare`); a registry change starts or stops endpoints
without a restart. When the registry withdraws a tile, removes its `supervisor_uid` or changes
its entry, the endpoint stops and every open connection to it closes, so no connection keeps
acting for an identity root has withdrawn. A connection is the tile's only when the kernel names
it: anyone else's is closed before a byte is read. The daemon classifies a connection once, from
the process `SO_PEERCRED` names when it connects; a process number reused before its cgroup is
read must still run as the tile's Unix user in the tile's slice to count, which only the tile's
own processes do. Requests then act as the tile's agent alone:

| Route | What a tile may do |
|---|---|
| `GET /v1/tile/whoami` | Its tile, agent and person, as the kernel and registry name them. |
| `POST /v1/messages` | Send as its own agent. A request naming another sender is refused, not rewritten. Attachments are refused for now. |
| `GET /v1/messages` | Its own mailbox. Asking for another's is refused. |
| `GET /v1/messages/read/ID` | A message it sent or received. Any other is not found. |

A message a tile sends may reach any agent and may carry `in_reply_to`, tags and a hash-pinned
document reference in its content, as a seat's message can today; only its sender is fixed.
Every other route the daemon serves, including its administrative API, is refused with 403.
`ST_AGENT`, `--as` and `--from` therefore cannot widen what a tile does; harness integration
(step 2) widens this list route by route.

## Sekrets

The sekrets gateway names a tile's caller the same way: the tile's Unix user, a process in the
tile's slice, and root's registry give the agent and its person, with no daemon's attestation.
The same Unix user anywhere else is unidentified, and a Unix user that is both a person and a
tile is refused. See [Sekrets: Tiles](sekrets.md#tiles).

## What it proves

`crates/st3/tests/tiles_root.rs` runs on a real host with `sudo -n`, systemd and cgroups v2. Its
tiles get user IDs from a test range, and afterwards `st tiles remove` removes every tile, unit
and directory:

```sh
ST_TILES_ROOT_TEST=1 cargo test -p st3 --test integration tiles_root:: -- --nocapture
```

CI does not run these: its runners have no root, and the tests return at once there. The builder
of every change to `crates/st3-daemon/src/tiles` or `tests/tiles_root.rs` runs them on a Linux host with
root before review, and the pull request says where and at which commit. Before a release that
ships a harness in a tile (design step 2), a release gate must run them on a host like the ones
that will run tiles; that gate is part of step 2.

- Identity: the command runs as the tile's user with no groups, no capability in any set,
  no-new-privileges and the filter, sees only its own processes, its own root and `lo`, and
  only standard streams are open.
- Siblings: a second tile cannot reach the first's abstract socket, pathname socket, loopback
  port or System V shared memory, see its files or processes, or signal any host process; the
  host's own loopback port and abstract socket are unreachable too. Outside the namespaces, one
  tile's user cannot read another's state.
- Keys: a node key written as st writes it, and every path of the supervisor's state, are
  unreadable by the tile's user and absent inside the tile.
- Escape: every namespace flag, `setns`, mounts and `chroot` are refused, and so are io_uring,
  `bpf`, `userfaultfd`, `perf_event_open`, the keyring calls, `kcmp` and
  `process_vm_readv`/`writev`; `fork` works until
  `TasksMax`; the tile's user cannot move a process into its own or a sibling's cgroup.
- Descriptors: a descriptor root's shell left open across `st tiles run` is not in the tile.
- Cleanup: killing the launcher, killing PID 1, stopping the unit and the command exiting each
  leave no unit, cgroup, process or mount, and the tile starts again.
- User IDs: each tile gets its own ID from the range, held by no account or group, and recorded
  in a root-owned 0644 file. The ID lasts across launches while the tile's state does. A process
  running as that ID outside the tile keeps it allocated. `remove` removes the state and gives
  the ID back. An ID that still owns a directory under the state root is skipped. A range that
  holds an account is refused.
- Refusals: a non-root caller, `enter` outside its unit, an account that took a tile's user ID,
  a registry or st executable someone other than root can change.
- Endpoint: a real tile, through `/run/st/st.sock`, is told its own agent, sends as itself, and is
  refused sending as its sibling, reading its sibling's mailbox and the administrative API; the
  graph holds its message from its own agent and never the forged one. The sibling's endpoint
  answers for the sibling alone. Unit tests in `api/tile_endpoint.rs` cover each route, a
  connection from this user outside the tile's slice or from another tile's slice, the daemon
  following the registry and the endpoint directory as they appear, and a kept-open connection
  closing when the registry withdraws the tile.
- Sekrets: a real tile's `st sekrets whoami`, at the client's default socket, is its agent
  working for its person, with no daemon; the same Unix user outside the tile is unidentified;
  a tile whose registry names a missing gateway socket is refused at launch. Unit tests in
  `sekrets/tests.rs` cover a grant to the tile's agent, attestations that change nothing, every
  cgroup outside the tile's slice, and a Unix user that is both a person and a tile.

## Not built yet

- One unmodified harness in a tile with its own per-agent login (design step 2). A tile's tools
  can read that agent's own credentials. Protecting them from the agent itself is later work.
- Network egress: a tile has no route off its loopback until a root-owned veth, route and
  firewall policy is added.
- A disk bound. A tile's home and work live on the host's file system with no quota: a tile can
  fill that disk. Its /tmp and /dev/shm are tmpfs, charged to its memory ceiling. A project
  quota or a size-limited file system for `STATE/NAME` must come before a harness runs in a
  tile.
- An allowlist filter. The seccomp filter refuses a list of calls; an allowlist of the calls a
  harness and its tools need, measured from real runs, would close new kernel interfaces by
  default. Decide it with step 2, when the workload is known.
- git and gh through pinned sekrets profiles from a tile's own checkout. The agent's declaration
  pins its profiles, and gh and git in the tile are thin shims that call sekrets. The gateway
  sandbox binds a checkout by the path the kernel gives for a passed descriptor, but a tile's
  mount namespace does not share that path. So today the gateway never binds a tile's directory
  and runs a tile's commands in the profile's home (the design's pins step).
- Landlock and an AppArmor or SELinux profile on top of the namespaces.
- macOS.
