# Live-path priority

A person must always be able to ask st what is happening and attach to a terminal, even while the
host's builds, tests and CI use every core and saturate the disk. st keeps its live path ahead of
that work.

The live path is the st daemon and each terminal's PTY server. The daemon answers every command.
The PTY server owns a terminal's screen and serves each attach. A harness and everything it starts
are work: its compiles, its tests, its tools.

## Linux

On Linux with a systemd user manager, st starts each terminal in a transient scope that it records
on the session as the `st3.scope-unit` tag. The PTY server starts there and forks the harness.
Once the session is published, st moves the PTY server alone into a scope of its own. The harness
stays behind.

| What | Unit | CPU weight | IO weight |
| --- | --- | --- | --- |
| st daemon | `st3.service` | 1000 | 1000 |
| each PTY server | `st3-pty-server-RUNTIME-PID.scope` | 1000 | 1000 |
| each harness and its children | `st3-RUNTIME-DAEMONPID-N.scope` | 100 (default) | 100 (default) |
| each exec task, such as a gate | `st3-RUNTIME-DAEMONPID-N.scope` | 100 (default) | 100 (default) |

The move goes through the user manager (`StartTransientUnit` with the server's pid), so systemd
owns both scopes and collects each one when its last process exits. The server scope also sets
`MemoryLow=64M`, which takes effect only once the user manager itself has a memory.low (see
below). Stopping a session still stops the whole tree: pty signals the server's process tree and
its process groups, which do not depend on cgroups.

The daemon moves a PTY server that an older release started, the first time its reconciler
observes it. It moves only a process that is still in the exact scope st recorded for that
session, and never itself.

`st service install` renders `CPUWeight=1000` and `IOWeight=1000` into `st3.service`. The
replication worker keeps the default weight. An existing install keeps its unit until it is
reinstalled; `systemctl --user set-property st3.service CPUWeight=1000 IOWeight=1000` applies the
same weights in place.

IO weights apply only when the user manager delegates the io controller and the disk has an IO
controller such as iocost. `ionice` is no substitute: its classes act only under the `bfq` and
`mq-deadline` schedulers, and an NVMe disk usually runs `none`. Most distributions delegate only
`cpu memory pids` to `user@.service`.
To delegate io, as root:

```sh
mkdir -p /etc/systemd/system/user@.service.d
printf '[Service]\nDelegate=cpu io memory pids\n' \
  > /etc/systemd/system/user@.service.d/60-delegate-io.conf
systemctl daemon-reload
systemctl restart user@1000.service   # or log out of every session and back in
```

Memory protection needs root too. A cgroup's `memory.low` protects it from reclaim and swap only
up to what every ancestor is protected for, and `user.slice`, `user-UID.slice` and
`user@UID.service` have none by default. As root, each of these applies at once and persists:

```sh
systemctl set-property user.slice MemoryLow=16G
systemctl set-property user-1000.slice MemoryLow=16G
systemctl set-property user@1000.service MemoryLow=16G
```

Then, as the user, `systemctl --user set-property app.slice MemoryLow=8G` and
`systemctl --user set-property st3.service MemoryLow=3G` protect the daemon, and each PTY server
scope's `MemoryLow=64M` applies.

Without a systemd user manager, st runs sessions detached and PTY servers share CPU and IO with
their harnesses.

## macOS

macOS has no cgroups. The daemon runs as a launchd agent with `ProcessType` `Interactive`, and the
PTY servers it starts keep that agent's default QoS. st starts each harness, and each exec task,
under `/usr/sbin/taskpolicy -c utility`. That clamps the program and every child it starts to the
utility QoS class, which schedules below the default class and throttles its disk IO. The
`background` clamp would confine builds to efficiency cores, so st does not use it.

## Checking a host

`st doctor` reports a `priority` check. On Linux it warns when the daemon's own weights are below
1000, when the user manager does not delegate io (with the root command above), and when any PTY
server still shares its harness's scope. On macOS it warns when `taskpolicy` is missing.
`st doctor --strict` fails on those warnings.

To look by hand:

```sh
systemd-cgls --user-unit st3.service
for d in /sys/fs/cgroup/user.slice/user-$(id -u).slice/user@$(id -u).service/app.slice/st3-*/; do
  echo "$(basename "$d") $(cat "$d/cpu.weight") $(cat "$d/io.weight")"
done
```

## What this does not cover

- The st CLI runs in its caller's cgroup. An agent that runs `st` from its own seat shares that
  seat's weight with its own build.
- Remote attach also crosses `fabric.service`, which st does not install. Give it the live weights
  with `systemctl --user set-property fabric.service CPUWeight=1000 IOWeight=1000`.
- Weights share a resource under contention. They cannot make a disk that is queued for seconds
  answer in milliseconds. Keeping the st state directory on a disk the builds do not use removes
  that contention entirely.
