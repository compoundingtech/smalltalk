# Smalltalk client UI contract

For package choices and a runnable client, see [Build your own client](build-your-own.md).

What every Smalltalk client shows and does. stui (the terminal client, `crates/stui/src/ui`) is
the reference; the iOS app (`apps/ios`) implements the same contract the Expo way. When the two
disagree, fix the one that breaks this document, or change this document first.

The machine-checked half of the contract lives in `fixtures/clients/`. stui writes it
(`STUI_UPDATE_CONTRACT=1 cargo test -p stui contract`) and fails its tests when it drifts; the
iOS app must test against the same files.

| File | What it pins |
|---|---|
| `demo-world.json` | The invented fleet both demo modes show. Serialized `ui::view::World`. |
| `theme.json` | Colour tokens (Catppuccin Mocha) and the one colour rule. |
| `words.json` | Mission words and explanations, agent states, Home tiers, tab names, glyphs. |
| `transcripts/*.json` | Real Claude and Codex timelines, scrubbed of private names. |
| `transcripts/*.expected.json` | The conversation a client must derive from each (display times omitted). |

## Definition of good

Every screen is judged by these.

1. **Never claim what you don't know.** Loading, empty and failed are three different states. A
   list that has not loaded says "Loading…", never "No items". A preview that cannot be read says
   why, and offers no approval.
2. **The conversation is the product.** One stream per agent, rendered like pi: the person's
   messages as tinted blocks, replies as real Markdown, tool calls as small boxes tinted by
   outcome and collapsed to their last lines, Smalltalk mail in the same stream. It is stable:
   a refresh that changes nothing moves nothing, new content only appends, and the view follows
   the end only while the reader is at the end.
3. **Everything visible is actionable.** Every tab, row, name and button responds to a tap.
4. **Quiet chrome.** Named colour tokens, few borders, selection as a background fill, glyphs
   with a legend instead of words like "reachable".
5. **Stable identity and order.** Rows show a name over the graph path. Ordering rules are
   visible (group headings) and a selection stays on the same item across refreshes.
6. **Show the thing being decided.** A card shows the mission it would launch, the draft it
   wants feedback on, the question it asks — never just an id.
7. **No internal jargon.** No raw ids where a name exists, no harness markup, no "undeclared
   import requires an exact native session ID".
8. **Every kind of attention has its own card.**
9. **Mission control makes the fleet understandable:** who must act on each mission, and why.

Mauve (`person`) means a person is needed. Nothing else may use it.

### Web client cold selection

The selected conversation owns its cold first-page request. Background follows require
explicit pointer or keyboard-focus intent, and are admitted only after every visible
conversation has committed a native first page through the frame writer. Reading a
hidden snapshot does not acquire a follow. Observations survive warm switches in memory;
reloads do not restore cached roster or transcript claims from browser storage.

### Native web data ports

Roster usage, requested checkout, workspace, activity, blocking reason and ask are
`Known(value)` or `Unknown`; missing facts are never fabricated as zero. Session start
and end remain unknown until the native roster supplies them. Native content search
reads server conversation content rather than scanning the loaded window. Attachment
upload, chunk reads and message sending independently honor their native grants and
preserve refusal envelopes. Upload alone never acknowledges a send; the gateway chooses
the canonical message identity independently of the client's idempotency key.

Only an actual replacement page with explicit pagination evidence establishes an empty
conversation. Newer deltas do not change the older-history boundary. `hasOlder` reports
that boundary, not an available fetch operation.

### Web session trace providers

`src/data/sessionTrace.ts` defines the Effect `SessionTraceProvider` service and the
`TraceSeries` schema. Queries carry a native session ID, range (`1d`, `7d`, `30d`)
and bucket (`5m`, `15m`, `1h`, `1d`). Native `UsageRow.native_session_id` is the
root-session join key for an independently injected provider, including its subagents.
The public schema contains no deployment or provider-specific identifiers.

Every metric is `Known(value)` or `Unknown(reason)`; buckets contain time, input/output
and cache tokens, cost with currency, and model, with a separate subagent breakdown.
Responses distinguish `self` from `including_subagents`, carry an explicit `partial`
flag and observation freshness. An observed read is not a claim of continuous liveness.

The live source installs the st-only layer. It reuses native usage rows and the L3
agent reader: roster meters are labelled `agent-incarnations`, exclude subagents and
are always partial. These cumulative meters are neither the requested range's totals
nor the selected root's totals. Missing values never become zero, and unavailable
series/subagent coverage remains `Unknown('no-provider')`, not an empty series.
Consumers use `sessionTrace(query)`; another Effect layer can provide
`SessionTraceProvider` via `Layer.provide` without changing consumer code.

### Web follow budgets and synchronization

Conversation follows have a bounded LRU lane. The advertised subscription budget reserves
four shared slots for the three standing windows and a terminal; older capability sets
use the conservative budget. Invisible warm follows continue folding until eviction,
which unsubscribes them; selecting an evicted conversation starts a new follow.

