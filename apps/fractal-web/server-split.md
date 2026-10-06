# Server and sidebar persistence boundary

## Server

`server/core.mts` supplies an HTTP server and embeddable middleware. The caller must inject the listener host/port, asset root, build identity, paired-gateway Unix socket and Host header, authorization, absolute upstream deadline, admission hook, and Effect tracer. There are no owner, machine, carrier, folder, credential-file, environment-variable, or deployment defaults. The gateway socket must be the authenticated paired-only gateway, never the trusted-local API socket.

The admission hook owns authentication, exact Host/Origin checks, and CSRF policy for the deployment; a missing policy is not an anonymous-access default. Refusals and hook failures stay outside tracing. The tracer/exporter is injected and owned by its caller; export transport is not exposed as an ingestion route. Request/gateway/static spans propagate W3C trace context, use bounded route/method attributes, and never record credentials, bodies, query strings, or user labels.

Only `/v1/client` HTTP and WebSocket requests reach the gateway. Other API, legacy-private, and telemetry-ingestion routes are refused. Bodies use streaming backpressure. Downstream disconnects interrupt upstream work. A server scope owns successful WebSocket tunnels and closes them at shutdown. Static serving retains SPA fallback, GET/HEAD, weak conditional ETags, Brotli/gzip build-time siblings and cache identity; paths and resolved symlinks must stay inside the immutable asset root. Mutable/untrusted asset directories are not supported.

## Arrangements client

The existing [Person arrangements contract](../../docs/st3/client-v0/README.md#person-arrangements) is authoritative. `src/folders/client.ts` uses the generated SDK's `arrangementsGet`, `arrangementsList`, `arrangementEdit`, and selected-subject collections subscription. It discovers granted capability version 1 before reading or editing, takes an explicit owner/UUIDv7 subject, and preserves caller action IDs, idempotency keys, and snapshot fences. Collection snapshots are authoritative; upserts and removals replace the selected resource. There is no privileged folder backend, invented endpoint, local graph replica, or hidden offline queue.

Ordinary folder operations touch only their named registers. A rename and move can coexist; the client never PUTs a replacement document. Arrangement admission order, not the historical sidebar HLC, decides new winners. Existing pairings lacking arrangements grants must be renewed before migration starts.

## One-time migration

Legacy custom sidebar claims are authoritative user-created state. `migration/legacy.ts` is the only historical wire/reducer boundary; it folds all immutable claim pages with the existing HLC/content tie-breaks and permanent remove-wins tombstones. It does not stop pagination merely because a page leaves the fold unchanged. The private host provides the actual trusted-local read transport; that transport is not part of this application.

Migration ordering is mandatory:

1. Discover arrangements and validate the explicit target ownership.
2. Obtain a durable **all-writer fence** from the host. Stop/drain legacy browser and native writers, including retained pending edits and in-flight accepted requests, at every source site. The fence must remain asserted across failure and process restart. Quiet polls, a stable cursor, or a local Web Lock alone do not establish this fence.
3. Read/fold the fenced source once and assert the fence again. Preserve stable folder IDs and all placements/tombstones. Resolve the legacy displayed parent tree, including cycles, before local cycle-refusing admission. Re-derive canonical fractional keys deterministically from legacy `(key, ID/subject)` order. A deleted folder has no surviving legacy name; its required, invisible target name is `Deleted folder`. The full old document/stamps remain in the staged provenance.
4. Durably stage the source snapshot/index, fence receipt, target ID, exact atomic operations, action identity, idempotency key and snapshot fence **before** sending anything. `migration/journal.ts` uses strict-durability IndexedDB and a Web Lock spanning the whole operation. The host owns retention/export of this sensitive local provenance; it is never telemetry or an upload artifact.
5. Submit one atomic `create` plus the planned folders/placements/tombstones. Keep the same paired-session identity and retry the exact staged request after failure. An accepted-but-lost response replays the saved receipt; it must not issue new operations. A different pre-existing target/create race is a typed refusal, never permission to overwrite or fold arbitrary live state.
6. Persist the matching completed receipt and independently read the target. Do not require equality with the import revision/body: legitimate edits after atomic creation may already be winners. A failed or unreadable target never activates.
7. Persist `readable`, perform an idempotent durable pointer cutover that routes **all** writers exclusively to arrangements, then persist `complete`. A crash after pointer cutover repeats only that pointer operation. Legacy history is retained; legacy writers remain fenced. Never dual-write or silently fall back to legacy reads.

The host supplies `LegacySource` and the durable pointer activation operation. This module does not pretend that a browser lock can fence a native application or a different device. Connecting those host seams and applying live migration are cutover operations, not consequences of installing this code.

### Bounded refusal, not partial migration

One arrangement edit admits at most 1,024 operations. Import cost is `1 + folders + placements + tombstoned folders`. The planner refuses over-limit input instead of creating a partially imported, concurrently editable target. Historical partial live folders, invalid UUIDv7 IDs, or names that cannot be admitted unchanged are also explicit refusals; source history remains untouched. Target cumulative resource quotas and graph-reference validation remain authoritative server refusals. No bulk-import API change is required for an input that fits these bounds.

## Test plan

Run `pnpm --dir apps/fractal-web test` and `pnpm --dir apps/fractal-web typecheck` using the repository's installed workspace. Tests cover:

- Real local Unix-gateway HTTP and raw upgrade transport, credential/header isolation, streamed request data, W3C traces, auth refusal/error, static negotiation/HEAD/ETag/SPA, traversal/symlink rejection, cancellation, deadline, tunnel shutdown, and socket resets during pending admission/rejected upgrade shutdown.
- Real generated SDK over local HTTP fixtures; exact arrangements routes/action shape, grants/owner checks, selected-subject subscription and authoritative snapshots/upserts/retirement.
- Property-based register join commutativity/associativity/idempotence, remove-wins deletion, Unicode ordering, stable-ID deterministic canonical rekeying and preservation of parent/placement semantics.
- Full pagination past unchanged pages, cursor-cycle refusal and source fence revocation.
- Strict IndexedDB journal persistence (using an in-process IndexedDB implementation), cross-run staged recovery, accepted-but-lost response replay, concurrent later target edits, failed staging/cutover and serialized migrators.

Local fixtures are transport/state-machine proofs, not a live daemon rollout. Before live cutover, independently exercise the generated SDK against an arrangements-capable daemon, test the host's all-writer fence and durable pointer on every source site, prove no pending legacy edit is stranded, and run the application acceptance matrix on a real Mac. Installing this module does not satisfy those operational gates or authorize deployment/publication.
