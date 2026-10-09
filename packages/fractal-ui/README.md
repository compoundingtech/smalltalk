# Fractal UI

A small, independent UI kit for the Fractal web client, built on React Aria Components and StyleX, with Storybook as its workshop.

It has two layers:

- `.` (`kit.tsx`, `tokens.css`): token-driven component families. The workshop renders them in three switchable visual directions (**Folio**, **Relay**, **Orbit**), each with light and dark palettes and two densities.
- `./assistant-ui` and its subpaths: the dark-first application surfaces the web app composes, built on assistant-ui 0.15.25. They cover the transcript, composer, tool calls and previews, the sidebar agent row and status glyph, the thread header, resizable splits, resource cards, the work log, the sync line and the workbench layout model, plus rich markdown with highlighted code, the thinking disclosure and the dismissible failure layer. Every surface is controlled: the host supplies data, clock and transport, and unknown values render as unknown.

## Run

```sh
nix develop .#web
pnpm install --frozen-lockfile  # repository root
pnpm --filter @smalltalk/fractal-ui storybook        # http://127.0.0.1:53705
pnpm --filter @smalltalk/fractal-ui typecheck
pnpm --filter @smalltalk/fractal-ui build-storybook  # packages/fractal-ui/storybook-static/
```

The package is an explicit member of the root pnpm workspace and uses its pinned toolchain, frozen lockfile and lifecycle-script suppression. `bash scripts/ci-fractal-web` runs its typecheck and the static Storybook build after the shared install.
The kit keeps TypeScript 6.0.3 and declares the Node types used by its Storybook configuration explicitly.

The dev server binds to localhost on a fixed port (53705). To view it from another machine, forward the port, for example `ssh -L 53705:localhost:53705 <host>`, and open http://localhost:53705.

Stories:

- **Fractal UI / Visual language**: one working context rendered in the chosen direction. It contains a thread list, a conversation with a tool call and a diff, a terminal pane, a command palette and component specimens. `Explore` exposes direction, scheme and density as canvas controls and Storybook args. The `FolioLight` … `OrbitDark` and `Compact` stories are fixed comparison entries.
- **Fractal UI / Component families**: every family on one canvas (`AllFamilies`, `FamiliesDark`), under the same three switches.
- **Fractal UI / Markdown**: CommonMark + GFM coverage with streamed fences and inline markup; language fences assert highlighted syntax and stable wrap/copy state.
- **Fractal UI / Thinking Entry**: the muted reasoning disclosure, settled and streaming.
- **Fractal UI / Sync Line**: every observation across both schemes; `TransitionSequence` asserts fixed-height slots and CLS 0 across all transitions.
- **Fractal UI / Transcript**: the locked U2·F3·Y3 transcript in dark and light: settled/expanded work, streamed answers, failed/interrupted/unknown work, loading, retained-history synchronization, answer metadata, unavailable conversations, the older-history boundary with and without a load action, pending and failed sends (including Pending→Sent identity) and the read-only empty state.
- **Fractal UI / Sidebar/Agent Row**: compact hover-card facts, omitted unreported fields, the known-vs-none time cohort and the quick Open hover action.

## Families

`kit.tsx` exports these families. Each is labeled, keyboard-reachable and token-driven:

