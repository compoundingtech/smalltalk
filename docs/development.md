# Developing Smalltalk

## Contributing

Read the [product README](../README.md) and the guide for the behavior you are changing.
Use invented people, host names, and paths in this public repository. Run
`python3 scripts/check-public-repo` before opening a pull request.

For st3 claims, missions, replication, runtime ownership, or recovery, start with the
[documentation index](st3/README.md). For st2 lifecycle, messaging, teardown, or presence,
read and preserve the proofs in [st2 invariants](st2/invariants.md).

## Build from source

From a checkout, install with Nix:

```sh
nix profile install .
```

For local development, enter `nix develop`. The shell provides Rust, sccache,
cargo-nextest, and mold on Linux. Cargo uses mold for Linux links and the system
linker on macOS; dev and test builds keep line tables for workspace crates and
omit dependency debug info. On both platforms the shell sets `RUSTC_WRAPPER` to
sccache. Run tests with `cargo nextest run --workspace --locked`.
Outside the Nix shell, install mold on Linux and cargo-nextest separately; the
repository's `.cargo/config.toml` still selects mold for Linux builds.

Client terminal screens use `pty-terminal` and the same pinned `libghostty-vt` artifact as
the PTY runtime. Styled runs carry terminal-cell widths (including wide characters), soft-wrap
continuations, strikethrough and admitted OSC 8 links; keyboard modes include kitty flags.
The Nix package and developer shell link the shared static library through pkg-config, so
building Smalltalk does not require Zig or a Ghostty source checkout.
The runtime, screen projector and terminal UI pin their PTY protocol crates to the same
producer revision, keeping one shared protocol source in the workspace.

Nix vendors the PTY Cargo git dependencies from the existing `pty` flake input's
GitHub archive, not an anonymous Git checkout. Vendoring fails during evaluation
if the resolved PTY revision in `Cargo.lock` differs from the flake input, so update
the Cargo dependency pins and the `pty` input together. The input's locked NAR hash
also supplies the Cargo source hash. Any new Cargo git dependency needs an
archive-backed source before Nix can vendor it.

Without Nix, build and install from a checkout with a Rust toolchain and the matching
`libghostty-vt` artifact. Set `PKG_CONFIG_PATH` to its `share/pkgconfig` directory;
`pkg-config --static --libs libghostty-vt-static` must resolve before building:

```sh
scripts/install                  # into ~/.local/bin
scripts/install --bin-dir DIR    # or anywhere else
```

When pkg-config cannot find the artifact, Cargo builds `libghostty-vt` from the pinned Ghostty
source instead, which needs Zig 0.15.2 on `PATH`. On macOS, the vendored
[`libghostty-vt-sys`](../vendor/libghostty-vt-sys/README.md) build script lets Zig 0.15.2 link against
the macOS 26.5 and 27 SDKs.

