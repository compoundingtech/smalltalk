# Interrupted-turn receipt source (draft qualification descriptor)

An idle snapshot and native session selection do not prove an interrupted turn ended.
The driver persists start receipts and positive terminal evidence in its FULL-sync
outbox. The graph projects their union at claim ingestion. Readers use materialized
receipt heads and cut versions; they do not replay native history.

This descriptor covers slice a of issue #1730. Slice b, the two reported seats
without successor observations, needs independent runtime evidence. No continuation
input, tool replay, automatic answer, or execution-resume API is implemented.

## Identity and settlement

`source_sequence` names an st receipt, never a provider turn/checkpoint. Its producer
fence is provider incarnation plus ownership sequence. Immutable receipt identity
also includes runtime incarnation, desired revision, native session/turn IDs and
start time. Hashing these fields into `receipt` prevents a changed revision/session
on the same source sequence from closing a different execution.

A native join is available only when desired revision, session ID, and turn ID are
all nonempty. Codex supplies exact thread and turn IDs. OMP supplies a native session
ID and a hook-retained st receipt, with no invented native turn ID. An OMP successor
cannot close its predecessor using that receipt. A channel replacement in the same
provider invocation can consume the receipt retained by the hook.

Only typed completion, cancellation, or failure settles a matching receipt. Idle,
process exit, lost visibility, prompt answer delivery, and session selection do not.
Qualified native terminal evidence also closes delayed starts carrying that exact
native join, independently of arrival or canonical snapshot order.

The launch captures `ST3_TURN_DESIRED_REVISION` using the existing pure canonical
hash of its captured `DesiredSubject`, after the environment overlay. It overrides
authored/inherited values and is removed for non-agent launches. No fresh declaration
read, launch-lineage alias, or effect/backoff decision is used for that capture.

## Physical tables

| Table | Physical primary key | Values and reverse dependencies |
| --- | --- | --- |
| `agent_turn_obligation_evidence` | `(subject, receipt, claim_id, terminal)` | native join, terminal bit, complete canonical key encoded as hexadecimal TEXT, local visible index, canonical JSON evidence body; OLD/NEW subject, receipt, native join, source claim |
| `agent_turn_obligations` | `(subject, receipt)` | native join, selected source claim/key/body, first-visible index, first-terminal-visible index, acknowledgement-visible index; OLD/NEW subject, receipt, join, source claim |
| `local_turn_obligation_versions` | `(subject, receipt, visible_index)` | selected source claim/body and terminal bit at that local graph cut; OLD/NEW subject, receipt, source claim, cut |
| `local_turn_obligation_pending` | `(claim_id)` with subject index | OLD/NEW source claim and subject |
| `local_turn_obligation_dirty` | `(subject)` | OLD/NEW agent subject requiring exceptional reconstruction |

Shared digests include selected receipt heads. The candidate evidence index is local:
checkpoint compaction may remove superseded carriers without changing selected debt.
The checkpoint reader proof also compares complete current turn evidence. Local arrival
indices (including acknowledged_index) are excluded from shared digests; cut versions
and mutation queues are local projections. Canonical
key selection includes admitted claim, batch origin/sequence, accepted decimal time,
wire position, batch ID and claim ID. Original SQL body and source claim remain
available for attachment and reader parity.

The corrected shared compatibility domain is `st3.shared-projections.turn-obligations.v4`.
Use the existing `store.runtime.schema_digest()` (or `crate::store::runtime().schema_digest()`
at construction) to bind the complete registry/layout fingerprint; the physical descriptor
and pure reducer fingerprints alone do not qualify shared authority. No new getter is needed.

Owned private controls measure a 400-byte tool delta, 11 traced projection statements
including digest triggers, and one evidence/head/version write each. An unchanged heartbeat
adds no receipt writes. The end-to-end checkpoint control drops superseded tool carriers,
proves graph and reader parity, and preserves current debt after trim. Receipt acknowledgement
projection is arrival independent; repairing its exact cited source retires its effect.
The full driver library has 978 passing controls and three existing ignored controls.
These measurements do not cover first-open migration or certify Source21 activation.

Claim insertion queues bounded ledger evidence; delete/body/identity/time/index
mutation reconstructs the affected subject. Batch origin/sequence and replica-record
position/claim/admission-state mutation invalidate canonical selection. Legacy earlier-position
insertion invalidates affected unstamped batch claims. OLD and NEW keys both matter.
Ordinary envelope receipt seeding that agrees with its legacy position stays quiet.

