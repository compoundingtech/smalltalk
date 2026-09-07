# Pipes: declaration and lifecycle prototype

Date: 2026-08-20

Branch: `schickling/2026-08-20-pipes` · commits `6ca0163`, `15bea2a`, and this one.

## Question

A "pipe" lets an agent subscribe to external events (GitHub CI updates, timers)
delivered over the existing message bus. This spike covers the **declaration and
lifecycle** half only: how does the event SOURCE get declared, launched,
supervised, and torn down? (A separate spike covers ingress.)

Hypothesis: a `pipe "name" { ... }` node on an agent declaration lowers to a
DERIVED exec companion task, exactly mirroring how the DING sidecar is
generated, and thereby inherits restart policy, flapping/parking, suspend and
retire teardown, and crash-loop surfacing for free.

## Result

**Confirmed, and cheaply.** The derived-companion coupling in `src/reconcile.rs`
and `src/run.rs` keys on `Task::derived`, never on the name `"ding"`. Exactly two
places in the whole codebase are name-scoped to DING:

- `src/reconcile.rs:298` — the fail-closed "unsupported derived task" gate.
- `crates/agent-spec/src/spec.rs:398` — `has_delivery_transport`, correctly
  scoped so a pipe cannot claim a transport.

Extending the first and leaving the second alone was the entire lifecycle
integration. **No change to `src/run.rs`, `src/flapping.rs`, `src/park.rs`,
`src/task_inventory.rs`, `src/exec_backend.rs`, or `src/doctor` was needed.** All
eleven named derived-DING lifecycle tests stay green.

---

## (a) What was built, where

| Area | File:line | What |
|---|---|---|
| Declaration shape | `crates/agent-spec/src/declared.rs:42-58` | Two new diagnostic codes: `pipe-launch-missing`, `unsupported-pipe-interval` |
| Declaration shape | `crates/agent-spec/src/declared.rs:307-345` | `pipe` arm: missing name, missing launch, reserved `every` |
| KDL lowering | `crates/agent-spec/src/kdl_format.rs:169-177, 445-484` | `pipe` node → `RawPipe`; duplicate-name and unknown-field refusal |
| Raw model | `crates/agent-spec/src/spec.rs:486-500` | `RawSpec::pipe: BTreeMap<String, RawPipe>`; `#[serde(default)]` keeps TOML/JSON parsing intact |
| Name grammar | `crates/agent-spec/src/spec.rs:501-560` | `validate_pipe_name`, `PIPE_TASK_PREFIX`, `pipe_marker_prefix`, `pipe_name_of_task` |
| Task synthesis | `crates/agent-spec/src/spec.rs:1022-1076` | One derived exec `Task` per pipe, beside the DING synthesis |
| Late binding | `src/reconcile.rs:270-340` | Extended (not bypassed) "unsupported derived task" gate; marker-prefix assertion, argv[0] and `$ST_ROOT` rewrite |
| Issue codes | `src/validate.rs:464-469` | The two new codes surface under their own names, not `declaration-shape` |
| Runner | `src/pipe.rs` (new, 505 lines) | `run_pipe`, `PipeConfig`, `OwnerWatch`/`OwnerGrace`, `idempotency_key`, `summarize` |
| CLI | `src/main.rs:688-717, 2147-2194` | `st2 pipe run --agent … --name … --root … (--command … \| -- argv…)` |
| Lifecycle proofs | `tests/run.rs:1727-2077` | 10 tests |
| Declaration proofs | `crates/agent-spec/tests/declared_document.rs:148-215`, `crates/agent-spec/tests/discovery.rs:1771-1925` | 5 tests |
| Inventory proof | `tests/task_inventory_cli.rs:825-880` | 1 test |
| E2E demo | `tests/pipe_e2e.rs` (new, 474 lines) | 11 tests |

---

## (b) Proof results

Every claim below is a test that was run, not a reading of the code.

### 4a — fresh agent launches with its pipe in one pass

`tests/run.rs::fresh_compact_agent_launches_with_its_derived_pipe` — one
`up_once` pass launches `["hetz.demo", "hetz.demo.ding", "hetz.demo.pipe-gh-ci"]`
and the pipe's launch is the exact late-bound argv:

