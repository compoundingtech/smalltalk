# Run-only system services

A gateway can serve a root-enrolled Linux system process without an st daemon, a
seat or fleet membership. It authenticates the socket's kernel UID and the process's
exact systemd unit, and rechecks its process birth and placement before a request.
It reports a `program/HOST/NAME` caller, never the person who owns its grant.

`setup --person person/ada --uid RUNNER_UID` alone does **not** enable this.
A person mapping requires a logind login scope; a system runner is not a person.
Do not map a runner UID as a person or give it profile management.

The operator installs a separate `sekrets` user/store/gateway and a pinned st binary
on ci1/ci2. NixOS installation belongs in the reviewed host module, not an ad hoc
execution of the Debian-style setup script. The following is the gateway config
portion; actual UID, unit, interpreter and script paths must come from that host's
reviewed installation. Do not copy example IDs to another host.

```toml
path = ["/nix/store/PYTHON-PIN/bin", "/nix/store/GIT-PIN/bin", "/nix/store/OPENSSH-PIN/bin"]
checkout_roots = ["/var/cache/ci1/work"]

[people]
"1000" = "person/ada" # actual operator login UID

[programs."999"] # actual dedicated system UID, separate from every person
principal = "program/ci1/ci1-1"
person = "person/ada"
unit = "github-runner-ci1-1.service"
command = ["python3", "/nix/store/SIGNER-PIN/evidence-sign"]

[programs."999".checkout]
backing_root = "/var/cache/ci1/work/ci1-1"
mount_root = "/w"
```

The entry applies only to `/system.slice/github-runner-ci1-1.service` and its
children. A login session, user-manager service, similar unit name, different UID
or reused/exited original process does not inherit this grant. UIDs must be
non-root and distinct from person mappings. Program names are literal three-part
identities; no wildcard program enrollment/grant exists.

With a readable root tile registry, a UID enrolled as both a Program and a tile
is refused, even from the enrolled system unit. If that registry cannot be read,
a configured Program is judged only by its exact system-unit and process-birth
checks: a process in the tile slice is unidentified, not a Program or agent. A
tile-only UID is refused when its registry cannot be read. Missing registry data
does not enroll a tile or supply a grant. Existing tile classification is captured
per connection; unlike Program requests, it has no per-request PID-start recheck.
Transfer of a tile connection is an existing tile-boundary follow-up, not a
newly qualified Program behavior.

`command` is the **entire** accepted argv. Added flags, another script, a shell,
login or profile-management request is refused. An explicit profile grant is also
required; mapping to Ada does not give ownership of Nathan's profiles. Profile
and grant policies both apply, including expiry, revocation and locks. Program
callers cannot change profiles/keys/grants, make authorized HTTP requests or read
the person's audit log. The human reads `st sekrets log` from their login session;
allowed calls/refusals/exits retain the program identity in the gateway's local log.
Program-caller `sekret.called`, `sekret.exited` and `sekret.refused` events are not
mirrored into the graph: the daemon importer rejects their caller reference,
skips the entries and advances its cursor. The gateway's local log is the only
record of these events. It is stored on the host under the gateway's `sekrets`
user, separate from the Program's UID; that account and privileged host users can
modify it. It is evidence of use, not a tamper-resistant audit. Graph queries do
not provide a history of Program calls, exits or refusals.

From the owner's actual login session after the reviewed installation:

```sh
st sekrets profile create ada/evidence-sign \
  --allow 'python3 /nix/store/SIGNER-PIN/evidence-sign'
st sekrets grant ada/evidence-sign --to program/ci1/ci1-1 \
  --allow 'python3 /nix/store/SIGNER-PIN/evidence-sign'
```

Operations provisions `.config/st/evidence-sign.json`, the key (0600), and the
private bare scratch repository **inside this profile's private HOME**, readable
only by sekrets. No key/config goes into a job, checkout, runner HOME or job env.
Pin the signer source/binary and interpreter via immutable root-owned Nix paths
and the install manifest; an argv policy alone is not an executable hash pin.
The gateway's tool lookup must select that reviewed interpreter. The signer must
be the hardened wrapper-owner implementation, not a caller-authored script.

The existing wrapper interface stays unchanged:

```sh
EVIDENCE_SIGNER='["st","sekrets","--profile","ada/evidence-sign","--","python3","/nix/store/SIGNER-PIN/evidence-sign"]' \
  scripts/evidence-wrap ...
```

The wrapper sends its measured JSON on stdin. Sekrets passes stdin unchanged and
requires the authorized checkout; Program calls never fall back to profile HOME.
CI's `/w` is a private per-runner bind mount. The gateway takes the received
checkout descriptor, resolves its relative path under that unit's configured
backing root, and compares device/inode with a descriptor-relative, no-symlink
walk of that root. Wrong owner, outside root, symlink, missing path or mismatched
mount refuses the call. The gateway retains its verified backing-directory
descriptor, identifying the same directory as the received descriptor, and binds
it read-only at the logical `/w/...` path inside its protected execution namespace.
Bubblewrap resolves the source in the gateway namespace and checks the mounted
inode against that retained descriptor. All
configured backing roots and mount anchors are otherwise hidden. No runner unit
BindPaths, WorkingDirectory or restart is required by this source algorithm.

