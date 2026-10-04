# Shared client views

`@smalltalk/st3-views` is the phone's view model, usable by an IDE or another TypeScript
client. It exports TypeScript source, like the sibling generated `@smalltalk/st3-client`.
Use a TypeScript-aware bundler, or Node 24 with `--experimental-transform-types`.
It has no React, React Native, Expo or platform key-store dependency.

The phone installs it through `file:../../clients/typescript/st3-views`. Another app in
this repository can use a file dependency pointing to this directory. The generated
client is a sibling file dependency; keep both directories together.

```ts
import { conversationEntries, applyConversation, homeRows, clientName } from '@smalltalk/st3-views';

const held = applyConversation(undefined, frame);
const entries = conversationEntries(held.entries, agentNames);
const rows = homeRows(attention, sessionActor);
const name = clientName('smalltalk-ide', version, build);
```

The root exports all functions and types. Subpaths named after the source modules
(`conversationView`, `conversationSimple`, `sessionView`, `homeView`, `requestView`,
`deviceSigning`, `clientName`, `time`) expose the same APIs individually.

Conversation parsing, simplified tool bundles, live-window/history joining, session
discovery, Home filtering/grouping, requests and structured answers live here. Home
colors are semantic tokens (`person`, `green`); renderers resolve them through their
palette. `clientName` takes the product name, version and optional build explicitly.
Signing builds canonical bytes and signature parameters; the caller owns the key,
hashing and signing. `signatureRefusal` accepts a device label (default `device`),
and the phone passes `phone` to retain its wording.

From the repository root:

```sh
npm ci --prefix clients/typescript/st3-views --ignore-scripts --no-audit --no-fund
npm test --prefix clients/typescript/st3-views
npm run typecheck --prefix clients/typescript/st3-views
```

The moved tests run without phone dependencies. Transcript tests read the original
`fixtures/clients/transcripts/*.json` and compare with the Rust model's expected
entries. Signing tests verify `fixtures/clients/device-signing-v1.json`. Fixtures are
shared, never copied into this package. CI also typechecks and tests the phone against
the package. After installing all three packages, `bash scripts/ci-typescript-client`
runs the combined checks.