The SDK publishes `SyncStatus` v2 for each follow and the gateway. `Requested` means the
subscribe was sent, and `Live` requires decoded protocol data, not merely an open socket.
Reconnect, resync, eviction and failure carry only observed evidence. The data layer
retains trusted content through failed reads but removes authorization-revoked rows.
Terminal screens also lose display authority when their dependent roster or replacement
runtime lookup is refused. A Live window status refreshes its native snapshot evidence
for every observed change while preserving the original Live transition clock.
Status consumers share their follow's lifetime, and last content never manufactures Live.

## Header

The header's right side names the host and person, led by the connection: `● live`, a spinner
while connecting, `○ offline`, or a red `⚠ diverged` while the host's pages carry a `sync` notice
in the `diverged` state (`World.diverged` lists those peers). A diverged host holds the same
envelopes as a peer but projects a different graph from them, so what the client shows can be
wrong until the host is repaired; exchanges cannot fix it.

## Tabs

Home, Agents, Missions, Fleet. A Worktrees tab waits, hidden, until the graph has worktree
resources: until then it could only show invented data.

### Home: what needs you

- Grouped by tier (`words.json` → `tier`): somebody is stopped on you, something broke, today,
  when there is time. The tab badge counts every open item and turns mauve when someone is
  stopped on you. A footer counts missions that don't need you.
- Each item: glyph and kind word, title, "who is waiting · waited N".
- Every card shows links to its mission and agent, a **related** section (who raised it, when
  st says, and each target as a link), **Chat about this**, and **Go to** its mission or agent.
- Cards per kind:
  - **review** (human gate): question, because, what to look at, step; Approve, or Request
    changes with a text box the agent reads (sent as the reject reason).
  - **feedback**: the draft excerpt and a feedback box. (Needs the feedback gate mode on the
    daemon; the demo shows it.)
  - **launch**: the proposed mission — goals, a step flow (`scan → fix → {review, lint} →
    merge`), each step's assignee, the agents with harness and host, and how many times it will
    stop to ask you. If st has no previewable candidate, say why and offer only Ask the planner
    and Cancel.
  - **revision**: the reason and the plan changes as `+`/`~`/`-` lines; Approve, Ask for
    changes (a message to the proposing agent until st takes a reason), Reject.
  - **fault / agent request**: what, because, source when it is not the item itself, a
    suggested fix when st gives one; Mark resolved.
  - **message**: the real message (load it by the item's source id: the attention item carries
    only a generic title); Reply, Mark read, Remind me later (demo: kept on the device only).
- Confirm destructive or binding actions (approve, cancel, reject, resolve) with a second tap.
- **Chat about this** sends a *new* Smalltalk message to the agent involved (or the chief of
  staff when no agent is), titled `About: <item title>`, with the item's id, title and mission
  as context. The card shows that thread's replies.

### Agents

- Grouped view (default): waiting on you, broken, working, idle, stopped, found running (not
  started by st). Each row: state glyph, name, harness, last activity; second line the graph
  path. A legend explains every glyph.
- Tree view (toggle): the graph path as folders, one line per agent.
- In both views, the subagents an agent's harness runs now hang beneath its row, one line each:
  what it does, its type, and how long it has run. They are part of the agent's row: selecting
  or clicking one selects the agent, and they have no actions of their own.
- Selecting an agent shows its conversation (see below) and a composer. A details panel shows
  what it holds now (mission › step, the step's goal, since when), its subagents, what is queued
  next, and how it runs (harness and state, runtime, host, worktree, parent, fault).
- An agent on another host may show only its Smalltalk mail, not its transcript; say so.
- An agent not started by st: show its saved transcript when st can identify it; otherwise
  explain in plain words why there is no conversation. A found process whose native session
  is unidentified opens into this explanatory state, not a missing-session error. OMP internal
  `__omp_worker_*` helpers are not agents and must not appear in the found-running group.

### Missions

- Grouped by the mission word (`words.json` → `mission`), in this order: needs you, stalled,
  unstaffed, unclaimed, queued, working, watching, held, idle, done, failed. Each group heading
  explains itself through the word's `explain` text in the detail view.
- Tree view (toggle): the mission path as folders (`fleet/smalltalk/ci/watch/…`).
- Detail: word pill, title, explanation; goals; **the decision card embedded** when a person is
  needed (the same card as Home, answerable in place); a **what you can do** card for stalled,
  failed and unstaffed missions (the stuck step, its blockers, restart the agent, retry the step,
  cancel the run, chat with it) and a **nothing for you to do** card for queued ones; the steps
  as a flow plus rows that expand to goals, constraints, gates, blockers and attempts; the
  agents; the worktree; the whole declaration when st provides it.
- Words, from the live graph:
  - **watching**: the only running steps are st's own keep-open steps (`keep-watch`, `retire`,
    `steward-intake`, `standing`, `keep-open`, `*-retirement`); the mission's observers start
    other missions. Not work, costs nothing.
  - **queued**: a ready step sits in a busy agent's queue; name what it waits behind.
  - **unstaffed**: a ready step is queued for an agent that is stopped or broken.
  - **unclaimed**: a ready step and st does not say who takes it.
  - Agentless steps shown with owner "st", never "Agentless step".

