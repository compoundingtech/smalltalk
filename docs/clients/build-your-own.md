# Build your own Smalltalk client

Start with the typed client for transport and actions, then reuse the conversation model and
feed. Your application owns layout, input, credentials and device storage. It does not need to
parse CLI output or copy stui or the phone's transcript parser.

The working [example TUI](../../examples/client-tui/) lists agents, follows the selected agent's
conversation and sends a message. It is a workspace member, built and tested by Linux/macOS
workspace CI. Its application code is about 400 lines, with no dependency on stui or the daemon.

## Run the example first

Use a checkout and the Rust development environment described in the [root README](../../README.md).
Have st running locally with at least one agent; the [getting-started guide](../getting-started.md)
sets that up. Choose a person already in your fleet; `person/avery` below is an invented example.

```sh
cargo run --locked -p st3-client-tui -- person/avery
# Or choose the local daemon socket explicitly:
cargo run --locked -p st3-client-tui -- person/avery /tmp/example-st3.sock
```

Without an explicit socket, the example passes `ST3_ENDPOINT` to `discover_unix_endpoint`, then
uses the normal st config and XDG runtime/state discovery. `Client::unix_as` is for the trusted
local socket, where st allows the local user to select the person. It does not authenticate a
remote device. For remote access, pair a device and use its credential as described below.

Up/Down selects an agent. Type a single-line message and press Enter to send it to the displayed
agent. Esc clears the draft or discards an uncertain-send retry; Ctrl-C quits. Discarding a retry
cannot retract a message st already accepted. The first bounded window contains at most 200
agents. The conversation follows its end, showing the recent page and saying when earlier
history exists. This example has no history scrolling, search, terminal attachment, fold toggles,
read acknowledgements, attachments or persistent cache; the pieces for those are described below.

Agent rows are ordered by effective display name, then resource ID. HTTP agents pagination
captures immutable final cards with its first page; later pages retain their original values
even when agents change. Card reads use declaration revision directly when present, otherwise
the last canonical claim, and use observed harness evidence rather than status-only work hints.

Open [main.rs](../../examples/client-tui/src/main.rs) for startup/input and
[lib.rs](../../examples/client-tui/src/lib.rs) for update handling, layout and sending.

## Choose the pieces

| Job | Rust | TypeScript |
| --- | --- | --- |
| Typed reads, action parameters/fences, errors and streams | [`st3-client`](../../crates/st3-client/README.md) | [`@smalltalk/st3-client`](../../clients/typescript/st3-client/README.md) |
| Live joined windows, member rotation and reconnect/resubscribe | [`st3-feed`](../../crates/st3-feed/README.md): `run_members`, `Command`, `Update` | Generated client's `collectionStream` and `applyWindow`; the caller owns reconnect/member selection (there is no TS feed package) |
| Timeline revisions, earlier pages, transcript cleaning and delivery joins | [`st3-conversation-ui`](../../crates/st3-conversation-ui/README.md): `Timeline`, `Frame`, `adapt::conversation` | [`@smalltalk/st3-views`](../../clients/typescript/st3-views/README.md): `applyConversation`, `applyOlderPage`, `readOlder`, `conversationEntries` |
| Conversation drawing, folds, selection and pane interactions | Enable `st3-conversation-ui`'s `ratatui` feature for `Cache`/`Theme`; model-only `State` emits `PaneIntent`s | `st3-views` supplies folding, simplified tool rows and entry types; your renderer draws them |
| Mission words, precedence, queue state and step identity | `st3-ui-model::missions::adapt`, borrowing typed projections and caller display policies | Phone-specific mission derivation remains in `apps/ios/missionsView.ts`; it is not part of `st3-views` |
| Session discovery, Home rows, request/answer formatting | Typed client resources; no single Rust package for all these views | `st3-views`: `sessionView`, `homeView`, `requestView` |
| Private read-only display cache | `st3-feed::cache` and `model::Model` | Phone's cache remains application code; it is not a shared TS cache API |
| Device-signing data and reported client name | Typed signature parameters in `st3-client`; caller owns key/signing; `set_client_name` reports your app | `st3-views`'s `deviceSigning` builds canonical bytes/parameters, and `clientName` formats caller-supplied product/version/build |