Rebuilds run at projection opening/version migration, explicit projection rebuild,
and checkpoint replay. Pending claims and exceptional subject reconstruction are consumed in store-index pages of 128. This bounds allocated pages, not total migration/repair work: exceptional mutation still reconstructs the affected subject inside the writer transaction. A source capture refuses pending/dirty repair by indexed subject seeks; it never performs that reconstruction during a read. The aggregate 128-row limit applies to the enclosing Kernel consume/repair/visibility work, not every legacy writer flush. This is not a bounded-total-repair certificate for Kernel activation.
Repaired-original claims confer no start or terminal authority. Removing a repaired terminal proof reopens remaining admitted starts; removing all admitted starts leaves no graph receipt. Metadata renumbering reselects canonical source bodies and rebuilds local cut versions from admitted source history.
Checkpoint rules keep the newest complete single-open-receipt carrier in its own receipt slot, while terminal, unknown, tool-result and multi-receipt evidence stays retained. A later sparse observation cannot witness start or terminal evidence. No retained claim is dropped merely
because a receipt is currently terminal. Deleting a terminal source invalidates its
projection and exposes remaining start evidence again.

## Bounded reads and captured eligibility

Current reads seek the partial unresolved-head index and return at most 33 candidates.
Each candidate seeks one cut version by its exact subject/receipt primary-key prefix.
Older cuts additionally seek terminal visibility. The 33rd candidate makes the result
unknown; it cannot silently certify no debt. The output contains at most 32 actionable open
receipts plus an unknown bit and exact source claims, within a 24 KiB combined actionable-body budget and a 64 KiB complete output bound. Exceeding either bound makes the result unavailable. All captured canonical images (including unknown and acknowledgement rows) are counted: at most 32 images, each at most 64 KiB, with an aggregate 2 MiB input bound. Acknowledged native bodies are validated and retained by exact unknown receipt/source references and attributed audit in selected_receipts; archived arguments and tool IDs do not crowd out a normal turn. No per-read historical fold is
performed. `source()` uses at most 68 physical rows/seeks: two health checks plus at most 33 candidate heads, 32 selected cut versions, and empty range probes within the same 68-point bound. The overflow witness refuses completeness without a version lookup; 32 named sources remain inspectable and acknowledgeable. The legacy view wrapper adds captured declaration/native-claim/repair eligibility seeks; the enclosing union must budget these too. Current declaration eligibility is one materialized `desired`/claim seek;
an older cut without that selected declaration cannot certify a current episode.

Producer snapshots contain at most 32 open receipts, 64 recent terminal proofs, and
128 pending tool IDs per receipt, within a 24 KiB schema bound (16 KiB start/tool budget). The graph retains
terminal proof independently, so the recent producer window does not reopen old debt.
Missing native fences, damaged predecessor evidence, or exhausted producer bounds
remain unknown. Schema validation applies to local and replicated admission.

`TurnRecoveryInput` is an immutable captured input containing desired revision/token, runtime incarnation, provider incarnation/ownership sequence, host eligibility, native state/human blocking and explicit stop/suspension. `capture(Connection, subject, cut, input)` returns `CapturedTurnRecovery { graph_index, input, value }`; formatting this output reads neither Store nor producer files. `value=None` means no admitted graph receipt at that cut, not certified native tracking capability. An unqualified baseline source must expose unavailable/unknown, not turn absence into a normal no-debt field.

An in-flight or native human-wait classification requires all these current runtime,
provider ownership, host and captured declaration fences. A resumed live native turn may qualify predecessor receipts
only through a matching exact desired/session/turn observation in that runtime. A
pending or submitted private prompt is never terminal turn evidence, and these
classifications do not authorize a new person decision or automatic response.

Suspension/intentional stop retain evidence without launching execution. Ownership,
host, runtime replacement, declaration selection, terminal source deletion, and
captured clock/cut changes invalidate attached card eligibility. Runtime faults use
the current declaration/incarnation fence and disappear when their source episode
no longer requires action. Missing successor observation does not erase old debt. Exact turn termination preserves a separate unknown-tool-outcome sentinel when an invocation has no observed result; no automatic replay or invented result settles that uncertainty. Captured output does not create a new deadline: native freshness/clock deadlines remain dependencies of the caller's already captured native view.

