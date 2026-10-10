# Shared client views

`@smalltalk/st3-views` is the phone's view model, usable by an IDE or another TypeScript
client. It exports TypeScript source, like the sibling generated `@smalltalk/st3-client`.
Use a TypeScript-aware bundler, or Node 24 with `--experimental-transform-types`.
It has no React, React Native, Expo or platform key-store dependency.

The phone depends on this package through `workspace:*`, and this package uses
`workspace:*` for the generated client. Other apps in the root pnpm workspace can
use the same dependency convention.

```ts
import { conversationEntries, applyConversation, homeRows, clientName } from '@smalltalk/st3-views';

const held = applyConversation(undefined, frame);
const entries = conversationEntries(held.entries, agentNames);
const rows = homeRows(attention, sessionActor);
const name = clientName('smalltalk-ide', version, build);
```

The root exports all functions and types. Subpaths named after the source modules
(`conversationView`, `conversationSimple`, `sessionView`, `homeView`, `requestView`,
`deviceSigning`, `clientName`, `time`, `metricCards`) expose the same APIs individually.

`metricCards` turns `machines.list` and `host-facts.read` into the cards stui and Fractal draw
through `crates/st-surface`; both check `fixtures/clients/metric-cards.json`.

Conversation parsing, simplified tool bundles, live-window/history joining, session
discovery, Home filtering/grouping, requests and structured answers live here. Home
colors are semantic tokens (`person`, `green`); renderers resolve them through their
palette. `clientName` takes the product name, version and optional build explicitly.
Signing builds canonical bytes and signature parameters; the caller owns the key,
hashing and signing. `signatureRefusal` accepts a device label (default `device`),
and the phone passes `phone` to retain its wording.

From the repository root with Node 24 and Corepack (or `nix develop .#web`):

```sh
corepack enable # Outside the Nix web shell
pnpm install --frozen-lockfile
pnpm --filter @smalltalk/st3-views test
pnpm --filter @smalltalk/st3-views typecheck
```

The moved tests run without phone dependencies. Transcript tests read the original
`fixtures/clients/transcripts/*.json` and compare with the Rust model's expected
entries. Signing tests verify `fixtures/clients/device-signing-v1.json`. Fixtures are
shared, never copied into this package. CI also typechecks and tests the phone against
the package. The root frozen install covers all three packages; `bash scripts/ci-typescript-client`
runs the combined checks.
