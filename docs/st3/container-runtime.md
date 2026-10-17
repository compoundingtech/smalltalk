# Container runtime contract

Status: source prototype. This contract does not enable container selection in the daemon or the
seat authoring kit. It does not install a bridge service. Do not deploy a sensitive seat with the
ordinary native runtime and assume that this contract isolates it.

## Boundary

`st_runtime::container::SeatRuntime` separates native and container execution. Native process
isolation still uses user scopes or detached processes. A container must never fall back to that
native path when its bridge is unavailable.

One container runs one private daemon, OMP, its native channel and PTY. The application user is
unprivileged. The host raw st socket and host PTY state are not mounted into the container. The host
remains trusted. A shared-kernel container is not a virtual machine.
Host root and live process memory are explicitly outside this contract's threat model.

## Lifecycle

The deployment adapter owns one fixed container unit and a durable generation counter. The
request cannot select a unit, executable, environment, file or upstream endpoint.

| Operation | Required result |
| --- | --- |
| Start | Reserve the next generation before launch. Return starting, not ready, until native readiness is proved. |
| Status | Return lifecycle metadata only. |
| Native ready | Verify the live private daemon, PTY and OMP native session/ready binding for this generation and incarnation. A socket or active unit alone is not proof. |
| Stop | Fence new work, stop the whole unit, prove the cgroup empty and erase volatile state before returning stopped. |
| Suspend | Fence work, require zero in-flight writes, and commit a private persistent handover document before teardown. Then stop and erase volatile state before returning suspended. |
| Resume | Allocate a fresh generation/native session. It must read the committed handover first, before new work or application readiness. Do not restore a transcript. |

Stop is available from every phase. Start requires stopped; resume requires suspended. Suspend
requires ready and zero in-flight writes. The adapter must check quiescence again after it fences
dispatch. A failed operation stays observable as failed or unknown, not as completed. Stop failure
must not produce stopped or suspended. Recover an ambiguous external write by its existing receipt
ID; never submit the write again with a new operation ID.

Ordinary native residency resume requires the old native session. Container cold restart does not
use that path. The seat commits working context, open tasks and next steps to one private persistent
handover document before suspend. Use a fixed private location, atomic replacement, file/directory
fsync and a generation-bound commit marker. A failed write must not claim successful suspend.
Keep the previous committed document if replacement fails. The fresh session must read the committed
document first on resume; missing, stale or failed reads must not claim application readiness.

Host-visible persistence remains limited to generation, phase, opaque operation/receipt IDs,
timestamps and counts. The handover is a separate, explicit content-persistence exception inside
the container's private storage. The bridge receives only a typed completion receipt, never document
bytes. After a crash, verify unit/generation identity before adoption; otherwise stop, erase volatile
state and recover from the committed handover plus current provider metadata. Emergency stop must
remain possible if handover generation fails, without claiming context-preserving suspend success.

## Seat bridge

Use one private Unix listener per seat and role. The deployment pins the seat, role and mapped
application UID. The server checks `SO_PEERCRED` before it reads a frame. The client checks the
server UID before it sends a frame. The socket owner must protect the directory and socket. Use a
stable user-namespace UID mapping. A missing socket, wrong peer or unsupported platform is an
error, never permission to use the raw host daemon.
The client compares the broker UID as observed in its own user namespace; the server compares the
application's host-mapped UID. Both identities need explicit mappings. An overflow UID is not a
valid bridge identity because unrelated host peers can share it inside a user namespace.

The controller listener permits start, status, stop, suspend and resume. Do not mount this listener
inside the container. The seat listener permits status and native-ready notification only. The
notification is not proof by itself: the adapter must verify the native binding. The server rejects
a foreign seat, unknown protocol version, wrong role, stale generation, invalid transition or
invalid observation before it returns success. The deployment must serialize calls to one adapter.

Each connection carries one length-prefixed JSON request and one response. The prefix is a
four-byte big-endian length; the limit is 4096 bytes. One absolute I/O deadline starts after connect
or accept; a slow peer cannot extend it with partial reads or writes. It does not interrupt adapter
execution. The deployment must bound its own unit operations.
Protocol version is 1. The request has `version`, `seat`, `generation` and a nested `operation` with
an explicit `kind`. Unknown fields and operations are rejected. Status has only `generation`,
`phase`, `ready` and `in_flight_writes`. Errors use closed codes. Error frames never contain command
output or parser diagnostics. Transport errors are not evidence that a mutation did not occur;
query status and reconcile before another operation.


The schema is an allowlist of typed fields. The seat identifier is at most 256 ASCII bytes and uses
restricted path-segment characters. It must equal the listener's pinned seat. Generations and
incarnations are integers. Blockers are closed error codes. No free text can pass through an ID
or blocker field.
The module supplies the runtime client, authenticated single-request server, authorization,
generation fences and response validation. `ContainerControl` is the deployment capability
boundary. Its implementation must own the fixed unit and the native readiness/erasure proofs.
It is not a general command execution interface. No deployed implementation is claimed here.

