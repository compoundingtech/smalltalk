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

Sources: [schema](schemas/client-v0.schema.json), [operation manifest](schemas/operations.json), [pi channel](../../../crates/st-drivers/src/pi_channel.rs), [omp channel](../../../hooks/omp-channel.ts), and [harness-context spec](../../vrs/08-harness-context/spec.md).

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

Existing send fields and action fencing remain unchanged.
Model identifiers and effort values are opaque to clients, not parsed from labels.
The server validates the requested combination against the current runtime.
An accepted action is not proof of harness consumption.
Existing idempotency rules apply to the full request, including these fields.
Replaying an accepted send must not repeat a model switch or enqueue a second message.

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

The conversation stream exposes an accepted queued message as pending, including its requested delivery mode.
It then exposes the existing delivery/consumption outcome without calling acceptance delivery.
A resumed stream or conversation read must recover the same pending state.
A runtime becoming unavailable must not silently turn queue into steer.
The existing message lifetime and failure policy still applies; this proposal adds no hidden expiry or retry policy.

Unsupported combinations fail with a typed action error, not a downgrade.
Reject before acceptance if the runtime cannot satisfy the mode.
If support disappears after acceptance, retain an observable pending or failed outcome under the existing delivery lifecycle.
The implementation must define the wire shape for this outcome in the schema and fixtures.

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

The agent/runtime read also advertises `runtime.supports`, including `steer`, `queue`, and `model_switch`.
A daemon capability means the API exists; it does not mean every runtime supports it.
Missing support is Unsupported, not optimistic availability inferred from the harness name.
Runtime changes invalidate this projection; server validation remains authoritative.
The runtime read supplies allowed model identifiers and their effort values alongside `model_switch` support.
This gives the picker one source independent of whether the commands catalog is enabled.

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
| PR1 | Client schema, daemon delivery, drivers | Queue capability, runtime support projection, send delivery field, st-owned pending queue and stream outcomes; addresses #277. |
| PR2 | Client schema and command dispatch | Catalog read, `command.invoke`, phase 1 st commands; `/model` waits for PR3 support. |
| PR3 | Runtime model control and drivers | Model/effort send fields, shared model-switch action, allowed-model read, observed receipt and turn reporting per harness. |

Each PR updates schema, operations, normative fixtures, and the client README.
Each regenerates Rust, Swift, and TypeScript clients, including Effect Schema output, with `st3-client-codegen`.
Generated TypeScript must expose the fields, reads, actions, capability names, and typed errors without an untyped escape hatch.
Implementation verification includes generator freshness and the TypeScript client gate.
A downstream web client can build against those schemas and retain Unsupported states until capabilities are advertised.
No downstream client implementation belongs in this design PR.

## Open questions for Nathan

- **DQ1 Sticky or one-shot model selection?** Proposed: sticky. One-shot needs a specified restoration boundary and behavior when a turn fails or is interrupted.
- **DQ2 Queue lifecycle across session replacement?** Decide whether a pending message remains bound to its original session/runtime or can be explicitly retargeted. Existing fences must prevent accidental delivery to a replacement.
- **DQ3 Harness command collisions and argument schema?** Before phase 2, choose stable qualified names and the exact supported argument keywords; do not let a harness shadow an st command.
- **DQ4 Model and effort policy authority?** Confirm whether the allowed-model read reflects driver discovery, agent policy, or their intersection. Proposed: policy-filtered driver support; unsupported effort combinations fail explicitly.