These are repository source packages, not crates published on crates.io or npm. In a Rust app
inside this checkout, use path dependencies like the [example manifest](../../examples/client-tui/Cargo.toml).
When using them in another repository, pin one Smalltalk revision and keep the sibling crates
together; they use relative path dependencies. The model's default features have no ratatui
dependency. Enable `features = ["ratatui"]` only when drawing with ratatui, whose version must
match the workspace. The example uses ratatui 0.30 and crossterm 0.29.

TypeScript packages export source and share the root pnpm workspace. With Node 24
and Corepack (or `nix develop .#web`), install from the repository root:

```sh
corepack enable # Outside the Nix web shell
pnpm install --frozen-lockfile
```

Use `workspace:*` for `@smalltalk/st3-views` in a workspace app, as the phone does.
The views package also depends on the generated client through `workspace:*`.
Use a TypeScript-aware bundler or Node 24's TypeScript loader. For no-emit checks of
`.ts` imports, enable `allowImportingTsExtensions`; for JavaScript output use
`rewriteRelativeImportExtensions`.
The shared views have no React, React Native, Expo or native key-store dependency. Home rows
return `person`/`green` color tokens; map them through your palette. Pass your product name,
version and build to `clientName`, and a device label to `signatureRefusal` if desired.

## Connect, follow and send

Rust local clients use `Client::unix` or `unix_as`. A paired Unix gateway uses `unix_gateway`;
a paired HTTP(S) gateway uses `fabric_loopback(url, credential)`. Despite the method name, the
HTTP transport can use a reachable authenticated gateway without Fabric. Keep each member's
credential on that member's client; `run_members` returns the selected client in `Connected`.
The example reports `smalltalk-example-tui VERSION` through `set_client_name`.

Pass the feed a standard update sender and a Tokio command receiver. Drain updates from your
UI loop. An agents `Window` replaces the ordered agent rows; keep selection by resource ID,
not row index. `Connected` means the socket opened, not that rows loaded: wait for a live
window before enabling actions. `Offline` preserves the last display while disabling sends.
`Converse { targets: vec![agent_id] }` asks st to join that agent's transcript and Smalltalk.
Apply conversation frames through `Timeline`, then feed `adapt::conversation` into `Cache`.
Do not append frames blindly: entries can be revised, streams can replace their window, and
an agent can restart into another session. The feed can follow at most three conversations.

A TypeScript app composes the corresponding public APIs directly:

```ts
import { St3Client, applyWindow, type CollectionWindow, type TimelineEntry } from '@smalltalk/st3-client';
import { applyConversation, conversationEntries, clientName, type Conversation } from '@smalltalk/st3-views';

const client = new St3Client({
  baseUrl: gatewayUrl, credential: () => pairingCredential,
  client: clientName('smalltalk-ide', version, build),
});
await client.discover();
let agents: CollectionWindow | undefined;
let conversation: Conversation<TimelineEntry> | undefined;
const stream = await client.collectionStream({
  onFrame(frame) {
    if ((frame.kind === 'snapshot' || frame.kind === 'changes') && frame.id === 'agents') {
      agents = applyWindow(agents, frame); // current ordered window and its snapshot
    } else if (frame.kind === 'conversation' && frame.id === 'chosen') {
      conversation = applyConversation(conversation, {
        replace: frame.replace, items: frame.items,
        hasMore: frame.has_more ?? false, sessionId: frame.session_id,
      });
      render(conversationEntries(conversation.entries, agentNames));
    }
  },
  onEnd(error) { markOffline(error); }, // reopen and resubscribe with backoff in your app
  socket: authenticatedWebSocketFactory,
});
stream.subscribe('agents', 'agents', 200);
stream.subscribeConversation('chosen', selectedAgentId);
// stream.close() when the view closes.
```

