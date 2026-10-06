# Owned sets

An owned set gives one publisher responsibility for a complete list of top-level seats, mission
definitions and schedules. A successful publication retires previously live members that are
absent from that list. Membership lives in the ordinary graph, on `owned-set/NAME` subjects with
immutable `owned-set.revised` claims. There is no inventory database alongside the graph.

Ordinary `st apply FILE...` publication remains an upsert: omission has no effect. Owned sets are
an explicit choice, made with `st apply --set NAME`. A file disappearing from a checkout does
nothing by itself. The publisher must submit a complete, valid bundle. An unreadable input,
invalid declaration, stale fence or unconfirmed mass retirement rejects the entire transaction.

## Publish a complete bundle

Each file starts with `version 2`. The files together declare the complete live membership:

```sh
st apply --set garden seats.kdl missions.kdl schedules.kdl \
  --repository acme/garden --ref refs/heads/main \
  --sha 0123456789abcdef0123456789abcdef01234567 \
  --source-sequence 42 --expect-set absent \
  --as person/operator --dry-run
```

Remove `--dry-run` to publish. A dry run prints blockers and exits with failure when any
are present. Add `--check` to execute gates during the preview, with `--workspace DIR` and
`--input NAME=VALUE` as needed; gate answers appear on stderr. Publication checks for broken
exec gates by default; `--no-gate-check` skips them.

All source flags are required together with `--set`. Without `--set`, publish plain files
with `st apply seats.kdl missions.kdl schedules.kdl --as person/operator`.

Initial creation requires `--expect-set absent`; subsequent
publications require the exact selected revision printed by `st sets show garden`. The CLI
previews first, then submits captured member heads with the apply. The daemon checks the set
revision, source sequence and every captured member head within the publication transaction.

An existing unmanaged declaration requires `--adopt agent/garden/orchard` (repeat for each
subject). Adoption captures the old heads in the set revision. A member already belonging to
another set is refused. Retirement keeps ownership; v1 has no release or transfer operation.
Reintroducing a retired member through its set makes it live again.

Mission runs, mission-owned seats, resources, observers and subscriptions cannot be set members.
Publish those through their existing routes. A mission definition may contain its normal
run-owned declarations; changing its definition preserves active runs and their pinned revisions.

## Declaration diffs and readback

`st apply --set NAME ... --dry-run` includes an additive `declaration_diffs` map keyed by
subject for added, changed and retiring members. Each entry has `before`, `after` and `fields`.
`before` is null when the subject has never been published; adoption includes its existing
unmanaged definition. `after` includes the proposed retirement declaration when retiring.
Unchanged members have no entry. `fields` lists the changed JSON pointers in sorted order;
objects are compared by field and arrays as a whole. The empty pointer means the complete
definition was added. The existing classifications, effects, fences, digests and exit codes
are unchanged. The preview uses the existing person-or-agent actor validation.

The values are canonical structured publication definitions: seats and schedules use the
normalized `DesiredSubject` (including its canonical declaration AST and compiled seat member),
and missions use the compiled `MissionSpec`. The mission compiler supplies concurrent run,
revision cutover, completion, retry and duration defaults. These values come from the same
compiler and selected graph definitions as publication; publishers need not reproduce defaults
or render KDL themselves. Derived revision and step definition hashes remain visible.
`st missions publish FILE --dry-run` and the intent preview reuse this same `declaration_diffs`
format. These JSON values are inspection data, not an alternative JSON publication input.

Read an applied definition with the client operation `publication.definition`:

```text
GET /v1/client/publication-definition?subject=mission%2Fgarden%2Fharvest
GET /v1/client/publication-definition?subject=schedule%2Fgarden%2Fdaily
```

