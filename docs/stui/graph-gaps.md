# What stui needs from the graph

To: the Small Talk owner agent (st3 daemon, schema, graph)
From: the stui rebuild
Status: draft, 2026-09-28. Line references are against `agent/stui-rebuild` at `6589bb70`.

## 1. Purpose

stui is being rebuilt so the person uses it every day. It has to answer three questions without
making anything up: what is waiting on me, what is the fleet doing, and where is the work on disk.
stui renders projections and never infers state it wasn't given. So each gap below is either
something the person can't see, or something stui would have to guess. The rest of this doc is
about removing those guesses.

Principles this serves:

- **Every attention kind has its own renderer.** Home shows the thing being decided, not an id.
- **A decision's words reach whoever acts on it.** If the person types a reason, the agent sees it.
- **Relationships are data.** "Which agents are in this worktree" is an edge, not a path-string match.
- **Order comes from the daemon.** stui does not re-rank.

## 2. Worktree as a resource

### Today

- `enum Resource` has 15 kinds and none of them is a worktree or workspace
  (`crates/st3-client/src/generated.rs:684-700`).
- A worktree exists only as an agent's KDL `checkout "REPO" base= branch= remove-at-run-end=`
  (`crates/st3/src/checkout.rs:21-46`, validated in `crates/st3/src/graph.rs:2500-2532`).
- The reconciler creates it when the workspace directory is missing
  (`crates/st3/src/reconcile.rs:912-935`, `2150-2202`). It removes it at run end
  (`reconcile.rs:2204-2338`). **A successful create or remove records nothing.** Only warnings
  leave a trace, as `harness.diagnostic` claims with the code `checkout-fetch-failed`
  (`reconcile.rs:2174-2185`) or `checkout-kept` (`reconcile.rs:2320-2336`).
- The workspace path appears only as a string: `MemberSpec.workspace` and `cwd`
  (`crates/st3/src/model.rs:125`, `128`), `mission-run.created.workspace`
  (`crates/st3-schema/src/lib.rs:1839`), `planning-session.started.workspace` (`lib.rs:2357`),
  `schedule.work-requested` (`lib.rs:2462`), and `subscription.mission-requested` (`lib.rs:2474`).
- No claim links an agent, run or runtime to a worktree. `Agent`, `Runtime`, `Mission` and `Work`
  don't carry a workspace (`generated.rs:429-590`). Only unmanaged native sessions put one in
  `Session.extra` (`crates/st3/src/api.rs:1453`, `1470`). `Machine.projects` is hard-coded to `[]`
  (`crates/st3/src/api/client_v0.rs:895`).
- A `resource/NAME` family with a registry of resource kinds already exists
  (`lib.rs:690-860`, `docs/st3/schema.md:32`). But clients can write `resource/*`, so the daemon's
  own worktree facts shouldn't live there.

### Proposal

Add a system-only subject family `worktree/HOST/HASH`. HASH is the first 16 hex characters of the
sha256 of the canonical absolute path. Fields, as a `worktree.observed` claim written by the host
that owns the path:

| Field | Meaning |
|---|---|
| `host` | `host/NAME` reference |
| `path` | absolute path (immutable) |
| `repository` | source repository path, plus `vcs.repository` reference when known |
| `branch`, `base`, `head` | checked-out branch, declared base, current commit |
| `dirty` | bool; `untracked`, `ahead`, `behind` counts when cheap |
| `created_by` | the agent or run whose `checkout` created it, or `external` |
| `lifecycle` | `creating`, `present`, `kept`, `removed`, `missing`, `failed` |
| `remove_at_run_end` | from the declaration |
| `reason` | last failure or kept reason |

Which claims write it:

- The reconciler records `creating`, then `present` or `failed`, around `Checkout::create`.
  It records `removed` or `kept` around `Checkout::remove`. This replaces the silent success path.
- An observer pass on each host (cheap: `git status --porcelain=v2 --branch`) refreshes `head`,
  `dirty` and the counts for worktrees that are present. Keep the result `local`/latest, not
  durable per-poll.
