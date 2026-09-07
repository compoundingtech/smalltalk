# Stable agent identity is separate from mutable presentation

Status: superseded by 0015-immutable-agent-id-and-mutable-address

Johannes authorized the requirements direction on 2026-07-31. The draft
required Nathan's merge and acceptance approval, which was not recorded.
Decision 0015-immutable-agent-id-and-mutable-address independently accepted the replacement contract.

## Context

Agent Spec currently overloads one identity string as both an automation key
and the only human recognition surface. Operators cannot improve a label
without changing routing, durable paths, task identity, and process lifecycle.
An experimental sibling `name` file is a second source of truth and does not
compose with canonical Agent Spec authoring.

The fleet requires a stable automation identity and presentation that can
change while the running process, PTY generation, bus, and durable state remain
continuous. The change must not introduce a stable-ID alias, dual parser, or
long-lived migration branch.

## Decision

The existing positional Agent Spec identity remains the sole stable automation
ID. Its grammar and the established `identity` JSON/TOML/roster spelling remain
unchanged. There is no stable-ID rename operation.

Agent Spec adds direct optional `name` and `description` fields. They are
non-authoritative, non-unique presentation. Omission means absence. The Agent
Spec declaration is their sole source of truth; a sibling `name` file is neither
read nor written.

Constrained KDL-only commands may mutate one presentation field without
accepting a second authored representation. Within the trusted-fleet model,
caller-supplied `ST_AGENT` limits an invocation to itself or declared
descendants; this is an operational guardrail, not an authenticated capability,
and absence selects the operator path. Declarations explicitly marked Nix-owned
remain writable only at their Nix source, and Nix emitters must publish that
marker before authoring is activated. st2 serializes these
edits with the shared persistent `.st2/catalog-authoring.lock`, preserves
unrelated source bytes, detects stale source, and atomically replaces the
declaration. The lock covers cooperating local st2 writers in one POSIX
filesystem/kernel lock domain; it does not claim exclusion across independently
synchronized hosts or direct external writers.

Healthy runtime reconciliation uses the atomic exact-ID-only `pty metadata
patch --id <task-id>` operation. It projects name to native PTY `displayName`
and a versioned st2-owned tag snapshot containing stable actor identity plus
optional description. Name is not duplicated in tags. One real patch emits one
coherent `metadata_change` event; an unchanged patch emits none. Automation
never uses human display-name resolution. Presentation drift degrades and
retries without restart or lifecycle accounting.

Harness drivers consume the current lowered Agent Spec rather than a second derived
presentation file. In-process drivers may read the lowered `AgentSpec` directly;
external hooks or drivers may select the exact qualified stable identity through
`st2 agents --identity <host>.<identity> --json`. The roster query returns
exactly one row or fails and keeps nullable name and description separate from
stable identity. Provider-native translation and exact session fencing remain
driver responsibilities; this read interface grants no lifecycle authority.

## Consequences

- Human labels can improve without breaking routing or continuity.
- Duplicate or absent names are valid; stable IDs remain visible for exact
  disambiguation and automation.
- The old equality between stable identity and every presentation surface is
  superseded, but stable routing semantics are preserved.
- Existing declarations require no presentation compatibility marker because
  the new fields are optional and additive. Adoption begins only after
  compatible PTY and st2 binaries are deployed; Nix-generated declarations
  first add their ownership marker.
- Decision 0015-immutable-agent-id-and-mutable-address superseded this unaccepted draft and independently accepted the
  presentation behavior it retained.

## Options

| Option | Result | Reason |
| --- | --- | --- |
| Stable identity plus direct `name` and `description` | Selected | Added mutable presentation without changing routing, task identity, or durable state. |
| Sibling mutable `name` file | Rejected | Created a second source of truth outside canonical Agent Spec authoring. |
| Rename the stable identity | Rejected | Disconnected routing and continuity and had no proved state migration. |

## Evidence and Argument

PTY already separated immutable native session ID from mutable display metadata
and provided one atomic exact-ID metadata patch. The st2 parser, roster,
source-preserving authoring, authority, and no-restart reconciliation proofs
listed below established that direct presentation fields compose with the
existing runtime without changing task incarnation or durable state.

## Amendment 1 — immutable subject ID and mutable address

Accepted decision 0015-immutable-agent-id-and-mutable-address supersedes this draft's routing model and independently
accepts its bounded presentation and authoring behavior. The target adds an
explicit immutable `id`; legacy subjects receive their existing host-qualified
bus identities as IDs during migration. Positional `identity` remains the
legacy address fallback. Agent ID is catalog-global; agent address is mutable
and unique per logical host; bus address is `<host>.<address>`. Exact ID
selection is explicit.

Decision 0015-immutable-agent-id-and-mutable-address also independently accepts nondisruptive PTY presentation
projection. Its rejection of a stable-ID alias remains: address is a mutable
route to the subject, not a second stable ID, and old addresses receive no
redirect or history.

## Evidence proposed by the draft

The unaccepted draft proposed:

- parser and roster tests across KDL, TOML, and JSON;
- source-preservation, authority, Nix refusal, and stale-writer tests;
- exact-ID PTY projection tests for set, clear, idempotence, and partial failure;
- exact ID roster selection, nullable values, absent-ID refusal, and co-located
  declaration tests;
- a live no-restart test preserving stable task ID, PID, creation identity, and
  generation across Agent Spec presentation changes;
- a genuine lifecycle-change control that still performs ordinary replacement.

Decision 0015-immutable-agent-id-and-mutable-address carries the accepted proof obligations.