The response envelope contains the selected `subject`, `declaration`, `revision` and immutable
claim `token`, with a snapshot from the same read transaction. Owned members follow the selected
set references; active mission runs keep their pinned revisions. Rust `publication_definition`,
Swift `publicationDefinition` and TypeScript `publicationDefinition` also accept agent subjects.
The operation requires both `read.projections` and `read.declarations`, since canonical values
can include environment values and embedded mission declarations. It returns 404 for a missing
definition and rejects unsupported subject kinds or an oversized response without truncation.
The existing redacted agent `subject.definition` operation remains available.

## Suspended seats

Publication rechecks suspension within the writer transaction. A launch-changing update or
omission of a suspended seat is deferred for **that seat only**: its previous declaration and
ownership remain selected, and the rest of the set publishes. Byte-identical and label-only
updates publish normally without ending the hold. A suspend accepted after preview is included
in this check; a suspend using a launch token superseded by publication fails `stale-fence`.
Guarded receipts also capture `suspension_operations[subject]`, the latest suspension operation
known to the publisher. Member selection rechecks those fences on each daemon: a suspension
accepted on a peer before a concurrent publication arrives retains its original declaration,
even if the publisher had not replicated the suspension yet. Other members remain selected.
An incompatible declaration stays pending after resume until a publication acknowledges that
resume operation, so delayed replication cannot silently release the deferred launch.

Preview exposes `deferred[subject]` and classifies the member change as `deferred`. The additive
blocker has `code: "suspended-seat"`, `subject`, the original suspension `reason` (nullable),
`suspension` phase/operation details, and `proposed`, the normalized proposed declaration.
Applied receipts retain locally observed deferrals at `receipt.deferred`; set readback merges
replication-race deferrals into top-level `deferred`, includes human-readable `blockers`, and
reports member status `rollout: "deferred"` with that `blocker`. Commit status cannot
report `satisfied` or `running` while these entries remain. CLI apply prints the partial publication
then exits nonzero, so automation must inspect readback instead of treating it as an atomic failure.
Dry-run also exits nonzero for deferrals. Ordinary unowned apply refuses launch changes with
`suspended-seat` and the blocker detail.

Resume continues the **previous** launch through the existing verified native-session protocol.
It does not automatically publish a deferred declaration: the receipt clearly remains pending.
After successful resume, publish the desired bundle with a new source sequence and exact current
set revision. Replaying the same request/sequence remains idempotent, including its deferral.
Stop and retirement declarations are not suspension and are guarded while the hold is active;
automation cannot use set omission to end a hold. No authored suspended desired-state field
is introduced.

The receipt fields are additive. Existing unguarded receipt hashes remain unchanged; new
receipts carry `suspension_guard: true` and optional operation/deferral maps. Older builds reject
guarded receipt hashes instead of projecting their members. New publishers require every active
fleet daemon to advertise `features.owned_set_suspension_guard: 1`; upgrade all publishers and
readers before publication. An old publisher still lacks the guard. Guard-aware builds use new
shared projection layout and checkpoint rules identities; unlike builds exchange claim authority
without comparing incompatible projection proofs.

## Retirement

Omitting a seat declares a stop while keeping its conversations and history. Omitting a mission
prevents new runs, including starts pinned to an older revision, while retaining active runs.
Omitting a schedule stops future occurrences and retains work already created.

A seat declared `one-shot` also permits its runtime host to retire that exact member after the
process exits. The daemon records a stop while retaining set ownership and the source bundle.
A new member declaration published through the set starts it again. Other members still require
ordinary source publication to retire.

A publication retiring ten or more members, or at least half the previous live membership,
requires `--confirm-retire DIGEST` from its dry-run preview. The digest binds the exact bundle,
source, prior set revision, member heads, adoption flags and empty-set flag. Any change requires
another preview. Automation must not automatically copy a refused preview's digest and retry.

An intentional empty set additionally requires `--allow-empty`. No input files are accepted only
with that flag; specifying a missing file always fails. Empty membership that retires existing
members still needs the exact retirement confirmation.

## Source ordering and replication

