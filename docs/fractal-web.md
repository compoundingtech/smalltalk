# Fractal web implementation

Run the commands below from the repository root after a frozen workspace install.
The [development guide](development.md) covers shared build and contribution tools.

## fractal-web Content Security Policy

The app server and Vite's development/preview servers set an enforcing
`Content-Security-Policy` on HTML and assets, including HEAD and cached responses.
This is defence in depth: agent Markdown must not automatically fetch remote images,
and an accidental renderer regression must not reveal the reader's network address
or contact an agent-selected tracking endpoint.

The policy is:

```text
default-src 'self'; script-src 'self' 'wasm-unsafe-eval' <inline-script SHA-256 hashes>; style-src 'self' 'sha256-38RhXrc7EdReTKsOm23ZPOCUgniTUUcjky8QOOrQx6o=' 'sha256-gYiS/BvZvRcK27JIXTuwhZ3hs2+VJ1X+2gUlE+farlg=' <inline-stylesheet SHA-256 hashes>; style-src-attr 'unsafe-inline'; img-src 'self' data: blob:; font-src 'self'; connect-src 'self' ws://<page-authority> wss://<page-authority> <configured collector origins>; object-src 'none'; base-uri 'self'; frame-ancestors 'none'; form-action 'self'
```

`'wasm-unsafe-eval'` lets the terminal key encoder compile its WebAssembly module.
It does not allow `eval()` or string-to-code functions.

Hashes authorize only scripts in trusted application HTML: the early collections
connect, the server's deployment identity, and Vite's development React-refresh
preamble. There is no script `unsafe-inline` or `unsafe-eval` exception. Compiled
StyleX CSS and fonts are same-origin. Only `style-src-attr` needs `unsafe-inline`:
StyleX variable values, resizable widths and React Aria positioning are runtime
style attributes whose changing values cannot use fixed hashes. The two fixed style
hashes authorize React Aria 3.52.1's fixed pressable touch-action and iOS modal
overscroll stylesheets (`usePress` and `usePreventScroll`); recheck them when
upgrading React Aria. Inline-stylesheet hashes cover the `<style>` elements of the
trusted application HTML: the themed first-paint shell, which must apply before any
bundle stylesheet loads. Production and preview admit no other inline stylesheet
elements, and no `data:` stylesheet. The bundle stylesheets (application CSS and
compiled StyleX) are deferred `media="print"` links that a hashed bootstrap enables
together once all have loaded; the app renders only then.
Vite development additionally allows `style-src-elem 'self' 'unsafe-inline'`
for its injected stylesheets and
`worker-src 'self' blob:` for Vite's HMR reconnect SharedWorker. These development
exceptions do not relax `img-src` or production's script/worker policy.

Both socket schemes are restricted to the request's exact authority (host and port),
supporting HTTP development and HTTPS behind TLS termination without trusting
forwarded headers. The browser gateway is `window.location.origin`; the early socket
uses the same origin. `WF_ST_GATEWAY` selects a server-side Unix socket, not a
cross-origin browser destination. Same-origin `/otlp` also needs no exception.
For an explicit cross-origin OTLP collector, Vite derives its exact HTTP(S) origin
from `VITE_OTLP_TRACES_URL`. Hosts embedding `createFractalWebServer` or
`createFractalWebMiddleware` must supply the same origin in `connectOrigins`.
Only exact origins are accepted, without credentials, paths or wildcards.
Cross-origin Vite HMR configuration is rejected; HMR must use the page authority.

The explicit **Open image · host** action opens the original URL in a new tab using
`window.open(url, '_blank', 'noopener,noreferrer')`, while retaining a placeholder
that shows the target host before the action. Do not implement a same-origin image
proxy: it would make the app server a fetcher for agent-selected URLs, introducing
SSRF/internal-network exposure and bandwidth abuse, while weakening this boundary.
`ConversationPane` passes the kit's `onLoadImage` callback through `Transcript` and
leaves `resolveImage` at its default `Defer`. The kit renders no remote image before
or after this callback; it never fetches the URL on the app's behalf.
`onOpenResource` remains reserved for workspace references.

