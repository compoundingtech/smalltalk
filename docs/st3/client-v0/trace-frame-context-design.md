# Trace context on subscribe and command frames

Status: proposed. Review and decision: Nathan. Agreed proposal from the fractal-web client
owner and the st3 OpenTelemetry owner; no implementation changes in this document.

This extends the [client v0 contract](README.md) and [collections protocol](collections.md).
The existing schemas and fixtures remain normative until an implementation changes them.

## Problem and current boundary

A client_v0 WebSocket is upgraded once per page. Its upgrade `traceparent` cannot relate
later per-subscription work to the user action that caused it: an agent switch → roster
build, or a conversation first-page request → first frame sent. Treating the page-long
connection as the parent of every operation loses those action boundaries.

HTTP already carries W3C trace context: [#1811](https://github.com/compoundingtech/smalltalk/pull/1811)
adds SDK `ClientOptions.traceContext`, while [#1622](https://github.com/compoundingtech/smalltalk/pull/1622)
provides daemon SERVER spans. This proposal carries the same context at the frame boundary,
not just at stream open. The telemetry foundation is
[#1607](https://github.com/compoundingtech/smalltalk/pull/1607).

### Decoder compatibility evidence

The collections socket decodes text with `serde_json::from_str::<CollectionSubscribe>` in
[`crates/st3/src/api/client_v0.rs`](../../../crates/st3/src/api/client_v0.rs#L954-L957).
[`CollectionSubscribe`](../../../crates/st3/src/api/client_v0.rs#L30-L49) derives
`Deserialize` without `deny_unknown_fields`: Serde ignores unknown object fields. Adding
`trace` therefore does not make current subscribe/unsubscribe frames fail decoding; older
daemons using this decoder ignore the field and preserve existing behaviour.

This is not a blanket claim about every client_v0 request: HTTP
[`ActionRequest`](../../../crates/st3/src/api/client_v0.rs#L7902-L7904) explicitly uses
`deny_unknown_fields`. Adding `trace` to that body would be rejected by older daemons.
Command-frame coverage is a decision in DQ2; this proposal does not add a field to strict
HTTP action bodies or claim compatibility for a separate, unverified decoder.

## Proposed wire contract

Add an optional field to subscribe and command frames, reusing the `TraceContext` shape
from #1811:

```typescript
type TraceContext = { traceparent: string; tracestate?: string };
// Optional field on the existing frame:
trace?: TraceContext;
```

An example subscribe frame is:

```json
{
  "kind": "subscribe",
  "id": "roster",
  "collection": "agents",
  "trace": {
    "traceparent": "00-4bf92f3577b34da6a3ce929d0e0e4736-00f067aa0ba902b7-01",
    "tracestate": "vendor=value"
  }
}
```

The lowercase `trace` key is repository-local to the client_v0 frame contract; Nathan owns
its final naming decision (DQ1). Its members are the W3C `traceparent` and `tracestate`
values, not a second trace identifier format. The field is additive and optional: no field
means existing client behaviour, and older collection decoders ignore it. It grants no
actor, scope, or other authority.

An invalid W3C `traceparent` means the entire `trace` field is ignored, not that the frame
is rejected. For example, `{"traceparent":"not-a-traceparent"}` is treated as absent.
Validation uses W3C Trace Context parsing, including nonzero trace/span IDs and the version
and flags rules; it is not merely a length check.

## Daemon span relationships

```text
user action span
  └─ frame SERVER span: subscribe received → roster build / first page → first frame sent

later subscription frame span ── link ──► original frame context
subscribe without trace      ── link ──► upgrade span
```

- **R01 Initial work:** The subscribe's own work, from receipt through roster build or
  conversation first page to the first frame sent, is a child of the valid frame context.
  It has a bounded lifetime; it does not remain open for the whole subscription.
- **R02 Later work:** Later frames for that subscription have a span link to the subscribe
  frame context, not that context as their parent. A long-lived subscription must not turn
  unrelated later work into children of an old user action. Resync's boundary is DQ3.
- **R03 Missing context:** A subscribe without `trace`, including one with an invalid
  `traceparent`, links to the upgrade span rather than claiming a per-action parent.
- **R04 Command work:** If commands are included in v0 (DQ2), their bounded execution work
  is a child of their valid frame context. This does not change authorization or command
  semantics.

## Attributes and service identity

| Signal | Proposed contract |
| --- | --- |
| Span display | `span.label` provides the human-readable operation label. |
| Span dimensions | Collection and cold/incremental mode are span attributes. |
| Span detail only | Subscription id, `request_id`, row/card counts, and bytes are span attributes only, never metric attributes. |
| Metric dimensions | Metric attributes remain closed enums; no IDs, counts, byte values, or free-form labels become metric dimensions. |
| Browser resource | `service.name = webfractal-web` |
| App-server resource | `service.name = webfractal-server` |
| Daemon resource | `service.name = st-daemon`; daemon tail sampling matches `st-*`. |
| Build resource | `service.version` is the build identity for each service. |

## Sampling

A sampled browser parent sets `st.parent.sampled=true` on st's SERVER span. The browser's
sampling decision controls end-to-end retention, rather than the daemon independently
losing the work caused by a sampled action. A link alone is not a parent sampling decision:
R02 preserves correlation without reparenting all later subscription work to that action.

## Proposed acceptance scenarios (not executed)

- A sampled agent-switch action and its subscribe → roster → first-frame work share a
  trace through parent/child relationships, with `st.parent.sampled=true` on the SERVER span.
- A conversation first-page action parents the initial page and frame work; subsequent
  subscription frames link to its context instead of extending that parent lifetime.
- Missing or invalid frame context leaves the operation functional and links the subscribe
  to the upgrade span. A current older decoder accepts the example frame and ignores `trace`.
- Span resources distinguish browser, app server, and daemon builds. Counts, bytes, and
  correlation IDs appear only on spans; metric attribute values remain closed enums.

## Questions for Nathan

- **DQ1 Field name:** Approve `trace`, or select another wire key before schema/SDK changes.
  Resolution fixes one name shared by the daemon and generated clients.
- **DQ2 Command scope:** Do command frames need context in v0, or only subscribe frames?
  Resolution identifies the exact command frame variants and their decoders; strict HTTP
  action bodies remain outside this additive-frame compatibility claim.
- **DQ3 Resync relationship:** Is resync later subscription work linked to the original
  context, or new bounded work that should be a child of a fresh triggering context?
  Resolution specifies the trigger/context and when the initial-work boundary restarts.