The gateway URL, credentials, selected ID, render callbacks and socket factory are application
inputs. Native/browser WebSockets do not all support authentication headers; use the client's
socket-factory interface for your platform (see the generated client README). Browser adapters
must use the gateway's supported authentication path; do not put a pairing credential in an
invented query parameter. Call `discover()` and check capability versions/states before enabling
optional controls. A missing or refused collection stays unavailable, not an empty successful list.

To send, take a fresh snapshot from `capabilities`, build `MessageSendParameters`, and call
`message_send` with a `Fence` and a fresh action/idempotency pair (`st3_feed::action_pair`).
TypeScript's generated `messageSend` takes the equivalent typed action. Reuse the same action ID,
idempotency key, recipient and body when retrying an uncertain result; never silently queue a
new mutation on reconnect. The example retains that request, locks recipient/editing until
acknowledgement or explicit discard, and says **Accepted**, not delivered. For actions that
continue asynchronously, follow the returned operation (`followOperation` in TypeScript;
`operations_get` in Rust) and display its terminal result. The server enforces actor and fences;
show stable refusals through the client's plain-error helpers instead of parsing CLI messages.

Remote device sends may require a device signature: pairing credentials authenticate the
gateway connection, while the signature attributes the message to the device's person.
`deviceSigning` provides canonical bytes and the typed signature parameter; your platform owns
the private key, SHA-256 and signing. Follow [device signing](../st3/device-signing.md) and its
shared vectors. Do not copy the phone's Secure Enclave module into a desktop client.

## Recover and extend

The Rust feed resubscribes after a disconnect and rotates among supplied members. It does not
replay mutations. Collection `resync` requests a fresh subscription; that snapshot is authoritative.
A TS app must implement the same reopen/resubscribe policy around its stream. Keep reconnecting,
loading, empty, failed and stale visible states distinct. Clear derived conversation state when
the selected target changes; only apply updates for the current target.

For older history, read the resolved session's timeline through the typed client and use
`Timeline::older_page` or TS `readOlder`/`applyOlderPage`. Expired cursors require a fresh read;
do not invent entries to cover gaps. The optional Rust snapshot/event model has a separate
cursor-gap recovery path: `Model::sync` reports the gap, resets its bounded collections and
requires the application to discard other derived state. This is not a reason to poll joined
live windows. Cached snapshots are display data, never authority for an offline action.

For terminal control, typed `terminal.attach` supplies a capability/incarnation; the feed can
follow a projected screen, or `st3-client` exposes a raw PTY connector for `pty-terminal`.
Input needs the owner's fresh sequence/incarnation fences; use the feed's fence helpers.
For richer interactions, use `PaneIntent` and execute its actions in your application. Drawing
code and pane state do not gain authority to send just by receiving text.

## Keep your client consistent

The contract is [ui-contract.md](ui-contract.md). Shared transcript fixtures in
`fixtures/clients/transcripts` pin cleaned/merged entries across Rust and TypeScript; signing
vectors pin the bytes devices sign. Extend those fixtures when behavior changes and keep both
sides passing. Each rendered client still owns its layout, keyboard/touch behavior and palette.

```sh
cargo test --locked -p st3-client-tui
cargo test --locked -p st3-conversation-ui --no-default-features
cargo test --locked -p st3-conversation-ui --features ratatui
pnpm --filter @smalltalk/st3-views test
pnpm --filter @smalltalk/st3-views typecheck
```

The example tests use a terminal test backend and a loopback test gateway with the real client
and feed: they check stable selection, stale/offline behavior, shared transcript rendering,
subscription composition, fenced message parameters and unchanged idempotency on retry. They
need no running daemon or paired device. Run server-backed st3 tests outside an agent seat's
`ST_AGENT` environment, as the repository's contribution constraints require.
