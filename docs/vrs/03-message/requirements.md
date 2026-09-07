# Message requirements

## Context

This subsystem defines the native message bus beyond terminal delivery. It
refines root requirement [`R05`](../requirements.md) for durable message
publication and inherits immutable agent ID and mutable address semantics from
`R19` and `R24`. DING consumes the recipient inbox under
[`01-ding/requirements.md`](../01-ding/requirements.md); this subsystem does not
redefine terminal delivery.

## Assumptions

- **MESSAGE-A01 Declared senders:** A catalog-backed ordinary sender is an Agent Spec identity. The
  eval-owned external requester remains an explicit compatibility capability because the canonical
  eval flow requires one non-Agent requester mailbox. A message with that capability at either
  endpoint is external eval traffic, not ordinary Agent Sent history, and does not weaken the
  Agent-only ownership contract.
- **MESSAGE-A02 Local publication filesystem:** One send executes against one selected catalog
  filesystem. Replication and network delivery remain eventual under root `A04`.
- **MESSAGE-A03 Bounded local trust root:** The atomically replaced sender head is the local
  completeness trust root. Coordinated replacement of that head and every matching sender record is
  outside the local loss and corruption contract.

## Acceptable Tradeoffs

- **MESSAGE-T01 Ordered local commits:** Recipient and sender directories do not commit atomically.
  st2 may expose a pending sender intent after interruption, but it must not expose a completed sent
  row before recipient publication.
- **MESSAGE-T02 Forward coverage:** st2 does not reconstruct sender history that predates the first
  sender-index marker. The API reports that boundary instead of presenting historical absence as an
  empty complete result.
- **MESSAGE-T03 Unkeyed response ambiguity:** An unkeyed send is at-least-once across a crash after
  commit but before the caller observes the result. Exact replay safety requires a caller-supplied
  idempotency key because identical intentional sends and an unkeyed retry are otherwise
  indistinguishable.

## Requirements

### Must own an honest sender view

- **MESSAGE-R01 Sender-owned enumeration:** Every successful ordinary `message send` and `message
  reply` by an indexed sender creates one durable sender-owned row. `message sent` enumerates only
  that sender-owned state; recipient inbox and archive state cannot remove or supply its rows.
- **MESSAGE-R02 Canonical direction and readable projection:** Ordinary Agent
  publication resolves each sender and recipient address to one immutable agent
  ID and persists those IDs as canonical direction and provenance. It also
  appends each endpoint's publication-time bus address as display-only snapshot
  metadata. A sent row carries canonical `to`; it does not repurpose the inbound
  row's `from` field. Inbox rendering includes canonical sender ID and readable
  sender address. DING resolves the sender ID to its current bus address at
  delivery and otherwise falls back to the immutable ID, showing the
  publication snapshot only when it is explicitly marked historical, because a
  released address is immediately reusable. Replies and automation always use
  the persisted ID. A separately admitted principal or external endpoint uses an
  explicit endpoint-kind discriminator and its canonical principal/external
  address; it is never represented as an agent ID.
- **MESSAGE-R03 Explicit coverage:** Machine output distinguishes an unavailable index, coverage
  beginning at one unix-millisecond boundary, and partial coverage with pending intents. An absent
  or interrupted index must never serialize as a complete empty history. Count output refuses
  unavailable or partial coverage. A constant-size head commits to an immutable linked ledger of
  every completed row. Exact traversal to genesis rejects missing, extra, substituted, corrupt,
  unreadable, or version-mismatched head, node, active-intent, pending, and row state instead of
  weakening the coverage claim.
- **MESSAGE-R04 Stable shared wire:** The sent envelope and row are serialized
  through `st2-wire`. Optional metadata and body omission preserve absent versus
  empty values. A durable record version changes whenever an existing field's
  meaning changes or a strict record gains fields. A reader may reinterpret a
  version-1 endpoint as an agent ID only for legacy bus identities that
  migration froze for that same subject. Bytes that migration reassigned away
  from an archived collision resolve only for the row's own state owner and are
  otherwise rendered as an unattributed historical address that carries no
  reply or automation authority; the reader never attributes them to the live
  subject that kept them.

### Must survive interruption without a false Sent claim

- **MESSAGE-R05 Recipient-first publication:** A send durably reserves sender intent, materializes
  the recipient copy idempotently, then publishes the sender row. A completed sender row proves that
  recipient materialization succeeded at least once. st2 makes no cross-directory atomicity claim.
- **MESSAGE-R06 Resumable intent:** A later sender operation resumes durable pending intents with
  their original filename and bytes. Interruption cannot create a second recipient filename or a
  second sender row for the same intent. An exact pending duplicate of the current committed head tip
  is cleanup state. A read reports completed coverage, and the next sender operation removes it.
- **MESSAGE-R07 Exact keyed retry:** An idempotency key is scoped by `(canonical sender, canonical
  recipient, idempotency key)`. Reusing one scoped key with the same message returns the original
  filename after any publication or response boundary. Reusing that scoped key with different
  message content fails. The key remains sender-owned even if the recipient deletes its copy.
- **MESSAGE-R08 Serialized sender writes:** Concurrent sends from one sender serialize through one
  local kernel lock. Intentional identical unkeyed sends remain distinct; payload equality is not an
  idempotency key. Each publication performs constant history-dependent write work; enumeration and
  ledger validation remain linear in sender history.

### Must remain usable as one bounded read

- **MESSAGE-R09 Directional filters:** `message sent [address]` selects the
  positional address, then `--as`, then the exact acting ID from `ST_AGENT`,
  and supports an explicit mutually exclusive `--id`. It also supports
  `--count`, `--include-body`, strict `--since`, JSON, and `--to` as the
  directional analogue of inbox `--from` wherever coverage can be represented
  honestly.
- **MESSAGE-R10 Catalog-scale read:** On one captured real catalog with at least 557 agents and at
  least one real sender row, one warm-up followed by ten timed reads of both `message sent --json`
  and same-sender `message ls --json` records both p95 values and their ratio. Sent p95 is no greater
  than 1.0 second and no greater than twice message-ls p95. The read must not depend on the number,
  readability, or retention state of recipient boxes; an unrelated recipient box remains
  structurally unreadable while Sent returns the sender rows without scanning it.
- **MESSAGE-R11 Typed-request separation:** Service-principal request publication state does not
  appear in ordinary Agent Sent history unless a later requirement defines that projection. The
  explicit external-eval requester capability is also excluded whenever it is either endpoint.

## Evidence

The crash and retry state model, alternative analysis, and benchmark method are recorded in
[`2026-08-12-sender-history-crash-model.md`](./.experiments/2026-08-12-sender-history-crash-model.md).
Executable controls live in `tests/message_cli.rs` and the shared schema controls live in
`crates/st2-wire/src/message.rs`.