```
<running st2> pipe run --agent hetz.demo --name gh-ci --root <catalog> --command poll-gh-ci.sh
```

`ST_AGENT=hetz.demo` is injected into the pipe like any other task (Runner-owned
task identity holds).

### 4b — suspend / retire / hold tear the pipe down

- `retired_compact_agent_stops_agent_and_derived_pipe`
- `suspended_compact_agent_stops_its_derived_pipe_without_touching_a_sibling`
- `held_adopt_only_compact_agent_stops_its_live_derived_pipe`
- `parked_compact_agent_stops_its_live_derived_pipe` (agent parks → pipe killed)
- `selected_missing_derived_pipe_is_held_without_broadening_to_its_agent`

### 4c — a crash-looping pipe parks WITHOUT affecting the agent, and surfaces

`tests/run.rs::a_crash_looping_pipe_parks_and_surfaces_without_disturbing_its_agent`

With `restart { attempts 1; mode "fail" }`, a live agent, a live ding, and a dead
pipe, four `reconcile`/`execute` passes over ONE `FlappingCap` give:

```
report.flapping     == ["hetz.demo.pipe-gh-ci"]
lifecycle op log    == ["reap:hetz.demo.pipe-gh-ci", "spawn:hetz.demo.pipe-gh-ci"]
crash_loops[0]      == { pty_id: "hetz.demo.pipe-gh-ci", identity: "demo",
                         supervisor: "cos-claude" }
surfaced message    tags ["crash-loop"], body names hetz.demo.pipe-gh-ci
```

**Measured caveat, not hidden:** the agent DOES appear in the raw runner op log,
as `patch:hetz.demo` — the pass's ordinary presentation batch, which touches every
live task and carries no lifecycle meaning. The test filters `patch:` explicitly
and says why. The first version of this test failed on exactly that line; the
claim was narrowed rather than the evidence.

`up_once` cannot express this case at all: both single-pass entry points build a
fresh `FlappingCap`, so a one-shot reconcile can never park anything.

### 4d — `st2 tasks --json` reports the pipe honestly

`tests/task_inventory_cli.rs::a_derived_pipe_task_is_reported_honestly_alongside_its_agent`.
Real CLI output against a live demo catalog:

```json
{"agent":"hetz.demo","task":"pipe-gh-ci","runtimeId":"hetz.demo.pipe-gh-ci",
 "kind":"exec","lifecycle":"service","retired":false,"desiredState":"running",
 "agentDesiredState":"running","agentDesiredStateReason":null,
 "runtime":{"state":"absent","pid":null,"createdAt":null,"generationId":null,"error":null},
 "parked":null}
```

No new field, no special case, no schema change. The inventory path runs the same
`compile_generated_tasks` the supervisor does, so this also exercises the
extended derived-task gate on the read-only diagnostic surface.

### 5 — E2E with a fake GitHub CI poller

`tests/pipe_e2e.rs` (11 tests) plus a hand-driven run of the real binary against
a temp catalog. The poller is a shell script reading a local state file
(`pending → running → success`), emitting one JSON line per transition with its
own `id`. No network.

Real CLI, first run:

```
$ st2 pipe run --agent hetz.demo --name gh-ci --root $D --host hetz \
    --command "./poll-gh-ci.sh ci-state.txt"
st2 pipe: 'gh-ci' source running → hetz.demo's inbox (task hetz.demo.pipe-gh-ci)
st2 pipe: exiting (SourceExited) after 3 delivered event(s)
$ ls $D/agents/hetz/demo/resources/inbox/
1787207698953-nhm7w0.md  1787207698957-he2enx.md  1787207698959-67fd1f.md
```

One delivered message:

```
---
from: hetz.demo
subject: pipe gh-ci: {"id":"run-7:pending","run":7,"state":"pending"}
tags: pipe, pipe:gh-ci
idempotency-key: pipe:gh-ci:id:run-7:pending
---
x-st2-pipe-task: hetz.demo.pipe-gh-ci

{"id":"run-7:pending","run":7,"state":"pending"}
```

