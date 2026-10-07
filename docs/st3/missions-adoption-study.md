# Missions adoption study (2026-10-03 → 2026-10-05)

Status: findings 2026-10-05; pilot run 1 result added 2026-10-07. Author: agent/smalltalk/missions-dev3. The study was read-only; the pilot mission was published later with Johannes's approval (q14).

## Question

Where would st missions (runs, steps, `depends-on`, gates, `st work ask`, lanes, seat queues, subscriptions, schedules, retries, fault routing) have removed hand-relayed messages, coordinator round-trips and chat-based gates in the fleet's real work during 2026-10-03 00:00 → 2026-10-05 10:00 Europe/Berlin?

## Evidence and method

- 4,795 `message.sent` claims from the dev3 st3 store (`~/.local/state/st3/claims.sqlite3`, read-only) in the window.
- 888 mission/run/step/gate/work/subscription claims from the same store.
- 909 Axe decision records from dev3 and dev5 (`~/.local/state/axe/*/*/decisions`).
- Live read-only CLI: `st missions ls/show`, `st lanes ls`, `st work --help`, `axe incidents status`.
- Four subagent reports (per flow) with reproducible counts. The raw reports are kept by agent/smalltalk/missions-dev3 and are not committed.

Counting rules that apply to every number below:

- Elapsed times are wall-clock gaps between messages. They are not labor and not guaranteed savings. Waits overlap.
- Relay counts are hand-checked lower bounds.
- `person/schickling` in messages is usually Johannes's Codex operator session on mbp2025, not Johannes typing. Human answers are counted from Axe records only.
- Pattern counts overlap across sections. Do not add them.

## Headline facts

| Fact | Value |
|---|---|
| Messages in window | 4,795 |
| Manager ↔ fod-lander-dev3 | 763 (16% of all traffic) |
| Manager ↔ deployer | 558 |
| Messages to or from the manager | 2,976 (62%) |
| Mission runs created in window | 59, of which 37 are `watch-smalltalk-*` pollers and 11 are steward daily-health |
| Mission runs for merge, deploy, restart or incident work | 0 |
| Open or historical st lanes | 0 (`st lanes ls --all` → `[]`) |
| Native `st work ask` used as the answer authority | 0 (18 asks are Axe mirrors; 17 cancelled, 0 person-done) |
| Axe decision requests | 418 (manager 48, wf 71, smalltalk-graph 43, st3-cleanup 33) |
| Human gates in use | 38, all to keep watcher runs alive; 0 feedback gates |

The fleet uses missions for scheduled maintenance and pollers. The fleet does not use missions for the flows that carry most of the traffic: landing PRs, deploying tuples, restart waves and incidents. Those flows run on chat through the manager.

## What works today

- `dotfiles/ci/experiments-r1`: 7/7 steps completed in 3h25m with run-owned parallel workers and a serialized bench step. This is the model to copy.
- `dotfiles/steward/daily`: 11/11 daily-health children completed (median 19m50s).
- `dotfiles/fractal-github/github-tab`: a real finite feature mission, 2/4 steps done at study time.

## What fails today, and why

These failures teach agents that missions are unreliable. Fix them before asking for wider adoption.