This bridge exposes lifecycle metadata only. It does not expose generic st API forwarding,
conversations, documents, terminal read/input, catalog publication, execution, credential access,
other seats or policy mutation. Private person-facing asks and digests need a separate reviewed
delivery contract. They must not be copied into shared manager stores.

## Privacy

Content-bearing runtime state remains in private, unswappable, per-generation volatile storage:
OMP transcripts, exec/PTY logs, screen/history state, private conversations, channel payloads,
harness cache and temporary files. Erase turn content at turn end and destroy volatile state at
stop/suspend. The approved exception is the private persistent handover document. It contains
context, not merely metadata; it must never leave the container's private storage, enter bridge
frames, logs, host exports, backups or shared manager stores. Retain only the current committed
handover, replacing it on the next successful suspend. This narrows the earlier blanket
metadata-only persistence rule. Do not create transcript/memory snapshots, terminal dumps, content
telemetry, core dumps or persistent journal output. Ordinary tmpfs can swap and is not sufficient.
Set `memory.swap.max=0` on the container cgroup. Disable zswap for that cgroup where supported.
Set `RLIMIT_CORE=0` inside the container and exclude the container from systemd-coredump.

The default native exec logger, provider JSONL storage and residency checkpoint path do not meet
this rule. A deployment must implement and prove the private storage and cleanup policy before
it can claim privacy. Manager-visible data is an allowlisted metadata projection only. Opaque IDs
must not encode content. Model-provider retention and authorization are separate gates.

## Host and kit prerequisite

A separate system configuration change must provide the nspawn machine, unprivileged user and
stable UID mapping, immutable closure, private writable storage, no swap/dumps/content logs, and
private networking with default-deny input and forwarding for both IP versions. Permit only the
reviewed model, account-action and control adapters. Do not mount shared homes, workspaces,
credential pools, SSH agents or privileged daemon sockets.

Fix the tool profile at build time: OMP, st, the scoped account-action wrapper and a minimal shell.
GitHub, Notion, credential-proxy and native account CLIs, and browsers must be absent from the
closure, not only from PATH. Do not mount host policy state.

The host must provide the fixed-unit controller, generation metadata store, private role listeners,
native readiness proof, dispatch fence and unit-empty/erasure acknowledgements. Narrow unit control
must not grant broad machine or service control to a shared user. This source change performs no
privileged host setup.

Provide a dedicated private persistent handover volume owned only by the guest application. Do not
mount it into the bridge or another seat. Exclude it from export, backup/snapshot and indexing.
The private daemon must provide generation-bound commit/read acknowledgements without document
bytes. Gate suspend completion on commit and resume readiness on the fresh session's first read.

The seat authoring kit must lower an explicit container runtime variant in a separate change. It
must resolve container-local workspace and tool paths, not the host generalist path. It must keep
publication held until the runtime and privacy proof passes. The daemon selector and native
channel integration must consume the same contract before live use. There is no environment-only
selector or shell-wrapper workaround.

## Tests and proof gate

The rootless module tests cover lifecycle metadata, cold-resume generations, stale readiness,
foreign seats, role restrictions, write quiescence, strict schemas, framing and Unix peer identity.
They use synthetic controls and Unix socket pairs. They do not prove nspawn or native OMP readiness.
The source change has not been built or tested by this worker.

A separate admitted test must use a synthetic container with no account credentials or private
data. Require real OMP and PTY native readiness after start, cgroup-empty and erasure evidence after
stop, quiescence and erasure after suspend, and a fresh generation/native session after resume.
Also prove socket, peer, seat, API, filesystem and network denial. Prove that a synthetic handover
with working context, open tasks and next steps survives suspend and is read first after resume.
Prove it stays in the private volume and is unavailable through the bridge, exports, shared stores
and peer containers. Test write/read failures and missing/stale handovers: they must not claim
context-preserving suspend/resume success. Other synthetic turn-content canaries must not survive
cleanup in persistent outputs. Keep the document bytes out of evidence; retain typed results and
generation receipts only. Container proof remains pending host setup and admission.

Profile probes own fixed closure exclusions and no-swap/no-dump controls. Runtime proof owns empty
tmpfs, private handover persistence/read ordering/confinement and bounded bridge schemas. No new
host-visible files are allowed outside the declared typed metadata store and private handover
replacement. Host root remains trusted; this is not protection from administrators.

## Owner decisions

The owner selected per-seat Unix sockets, a private daemon and PTY inside the container, and cold
restart with a private persistent handover. The seat writes working context, open tasks and next
steps before suspend; the fresh session reads that document first after resume. The handover never
leaves the container's private storage. These decisions replace the earlier recommendations.