Second, identical run — the supervisor-restart case:

```
st2 pipe: exiting (SourceExited) after 3 delivered event(s)
$ ls .../inbox | wc -l
3
```

Zero new messages, and the receipts are byte-identical
(`a_supervisor_restart_replays_the_source_without_duplicating_its_events`
asserts the filenames match). After appending a `failure` transition, the same
command produces 4 messages — the dedup is on event identity, not on "already
ran".

`killing the poller respects restart policy`:
`a_dying_source_ends_the_run_instead_of_being_respawned_in_process` proves the
runner RETURNS (`SourceExited`, status 9) with the pre-crash event preserved and
no in-process respawn; the supervisor half is 4c above. There is deliberately no
retry ladder inside `run_pipe` — a second lifecycle owner is exactly what this
shape argues against.

### Full-suite status

`cargo test --workspace --no-fail-fast`, before and after, produces the
**identical** failure set. Seven suites fail for pre-existing, environment-shaped
reasons unrelated to pipes:

| Suite | Test |
|---|---|
| `catalog_diff` | `classification_only_and_nested_agent_filename_changes_are_exact` |
| `eval_run_e2e` | `canonical_agents_freeze_the_admitted_route_across_post_boot_catalog_mutation` |
| `eval_up` | 4 tests |
| `materialize` | `up_materialize_only_writes_the_overlay_without_needing_pty` |
| `native_only` | `clean_path_supports_help_validate_env_and_doctor`, `tracked_product_surface_contains_only_native_names` (fails on a retired product name still present in `docs/vrs/spec.md`) |
| `targeted_reconcile` | `targeted_once_real_pty_preserves_sibling_generation_across_selected_lifecycle` |
| `task_inventory_cli` | `completed_catalog_aba_during_runtime_observation_is_incomplete` |

Green and directly relevant: `--test run` (57), `--test pipe_e2e` (11),
`-p agent-spec` (68), `--lib` (327), `--test invariants`, `--test reconcile`.

---

## (c) The KDL grammar

```kdl
agent "demo" {
  host "hetz"
  command "…"
  ding

  pipe "gh-ci" { command "gh run watch --json" }   // shell, under sh -c
  pipe "tick"  { argv "tick-source" "--json" }     // direct, no shell
}
```

The child set is deliberately two nodes. A pipe declares WHERE events come from.
How it is supervised is inherited from the agent (`restart{}`, `desired-state`,
`lifecycle`, `keep`); how events are delivered is the bus contract. Nothing else
needed a knob.

`pipe "<name>"` lowers to a derived exec task named `pipe-<name>` with runtime id
`<host>.<identity>.pipe-<name>`.

### Validation diagnostics

Source-located, from `parse_declared_document` (these fail discovery, so an
invalid pipe never becomes a runnable spec):

| Code | Message |
|---|---|
| `task-name-missing` | `pipe task must have one positional string name` |
| `pipe-launch-missing` | `pipe must declare exactly one of \`command\` or \`argv\`` |
| `unsupported-pipe-interval` | ``pipe `every` is reserved for the future `schedule` contract; a pipe declares a long-running event source`` |

Lowering-level refusals (anyhow, from `kdl_format` / `into_agent_spec`):

| Trigger | Message |
|---|---|
| both forms | ``pipe 'gh-ci' must declare exactly one of `command` or `argv` `` |
| duplicate child | ``pipe 'gh-ci' has duplicate `command` `` |
| unknown child | ``pipe 'gh-ci' has unsupported field `on-error` `` |
| duplicate pipe | ``agent declares `pipe "gh-ci"` more than once`` |
| bad name | `agent 'demo' pipe name 'GH_CI' must match [a-z0-9] ([a-z0-9-]*[a-z0-9])?` |
| empty command | ``agent 'demo' pipe 'gh-ci' has an empty `command` `` |
| name collision | ``agent 'demo' declares both `pipe "gh-ci"` and a task named `pipe-gh-ci`; choose one form`` |

`st2 validate` on a catalog with three bad pipes:

```
ERROR  agents/hetz/bad/agent.kdl: pipe must declare exactly one of `command` or `argv`
ERROR  agents/hetz/bad/agent.kdl: pipe `every` is reserved for the future `schedule` contract; …
ERROR  agents/hetz/bad/agent.kdl: pipe task must have one positional string name
─ 4 errors, 0 warnings across 0 agents
```

---

## (d) How well the derived-companion seam fit

### Fit with no change at all

- **Coupling.** `reconcile.rs:685` (a derived task is not launchable unless its
  agent's session is alive) and `reconcile.rs:874` (an ineligible agent's derived
  companions are stopped) are generic over `Task::derived`. Both directions of
  the pipe⇄agent coupling worked on the first run.
- **Parking and crash-loop surfacing.** `execute`'s `GaveUp` branch builds a
  `CrashLoop` for ANY target, and only resets `agent_available` when the parked
  target is the canonical agent. A parked pipe therefore surfaces and leaves its
  agent alone with zero code change.
- **`is_runnable`** (`spec.rs:388`) filters derived tasks, so a pipe-only
  declaration is correctly `unrunnable`. Asserted
  (`a_pipe_alone_does_not_make_an_agent_runnable`) rather than bent.
- **`has_delivery_transport`** is name-scoped to `ding`, so a pipe cannot claim
  one. Asserted (`a_pipe_does_not_claim_a_delivery_transport`).
- **`st2 tasks --json`** wire shape: unchanged.
- **GC / keep / targeted reconciliation:** unchanged.

### What had to bend

1. **The derived marker carries no payload.** DING needed none — its whole
   invocation is derivable from the bus id. A pipe must carry the author's
   command through lowering to reconcile. Rather than add a field to `Task`, the
   payload rides in the **marker argv**, and reconcile asserts a fixed prefix
   before rewriting argv[0] and the `$ST_ROOT` slot. This is the contract
   `compile_driver_agent_tasks` already enforces on a driver expansion, so it is
   a reuse rather than a new mechanism — but it means the `Task` model still
   cannot describe a parameterized companion, and a third companion kind with
   structured parameters would repeat this.

2. **A task identity cannot be an idempotent bus sender.** This is the sharpest
   friction found. `message::send_to_resolved_inbox` resolves the SENDER to a
   catalog agent, because that agent's folder holds the sent-message idempotency
   ledger. A pipe companion is a task, not an agent, so
   `from = "hetz.demo.pipe-gh-ci"` fails with *"no agent … found in catalog"*.
   The only existing non-agent sender path (`message::send_to_inbox`, used by
   `surface_crash_loop`) bypasses the ledger entirely and would forfeit
   deduplication — the one property a pipe most needs.

   Worked around by sending as the owning agent, with provenance in the tags and
   an `x-st2-pipe-task:` body line. The visible cost is measured in
   `a_pipe_event_pokes_the_agent_as_a_self_addressed_notice`: the agent reads

   ```
   [DING] ↺ hetz.demo: pipe gh-ci: {"id":"run-7:success",…} [id:2f7b]
   ```

   The `↺` marker means "from myself" — precisely the wrong story for an external
   event. See (f).

3. **Task-name grammar had to be invented, not inherited.** There is no existing
   task-name validation: `pty "…"` / `exec "…"` accept any string. Since a pipe
   name becomes part of a runner-owned task name AND runtime id, this spike
   imposes `[a-z0-9] ([a-z0-9-]*[a-z0-9])?`, ≤40 chars, plus an explicit collision
   check against authored `pty`/`exec` names. Without the collision check, a
   duplicate runtime id would only surface later as a `st2 tasks` error.

4. **Startup grace is load-bearing, and was a real bug.** One pass launches the
   agent PTY and its companions together, so a pipe routinely probes before the
   pty registers its pidfile. A naive "owner not alive → exit" made a fresh pipe
   exit instantly (observed on the demo catalog) and would have crash-looped it
   into a park. Fixed by mirroring `ding::SessionWatch` exactly: never exit until
   the owner has been seen alive once, then a 3-miss debounce
   (`an_owner_that_has_not_registered_yet_does_not_end_the_run`).