| Family | Notes |
|---|---|
| Button | `primary`/`secondary`/`quiet`/`danger`; `sm`/`md`; square icon-only; disabled |
| Badge, Pill | Labeled semantic status tags; tone is never the only signal |
| StatusDot | `ready`/`building`/`queued`/`error`/`canceled`, always with text |
| Spinner | Labeled `role=status` |
| Progress | `ProgressBar` with label, value, `maxValue`, tone and size |
| CodeBlock | Filename, language chip, copy action |
| CommandMenu | Grouped items with description, keywords and shortcut; `Autocomplete` virtual focus keeps typing in the field while arrows and Enter drive the list; empty state |
| ContextCard | Preview on hover **and** keyboard focus after a 350 ms intent delay |
| Description, Entity | Metadata pairs; named actor chip |
| EmptyState, Note | Honest empty state with an action; severity notes, filled or outlined |
| Input | Controlled; slash hint; Escape clears; clear button |
| Kbd | Shortcut keys |
| Toggle, Checkbox | `Switch`; checkbox with checked, indeterminate and disabled states |
| Menu | `MenuTrigger` with popover, shortcut hints, separators |
| Modal | `Modal`: focus trap, Escape and scrim dismissal, explicit close |
| Table | Row headers, single replace-selection, compact, optional sticky header |
| Tabs | Controlled; `shouldForceMount` keeps hidden panels' state |
| Tooltip | Terse labels, configurable delay and placement, no close delay |
| Icons | `CheckIcon`, `CopyIcon`, `XCircleIcon`: original inline strokes, `currentColor` |

## Tokens

`tokens.css` defines semantic tokens per direction × scheme:

- Color: canvas, panel, recess, ink, muted, line, accent, on-accent, selection, good, warning, danger, added, removed.
- Type: sans, display, mono.
- Radius: control, panel.
- Motion: duration, curve.
- Density: spacing, control height, chrome size. Each direction applies its own pixel bias.

Every foreground/background pair used for text meets WCAG AA (≥ 4.5:1) in all six palettes. The minimum, 4.59:1, is in Relay dark. Under `prefers-reduced-motion`, durations are zero and spinner and pulse animations stop. Fonts come from system stacks only; no font or image assets are distributed.

## Assistant-ui surfaces

### Gated transcript and work log

`Transcript` is the locked U2·F3·Y3 presentation: left-aligned prompts with an accent rule, four-line highlighted tool previews with a host-owned Open action, and a thin synchronization/run progress rail above the header status. Render it under `EmbraceRuntimeProvider` with the same source references used by the runtime. The host supplies `TranscriptTurn` boundaries, work/lifecycle facts, sender captions, retry actions, output navigation and the observation clock. The transcript never creates a transport or turn state machine.

Turns render once the runtime contains their prompt id; adopting replacement snapshot objects with the same ids preserves rows, open disclosures and scroll anchors. Settled work folds; running, failed, interrupted and explicitly incomplete work stays expanded. Notices, events, unknown events, system text, status and usage summaries render as quiet, muted meta-size inline rows in item order, reusing the runtime converter's text summaries without avatars, sender headers or borders. Harness and subagent messages use the U2 Markdown answer row with a muted sender caption: the host's `senderCaptions` entry takes precedence over the item's sender label. No item kind requires the S2 sender presentation inside `Transcript`. Shared failed-send and empty-state content lives in `composition/TranscriptFeedback.tsx`; `Transcript` and `EmbraceThread` do not import each other. Tool rows identify state and observed duration, use non-interactive rows for absent output, highlight commands, and retain an expanded-work divider. Recognized output media types select syntax highlighting; read paths provide an extension fallback for plain or unrecognized media types. Counts appear only when the host reports them or marks the helper's source history complete. A settled answer keeps its copy action and known completion time below the prose; invalid timestamps are omitted and streamed answers have no settled footer. The one response-in-progress status stays at the turn's live edge.

The runtime adopts new messages in a passive effect after React commits the new snapshot, and its store publishes them a task later. Until then, `Transcript` renders each turn's adopted prefix: a turn whose prompt is already published stays mounted with the items up to the first pending id, and its work calls are filtered to those items. Appending a tool call, reasoning or answer therefore never unmounts the existing turn, prompt, open disclosures or scroll position, and never flashes a fallback. `EmbraceRuntimeProvider` reads the runtime's message ids right after each adoption; an id that is absent from the runtime once its snapshot has been through a commit (an adapter that filters it, a duplicate id) is stranded, not pending: it renders in place as a compact muted row with the item's own text, or **Couldn't display this entry** when it has none, later items keep their order, and development builds `console.warn` the ids. There is no timeout; the signal is the commit epoch plus the runtime's adopted ids. Both are consulted only for ids the store has not published: the provider exposes its adopted ids as a store, and the epoch is recorded only when an unpublished id is not held, so a fully published transcript mounts in one commit. Stranded tool calls and reasoning stay in the work log, which reads the turn rather than the runtime. The dark/light `AppendedItems` stories assert node identity and no fallback flash with a `MutationObserver` across each append; the dark/light `StrandedItem` stories withhold ids from the adapter and assert the fallback rows, their order and the warning.
The adoption subscription uses the same snapshot getter for server and client rendering. `test:transcript` checks `renderToStaticMarkup` for both empty history and an unpublished prompt using the real runtime provider and the kit's StyleX transform.

