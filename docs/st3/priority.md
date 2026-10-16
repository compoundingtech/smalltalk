# Live-path priority

A person must always be able to ask st what is happening and attach to a terminal, even while the
host's builds, tests and CI use every core and saturate the disk. st keeps its live path ahead of
that work.

The live path is the st daemon and each terminal's PTY server. The daemon answers every command.
The PTY server owns a terminal's screen and serves each attach. A harness and everything it starts
are work: its compiles, its tests, its tools.

## Seat priority

Every seat's harness, and every exec task such as a gate, starts below the daemon by default, and
everything either starts inherits that. One setting changes it for the install, and one line
changes it for a seat.

| Setting | Linux | macOS |
| --- | --- | --- |
| `background` (default) | `nice -n 10` and IO class best-effort, level 7 | `taskpolicy -b` |
| `normal` | the daemon's own priority | the daemon's own priority |

Only agent seats and what they start go to background: their harnesses, helpers, shells an agent
opens, and gates. A terminal a person owns (`pty/person/NAME/ID`, such as a shell opened from the
TUI or the phone) and anything else a person drives interactively always run at normal priority,
whatever `seat_priority` and `ST_SEAT_PRIORITY` say. The daemon, the PTY servers and the `st` CLI
keep normal priority.

The prefix changes priority only. It does not change a seat's parent or lifetime: a seat's parent
is its PTY server, not the daemon, so restarting the daemon leaves every seat and its children
running, exactly as before. Nothing needs root, a new user or
a delegated cgroup controller, so it works on a fresh user install. On Linux, `nice` is added to
the daemon's own value, which is 0 under `st service install`. A harness that lowers its own
priority further stays lower. The scope weights below divide the machine between cgroups; `nice`
orders processes inside one, and a harness's work runs in a scope of its own.

**Install-wide.** In `~/.config/st3/config.toml` (Home Manager: `services.smalltalk.seatPriority`):

```toml
seat_priority = "normal"   # or "background", which is also what a missing key means
```

Run `st service restart`, then `st agents restart AGENT --as PERSON` for each running seat: a
seat keeps the priority it started with. `st doctor` names the value the daemon started with.

**One seat.** Declare `ST_SEAT_PRIORITY` in the seat's environment. It replaces the install's
value for that seat, in either direction:

```kdl
version 2
agent "garden/builder" {
  workspace "/work/garden"
  harness "claude" {}
  env { ST_SEAT_PRIORITY "normal" }
}
```

An unrecognized value reads as `background`, so a typo never gives a seat more CPU than the default.

IO level 7 acts only under the `bfq` and `mq-deadline` disk schedulers. On an NVMe disk with the
`none` scheduler, as on most hosts, it changes nothing; the CPU priority is what applies there.

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
below). The move does not change what a stop ends; see [Stopping a session](#stopping-a-session).

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
under `/usr/sbin/taskpolicy -b`. That runs the program and every child it starts with its resources
throttled, below the daemon's default QoS. `nice` alone is not enough on macOS: on a
10-core Mac, a test run at `nice 20` (13 processes, 226% CPU) took the load average to 54 and made
the daemon answer slowly, and `taskpolicy -b` on the same processes brought it to 23 within a minute
and 16 within five. The cost is that throttled builds can be confined to efficiency cores; set
`normal` for a seat that needs more. A seat at `normal` priority starts without the policy.

## Stopping a session

Stopping a seat, or an exec task such as a gate, ends every process it started.

On Linux with a systemd user manager, everything a harness or exec task starts stays in its work
scope, `st3-RUNTIME-DAEMONPID-N.scope`: a process whose parent exited, one that started a session
or process group of its own, such as each test `cargo nextest` runs, and one started while the stop
runs. A stop ends that scope.

1. For a PTY session, pty stops the PTY server, then signals the process tree and process groups
   it measured before the signal. st then sends SIGTERM to every process still in the work scope
   (`systemctl --user kill`), waits up to two seconds, and sends SIGKILL.
2. For an exec task, the whole work scope gets SIGTERM, not only the task's process group.
3. At the shutdown deadline, a kill sends SIGKILL to the terminal's or the task's process group and
   to the whole work scope. A PTY server still in the work scope leaves it first, so it outlives
   its harness and records the exit.
4. A harness can exit on its own and leave processes behind, and its session's record then removes
   itself. st keeps the name of each launch's work scope beside the session's spawn lock. It ends
   that scope, SIGTERM and then SIGKILL as above, when the reconciler next finds the stopped
   runtime not running, when the session is removed, and before a restart launches the next
   incarnation. An exec task's record names its scope, and the same three moments end it. Each
   launch has a scope of its own, so ending an old one never reaches the next incarnation.

A stop or a retirement of an agent root also stops every exec and PTY task declared under it
(`exec/ROOT/TASK`), each through its own stop above, whether or not the root has a process of its
own. Those tasks are not started or restarted while the root is stopped, whatever their restart
policy, and the root is not recorded as stopped while one of them still runs on this host; a task
whose stop fails keeps its own fault, and the root stays unsettled until it ends. A task of an
earlier release could outlive a stop of its root; it no longer does. A task is stopped by the
host it runs on. Starting the root again declares them again.

A unit that is no longer loaded has ended: systemd collects a work scope once its last process
exits. st never ends a PTY server's own scope, a service, or the unit it runs in.

Without a systemd user manager, on macOS and on Linux without a user bus such as many CI runners,
st has no scope to end. A stop ends the PTY server's process tree and the process groups pty
measured before the signal, and an exec task's process group. Three kinds of process keep
running: one that left that tree before the stop because its parent exited (`nohup`, `setsid`, a
tool's background task), a process group started after pty measured the tree, and whatever a
harness left behind when it exited on its own.

A session that a release before this one started records no scope beside its spawn lock. Its
leftovers end when it is stopped while it runs, or while its own record still names the scope.

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
- A process that a harness moves out of its work scope itself, such as one it starts with
  `systemd-run --user`, is no longer in that scope and outlives a stop of the seat.
- Remote attach also crosses `fabric.service`, which st does not install. Give it the live weights
  with `systemctl --user set-property fabric.service CPUWeight=1000 IOWeight=1000`.
- Weights share a resource under contention. They cannot make a disk that is queued for seconds
  answer in milliseconds. Keeping the st state directory on a disk the builds do not use removes
  that contention entirely.
