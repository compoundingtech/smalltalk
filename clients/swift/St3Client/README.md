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

Use `ArrangementEditParameters(version: 2)` when creating a new folder-only arrangement
(with the other required arguments); omitted version retains v1 creation.
`ArrangementBody` distinguishes `.v1` and `.v2` and rejects unsupported versions.
`arrangementMembershipEdit` accepts typed place/remove operations with explicit nullable
root buckets. `arrangementsMemberships` returns a snapshot-bound typed page; pass the
route's person name/UUID and owning `person/NAME`. `orderedMembershipsStream` requires
that person and arrangement subject and follows lifecycle-aware held-window changes.
Its `snapshot` and `changes` frames require `membership: OrderedMembershipState` for the whole
container, which moves on edits outside the window too; `changedIndex` is an opaque host-local
invalidation frontier scoped by `snapshot.hostID`, not a canonical revision or cross-host value.
Existing v1 placements are not implicitly migrated.

The generated models and complete typed operation surfaces are refreshed from the normative schema
and machine manifest with `cargo run -p st3-client-codegen`; `--check` renders every artifact in
memory and byte-compares it to the checked-in output.