Run the regression proofs with:

```sh
CI=1 pnpm --dir apps/fractal-web exec vitest run server/core.integration.test.ts server/csp.unit.test.ts
CI=1 node apps/fractal-web/scripts/csp-proof.mjs
CI=1 pnpm exec tsc --noEmit -p apps/fractal-web
```

The browser proof builds and serves the real app, exercises production, preview
and development, and proves the native transcript's image action shows the host,
hands off to the new-tab opener, and renders no remote image or image request.
The cold development phase prebundles the transcript's Markdown and React dependencies
together, so late dependency discovery cannot invalidate its in-flight imports.
It then records CSP violations before and after injecting a remote image.
Its test-only proxy strips the header for a negative control: the image
request then reaches a browser route, which aborts it without contacting the
remote site. With CSP enforced the route is never reached and `img-src` reports
the violation. Browser sessions, listeners and temporary build files are closed
and removed on exit. There is no production CSP-disable switch.

## Hosting the web app on a tailnet

`apps/fractal-web/server/main.mts` runs the built app for one person. It listens on loopback
only, and `tailscale serve` publishes it over HTTPS on the tailnet. The server injects one paired
bearer into every admitted request, so admission is the whole authorization. With a full-control
pairing, any script running in the app holds that person's full st authority. A request must
carry an allowed `Tailscale-User-Login`, which `tailscale serve` attaches for tailnet users. Any
browser `Origin` must be exactly the published origin, and upgrades and unsafe methods must
carry one. An Origin-less read is refused when the browser's `Sec-Fetch-Site` says another site sent it. Never publish it with Funnel, which carries no tailnet identity. Anything else that
can reach the loopback port could claim a login, so run it only on a host where every local
user is already trusted with that person's st.

```sh
pnpm --dir apps/fractal-web build            # writes dist/web, including build-identity.json
WF_PORT=7410 \
WF_PUBLIC_ORIGIN=https://atlas.example.ts.net:8443 \
WF_TAILNET_LOGINS=river@example.com \
WF_ST_GATEWAY=$XDG_RUNTIME_DIR/st3-client.sock \
WF_ST_AUTHORIZATION_FILE=/path/to/paired-bearer \
  pnpm --dir apps/fractal-web start
tailscale serve --bg --https=8443 http://127.0.0.1:7410
```

The settings:

- `WF_ST_GATEWAY` names st's paired client gateway socket, never the privileged `st3.sock`.
- `WF_ST_AUTHORIZATION_FILE` holds the paired device's bearer. The composer needs a full-control pairing.
  The host refuses to start unless the file belongs to its user and nobody else can read it.
- `WF_DIST` defaults to `dist/web`.
- `WF_HOST` must be a loopback address.
- `WF_DEPLOYMENT_ID` defaults to the published host.

Before relying on a published endpoint, prove that `tailscale serve` replaces a forged login. A request
to the published HTTPS address carrying `Tailscale-User-Login: <an allowed login>` from a tailnet
user who is not on the list must still get `403`. Also check that `tailscale funnel status` shows
nothing for the port.

The build records `rev`, `dirty`, `commitTs` and `buildTs` from git. The bundle embeds them, and
the host serves them at `/build-identity.json`. That response names the exact commit that is
running.

## Fractal web transcript switch paint

The conversation pane reports `ux.transcriptCommitted(ref)` only after its scoped DOM
contains real turns or the authoritative empty state, without a loading placeholder.
The external-store runtime adopts messages asynchronously; feed arrival alone is not a
commit. The pane observes that adoption inside a display-contents host boundary and cancels
its pending paint when hidden or unmounted. Telemetry completes after rAF plus MessageChannel,
not inside render.

