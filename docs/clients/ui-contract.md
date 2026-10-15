# Smalltalk client UI contract

For package choices and a runnable client, see [Build your own client](build-your-own.md).

What every Smalltalk client shows and does. Three clients implement it: the terminal UI
(`crates/stui/src/ui`), the iOS app (`apps/ios`, the Expo way) and the web app, Fractal
(`apps/fractal-web`). Each part of the contract has one lead:

- **Screens and navigation:** the terminal UI is the reference.
- **The conversation view:** the web app leads (see
  [Who leads the conversation](#who-leads-the-conversation)).
- **Mission, agent and attention state:** st computes it once and serves it as
  [presentation projections](#presentation-projections). Clients read the projections and keep
  no derivation of their own.

When a client disagrees with this document, fix the client, or change this document first.

The machine-checked half of the contract lives in `fixtures/clients/`. The terminal UI writes
most of it (`STUI_UPDATE_CONTRACT=1 cargo test -p stui contract`) and fails its tests when it
drifts. The iOS app tests `conversation-style.json`, the device-signing and pairing vectors, and
the shared transcripts through `@smalltalk/st3-views`. The transcripts are the conversation
view's contract; the web app updates them (see below).

| File | What it pins |
|---|---|
| `demo-world.json` | The invented terminal demo fleet. Serialized `ui::view::World`. |
| `theme.json` | Colour tokens (Catppuccin Mocha) and the one colour rule. |
| `words.json` | Mission words and explanations, agent states, Now tiers, tab names, glyphs. |
| `conversation-style.json` | Shared conversation presentation rules. |
| `agent-launch.json` | Agent launch defaults and request shape. |
| `arrangements-v1.json` | Arrangement structure and placement operations. |
| `device-signing-v1.json` | Signed device request vectors. |
| `device-pairing-proofs-v1.json` | Pairing proof and fingerprint vectors. |
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

`apps/fractal-web/src/data/sessionTrace.ts` defines the Effect `SessionTraceProvider` service and the
`TraceSeries` schema. Queries carry a native session ID, range (`1d`, `7d`, `30d`)
and bucket (`5m`, `15m`, `1h`, `1d`). Native `UsageRow.native_session_id` is the
root-session join key for an independently injected provider, including its subagents.
The public schema contains no deployment or provider-specific identifiers.

Every metric is `Known(value)` or `Unknown(reason)`; buckets contain time, input/output
and cache tokens, cost with currency, and model, with a separate subagent breakdown.
Responses distinguish `self` from `including_subagents`, carry an explicit `partial`
flag and observation freshness. An observed read is not a claim of continuous liveness.

The live source installs the st-only layer. It reuses native usage rows and the L3
agent reader: self-only roster spend meters are labelled `agent-incarnations` and
always partial. Native aggregate totals can include subagent responses and do not
establish self-only coverage across historical incarnations. The default therefore
keeps spend `Unknown('not-attributed')` until accounting evidence establishes that
coverage; a current driver or a root join alone is not sufficient. These cumulative
meters are neither the requested range's totals nor the selected root's totals.
Missing values never become zero, and unavailable
series/subagent coverage remains `Unknown('no-provider')`, not an empty series.
Context-only native summaries leave spend unknown; observed context meters remain
available. Consumers call the live source's `sessionTrace(query)` port, bound to its
owned runtime, or use the same-named Effect operation inside a provided context.
Another Effect layer can provide `SessionTraceProvider` via `Layer.provide`, or be
passed as `liveSource({ sessionTraceLayer, options })`, without changing consumer code.

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

## Navigation

The terminal UI's sidebar and palette expose Now, Agents, Missions, Fleet, and Usage.
The phone tabs are Now, Resources, Fleet, and Spaces.

### Now: what needs you

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
  - **message** (demo only; a live unread message stays in its conversation and has no tier):
    the real message (load it by the item's source id: the attention item carries only a
    generic title); Reply, Mark read, Remind me later (kept on the device only).
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
  unstaffed, unclaimed, queued, working, watching, held, idle, done, failed, cancelled, not started. Each group heading
  explains itself through the word's `explain` text in the detail view.
- Tree view (toggle): the mission path as folders (`fleet/smalltalk/ci/watch/…`).
- Detail: word pill, title, explanation; goals; **the decision card embedded** when a person is
  needed (the same card as Now, answerable in place); a **what you can do** card for stalled,
  failed and unstaffed missions (the stuck step, its blockers, restart the agent, retry the step,
  cancel the run, chat with it) and a **nothing for you to do** card for queued ones; the steps
  as a flow plus rows that expand to goals, constraints, gates, blockers and attempts; the
  agents; the worktree; the whole declaration when st provides it.
- Words, from the live graph:
  - **watching**: the only running steps are st's own keep-open steps (see
    [Graph flags](#graph-flags)); the mission's observers start other missions. Not work, costs
    nothing.
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
refuses a changed runtime incarnation. Leaving the view detaches. Terminal UI: Ctrl+] on an agent's conversation attaches its terminal
and Ctrl+\ returns; iOS: a Terminal button on the agent screen.

### Starting a mission

Missions has **New mission**: the person writes what they want (and picks a workspace), and the
client creates a launch. The planner's proposal then appears on Now as a launch card. Nothing
runs until the person approves it there.

## Conversations

### Who leads the conversation

The web app is the most capable display, so it leads the conversation view:

1. **New conversation behavior starts in Fractal.** A new kind of entry, a new cleaning rule or
   a new way of grouping turns is designed and shipped in the web app first.
2. **The shared transcript fixtures are the contract.** `fixtures/clients/transcripts/*.json`
   holds the timelines and `*.expected.json` holds the conversation a client must derive from
   each. A PR that changes how a conversation reads updates those fixtures in the same PR, adding
   a scrubbed timeline when no existing one shows the change. A behavior change without a fixture
   change is not done.
3. **Every derivation runs the fixtures.** Rust runs them in `crates/st3-conversation-ui` and
   `crates/stui` (`contract`), the phone through `@smalltalk/st3-views`
   (`tests/transcripts.test.mjs`), and Fractal in its own unit tests, either by importing
   `st3-views` or by running the fixtures through its own derivation
   (`apps/fractal-web/src/conversation/*.unit.test.ts`). A fixture change fails every derivation
   that does not follow it yet.
4. **The terminal UI and the phone follow as best they can.** They take the fixture change when
   they can draw it. When one cannot yet, its PR or a follow-up says which expected entries it
   skips and why. The fixtures stay as Fractal wrote them.

Today only Rust and the phone run the fixtures; `noise` and `claude-skill` run only in Rust,
and only the phone runs `conversation-style.json`. Fractal cleans conversations in
`apps/fractal-web/src/conversation/` without `st3-views` and runs none of them. Adding that
test is the first step of this rule
([#2647](https://github.com/compoundingtech/smalltalk/issues/2647)).
Once the [conversation projection](#conversation-stream) is served, it is the derivation the
fixtures pin, and a Fractal PR that changes the fixtures changes the projection with them.

### Rules

Rules every client follows; `transcripts/*.expected.json` checks the cleaning.

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

## Presentation projections

Today the terminal UI derives mission words, agent states and Now cards in Rust, the phone
derives its own copies in TypeScript, and the web app would need a third. The copies already
disagree. st serves each of these once, as a presentation projection, so every client reads the
same answer and one place is measured and optimized
([#2646](https://github.com/compoundingtech/smalltalk/issues/2646)).

### Rules for every projection

- **On a published view, never derived on read.** A projection is a `presentation` object on the
  rows of a collection a background refresher already publishes (`missions`, `agents`,
  `attention`), and on that collection's HTTP list and get reads, which answer from the same
  publication. The refresher computes it as it folds the row, from the row's own cut. No read
  computes one, with one exception: the agent presentation reads the clock-dependent facts that
  the roster's overlay settles on each read, so it is computed with them (see [Agent](#agent)). Before a list's first publication, its windows get a retryable `resync` and wait,
  as work windows do; the missions list adopts that rule when its projection ships. Serving them
  on existing rows adds no subscriptions to a socket's sixteen.
- **One derivation, in Rust.** The refresher calls `st3-ui-model`, which the summary already
  uses for mission counts. Each client deletes its own derivation when it moves onto the
  projection.
- **Refold when any input changes.** Each projection below lists its inputs. A change to any of
  them refolds the affected rows, not only a change to the row's own claims.
- **Vocabulary from `words.json`.** Words are served as their `fixtures/clients/words.json` ids
  (`decision`, `needs_you`, `stopped`, …). Clients take each word's name, glyph and explanation
  from that file. A daemon test fails when the daemon serves an id that is missing from it.
- **Names, not ids.** Every subject comes with its display `label`.
- **Times, not ages.** A projection carries timestamps. The reader's clock formats "waited 3h"
  and decides "before today".
- **Unknown is not a value.** A fact the refresher cannot establish is `null`, with
  `unknown_reason` beside it. It is never a default.
- **Optional on the wire.** `presentation` is optional in the schema. A daemon that serves it
  advertises the `presentation` capability, version 1. A row without it, from a daemon without
  the capability, shows the row's raw `state` and says this host's st is older. A client does
  not keep a derivation as a fallback. The terminal UI reads each row's own field, so a row st
  could not present (the daemon logs it) reads the same way.

**SLOs.** The first frame of each projection's stream, and its list and get reads, are
`person-read` paths: p99 100 ms, or 300 ms from another machine. `stream missions`,
`GET /v1/client/missions` and `GET /v1/client/missions/{*id}` join that target when the mission
projection ships; agents and attention are already in it. A new `projection-fresh` target
covers the time from the commit that changes an input to the publication that carries it:
p99 1500 ms, since refreshers fold at most once a second. Its paths are `publish missions`,
`publish agents` and `publish attention`, timed by the refresher. Both targets appear on
`GET /v1/client/request-latency` and in `st doctor`. `daemon_load` keeps its 300 ms
first-snapshot budget for every window, now with projections on its rows, and stays under
twice main's baseline. Bounds per row: a mission projection stays within 4 KiB, an agent
projection within 2 KiB, and an attention projection within 16 KiB including its launch preview.

### Mission

Served on the `missions` collection as `Mission.presentation`, on `GET /v1/client/missions` and
on `GET /v1/client/missions/{*id}`. The summary's mission counts read the same projection.

| Field | Meaning |
|---|---|
| `word` | The mission word: one of the thirteen `mission` ids in `words.json`. |
| `system` | The mission is st's own or a standing observer (see [Graph flags](#graph-flags)). |
| `who` | Who must act now: `{kind, agent, label}`. `kind` is `you`, `agent`, `st` or `nobody`. |
| `steps[]` | The steps of the open runs, or of the latest run when none is open, in display order: `{id, path, title, word, owner, note, attempts, blockers[], keep_open}`. The step `word` is one of `needs_you`, `done`, `working`, `ready`, `waiting`, `cancelled`, `failed` or `pending`. `owner` is `{kind, agent, label}`, as in `who`. |
| `stuck` | For stalled, failed, unstaffed and queued missions: `{step, word, attempts, blockers[], agent}`. The step is the first one that is failed, ready or waiting. `agent` is that step's seat (its claimant, else the agent queuing it) as `{id, label, state}`. Otherwise `null`. |
| `queued_behind` | For a ready step waiting on a busy agent: `{agent, label, busy_with}`, where `busy_with` is the `{mission, path, label}` it holds. When the agent is not busy, `state` replaces `busy_with`. Otherwise `null`. |
| `card` | `what-you-can-do` (stalled, failed, unstaffed), `nothing-to-do` (queued), or `null`. |
| `actions[]` | What a person can do: `{id, action, target, confirm}`. `id` is `retry-step` (`work.retry`), `restart-agent` (`agent.start`, or `runtime.restart` for a running seat), `cancel-run` (`mission.cancel`, `confirm: true`) or `chat` (`message.send` to the stuck agent). Clients run these actions instead of printing CLI hints. |
| `progress`, `last_progress`, `settled_at` | `{done, total}`; the latest progress summary of a step in hand; and the time it completed, failed or was cancelled (else `null`). |

Precedence is the one in `st3-ui-model::missions::adapt` today, first match wins: an open human
gate on the mission (decision); no runs (not_started); a blocked mission or step (stalled);
completed (done); a failed mission or step (failed); cancelled; only keep-open steps still
running (watching); a step claimed, working or verifying (working); a ready unclaimed step,
which is unstaffed when its queuing agent is stopped, failed or faulted, queued when an agent
queues it, and unclaimed otherwise; a waiting or pending step (held); standing, running, ready or
draft (idle); otherwise done.

`who` is `you` for decision; the stuck or working step's seat (its claimant, else the agent
queuing it) for working, queued, unstaffed, stalled and failed; `st` when only agentless steps
are open; and `nobody` otherwise.

The existing `must_act` (`you`, `agent`, `system`, `blocked`, `nobody`) stays on the row for
older clients. `who` replaces it for display.

**Inputs:** the mission's runs, steps and worker leases; open human-gate attention on it; and the
state, fault and queue of every agent that claims or queues one of its steps. The missions
refresher refolds on the first two today, and must also refold when one of those agents changes.

**Today, and what the projection settles:**

| | Derivation | Differs from this contract |
|---|---|---|
| Terminal UI | `crates/st3-ui-model/src/missions.rs` (496 lines); `crates/stui/src/ui/screens.rs` `display_rank` and `what_you_can_do` | Sorts working before needs you. Finds the stuck agent only among agents already working on the mission, so an unstaffed mission's idle agent never appears. Retry and restart print CLI hints. |
| Phone | `apps/ios/missionsView.ts` (163 lines) and parts of `presentation.ts` | Calls decision "alert", has no `not_started`, reads every run's steps, has no who-must-act, stuck step or actions. |
| Fractal | None live; the unmounted `shell/Sidebar.tsx` groups by `must_act` | Nothing to delete; reads the projection when it adds Missions. |

Settled: the word for decision is "needs you"; `not_started` is a word; steps come from the open
runs, else the latest run; missions list in `words.json` order, which is the order in
[Missions](#missions); the stuck agent is the stuck step's seat. Hiding by default (system
missions, not started, and done, failed or cancelled before the reader's today) stays in the
client, applied to `system`, `word` and `settled_at`.

### Agent

Served on the `agents` collection as `Agent.presentation`, and on `GET /v1/client/agents` and
`GET /v1/client/agents/{*id}`. `st3-ui-model::agents::present` computes it from the row alone.

| Field | Meaning |
|---|---|
| `state` | One of the eleven `agent` ids in `words.json`: `needs_you`, `needs_login`, `fault`, `working`, `compacting`, `idle`, `starting`, `down`, `stopped`, `retired`, `unknown`. A retired agent is `retired` whatever else st says of it. |
| `group` | The list group: `needs_you`, `needs_login`, `fault` (fault, down), `working` (working, compacting), `idle` (idle, starting), `stopped` (stopped, unknown) or `retired`. Sessions st found running but did not start are not agent rows; clients list them in their own "found running" group. |
| `because` | Why it needs someone, in plain words: the fault or why it is broken, when it lost its session, the ask, or the login it needs, for `fault`, `down`, `needs_you` and `needs_login`; otherwise `null`. |
| `now` | What it holds: `{step, mission, path, title, label, goal, since, holder_down_since}`, or `null`. `label` is `mission › path`. The step's progress is on the mission projection's step. |
| `next[]`, `queued` | Up to five queued steps as `{step, mission, path, title, label}`, next first, and how many are queued in all. |

Precedence, first match wins: `fault` when a fault is set, delivery is stale or the runtime
failed; `down`; `needs_login`; `compacting`; `working`; `idle` while running; `needs_you` when
waiting on a person (`blocked_on: human`); `fault` when blocked on its channel; `needs_you` when
blocked otherwise; `idle` when the harness is indeterminate but the seat is reachable; `starting`;
`stopped`; `unknown`.

The row's freshness facts (`observation`, `down`, `delivery`) depend on the reader's clock, so st
settles them in a light overlay on each read of the published roster. The presentation reads
those facts, so the same overlay computes it right after them, from that one row. Nothing else is
read or computed.

**Inputs:** the agent row's runtime, harness observation, delivery, fault and queue, which the
roster refresher already folds.

| | Derivation | Differs from this contract |
|---|---|---|
| Terminal UI | Reads `presentation.state` and `because` (`crates/stui/src/ui/adapt.rs`); its derivation moved to `st3-ui-model::agents` | Its labels still use mission titles from the missions window. |
| Phone | `apps/ios/agentsView.ts` `agentState` (file 294 lines) | Ignores `blocked_on`, so every blocked seat reads as needs you; calls needs you "alert"; shows no next. |
| Fractal | `apps/fractal-web/src/data/projections.ts` `activityOf`, `statusOf`; `web/sidebarRow.ts` | Its own status line and activity words; no shared states. |

### Attention row

Served on the `attention` collection (also subscribable as `alerts`) as
`Attention.presentation`, and on `GET /v1/client/attention`, `GET /v1/client/attention/{*id}` and
`GET /v1/client/now`. The row's existing `alert` stays the one alert flag; clients read it and
never infer it.

| Field | Meaning |
|---|---|
| `tier` | The Now group: `stopped`, `alert`, `today`, `later` (`words.json` `tier`), or `null` for a row that is not on Now (an unread message stays in its conversation; a fault goes to its agent). `null` means not on Now, never "unknown". No card is in `alert` yet. |
| `kind` | The card: `review`, `feedback`, `request`, `update`, `launch`, `revision`, `prompt`, `login` or `fault`, or `null` for a row that is no card at all (an unread message). |
| `agent` | Who **Chat about this** reaches, `{id, label}`: the conversation, else the step's claimant, else an agent working in the mission (the first in the agents roster's order, when several are), else the requester. `null` means the chief of staff. |
| `raised_by`, `waiting` | Who raised it, `{id, label}`: the requester, else the agent behind it, with st's own actors (`daemon/…`, `agent/st3/reconciler`) labeled "st", else "the … mission" it came from; and the step it holds, `{step, path}`. A person's label is their id. |
| `launch` | On a launch card: the newest variant's mission as `{variant, goals[], steps[], agents[], asks_you}`. Each step is `{path, title, assignee, after[], asks_you}`, in display order without `finally` steps. `assignee` is `{kind, agent, label}`, with `kind` one of `agent`, `any`, `st` or `none`. Each agent is `{id, label, harness, host}`. `asks_you` counts the steps that stop for a person. With no previewable variant it is `{unavailable: {reason, diagnostics[]}}`, with up to three diagnostics. |
| `revision` | On a revision card: the changes (see [Revision changes](#revision-changes)). |
| `prompt` | On a harness-prompt card: `{calls[], answers[], allow}`. Each call is a tool call without a result in the seat's conversation: `{call_id, tool, input, input_complete}`, at most five, each input within 4 KiB. `answers` come from `action_parameters["prompt.respond"]`. `allow` is true only when exactly one call is pending and its input is complete. When st cannot read the timeline, `calls` is `null` with `unknown_reason` and `allow` is false: the card says to answer in the terminal. |

Kinds come from `attention_kind`: a human gate is `stopped` · `review` (`feedback` in feedback
mode); a person step or agent request is `stopped` · `request`, or `later` · `update` when it
carries an update; a launch approval is `today` · `launch`; a revision approval is `today` ·
`revision`; a `custom.*` kind is `today` · `request`; a harness prompt is `stopped` · `prompt`;
a harness login is `stopped` · `login`. A fault is a `fault` card with no tier: st sends each
fault to the agent that owns it, and neither client shows one on Now. An unread message has
neither.

An agent's label is its name's last segment in title case, without its own host's prefix
(`st3_ui_model::agents::label`, which the terminal UI also uses); an agent st does not know reads
as its id without `agent/`. The refresher presents each row when it folds the list, and again
when the agents roster it reads changes; while no roster is available it keeps the agents it
last read. Every route copies the row's presentation from that list. A `history` read returns the
same open rows (st keeps no resolved attention rows), so it carries the same presentations.

**Inputs:** the attention row; for a launch, its newest variant; for a prompt, the seat's
conversation timeline at the observation; for `agent`, the step's claimant and the mission's
agents.

| | Derivation | Differs from this contract |
|---|---|---|
| Terminal UI | Reads `tier`, `kind`, `agent`, `raised_by` and `waiting` (`crates/stui/src/ui/adapt.rs` `attention()`); a row without a presentation (an older st) shows st's text where its `alert` flag puts it. Still its own: `preview()`, `alerts.rs` `unanswered_calls`, the variants fetch in `live.rs` | Fills launch agents' harness and host as unknown. Withholds allow when a call runs past four lines, instead of when its input is incomplete. |
| Phone | `clients/typescript/st3-views/homeView.ts` (225 lines) `homeKind`, `pendingCalls`, `offeredAnswers` | Drops `custom.*` kinds. Shows launch variants and diagnostics, with no step preview. |
| Fractal | None live; `projections.ts` `attentionRefs` only | Nothing to delete. |

Settled: `custom.*` kinds are Today requests, as in the terminal UI. Allow depends on
`input_complete`; a client that cannot show the whole input offers deny only.

### Revision changes

On a revision card, `Attention.presentation.revision` is
`{from_revision, to_revision, reason, lines[], truncated}`. Each line is `{op, text}`: `+` adds a
step or a field, `-` removes one, and `~` changes one, naming the step and the fields that
changed in plain words (`~ review: goal, assignee`). There are at most 200 lines; beyond that,
`truncated` is true. The refresher computes the lines from the declaration diff between the
current and the candidate revision, the comparison `st apply --dry-run` already makes.

Today no client has these lines live: the terminal UI's revision card is always empty outside
the demo, and the phone and Fractal show no changes.

### Mission declaration

A read, not a projection: `mission-declaration.get`,
`GET /v1/client/mission-declaration?mission=mission/…`, with an optional `revision`. Mission
ids contain slashes, so it takes the subject as a query, as `publication-definition` does. It answers `{mission, revision, published_at, text}`, where `text` is
the published revision rendered as KDL by st's one renderer. Clients show it monospaced. It
needs the same `read.declarations` grant as `publication.definition`. A refusal reads "st does not
show this declaration to this device". A published revision never changes, so this read reads
it directly. It is a `person-read` path.

Today no client reads a declaration: the terminal UI shows one only in the demo, and
`subject.definition` refuses missions.

### Graph flags

Two declared flags replace name heuristics:

- **`keep-open #true` on a step** compiles to `StepSpec.keep_open`. It marks st's own steps that
  stay open while a mission watches; the mission projection's `watching` word and its step rows'
  `keep_open` read it.
- **`system #true` on a mission** compiles to `MissionSpec.system`. Missions under
  `mission/__st3/` are system without declaring it. The mission projection's `system` reads it.

For revisions published before the flags existed, the refresher treats an unset flag with
today's name rule: steps named `keep-watch`, `retire`, `steward-intake`, `standing`,
`keep-open` or `*-retirement`, and missions with a `ci` path segment. That one copy lives in the
daemon and goes once published missions declare their flags. Today the rule is copied in
`crates/st3-ui-model/src/missions.rs` (`keeps_open`, `is_system_mission`) and
`apps/ios/missionsView.ts`; Fractal has none.

### Sidebar identity

Each person has one Sidebar arrangement, at the reserved subject
`arrangement/person/NAME/00000000-0000-7000-8000-000000000001`
([#2043](https://github.com/compoundingtech/smalltalk/issues/2043)). The generated Rust, Swift
and TypeScript clients expose that UUID as `SIDEBAR_ARRANGEMENT_UUID` (Swift:
`st3SidebarArrangementUuid`), from `constants` in `operations.json`. Every client picks the same
Sidebar:

1. the live reserved subject, whatever its name;
2. otherwise the live arrangement named exactly "Sidebar" with the lowest id, which a client
   created before the reserved id;
3. otherwise none, and the first edit creates the reserved subject named "Sidebar". If st refuses
   that create with `arrangement-retired`, the person removed their Sidebar: the client says
   restoring a removed Sidebar is not available yet and never tries to create it again.

A client edits the Sidebar it picked and never copies a legacy one, so an older client still
editing it loses nothing. When two clients create the reserved subject at once, the second gets
`arrangement-exists` and adopts the first, and replicas converge under last-writer-wins. The
terminal UI (`crates/stui/src/ui/resource_sidebar.rs`), the phone (`apps/ios/sidebarEdits.ts`)
and Fractal (`apps/fractal-web/src/data/arrangements.ts`) follow this rule.

### Conversation stream

The conversation follow (`stream conversation`, `GET /v1/client/sessions/{*id}` and
`conversation.changes`) gains `entries`: the cleaned conversation, in the `*.expected.json`
shape `{id, body: {kind, value}}` plus its time. The daemon derives it with
`st3-conversation-ui`'s `adapt::conversation` as it joins the timeline and the Smalltalk mail,
and revises entries by id. A delta carries only the entries it changes. Raw frames stay for
clients that have not moved. The transcript fixtures pin `entries`, and the web app leads it as
[above](#who-leads-the-conversation). It is a `person-read` path already. It ships last, after a
Fractal test runs the fixtures.

Today the terminal UI cleans with `st3-conversation-ui`, the phone with `st3-views`
(`conversationView.ts`, 1032 lines), and Fractal with its own
`apps/fractal-web/src/conversation/` modules.

### Order and adoption

Projections ship in this order: graph flags and the Sidebar constant; agent; mission; attention,
with launch, prompt and revision; the declaration read; the conversation stream. The agent
projection comes before the mission projection because it reads only its own row, while a
mission's word also depends on the agents that queue its steps. Each lands in
small PRs. The daemon PR carries the schema and regenerated clients, the refresher, the
`words.json` check, the SLO entries and the `daemon_load` window. Separate PRs then move the
terminal UI and the phone onto the projection and delete their derivations. When a projection
merges, its route and collection are posted on
[#2646](https://github.com/compoundingtech/smalltalk/issues/2646) for Fractal to adopt; Fractal
asks there for any field it needs that is not listed here.

## iOS, the Expo way

Same capabilities, native idioms:

| terminal UI | iOS |
|---|---|
| `Ctrl+K` palette / sections `1`–`5` | Bottom tab bar: Now, Resources, Fleet, Spaces, with the Now alert badge |
| Sidebar list + detail pane | List screen pushing a detail screen (stack navigation); iPad may use a split view |
| Popover on a name | A sheet with the same summary, **Go to** and **Message** |
| Keys on a card (`a`, `c`, `t`, `g`) | Buttons on the card; swipe actions on list rows for the common ones |
| `y` to confirm | A confirmation alert or a second tap on the armed button |
| Tree view control | A segmented control above the list: Groups / Tree |
| `Ctrl+D` details | A details section or sheet from the agent screen's header button |
| Wheel, follow the end, "↓ N new lines" | Scroll view pinned to the end while at the end; a floating "N new" chip |
| Drag to select and copy | Native text selection on messages |
| Composer grows to eight lines | Growing text input above the keyboard |
| Loading / empty / failed | The same three states, never an empty list while loading |
| `Ctrl+]` on an agent: its terminal | Terminal button on the agent screen; keys as buttons |
| New mission control | A New mission button opening a composer sheet |

iOS demo screenshots use `apps/ios/demoGateway.mjs`: invented data, nothing real.
