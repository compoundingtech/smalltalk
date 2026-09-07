# DELTA-003: the immutable subject ID is not implemented

Status: open

Narrowed by [0015 Amendment 1](../.decisions/0015-immutable-agent-id-and-mutable-address.md)
on 2026-09-05: the mutable address shipped, the immutable ID is staged behind the
triggers that amendment names.

## Divergence

[Decision 0015](../.decisions/0015-immutable-agent-id-and-mutable-address.md)
and root requirements R19 and R24-R26 define the accepted target identity
model. `AgentSpec` now admits an optional `id` and an optional `address`;
`st2 agent address` authors the route; ordinary references, inbox and status
selection, recipients, and stream ingress all resolve through the fail-closed
bare-or-qualified address algorithm; roster and graph publish `id`, `address`,
and `busAddress`.

What remains divergent is the ID half. No writer emits `id`, so the effective ID
of every subject is still its positional `<host>.<identity>` bus identity, and
that value — not an explicit ID — is what ownership keys, task prefixes, durable
record endpoints, supervisor edges, PTY tags, and `ST_AGENT` carry. A subject
created after this delta closes would need UUIDv7 and ID-keyed resolution to be
reachable at all.

## VRS

The target has an explicit catalog-global immutable agent ID, an optional
host-local mutable agent address, a derived bus address, and separate non-unique
presentation. Migration freezes every live legacy subject's existing
host-qualified bus identity without moving declarations or declaration-parent
state. A structurally archived subject freezes the same bytes when they remain
unique across the combined live-and-archived set; an archived collision receives
UUIDv7 in its declaration and tombstone. Migration also records every
reassigned legacy bus identity with the subject that kept it and the archived
subject's generated ID, so a tolerant reader never retypes colliding
version-1 bytes into the wrong subject. Supervisor
resolution uses the combined pre-migration live-and-archived subject index; the
same migration rewrites every reference to the parent's migrated ID. A missing
or ambiguous reference refuses before writes with
`legacy-supervisor-unresolved`; the operator must unarchive and repair that
declaration through the pre-activation legacy authoring path, then retry.
Positional `identity` remains the legacy address fallback until an explicit
address is assigned. Address assignment is an immediate route cutover with no
alias or history. Exact ID selection is explicit and ordinary references use
the fail-closed bare-or-qualified address algorithm.

Agent endpoints persist an immutable ID plus a publication-time address
snapshot. Principal and external endpoints persist an explicit endpoint kind
and canonical typed address instead of pretending that address is an agent ID.
That is a new durable record version for `SentRecord`, which rejects unknown
fields (`src/message.rs`). It is not one for harness-state or harness-context,
whose readers ignore unknown fields by policy
(`crates/st2-wire/src/lib.rs`) — the premise that strict version-1 readers reject
additive fields was wrong for every record but the sender ledger, and each
version-2 reader belongs in the pull request that adds the writer emitting it.

## Implementation

The address half is implemented. What it leaves is the ID half, and it must
propagate one typed ID distinction through:

- live and archived catalog validation, explicit-ID migration, unarchive, and
  ID-keyed supervisor references — `supervisor_chain::resolve_spec` resolves a
  parent by `bus_id(host)` or bare `identity` only, so it must accept
  `effective_id` before a UUIDv7-born subject can hold an org-chart edge, and
  `st2 catalog migrate-ids` must exempt `agent-id-missing` from its own
  pre-admission gate or the prescribed rollout order deadlocks;
- every ambient `ST_AGENT` consumer, generated hook, channel adapter, and driver
  argument, which today carry the positional bus identity;
- runtime ownership, default task IDs, task inventory, socket admission, PTY
  schema-2 metadata, and launch metadata while keeping declaration-parent state
  and Resource paths stable;
- version-2 Sent records, typed non-Agent endpoints, DING sender projection,
  stream and resync ownership keys, harness-state, and harness-context records;
  and
- all supported downstream evals and generators.

The remaining reader-first obligation of the shipped half is single-field and
about routing: a build that does not read `address` routes the positional
identity and refuses an authored address, so an `address`-reading build must be
deployed on every admitted host **before** any address is authored. That is
satisfied by the release carrying the grammar; no record version, downstream
reader survey, or catalog transaction is implied by it.

Activating the ID half is still a reader-first transition, not a one-version flag
day:

1. Deploy readers that accept legacy and target Agent Specs, message versions 1
   and 2, and PTY schemas 1 and 2. Keep every writer on legacy output. The
   harness-state and harness-context readers are additively tolerant already, so
   their version-2 arms ship with their writers rather than ahead of them.
2. Prove reader readiness on every admitted host and supported downstream
   consumer. An unreadable or unknown reader is not ready.
3. In one catalog transaction, add migrated unique IDs to live and structurally
   archived declarations, update archived tombstones, and rewrite every
   supervisor reference to its already-resolved migrated ID.
4. Re-prove reader readiness immediately before enabling target writers.
5. Activate UUIDv7 creation, raw-ID `ST_AGENT`, ID-keyed runtime ownership,
   message version 2, harness-state and harness-context version 2, and PTY
   schema 2 together.

No timeout substitutes for readiness. Until step 5 completes, the positional bus
identity remains the normative durable key and every current invariant remains
normative implementation behavior. After activation, an unmigrated archived
declaration cannot re-enter the catalog; unarchive validates ID uniqueness, and a
transition from retired to routable validates full-catalog address uniqueness.

## Direction

update implementation

## Resolution Signal

Close this delta only after the canonical Agent Spec requires `id` and accepts
`address`, all live and archived legacy subjects have explicit migrated IDs and
ID-keyed supervisor references, st2 and all supported downstream generators emit
and consume the new model, and the model-free proof corpus passes against an
immutable st2 artifact. The proof must cover UUIDv7 creation, legacy bus-ID
migration, global uniqueness, address fallback and grammar, dotted
bare/qualified disambiguation including host-pinned qualified input, host-local
address uniqueness, explicit ID selection, immediate address cutover and reuse,
retired-address release and reactivation validation, transactional concurrency,
authority and Nix refusal, nondisruptive live continuity, readable
message/DING sender projection and cosmetic fallback, version-1 record
compatibility including collision-aware legacy attribution, harness-state and
harness-context version-2 records read by tolerant readers, typed non-Agent
endpoints, host/graph/launch lifecycle controls, and every public machine wire
shape named by decision 0015-immutable-agent-id-and-mutable-address.

Update these load-bearing invariant rows and their named tests in the same
implementation:

- `Runner-owned task identity`, including its PTY actor-tag clause, from
  host-qualified `ST_AGENT` and schema-1 `agent.actor.path` to raw immutable ID
  and schema-2 `agent.actor.id` plus `agent.actor.address`;
- `Stable roster JSON` for appended immutable ID, nullable current bus address,
  presentation, and migrated supervisor projection;
- `R23 fail-closed diagnostic inventory` for ID-keyed ownership and nullable
  address without weakening completeness;
- `Archival leaves the live catalog` for frozen archived IDs and safe unarchive;
  and
- `Unbindable session sockets fail at admission` for the new default task-ID
  shape and preserved legacy socket continuity.

Record the exact release, rollout evidence, and eval artifact here when closing.
