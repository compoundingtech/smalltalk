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
`deviceSigning`, `clientName`, `time`, `sidebarView`) expose the same APIs individually.

Conversation parsing, simplified tool bundles, live-window/history joining, session
discovery, Home filtering/grouping, requests and structured answers live here. Home
colors are semantic tokens (`person`, `green`); renderers resolve them through their
palette. `clientName` takes the product name, version and optional build explicitly.
Signing builds canonical bytes and signature parameters; the caller owns the key,
hashing and signing. `signatureRefusal` accepts a device label (default `device`),
and the phone passes `phone` to retain its wording.

## Folder-only sidebar semantics

`@smalltalk/st3-views/sidebarView` is the React-free twin of
`st3_ui_model::sidebar`. `build(arrangement, inventory)` projects native version-1
arrangements using the daemon's authoritative `resolved` maps. Its tagged result
is `Built { tree }` or `Refused { error }`; missing resolution is `NotReady`,
not a guessed parent graph. Opaque register revisions cannot establish which
concurrent edge won a cycle.

The tree retains stored `parent`, resolved `shownUnder`, position `key` and
`revision`. Folder and subject sibling orders remain separate: folders first,
then placed subjects, each ordered by key and identity; inventory-only subjects
follow in identity order. Renderers still own labels, filtering, collapse state,
host groups and stui's flat folder presentation. `nodeKey` encodes identity
without changing it. The inventory is additive: placements absent from it,
including dormant and non-agent subjects, remain in the tree. A partial
inventory never removes a placement or changes its resolved ancestry.

`canPlace` and `planMove` validate semantic `{ node, parent, before }` intents
against the full tree, including dormant placements. `before: null` means append
within the node's folder/subject order domain; cross-domain anchors are refused.
Only folders can be parents. Refusals are `_tag` unions with structured
references, not display text. Planning does not allocate fractional keys, emit
wire operations or replace fresh daemon admission. An explicit move of a lifted
row to its shown parent is not treated as unchanged.

`rollup(tree, needsYou)` counts subjects through resolved folder ancestry and
returns folder-keyed `{ needsYou, display: 'Always' }` descriptors. Presentation
copy and widgets remain consumer-owned.

Shared Rust/TypeScript vectors live in `fixtures/clients/sidebar-v2.json`; the
filename reserves the sidebar contract's vector family, while its
`scope: "folder-only-v1"` explicitly identifies today's arrangement data.
Folder identities are fixed UUIDv7 literals, ordered like the original symbolic
identities so ID tie-breaks stay deterministic. The TypeScript twin tests decode
every arrangement with the strict generated schema before checking projection.
This seam changes no storage, schema or wire format. Mixed parents, declared
defaults/reset, availability, optimistic application and revision/cycle-cut
settlement belong to format-v2 work, not this API.

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
the package. The root frozen install covers every workspace package; `bash scripts/ci-typescript-client`
runs the combined checks.
