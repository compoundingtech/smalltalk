# Client v0 composer and workbench proposal

Status: proposed. Review: Nathan. No implementation changes in this document.

This proposal extends the [client contract](README.md) for a web client's composer and terminal drawer.
It addresses sender-selected delivery in [#277](https://github.com/compoundingtech/smalltalk/issues/277), command discovery, message-associated model/effort selection, and agent-associated workbench terminals.
The schema and fixtures remain normative until implementation PRs change them.

## Scope and terms

| Term | Meaning |
| --- | --- |
| Steer | Deliver through the harness's supported in-turn injection boundary. |
| Queue | Hold the message in st until the harness is next observed idle. |
| Pending | Accepted by st, but not yet consumed by the harness. |
| Model/effort selection | Apply a requested model and/or effort before delivering the associated message; persistence is DQ1. |
| Command | A server-discovered operation invoked by name with typed arguments. |
| Associated terminal | A person-owned shell linked to an agent, distinct from the agent's harness pane. |

The composer uses `POST /v1/client/actions`, not a privileged message endpoint.
The conversation WebSocket remains read-only.
This proposal does not define layout, terminal automation, arbitrary harness command execution, a permission picker, or shelves.
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
| `effort` | A runtime-advertised value for `model` when supplied, otherwise for the current effective model. Does not require `model`. |

Existing send fields remain unchanged; the action gateway carries the additional control fences described below.
Model identifiers and effort values are opaque to clients, not parsed from labels or translated between runtimes.
The server validates the requested combination against this agent's runtime, policy, model, and support revision.
An effort-only send changes no model. Resolve the current model at the message's delivery boundary, with the advertised model/revision fence; if it changed since admission, refuse explicitly rather than silently applying effort to another model.
An omitted effort requests no effort change; it uses the effective default under the persistence choice in DQ1.
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
A send that cannot apply its requested model/effort to an active turn fails with `model-switch-needs-idle` unless `delivery: "queue"` permits waiting.
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

Add optional daemon capabilities `message.delivery.queue`, `message.model`, `message.effort`, `agent.commands`, and `terminal.agent-association`.
These names are repository-owned client capability identifiers, not new WebSocket subprotocols.
Absence means that the client must not send the corresponding new fields or operations.
Old servers keep receiving the old envelope.

A daemon capability means the API exists; it does not mean every conversation supports it.
Per-conversation control state is `supported | unsupported(reason) | unknown` for steer, queue, model switch, effort selection, commands, queue removal, and exact-turn cancel.
A known read-only source is unsupported; an unobserved or unreachable live owner is unknown.
Missing observations never become unsupported or supported by inference from a harness name.
The agent/conversation rows and runtime reads supply the same support facts, allowed model identifiers, per-model accepted effort values, and requested versus observed selection/defaults.

Carry optional `controls` on `TimelinePage`, `ConversationChanges`, and collection `kind: "conversation"` frames, including initial empty reads/frames.
Controls contain canonical conversation/session identity, runtime incarnation, revision, and per-control state.
Binding, activity, or support changes emit control-only updates (`items: []`) and advance the conversation cursor.
Reconnect/reset reloads controls; replacement invalidates old write targets. Catalog/model revisions invalidate cached choices.
Server validation remains authoritative; a read/frame is not authorization.
Use existing `control.messages` for send/steer, `control.runtimes` and #1441's concrete-person admission for model/effort/cancel, and each command handler's existing scope; DQ9 asks for confirmation, not new pairing scopes.

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
| PR1 | Client schema, daemon delivery, drivers | Reuse #1441 for queue capability, controls frames, message identity, send delivery field, pending queue/removal and receipts; addresses #277. Exact-turn cancel requires DQ5 admission. |
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
- **DQ5 Exact-turn cancel and queue preservation?** Require turn-only cancellation preserving queued input and independent jobs? Proposed: yes; adapters that cannot exclude successors or broader effects are unsupported.
- **DQ6 Queue edit semantics?** Proposed: confirmed cancel plus re-send with a new ID. Prefer fenced in-place editing using #1441's replace operation?
- **DQ7 Reuse and admission prerequisites?** Confirm #1441/#1516 as prerequisites. What native turn/admission proof permits supported steer instead of `native-pre-dequeue-api-unavailable`?
- **DQ8 Cancel capability advertisement?** Confirm the action capability and per-conversation state required before exposing `run.cancel`; do not substitute SIGINT support.
- **DQ9 Permission and actor admission?** Retain existing message/runtime scopes, concrete-person model/effort/cancel admission, and each command handler's scope without adding pairing scopes?
- **DQ10 Durable identity across re-pair?** Approve actor-scoped deduplication for at least 30 days while retaining paired-session action-key scope, using #1441's receipts and explicit indeterminate outcomes?
- **DQ11 Terminal placement and workdir policy?** Approve agent-host placement, worktree/workdir default, explicit same-host cwd, conflicting-host refusal, and fenced creation-time placement? What is the canonical workdir when an agent has no worktree, is stopped, or its directory has been removed?
- **DQ12 Association retention and access after removal?** Choose the durable declaration/projection representation, tombstone retention, and authorized removed-agent filter behavior. Preserve association and creator management without leaking agent details or silently killing shells; decide whether any explicit worktree-retention policy is needed.
- **DQ13 Terminal association advertisement and filter contract?** Approve `terminal.agent-association` gating both the create field and `agent` list filter, filter acknowledgment/cursor binding, terminal role metadata, and existing terminal scopes plus concrete-person/agent-visibility admission?