5. **Presentation patching is not lifecycle.** See 4c.

### Inherited but NOT separately proven

Stated so the derived-companion framing does not imply coverage it lacks. These
enumerate `spec.tasks` and so should work, but this spike wrote no pipe-specific
test for them:

- `st2 doctor` retirement and suspension health predicates.
- Final GC (`remove`) of a dead retired pipe record.
- A **parked** pipe row through `st2 tasks --json` (parking is proven in
  `tests/run.rs`, and a healthy inventory row is proven; the combination is not).

---

## (e) Interactions with reserved `schedule`, and DQ1's four items

### `schedule` (README ~274-290, `declared.rs`)

`pipe` and `schedule` are independent nodes and neither absorbs the other; a
document with both reports only the schedule's rejection
(`a_pipe_and_the_reserved_schedule_node_are_independent_shapes`).

The interesting decision is `every`. An optional `every "30s"` on a pipe was
prototyped and then deliberately **rejected with its own diagnostic**. The
reasoning: an interval turns a pipe into scheduled work, which is the reserved
node's contract, and an interval-respawn loop inside `run_pipe` would make the
sidecar a second lifecycle owner — the exact thing this shape argues against.

The two concepts stay cleanly separable if `pipe` means *"a long-running process
whose stdout is events"* and `schedule` means *"st2 runs this on a cadence"*. A
polling source that wants a cadence should own its own sleep loop, or — better —
`schedule` should eventually be able to target a pipe. **Interval-respawn mode
was not implemented.** What it would take: a `PipeExit::SourceExited` that sleeps
and re-spawns rather than returning, which is ~15 lines but re-opens the
lifecycle-ownership question; the honest version is for the supervisor to grow a
"restart after N seconds, not immediately" policy, which `restart { delay }`
already almost expresses.

### DQ1's four unspecified items

- **KDL shape** — proposed and running (section c). Minimal: a name and one of
  `command`/`argv`.
- **Event inbox** — this spike uses the agent's ordinary inbox with `pipe` and
  `pipe:<name>` tags. It works, and the measured cost is that pipe events queue
  FIFO behind human and agent messages and consume the same DING budget. A
  high-rate pipe would drown the inbox. Whether pipe events deserve a separate
  box, or just a filter, is unresolved — but the `[DING]` frame carries **no
  body**, only sender and subject, so *any* answer must put the signal in the
  subject. That is why `summarize` exists.
- **Deduplication boundary** — the sharpest finding. Two regimes, both proven:
  - A source-supplied JSON `id` → key `pipe:<name>:id:<id>`. This is the source
    declaring which occurrences are the same occurrence, and it is the right
    boundary: replay dedups, genuinely new events land.
  - No id → key `pipe:<name>:sha:<fnv1a>` over the trimmed line. This
    **cannot distinguish** "the supervisor restarted us and the source replayed a
    still-current state" (dedup wanted) from "the same event legitimately
    happened twice" (dedup wrong). Demonstrated, not asserted:
    `an_id_less_source_dedups_by_content_which_also_collapses_real_repeats` emits
    `ci failed` twice from one run and gets **one** message.

    So: **an event id is not a nicety, it is the contract.** A pipe whose source
    cannot supply one has no sound dedup boundary. The design should probably
    make `id` mandatory-by-declaration (a `pipe "x" { id-field "run_id" }` or
    similar) rather than silently degrading.
  - Keys are namespaced per pipe, so two pipes on one agent cannot collide.
  - Key and body must derive from the SAME bytes, or a replay becomes a hard
    "idempotency key reused with different content" error instead of a reused
    receipt. This constrains the subject too (hence `summarize` being pure).
- **Execution receipts** — partially free. The bus's own archive filename is
  already a durable receipt, and the idempotency ledger in the sending agent's
  folder is a second one. `PipeReport` additionally reports which lines were
  in-run duplicates. What does NOT exist: any record of *how far the source got*
  before it died. A restarted pipe re-reads its source from scratch and relies
  entirely on dedup. For a poller that is correct; for a streaming source with a
  cursor, it is not, and nothing here addresses a cursor.

