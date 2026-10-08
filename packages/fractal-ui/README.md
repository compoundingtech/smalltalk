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
- **Fractal UI / Markdown**: CommonMark + GFM coverage with streamed fences and inline markup; language fences assert lazy grammar loading and stable wrap/copy state.
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

`Transcript` is the locked U2·F3·Y3 presentation: left-aligned prompts with an accent rule, four-line highlighted tool previews with a host-owned Open action, and a thin synchronization/run progress rail above the header status. Render it under `EmbraceRuntimeProvider` with the same source references used by the runtime. The host supplies `TranscriptTurn` boundaries, work/lifecycle facts, sender captions, retry actions, output navigation and the observation clock. The kit never creates a transport, split-pane controller or turn state machine.

Turns render once the runtime contains their prompt and item ids; adopting replacement snapshot objects with those ids preserves rows, open disclosures and scroll anchors. Settled work folds; running, failed, interrupted and explicitly incomplete work stays expanded. Notices, events, unknown events, system text, status and usage summaries render as quiet, muted meta-size inline rows in item order, reusing the runtime converter's text summaries without avatars, sender headers or borders. Harness and subagent messages use the U2 Markdown answer row with a muted sender caption: the host's `senderCaptions` entry takes precedence over the item's sender label. No item kind requires the S2 sender presentation inside `Transcript`. Shared failed-send and empty-state content lives in `composition/TranscriptFeedback.tsx`; `Transcript` and `EmbraceThread` do not import each other. Tool rows identify state and observed duration, use non-interactive rows for absent output, highlight commands, and retain an expanded-work divider. Recognized output media types select syntax highlighting; read paths provide an extension fallback for plain or unrecognized media types. Counts appear only when the host reports them or marks the helper's source history complete. A settled answer keeps its copy action and known completion time below the prose; invalid timestamps are omitted and streamed answers have no settled footer. The one response-in-progress status stays at the turn's live edge.

The host also states what it knows about the conversation itself. `availability` (`Available` or `Unavailable` with host-supplied `reason`/`detail`) swaps the thread for a calm inline state. `history` (`Complete` or `HasOlder`) adds an "Earlier messages not loaded" boundary; "Load earlier messages" appears only when the host passes `onLoadEarlier`. A user `TextItem` may carry `sendState` (`Sent`, `Pending`, or `Failed` with `reason`/`detail`): pending prompts render muted, failed prompts show the reason as a danger line with detail on disclosure, and the item keeps its id from Pending to Sent so the server echo replaces the row in place. `emptyState` (a node or `{ title, body }`) replaces the neutral "No messages yet" once the conversation is live; `EmbraceThread` accepts the same prop. The kit never invents reason copy.
Both `Transcript` and `EmbraceThread` accept `onRetrySend?: (itemId: string) => void`. Failed-send Retry appears only when this callback is supplied; the host owns the transition to Pending and Sent. Keep the same item id throughout Failed→Pending→Sent to retain the row and its DOM identity. The dark/light `FailedRetry` stories exercise the callback and both transitions.

`EmbraceScrollViewport` follows the latest messages until the reader scrolls, focuses or navigates within history. It preserves the visible row anchor through content growth and width changes without invalidating the message subtree. New content while detached reveals a docked **New messages ↓** button; activating it resumes following. `EmbraceThread` uses the same viewport for its non-virtual E3 lane.

The shared Markdown boundary keeps exactly one streaming caret on the final paragraph line, or immediately after another terminal block. Tool output uses the exported `HighlightedSource` boundary. Failure banners optionally expose a host-owned **Open output** action without clearing or reflowing history.


### Portable sync seam

`src/assistant-ui/st3-views/sync-status.ts` defines the decoded observation contract with no Effect import: `SyncStatus`, `SyncStage`, `StaleReason` and `SyncFailureCause`; timestamps are epoch milliseconds supplied by the host. `sync-line.ts` exports `syncLine(input): SyncLineValue | undefined` — the shared status vocabulary — and `observeSyncStatus(previous, status, now)`, the client transition clock. `SyncLine.tsx` exports `<SyncLine>`, requiring explicit `now`/`observedAt` numbers and never reading a clock. Hosts decode their own transport into the plain union.