The shell loads the conversation pane through `React.lazy` and per-pane `Suspense`, keeping
the transcript, Markdown parser and syntax grammar registration outside its eager module
graph. A selected thread requests its chunk immediately; an empty shell prefetches it after
its first paint. Demand and prefetch share one import. While code loads, the lightweight
app-side fallback mirrors the kit's Waiting transcript chrome and skeleton. Its drift test
compares the real kit structure, testids, accessibility labels and geometry; replace this
mirror when the kit exposes its standalone skeleton.
The fallback uses the kit's shared reading-column styles so chunk loading does not shift
the history lane when the real transcript mounts.
An import failure stays local to the requested pane, with the fixed message “This conversation
view could not load.” and a “Try again” action. Rejection clears the shared request; explicit
retry creates and shares a fresh lazy wrapper so React's cached rejection cannot poison the
next attempt or conversations opened after recovery.

Switching back to one of the three most recently opened agents reveals its retained pane
instead of mounting the kit transcript again. The retained panes live only in memory, in
first-open order, so a pane's DOM never moves. Hidden panes use `content-visibility: hidden`:
the browser keeps their layout but skips their rendering, focus and accessibility tree.
The outgoing pane stays under the opaque shown pane for the switch frame and is hidden after
that paint, because hiding costs time proportional to its rows. Covered panes are immediately
excluded from the accessibility tree, lose focus, and have only their small composer made
inert; hidden-pane input events are suppressed. Non-thread notices paint above retained
backgrounds. A hidden pane unmounts its
feed interest, so the SDK marks its follow invisible, and the follow may be evicted when an
open needs the slot. The pane's visibility lives in a wrapper, and the kit transcript is
memoized, so a switch never re-renders a retained transcript. The panes are portalled from
outside the workspace's shell context providers. Those providers change on every switch, and
React would otherwise walk every retained fiber looking for consumers.

Reading position is independent of that three-pane DOM cache. `WorkspaceBody` owns one
memory-only kit `ViewportStore` for its surface lifetime, above the keyed panes, and each
transcript uses its stable agent conversation ref as `viewportKey`. A detached reading line
survives switching away and back, even after its pane is evicted and remounted. Memory is
limited to the 32 most recently visited conversations still in the current agent roster;
layout-phase pruning drops departed agents and older visits after viewport cleanup saves.
Returning to a dropped key opens at the end. Remounting
the whole surface (including a reload) discards the store and opens at the live end.
The existing `lastSendId` command still re-attaches on a new own send; no clock tick drives
running-turn motion. The composition integration test exercises A → B → A, eviction through
C and D, a fresh surface, roster departure, and the 32-conversation bound, using the real
kit controller with deterministic jsdom sizing.
The surface imports viewport ownership from the lightweight scroll module rather than the
full kit barrel, preserving the shell's eager-graph boundary.

`node apps/fractal-web/scripts/switch-budget-proof.mjs [--turns=100] [--baseline]` builds a
production bundle with a synthetic three-agent fixture
(`apps/fractal-web/scripts/switch-budget-fixture.tsx`). The fixture's turns contain markdown,
lists and TypeScript fences, and it serves first pages after a fixed 120 ms delay. The script
drives Chrome through `playwright-cli` and times each switch from the roster click (capture
phase) to the paint that first shows the selected, visible lane with turns and no placeholder
(rAF, then a MessageChannel task). It takes two first opens (B3: the 300 ms budget, including
the synthetic server wait, reported as click to request, request to frame and frame to
paint), then nine untraced switches back (B2: the median must stay under 50 ms). A separate
traced pass reports where the time goes. `--baseline` prints without failing. The numbers
depend on host load, so compare runs interleaved on the same host.
`--regression=notice` checks that Usage and Terminal notices are exposed by real browser
hit-testing after opening a transcript. `--regression=keyboard` holds the deferred hide,
then checks that the covered composer loses focus, cannot regain it, leaves the accessibility
tree and cannot send its retained draft on Enter. Its synthetic send port records attempted
sends and refuses them without contacting a gateway. Both regressions fail independently of
the timing gate.

## Fractal web captured changes