Sender captions are paragraphs (`message-sender`), not headings, so embedding a transcript inside a Details surface never introduces a skipped heading level. `SemanticItems` checks the Host and Review delegate captions in both schemes.

The host also states what it knows about the conversation itself. `availability` (`Available` or `Unavailable` with host-supplied `reason`/`detail`) swaps the thread for a calm inline state. `history` (`Complete` or `HasOlder`) adds an "Earlier messages not loaded" boundary; "Load earlier messages" appears only when the host passes `onLoadEarlier`. A user `TextItem` may carry `sendState` (`Sent`, `Pending`, or `Failed` with `reason`/`detail`): pending prompts render muted, failed prompts show the reason as a danger line with detail on disclosure, and the item keeps its id from Pending to Sent so the server echo replaces the row in place. `emptyState` (a node or `{ title, body }`) replaces the neutral "No messages yet" once the conversation is live; `EmbraceThread` accepts the same prop. The kit never invents reason copy.
Both `Transcript` and `EmbraceThread` accept `onRetrySend?: (itemId: string) => void`. Failed-send Retry appears only when this callback is supplied; the host owns the transition to Pending and Sent. Keep the same item id throughout Failed→Pending→Sent to retain the row and its DOM identity. The dark/light `FailedRetry` stories exercise the callback and both transitions.
`TranscriptTurn.prompt?: TextItem & { readonly role: 'user' }` is optional: truncated history or standing-seat output can begin mid-turn. An omitted prompt renders no prompt bubble; the host does not need to invent a message. `MidTurnHistory` covers a first promptless turn below the `HasOlder` boundary.
Omit `onOpenTool?: (call: WorkLogCall) => void` for a read-only transcript: tool rows have no action button or disclosure chevron, and recorded details appear in full inline. A turn with neither tool calls nor reasoning has no work summary. Initial loading shows **Loading conversation…**; synchronization copy uses user-facing loading and update wording rather than transport phase names.
Failed sends carry `reason: { _tag: 'Rejected' | 'Ungranted' | 'Invalid' | 'Failed' | 'StaleFence' | 'SnapshotUnavailable' }`. Unknown host causes map to `Failed`, shown as **Couldn't send**. Reason tags stay in a data attribute; optional diagnostic `detail` stays behind the disclosure. Retry requires a callback and a `Failed`, `StaleFence`, or `SnapshotUnavailable` reason; rejection, permission and invalid-message failures never expose Retry.
Agent-authored Markdown never loads images automatically. Each image defaults to an alt-text/host placeholder with a per-image **Load image** action; links use `target="_blank"` and `rel="noopener noreferrer"` without prefetch, and raw HTML is skipped. `Markdown`, `Transcript`, and `EmbraceThread` accept `resolveImage?: (src: string) => { _tag: 'Load'; src: string } | { _tag: 'Defer' }`. A host can explicitly allow its attachment URLs; no URL shape or origin is implicitly trusted. Loaded images suppress referrers, and `srcset`, raw HTML/CSS URLs and content-driven previews are not rendered. `Content network safety` stories exercise both transcript boundaries under request interception.
Hosts whose CSP blocks remote images inline (for example `img-src 'self' data: blob:`) pass `onLoadImage?: (src: string) => void` alongside `resolveImage` on `Markdown`, `Transcript`, `EmbraceThread` and `EmbraceMarkdownPreview`. The placeholder action then reads **Open image · host** and calls the host with the source instead of loading it inline; the host opens it in a new tab with `noopener,noreferrer`. Nothing is fetched before the click, and with `onLoadImage` set nothing is fetched after it either (`*HostOpens*` stories). Without it, the inline load after consent is unchanged.
`EmbraceMarkdownPreview` uses that same boundary and accepts the same resolver and opener, including when rendered inside an Embrace tool call. There is no separate permissive tool-output Markdown renderer.
Syntax grammars are a fixed, eagerly imported package set; an authored code fence cannot cause a lazy grammar/module network request.
The syntax palette is neutral: strings, characters, attribute values and inserted diff lines use the muted foreground token (as the review route's string colour does), keywords and links the running foreground, literals the attention token, deleted lines the danger foreground. No transcript-reachable token resolves to green; the dark/light `NoGreenAllStates` stories check the computed colours of every element across all transcript states, including quoted shell strings and inserted diff tokens.

Added diff rows use the shared blue `statusVars.diffAddTint` token (`#7295ed` dark, `#4269df` light): the gutter marker uses its ink, and the wash mixes that same ink at 9% with transparency. Diff, resource and Embrace preview counts are neutral foreground text. Embrace's legacy `added` token aliases `diffAddTint`; its `good` and the composition `done` defaults use the route's soft foreground token. The kit's syntax strings already use muted foreground directly, so no green `syntaxStringFg` default is introduced. `DiffTint` carries the route's assertion for a non-green wash, a shared marker/wash token and addition/deletion luminance parity.

`EmbraceScrollViewport` follows the latest messages until the reader scrolls, focuses or navigates within history. It preserves the visible row anchor through content growth and width changes without invalidating the message subtree. New content while detached reveals a docked **New messages ↓** button; activating it resumes following. `EmbraceThread` uses the same viewport for its non-virtual E3 lane.
While a row action is pressed, the viewport pins that row's viewport-relative top through insertions above it, rather than relying on the browser's first-visible-row anchor. The jump dock waits for all pointer releases. Wheel, touch, navigation keys, or an actual reader scroll give scrolling back to the reader and stop press compensation. `Transcript press/InsertAbovePressedRow` covers a reply arriving above a failed send between pointerdown and coordinate-based pointerup; the host still owns turn order.
Press compensation also rebases the ordinary history anchor to the adjusted viewport, so releasing the press and revealing the dock never undo the scroll. The insertion play waits for the pointerdown history capture before inserting and checks row position again after release and after dock layout settles.

The shared Markdown boundary keeps exactly one streaming caret on the final paragraph line, or immediately after another terminal block. Tool output uses the exported `HighlightedSource` boundary. Failure banners optionally expose a host-owned **Open output** action without clearing or reflowing history.


### Portable sync seam

`src/assistant-ui/st3-views/sync-status.ts` defines the decoded observation contract with no Effect import: `SyncStatus`, `SyncStage`, `StaleReason` and `SyncFailureCause`; timestamps are epoch milliseconds supplied by the host. `sync-line.ts` exports `syncLine(input): SyncLineValue | undefined` — the shared status vocabulary — and `observeSyncStatus(previous, status, now)`, the client transition clock. `SyncLine.tsx` exports `<SyncLine>`, requiring explicit `now`/`observedAt` numbers and never reading a clock. Hosts decode their own transport into the plain union.

`Stale.reason` is explicit: an observation without a reason binds `{ _tag: 'Unknown' }`, shown immediately as **Waiting for an update** with its observed age, or **Last updated** with a known last-live age, never an invented cause. Only known `Resync`/`Reconnecting` reasons receive their 400 ms/2 s delay. `Failed.cause` is `{ _tag: 'Server'; code; message } | { _tag: 'Local'; kind; detail?: { cap?: number; message?: string } } | { _tag: 'Unknown' }`. Local and server subscription-limit failures share one plain vocabulary; a missing cap is never invented and a reported cap of zero is displayed. Usage surfaces are HTTP reads and never present subscription-slot failures. Retry appears only for retryable causes; Details shows the decoded cause facts.

Hosts must not invent `Progress` or `Quiet` observations to fill gaps in the wire contract. `observeSyncStatus` retains its timestamp only in memory for the same status/stage/reason; do not persist or hydrate it.

### Sidebar agent row

Rows render reported facts only; unreported fields are omitted — never shown as placeholders — and remain in the accessible details. Line-one metric and trailing-signal tracks share the widest intrinsic content width in their row cohort, so rows without a reported since-time keep the same title start as their cohort; tree nesting supplies hierarchy without a second indentation. Hovering a row swaps the time slot for a quick Open action. The hover card stays compact — status, host, current work, spend, duration, Last turn/Last activity, model, PR, branch and subagents — and never shows raw timestamps.

### Markdown and the thinking disclosure

The Markdown boundary renders CommonMark and GFM prose — emphasis, nested lists, tables, linked headings — while preserving resource-chip and inline-reference seams. Fenced code uses the pinned `refractor@5.0.0` dependency with fixed, eagerly imported grammars for the explicit set (TypeScript, TSX, JavaScript, JSON, Bash, diff, Rust, Nix, Python, YAML, Markdown, CSS); authored fences cannot trigger grammar requests, and unknown language labels stay visible above plain source. Each code `<pre>` is keyboard-focusable (`tabIndex={0}`), named **Code, language** (or **Code, text**), and shows an inset semantic-token focus ring. `SemanticItems` exercises an overflowing code block, Tab from Copy code, its accessible name and the visible ring in dark and light. Wrap and copy are local to the stable code-block identity, so streaming text updates do not reset them. Unfinished streaming link tails complete through a linear backward scan of the current line, including escape-run handling, before label brackets are matched. Settled reasoning uses a muted `Thinking` disclosure whose expanded content renders through the same Markdown seam.

### Floating failure layer

`ErrorOverlayHost` defaults to one dismissible floating layer per surface; failures portal into the nearest host so deep banners do not clip or reflow history. Its optional `lane` mode renders notices in place instead and attaches no portal layer. `Transcript` uses lane mode so run failures remain beside their work log without covering history; synchronization failures remain within that host. Escape inside the host dismisses the newest inline or floating banner without moving focus, and a new failure id reappears after a dismissal. Without a host, the work log retains its inline banner.

### Gated workbench and composer

The focused Workbench stories use W3/H1/P3/D2; Composer stories use C2/R3/M2/K1 while exercising the gated alternate states. They do not import the route's sidebar, details fixtures, exploration controls or runtime provider. Workbench thread resources provide the kit's `TranscriptProps` and `ConversationRuntimeOptions` directly: the host owns turns, run state, synchronization facts, observation times and tool actions. This is deliberately different from the route's combined `ConversationState` provider.

Workbench pane identities survive layout changes through persistent pane slots. `workspaceId` scopes stored ratios, tabs and composer drafts, so two workspaces displaying the same pane do not share draft text. `ViewportStore` and `ViewportStoreContext` retain per-conversation scroll/follow/unread state when a thread pane is parked or moved; `Transcript.viewportKey` identifies the owning conversation. The minimal terminal boundary is host-supplied read-only lines/session facts for pane rendering and drag targets, not a terminal transport or emulator.

`DiffPanel` accepts numeric or string widths, optional `onClose` and `landmarkContext`, current-turn and branch file lists, and an explicit `{ path, sequence }` reveal request. Missing file facts remain missing. The host interprets `onOpenTool` and drives controlled diff reveals; the kit never guesses a path from a tool's input. Its scope picker, line-wrap and whitespace controls remain keyboard accessible, with semantic-token React Aria portals owned by `ThemePortal`.

The composer preserves kit send, cancellation, byte-limit and token-history contracts while adding adaptive pill/slab geometry, grouped mention/command popovers, effort selection, and the recipient/model toolbar. `ComposerSession` owns only device-local draft persistence and composer interaction policy; transports and runtime facts remain host-owned. Its tooltips retain the gated 150 ms delay and viewport-clamped, wrapping contents; Workbench/DiffPanel share the 350 ms Controls tooltip instead.

The Workbench thread dock uses the reviewed route's composition composer policy, distinct from `ComposerSession`'s R3 choice: idle Enter sends immediately; during a run Enter queues and modified Enter explicitly steers. `IdleSample` seeds the route's exact `designItems('idle', 'sample')` messages with explicit idle host facts. `RunningQueueAndSteer` keeps the running case separate and asserts both actions without changing the queue. Neither fixture infers a runtime state from a header label.

State and draft codecs use the workspace's single `effect@4.0.0-rc.118` runtime and the matching `@effect/atom-react@4.0.0-rc.118` binding. `react-aria@3.52.1` is declared explicitly for the separator and portal APIs already used by the kit. Performance counters and `RenderProfiler` stay internal rather than becoming package-root exports.

The codecs use native `Schema.optional`, preserving both absent fields and explicitly present `undefined` fields from the original Effect 3 contract. `OptionalSnapshotFields` exercises the real pane-key parser and snapshot encode/read path, including form-only, view-only and undefined-ratio inputs.

The `Geometry` story is a non-interacting fixture for the reusable composer and its layout/running/target/mention matrices. It lets the source geometry assertions measure the same native composer before story play actions mutate its state. Its plain 768px host lane preserves the composer/body width and footer/popover containment assertions without shipping the route's Explore controls, conversation or application chrome. The single lane fills the viewport height and bottom-anchors the composer like the reviewed single preview, so grouped mention popovers have the same room above the composer. `KitLabelResize` returns to its short label after its own play, so external gates measure the real label growth.

The exported legacy Folio, Relay and Orbit palettes also avoid green: `good` is the direction's 80% ink foreground; `added` is a non-green wash whose luminance difference from the panel is within 20% of the direction's removed wash. Run `pnpm --filter @smalltalk/fractal-ui test:palettes` to check all six direction/scheme combinations without a browser. The public names and palette-selection API are unchanged.

The shared diff tint assertion identifies a token through its causal effect on both the row wash and gutter, letting the browser resolve CSS layers, specificity and conditional rules. It restores exact inline styles and verifies computed paint restoration after each probe. Every dark/light tint story also rejects the original neutral pre-tint paint and an equal-blue wash backed by an unrelated variable, then verifies the restored positive result.

### Interactive terminal presentation

`TerminalSurface`, `TerminalDrawer`, `resolveTerminalColor`, `createTerminalPalette`
and their prop types are exported from the package root, `assistant-ui` and `assistant-ui/shell`.
The surface renders the generated `TerminalScreen` DOM lines/runs; it has no
JavaScript terminal emulator, client actions, network transport, or PTY ownership.
The host owns subscriptions, grants, incarnation fences, resize actions and process
lifecycle. The drawer accepts one `screen` and `connection` per agent, with no tabs.
`open`, `height` and `onHeight` are controlled. Pointer resizing previews locally
and commits `onHeight` once on release; keyboard resizing commits immediately.
Close (including Enter on the resize separator) calls `onDetach` then `onToggle`,
not `onKill`. Ending the process requires the destructive confirmation dialog.
Bell notifications remain host-owned: the projected screen has no bell field, so
the kit does not expose an unobservable `onBell` callback.

`onInput(data)` receives plain strings containing valid UTF-8 escape sequences,
never base64. The kit alone encodes application cursor/keypad modes, normalizes
CRLF/LF paste newlines to CR and adds bracketed-paste delimiters when requested.
`onPaste` is an observation callback, not a second input owner. The app sends the
received string as terminal input mode `raw` after obtaining a fresh fence. Focus
events are forwarded only when the screen requests them. Shift+Tab leaves the
surface; Tab otherwise sends a terminal tab. Input is refused for `readOnly`, null
screens and any non-live connection. `readOnlyReason` is announced and describes
why control is not available. Ended/unavailable states carry a human `reason` and
host-provided `onRecover` action (`recoveryLabel` can override its label).

Font metrics use `floor(width / (sizePx * advanceEm))` columns and
`floor(height / lineHeightPx)` rows, with a minimum of one and duplicate size
suppression. The default is mono 13/20, advance 0.6; hosts with different fonts must
supply their measured advance. The optional `handleRef` exposes focus, measurement,
copySelection and clearSelection. Selection preserves wide-character text and joins
wrapped lines. `onCopy` observes the same text as native clipboard copy.

Scrollback is **local**, bounded to 1000 lines by default (0 means current screen
only), and never claimed to be PTY history. Only matched upward screen shifts are
retained; switching terminal/incarnation or entering alternate-screen clears it.
The visible note reports dropped local lines and projected truncation.

**Deliberate palette deviation:** ANSI green/bright-green use the addition-blue
family (`diffAddTint`: dark #7295ed, light #4269df) with distinct luminance steps.
The resolver applies the same remap to every non-neutral indexed/truecolor hue in
90–160 degrees. Cyan is shifted out of teal, and normal/bright blue remain
luminance-separated. This intentionally differs from faithful ANSI greens.
Reduced-motion users get a solid cursor rather than requested blinking.

`Terminal.stories.tsx` covers input, paste, focus, metrics, wide/wrapped selection,
local history, palette and rendered paint, denied input, recovery, SSR, imports,
cursor and drawer lifecycle in both themes. Each scenario exposes `defect`, a
negative-control fixture/behavior that must make that scenario's play fail.
Null-screen SSR renders a static connection placeholder; screen SSR renders DOM
runs without accessing browser globals.

`test:terminal` server-renders null and real screens in both schemes with no
`window`, and asserts that terminal modules import no emulator or runtime client
actions. The base placeholder and forbidden imports are checked as failing
controls.

## Clean-room note

The original kit's behavior references are listed below. The gated workbench/composer additions reuse our independently authored internal review route at `98a270d4e58a44e4c4625cf1956ad09dbabfc1a7`, with the reviewed tint cutover from `35b6922a1559b4b3bcd78b672cb6b707c5d1e108`; their host/runtime boundaries are adapted to this kit as described above. This provenance is not a claim that the later additions were authored without consulting our own source.

The original workflow requirements cited public product documentation of coding-agent workflows; those behavioral references are separate from the internal-source port described above:

- [Coding-agent app workflow introduction](https://openai.com/index/introducing-the-codex-app/): parallel threads, isolated worktrees, review before landing.
- [Zed agent panel](https://zed.dev/docs/ai/agent-panel): keyboard-first palette, per-change review, explicit execution state.

Per direction, the metaphor and the behavior it emphasizes are:

- **Folio**, an annotated working notebook: warm paper, plum annotations, serif headings, deliberate 180 ms easing. It emphasizes progressive disclosure of tool input and output, and an explicit review checkpoint.
- **Relay**, a dispatch desk: mineral surfaces, teal signals, squared 2 px geometry, compact rows, 90 ms linear motion. It emphasizes keyboard-reachable actions, discoverable commands and visible execution outcomes.
- **Orbit**, a navigation instrument: indigo layers, amber bearings, 14/20 px curves, roomier spacing, 240 ms settling motion. It emphasizes keeping context while moving between threads, review and execution, and text-labeled attention.

All displayed content is newly written synthetic data. Workbench terminal fixtures remain read-only; the separately exported interactive terminal is a DOM screen renderer, not an emulator. Nothing in the kit contacts a filesystem, network service, model or provider.

This note records provenance practice. It is not legal clearance or an independent originality certification.