## Physical attachment hook

`turn_obligation::physical::{Key, Row, Change, capture_row, reselect}` captures exact primary keys on the caller's same commit-inclusive connection. `Row` retains all physical cells and the immutable PK before mutation. `Change` retains OLD plus optional NEW PK; reselection returns complete OLD/NEW rows and their subject dependency union. Removal retains the OLD subject, retargeting retains both subjects, and canonical renumbering reselects changed values under the unchanged PK. No subject/native fanout scan or legacy flush occurs in this hook. The union owner retains and binds raw table changes and repairs cards; these helpers do not install that binding.

The caller supplies its remaining aggregate budget, at most 128. Each retained OLD consumed and each NEW lookup, including absent rows, charges one unit. Initial OLD capture separately charges one lookup. Oversized batches, malformed retained PKs, invalid cut keys, unsupported SQL values and rows over 64 KiB refuse the operation. The enclosing Source must defer the entire family on refusal, dirty/pending reconstruction, missed capture or unqualified callback coverage; it cannot publish a clean null. Delta owns attachment and the commit-inclusive source/callback proof.

## Public activation fence

Baseline `st3.agent-card.complete.v2` / `public-card-v0` does not cover these tables or
`turn_recovery`. It cannot supply a normal absent/none result for this dependency.
Activation needs an explicit successor physical union manifest/source binding,
qualified Kernel operators, CardParts/formatter/read-row parity, and the full OLD/NEW
incarnation/ownership/declaration/clock/local dependency closure. AgentCore owns that
card seam; Delta owns successor source attachment. There is no independent registry,
Installer, or Publisher in this feature.

This draft is not a completed certificate. Actual SQL work bounds, reordered replica
admission, native parity, terminal source deletion, and public activation must have
test and exact source revision evidence before qualification or landing.


## Reversible acknowledgement/pure-seam checkpoint

This importable helper contract is qualified by the owned native and source controls
below. It is not Source21, card activation, or merge qualification. The source owners
retain unavailable production behavior until their exact composition is qualified.

The owned pure chain is `aggregate_receipts(Vec<CapturedReceipt>, action_overflow)`
returning `Result<Option<(String, Value)>>`, followed by
`from_evidence(graph_index, TurnRecoveryInput, evidence)` returning
`CapturedTurnRecovery`, then `apply_capture(&mut CurrentHarnessView, &capture)`
BEFORE since/transition enrichment. `CapturedReceipt` carries a receipt key and
`Option<(source_claim, canonical_body, terminal)>`; a known missing selected version
is incomplete, never clean absence. Native capture delegates to these same functions.

The exact schema-order tuples and keys are `physical::TABLES` (five tables):

- evidence: subject, receipt, claim_id, native_key, terminal, canonical_key,
  visible_index, body; key subject/receipt/claim_id/terminal.
- heads: subject, receipt, native_key, source_claim, source_key, body, first_index,
  terminal_index, acknowledged_index; key subject/receipt.
- versions: subject, receipt, visible_index, source_claim, body, terminal;
  key subject/receipt/visible_index.
- pending: claim_id, subject; key claim_id.
- dirty: subject; key subject.

`schema_fingerprint()` hashes these complete descriptors.
`reducer_fingerprint()` hashes `REDUCER_CONTRACT` plus that physical fingerprint;
these are contract fingerprints, not proof that arbitrary code changes preserve
semantics. Tests print both values and check actual PRAGMA column/PK order.

An acknowledgement is an attributed durable `harness.turn-acknowledged` claim with
exact receipt/source pairs, captured selected declaration/cut, actor, acceptance
stamp and reason. Its evidence row has receipt `ack:` plus the original receipt
and terminal=false. The original unresolved head retains the native evidence and
adds `_acknowledgement`; the ninth index is local arrival metadata and excluded
from the shared digest. It quiets owner action only. Execution remains unknown,
never completed. A different interruption has a different receipt and stays owed.

Aggregation keeps the complete retained evidence separate from actionable evidence.
Admitted unknown provenance or acknowledged unknown execution is source-complete;
a missing/malformed/overflowed captured image is unavailable. Candidates prioritize
unacknowledged action before bounded acknowledged history; 33 actionable candidates
refuse completeness. The enclosing minimal21 source must reserve the owners' 72
point preflight, charge actual candidate/version/health/eligibility work, and reserve
its shared aggregate remainder before the card slot. No independent namespace,
Installer or Publisher is introduced here.