| Run | Outcome | Cause |
|---|---|---|
| `dotfiles/deps/weekly-cycle` | 13/15 historical runs failed, 2 cancelled | Exec `branchy new` gate exits 127 / 1; reused dirty worktrees; private Cachix 401; one step failed instead of waiting on a Johannes decision; one step had no Axe route |
| `dotfiles/webfractal/m1` | 2 runs failed at `start-team` | Run-owned helpers crash-loop: login PATH picks OMP 18.6.0, st refuses the unadmitted provider (dotfiles#4640) |
| `dotfiles/ci/bbr-offpeak/2026-10-04` | Failed after 3 s | A "not before 04:00" condition is an exec gate; the installed runtime treats exit 1 as fail, not "not yet" |
| st-delegation `retire-workarounds` | Stuck claim, 5h32m to recover | Dead incarnation lease read as ready but not claimable; needed the #957 daemon deploy |

Version skew: the installed CLI (`st 0.1.0` on dev3) has no `st gate`, no `st missions check`, no structured `st work ask --request` and no `merged` built-in gate. `docs/` on main describes these features. Agents that follow the docs hit errors. Proposals below mark each feature they need.

## Ranked proposals

Ranking weighs traffic removed, coordinator round-trips removed, and how much the installed runtime already supports.

| # | Proposal | Main evidence | Needs |
|---|---|---|---|
| 1 | Dotfiles merge train on an st lane (already decided; owned by agent/dotfiles/ci as "train v0") | 763 msgs; 68 queue restatements over 153 PRs; ready → turn median 55m | Lanes (installed); controller in train v0 |
| 2 | Landing chain as dependent steps: merge → repin → manifest → deploy plan | 20 confirmed manager relays (median 36.5s, max 22m); merge → deploy relay 11m | `depends-on`, products (installed) |
| 3 | Deploy mission per tuple: admit → canary → system → hm → verify, human gate only for real authority | 11 tuple reviews; S3 and credential holds ~9h each; 4 fault loops | Steps, human gates, `retry` (installed) |
| 4 | Restart / maintenance-window wave mission | 12 broadcasts = 223 copies; S8 heads-up → cancel → reissue = 43 copies | Steps; restart bugs #1370 / #1373 |
| 5 | Bind Johannes decisions to the consuming step | 48 manager Axe pairs; q75–q77 restated to operator and owners | `st work ask --step` (installed, simple form); structured asks need upgrade |
| 6 | Incident runs with pre-authorized fallback and a probe on another host | dev3 root critical → decision request 3h31m; dev5 outage → recovery notice 7h08m | Schedules (installed); host-health observer (missing) |
| 7 | Seat provisioning + kickoff as one run start | 5 handoffs, median 4m25s | Seat queue (installed) |

### 1. Dotfiles merge train on an st lane

What happened: the manager kept the landing order in its head and restated it in chat. 68 manager → lander messages carry a queue or order list (578 PR references, 153 distinct PRs). Owners asked for named turns. Priorities changed after the PR had already merged (#4511, `message/73845e3104405e8f`). Five sampled ready → turn waits: 18m, 57m, 91m, 55m, 47m (median 55m27s). A shared `wf-heavy` flock starved #4541 for 2h26m and needed two manager interventions (`message/fd8823b345fa315e`, `message/61cfd246c8725750`). Five native-stack merge refusals needed manual unstack (#4511, #4537, #4557, #4285, #4584).

Mission shape:

```kdl
mission "dotfiles/merge-train" state="ready" {
  goal "Land admitted dotfiles PRs one at a time on current main."
  lane "dotfiles" {
    entries "resource/github/schickling/dotfiles/pull-request/"
    approver "person/schickling"   // only for PRs that need Johannes's admin merge
  }
  // driver: the train v0 controller marks held/ready/running, merges the front entry, leaves it with the actual SHA
}
```

- Owners `st lanes join dotfiles 4543 --reason "SOURCE GO at <head>"` once.
- Manager changes priority with `st lanes move ... --reason`, not a restated list.
- Johannes's merge-go becomes `st lanes approve` on that entry.
- Known stack refusal becomes a fault routed to the owner, not a manager relay.

Gaps: a lane stores order and status only. The driver must implement next-ready policy, head invalidation and the SHA guard. A lane does not make `flock` fair; heavy proofs need one executor that all producers use (separate follow-up).

Ownership: this proposal is already decided. Johannes chose an st-native merge controller that retires Hypermerge (Axe ci `l5b79w`), a thin vertical slice first (`qchgp5`), train v0 controls intake / hold / reorder / pause (`n1619l`), and "build train v0 as an st mission, shadow first" (`0ezurz`). agent/dotfiles/ci owns it. This study's contribution is evidence for train v0: Johannes admin-merge approval as lane `approve`, native-stack refusal as an owner fault, heavy-proof fairness, and dotfiles PR observation.

The dev3 store holds only 3 `resource/github/schickling/dotfiles/pull-request/*` subjects, so st observes almost no dotfiles PRs today. Head, review and check facts for lane entries need a GitHub watch on the dotfiles repo first.

### 2. Landing chain as dependent steps

What happened: after a merge, the manager typed the actual SHA to the next owner. #4307 → upstream #1575 → manifest repin → ledger → megarepo-all #121 → dev3 canary was serialized through chat (`message/8e2aa1f2e97b32d2`). #4418 merge → manager deploy relay took 11m (`message/eef00a306d90ac52` → `message/b9e821466762359c`). Reviewer findings reached authors through the manager 8 times; one took 5h01m (`message/2330f34737776f1b` → `message/1bfd9cf6a3daf79a`). The dev4 reclaim fix #4598 merged but was never deployed, so the same disk floor came back next morning.

Mission shape: one finite run per landing chain. Steps `review` → `repair` → `merge` (produces actual SHA and tree) → `repin` → `manifest-close` → `deploy-plan` (deployer). The run stays open until the operational check passes, not until the PR merges.

Gaps: products for local receipts (test chronology, log digest) need a small capture adapter. One evidence episode (#4666) took 3 packets and 13m55s because the receipt had the wrong timestamp (`message/409fda4bba161d8d`).

### 3. Deploy mission per tuple

What happened: each tuple went deployer → manager → GO in chat. 11 matched submissions → acceptance: median 53s, total 32m. The costly waits were different kinds of hold mixed in one chat state: S3 waited 8h56m on checkpoint scope and operator handoff; the dev5 SYSTEM transport waited 8h54m on a credential only Johannes may provision (`message/011d7d983172ed60` → `message/e221f4dcc609cee8`). S7 had four fault loops (wrong-phase POST, port held by a proof server, SCG reviewer key twice). Mac S4 rollback left a forward-migrated st DB; the manager approved three component recoveries one by one.

Mission shape:

```kdl
mission "dotfiles/deploy" {
  input "tuple" kind="resource"
  step "admit"   { assigned-to "agent/dotfiles/deployer" }
  step "credential-authority" { agentless
    gate "Johannes approves credential scope" type="human" { reviewer "person/schickling" } }
  step "canary"  { depends-on { step "admit" completed }; retry { attempts 2; backoff "30s" } }
  step "system"  { depends-on { step "canary" completed } }
  step "hm"      { depends-on { step "system" completed } }
  step "verify"  { depends-on { step "hm" completed } }
}
```

- The human gate exists only when standing authority excludes the operation.
- Manager review becomes an assigned step that looks only at deviations.
- A changed SHA is a new run, because inputs are immutable.

Gaps: st does not do host locks, rollback or receipt validation; the existing deploy controller stays the executor. Retry is only for the readiness canary; activation stays fail-closed.

### 4. Restart / maintenance-window wave

What happened: the deployer announced each window, cancel and close to every seat by hand. 12 exact-content broadcasts produced 223 messages (recipients 13–41). S8 alone: heads-up → hold → new heads-up = 43 copies (`message/153bb3a261ccf94f`). The exclusion inventory grew in chat (`message/2f3433a64db0352b`). An issue-sweeper crash-loop stopped a wave at 23:54 and needed 8 messages to resume (`message/ab2ad1432a08308f`).

Mission shape: one run per wave with an immutable eligible/exempt inventory and a per-seat chain `checkpoint → relaunch → verify`, served one seat at a time. Affected seats read the window from the graph instead of from 34 copies.

Gaps: restart binding bugs #1370 (when-idle cycle) and #1373 (explicit restart parks). Quiescence detection must come from the restart helper. agent/dotfiles/smalltalk-graph owns restart-wave tooling; this proposal must go through that seat.

### 5. Bind Johannes decisions to the consuming step

What happened: Johannes answers fast (manager Axe: 48 pairs, median 3m18s; blocker median 4m39s). The slow part is consumption: the manager restated q75, q76, q77 to the operator and owners, and the operator re-read them (`message/95d6e5a3e30efd62`, `message/32bab07b57bebd4b`). The 18 native asks are mirrors of Axe questions and are cancelled when Axe answers, so no step ever resumes on an answer. Of 418 Axe requests, 20 are planned approvals (gate candidates, e.g. `34ss2a` merge-go #4307) and 397 are discovered choices (ask candidates).

Proposal: planned approvals become human gates in the mission that consumes them. Discovered choices use `st work ask --step <claimed step>` so the asking step resumes on the answer. Axe stays the decision history; the bridge records the answer as person-done on the step instead of cancelling.

Gaps: installed CLI has only the simple ask (no `--request`, no answer IDs). Axe guards, supersession and assumptions are not native (#718/#719).

### 6. Incident runs with pre-authorized fallback

What happened: overnight on 2026-10-04/05, faults went to the manager by chat while the manager waited. dev3 root at 2.6 GB (05:00) → first decision request to Johannes 3h31m later; Johannes answered q85 in 2m35s. dev5 unreachable 01:27 → recovery notice 7h08m later; the worker's fallback request got "no answer yet" after 1h43m (`message/b196301e27f69447`). Durable incident dotfiles#4682 appeared 1h26m after the critical message.

Mission shape: scheduled read-only probe mission on a healthy host → on breach, a finite `infra/root-pressure` run: `diagnose` (free-disks, 10m) → `policy-reclaim` → human gate only for non-policy storage changes → `prove-headroom`. Approved proof-host fallbacks live in the mission constraints, so a host outage does not wait on chat permission.

Gaps: no observer provider for host health, Prometheus or SSH liveness today; use a schedule until one exists. `handles-faults` seat routing exists but was not usable on 2026-10-03 (old pin, DSL gap).

### 7. Seat provisioning + kickoff

What happened: 5 request → running receipt → separate kickoff handoffs (median 4m25s). Low cost. Fold kickoff into the first run start on the new seat's queue. Lowest priority.

## Not proposed

- Do not put a human gate on every PR or tuple. That adds round-trips.
- Do not model standing availability as an empty mission; it completes at once.
- Do not rely on mission authority blocks as a security boundary; free mode ignores them.
- Do not retry side-effecting steps (activation, deletion, quota) automatically.

## Prerequisites before a pilot

1. Deploy a current st to the fleet, or restrict pilots to features the installed binary has (lanes, `depends-on`, human gates, simple `work ask`, `retry`, schedules).
2. Fix or retire `dotfiles/deps/weekly` and the BBR gate model; failing missions are the strongest anti-adoption signal.

## Bakeoff results (2026-10-05, scratch st daemon, installed `st 0.1.0`)

Raw reports (ParetoMiner, GateBakeoff, ChainBakeoff) are kept by agent/smalltalk/missions-dev3 and are not committed.

- Landing chain (measured): merge → repin → manifest → deploy-authority → deploy-plan completed twice. The SHA passed between steps through resource claims and a ~25-line receipt adapter, so nobody retyped it. `${step.merge.sha}` interpolation does not exist (`422 unknown-variable`). A human gate (`st attention approve`) held the deploy step until approval. A mid-run revision carried the completed steps forward and rejected an approval from the old generation. Exec steps need `restart "never"`; the default produced 3 receipts per stage.
- Gate scheduling (measured): replacing gate-slot is not viable today. A direct `missions start` at `max=2` rejected 4 of 6 requests (no durable queue). There is no host or class capacity and no agentless priority. Exec retry reran the step but still ended failed. `st trace wait --for terminal` reports success for failed runs. Proven: gate-slot admission → receipt (HEAD, tree, log sha256) → mission completes, with no model turns. Gate-slot already removed most contention (#4680 waited 0s).
- Decision binding (measured): `st work ask` and feedback gates need a live worker incarnation (`stale-work-ask`, `feedback-gate-needs-worker`). Not yet proven end to end.
- Frequency (measured, week to 2026-10-05): 19 merge → deploy instructions; manager ↔ deployer 698 messages; 4 incident episodes; `deps/weekly` 13/15 runs failed.

## Aligned pilot plan (Axe decisions, missions-dev3 tree)

| Handle | Decision |
|---|---|
| q2 `m77p2f` | This seat authors one pilot and fixes the st gaps the pilot hits. Small scope, real signal. |
| q3 `9nq1j4` | Choose by Pareto benefit/effort; use subagent mining and bakeoffs; 2 pilots at once is allowed. |
| q4 `ck0rz7` | Mission definitions are typed TS in smalltalk-graph (`graph/<id>/mission.ts`), published by agent/dotfiles/smalltalk-graph. |
| q5 `vqx0c6` | Success = N consecutive clean real runs (no manual relay or rescue); stop after 2 rescues. Also report relay and wait reduction against this study's baseline. |
| q6 `hvmzfw` | One pilot: gate receipt → merge → repin → manifest → deploy-plan, human gate only for deploy authority. Replacing gate-slot is out of scope. |
| q7 `xnvh8q` | Scope: smalltalk merge → dotfiles repin → manifest → HM deploy chains (delegated; chosen by missions-dev3). Inputs stay generic so other chains can join later. |
| q8 `a4cbj0` | missions-dev3 starts the pilot runs, then hands the requester role to agent/dotfiles/manager. |
| q9 `m39vq3` | Plan confirmed; N = 5 clean runs. |
| q10 `44rnn8` | Pinned genie (and even the #1630/#1631/#1635 stack) cannot express human gates, run inputs or produced resources. Build a lean v0 within the stack (no human gate; receipts as st claims checked by field gates), and write genie PR D (human gate, inputs, produces) in parallel. Next: show Johannes the DSL with concrete examples. |
| q11 `xdrq07` | v0 is written on the pinned genie schema now; the receipt adapter does checks inside exec steps and bounded merge polling. Migrate to native gates and retry after #1630/#1631. |
| q12 `i5ci10` | Vista publishes under the command-local owner `dev3.smalltalk-missions-dev3` until st3-cleanup and Johannes redesign Vista owner identity. |
| q13 `vv2gnw` | agent/dotfiles/fod-lander-dev3 owns the repin step (opens, proves, merges). |
| q14 `e7cw0l` | Brief lander and deployer, open the smalltalk-graph PR with `publish = true`; the next real repin runs through the mission, chat as fallback. |
| q15 `j87393` | Write genie PR D (human gate, inputs, produces) after 2 real runs. |

Work items, in order:

1. Author `graph/dotfiles/landing/smalltalk-repin/mission.ts` in smalltalk-graph (main c622cc8+ loads every `mission.ts`; no extension needed). missions-dev3 opens the PR; agent/dotfiles/smalltalk-graph reviews, merges and publishes. Use the pinned genie MissionSchema and imported seat refs; acceptance as native gates. Steps: `gate-receipt`, `merge`, `repin`, `manifest`, `deploy-authority` (human gate only when standing authority does not cover the deploy), `deploy-plan`. Each step produces one receipt resource; the receipt adapter lives next to the mission.
2. Prove it on a scratch daemon, then run it beside chat on the next real smalltalk repin.
3. Count clean runs and rescues; at the N-th clean run, hand the requester role to the manager.
4. File each st gap hit along the way upstream (smalltalk). Known so far: no predecessor-output binding, `trace wait` success on failed runs, exec retry ends failed, no durable queued start.

## Still open

- Whether the Axe → st bridge records answers as person-done on the consuming step (proposal 5).

Pilot hypothesis record: schickling/dotfiles `.hypo/missions-coordination/` (experiment `smalltalk-repin-chain`).

v0 mission: smalltalk-graph#219, published as `mission/dotfiles/landing/smalltalk-repin`. Review page: https://vista-dev3.tail8108.ts.net/r/agent%2Fsmalltalk%2Fmissions-dev3/mission-dsl-options/v1/

## Pilot run 1 (2026-10-07, `r1-92751ff`)

The run repinned smalltalk `92751ff6` as dotfiles #5018 (main `8678de78`). All 5 steps completed in 1 h 12 min.

| Measure | Run 1 | Chat baseline |
|---|---|---|
| Repin submitted → deploy-plan ready | 11 s, no messages | #4418: 11 min manager relay |
| deploy-plan ready → deployer claim | 23 s, from the seat queue | relayed by chat |
| Messages about the chain | 8 | #4418: 87; #4500: 68 |

- Verdict under q5: rescued (1 of 2). The requester relayed the run requirements (MemoryMax, gate order) to the lander, because v0 has no typed run inputs and the `upstream` receipt dropped the field. Fix: smalltalk-graph#220 forwards `requirements`.
- The manager's chat relay to the deployer arrived 2 s after the deployer had claimed the step from the graph. It was redundant.
- The run ended at the deploy plan. The dev3 Home Manager build then failed on a Cargo git fetch (incident #3936; fix smalltalk#1662). A completed run does not mean a deployed change.
