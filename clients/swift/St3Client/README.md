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

Native identities use `subjectGet`, `subjectsList`, `subjectClaims`, `subjectHistory`, and
`subjectSchemas`, separately from operational resources and applied agent definitions.
`subscribeSubjects` selects either one native ref or a bounded family window on the existing
collections socket. Native claim fields preserve absent, null, and present values through
`ProjectedField`; retained history includes answering-host local observations, not fleet-wide
lifetime history.

Generated concrete models validate native references, claim kinds, and content-addressed
descriptors during decoding. Unknown subject or claim descriptors produce the payload-free
`.unsupported` case in the native unions; malformed known payloads fail decoding. Snapshot and
change frames validate their outer headers before decoding each row through those unions.

The generated models and complete typed operation surfaces are refreshed from the normative schema
and machine manifest with `cargo run -p st3-client-codegen`; `--check` renders every artifact in
memory and byte-compares it to the checked-in output.