The bounded reducer v3 also rejects malformed acknowledgement audits, mismatched
native receipt hashes, malformed typed unknown provenance, missing selected versions,
and oversized input/output. Acknowledged open executions remain `unknown=true` with
exact original receipt/source and who/when audit; they are never rewritten as terminal.
The pure actionable ledger excludes these named acknowledged references, so a fresh
normal turn remains in-flight while the old unknown audit stays visible. Late positive
native tool results carry the real original terminal and named tool ID. They settle
only `tools:` plus that original receipt, never another turn or a legacy sentinel.
A clean later native turn alone leaves the original invocation unknown.

Ordinary freshness/categorical writes use atomic rename without the new file/directory
fsync. Receipt ledger changes retain FULL synchronization; the pre-existing enabled
SQLite outbox durability is unchanged. An invented fallback fixture measured 16
start/result boundaries (median 160 us, maximum 185 us on this machine); this is not
fleet or reboot latency evidence. A 32-open producer emits a 400-byte single-receipt
tool delta, one evidence row plus one head/version write, and zero receipt writes on
an unchanged heartbeat. Tool-ID-only updates do not wake reconciliation.

Default driver observations live under the configured persistent state directory,
not the runtime socket directory. This machine's default state directory is on ext4
(`/dev/nvme0n1p4`, read-only mount inspection). State-directory overrides need durable
storage for reboot survival. No live fleet reboot or restart was used as a fixture.
First-open claims-index construction and exceptional projection reconstruction still
scan persisted claims; total migration downtime is unknown and is not a bounded-read
certificate. Local and replicated admission share the same receipt schema.

Observation authority remains this seat's admitted actor/native ownership fences;
this is not a cryptographic attestation of arbitrary provider events. An actor with
that seat's claim authority can forge native evidence, as it can other self observations.


The current owned reducer v3 contract fingerprint is
`736b14059087d33443aad7d3c7fddb9d3a95499cbae414d0f589d962977c19d8`;
the exact five-table schema fingerprint remains
`9d651fe429bd759d16f120d124c37d7a74f64d74dc5c3c4740b02c9f50a763b3`.
Owned source controls: 19 passed, including same-helper native authority, actionable
versus acknowledged unknown, normal in-flight work with retained audit, malformed
capture, signed reordered terminal evidence, physical OLD/NEW and source bounds.
Native receipt controls: 12 passed, including positive late invocation results and
isolated process loss. Both OMP assets pass strict type checking and their smoke
controls cover capability absence, acknowledgement timeout and original-receipt
correlation. All st3 targets compile. These are owned producer/helper controls;
Source21 binding, public existing-field/native parity and activation remain separate
owners' qualification. Core has separately qualified existing-field parity against
immutable native checkpoint 2a0e7f; that proof does not certify Source21 or a later
integration base. The overall PR remains draft pending curator review, ordinary
required checks and the actual Source21 composition.

Fresh positive start receipts commit the active categorical state in the same durable
snapshot. They cannot publish the preceding idle state as interrupted work. Replayed
starts preserve a current human wait, and a start whose exact terminal is already
retained cannot reopen execution. Exact terminal receipt provenance remains after
its own start even for same-millisecond writes; settlement still requires native
identity. The native outbox prefix and loss-before-idle controls cover these cases.

The legacy receipt drain fixes its queue as the outer loop, with indexed claim PK
lookups. The unchanged cost budgets pass all 149 measured routes on 2,508 versus
24,787 claims: the obligation read uses 157 VM steps at both sizes and acknowledgement
uses 2,352 at both sizes, each with zero full scans. Generic claim, document and schema
writes remain constant too. These controls were run on the owned native integration
based on 0fd121; they do not certify a later base or Source21 activation.

The continuation contract is agreed: a supported continuation may finish the same
logical turn with a transcript marker, without injecting a prompt or replaying unknown
tool outcomes. The [upstream response](https://github.com/can1357/oh-my-pi/issues/14898#issuecomment-6050451706)
reports no supported no-input continuation API, durable logical-turn identity or
server-side idempotency today. Inspection through session selection and entries does
not establish that dispatch capability. No continuation is implemented here.