The Changes panel reuses `source.conversation(agentRef)`: its live source follows
`/v1/client/collections/stream` with `collection: "conversation"`. Decoded `tool_call`
arguments and successful `tool_result` content supply recorded patches or edited excerpts;
there is no private capture fixture, repository read, or separate file-change endpoint.
The public `/v1/client/conversations/{id}/changes` route reports conversation deltas,
not current-worktree diffs.

Only successful calls with an observed nonerror result and known changed content appear.
The inspector preserves reported tool/sender provenance, separates repeated captures of
the same path, and scopes counts to the shown capture and loaded transcript window.
Missing captures do not establish that a repository is unchanged. Waiting, unavailable,
stale observations and older-history boundaries remain visible. Resource card/chip actions
select captured content locally; they do not fetch repository state.

The panel acquires visible conversation demand while mounted and shares the thread's
retained feed. Its kit diff viewer loads on demand to preserve the shell's eager-graph
boundary. `LiveHeader.integration.test.tsx` exercises the actual shell and kit inspector
with decoded public conversation frames; `capturedChanges.unit.test.ts` guards the
capture, count, provenance and selection rules.

## Fractal web composer

The conversation pane mounts the kit's `EmbraceComposer`. Its app-side binding lives in
`apps/fractal-web/src/web/composerSend.ts`; sending, idempotency, the in-memory outbox and
echo reconciliation remain owned by `source.attachments.send`. New submissions use `Send`;
a failed outbox row's Retry uses `Resend` with the exact original source-owned key.
Outbox rows keep their original send time on Retry and stay after earlier or equal-time
transcript rows, before later replies. Multiple own sends retain submission order; only an
authoritative message-identity echo replaces the matching outbox row.
Merging visits transcript and retained outbox timestamps at most once each per projection,
using monotone insertion boundaries to preserve that order even if send times move backward.
The source retains the newest new Send's identity as `lastSendId`, which the pane passes
through as the kit's `scrollToBottomKey`. Settlement, echo and Retry keep that identity
unchanged; only another new submission resumes following. The composer opts into the
transcript's shared reading column without overriding the default placeholder or adding hints.
Pending, Sent and Failed (reason plus disclosed detail) are rendered by kit A.
Send failures use allowlisted human-readable explanations; raw diagnostics never enter row
disclosures, composer help, status text or accessible names. Unclassified reasons use generic copy.
Each pane retains its mapped runtime items and transcript turns across clock/sync updates
and semantically unchanged frames, including a replacement decode with new object references.
The cache uses the conversation model's schema equivalence and retains only the latest projection.
A confirmed Sent row stays visible until its authoritative in-window echo, even if a later
read omits remotely owned mail. Neither drafts nor the outbox are persisted in the browser.

Only `grants.messageSend` authorizes the composer. An absent send port shows Unknown, and
non-retryable refusals disable new submissions with their classified human-readable reason. Retryable send failures
belong only to the failed row; they neither disable the next draft nor replace its footer help.
The data source owns the action
fence and the idempotency key; the composer passes only `Send` or `Resend`. Cancel remains Unknown and disabled; no runtime signal is wired.
The kit's optional `onRetrySend` prop is a callback seam, not regeneration or a send implementation.

The app's node-rendering tests mock only the StyleX runtime, preserving semantic DOM and the folder style-key assertions without a global CSS compiler transform. Browser proofs use the real compiled styles. The shell geometry proof scans its own fixture entry and prebundles React with assistant-ui before first paint, keeping one runtime graph during roster population.

```sh
pnpm --dir apps/fractal-web exec vitest run src/web/ConversationPane.interest.integration.test.tsx src/web/composerSend.unit.test.ts src/web/ConversationPane.unit.test.tsx src/web/conversationTranscript.unit.test.ts
pnpm exec tsc --noEmit -p apps/fractal-web
node apps/fractal-web/scripts/composer-send-proof.mjs
node apps/fractal-web/scripts/composer-send-proof.mjs --without-binding
```