The first publication fixes the repository and full branch ref. Each later publication must have
a greater unsigned source sequence. Equal sequence is an idempotent retry only when both the SHA
and normalized declaration bundle match. A newer source with unchanged declarations records a
receipt and preserves the member tokens, so it does not relaunch seats.

The publisher derives the sequence from the commit's full first-parent depth and verifies ancestry
against its last successful source. It must reject shallow history, non-descendants and rewritten
branches. st checks the numeric fence and graph references; it does not fetch Git or verify that a
publisher derived the sequence honestly. Roll back with a new revert commit and a larger sequence.

Every upgraded replica selects the highest source sequence, irrespective of arrival order or wall
clock time. Membership is complete: a member learned only from a losing branch is also retired
when absent from the winner. Its stop derives from the winning set claim; the next publication
records an explicit stop reference in the retired map. Equal-sequence divergent content is a visible conflict and holds member effects.
Missing previous revisions or member references also hold effects until replication supplies the
dependencies. Managed desired state resolves through the selected set's exact references, so an
older independent declaration cannot override it when partitions heal. Checkpoint replay preserves
set revisions and their member references.

A disconnected publisher can accept locally valid work and cause local effects before learning
of a higher sequence. This protocol provides convergence after healing; it does not provide
exclusive leadership during a partition. Keep the existing serial applier.

Before activating owned sets, upgrade every active fleet member. Publication refuses activation
unless each admitted member's latest own `daemon.started` advertises both `features.owned_sets=1`
and `features.owned_set_suspension_guard=1`.
Ordinary publication and existing clients remain usable during the upgrade. An older daemon must
not be introduced into a fleet after set activation.

## Inspect publication and rollout

```sh
st sets ls
st sets show garden
st sets status garden --sha 0123456789abcdef0123456789abcdef01234567
```

The selected resource reports source, immutable live and retired references, blockers, and each
member's desired token, launched token and running incarnation. A successful `start` action's
`desired_token` proves the declaration that launched that incarnation. Publication alone does not
prove a seat is running. Label-only updates share their existing launch lineage, so a running
seat stays current while its reported launched token retains the original label revision. A commit
query distinguishes a recorded receipt, local visibility,
supersession and current rollout. Remote replica visibility remains unknown in this response.
`st sets status <set> --sha <sha>` returns 404 until that SHA has a receipt; this is intended.
Read `st sets show <set>` to obtain the current revision for publication fencing.

A native seat can declare `rollout "manual"` in its body. The declaration is published and
included in the receipt digest, while a changed running seat stays on its incarnation until
`st agents rollout <seat>` is requested. Reads show `published, rollout pending (manual)`;
this intentional deferral allows the source's `satisfied` field to be true while `running`
remains false. The property survives each complete apply and needs no apply-time flag.

Plain declaration-changing start, stop, rename and publication routes refuse managed subjects.
Restart, suspend and resume require an unblocked set and cannot bypass an active, held or pending manual owned-seat rollout. Use `st agents rollout` to retry its cutover with fresh fences.
Use the set publisher to change a managed declaration or retire it.

Owned sets do not install a repository watcher. Git-backed automation remains a separate,
configured `github.ref` observation, pinned subscription and CI gate feeding the serial applier.
The applier chooses the input files and publishes the complete bundle through this command.

The publication API is `/v1/sets/preview` followed by `/v1/sets/apply`; both take the same typed
request with intent, source, set fence and actor. Apply also needs the preview's member heads and,
when required, its retirement digest. Client-v0 read routes are `/v1/client/sets` and
`/v1/client/sets/NAME`, with optional `?sha=SHA` on the detail route. Rust, Swift and TypeScript
clients expose the additive `owned-set` resource and set list/detail operations.

An optional [`when-idle` rollout policy](owned-seat-cutover.md) drains changed and retiring native seats
and verifies their original conversation on the replacement. Without it, automatic seats retain their
immediate runtime behavior. Manual seats defer cutover with or without the set policy.
