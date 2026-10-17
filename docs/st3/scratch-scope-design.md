# Disposable scratch scopes for agent preflight

Status: proposed design for [#832](https://github.com/compoundingtech/smalltalk/issues/832)
and [#841](https://github.com/compoundingtech/smalltalk/issues/841). This note grants no
authority and implements no command. Nathan's 2026-10-14 disposition cancels Slice 0
and the scratch-pair proof on our machines. Johannes's agent runs that replication proof
on Johannes's machines; requests for needed st changes go to #832. No Docker or container
backend is permitted on any of our machines. Approval in principle covers this design note
only, not an interim mechanism, tool, root helper or implementation.

An agent should be able to validate a generated graph against disposable daemons, invite a
second disposable member and seed 50 invented documents without becoming a person. The
scratch authority must end at those daemons. It must never authorize a production invitation,
membership change, document write, credential use or service operation.

## Source baseline and existing boundary

This read uses main `2bc2ed54a900d884c99781b2a0ebc531e531d2e5` on 2026-10-14. It is source
evidence, not an installed-build or runtime test receipt.

* [main.rs](../../crates/st3/src/main.rs) and [fleet_cli.rs](../../crates/st3/src/fleet_cli.rs): `fleet_person`
  requires a person path for invite; `run_fleet` can found a fleet without that helper. Create and join already lock the state
  admission boundary. A separate state directory alone supplies no agent bootstrap authority.
* [api.rs](../../crates/st3-api/src/api.rs): `harness_ancestor` and `guard_bound_request` bind Linux
  harness requests to their agent. Invite creation and revocation require `concrete_person`.
  The non-Linux ancestry helper returns `None`; it cannot authenticate a Mac scratch owner.
* [setup.rs](../../crates/st3/src/setup.rs): setup refuses an agent process. Do not remove
  `ST_AGENT` or send a person actor to evade this check.
* [fixture.rs](../../crates/st3/src/fixture.rs), [test_support.rs](../../crates/st3-store/src/test_support.rs)
  and [Cargo.toml](../../crates/st3/Cargo.toml) provide a separate, non-installed
  `st3-fixture` executable requiring `test-support`. It initializes test state that disables
  ancestry binding in its server and clears seat environment in fixture subprocess helpers.
  That mechanism is for repository tests. Its existence does not certify an isolated fleet,
  restrict a copied production store or provide a supported agent bootstrap command.
* [handshake.rs](../../crates/smallclaims/src/fleet/handshake.rs) signs the current join
  transcript; [code.rs](../../crates/smallclaims/src/fleet/code.rs) defines `stj1-` codes.
  Neither inspected format contains the scratch domain and lifetime described here.

Keep normal actor binding, free-mode behavior within normal fleets and person workflow
gates unchanged. A scratch operation records the original agent and an explicit scratch
authority reference; it never records that agent as `person/example`.

## Proposed authority and host model

Use an explicit disposable scope, created by a reviewed scratch launcher. The launcher is a
separate trusted host service with a fixed executable and configuration. A caller requests a
scope by authenticated agent identity. It cannot choose a production state path, socket,
configuration, credential home, executable or network identity.

The launcher needs an operator-installed enrollment policy for the caller and a supported
identity verifier. On Linux it must bind the connecting process to a reviewed principal,
process birth and execution placement, then recheck that binding before mutation. An
environment variable or a caller-selected principal is insufficient. On macOS a reviewed
authenticated client credential bound to an enrolled agent is required; absence of Linux
ancestry is not permission. These are implementation prerequisites, not APIs already supplied
by this proposal. If the host has no such verifier, creation returns unavailable and names
the missing prerequisite.

Run each member in an OS isolation boundary with a separate security identity and private
state, configuration, runtime sockets and credentials. Prefer a disposable VM where the host
cannot enforce the necessary process, filesystem and network separation. A directory owned
by the same Unix account as production is not a security boundary against that account.
The first implementation must name and qualify one supported host backend; it must refuse
unsupported hosts rather than silently using an unconfined process. Do not infer that a
production Fabric or Tailscale installation provides this backend.

The trusted launcher owns a registry outside the member's writable filesystem. Each entry
binds a random scope ID, lifetime ID, creator agent, enrolled participant host identities,
member public keys, policy digest, creation and expiry times, status and immutable binary/source pin.
A scope has at most two members in the first slice. Member names are labels, not identities.
The registry is authoritative for admission and removal; an editable `scratch=true` file,
environment flag, copied database or path prefix cannot recreate an entry.

For a two-host test, both independently enrolled launchers attest their new isolated member
and agree on the same scope manifest before any member is admitted. An untrusted remote
daemon cannot self-certify that it is disposable. A participant whose host cannot provide
this attestation is unavailable; there is no fallback to its production daemon.

The scope controller signs a domain-separated manifest binding all these fields and the
participant attestations. Its key belongs to the trusted launcher, outside member and caller
access. Production principal enrollment never treats a scratch controller or member key as
a person, production agent or production member. Cryptographic encoding and trust-root
distribution require implementation security review; this note does not select a shortcut
based on the current person-signature rollout.

## Membership and replication admission

Scratch and production are distinct protocol domains. Introduce a separately versioned
scratch join and exchange protocol whose authenticated transcript includes domain, scope,
lifetime, expiry, policy digest and both member identities. Do not append an optional field
to `st3-join-v1` or send a scratch invitation as a normal `stj1-` code. Old programs must
refuse the new protocol, and unsupported domain versions must fail closed.

The scratch sponsor checks the current registry, creator authority and participant
attestation before issuing a single-use invitation for the exact second member key. The
joiner checks the same scope and lifetime, sponsor key and expiry before accepting any sealed
credential or writing membership. Both checks are needed: a special sponsor alone cannot
prevent a production joiner from importing scratch state. Migration and admission of an
existing configured member are unavailable in the first slice.

Every replication ingress, including direct HTTP, Fabric, Tailscale, import and checkpoint
restore, checks the authenticated domain and current scope lifetime before persistence or
projection. Scratch peers reject production and foreign-scope payloads; production peers
reject scratch payloads. Exported scratch data and checkpoints retain authenticated scope
provenance. Normal import cannot strip that provenance and promote the data. An implementation
must inventory all source import paths and prove these checks, including any legacy format.
If it cannot account for one path, that path is disabled for scratch.

Transport is not authority. Use scope-specific Fabric keys, home and ALPN, with exact peer
grants limited to the two attested scratch identities; never mount the production Fabric
home or grant shell/exec. A Tailscale comparison needs separately provisioned disposable
identities and ACLs permitting only these members and the test endpoints. Tailnet membership
alone does not admit an st member. Provisioning disposable transport identities is an
operator prerequisite and must not borrow a production credential. Restrict DNS, relays and
egress explicitly for the selected backend; changing the transport must not widen authority.

## Proposed operations and bounds

The following names illustrate the contract. They are not available CLI instructions.

| Operation | Required evidence and result |
| --- | --- |
| `scratch create` | Bound enrolled agent; fixed reviewed backend/binary; fresh empty state; exact limits and expiry. Returns public scope descriptor and private client access through a protected channel. |
| `scratch member add` | Second host attestation and exact member key within the same scope manifest; no user-supplied production root. |
| `scratch invite` / `scratch join` | Current creator authority plus both member attestations; signed single-use domain-specific invitation and checked redemption. |
| `scratch apply` / `scratch document put` | Actor remains the creator agent; target is the certified scope instance; bounded fixture input; current lifetime rechecked at commit. |
| `scratch inspect` | Scope, member/image pins, expiry, cut and bounded test results; no credential contents. |
| `scratch remove` | Atomically retire lifetime before stopping workers and removing private state. Reports each participant's actual cleanup receipt. |

Proposed first-slice limits are a one-hour default lifetime with a four-hour maximum, two
members, 128 fixture documents, 64 KiB per document and 8 MiB aggregate source bytes. The
50-document use case fits. Limits apply before parsing/copying payloads and also to stored
history, envelopes, output, subprocesses, descriptors and queued work. The implementation
must add explicit CPU, memory, disk and transport budgets for its selected backend; reaching
a budget refuses or stops the scope with an attributed terminal result, never spills work
into a production directory. Limits are reviewable design choices, not changes to production
source admission or retention.

Permit graph parsing and bounded desired-state simulation, but block host service installation,
arbitrary host commands, production filesystem adapters, real account/key provisioning,
credential export, external side effects and production client pairing. Any later need to
exercise a physical adapter requires a separately reviewed disposable adapter boundary.
Scratch authority cannot satisfy a production person review gate, rewrite a production
mission or sign evidence accepted for production landing. Do not export scratch results as
fresh operational instructions to a live seat.

## Filesystem, lifetime and recovery

Creation allocates a new root beneath a trusted anchor using descriptor-relative, no-follow
operations. It refuses an existing state tree, non-directory, symlink, unexpected owner,
device/inode substitution or production markers. Bind exactly the verified retained
descriptors into the sandbox; do not reopen by path after checking. No user-configured HOME,
XDG, gateway socket, secrets store, SSH agent, Docker socket, production workspace or host
filesystem tree is inherited. Test input enters through bounded explicit copies or reviewed
read-only descriptors, with names and types checked before use.

An existing scratch scope may reopen only when the external registry still has its exact
lifetime and authorized immutable root identity. Restoration or copying creates neither a
new lifetime nor a second live instance. Reopening a copied store, moving it outside its
authorized root, presenting a manifest from another host or replacing a mounted directory
must refuse. There is no conversion from a scratch store to a production store.

The launcher enforces local elapsed lifetime with a monotonic clock and records the UTC
expiry for audit. A restart must not extend lifetime: validate a durable deadline through
the trusted registry and refuse uncertain clock or rollback state. Remote participants
enforce their own admitted deadline and stop serving when the controller lease cannot be
validated. Clock uncertainty reduces availability; it does not grant extra time. Requests
queued before expiry or revocation recheck at their actual mutation boundary.

Removal first durably retires the lifetime and cancels outstanding invitations, then closes
admission, stops and reaps owned workers, revokes transport grants, closes sockets and removes
only recorded scope roots. Audit receipts survive separately without fixture bodies or
secrets. A disconnected host is reported as cleanup pending, not removed successfully; its
local expiry still prevents renewed service. Crash recovery finishes cleanup from the
retirement record. Recreating the same label allocates fresh scope/lifetime/member keys;
an old invitation, open connection or copied capability cannot address it.

## Threats and required refusals

| Attempt | Enforcement required before side effects |
| --- | --- |
| Change `ST_AGENT`, actor or creator field | Authenticate caller independently and require actor equals the bound enrolled agent. No Person fallback. |
| Point a scratch command at production socket/state | Trusted registry and retained instance identity must match; production routes reject scratch authority independently. |
| Copy/edit manifest or replace root between check and use | Signature plus external live registry and retained descriptor/instance binding; no authority from filesystem labels. |
| Replay invite after redemption, expiry or removal | Exact member-bound token, durable single-use transition and current lifetime check. |
| Join production using scratch code, or scratch using production code | Separate domain and authenticated join transcript, checked by both sides before credential/state writes. |
| Send exchange/checkpoint/import from a foreign scope | Domain, scope and lifetime validation on every ingress before admission or projection. |
| Read host keys or execute against a real workspace | OS isolation, fixed image and mount/egress allowlist; no inherited credentials or host control sockets. |
| Obtain more authority through fixture graph or a person gate | Scratch-local bounded simulation only; scratch principal never becomes a production or person principal. |
| Exhaust disk or keep orphaned processes after owner exit | Pre-copy budgets, owned execution supervisor, terminal cleanup receipts and durable retirement. |
| Use the same Unix UID to bypass the service entirely | Outside the application's adversarial isolation claim; require a separate host security identity/backend rather than asserting directory permissions solve it. |

These obligations require both API-level checks and OS/protocol tests. A cooperative wrapper
is not evidence that hostile code cannot access production credentials.

## Relation to delegated grants

[#882](https://github.com/compoundingtech/smalltalk/issues/882) describes scoped, expiring,
person-issued grants with the agent remaining the actor. Its original proposal explicitly
does not solve fresh scratch bootstrap; later comments place grants in the coherent principals
system and retain free mode meanwhile. Do not revive its earlier permission checks or build
a competing grants mechanism in this design.

A future supported grant may authorize the exact launcher creation request and its limits.
That grant does not replace fresh state, member attestation, domain separation or cleanup.
A production invitation grant would still create production membership and would not meet
this scratch-only requirement. Use the same principal/audit vocabulary when it is qualified;
until then, the trusted launcher's operator-reviewed enrollment is an explicit prerequisite,
not a fictitious existing grant API. Factory and Intake must review this bootstrap choice
before implementation.

## Implementation acceptance plan

No test below has been run for this note. A later mission must pin source, image, launcher
policy, host backend and actual participants, execute the controls and retain failures as
well as passes. Use synthetic production fixtures for negative tests, never probe a live
fleet as a denial test.

1. From a genuinely bound agent, create two empty disposable members, issue and redeem the
   exact second-member invitation, and write 50 invented documents as that agent. Retain
   source bytes, operation/claim identities, both certified cuts, actual member/domain pins
   and full fixture content/digest comparison. Demonstrate idempotent retries without changing
   actor attribution or duplicating claims.
2. Repeat the same bounded dataset over independently qualified Fabric and Tailscale
   configurations. Record endpoints, scope-only grants, latency/work/byte budgets and actual
   test completion. A transfer hash alone is not proof of projected-state equality or iOS
   session acceptance.
3. Exercise both directions of production/scratch and cross-scope invite, exchange, import,
   checkpoint and restore refusal. Assert zero membership/credential/claim/digest writes and
   no external connection before the refusal where applicable. Test direct APIs as well as CLI.
4. Exercise wrong actor, forged host attestation, another member key, replayed invite,
   missing/expired/revoked authority and a queued request after retirement. Keep existing
   person and agent actor-binding regression controls passing.
5. Exercise symlink/hardlink escapes, received descriptor mismatch, mount/path replacement
   after verification, copied roots/manifests, existing non-scratch state, host HOME/XDG
   overrides, real secret paths and unsupported OS/backend. Require refusal or verified
   original inode with no host mutation; never accept a substituted directory.
6. Kill creator, controller and individual workers at creation, redemption, apply and removal
   boundaries. Test reboot/clock rollback, disconnected second host, exhausted budgets and
   retained open connections. Require no resurrected authority, no orphan process/write,
   truthful pending cleanup and bounded eventual cleanup after connectivity returns.

The source/compatibility review must cover protocol negotiation and all import paths,
separate installed production and scratch artifacts, supported backend deployment and
rollback. Keep old production protocols unchanged; an old peer must refuse scratch rather
than receive a downgraded invitation. No storage version, rule change, schema migration or
transport enrollment is reserved by this design note.

## Review and next assignment

Factory records needed st changes for Johannes's replication proof on his machines. Intake then checks the threat matrix, bootstrap identity,
cross-host attestation, protocol/import coverage, deadline recovery and OS boundary. The
remaining implementation decisions are the qualified first backend and caller verifier,
trusted launcher enrollment, manifest encoding, transport provisioning and resource limits.
Unavailable prerequisites must be stated in the implementation plan with a named owner.

Only after those reviews and the applicable Nathan decision may a separate implementation mission specify source owners,
native controls, exact-head CI, operator installation and the separate end-to-end replication
proof. Landing this note does not close #832's implementation demand or establish the iOS
replication proof.

## Design and security review amendments

Factory reviewed `d4d37c2b` and accepted the direction with six changes. Intake independently
accepted docs-only landing with seven security requirements before implementation. The
following requirements refine the proposal above; they grant no implementation or execution.

### Adversary, caller and registry

The first slice protects against an honest agent making a wrong request, including requests
induced by untrusted fixture content, and against code inside the disposable guest. It does
not claim to confine a hostile host process sharing the production user's UID. That process
already has the user's filesystem authority; protecting against it requires a separately
enforced host security identity. Environment ancestry and application actor checks are
cooperative host attribution, not that security boundary.

The proposed Linux caller verifier is a root-installed per-seat request proxy. The launcher
checks `SO_PEERCRED` against its exact enrolled UID, PID, process birth, root-controlled
systemd unit/cgroup and pinned executable on every mutation. The registry maps that proxy to
one agent; a request cannot select another agent's identity. Direct CLI processes, proxy
children and reparented descendants have different PIDs/births and refuse, even when their
environment is cleared or their cgroup is unchanged. Retain a pidfd where supported and
recheck process birth and enrollment at every mutation, not only at connect; `SO_PEERCRED`
alone does not prevent PID reuse after the peer exits. A proxy restart needs trusted
re-enrollment; PID reuse must refuse.

The proxy itself needs a protected, authenticated per-seat channel from the enrolled harness
driver. This channel is a new reviewed prerequisite, not a public same-UID socket, ancestry
walk or caller-supplied session name. An unqualified driver or missing enrollment returns
unavailable. Mac hosts need an independently reviewed authenticated adapter. The driver,
proxy and registry source must be qualified together; this note does not assert their APIs
already exist.

Proposed Linux registry storage is `/var/lib/st-scratch-controller/registry`, mode 0700,
owned by the separate launcher service account. Only the narrow launcher writer may mutate
it. Operators and the launcher can read full creator bindings; each agent sees only its own
public descriptor. Retirement uses a separately protected monotonic witness outside guest
snapshots and registry backups. Registry opening compares its retirement sequence/hash with
that witness. Restoring an older registry, losing the witness or uncertain controller rollback
refuses creation/reopen until operator recovery reconciles retirement. An append-only file
restored with the same backup would not suffice; Operations must qualify the witness backend.
Cross-host launcher trust roots are operator-installed out of band, never supplied in an
invitation. If one launcher is unavailable, new cross-host admission is unavailable.

### Isolation is the primary production boundary

Today's production binaries do not implement a scratch-domain check. Protocol separation
is future defence in depth and cannot be assumed on old or mixed-build peers. Enforced OS
isolation, no production state/credentials/transport homes, and deny-by-default egress are
the primary controls. The first acceptance tests must prove the allowlisted scratch peer is
reachable while every production address is unreachable, and inspect the guest image,
mounts and environment for inherited production credentials before any protocol test.

Tailscale provisioning requires ephemeral, tag-scoped, pre-authorized single-use keys issued
by an operator, preferably in a separate test tailnet. The issuing/admin credential never
reaches the agent or guest. ACL tests must deny all production tags. Fabric uses disposable
scope-only identities with exact peer grants and verified expiry, never a production home.
These are reviewed future configurations, not currently installed grants.

### Withdrawn Slice 0 and implementation boundary

The interim Slice 0, its operator-prepared pair, image packaging, root trigger and hosted
replacement fixture are withdrawn. The two-instance replacement was also canceled; no
pair proof will run on our machines from this note. The earlier packaging estimates and
first-run dates are void. Completed source and security reviews remain historical proposals,
not execution authority or evidence of replication, isolation or cleanup.

Johannes's agent performs the replication proof on Johannes's machines. Factory watches
[#832](https://github.com/compoundingtech/smalltalk/issues/832) for requests for needed st
changes. Our machines run our work; a machine request from Johannes's side stays on his
machines unless Nathan explicitly directs otherwise.

The unified Fabric-trust design describes the proposed agent path for creating an invite,
joining a second member and writing test documents. It must distinguish those proposed
capabilities from the current person guards. This note changes neither those guards nor
actor authority. Any implementation, replicated vocabulary, storage/rules change or public
contract requires its own reviewed assignment and applicable Nathan decision.

A future design may discuss two separate `st --instance NAME` instances, and existing
tiles/namespaces where isolation is needed. Separate instance state, sockets and service
names do not supply a security boundary against a same-UID process. No Docker/container
backend, root-owned helper, launcher installation, disposable transport credential or
machine allocation is promised or authorized here. Missing isolation and caller-verification
primitives remain explicit prerequisites rather than an interim tool to build now.

### Proposed receipt requirements, no assigned fixture

If a separately approved future operation is assigned, its reviewed check set and immutable
binary/source pins must be fixed; input is bounded data, never an agent-supplied command.
Its result must bind the actual participants, certified cuts, field-level comparison and each
participant's cleanup status. Failed checks and incomplete cleanup stay explicit. Private
keys, invitation tokens and access secrets never appear in receipts or logs.

A replication comparison must use the same certified cut, including source identity and
projected state, rather than counts or transfer hashes alone. An unavailable certificate
means incomplete evidence, not an invented accessor. These are proposed acceptance
requirements, not an available command, fixture assignment or scheduled run.

Delegated-grant and authority migration remain future design questions. No implementation
follows from landing this note; Nathan must see the reviewed design before any interim
mechanism, tool or root helper is built from it.

### Additional acceptance controls

Before implementation acceptance, prove caller refusal for cleared-environment/reparented
descendants, unenrolled callers and another enrolled agent's identity; restored older registry
cannot revive a retired lifetime; guest pause/snapshot cannot extend expiry; one-shot/debug
commands reject caller-supplied scripts; and transport keys are single-use and expire while
ACLs refuse production tags. These supplement the existing actor, path, import and cleanup
controls. None has run for this docs-only note.