`Stale.reason` is explicit: an observation without a reason binds `{ _tag: 'Unknown' }`, shown immediately as **Stale** with its observed age or known last-live age, never an invented cause. Only known `Resync`/`Reconnecting` reasons receive their 400 ms/2 s delay. `Failed.cause` is `{ _tag: 'Server'; code; message } | { _tag: 'Local'; kind; detail?: { cap?: number; message?: string } } | { _tag: 'Unknown' }`. Local and server subscription-limit failures share one plain vocabulary; a missing cap is never invented and a reported cap of zero is displayed. Usage surfaces are HTTP reads and never present subscription-slot failures. Retry appears only for retryable causes; Details shows the decoded cause facts.

Hosts must not invent `Progress` or `Quiet` observations to fill gaps in the wire contract. `observeSyncStatus` retains its timestamp only in memory for the same status/stage/reason; do not persist or hydrate it.

### Sidebar agent row

Rows render reported facts only; unreported fields are omitted — never shown as placeholders — and remain in the accessible details. Line-one metric and trailing-signal tracks share the widest intrinsic content width in their row cohort, so rows without a reported since-time keep the same title start as their cohort; tree nesting supplies hierarchy without a second indentation. Hovering a row swaps the time slot for a quick Open action. The hover card stays compact — status, host, current work, spend, duration, Last turn/Last activity, model, PR, branch and subagents — and never shows raw timestamps.

### Markdown and the thinking disclosure

The Markdown boundary renders CommonMark and GFM prose — emphasis, nested lists, tables, linked headings — while preserving resource-chip and inline-reference seams. Fenced code uses the pinned `refractor@5.0.0` dependency with lazily loaded grammars for the explicit set (TypeScript, TSX, JavaScript, JSON, Bash, diff, Rust, Nix, Python, YAML, Markdown, CSS), loaded through `refractor/core` only when a fence appears; unknown language labels stay visible above plain source. Wrap and copy are local to the stable code-block identity, so streaming text updates do not reset them. Unfinished streaming link tails complete through a linear backward scan of the current line, including escape-run handling, before label brackets are matched. Settled reasoning uses a muted `Thinking` disclosure whose expanded content renders through the same Markdown seam.

### Floating failure layer

`ErrorOverlayHost` defaults to one dismissible floating layer per surface; failures portal into the nearest host so deep banners do not clip or reflow history. Its optional `lane` mode renders notices in place instead. `Transcript` uses lane mode so run failures remain beside their work log without covering history; synchronization failures remain within that host. Escape inside the host dismisses the newest inline or floating banner without moving focus, and a new failure id reappears after a dismissal. Without a host, the work log retains its inline banner.

## Clean-room note

This kit was written fresh from behavior-only requirements. No existing design-system source, CSS, tokens, fonts, icons, logos, screenshots or microcopy was imported or consulted. Every palette value, spacing bias, type stack, corner scale and motion curve was chosen independently for this kit.

Behavioral inspiration came only from public product documentation of coding-agent workflows. No source code, assets, branding or copy was reused:

- [Coding-agent app workflow introduction](https://openai.com/index/introducing-the-codex-app/): parallel threads, isolated worktrees, review before landing.
- [Zed agent panel](https://zed.dev/docs/ai/agent-panel): keyboard-first palette, per-change review, explicit execution state.

Per direction, the metaphor and the behavior it emphasizes are:

- **Folio**, an annotated working notebook: warm paper, plum annotations, serif headings, deliberate 180 ms easing. It emphasizes progressive disclosure of tool input and output, and an explicit review checkpoint.
- **Relay**, a dispatch desk: mineral surfaces, teal signals, squared 2 px geometry, compact rows, 90 ms linear motion. It emphasizes keyboard-reachable actions, discoverable commands and visible execution outcomes.
- **Orbit**, a navigation instrument: indigo layers, amber bearings, 14/20 px curves, roomier spacing, 240 ms settling motion. It emphasizes keeping context while moving between threads, review and execution, and text-labeled attention.

All displayed content is newly written synthetic data. The terminal is a read-only fixture, not an emulator. Nothing contacts a filesystem, network service, model or provider.

This note records provenance practice. It is not legal clearance or an independent originality certification.