---

## (f) Frictions and recommended refactors

1. **Let a declared task identity own a sender ledger.** The "no agent
   'hetz.demo.pipe-gh-ci'" failure is the one place the seam genuinely does not
   reach. The mechanism already exists in a neighbouring shape — the *Idempotent
   service requests* invariant describes "a declared non-agent service principal"
   publishing one exact request per idempotency key. Extending that principal
   notion to runner-owned task identities would let a pipe be its own sender,
   which fixes the `↺` marker and makes provenance structural instead of a body
   line. **This is the top recommendation.**

2. **Give the derived-companion seam a payload.** Marker argv works and reuses an
   established pattern, but a third parameterized companion would justify a typed
   `Task::companion: Option<Companion>` and a single `compile_generated_companions`
   with an exhaustive match.

3. **Rename `compile_generated_ding_tasks`.** It now compiles two companion kinds
   and holds the fail-closed gate for all of them. Left unrenamed here to keep
   the spike diff legible.

4. **Add task-name validation for authored `pty`/`exec` too.** The pipe path now
   validates names; the authored path still does not, so `exec "a.b.c"` can
   produce a confusing runtime id.

5. **Make the event id declarable.** See DQ1 dedup above.

6. **Consider whether `patch:` belongs in the runner op stream at all**, or
   whether presentation should be a distinct trait — it made an otherwise clean
   "nothing else was touched" assertion need an exception.

---

## (g) Open questions only a human can settle

1. **Who is the sender of an ingested external event?** The owning agent
   (self-addressed, `↺`, works today), a per-pipe principal (needs the ledger
   change in (f)(1)), or the runner `st2.<host>` (loses idempotency)? This is a
   product question about what an agent should *read*, not a technical one.

2. **Is a source-supplied event id mandatory?** Content-hash dedup is unsound in
   a way that is invisible until it silently drops a real duplicate event.
   Refusing a pipe that cannot declare an id is safer and more annoying.

3. **Do pipe events belong in the agent's ordinary inbox?** They currently queue
   FIFO against human messages and consume DING budget. A busy CI pipe will
   change how an agent's inbox feels.

4. **Should `schedule` be able to target a `pipe`,** or should `pipe` grow its own
   cadence? This spike deliberately kept them apart, but a poller is the most
   obvious pipe and pollers want cadence.

5. **What happens to events while an agent is suspended?** Today the pipe is torn
   down with the agent, so events during suspension are simply not observed —
   there is no backfill. Correct for a live-CI feed, wrong for an audit trail.

6. **Should a parked pipe be louder than a parked task?** A silently parked pipe
   means the agent stops receiving a class of event with no local signal. The
   crash-loop message goes to the *supervisor*, not to the agent whose eyes just
   closed.

## VRS Impact

None yet — this is a spike. It supplies evidence toward DQ1 (KDL shape, event
inbox, deduplication boundary, execution receipts) and adds no requirement,
invariant, or ontology term. No INVARIANTS.md row was added: the derived-companion
lifecycle row already covers what the pipe inherits, and this spike introduces no
new load-bearing guarantee of its own.

## Method

E2e prototype in an isolated worktree of `main` (8ff140e): parse a nested
`pipe` declaration, lower it to a derived exec companion through the DING
seam, implement a supervising runner and emit path, then prove lifecycle
behavior by extending the derived-DING test fixtures — §(a)–§(g) document the
build, proofs, grammar, seam fit, and frictions; 27 new tests plus the eleven
named derived-DING lifecycle tests.

## Conclusion

The derived-companion seam carries stream sources with no changes to run,
flapping, park, or task inventory — the hypothesis holds cheaply. The two
findings that outlived the prototype: a task identity cannot be an idempotent
bus sender (the self-send workaround renders `↺` for an external event), and
a producer-supplied event id is the dedup contract (content hashes cannot
distinguish replay from repeat). Decisions 0004/0005-streams-are-agent-nested-and-stream-named supersede this
prototype's `pipe` naming and self-send emit path; its lifecycle proofs and
KDL shape carry forward into STREAM-R01 and STREAM-R08.