Operations must provide a root-controlled backing-root parent, a runner-owned
backing root, search-only ACLs for sekrets on the root/ancestors, and an existing
root-controlled empty `/w` mount anchor on the gateway host. The anchor contains
no job data and is hidden in the sandbox before the actual directory is bound.
The gateway never changes into its host's `/w` to locate a checkout. Nested
checkout directories must also be searchable; an inaccessible directory refuses.
Linked worktrees whose Git directories escape this runner root refuse too.
The new standalone NixOS gateway module and actual service/user/mount acceptance
are separate installation inputs; no no-restart deployment has been qualified.

The interpreter and absolute fixed argv inputs are checked as root-controlled
paths before execution. The signer derives
the requested commit's tree and lock digest from the bound checkout, validates
its structured input and constructs the statement itself; it must not accept
arbitrary bytes, caller-selected key/config, Git executable overrides or a clock
override in production. No GitHub observation API is needed for these local facts.

Every process inside the enrolled runner unit is the same Program caller,
including same-repository PR code. The gateway identifies the runner, not the
job, and `exit_code` and `output_sha256` are caller-supplied. A same-repository
job on the enrolled pool can obtain a fleet-level note for a tree it holds.

In the current rollout phase, a `wrapper` note signed by `fleet/ci` at trust
level `fleet` may satisfy a skip policy that explicitly accepts that kind for
the matching check. The verifier still matches the exact tree, command, inputs,
signer and accepted-base policy; future dates and newer failures prevent skips.
The ledger and UI must label skips by kind. `observed` remains stricter and may
be required per check; a wrapper note cannot satisfy an observed-only policy.
Keep CI and developer signer roles distinct rather than inventing a schema tier.

Fork PRs never sign or enroll. Enrollment on the current CI pools is permitted;
a merge-group-only runner unit is not required in this phase. First installation records the repository's
`all_external_contributors` fork-workflow approval setting and the working rule:
no maintainer, curator or administrative automation approves a fork Actions run
unless the workflow diff has been reviewed and the run is routed away from the
signing pools. This is an interim approval/routing rule, not technical proof that
all fork-owned jobs are excluded. The gateway's Program identity alone has no
GitHub event/repository provenance.

Known defects in this phase:

- Cooperative measurements: a signature attests to a runner's report, not proof
  that a hostile job executed the stated check. Same-repository PR code can
  request a passing or failing note for a tree it holds. The phase policy accepts
  this gap for matching wrapper-kind skips; it must remain visible and tracked.
- Fork admission enforcement: an unrestricted runner group and PR-owned workflow
  definitions can bypass a picker or label convention if a fork workflow is
  approved. The interim rule above is the first-install acceptance condition;
  an actual runner-group restriction to qualified trusted workflow definitions,
  preserving internal PR and merge-group coverage, is the required fix. Do not
  treat withheld GitHub secrets or approval settings alone as a kernel boundary.
- Bridge key exposure: until the existing wrapper consumer moves behind the
  protected profile, its environment-secret or local key-file bridge may be
  reachable by code under test. This source does not remove that bridge. The
  operator must prove the replacement's key is inaccessible to the job before
  switching and removing the old key through its owner.

The key-isolation boundary remains exact UID/unit/argv/grant, a root-pinned signer,
private profile HOME/key/config, no job management, and a read-only authorized
checkout. Signing a constructed statement does not prove that a caller-provided
commit is the actual PR head.
Verifier policy
must compare exact tree/check/input keys and signer role. Developer/cooperative
trust is a **different principal/level**, not an invented `cooperative` value in
the existing `full/degraded/test` schema. The service never decides whether CI is
skipped. Keep PR and landing policy separate.

Authored source tests and isolation VM controls cover unit/UID classification, exact argv,
management refusal, grant/revocation, audit and store/key unreadability before
any st daemon starts, including private-mount binding, read-only checkout and stdin.
They must pass before installation; source parsing and formatting alone do not
qualify these controls. They do not claim that ci1/ci2 are installed or a
real merge-group note is accepted. Record the actual image, config, grant, run,
attempt, tree, signed note and verifier job before switching consumers/removing
the obsolete job-visible key. Namespace's remote signing path and the developer
Fabric service are subsequent steps of the same evidence-signing-service mission.

Existing people/agent/host callers keep their behavior. Upgrade the gateway before
adding `[programs]`: older gateways reject the unknown config and caller kind.
Rollback removes program enrollment/grants first; never convert a program into a
person mapping. No st graph/schema/replay or gateway DB migration is introduced.

The inode-retention Rust control exercises the descriptor-relative walk and exact
directory match directly, then replaces the path and checks the retained original
inode. It does not assume the test runner owns a root-controlled temporary parent:
the Nix sandbox can own `/tmp` as its build UID. The isolation VM separately tests
the root-controlled configuration, private `/w` mount, read-only bind, swap barrier
and wrong-inode refusal before bubblewrap. A passing helper control is not VM proof.