The script builds and installs `st3`, `stui`, and `st3-migrate`, and makes `st` a symlink to
the installed `st3`. `st` is never a separate build. On macOS, both tools live in a fixed app bundle; see [macOS installation and signing](st3/macos-installation.md). A source install also needs [`pty`](https://github.com/compoundingtech/pty-rust) on `PATH`.

## Workspace dependency licenses

The root pnpm workspace includes the iOS app and TypeScript clients.
`buck2/dependencies/licenses.json` records every name/version in the lockfile's `packages`
set, not just the narrower Buck dependency closure. The inventory is generated, including
font licenses such as `MIT AND OFL-1.1`; do not edit its entries by hand.

Regenerate after changing the lockfile:

```sh
nix develop .#web -c pnpm install --frozen-lockfile
nix develop .#web -c python3 scripts/ci-fractal-web-licenses
```

The generator reads installed package manifests, including nested versions in the hoisted
install, using `license` and falling back to legacy `licenses`. Missing installed packages
are reported explicitly and resolved from their exact npm tarballs, verified against the
lockfile's SHA-512 integrity before reading `package.json`. This covers optional binaries
for other platforms without substituting registry metadata, hand-maintained exceptions or
`UNKNOWN`. Generation and full checks need network access for any missing packages.
Missing license declarations or conflicting installed copies fail closed.

Two CI guards use the same script:

```sh
# No node_modules or network: exact package-set coverage and lockfile/provenance drift.
python3 scripts/ci-fractal-web-licenses --check-lockfile
# After a frozen install: exact inventory content versus package manifests.
nix develop .#web -c python3 scripts/ci-fractal-web-licenses --check
```

`genie-freshness` runs the lock-only guard and the companion
`python3 scripts/ci-fractal-web-licenses-test`. The fractal-web execution lane runs the
full guard immediately after its frozen install. The generator/full guard restore the
inventory's read-only permissions (Git does not preserve them).

## fractal-web Content Security Policy

The app server and Vite's development/preview servers set an enforcing
`Content-Security-Policy` on HTML and assets, including HEAD and cached responses.
This is defence in depth: agent Markdown must not automatically fetch remote images,
and an accidental renderer regression must not reveal the reader's network address
or contact an agent-selected tracking endpoint.

The policy is:

```text
default-src 'self'; script-src 'self' 'wasm-unsafe-eval' <inline-script SHA-256 hashes>; style-src 'self' 'sha256-38RhXrc7EdReTKsOm23ZPOCUgniTUUcjky8QOOrQx6o=' 'sha256-gYiS/BvZvRcK27JIXTuwhZ3hs2+VJ1X+2gUlE+farlg='; style-src-attr 'unsafe-inline'; img-src 'self' data: blob:; font-src 'self'; connect-src 'self' ws://<page-authority> wss://<page-authority> <configured collector origins>; object-src 'none'; base-uri 'self'; frame-ancestors 'none'; form-action 'self'
```

`'wasm-unsafe-eval'` lets the terminal key encoder compile its WebAssembly module.
It does not allow `eval()` or string-to-code functions.

Hashes authorize only scripts in trusted application HTML: the early collections
connect, the server's deployment identity, and Vite's development React-refresh
preamble. There is no script `unsafe-inline` or `unsafe-eval` exception. Compiled
StyleX CSS and fonts are same-origin. Only `style-src-attr` needs `unsafe-inline`:
StyleX variable values, resizable widths and React Aria positioning are runtime
style attributes whose changing values cannot use fixed hashes. The two style
hashes authorize React Aria 3.52.1's fixed pressable touch-action and iOS modal
overscroll stylesheets (`usePress` and `usePreventScroll`); recheck them when
upgrading React Aria. Production and preview admit no other inline stylesheet
elements. Vite development additionally allows `style-src-elem 'self' 'unsafe-inline'`
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

## fractal-web dev-server console forwarding

The app's Vite config sets `server.forwardConsole: false`; browser diagnostics stay in DevTools.
Vite enables console forwarding by default when it detects an AI agent environment. Before
Vite 8.0.14 the forwarder sends every unhandled rejection to the dev server and leaves its own
failed send unhandled ([vitejs/vite#22407](https://github.com/vitejs/vite/issues/22407), fixed by
[#22450](https://github.com/vitejs/vite/pull/22450)). Once the HMR socket drops, one rejection
becomes an unbounded "send was called before connect" loop. The hoisted workspace Vite is 8.0.5.

```sh
CI=1 node apps/fractal-web/scripts/hmr-disconnect-proof.mjs
VITE_MODULE="$PWD/node_modules/vite/dist/node/index.js" CI=1 node apps/fractal-web/scripts/hmr-disconnect-proof.mjs --without-fix
```

The proof serves a temporary StyleX fixture with the app config, saves it four times (rename and
in place), refuses four compiled-stylesheet fetches with 410, closes the HMR socket and raises
one console error and one rejection. It passes with zero send-before-connect errors. The
negative control re-enables forwarding and must storm, so it needs an affected Vite.
`PLAYWRIGHT_MODULE` and `CHROMIUM_PATH` select the browser, as in the terminal proofs.

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
The current Tree has no folder menu or rename field; this change adds affected-row refusal
feedback and an explicit retry control, not a second DnD implementation.

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

The sidebar already receives this projected agent from `useFleet` and passes it to `sidebarRow`. Kit C can wire its future
row prop from that lifecycle field; `Unknown` must render no glyph. No kit prop is added here.
The temporary `trial: smalltalk#1991 client roster lifecycle` commit carries only the upstream
generated client/schema diff and should be dropped once #1991 merges into the base.

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

## Continuous integration

Workspace CI runs on pull requests and merge groups. The five required checks are `linux-gate`,
`isolation-vm`, `genie-freshness`, `typescript-client`, and `mail-redelivery-canaries`.
A ready pull request lands through GitHub's merge queue:

```sh
gh pr merge NUMBER --auto
```

The queue checks the exact commit that lands. Main upkeep verifies those checks, runs `perf-cost`,
and fills missing main caches. The primary Linux test job uses our self-hosted ci1 runners, with
Namespace as overflow for ordinary PRs. The second test shard and supporting jobs use Namespace. Trusted PRs labelled `ci-priority` and their queue entries use the reserved
`ci1-priority` lane; other merge groups use `ci1-merge`. Forks run on Namespace.
macOS CI is currently disabled; its retained workflow is not a required check.
See [CI operations](ci.md) for stage scope, routing, caches, and failure inspection.

## Shared UI models

[Build your own client](clients/build-your-own.md) maps the Rust and TypeScript pieces
and includes a small [example TUI](../examples/client-tui/) built and tested in CI.

`crates/st3-ui-model` provides renderer-independent UI semantics without a ratatui dependency.
Its broad name is intentional; initially it contains only stui's mission model (`Word`,
`StepState`, `Mission`, `Step`) and mission derivation. stui consumes that same model.
`missions::adapt` borrows typed mission and agent projections plus unresolved, actor-filtered
attention. Callers supply the current time and display policies explicitly; collection loading,
clocks and application naming remain outside the crate. Mission precedence, queue/keep-open
rules, outcomes and rich step details retain stui's existing behavior.
Adapted steps retain their stable step-run `id` and claimant-or-assignee `seat` (absent for
agentless steps), so consumers can select duplicate paths across open runs and navigate to
the actual execution seat without reconstructing identity from display labels.
`Mission.step_metadata` retains each selected step's raw `blocked_reason` and `last_progress`,
keyed by its step-run ID. These facts survive state changes independently of a step's waiting
blocker or derived queue `note`, and are not normalized by display text policies.
Shared widgets and other UI models are separate follow-up work, not part of this mission extraction.

## Embedding native conversations

`crates/st3-conversation-ui` provides the same native conversation presentation used by
`stui`, without terminal acquisition, application tabs or daemon connections. Feed
`st3-client` conversation frames into `Timeline::apply(Frame { replace, has_more, items })`,
adapt the timeline with caller-owned display names, and render it with `Cache::render`
and caller-supplied `Theme` tokens. The returned document contains styled lines and typed
`PaneIntent` targets. `State` retains scrolling, tool expansion, display-column selection
and composer drafts; send, open and older-history intents are executed by the embedding app.
Replacement frames remove the previous bounded window; incremental frames revise entries
by ID. Frame `has_more` is `Option<bool>`: an absent delta leaves the timeline's availability
unchanged, while an explicit value updates it. A replacement or changed session starts fresh.
The timeline keeps a boolean for the current window, and older pages track their own start.

The shared crate and `stui` use workspace Ratatui 0.30. An embedding application must align its
rendering dependency before passing buffers or lines across this boundary. Native conversations are not a terminal emulator: harness menus and arbitrary
permission prompts still require access to the harness terminal.

See the [conversation crate](../crates/st3-conversation-ui/README.md) for model-only and
Ratatui builds, and [build your own client](clients/build-your-own.md) for the client contract.

## Repository layout

| Path | Contents |
| --- | --- |
| `crates/st3` | Current daemon and CLI. |
| `crates/stui` | Terminal app. |
| `crates/st-drivers`, `crates/st-runtime` | Harness drivers and shared runtime. |
| `crates/st3-client`, `crates/st3-feed`, `crates/st3-schema`, `crates/st3-client-codegen` | Client API, feeds, schema, and code generation. |
| `crates/st3-ui-model`, `crates/st3-conversation-ui` | Shared mission and conversation presentation. |
| `clients`, `apps/ios` | TypeScript/Swift clients and the iOS app. |
| `crates/st3-migrate` | Migration tooling. |
| `src`, `tests` | Legacy st2 package and its integration tests. |
| `components`, `evals`, `fixtures` | Provider components, evals, and proof fixtures. |
| `examples/st3`, `docs` | Runnable declarations and documentation. |
| `nix`, `flake.nix`, `scripts`, `.github` | Packaging, developer tools, and generated CI workflows. |