### Fleet

Machines with online state, platform, last seen, the peers each reaches, and the agents on it.
The person's own paired devices are listed here too (name, scope, last seen), each revocable
after a confirmation (`pairing.revoke`).

### Terminals

An agent with a live terminal can be opened in place from its conversation: the screen streams
with its colours and styles (never polled), follows the terminal's size, and says so when the
terminal restarts. With the `terminal.input` capability the person can send a line and the keys
Enter, Tab, Esc, Up, Down, and a confirmed Ctrl-C; every input takes a fresh terminal fence and
refuses a changed runtime incarnation. Leaving the view detaches. stui: Enter on an agent opens
it and Ctrl+\ returns; iOS: a Terminal button on the agent screen.

### Starting a mission

Missions has **New mission**: the person writes what they want (and picks a workspace), and the
client creates a launch. The planner's proposal then appears on Home as a launch card. Nothing
runs until the person approves it there.

## Conversations

Rules both clients follow; `transcripts/*.expected.json` checks the cleaning.

- Merge the harness timeline and the agent's Smalltalk messages, ordered by time.
- Claude and Codex add markup for the model. Turn it into what it means:
  - `<task-notification>` → one event line: `background task <status>: <summary>`.
  - `<channel …>` (an st delivery) → `delivered to the agent: <subject> · from <sender>`; the
    message itself is already in the stream as mail.
  - `<command-name>`/`<command-args>` → the command the person typed; `<local-command-stdout>`
    → a tool box.
  - Codex `<send_user_message_question_reply>` → the person's answers as their message;
    `<turn_aborted>` → "the turn was interrupted".
  - Drop `<system-reminder>`, `<local-command-caveat>`, Codex `<environment_context>`,
    `<permissions …>`, `<collaboration_mode>`, `<multi_agent_mode>`, `*_instructions` blocks,
    including blocks that never close.
  - Leave assistant prose alone: an agent may mention `<smalltalk-message>` by name.
- "Mission step ready" pings from `daemon/runtime` render as one event line.
- Message headers use names ("you", "st", agent names), not graph ids.
- Tool calls take their results by call id; a failed result tints the box red.

### Sending

- Show a sent message at once, dim, marked "sending…". When st accepts it, keep the returned
  `message/…` id (the action result's `affected_ids`). When that id appears in the
  conversation's messages, drop the pending copy. If the send fails, show it red with the reason.
- Read a fresh snapshot right before every action and retry once on `StaleFence`: a graph that
  moved is not the person's problem.
- After a send, refresh that conversation every few seconds for two minutes, until live
  conversation updates exist (`fleet/smalltalk/live-conversations`).

## iOS, the Expo way

Same capabilities, native idioms:

| stui | iOS |
|---|---|
| Tabs `1`–`5` | Bottom tab bar with the same five tabs and the Home badge |
| Sidebar list + detail pane | List screen pushing a detail screen (stack navigation); iPad may use a split view |
| Popover on a name | A sheet with the same summary, **Go to** and **Message** |
| Keys on a card (`a`, `c`, `t`, `g`) | Buttons on the card; swipe actions on list rows for the common ones |
| `y` to confirm | A confirmation alert or a second tap on the armed button |
| `t` tree toggle | A segmented control above the list: Groups / Tree |
| `i` details pane | A details section or sheet from the agent screen's header button |
| Wheel, follow the end, "↓ N new lines" | Scroll view pinned to the end while at the end; a floating "N new" chip |
| Drag to select and copy | Native text selection on messages |
| Composer grows to eight lines | Growing text input above the keyboard |
| Loading / empty / failed | The same three states, never an empty list while loading |
| Enter on an agent: its terminal | Terminal button on the agent screen; keys as buttons |
| `n` on Missions: new mission | A New mission button opening a composer sheet |

The demo mode (invented data, nothing sent) must exist on iOS too, reading `demo-world.json`.

## What the graph still lacks

`docs/stui/graph-gaps.md` lists it; clients show an honest "st does not say" until then.

## Next reuse step: derive the view once, in st

Today each client derives the view from raw projections: who must act on a mission, an agent's
state, a step's queue position, a cleaned conversation, an attention card. stui does it in Rust
(`ui/adapt.rs`) and the iOS app will do it again in TypeScript, held together only by the shared
fixtures. The larger saving is to move that derivation into st's client API: a presentation
projection per mission (its word, the stuck step and why, what a person can do), per agent (state,
now, next) and per session (the cleaned, merged conversation, streamed once live conversations
land). Clients would then only draw, and the fixtures above become the daemon's golden tests.