The live proof requires a send-granted gateway through the existing `WF_ST_GATEWAY` and
`WF_ST_AUTHORIZATION` environment variables and a disposable seat in `WF_E2E_SCRATCH_SEAT`. It
starts its own loopback server on an ephemeral port and permits message actions only to that seat.
It gates real HTTP/echo delivery to observe Pending→Sent→echo, injects one explicit refusal,
checks Retry reuses the key, and prints build revision, seat and timings. The negative control
removes the submit binding and proves the send criterion rejects it without issuing mail.
Both commands skip all live activity when the one-minute `/proc/loadavg` value is 32 or higher.

## Fractal web arrangement edits

The Sidebar first adopts the live reserved subject
`arrangement/<owner>/00000000-0000-7000-8000-000000000001`, regardless of its name.
Without it, the owner's lowest-UUIDv7 live legacy arrangement named exactly `Sidebar`
wins. Multiple legacy candidates are named in a non-blocking note; there is no chooser
or automatic consolidation.
`apps/fractal-web/src/folders/edit.ts` exports `sidebarOperations` and the memory-only
editor; the live `folders` atom exposes `edit(operations)` and `retryEdit()`. The kit can
bind its edit/DnD controls to that seam without translating through legacy folder stamps.
The Tree shows affected-row refusal feedback and an explicit retry control.

Folder creation, rename, move/reorder and deletion map to `folder.create`, `folder.rename`,
`folder.move` and `folder.delete`. Agent move/reorder/unfile maps to `subject.place`; the
caller supplies its canonical fractional key and can include required rekeys atomically.
Sidebar creation is managed internally: only a user's first edit without an adopted
Sidebar includes `create` with name `Sidebar`, targeting the reserved subject shared
by concurrent clients. Complete owner inventory reads precede submission; uncertain
creation replies replay the same action and subject, never a random replacement.
An acknowledged creation stays guarded until a complete inventory observes its subject.
A native rename preserves reserved identity. Retirement is terminal: once observed or
refused by the server, the session shows an empty Sidebar with unavailable Restore and
never sends another create. New sessions learn invisible retired subjects from admission
refusals because the current list exposes only live rows. `arrangement-exists` refreshes
and adopts a live Sidebar, but applying the refused edit requires explicit retry.
Renaming or retiring the arrangement itself is not offered by this folder editing seam.

Actions use the SDK HTTP `snapshot`/`submitAction` port and the generated action contract.
Optimistic registers appear synchronously and roll back to the newest complete inventory
on refusal. Typed daemon codes and complete envelopes are preserved; uncoded transport
failures, rejected acknowledgements without reasons and future codes stay explicitly
Unknown in structured data. A stale fence refetches snapshot and inventory but never silently retries.
Unknown delivery retries retain the original action identity, parameters and key.
An uncertain edit is refused rather than replayed when its original subject is no longer
the adopted Sidebar, including when the refreshed inventory has no winner.
An authoritative action refusal or rejected acknowledgement is definitely non-applied:
explicit retry prepares against the freshly adopted Sidebar, with a new identity/key if
the target or operations change. This includes retrying a refused reserved create against
a later-visible legacy Sidebar. A new explicit edit replaces a finished failed attempt,
but never silently replays its operations; every submission still refreshes its fence.
Snapshot refusal alone does not establish the delivery outcome of a previous action.
Complete native inventories retain the first page's snapshot frontier. Older responses
from the same host and projection cannot replace a newer accepted inventory, even when
a subscription read completes after the editor's post-submit read.
Concurrent editor/subscription inventory reads use generated clients bound permanently
to each read's AbortSignal. They share capability discovery, but refreshing a subscription
cannot abort an editor request or leave the abandoned subscription request uncancelled.
Refused request labels remain as ephemeral target-specific rows outside normal filtering,
with explicit retry, without reinserting rolled-back folders into the authoritative document.
Visible refusal feedback is one plain sentence with an actionable next step for known
arrangement errors, or “The change was not saved.” for other cases. Server detail and raw
codes never appear in that copy; `data-wf-refusal-reason` and `data-wf-refusal-code` retain
diagnostics for tests on both the affected-row and ephemeral-target feedback.
Native root/unfiled ordering uses placement keys and a deterministic subject tie-break;
subjects without placements follow keyed roots. Non-manual view sorts stay independent.
No folder-edit state is written to localStorage, IndexedDB or another client store.
`control.arrangements` must be granted separately from the arrangement read capability.