- Plain workspaces (`workspace_create`, and paths that aren't checkouts) get the same subject
  with `repository` unset, so every workspace is listable.

Edges, as reference fields rather than path matches:

- `agent.workspace { worktree }` on the agent when its member spec is accepted (agent works in).
- `mission-run.created.workspace_ref` next to the existing string (run's workspace).
- `runtime.observed.cwd_ref` (runtime cwd), and `session.cwd` for managed sessions.

Client: add `Resource::Worktree` and `GET /v1/client/worktrees`. Each row carries the fields
above plus derived `agent_ids`, `mission_run_ids`, `runtime_ids` and `session_ids`, each current
by default and historical with `history=true`. Add `worktree_id` to `Agent`, `Runtime`,
`Session`, and each run inside `Mission`. Fill `Machine.projects` from repositories with present
worktrees.

Migration: keep the string fields. Resolve old strings to `worktree/…` on read by hashing
host+path, so existing runs show up without rewriting claims. New writers emit both until the next
schema major.

## 3. Gates and attention, per kind

Kinds come from `store.rs:8307-8486`. The actions come from `api.rs:1639-1650`.

| Kind (source) | Home must show | Exists today | Gap |
|---|---|---|---|
| `human-gate` (`store.rs:15166`) | question, what to review (target contents, the step's submitted summary), who did the work, mission/step titles | `detail`=question, `targets`=review target ids, `mission_id`/`step_run_id` | Target contents aren't resolved. The step's `work.submitted` summary isn't attached. There's no worker id. Only approve/reject. |
| `launch-approval` (`store.rs:15209`) | proposed mission: goal, steps with assignee and depends, agents with harness and host, gate count, diagnostics, what the person asked for | Everything is on `LaunchVariant.normalized_mission`/`visualization` (`api.rs:2160-2230`) | Attention `source_id` is `planning-session/ID`, with no `launch_id` or variant. The title is `Approve mission/ID` and the detail is fixed text (`store.rs:15218-15219`). `Launch.title` is the mission id (`api.rs:2316`). Targets are doc names whose content can't be read by a client. |
| `revision-approval` (`store.rs:15258`) | old vs new mission, reason, affected running steps | `detail`=reason, target `mission@rev` | No diff on the attention. stui has to find the proposal some other way. |
| `unread-message` (`store.rs:15306`) | sender, first lines, thread | title, fixed detail `Unread message from X.` | No `message_id` field (it's the `source_id`) and no excerpt. Reading the `Message` separately works. |
| `fault` from `attention.requested` (`store.rs:15337`) | who is asking, what, why, what the person should do, reply box | title, reason, severity→priority (`api.rs:1653-1660`), target states | **An agent asking for input is labelled a fault.** The requester (`AttentionRequestView.actor`, `model.rs:1601`) is dropped from the projection (`api.rs:1713-1728`). Outcomes are only `resolved`/`dismissed` (`store.rs:8054`). |
| `fault` from `subscription.mission-failed` (`store.rs:8423-8470`) | what failed, why, fix | `detail`=`code: reason` | Read with no resolution filter, so the item never leaves. The subject is a claim id, so `attention.resolve` fails with not-found (`client_v0.rs:3806-3814`). |

### Words never reach the actor

- `review.reject` stores the reason on `gate.result` (`api.rs:6060-6096`). The reconciler then
  fails the step with the fixed text "the human reviewer rejected the work"
  (`reconcile.rs:7094`, `8245`). The agent never sees the reviewer's words.
- `attention.resolve` appends `attention.resolved` with a reason (`store.rs:8049-8100`), but
  nothing tells the requester. Agents are told they wake only for messages and eligible steps
  (`crates/st3/src/boot.rs:41`).
- The good pattern already exists: `launch.revise` stores feedback as a document and messages
  the planner (`api.rs:4609-4661`).

### Proposal

1. **A human gate with a mode.** Extend `gate "…" type="human"`, which today allows only
   `reviewer`/`question`/`review` (`graph.rs:1957`). Add `mode="approve"` (the default and current
   behaviour), `mode="feedback"`, or `mode="answer"` with `options`. `gate.requested` already has a
   `decisions` array, currently hard-coded to `["approved","rejected"]` (`reconcile.rs:7037-7045`).
   Fill it from the mode:
   - approve: `approved`, `rejected`. The reason is required on reject.
   - feedback: `approved`, `changes-requested` with free text. On `changes-requested`, the text
     goes to the step's assignee as a thread message (in reply to the gate), and it is appended to
     the rerun's goal as `Reviewer feedback: …`. The step reruns and doesn't fail.
   - answer: one of the `options` or free text. It is delivered the same way, and the step
     continues.
   The existing, unused `human.review` resource kind already models
   `approve|request-changes|comment` (`lib.rs:810-826`), so reuse its vocabulary.
2. **Deliver every reason.** `review.reject` puts the reason into the `GateOutcome::Fail` text and
   sends it as a message to the step's claimant. `attention.resolve` sends the outcome and reason
   to the requester (`actor`) as a message.
3. **Split agent requests from faults.** Project `attention.requested` from an agent as
   `agent-request`, with `requester_id` and an `answer` action that carries free text. Keep
   `fault` for daemon-raised problems.
4. **Structured faults.** Add optional `what`, `because` and `fix` (the fix may name an action
   and its parameters) to `attention.requested` and to `subscription.mission-failed` items. Give
   subscription failures a real resolvable subject, or filter them by a later success or
   resolution.
5. **A compact launch preview on the attention item.** Add `launch_id`, `variant_id`,
   `preview_token` and a `preview` object: `goal`, `steps[{path,title,assignee,depends}]`,
   `agents[{id,harness,host,worktree}]`, `gates{human,automated}`, `diagnostics_count`, and
   `request_excerpt`. The same `preview` object belongs on `Launch`.
6. **A documents read API for clients.** `GET /v1/client/documents/content?name=…@hash`
   (`read.projections` scope), plus `Client::document_get`. The daemon route already exists
   outside the client surface (`api.rs:357-358`).
7. **Richer human-gate payload.** Add `worker_id`, `submitted_summary`, and
   `target_summaries[{id,kind,title,state,url}]` resolved from the review targets.

## 4. Mission control data

The Missions view needs each mission, ordered by who must act:

| Need | Exists? |
|---|---|
| `must_act`: `you`, `agent`, `system`, `blocked`, `nobody` | No. It can be derived from open attention plus step states. |
| progress (steps done/total, current loop round) | No. `Mission` carries run ids only (`client_v0.rs:486-497`). |
| current step(s): title, assignee, state, since | Partly. `Work` has `path`/`state`/`claimant`, but no `assigned_to`, `title` or `ready_age_ms` (`api.rs:1066-1094`), although `StepRunView` has them (`model.rs:1995-2035`). |
| owner / requester | No. `MissionRunView.requester` (`model.rs:1921`) is dropped. |
| blocker (with reason) and the run it waits on (`after`) | Per step (`blocked_reason`, `blockers`). There's no run-level summary and `after` (`model.rs:1931`) is dropped. |
| wait time (how long in the current state) | No `state_since` on the mission or the run. |
| last progress note | No. The `work.progress.summary` claim field (`lib.rs:1937-1947`) isn't projected. |
| title / goal | `title` is the id without its `mission/` prefix (`client_v0.rs:490`), with no goal. |

Proposal: add `Resource::MissionRun` (or a `runs[]` array on `Mission`) that carries `status`,
`phase`, `requester`, `workspace_ref`, `after`, `deadline`, `must_act`, `progress{done,total}`,
`current_steps[{id,title,assignee,claimant,state,since}]`, `blocker{step,reason}`, `state_since`
and `last_progress`. Also add `title`, `assigned_to` and `last_progress` to `Work`.

## 5. Smaller gaps

- **Now ordering is discarded.** `client_attention_resources` sorts by priority, then age
  (`api.rs:1787-1802`). Then `/v1/client/now` sorts again by kind and **id**
  (`client_v0.rs:1080-1090`), so Home order is effectively random within attention. Keep the
  upstream order. Also give non-fault kinds a real priority; today they are all `normal`
  (`api.rs:1701-1707`).
- **Agent activity.** `harness_state` (ready/working/idle/…) and `driver` are exposed
  (`api.rs:1191-1199`). `state` folds working and idle into `running` (`api.rs:1220-1235`), and
  `Runtime.state` does the same (`client_v0.rs:552`). `updated_at` is the last harness
  observation (`api.rs:1258-1271`), not the last real activity. Add `last_activity_at`
  (last tool call, message or progress) and `silent_since` when working with no output. Add
  `host_id` to `Agent`, since only `Runtime.owner_host_id` has it today.
- **Message read state exists.** It's the `message.read` claim (`lib.rs:2352`), surfaced as
  `Message.state` (`api.rs:1883`). No change is needed. Adding `message_id` and an excerpt to
  `unread-message` attention would save a fetch.
- **Resolution reasons in history.** `AttentionRequestView.resolution_reason` (`model.rs:1606`)
  isn't in the projection, so "what did I decide" can't be shown.

- **A person cannot fix a broken agent or a stalled step from a client.** `runtime.restart`,
  `runtime.reset` and `runtime.stop` are person actions but are left out of the client's
  `AVAILABLE_ACTIONS` (`crates/st3/src/api/client_v0.rs`, the two action lists near the top), and
  there is no client action for `st work retry`. stui can only cancel a run or point at the CLI.
  Offer restart, reset and retry to persons through the client API, fenced like `mission.cancel`.

## 6. Proposed order of work

Smallest useful change first:

1. Stop re-sorting `/now` (a one-line fix). Add `requester_id`, `launch_id`, `variant_id` and
   `message_id` to attention items.
2. Deliver the `review.reject` and `attention.resolve` reasons to the agent as messages.
3. Add the client documents read endpoint and method.
4. Add the compact `preview` to launch-approval attention and `Launch`.
5. Split `agent-request` out of `fault`. Make subscription failures resolvable, and give faults
   `what`/`because`/`fix`.
6. Add `title`, `assigned_to` and `last_progress` to `Work`, and run-level
   `requester`/`status`/`progress`/`current_steps`/`must_act`/`state_since`.
7. Add the human gate `mode` (approve, feedback, answer) with feedback delivered into the rerun.
8. Add the `worktree/…` subject, lifecycle claims from the reconciler, and the edges.
   Then the `Worktree` client resource and list endpoint, with legacy strings resolved on read.
9. Add agent `last_activity_at`/`silent_since`/`host_id`, and fill `Machine.projects`.

stui can ship Home and Missions after steps 1-6. It will show an honest "unknown" where later
steps haven't landed. The Worktrees tab waits for step 8.
