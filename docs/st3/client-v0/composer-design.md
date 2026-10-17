# Client v0 composer and workbench proposal

Status: proposed. Review: Nathan. No implementation changes in this document.

This proposal extends the [client contract](README.md) for a web client's composer and terminal drawer.
It addresses sender-selected delivery in [#277](https://github.com/compoundingtech/smalltalk/issues/277), command discovery, message-associated model/effort selection, and agent-associated workbench terminals.
The schema and fixtures remain normative until implementation PRs change them.

## Scope and terms

| Term | Meaning |
| --- | --- |
| Steer | Deliver through the harness's supported in-turn injection boundary. |
| Queue | Durably hold an input in st for the next turn; release only with positive owner-side idle and admission evidence. |
| Pending | Accepted by st, but not yet consumed by the harness. |
| Model/effort selection | Apply a requested model and/or effort before delivering the associated message; persistence is DQ1. |
| Command | A server-discovered operation invoked by name with typed arguments. |
| Associated terminal | A person-owned shell linked to an agent, distinct from the agent's harness pane. |

The composer uses `POST /v1/client/actions`, not a privileged message endpoint.
The conversation WebSocket remains read-only.
This proposal does not define layout, terminal automation, arbitrary harness command execution, a permission picker, shelves, or mission queues.
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
- Terminals can be listed by exact owner and state. An agent owner matches its harness pane, not person-owned shells opened alongside it.
- `terminal.create` opens a caller-owned interactive shell with optional host/cwd; `terminal.end` is creator-only. There is no agent association or associated-terminal list filter.

Sources: [schema](schemas/client-v0.schema.json), [operation manifest](schemas/operations.json), [pi channel](../../../crates/st-drivers/src/pi_channel.rs), [omp channel](../../../crates/st-drivers/hooks/omp-channel.ts), [harness-context spec](../../vrs/08-harness-context/spec.md), and the implemented [terminal creation](README.md#agent-and-plain-shell-creation) and [lookup](README.md#exact-terminal-lookup) contracts.

Reuse pending [#1441](https://github.com/compoundingtech/smalltalk/pull/1441) for fenced queue/model controls, signed owner dispatch, and durable control receipts.
Its steer admission currently refuses with `native-pre-dequeue-api-unavailable`; a native steer API alone is not proof of safe admission.
Compose discovery with [#1516](https://github.com/compoundingtech/smalltalk/pull/1516), not a second native scanner.
Its inventory intentionally exposes names without descriptions or bodies; inventory does not grant invocation.

## Exact-turn composer controls (P2)

Proposed client mapping: **Enter queues durably**, **Cmd+Enter steers the exact active turn**, and **Stop cancels only that turn**.
Expose Steer and Stop only when their exact-target capabilities are supported; explain unsupported or unknown state.
Never silently turn steer into queue, queue into steer, or Stop into a runtime/seat kill.
Mission queues are out of scope.

### Shared execution and turn identity

Reuse P1's **`ExecutionKey`** in [#1912](https://github.com/compoundingtech/smalltalk/pull/1912), not a second run identifier:
`seat_id`, `owner_host_id`, `runtime_incarnation`, `native_session_id`, and `execution_id`.
`execution_id` identifies a root or child run and changes when the native session resumes as another run.
An active turn is the pair `{execution: ExecutionKey, turn_id: string}`.
The owner publishes `active_turn_id: Fact<string | null>`: known non-null identifies the active turn; known null proves no active turn; unknown proves neither.
Timeline entries, status, input dispositions, and controls refer to this same turn identity.

Turn IDs must come from an admitted native identity or a durable owner join approved with #1872.
Do not use a receipt sequence, last message, timestamp, or “working” status as a turn ID.
The owner checks execution, conversation/session binding, desired revision, support revision, and expected turn again at the native admission boundary.
Unknown identity cannot authorize steer or cancel. A changed target refuses; refresh never retargets an old request automatically.

### `TurnControl` and existing action envelopes

The following is a semantic sketch, not a new endpoint or ratified schema.
It uses `kind` for unions and snake_case fields; the existing outer action discriminator remains `type`.
`MessageSendParameters` means the existing body, including content, attachments, reply, tags, and signature fields, with the independent model/effort extensions below.
`ExecutionKey` and `Fact` are defined by P1.

```ts
type TurnControl =
  | { kind: "send"; input_id: string; execution: ExecutionKey;
      intent: "next_turn"; parameters: MessageSendParameters }
  | { kind: "send"; input_id: string; execution: ExecutionKey;
      intent: "steer"; expected_turn_id: string;
      parameters: MessageSendParameters }
  | { kind: "cancel"; control_id: string; execution: ExecutionKey;
      expected_turn_id: string }
  | { kind: "withdraw_queued"; control_id: string; execution: ExecutionKey;
      input_id: string }
```

Map Send to `message.send`, Cancel to proposed `run.cancel`, and WithdrawQueued to proposed `message.cancel-queued` through `POST /v1/client/actions`.
Retain action `id`, paired-session `idempotency_key`, snapshot/resource fences, authentication, and existing error envelopes.
For new sends, `intent: "next_turn" | "steer"` replaces the earlier proposed `delivery` field; no duplicate delivery spelling.
The negotiated intent is explicit. Unextended clients keep the implemented legacy send envelope and behavior; this proposal does not change them.
`expected_turn_id` is required for steer and cancel, forbidden for next-turn delivery, and never inferred by the gateway.
The action fence carries the complete execution key and the owner-side revisions; the union does not replace that fence.

`input_id` is the logical send's stable idempotent identity, projected as `client_message_id` on message bodies.
Proposed: use the send action's `idempotency_key`, not a second caller-selected key.
Cancel and withdrawal have their own stable action/control IDs and keys; withdrawing an input never reuses its send key for a different operation.
Persist action ID, key, full request, and exact conversation/execution binding in the client outbox before submission.
Retain paired-session action-key scope; proposed logical-send deduplication additionally binds authenticated actor, binding, and request digest across reconnect/re-pair.
Same identity and input returns the original message and outcome; conflicting reuse is `idempotency-conflict`.
Distinct IDs with identical text remain distinct.

### Durable `InputDisposition`

Project these authoritative acknowledgements from [#1441](https://github.com/compoundingtech/smalltalk/pull/1441)'s durable receipt/queue boundary.
Do not add a second queue or outcome ledger.

```ts
type InputDisposition =
  | { kind: "queued"; input_id: string; execution: ExecutionKey;
      revision: string; position: number; accepted_at: string }
  | { kind: "applied"; input_id: string; execution: ExecutionKey;
      revision: string; turn_id: string;
      intent: "next_turn" | "steer"; at: string }
  | { kind: "withdrawn"; input_id: string; execution: ExecutionKey;
      revision: string; at: string }
  | { kind: "refused"; input_id: string; execution: ExecutionKey;
      revision: string; code: string; reason: string; at: string }
```

Times are RFC 3339 UTC producer facts. `position` is the current queue position at that revision, not a delivery deadline.
Include actor/binding scope when looking up an input ID; the bare string is not globally unique and does not grant read authority.
Durable disposition revisions are ordered by the owner and projected into conversation reads, resumed streams, and bounded queue pages.
Clients must not regress a newer applied/withdrawn/refused revision when a delayed queued event arrives.
Action acceptance, graph graft creation, socket write, native storage, and generic idle status are not `applied`.
Applied requires positive consumption/admission evidence for that input and exact turn, not successful turn completion.

Next-turn Send durably publishes queued, then applied or withdrawn; a definitive rejection before application publishes refused with its reason.
Steer applies to the named active turn or is refused; it never publishes a next-turn queue disposition.
Reservation and transport progress remain in the control receipt, separate from the input disposition.
A possible native effect without settlement remains explicitly unknown/indeterminate in that receipt; do not fabricate applied, refused, or withdrawn.
Do not release, auto-withdraw, or resend such an input. A pending queue read marks it nonwithdrawable once native dispatch is reserved.
Post-acceptance loss of capability retains the last disposition and explicit uncertainty until an authoritative outcome is available.
Replay saved results before stale-fence rejection; retries must not repeat a model switch, create another queue entry, or dispatch a possible effect again.
DQ10 sets cross-pairing retention; this is not an existing indefinite deduplication guarantee or the README's legacy 30-day cleanup exception.

Reconcile optimistic rows by input identity and exact native correlation, never by text, adjacency, or queue position.
[#1856](https://github.com/compoundingtech/smalltalk/pull/1856) owns native-echo suppression and bounded torn-read recovery.
It does not persist the raw client key today; this proposal supplies that missing logical-send identity without changing timeline ordering.
Its pi-family delivery-token gap remains a prerequisite for exact correlation, not permission to guess applied.

### Queue ownership and withdrawal

st owns next-turn hold and acceptance order per destination session; do not translate it into a harness `followUp` queue.
The owner must exclude an active turn atomically at dequeue/admission; stale idle evidence alone cannot release queued input.
Separate accepted inputs retain separate IDs and dispositions even if transport notices are coalesced.
Conversation controls and bounded queue reads expose input ID, disposition revision/position, content reference, requested model/effort, and withdrawability.
Reconnect/reset recovers the same entries and outcomes.

WithdrawQueued names the input ID and exact execution/binding and removes only an undispatched queued input.
The owner serializes withdrawal against dispatch reservation.
Successful withdrawal publishes withdrawn; an applied, withdrawn, refused, or already-reserved input is not removed again.
Replay of a successful withdrawal returns its saved result. A fresh failed withdrawal returns a typed control refusal without overwriting the input's disposition.
An input is not withdrawn merely because its bubble was hidden or a cancel request was accepted.
Edit means confirmed withdrawal then a new Send with a new input ID; never send a replacement after refused/unknown withdrawal.
DQ6 retains the alternative of #1441's fenced in-place replacement.
Session/runtime replacement does not silently retarget queued input (DQ2).

### Exact-turn Cancel and native settlement

Cancel names the complete execution and expected turn and uses its own idempotent control receipt.
It requests turn-only interruption; Stop must never call `runtime.stop`, `agent.stop`, `runtime.signal`, terminal key injection, or process kill as a fallback.
Recheck the target at native admission and atomically exclude a successor.
No exact primitive means `unsupported-capability`; a settled turn means `turn-not-active`; a replaced execution means `stale-fence`.
Unknown owner/turn authority refuses dispatch and retains unknown evidence, not an inferred successful stop.
Queued next-turn inputs, independent jobs, standing seat lifecycle, and partial transcript content are preserved.

Cancel acceptance means requested, not cancelled. Only matching positive native terminal evidence settles the target as completed, cancelled, or failed.
Completion racing cancellation can remain completed; a possible abort without settlement is indeterminate and cannot authorize a fresh-key retry on a successor.
[#1872](https://github.com/compoundingtech/smalltalk/pull/1872)'s turn-debt fence is the authority boundary: idle, process exit, selected session, or owner acknowledgement does not prove the interrupted native turn ended.
Reuse its receipt join/terminal evidence and successor hold rather than inventing another obligation ledger.
OMP's hook-retained receipt is not a native turn ID; no fabricated ID grants exact-turn control.
Align outcome display with [#1478](https://github.com/compoundingtech/smalltalk/pull/1478): preserve provider abort/error/exit details and visible size continuations.
Its recognized managed-stop `cancelled` display is not by itself proof that this Cancel stopped the named turn.
Unknown tool outcomes remain unknown; cancellation never authorizes automatic tool replay or continuation.

### Per-adapter capability evidence

This matrix describes inspected source, not a live-runtime probe or an implemented P2 capability.
Native injection is distinct from admitted exact-turn steer. #1441 currently refuses steer with `native-pre-dequeue-api-unavailable`; native API availability alone does not remove that refusal.
No inspected adapter establishes the complete P2 turn-only Stop contract.

| Adapter | Native delivery today | Exact-turn Steer / Stop admission today |
| --- | --- | --- |
| Codex | `codex_delivery_request` uses `turn/start` when idle and `turn/steer` with native `expectedTurnId` when active; typed user consumption is distinct from RPC acceptance. [C1] | The native steer target exists, but P2 admission still needs #1441's fence/reservation boundary. No exact-turn cancel operation is integrated into this inspected adapter; do not infer support from an upstream API. [C1, C5] |
| pi | `pi-channel.ts` uses positive `ctx.isIdle()` and `sendUserMessage(..., {deliverAs: "steer"})` while active. [C2] | No expected-turn identity is carried to native injection and no turn-only abort is wired in this channel. Exact steer/Stop remain unsupported until the owner boundary can exclude a successor. [C2, C5] |
| omp | The extension holds/coalesces mail to a tool-batch boundary or run end, with a ten-second cap, then calls `sendUserMessage` as idle input or steer. [C3] | That hold is not an exact-turn fence; receipt support in #1872 must not be advertised as a native turn ID or cancellation API. No exact steer/Stop admission is established. [C3, C5, C6] |
| Claude | MCP `notifications/claude/channel` carries the input; `UserPromptSubmit` correlation proves promotion, not the channel write. [C4] | The notification carries no expected native turn or turn-only abort. Do not advertise exact steer/Stop from channel presence alone. [C4, C5] |
| OpenCode | `prompt_async` uses a stable `messageID`; read-back proves persisted input, not scheduler admission. [C7] | No exact-turn steer or cancel primitive is integrated; persisted input cannot prove either. [C7] |
| Terminal-only / other | The current idle-only fallback does not prove native turn admission. [C8] | Unsupported unless a separately admitted adapter proves the exact target and outcome; SIGINT/key input is not proof. [C8] |

Source references are pinned so capability claims remain reviewable:

- **C1:** [Codex adapter at the inspected main pin](https://github.com/compoundingtech/smalltalk/blob/cf8fc83c6cc075c24176467407aaa319a0c03d20/crates/st-drivers/src/codex_app_server.rs#L1962-L1981), required RPC surface at lines 42–60, and delivery receipt grading at lines 665–669.
- **C2:** [pi channel](https://github.com/compoundingtech/smalltalk/blob/cf8fc83c6cc075c24176467407aaa319a0c03d20/crates/st-drivers/hooks/pi-channel.ts#L284-L295).
- **C3:** [omp hold policy](https://github.com/compoundingtech/smalltalk/blob/cf8fc83c6cc075c24176467407aaa319a0c03d20/crates/st-drivers/hooks/omp-channel.ts#L44-L54) and [native handoff](https://github.com/compoundingtech/smalltalk/blob/cf8fc83c6cc075c24176467407aaa319a0c03d20/crates/st-drivers/hooks/omp-channel.ts#L596-L613).
- **C4:** [Claude channel/correlation](https://github.com/compoundingtech/smalltalk/blob/cf8fc83c6cc075c24176467407aaa319a0c03d20/crates/st-drivers/src/claude_mcp.rs#L127-L148).
- **C5:** [#1441 admission decision](https://github.com/compoundingtech/smalltalk/pull/1441) at inspected head `d653c22fce72c6c70896067c592137808aa44aaf`; reuse its typed queues, signed owner routing, and durable receipts.
- **C6:** [#1872 receipt authority](https://github.com/compoundingtech/smalltalk/blob/1510b25ed7c5a9218ba85ab8e026ff51602377fe/docs/st3/turn-obligation-source.md#identity-and-settlement).
- **C7:** [OpenCode receipt boundary](https://github.com/compoundingtech/smalltalk/blob/cf8fc83c6cc075c24176467407aaa319a0c03d20/crates/st-drivers/src/opencode_session.rs#L1-L19) and `send` at lines 1980–2003.
- **C8:** [current fallback boundary](#current-boundary) and [implemented runtime controls](README.md#typed-actions-and-fences), which fence runtimes rather than turns.

Runtime/version admission and current owner observations determine capabilities; this table is not a client hardcoded allowlist.
Unreachable or unobserved owners remain unknown, not unsupported or supported by inference.

## Proposed model/effort send fields

Alongside the explicit intent, the existing `message.send` body gains optional `model` and `effort`.
Model identifiers and effort values are opaque, runtime/model-specific, and policy-filtered; effort does not require model.
An effort-only input resolves the current effective model at its delivery boundary with the advertised model/revision fence.
If that model changes after admission, refuse explicitly rather than applying effort to another model.
Omitted effort requests no effort change under DQ1. An accepted action is not proof of model/effort application or input consumption.

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

## Model and effort semantics

```text
accepted message -> wait for delivery boundary -> apply requested model and/or effort
                                              -> deliver message -> report observed turn selection
```

Each send may request a model, effort, both, or neither.
The selected effort applies to that message's turn even when `model` is omitted.
Sticky versus one-shot persistence remains an open decision (DQ1), not a promised default:

| Choice | Message behavior and picker state |
| --- | --- |
| Sticky | Apply the selection before delivery and retain it for later messages in the same session. Agent/conversation rows expose current effective model and effort as separate requested/configured and observed values, with session/incarnation, revision, and observation provenance. Reloads and other devices recover that state; a picker must not rely on its last local selection. |
| One-shot | Apply the selection only to the associated message's turn, then restore the prior session selection at an authoritative completion boundary. Explicitly advertise one-shot semantics. The row exposes launch model and effort, current requested/configured and observed selection, and the next-send defaults; the effort picker defaults to launch effort, not the preceding message's override. Unknown launch effort stays unknown, not a guessed level. |

If model and effort choose different persistence policies, advertise each axis separately and apply the same row/default rules to each.
Unknown observed values use the contract's null convention, not the requested or launch value presented as observed.
An active one-shot turn must not become the default for a later send; its row/turn identity distinguishes the active override from the next-send defaults.
DQ1 must settle restoration and defaults when a one-shot model cannot honor launch effort.

Apply selection immediately before that message's delivery, not when a queued message is accepted.
A later queued choice must not affect an earlier message.
If selection fails, do not deliver under the previous model/effort as a successful selection.
Selection and delivery remain ordered within the destination session.
If selection succeeded but delivery failed, expose that partial outcome and resulting session state; do not claim restoration without evidence.
One-shot support requires a reliable completion/restoration boundary; no adapter may advertise it based only on a launch flag.

| Harness | Proposed live selection support; not an implemented guarantee |
| --- | --- |
| pi / omp | Use supported driver RPC for model and thinking changes before delivery; prove effort application and chosen persistence independently. A model RPC alone does not establish effort support. |
| codex | Use `turn/start` model/effort overrides when starting a turn; retain or scope them according to DQ1. |
| claude | Live model/effort selection is unsupported until a supported runtime surface exists; launch `--effort` alone is insufficient. |
| Other or terminal-only runtime | Unsupported unless its driver proves the requested behavior. |

Codex cannot switch an active turn's model through `turn/steer`.
A steer that cannot apply its requested model/effort to the active turn fails with `model-switch-needs-idle`; only explicit `intent: "next_turn"` permits waiting.
The client can offer queue explicitly; the server must not choose it silently.
An idle race is checked at dispatch, not only at acceptance.

The message receipt, persisted message/turn read, and conversation stream distinguish requested model and effort from observed model and effort actually used.
For effort-only sends, retain the absent requested model and expose the resolved effective model separately.
Record the inherited/default selection too, so messages without explicit effort still expose what applied.
Observed selection comes from authoritative harness-context turn evidence with turn/session/incarnation provenance, not configured launch inputs or a copied request.
It remains null until observed; absence of native effort evidence must never be presented as confirmed application.
Codex currently reports no model identifier, so its implementation must add authoritative reporting before claiming this criterion is met.

### Runtime-specific effort advertisement

Every agent/conversation row and its control updates expose effort support independently of model-switch support:

- `supported`, `unsupported(reason)`, or `unknown`, scoped to the runtime incarnation and support revision.
- The current effective model and its accepted effort values, plus per-model accepted values for advertised model choices.
- Requested/configured versus observed current selection, launch selection, and next-send defaults under DQ1.

The accepted values are the intersection of native runtime/version/model support and agent policy, not a global client enum.
Only `supported` with an advertised value permits the picker to offer it. Empty known support hides the picker; unknown disables it pending authoritative discovery.
A row must not advertise live effort support merely because its harness accepts a launch flag.
Validate again at dispatch: unknown/unsupported effort or a value outside the advertised set produces typed `effort-unsupported` (including the support reason); never coerce, clamp, substitute, or silently omit it.

| Runtime | Verified repository evidence for the vocabulary |
| --- | --- |
| codex | The app-server resume override validator explicitly accepts `minimal`, `low`, `medium`, `high`, `xhigh`. This is a verified adapter vocabulary, not proof every model or live control accepts all five. |
| claude | The driver and graph forward the string unchanged to `--effort`; neither enumerates accepted levels. Publish the installed runtime/model's verified values, not an inferred Claude enum. The driver's `xhigh` argv fixture tests forwarding, not native acceptance. |
| pi | The driver and graph forward the string unchanged to `--thinking`; neither enumerates accepted thinking levels. Discover and advertise the installed runtime/model's set; do not assume it equals omp's. |
| omp | The code likewise forwards `--thinking` verbatim without an enum. The existing omp VRS describes `off`, `minimal`, `low`, `medium`, `high`, `xhigh`, `max`, `auto`; this is documented vocabulary, not code validation or live/model support proof. Verify the installed runtime before advertising a subset. |
| opencode | No effort axis in the typed driver; graph validation rejects effort. Advertise `unsupported` with accepted values `[]`. |

Code sources: [launch forwarding](../../../crates/st-drivers/src/driver.rs) (`expand_codex`, `expand_pi`, `expand_omp`, Claude expansion), [graph forwarding and allowed children](../../../crates/st3/src/graph.rs), [typed driver fields](../../../crates/agent-spec/src/spec.rs), and [Codex override validation](../../../crates/st-drivers/src/codex_app_server.rs) (`apply_resume_config_override`).
The separate [omp vocabulary description](../../vrs/06-omp-driver/spec.md#driver-block) still uses the older `thinking` struct-field name; current code names that field `effort`.
No exhaustive Claude/pi accepted-value set is established by those code sources; native discovery/verification is an implementation prerequisite, not permission to hardcode a plausible list.

## Capability negotiation

Add optional daemon capabilities `message.turn-control` (explicit intent, durable input dispositions, and queue withdrawal), `run.cancel` (exact-turn cancellation), `message.model`, `message.effort`, `agent.commands`, and `terminal.agent-association`; final identifiers are DQ8.
These names are repository-owned client capability identifiers, not new WebSocket subprotocols.
Absence means that the client must not send the corresponding new fields or operations.
Old servers keep receiving the old envelope.

A daemon capability means the API exists; it does not mean every conversation supports it.
Per-conversation control state is `supported | unsupported(reason) | unknown` for steer, queue, model switch, effort selection, commands, queue removal, and exact-turn cancel.
A known read-only source is unsupported; an unobserved or unreachable live owner is unknown.
Missing observations never become unsupported or supported by inference from a harness name.
The agent/conversation rows and runtime reads supply the same support facts, allowed model identifiers, per-model accepted effort values, and requested versus observed selection/defaults.

Carry optional `controls` on `TimelinePage`, `ConversationChanges`, and collection `kind: "conversation"` frames, including initial empty reads/frames.
Controls contain canonical conversation/session identity, P1's `Fact<ExecutionKey>`, `active_turn_id: Fact<string | null>`, desired/support revisions, and per-control state.
Binding, activity, or support changes emit control-only updates (`items: []`) and advance the conversation cursor.
Reconnect/reset reloads controls; replacement invalidates old write targets. Catalog/model revisions invalidate cached choices.
Server validation remains authoritative; a read/frame is not authorization.
Use existing `control.messages` for send/steer/withdrawal, `control.runtimes` and #1441's concrete-person admission for model/effort/cancel, and each command handler's existing scope; DQ9 asks for confirmation, not new pairing scopes.

## Agent workbench terminals

Status: proposal. A web client needs a terminal drawer with one tab per terminal: list the agent's harness pane and associated shells, create a shell, attach using existing terminal transports, and end a shell the person created.
This is an association, not a change in terminal ownership or permission policy.

### Creation and placement

Extend `terminal.create` with optional `agent` (the canonical agent ID):

```json
{
  "name": "Workbench",
  "agent": "agent-id"
}
```

- Without `agent`, retain standalone-shell behavior.
- With `agent`, resolve its declared execution host and current worktree/workdir at the fenced snapshot. The shell runs on that host, even when the action arrives through another gateway.
- Omitted `cwd` defaults to that resolved worktree/workdir. An explicit absolute `cwd` is interpreted on the agent's host under existing admission rules; missing or unusable workdir is an explicit refusal, not fallback to the gateway's directory.
- Omitted `host` uses the agent's host; a supplied conflicting host is rejected, not treated as permission to create an associated shell elsewhere.
- Persist the canonical association and creation-time host/workdir independently of the live harness runtime. Agent/session replacement does not transfer ownership, move the shell, or silently retarget its association.
- Keep the shell person-owned, running the existing interactive shell with the existing restart policy. Its `owner_id` remains the authenticated creator, not the associated agent.

Retain the existing action snapshot fence, idempotency key, creation recovery, and durable receipt.
Creation completion means the declaration was applied, not that a PTY is ready; the client follows the terminal projection before attaching.
Host/workdir changes between admission and dispatch produce a stale-fence refusal rather than launching on a newly resolved target.

### Listing and tabs

Add `GET /v1/client/terminals?agent=agent-id`, distinct from the existing exact `owner` filter.
At the selected snapshot, it returns the agent's harness pane plus shells carrying that association, filtered by the caller's existing terminal read grants.
`owner` continues to mean actual owner; combined `agent`, `owner`, and `state` filters use AND. Filtering happens before the page limit and binds/echoes the filter in cursors, following the current terminal lookup pattern.
`history=true` retains its existing meaning for ended/stopped entries; association alone does not include all history.

Each returned terminal projects stable terminal ID, actual owner, associated agent ID, role (`harness` or `shell`), host, lifecycle state, and authorized actions.
The harness pane and shells are separate tabs; labels and membership come from the projection, never ID parsing or a client-local list.
Use existing screen/stream/raw-attach, input, and resize APIs with their existing scopes and incarnation/sequence fences.
Closing a creator-owned shell tab may explicitly invoke `terminal.end`; hiding the drawer or detaching a tab never ends a process.
Do not offer `terminal.end` on the harness pane or another person's shell; removing the harness remains a separate runtime operation.

### Authorization and lifecycle

Associated-shell creation requires `terminal.control`, an authenticated concrete person, and permission to see the associated agent.
The request cannot select another creator. Reading the agent does not itself grant access to its host or any terminal: existing host/terminal admission and read/control grants still apply.
Recheck authorization at admission/dispatch; association never broadens screen, input, or resize privileges.
Listing needs the existing projection-read scope plus agent visibility and individual terminal visibility.
Ending requires `terminal.control` and that same creator, including after the associated agent disappears; association does not grant another person end authority.

Stopping or removing an agent does not silently kill associated shells. They remain person-owned and may continue running until shell exit or explicit creator end.
Keep the recorded association visible on authorized terminal reads, even after removal, with an explicit stopped/removed agent reference rather than silently clearing or reassigning it.
Do not retain privileged agent details beyond the viewer's grants. DQ12 settles tombstone visibility and retention; loss of agent visibility must not strand a creator who can still manage their own shell.
Association is not a worktree lease: removal may affect the underlying directory. Expose that outcome rather than promising automatic directory preservation.

Cross-host actions use existing authenticated owner-host routing; success is not inferred from local gateway acceptance.
If the agent's host is unreachable, report the existing unavailable/indeterminate outcome, never create a replacement locally.
A later agent host/workdir change leaves existing shells at their recorded creation placement; new shells use the newly admitted placement.
DQ11–DQ13 cover placement, retained association visibility, and action/filter advertisement.

## Typed refusals

| Code | Meaning |
| --- | --- |
| `unsupported-capability` / `native-pre-dequeue-api-unavailable` | Identify the feature and precise adapter admission refusal. |
| `stale-fence` | Binding, incarnation, turn, catalog, or model revision changed; refresh without automatic retargeting. |
| `idempotency-conflict` | Action key or client-message identity reused with conflicting input. |
| `turn-not-active` | Exact steer/cancel target settled; never operate on its successor. |
| `model-switch-needs-idle` | Active turn cannot change model; queue requires explicit selection. |
| `model-unavailable` / `effort-unsupported` | Requested selection cannot be honored, including effort without model; retain the specific native/policy reason and never coerce. |
| `command-not-found` / `command-unavailable` | Unknown registry name versus registered but noninvocable command. |
| `remote-unavailable` / `runtime-authority-indeterminate` | Owner is unknown; inspect reserved receipt before retrying. |
| `native-outcome-indeterminate` | Possible native effect without settlement; no automatic fresh-key retry. |
| `queue-entry-not-pending` | Queue removal raced dispatch; do not cancel a turn or re-send automatically. |
| `terminal-agent-unavailable` / `terminal-agent-workdir-unavailable` | Proposed associated-shell refusal when placement cannot be authoritatively resolved; no local or directory fallback. |
| `terminal-agent-host-mismatch` | Explicit host conflicts with the associated agent's host. |
| `terminal-owner-forbidden` | Association does not override creator-only shell ending. |

## Alternatives

| Alternative | Reason not selected |
| --- | --- |
| Harness-owned follow-up queues | Pending work would be hidden from st and inconsistent across drivers. |
| Slash text interpreted on send | Ambiguous with ordinary text; no typed arguments or discoverable availability. |
| Client hardcoded command mapping | Clients diverge as actions and harness support change. |
| Restart for each model choice | Changes session lifecycle instead of switching the current session. |
| Treating message association as necessarily sticky | Conflates which message receives the setting with its lifetime; both persistence choices remain DQ1. |
| Daemon-only feature flags | Cannot express unsupported individual runtimes. |
| Reusing `owner=agent` for associated shells | Would misrepresent person ownership and change the existing exact-owner filter. |
| Agent-owned workbench shells | Agent removal could accidentally terminate a person's independent work. |

## Implementation ownership and PR split

| PR | Owner boundary | Deliverable |
| --- | --- | --- |
| PR1 | Client schema, daemon delivery, drivers | Reuse P1/#1912's ExecutionKey, #1441's queue/receipt boundary, and #1872's turn-debt authority for TurnControl, InputDisposition, controls frames, raw logical input identity, and exact-turn admission. Compose #1856 correlation and #1478 outcomes; addresses #277. No mission queue changes. |
| PR2 | Client schema and command dispatch | Reuse #1516 inventory; catalog read, `command.invoke`, phase 1 st commands; `/model` waits for PR3 support. |
| PR3 | Runtime model/effort control and drivers | Independent model/effort send fields, shared selection action, runtime/model-specific accepted values, row/default state, and requested/observed message and turn reporting. |
| PR4 | Terminal declaration, projection, client schema | Optional agent association, owner-host/workdir placement, list filter and terminal metadata; preserve creator ownership, authorization, and lifecycle. |

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
- Enter durably queues; Cmd+Enter explicitly steers only a supported exact turn. Unsupported steer never queues, and unsupported Stop never signals/kills the seat.
- Queued → applied/withdrawn/refused dispositions survive restart/reconnect and same-key replay; delayed queued revisions cannot regress final state. Distinct same-text inputs remain distinct.
- Dispatch reservation racing withdrawal yields exactly one outcome: withdrawn without native effect, or refused withdrawal with the input's existing disposition unchanged.
- Missing native consumption/correlation remains unknown; native storage, cancel acceptance, idle, process exit, and #1872 owner acknowledgement cannot manufacture application or terminal settlement.
- An old execution key or turn ID cannot stop/steer a resumed execution or successor turn. Cancel preserves queued work, unrelated jobs, partial content, and the standing seat.
- Inventory-only names remain noninvocable; stale catalog revisions refuse; a successful model switch followed by failed delivery has an explicit partial receipt.
- Effort-only sends apply to the current fenced model; unsupported values refuse without coercion. Each runtime/model row advertises only verified accepted values, including empty opencode support.
- Explicit and inherited message selections expose requested/resolved versus observed model/effort; unknown evidence stays null. Reload and another device recover sticky effective state, or one-shot launch defaults and completion/restoration state.
- Associated shells launch on the agent's host/workdir through another gateway; conflicting host, missing workdir, stale placement, and lost agent visibility refuse without a local fallback.
- Agent filtering returns authorized harness and associated-shell tabs while exact-owner filtering remains unchanged; stale filtered cursors refuse.
- Agent stop/removal leaves person-owned shells intact with visible association; only the creator can end them, including after removal. Detach/hide does not terminate.

## Open questions for Nathan

- **DQ1 Sticky or one-shot model/effort selection and settlement?** Choose persistence per axis. Sticky requires row-level current requested/configured and observed model/effort across reload/devices. One-shot must explicitly advertise that lifetime, expose launch effort as the picker default, distinguish active overrides from next-send defaults, and prove completion/restoration (including one-shot model versus launch-effort compatibility). Settle selection/input atomicity and successful-selection/failed-delivery receipts.
- **DQ2 Queue lifecycle across session replacement?** Decide whether a pending message remains bound to its original session/runtime or can be explicitly retargeted. Existing fences must prevent accidental delivery to a replacement.
- **DQ3 Harness command collisions and argument schema?** Choose qualified names and supported keywords; do not allow shadowing or make #1516 inventory names invocable without admitted invocation contracts.
- **DQ4 Model and effort policy authority?** Confirm policy-filtered runtime/version/model discovery as the accepted-value authority. How do Claude/pi/omp supply a verified vocabulary and authoritative applied-effort observations? Launch forwarding and documentation alone are not live support proof.
- **DQ5 Exact-turn cancel and queue preservation?** Proposed: require complete ExecutionKey plus expected turn and positive native settlement; preserve queued inputs, independent jobs, and standing seat lifecycle. Approve composing #1872 receipt/debt fences and #1478 native outcomes, with no SIGINT/seat-kill fallback?
- **DQ6 Queue edit semantics?** Proposed: confirmed WithdrawQueued then Send with a new input ID. Prefer fenced in-place editing using #1441's replace operation?
- **DQ7 Reuse and admission prerequisites?** Confirm #1441/#1516 as prerequisites. Which native turn/admission proof permits supported steer instead of `native-pre-dequeue-api-unavailable` for each adapter? Approve Enter=next_turn and Cmd+Enter=steer with no silent downgrade?
- **DQ8 Control schema and capability advertisement?** Approve the kind-tagged TurnControl/InputDisposition sketch, reuse of P1/#1912 ExecutionKey and active-turn fact, and the queued/applied/withdrawn/refused vocabulary over #1441 receipts? Choose final action/capability identifiers and canonical turn-ID authority; require per-conversation supported state before exposing Steer/Stop.
- **DQ9 Permission and actor admission?** Retain existing message/runtime scopes, concrete-person model/effort/cancel admission, queue-withdrawal authority, and each command handler's scope without adding pairing scopes?
- **DQ10 Durable identity across re-pair?** Approve input_id = send idempotency key projected as client_message_id, actor/binding-scoped deduplication for at least 30 days, paired-session action-key scope, and #1441's durable receipts with explicit indeterminate outcomes? Set disposition revision/retention and bounded read policy; #1856 does not yet persist the raw key or decide timeline ordering.
- **DQ11 Terminal placement and workdir policy?** Approve agent-host placement, worktree/workdir default, explicit same-host cwd, conflicting-host refusal, and fenced creation-time placement? What is the canonical workdir when an agent has no worktree, is stopped, or its directory has been removed?
- **DQ12 Association retention and access after removal?** Choose the durable declaration/projection representation, tombstone retention, and authorized removed-agent filter behavior. Preserve association and creator management without leaking agent details or silently killing shells; decide whether any explicit worktree-retention policy is needed.
- **DQ13 Terminal association advertisement and filter contract?** Approve `terminal.agent-association` gating both the create field and `agent` list filter, filter acknowledgment/cursor binding, terminal role metadata, and existing terminal scopes plus concrete-person/agent-visibility admission?
