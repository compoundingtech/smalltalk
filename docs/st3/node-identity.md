# Stable node identity

An st node name identifies both a claim writer and the host that owns its seats.
It is independent of the computer's macOS name, hostname, or Tailscale device name.
Renaming a computer does not move its provider sessions or change its fleet authority.

The daemon pins the resolved name in `STATE/node-identity.json` and holds an exclusive
state-directory lock for its lifetime. Starting over that directory with another
`--node` keeps the pinned identity and prints the requested and retained names.
The replication worker and service installer use the same identity. Changing sockets
does not allow a second daemon to write the same state directory.

For an older state directory without a pin, startup reads the existing database without
migrating or writing it. A local `runtime.action.succeeded` start receipt binds a seat to
its immutable launch declaration. A matching observation must still report that source's
runtime running. A unique host proved by those local launch receipts takes precedence.
This recovers a directory already restarted under a different name while its seats still
belong to the old name. Replicated startup, host and runtime claims without a local launch
receipt never select this directory's identity. Without that proof the configured name
is retained and pinned. Databases without the receipt columns needed for this lookup
also keep the configured name and proceed to the store's normal migration checks;
identity inspection does not migrate an older schema. Restore the original launcher identity before the first updated
startup for manually adopted seats or history whose local receipts have expired.
Changing `--node` afterward cannot override an existing pin; do not edit or delete the pin
to bypass a refusal. Ambiguous local launch placements,
malformed pins and conflicting pinned fleet membership refuse startup before runtime actions.
Keep the state and keys intact; do not change member names or delete state to bypass a refusal.

If legacy receipts prove multiple local hosts, or disagree with the configured fleet member,
the automatic upgrade remains blocked. Back up the original state and launcher configuration,
restore the prior compatible binary and its known writer configuration, and have the operator
verify current membership, keys and local runtime ownership before attempting recovery.
Historical running observations can be stale after an earlier membership change; this lookup
cannot safely choose between them. Do not rewrite receipts or invent exit claims to make it pass.
For a corrupt pin, stop its daemon and worker and restore the exact pin from a known-good backup
for this state and its keys. Without that backup, use the same operator recovery branch; do not
guess a new pin or delete the file to select a writer.

Explicit `st fleet create --name`, `st fleet migrate` and `st fleet join` establish membership,
independently of computer renames. They stop installed services, hold the same exclusive state
lock, and record the successfully founded/admitted name before services start. A foreground
daemon must be stopped first when using `--no-service`; refusal happens before membership writes.
After service stop, admission waits up to ten seconds for the previous daemon's lock to close.
A failed join handshake does not update the pin. A saved join can resume after membership was
saved but its pin write was interrupted. Local checkpoint/config files belong to the local owner;
matching them is a consistency check, not authentication against an owner who can edit them.
Changing fleet.toml alone still refuses a conflicting identity at ordinary startup.

Restore claim backups into a fresh state directory, including no existing identity pin, and
configure the new restored writer. A restored database under another writer's existing pin
refuses startup with this instruction. Do not clone a live state directory and start the copy
on another machine: the copied pin and keys preserve the same writer, and this file lock only
fences processes sharing its local inode. Separate disks/machines require separate admitted
member identities; a file lock cannot fence the other copy.

For example, two live seats on `orchid` remain on `orchid` when the same directory restarts
with `--node orchid-laptop`. Existing driver incarnations and native session bindings remain
unchanged. Seats on the peer `fern` keep their placements. Restoring `--node orchid` reverses
the launcher configuration without rewriting graph history. Historical rows for a previously
used name can remain visible; this recovery does not fabricate their terminal state or erase
their signed history.

`st doctor` compares local live PTY/exec evidence with selected host placement. A local process
placed on a different host fails `runtime-ownership` and appears in `runtime-drift`, even when
replication is healthy. A remote seat with no corresponding local process is valid. Replication
status is an inspection command and cannot repair placement.

`st agents start EXACT_SEAT --host DESTINATION --as ACTOR` is a deliberate fenced placement
handoff, not a computer rename. The former host stops its process and acknowledges departure
before the destination starts. Do not use `--source-offline` while that process is still live.
Mission seats change through their mission declaration. See [seat suspension](suspend.md) for
planned native-session moves and [claim backups](backups.md) before configuration changes.

For macOS service installations, both LaunchAgent plists bake `--node`. Editing config.toml
and running `st service restart` alone does not regenerate those arguments. The app installer
changes the executable path while preserving other arguments. See [macOS installation](macos-installation.md).
The stable identity resolves this discrepancy on the updated daemon, worker and service installer;
older binaries still require the source-verified launcher restoration procedure.

This behavior has isolated Linux coverage for same-state recovery, unchanged runtime incarnations
and session bindings, remote isolation, reversal and duplicate-daemon refusal. It has not been
qualified on a live macOS installation. A provider that exited during the outage still requires
its ordinary restart/session recovery; preserving the state directory cannot preserve a dead process.
