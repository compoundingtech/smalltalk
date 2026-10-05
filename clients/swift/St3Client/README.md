# St3Client

Swift 6 package for iOS 17/macOS 14 clients of `st3.client.v0`. It is an online thin client over an
authenticated Fabric-loopback URL and includes typed capabilities, resources, timeline/events,
actions, pairing, styled terminal screens and paged retained terminal history;
`terminalStream` yields each changed screen.

```swift
let client = St3Client(fabricLoopbackURL: gatewayURL, credential: credential)
let capabilities = try await client.capabilities()
let work = try await client.workList(limit: 100)
```

After a `cursor-gap`, discard projection caches, fetch fresh first pages, and resume from the new
capabilities `eventCursor`. Never queue mutations offline. Construct actions with the snapshot and
resource fences the user actually viewed. App code uses generated named read methods and typed
action methods; collection strings, raw paths, and untyped action dictionaries are private.

Use `terminalHistory(_:runtimeIncarnation:before:limit:)` with the terminal's canonical subject ID
and the incarnation from its runtime detail. It requires `terminal.read` and returns styled rows
retained in the owner's main-buffer memory, excluding the live viewport. Rows within a page are
oldest-first; pass `nextBefore` unchanged to read the preceding page. Retention is not archival:
an expired or discarded boundary returns `history-cursor-gap`, and alternate-screen reads return
`history-alternate-screen`. Restart pagination after a gap; do not reuse cursors across incarnations.

The generated models and complete typed operation surfaces are refreshed from the normative schema
and machine manifest with `cargo run -p st3-client-codegen`; `--check` renders every artifact in
memory and byte-compares it to the checked-in output.