```sh
CI=1 pnpm --dir apps/fractal-web exec vitest run src/folders/edit.unit.test.ts src/folders/sidebar.unit.test.ts src/shell/AgentFolders.unit.test.tsx
CI=1 pnpm --dir apps/fractal-web exec vitest run src/folders/inventory-frontier.unit.test.ts src/folders/edit.lifecycle.unit.test.ts src/folders/root-order.unit.test.ts src/shell/AgentFolders.edit-refusal.unit.test.tsx
CI=1 pnpm --dir apps/fractal-web exec vitest run src/data/inventory-transport.unit.test.ts
pnpm --filter fractal-web typecheck
```

## Fractal web roster lifecycle

The generated client-v0 `Agent` schema models lifecycle as an optional open enum:
`standing`, `owner` and `bounded` decode as known literals; an unrecognized string decodes to
`{ _tag: 'Unknown', raw }` in the default tolerant mode. The roster uses plain generated
resource decoding, with no app-side stripping. Other field schemas remain enforced (for
example, invalid timestamps fail), while additional server fields and open-enum cases retain
generated forward compatibility. Known lifecycles project to `Standing`, `Owner` and `Bounded`
tags on `data/source.ts`'s `Agent.lifecycle`. Both an absent declaration and the generated
unknown case project to the app's `Unknown`,
including when the agent name sounds standing-like. Names, observed activity and runtime state
never supply lifecycle intent; unknown values never supply glyphs.

The sidebar receives this projected agent from `useFleet` and passes it to `sidebarRow`;
`Unknown` renders no glyph.

```sh
pnpm --dir apps/fractal-web exec vitest run src/data/projections.unit.test.ts
pnpm exec tsc --noEmit -p apps/fractal-web
```

## Fractal web terminal attachment

The header's **Toggle terminal drawer** stays disabled with an accessible description and
hover title until the live roster establishes a runtime for the selected agent. While
loading, its reason is “Waiting for the live agent roster to find this agent’s terminal.”
An absent roster row or an empty runtime list keeps it disabled with the specific cause.
An explicit terminal deep link still shows its local reason beside the usable thread.

`NO_TERMINAL=1 node apps/fractal-web/scripts/terminal-failure-e2e.mjs <baseUrl> <outDir> <seat>`
is a read-only proof for a terminal-less seat. It samples the page-level button's disabled,
title and `aria-describedby` attributes as soon as the control is visible, before waiting
for the transcript, and again after loading. The negative control is the earlier
roster-loading fallback that enabled a canonical terminal subject without runtime evidence:
the first-control assertion must fail even if the settled control is correctly disabled.

`data/liveSource.ts` resolves an agent's terminal runtime and passes the decoded runtime
and its snapshot ID as `FollowSpec.Terminal.initialRuntime` to `@st3/sdk/effect`.
The first attach consumes that observation rather than reading the same runtime twice.
Admission identity remains the runtime ID; the observation is not a runtime cache.
Reconnects and resyncs read the runtime afresh before obtaining another attachment.

The daemon still checks the incarnation fence; an older same-host snapshot is valid
([client-v0 terminal contract](st3/client-v0/README.md#terminal-protocol)).
Only a `stale-fence` attach refusal triggers one runtime re-read and one retry, with the
new terminal ID, incarnation and snapshot. A second refusal ends the follow, as do other
attach errors without a refresh. This never retries terminal input.

`src/data/liveSource.integration.test.ts` exercises the real live source, SDK and generated
client against a controlled gateway: one runtime read per happy-path attach, fresh reconnect
resolution, refreshed fences after a runtime replacement, and bounded refusal handling.
For the stale-fence negative control, replace the SDK retry's `readTerminalRuntime(runtimeRef)`
with `Effect.succeed(observation)`: the stale-attach test must fail because no fresh runtime
is read and the replacement terminal cannot be subscribed.

