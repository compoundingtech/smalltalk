# St3Client

Swift 6 package for iOS 17/macOS 14 clients of `st3.client.v0`. It is an online thin client over an
authenticated Fabric-loopback URL and includes typed capabilities, resources, timeline/events,
actions, pairing, and styled terminal screens; `terminalStream` yields each changed screen.

```swift
let client = St3Client(fabricLoopbackURL: gatewayURL, credential: credential)
let capabilities = try await client.capabilities()
let work = try await client.workList(limit: 100)
```

After a `cursor-gap`, discard projection caches, fetch fresh first pages, and resume from the new
capabilities `eventCursor`. Never queue mutations offline. Construct actions with the snapshot and
resource fences the user actually viewed. App code uses generated named read methods and typed
action methods; collection strings, raw paths, and untyped action dictionaries are private.

## Trace context

An iOS app can connect its tracing adapter to the client with an optional synchronous,
`@Sendable` callback. Have the adapter return the **currently active span's** W3C
`traceparent` and optional `tracestate`, or `nil` when no span is active. For example,
accept the app's active-span lookup when constructing the shared client:

```swift
func makeClient(
    gatewayURL: URL,
    credential: String,
    activeSpanContext: @escaping @Sendable () -> TraceContext?
) -> St3Client {
    St3Client(
        fabricLoopbackURL: gatewayURL,
        credential: credential,
        traceContext: activeSpanContext
    )
}
```

The callback runs synchronously on the client actor, once for every HTTP request and
every WebSocket open (conversation, glasses, arrangements, and terminal streams), not
once per client or per stream message. Its captured tracing adapter must be safe to
access there, rather than requiring the main actor. The package has no tracing-library
dependency and does not create spans itself.

No callback, or a `nil` result, sends neither trace header. `TraceContext` is a public
`Sendable`, `Equatable` value with `traceparent: String` and `tracestate: String?`.
Before sending it, the client requires exactly the lowercase W3C shape
`^[0-9a-f]{2}-[0-9a-f]{32}-[0-9a-f]{16}-[0-9a-f]{2}$`, rejects version `ff`, and rejects
all-zero trace or span IDs. An invalid `traceparent` suppresses **both** headers.
For a valid parent, `tracestate` is forwarded verbatim, including existing `st`
entries: the SDK never adds or rewrites `st=c`. A daemon with trace propagation
records `st.parent.sampled` for a sampled incoming parent without an `st` tracestate
entry, so the collector can honour external app sampling decisions. Daemons without
trace propagation ignore both headers.

## Generated artifacts

The generated models and complete typed operation surfaces are refreshed from the normative schema
and machine manifest with `cargo run -p st3-client-codegen`; `--check` renders every artifact in
memory and byte-compares it to the checked-in output.
