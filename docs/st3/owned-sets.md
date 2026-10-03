# Owned sets

An owned set gives one publisher responsibility for a complete list of top-level seats, mission
definitions and schedules. A successful publication retires previously live members that are
absent from that list. Membership lives in the ordinary graph, on `owned-set/NAME` subjects with
immutable `owned-set.revised` claims. There is no inventory database alongside the graph.

Ordinary purpose-specific publication remains an upsert: omission has no effect. Owned sets are
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

Remove `--dry-run` to publish. Initial creation requires `--expect-set absent`; subsequent
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

## Retirement

Omitting a seat declares a stop while keeping its conversations and history. Omitting a mission
prevents new runs, including starts pinned to an older revision, while retaining active runs.
Omitting a schedule stops future occurrences and retains work already created.

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
unless each admitted member's latest own `daemon.started` advertises `features.owned_sets=1`.
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

Plain declaration-changing start, stop, rename and publication routes refuse managed subjects.
Restart, suspend and resume require an unblocked set and cannot bypass an active or held owned-seat rollout. Use `st agents rollout` to retry its cutover with fresh fences.
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
and verifies their original conversation on the replacement. Without it, publication retains its
immediate runtime behavior.
