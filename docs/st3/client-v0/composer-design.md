# Client v0 composer proposal

Status: proposed. Review: Nathan. No implementation changes in this document.

This proposal extends the [client contract](README.md) for a web chat composer.
It addresses sender-selected delivery in [#277](https://github.com/compoundingtech/smalltalk/issues/277), command discovery, and message-associated model selection.
The schema and fixtures remain normative until implementation PRs change them.

## Scope and terms

| Term | Meaning |
| --- | --- |
| Steer | Deliver through the harness's supported in-turn injection boundary. |
| Queue | Hold the message in st until the harness is next observed idle. |
| Pending | Accepted by st, but not yet consumed by the harness. |
| Model switch | Change the session model before delivering the associated message. |
| Command | A server-discovered operation invoked by name with typed arguments. |

The composer uses `POST /v1/client/actions`, not a privileged message endpoint.
The conversation WebSocket remains read-only.
This proposal does not define composer layout, terminal automation, or arbitrary harness command execution.
It adds no implementation, generated client changes, or ratified requirements in this PR.

A client-contract design document is the appropriate home: `.delta` records existing divergence; `.experiments` records measured evidence.
This is a proposed boundary extension, not either of those records.

## Current boundary

- `message.send` has `to` and `content`, with optional session, title, reply, tags, attachments, and signature fields.
- `MessageSendParameters` rejects additional properties. Sending a new field to an old server is not compatible.
- Native delivery paths select steer today. Each harness defines its injection boundary.
- omp holds incoming mail to a tool-batch boundary or run end, coalesces it, and bounds that hold at ten seconds.
- The idle-only terminal fallback cannot promise in-turn steering.
- Model and effort are launch-time inputs. Harness-context records report observations; they are not model-setting commands.
- There is no client command catalog or runtime model-setting action.
- `GET /v1/client/capabilities` advertises daemon capabilities. Required unknown capabilities stop a client; optional unknown capabilities are ignored.

Sources: [schema](schemas/client-v0.schema.json), [operation manifest](schemas/operations.json), [pi channel](../../../crates/st-drivers/src/pi_channel.rs), [omp channel](../../../crates/st-drivers/hooks/omp-channel.ts), and [harness-context spec](../../vrs/08-harness-context/spec.md).

Reuse pending [#1441](https://github.com/compoundingtech/smalltalk/pull/1441) for fenced queue/model controls, signed owner dispatch, and durable control receipts.
Its steer admission currently refuses with `native-pre-dequeue-api-unavailable`; a native steer API alone is not proof of safe admission.
Compose discovery with [#1516](https://github.com/compoundingtech/smalltalk/pull/1516), not a second native scanner.
Its inventory intentionally exposes names without descriptions or bodies; inventory does not grant invocation.

## Proposed send envelope

The existing `message.send` parameters gain three optional fields:

```json
{
  "to": "agent-id",
  "content": "Continue with the smaller model after this turn.",
  "delivery": "queue",
  "model": "provider/model-id",
  "effort": "low"
}
```

| Field | Contract |
| --- | --- |
| `delivery` | `"steer"` or `"queue"`; absent means `"steer"`. |
| `model` | A model identifier from this agent's advertised allowed models. |
| `effort` | An effort value advertised for the selected model; requires `model`. |

Existing send fields remain unchanged; the action gateway carries the additional control fences described below.
Model identifiers and effort values are opaque to clients, not parsed from labels.
The server validates the requested combination against the current runtime.
An accepted action is not proof of harness consumption.

### Message identity and durable receipts

`client_message_id` is the `message.send` action's `idempotency_key`, projected on message bodies and receipts; it is not a second caller-selected key.
Persist the action ID, key, full request, and conversation binding in the client outbox before submission.
Retain paired-session action-key scope; proposed logical-send deduplication also binds authenticated actor, conversation binding, and request digest across reconnect or re-pair.
Same identity and input returns the original message and receipt; conflicting reuse is `idempotency-conflict`.
Distinct IDs with identical text remain distinct. Reconcile optimistic bubbles by identity, never by text.
Map native correlation IDs to that identity when available.
Reuse #1441's receipt boundary: reserve before native handoff, persist outcomes, and replay saved results before stale-fence rejection.
A possible native effect without settlement is indeterminate, not permission to resend under a fresh key.
Replaying an accepted send must not repeat a model switch or enqueue a second message.
DQ10 covers actor-scoped retention across re-pair; this proposal does not reinterpret the README's legacy 30-day cleanup exception as an existing general deduplication guarantee.

## Delivery ownership

```text
message.send -> validate runtime support -> st pending message
                                         -> steer boundary -> consumption receipt
                                         -> idle observation -> queue delivery -> consumption receipt
```

Steer preserves today's behavior when `delivery` is absent.
It does not mean immediate interruption, cancellation, or a guaranteed tool boundary shared by all harnesses.
Queue never injects into an active turn.
The driver supplies an authoritative idle observation before st releases a queued message.
A stale idle observation is not permission to deliver into a newly active turn.

st owns the hold. Do not translate queue to pi `followUp`.
That hides pending work inside the harness and prevents a consistent client projection.
The existing pi-family path also records worse behavior for omp `follow_up`; see [delivery selection](../../../crates/st3/src/main.rs).
Queue order is acceptance order per destination session.
Separate accepted messages retain separate identity even if a driver coalesces transport notices.

The send receipt exposes `accepted -> pending -> delivered` in the conversation stream.
`accepted` means admitted by st; `pending` means held, including a queued message; `delivered` means consumed by the harness, not merely written to a transport.
Immediate consumption may skip a visible pending interval. Failed or indeterminate outcomes remain explicit.
Requested delivery mode and message identity accompany these updates; resumed streams and reads recover the same outcome.
A runtime becoming unavailable must not silently turn queue into steer.
Reject unsupported combinations before acceptance with a typed error, never a downgrade.
If support disappears after acceptance, retain an observable pending or failed outcome under the existing delivery lifecycle.
The existing message lifetime and failure policy applies; this proposal adds no hidden expiry or retry policy.

### Queue visibility and removal

Each conversation read and stream projects an ordered list of queued entries, each with a stable entry `id`, `client_message_id`, content, and requested model/effort.
Reuse #1441's queue entry identity and receipts; do not introduce a second queue ledger.
Add `message.cancel-queued`, fenced by the entry ID and current conversation/session/incarnation binding.
The server removes only that still-pending entry; an entry already dispatched refuses rather than cancelling an active turn.
Replay returns the saved cancellation receipt, not a second removal.
Edit means cancel plus re-send: wait for confirmed removal, then send edited content with a new message ID.
If removal is refused, do not send an automatic replacement. DQ6 asks whether to expose in-place editing instead.

## Exact-turn cancellation

Propose `run.cancel` as a separate action, not a use of `runtime.signal`.
`runtime.signal` sends SIGINT to a fenced runtime; success does not prove an exact turn stopped.
Timeline entries and status bodies gain `turn_id`; running, completed, failed, and cancelled status identify their exact turn.
The cancel request names that turn and carries active-run, conversation/session binding, and runtime-incarnation fences.
Revalidate before native dispatch: a delayed request must never cancel its successor.
Apply normal action idempotency and durable receipts; acceptance means cancellation requested, not cancellation confirmed.
Only authoritative native evidence settles the target as canonical `cancelled`; preserve partial content and identify the cancelled turn.
Return typed refusals for a settled target, stale fence, unsupported exact abort, or unavailable authority.
A possible abort without settlement remains indeterminate, not a reason to retry on a new turn.
Do not advertise support if the adapter cannot exclude successors or if abort affects unrelated work.
Proposed cancellation preserves queued next-work input and independent jobs; DQ5 requests an explicit decision.

## Discoverable commands

```text
GET agent commands -> client renders catalog -> command.invoke
                                            -> server validates -> action handler
```

Add `GET /v1/client/agents/{agent}/commands`.
Each catalog entry contains:

| Field | Meaning |
| --- | --- |
| `name` | Canonical command name without the leading slash. |
| `description` | Short text suitable for a command picker. |
| `arguments` | JSON Schema subset describing an argument object. |
| `source` | `"st"` or `"harness"`. |
| `availability` | Supported and currently invocable, temporarily unavailable, or unsupported; includes a reason when not invocable. |

The client renders `/` before `name`; the slash is presentation, not a wire delimiter.
st owns these repository-local names. Phase 1 names are lowercase ASCII words separated by hyphens.
Names are unique within an agent catalog. Reserved st names cannot be replaced by harness entries.
The exact collision policy for later harness entries is DQ3.
The initial argument subset is object properties, required fields, primitive types, enums, and bounds; no executable expressions or remote schema references.
The implementation PR must enumerate the supported keywords in the contract.

Add action `command.invoke` with `{agent, name, arguments}`.
The server resolves the name and checks current availability, authorization, and argument validity.
Catalog availability is guidance, not an authorization grant or a runtime-state fence.
Unknown names, unavailable commands, and invalid arguments produce typed action errors.
The client never hardcodes command-to-action mappings or sends slash text as an implicit command.
Ordinary `message.send` content beginning with `/` remains message content.

Phase 1 is st-defined commands only:

| Command | Server mapping |
| --- | --- |
| `/clear` | `runtime.context-clear` |
| `/restart` | `runtime.restart` |
| `/model` | A new runtime model-switch action using the same validation as `message.send.model`. |

The mapping is illustrative catalog content, not a new client convention.
A server publishes `/model` only when its model-switch implementation is available.
Phase 2 is harness-reported commands and skills through drivers for pi, omp, and claude.
That phase requires structured discovery and invocation, not parsing terminal help or evaluating arbitrary shell text.
Use #1516's native inventory as the discovery input; inventory-only names remain noninvocable until admitted invocation and argument contracts exist.
Catalog/model revisions invalidate cached choices, and command invocation carries the catalog revision fence.

## Model switch semantics

```text
accepted message -> wait for delivery boundary -> set model and effort
                                              -> deliver message -> report observed turn model
```

The proposed default is sticky: the chosen model applies from this message onward in the session.
The field is associated with a message; it does not promise a one-turn override.
Switch immediately before that message's delivery, not when a queued message is accepted.
A later queued model choice must not affect an earlier message.
If switching fails, do not deliver under the previous model as a successful switch.
Selection and delivery remain ordered within the destination session.
The implementation must expose failure if switching succeeded but subsequent delivery failed; it must not claim the model was restored.

| Harness | Proposed support; not an implemented guarantee |
| --- | --- |
| pi / omp | Driver RPC `set_model` before delivery; confirm model and effort semantics during implementation. |
| codex | `turn/start` model override when starting a turn; retain the choice for subsequent starts. |
| claude | Unsupported until a supported runtime switching surface exists. |
| Other or terminal-only runtime | Unsupported unless its driver proves the requested behavior. |

Codex cannot switch the model of an active turn through `turn/steer`.
A send with a model change into an active codex turn fails with `model-switch-needs-idle` unless `delivery: "queue"` permits waiting.
The client can offer queue explicitly; the server must not choose it silently.
An idle race is checked at dispatch, not only at acceptance.

The message receipt and conversation stream distinguish requested model from the model actually used.
The latter comes from an authoritative harness-context turn observation, not the configured launch model or the requested value.
It is unknown until observed; use the contract's null convention rather than inventing an identifier.
Codex currently reports no model identifier, so its implementation must add authoritative reporting before claiming this criterion is met.

## Capability negotiation

Add optional daemon capabilities `message.delivery.queue`, `message.model`, and `agent.commands`.
These names are repository-owned client capability identifiers, not new WebSocket subprotocols.
Absence means that the client must not send the corresponding new fields or operations.
Old servers keep receiving the old envelope.

A daemon capability means the API exists; it does not mean every conversation supports it.
Per-conversation control state is `supported | unsupported(reason) | unknown` for steer, queue, model switch, commands, queue removal, and exact-turn cancel.
A known read-only source is unsupported; an unobserved or unreachable live owner is unknown.
Missing observations never become unsupported or supported by inference from a harness name.
The agent/runtime read supplies the same support facts plus allowed model identifiers and effort values.

Carry optional `controls` on `TimelinePage`, `ConversationChanges`, and collection `kind: "conversation"` frames, including initial empty reads/frames.
Controls contain canonical conversation/session identity, runtime incarnation, revision, and per-control state.
Binding, activity, or support changes emit control-only updates (`items: []`) and advance the conversation cursor.
Reconnect/reset reloads controls; replacement invalidates old write targets. Catalog/model revisions invalidate cached choices.
Server validation remains authoritative; a read/frame is not authorization.
Use existing `control.messages` for send/steer, `control.runtimes` and #1441's concrete-person admission for model/cancel, and each command handler's existing scope; DQ9 asks for confirmation, not new pairing scopes.

## Typed refusals

| Code | Meaning |
| --- | --- |
| `unsupported-capability` / `native-pre-dequeue-api-unavailable` | Identify the feature and precise adapter admission refusal. |
| `stale-fence` | Binding, incarnation, turn, catalog, or model revision changed; refresh without automatic retargeting. |
| `idempotency-conflict` | Action key or client-message identity reused with conflicting input. |
| `turn-not-active` | Exact steer/cancel target settled; never operate on its successor. |
| `model-switch-needs-idle` | Active turn cannot change model; queue requires explicit selection. |
| `model-unavailable` / `effort-unsupported` | Requested selection cannot be honored; adopt #1441's native refusal reasons. |
| `command-not-found` / `command-unavailable` | Unknown registry name versus registered but noninvocable command. |
| `remote-unavailable` / `runtime-authority-indeterminate` | Owner is unknown; inspect reserved receipt before retrying. |
| `native-outcome-indeterminate` | Possible native effect without settlement; no automatic fresh-key retry. |
| `queue-entry-not-pending` | Queue removal raced dispatch; do not cancel a turn or re-send automatically. |

## Alternatives

| Alternative | Reason not selected |
| --- | --- |
| Harness-owned follow-up queues | Pending work would be hidden from st and inconsistent across drivers. |
| Slash text interpreted on send | Ambiguous with ordinary text; no typed arguments or discoverable availability. |
| Client hardcoded command mapping | Clients diverge as actions and harness support change. |
| Restart for each model choice | Changes session lifecycle instead of switching the current session. |
| One-shot model override | Requires reliable turn completion and restoration; see DQ1. |
| Daemon-only feature flags | Cannot express unsupported individual runtimes. |

## Implementation ownership and PR split

| PR | Owner boundary | Deliverable |
| --- | --- | --- |
| PR1 | Client schema, daemon delivery, drivers | Reuse #1441 for queue capability, controls frames, message identity, send delivery field, pending queue/removal and receipts; addresses #277. Exact-turn cancel requires DQ5 admission. |
| PR2 | Client schema and command dispatch | Reuse #1516 inventory; catalog read, `command.invoke`, phase 1 st commands; `/model` waits for PR3 support. |
| PR3 | Runtime model control and drivers | Model/effort send fields, shared model-switch action, allowed-model read, observed receipt and turn reporting per harness. |

Each PR updates schema, operations, normative fixtures, and the client README.
Each regenerates Rust, Swift, and TypeScript clients, including Effect Schema output, with `st3-client-codegen`.
Generated TypeScript must expose the fields, reads, actions, capability names, and typed errors without an untyped escape hatch.
Implementation verification includes generator freshness and the TypeScript client gate.
A downstream web client can build against those schemas and retain Unsupported or Unknown states until support is observed.
No downstream client implementation belongs in this design PR.

### Proposed acceptance scenarios (not executed)

- Empty reads, owner loss, permission revocation, replacement, control-only updates, and reset preserve the support tri-state.
- Identical text with distinct IDs stays distinct; lost-response, reconnect, and re-pair retries recover one receipt and one bubble; conflicting reuse refuses.
- Queued entries remain ordered and removable; removal racing dispatch refuses without an unintended replacement.
- Steer/cancel racing completion or replacement refuses the stale target without touching its successor; cancellation preserves partial content and queued input.
- Inventory-only names remain noninvocable; stale catalog revisions refuse; a successful model switch followed by failed delivery has an explicit partial receipt.

## Open questions for Nathan

- **DQ1 Sticky or one-shot model selection and settlement?** Proposed: sticky. Specify restoration for one-shot, selection/input atomicity, and the receipt for successful switching followed by failed delivery.
- **DQ2 Queue lifecycle across session replacement?** Decide whether a pending message remains bound to its original session/runtime or can be explicitly retargeted. Existing fences must prevent accidental delivery to a replacement.
- **DQ3 Harness command collisions and argument schema?** Choose qualified names and supported keywords; do not allow shadowing or make #1516 inventory names invocable without admitted invocation contracts.
- **DQ4 Model and effort policy authority?** Confirm whether the allowed-model read reflects driver discovery, agent policy, or their intersection. Proposed: policy-filtered driver support; unsupported effort combinations fail explicitly.
- **DQ5 Exact-turn cancel and queue preservation?** Require turn-only cancellation preserving queued input and independent jobs? Proposed: yes; adapters that cannot exclude successors or broader effects are unsupported.
- **DQ6 Queue edit semantics?** Proposed: confirmed cancel plus re-send with a new ID. Prefer fenced in-place editing using #1441's replace operation?
- **DQ7 Reuse and admission prerequisites?** Confirm #1441/#1516 as prerequisites. What native turn/admission proof permits supported steer instead of `native-pre-dequeue-api-unavailable`?
- **DQ8 Cancel capability advertisement?** Confirm the action capability and per-conversation state required before exposing `run.cancel`; do not substitute SIGINT support.
- **DQ9 Permission and actor admission?** Retain existing message/runtime scopes, concrete-person model/cancel admission, and each command handler's scope without adding pairing scopes?
- **DQ10 Durable identity across re-pair?** Approve actor-scoped deduplication for at least 30 days while retaining paired-session action-key scope, using #1441's receipts and explicit indeterminate outcomes?
