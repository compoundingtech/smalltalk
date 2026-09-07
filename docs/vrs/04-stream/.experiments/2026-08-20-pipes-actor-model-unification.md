# Pipes: one actor model, one record family

Date: 2026-08-20

Branch: `schickling/2026-08-20-pipes`

## Question

st2 has three overlapping answers to "who can put a durable item into an agent's inbox":

1. **Ordinary agent messages** (`src/message.rs`) — the sender must be an Agent Spec identity; a
   sender-owned hash-chained ledger under `resources/sent/`; idempotency scoped
   `(sender, recipient, key)`; DING renders a relationship glyph.
2. **Service-principal requests** (`src/request.rs`) — a declared non-agent principal at
   `principals/<host>/<identity>/principal.kdl`; a *different* durable store under
   `resources/request-state/`; idempotency scoped `(principal, key)` with the recipient omitted;
   DING renders `?` and a hardcoded `request <key>` subject.
3. **Pipe sources** (prototyped, unmerged) — an external event producer supervised as a derived exec
   companion, with no bus identity of its own at all.

This document takes the unification hypothesis seriously: that all three are one *actor* publishing
one *record family*, and that the second mechanism exists because of an accident rather than a
distinction. It then tests that hypothesis against the codebase and reports where it holds.

**Conclusion: unification is the global maximum, and it is a net deletion.** The target model
removes one durable store, one CLI command group, one identity kind, one wire envelope family, and
the entire prototyped pipe runner, while adding one actor kind field, one frontmatter key, and one
DING glyph. The spike proves the seam that decides it.

## Evidence base

Two prototypes on this branch, read in full:

- `2026-08-20-pipes-ingress-prototype.md` — compared the message path and the principal path for
  external-event ingress. Recommended the principal path on identity and separation grounds, and
  listed four presentation defects plus one real correctness defect (fan-out collision).
- `2026-08-20-pipes-lifecycle-prototype.md` — proved `pipe "name" {}` lowers cleanly to a derived
  exec companion. Its **top recommendation** was: *"Let a declared task identity own a sender
  ledger… Extending that principal notion to runner-owned task identities would let a pipe be its
  own sender."*

Both prototypes, from opposite ends, arrived at the same missing piece: **a non-agent producer that
owns a real sender ledger.** That is the unification, stated as a defect report. This document names
it as a design and the spike proves it works.

---

## 1. The target model

### 1.1 Actors

One concept replaces three. A **bus actor** is a declared identity with a two-component bus id
`<host>.<identity>` and a home directory owning `resources/{inbox,archive,sent}`. Actors differ only
by **kind**, which is a property of the declaration site and never of a message:

| Kind | Declared at | Has | Does not have |
|---|---|---|---|
| `agent` | `agents/<host>/<identity>/agent.kdl` | persona, tasks, presence, roster row, DING sidecar, doctor obligations, supervisor edges | — |
| `service` | `principals/<host>/<identity>/principal.kdl` | mailbox, sender ledger, addressability | persona, task, presence, roster row, DING sidecar, doctor obligation, supervisor edge |

There is no third kind. A **pipe source is a `service` actor that declares a command**; a service
principal is a `service` actor that does not. This is the crux: the differentiation pole needs a
`pipe` concept *and* a `principal` concept; unification needs one.

`src/actor.rs` in this worktree is the prototype of this table.

**Identity grammar is explicitly two namespaces, and they do not merge.** Bus ids stay
two-component (`hetz.gh-ci`). Task ids stay three-component (`hetz.demo.ding`,
`hetz.demo.source-gh-ci`). A task is not an actor and never becomes one — which is precisely why the
lifecycle prototype's `from = "hetz.demo.pipe-gh-ci"` failed, and why the fix is to give the source a
real actor rather than to teach the ledger about task ids.

### 1.2 Declaration surface

The principal declaration grows two optional children and the agent declaration grows nothing:

```kdl
// today, unchanged: an external producer that calls the CLI itself
principal "example-ci" host="host-a"

// a supervised event source
principal "gh-ci" host="hetz" {
  serves "demo"                          // lifecycle owner; must be an agent on this host
  source { command "poll-gh-ci.sh" }     // or: argv "poll" "--json"
}
```

`source` requires `serves`. `serves` names the agent whose lifecycle the source is coupled to, and
lowers to a derived exec task `hetz.demo.source-gh-ci` — inheriting, unchanged, everything the
lifecycle prototype measured: restart policy, flapping and parking, suspend/retire teardown,
crash-loop surfacing, and `st2 tasks --json` reporting. `serves` is *not* the recipient: the source
addresses whomever it likes, and fan-out works because the key scope includes the recipient.

The runner injects `ST_PRINCIPAL=hetz.gh-ci` beside the existing `ST_AGENT`, so the source script
publishes as itself.

**What the source command does is call the ordinary CLI.** This is the largest single deletion in
the design:

```sh
st2 message send hetz.demo \
  --idempotency-key "$run_id" \
  --subject "CI $state on PR #42" \
  -m "$payload"
```

The lifecycle prototype's `src/pipe.rs` (505 lines), its `st2 pipe run` command, its JSON-line
protocol, its `summarize` function, and its content-hash dedup fallback all exist *only* because a
task had no identity and therefore could not be an idempotent sender. Once the source is an actor,
none of it is needed. The content-hash fallback is worth calling out separately: the prototype proved
it unsound (`an_id_less_source_dedups_by_content_which_also_collapses_real_repeats`), and this design
deletes it rather than shipping it — a source that cannot supply an event id passes no
`--idempotency-key` and gets at-least-once, honestly.

### 1.3 One record family

The inbox record is **already** one family today — a message file with frontmatter, where a typed
request is distinguished only by `tags: st2-request` and a JSON body. The divergence is entirely on
the producer side. So unification does not invent a merged record; it deletes the second producer
store and makes the existing discriminator typed.

Frontmatter gains exactly one key:

```
---
from: hetz.gh-ci
subject: CI failure on PR #42
kind: event
idempotency-key: run-7
---
build job failed
```

`kind` is one of `message` (default when absent), `request`, `reply`, `event`. `parse_message`
ignores unknown frontmatter keys (`src/message.rs`, the `_ => {}` arm), so every shipped reader
tolerates it and every older `st2` binary reads the file correctly, just without the discriminator.

**Note what is *absent* from that frontmatter.** The ingress prototype's spike had to add `source:`
and `event-id:` fields, because `from:` was forced to name a fake agent and therefore could not name
the producer. Under unification `from:` *is* the producer and `idempotency-key` *is* the event
identity, so issue #137's four provenance questions are answered with **zero new provenance fields**:

| #137 asks | Answered by |
|---|---|
| which producer emitted it? | `from` — a declared actor, resolvable to a kind |
| what external event ID caused it? | `idempotency-key` |
| which message/receipt represents it? | `filename` |
| newly accepted or deduplicated? | the send receipt's `deduplicated` flag |

### 1.4 One durable producer store

`resources/sent/` — the existing head / active / pending / messages / commits / keys ledger — for
every actor, with idempotency uniformly scoped `(from, to, key)`.

`resources/request-state/` is **deleted entirely**. Both of its halves are subsumed exactly:

- `request-state/outgoing/<sha256(key)>.json` → `sent/keys/<sha256(to,key)>.json`. This is a strict
  *improvement*: the request store omits the recipient from the key, which is the fan-out collision
  the ingress prototype measured (`one_event_fans_out_on_rep1_but_collides_on_rep2`). The unified
  scope fixes it by construction, per MESSAGE-R07.
- `request-state/replies/<sha256(reply_to \0 key)>.json` → the same `sent/keys` entry on the
  replying agent. Today's reply state key is `(agent, principal, key)`; `sent/keys` is
  `(from, to, key)`. **Identical scope.** The subsumption is exact, not approximate.

`request status` reads `request-state/outgoing` to recover the filename, then scans the principal's
inbox/archive for a matching reply. Under unification the filename comes from `sent/keys` and the
reply scan is unchanged.

### 1.5 CLI surface

`request` disappears as a command group; its one genuinely distinct capability is generalized.

| Today | Target |
|---|---|
| `request send` | `message send --kind request --idempotency-key …` |
| `request read` | `message read --json` (the body is the caller's JSON; `kind`, `from`, and `idempotency-key` are structured frontmatter) |
| `request reply` | `message reply <filename>` — recipient and threading already derive from the inbox item |
| `request status --idempotency-key` | `message status --idempotency-key … --to …` — now available to **every** actor, so an agent can finally ask "was my keyed message answered?", which it cannot today |

Net: 11 commands in two groups → 8 commands in one group.

### 1.6 DING glyphs

The vocabulary is code-only — there is no glyph table anywhere in `docs/vrs/`, so this amends no
document. Today: `↺` self, `↓`×n from a supervisor, `↑`×n from a report, `←` from a peer, `?`
fallback. Target adds `»` for a declared non-agent actor.

This is +1 glyph and it is worth it, because it removes two measured lies and one overload:

```
today, routed through an agent (ingress Rep 1):   [DING] ← h.pipe-agent: CI failure on PR #42
today, routed through the owning agent (lifecycle): [DING] ↺ hetz.demo: pipe gh-ci: {"id":…}
today, routed through a principal (ingress Rep 2): [DING] ? h.pipe-gh-ci: request github:ci#run-11
target:                                            [DING] » hetz.gh-ci: CI failure on PR #42
```

`?` goes back to meaning only "unknown or unreadable", and the hardcoded `request <key>` subject —
which `request::publish` bakes into the durable record and which is the only thing DING shows — dies
with `request send`.

The glyph character itself is a taste decision (see §7).

### 1.7 Deleting the third identity kind

`ExternalInbox` (`src/message.rs`) is a path-based, undeclared identity that exists solely because
the eval flow needs one non-agent requester mailbox — MESSAGE-A01 calls it "an explicit compatibility
capability". Under unification the eval flow declares a `service` actor and `ExternalInbox`,
`resolve_inbox_with_external`, the `external_sender` branch in `send_to_resolved_inbox`, the
`external: Option<&ExternalInbox>` parameter threaded through five functions, and MESSAGE-A01's
carve-out all disappear.

This matters for the objective function: unification **narrows** the identity surface from three
kinds to two. It is the strongest available answer to the "does this become an identity platform?"
objection.

Call sites are shallow (`src/eval_run.rs:1151`, `src/main.rs:2169`, `src/main.rs:2466`), but the
spike did not do this work, so it is a proposal with a verified cost estimate, not a proven one.

---

## 2. Concept count: today vs target

Counted conservatively. `Flat`/`catalogless` is a recovery compatibility mode, not an identity kind,
and is excluded from both columns. The "today" column counts what ships plus what the differentiation
pole would add for pipes, since that is the real comparison.

| Axis | Today (shipped) | Differentiation pole (ships pipes as a third mechanism) | **Target** |
|---|---|---|---|
| Identity kinds | 3 (agent, principal, eval external requester) | 3 | **2** (agent, service) |
| Producer declaration nodes | 1 (`principal`) | 2 (`principal`, `pipe` on agent) | **1** (`principal`, +2 optional children) |
| Durable producer stores | 2 (`sent/`, `request-state/{outgoing,replies}`) | 2 | **1** (`sent/`) |
| Idempotency key scopes | 2 (`(from,to,key)`, `(from,key)`) | 2 | **1** (`(from,to,key)`) |
| Publication transactions in code | 2 (`send_with_ledger`, `publish_once`) | 2 | **1** |
| Typed request/reply envelope types | 3 (`RequestEnvelope`, `ReplyEnvelope`, `PublicationRecord`) — all with `deny_unknown_fields` | 3 | **0** (the message record carries `kind`) |
| Message frontmatter keys | 6 (`from`, `subject`, `in-reply-to`, `tags`, `priority`, `idempotency-key`) | 8 (`+source`, `+event-id`) | **7** (`+kind`) |
| CLI command groups / commands | 2 / 11 | 3 / 12 (`+pipe run`) | **1 / 8** |
| DING glyphs | 5 | 5 (with two lies and one overload) | **6** |
| Event-source runner | — | `src/pipe.rs`, 505 lines + JSON-line protocol + content-hash dedup | **0** (the source calls the CLI) |
| Invariant rows touching publication | 2 | 2 | **1** (merged; see §4) |

Every axis is flat or down against today, and every axis is down against the differentiation pole.

---

## 3. Delta from today, by module

| Module | Change |
|---|---|
| `src/actor.rs` | **new.** The actor table: `ActorKind`, `Actor`, `resolve_service`, `service_bus_ids`. ~80 lines. |
| `src/message.rs` | `DeliveryEndpoint::Service` variant; service branch in `resolve_delivery_endpoint`, in the sender resolution inside `send_to_resolved_inbox`, and in `with_resolved_state_dir`. **Also required and not done in the spike:** the same branch in `resolve_list_box` and `archive_resolved_message`, without which `message ls`/`read`/`archive` cannot reach a service actor's mailbox (§5). Later: delete `ExternalInbox` and the `external` parameter from five signatures; add `kind` to `Message`, `SentRecord`, and the frontmatter renderer/parser. |
| `src/request.rs` | **deleted.** `publish_once`, `record_path`, `atomic_create`, `RequestEnvelope`, `ReplyEnvelope`, `PublicationRecord`, and the hardcoded subject all go. `discover_principals` and `ServicePrincipal` (~100 lines) relocate to `src/actor.rs`; the remaining ~400 lines are deleted outright. |
| `src/ding/mod.rs` | `RelationshipResolver` gains `services`; `relationship_marker` gains the `»` branch *after* the fail-closed validity check. |
| `src/main.rs` | `RequestCmd` deleted; `MessageCmd::Status` added; `--kind` on send. |
| `crates/st2-wire/src/message.rs` | `MessageRow.kind` / `SentMessageRow.kind`, optional, no `deny_unknown_fields` (unchanged policy). |
| `crates/agent-spec/src/spec.rs`, `kdl_format.rs`, `declared.rs` | `serves` + `source` on the principal declaration; source-task synthesis. No `pipe` node on the agent. |
| `src/reconcile.rs` | Extend the "unsupported derived task" gate to source companions — one arm, exactly as the lifecycle prototype measured. |
| `src/eval_run.rs` | Declare a service actor instead of provisioning an `ExternalInbox`. |

---

## 4. Requirements and invariants: survives, re-expressed, or amended

### Survives unchanged

- **MESSAGE-R01/R02/R03/R04/R05/R06/R08/R09/R10** — the ledger transaction, recipient-first
  ordering, coverage honesty, shared wire, resumable intent, serialized writes, directional filters,
  and the catalog-scale read are all untouched. The spike exercises R05/R06 for a non-agent sender at
  every checkpoint.
- **MESSAGE-R07 exact keyed retry** — *strengthened by extension.* The `(sender, recipient, key)`
  scope now governs every actor, which is exactly what removes the request path's fan-out collision.
- **Exactly-once-safe native bus** — filename grammar and archive receipts untouched.
- **Fail-closed observed native DING** — the new glyph is unreachable while
  `!resolver.valid`, proven by a dedicated test (§5).
- **Derived companion lifecycle** — a source companion is a derived task; the lifecycle prototype
  proved the seam is keyed on `Task::derived`, not on the name `ding`.
- **Runner-owned task identity** — unchanged; `ST_PRINCIPAL` is additive and does not touch
  `ST_AGENT`.
- **R19, R23, R24** — stable identity, targeted reconciliation, and the task inventory wire shape are
  untouched. The lifecycle prototype already proved a derived source task reports honestly through
  `st2 tasks --json` with no schema change.

### Re-expressed (same purpose, different mechanism)

- **MESSAGE-R11 typed-request separation.** Today: "request publication state does not appear in
  ordinary Agent Sent history", enforced by *keeping a separate store*. Target: enforced by a *kind
  projection* over one store — `message sent` defaults to `--kind message`.

  This is the amendment that most deserves scrutiny, so state its purpose plainly: R11 exists so that
  Sent is an honest record of an agent's conversations rather than a machine activity feed. A
  kind-filtered projection preserves that purpose exactly while deleting a store. What it gives up is
  *structural* enforcement — today an agent physically cannot see request state in Sent; tomorrow it
  is a default filter. Mitigation: `kind` is typed frontmatter set by the publisher at render time
  and carried into the immutable `SentRecord`, so it cannot be retro-edited without failing the
  row-digest check. The proposed R11 text:

  > **MESSAGE-R11 Typed-record separation:** `message sent` enumerates only rows whose kind is
  > `message` unless a kind filter selects otherwise. Request, reply, and event rows remain
  > sender-owned and durable but are not ordinary Agent Sent history.

- **MESSAGE-A01 declared senders.** The eval external-requester carve-out is deleted; the assumption
  becomes "a catalog-backed sender is a declared actor", which is a *narrower* statement than today's
  because it removes the undeclared path-based identity.

- **Idempotent service requests** (INVARIANTS row). Merges into the message row. The four named
  proofs in `tests/request_cli.rs` are the guarantee's current evidence, and
  `tests/invariants.rs::qualified_proof_references_resolve` fails if they are deleted without
  replacement. The migration order is therefore fixed: land equivalent proofs on the unified path,
  edit the row to name them, **then** delete `request.rs`. The spike keeps all four green (§5).

### Deliberately amended

- **R10 Agent-only identity** — verbatim: *"st2 models agents. Non-agent identities are
  unsupported."*

  This requirement's letter is **already violated by shipped code**: `docs/vrs/spec.md` §"Service-principal
  request transport" declares `principals/<host>/<identity>/principal.kdl` and `st2 request send`
  ships today. R10 has not been reconciled with it. Unification does not widen the breach — it
  reduces the number of non-agent identity kinds from three to two and makes the remaining one
  declared, catalog-visible, and validated. Proposed text:

  > **R10 Declared-actor identity:** st2 models agents. Every other bus participant is a declared
  > non-agent actor with a catalog declaration, a bus id, and a mailbox, and with no persona, task,
  > presence, roster, delivery-transport, or supervisory authority. Undeclared identities are
  > unsupported.

- **Vision, "What This Is Not"** — verbatim: *"A general messaging or identity platform for people,
  services, or arbitrary non-agent actors while the agent and st2 specs are still stabilizing."*

  The carve-out holds without amendment, and the operative word is **arbitrary**. A service actor is
  not arbitrary: it requires a catalog declaration whose content must exactly match its path, it
  cannot collide with an Agent Spec identity, it acquires no authority over any agent, and it cannot
  claim a delivery transport. The vision clause excludes an *open* identity platform; the target
  model is a *closed, declared* two-kind table. If anything the spirit is better served after the
  change, because the undeclared `ExternalInbox` path — which genuinely is an ad-hoc identity — is
  the thing being deleted.

### New ontology terms

`docs/vrs/ontology.md` has no entry for `principal`, `request`, or `inbox` today (its rule is that
terms without an independent authority are deliberately absent). The target adds two with real
authorities: **bus actor** (`src/actor.rs::Actor`) and **actor kind** (`src/actor.rs::ActorKind`), and
retires the term "service principal" in favour of "service actor". Cost: +2 ontology entries.

---

## 5. The spike

### What was chosen and why

The riskiest seam is **whether a non-agent actor can own the real durable sender ledger**. If it
cannot, `request-state` must survive, the fan-out defect stays, pipes need a third mechanism, and the
whole design collapses into a rename. Everything else in the design is presentation, naming, or
mechanical deletion.

The advisor's sharpening was decisive here: both prototypes proved *concurrent replay* for their own
sender, but **neither proved crash recovery for a non-agent sender**. The principal path gets its
crash safety from a pre-reserved filename — a completely different mechanism — so its evidence says
nothing about whether the ledger transaction works when the actor is not an agent. And
`recover_active` re-resolves the recipient endpoint mid-recovery, so the recovery path exercises the
actor table independently of the happy path. That is the discriminating question.

### The feasibility signal

`send_with_ledger` is already generic over `sender_root: &Path` and `from: &str`. The entire agent
coupling lives one layer up, in resolution. This predicted the spike would succeed, and it did.

### Code

| Change | Where |
|---|---|
| The actor table | `src/actor.rs` (new, ~140 lines with tests) |
| `DeliveryEndpoint::Service`; service branches in `resolve_delivery_endpoint`, sender resolution, `with_resolved_state_dir` | `src/message.rs` |
| `»` marker, `services` on the resolver, fail-closed ordering | `src/ding/mod.rs` |
| E2E evidence | `tests/actor_unification.rs` (new, 6 tests) |
| Marker evidence | `src/ding/mod.rs::tests` (2 tests) |
| Actor-table evidence | `src/actor.rs::tests` (4 tests) |

`request.rs` was **not** deleted in the spike. The point was to prove the seam, not to perform the
migration; leaving it in place also proves the two paths coexist, which is what the migration
requires (§6).

### Results — 13 tests, all green

**The discriminator.** `tests/actor_unification.rs::a_service_actor_recovers_from_a_crash_at_every_publication_checkpoint`
injects a failure after each of the nine publication checkpoints (`coverage`, `pending`, `active`,
`recipient`, `row`, `node`, `head`, `pending-cleanup`, `active-cleanup`) with a non-agent sender, then
retries. For every checkpoint: exactly one durable inbox item, the retry reports the delivered
filename, a third replay adds nothing, and `message sent` reports complete `since` coverage rather
than `partial`. **A non-agent actor owns the ledger with full crash-recovery semantics.**

**The seam.** `a_service_actor_owns_the_same_durable_sender_ledger_as_an_agent` — a principal's send
creates `principals/h/gh-ci/resources/sent/{index.json,messages,commits,keys}` and creates **no**
`request-state` directory; `message sent h.gh-ci --json` returns `coverage._tag = "since"` with the
row's `to`, `subject`, and `idempotencyKey`.

**The measured defect, fixed.** `one_event_key_fans_out_to_two_agents_instead_of_colliding` — one
key, two recipients, two messages, two sender rows. This is the exact case the ingress prototype
recorded as `Error: idempotency key reused with different request`. Conflicting reuse of one scoped
key still fails per recipient.

**The reply channel, for free.** `an_agent_replies_to_a_service_actor_over_the_ordinary_message_path`
— `message reply` reaches the service actor's inbox and produces an ordinary sender-owned row with
`to = h.gh-ci`. No typed envelope, no `request reply`.

**Additivity.** `an_agent_identity_still_wins_over_an_identically_named_service_actor` (agents win
every ambiguity, so no shipped address can be re-pointed) and `an_undeclared_sender_is_still_refused`
(publication authority unchanged).

**DING.** `src/ding/tests.rs::a_declared_service_actor_renders_its_own_marker_rather_than_unknown`
renders `[DING] » h.gh-ci: CI failure on PR #42 [id:abc123]` while an undeclared sender still renders
`?`. `an_invalid_catalog_cannot_promote_a_service_sender_out_of_unknown` is the fail-closed gate: a
broken declaration anywhere in the catalog forces `?` even for a genuinely declared service actor, so
the new glyph cannot be reached on a guess.

**Actor table.** `src/actor.rs::tests` — resolution by bus id and by local bare identity agree; a bare
identity does not reach another host's actor; a malformed `principals/` tree yields *no* actors rather
than a partial table; an absent tree is simply empty.

### Regression status

Green after the change: `--test actor_unification` (6), `--test message` (6), `--test message_cli`
(17), `--test request_cli` (9), `--test invariants` (1), `--lib` (see §8).

All four `tests/request_cli.rs` proofs named in the "Idempotent service requests" invariant row are
green, and `tests/invariants.rs::qualified_proof_references_resolve` passes.

### What the spike did NOT prove

Stated so the design is not credited with coverage it lacks:

- **The read path is only half extended, and this is measured, not assumed.**
  `the_service_actor_read_path_boundary_is_exactly_where_resolution_was_extended` pins it: `message
  sent` reaches a service actor (via `with_resolved_state_dir`), while `message ls` and `message
  read` still fail with `no agent 'h.gh-ci' found in catalog` because `resolve_list_box` and
  `archive_resolved_message` were not extended. Delivery, publication, and recovery — the parts the
  design's central claim rests on — are unaffected; a service actor's mailbox is simply not yet
  readable through the ordinary list commands. The test asserts the *current* failure, so it turns
  red the moment the real implementation closes the gap.
- `request.rs` deletion and the `request-state` → `sent/keys` subsumption. Argued exactly in §1.4;
  not executed.
- `ExternalInbox` deletion and the eval-flow refactor.
- The `kind` frontmatter key and the R11 projection.
- `serves` / `source` KDL lowering and source-task synthesis. The lifecycle prototype proved the
  derived-companion seam for a `pipe` node; the same seam under a different declaration site is very
  likely but unproven.
- `message status`.
- High-volume ledger behavior (§7).

---

## 6. Migration and compatibility

### The constraint that decides the shape

`src/request.rs` puts `deny_unknown_fields` on all three of `RequestEnvelope`, `ReplyEnvelope`, and
`PublicationRecord` (lines 41, 53, 64), and every one of `request read`/`reply`/`status` parses
through them. So a new field in the request envelope is a **hard parse failure** in an older binary,
not a tolerated addition. In-place evolution of the request wire is therefore not available.

The message wire is the opposite: `crates/st2-wire` documents that no type uses
`deny_unknown_fields`, and `parse_message` ignores unknown frontmatter keys outright. Adding `kind`
is safe in both directions.

This settles the migration on the house pattern of fresh namespaces over in-place migration.

### Migration plan

1. **Land `src/actor.rs` and the service-sender seam.** Purely additive — agents win every ambiguity,
   so no existing address changes. This is what the spike already does, and it is independently
   shippable.
2. **Land `kind` frontmatter and the R11 projection.** Old readers ignore it; `message sent` behavior
   is unchanged for agents because absent `kind` defaults to `message`.
3. **Land `message status` and equivalent proofs for the four request invariants on the unified
   path.** Edit the INVARIANTS row to name the new proofs. `qualified_proof_references_resolve` makes
   this order mandatory.
4. **Deprecate `request send/read/reply/status`.** They keep working against `request-state`.
5. **Delete `request.rs` and `request-state`.**

### What happens to existing state

- **Existing catalogs**: unchanged. `agents/` and `principals/` keep their paths and their content
  rules; `principal.kdl` gains only optional children.
- **Existing messages**: unchanged on disk. Every inbox item without `kind` reads as
  `kind: message`, which is what it is.
- **Existing sender ledgers**: unchanged. `sent/` gains no new files; `SentRecord` gains one optional
  field, and old rows without it deserialize with `kind = message`.
- **Existing `request-state` records**: **not migrated.** They are producer-side dedup receipts, and
  the honest consequence is that a request published under the old path and replayed after the
  cutover would publish once more. Mitigation: step 4's deprecation window is where a producer
  re-keys, and the receipts are already unpruned garbage today (the ingress prototype's open question
  #4: "nothing prunes them today"). Deleting the directory at cutover is correct and should be stated
  in the release note rather than papered over with a converter.
- **In-flight typed requests** across the step-5 boundary lose their `request status` channel. The
  deprecation window in step 4 exists for exactly this.

### Verdict

Compatible for readers and for on-disk message state at every step; a deliberate, announced
one-time loss of producer-side request dedup receipts at the final cutover, with a deprecation window
to drain them.

---

## 7. Costs and risks

1. **One ledger shape for wildly different volumes.** Every actor gets the full hash-chained coverage
   ledger. A CI poller emitting 100k events accumulates a Merkle chain nobody reads, and `message
   sent` on that actor is O(history). Writes stay O(1) and MESSAGE-R10's p95 gate is about catalog
   scale (557 agents, one sender row), not history depth — so the gate is not threatened, but the
   storage is real.

   **This is the strongest honest objection and it must not be soft-pedalled.** The alternative — a
   `keys/`-only ledger profile for service actors — would be a kind-dependent store shape, which
   quietly re-splits the store this design exists to merge. The position taken here is: one shape,
   and unbounded growth becomes *one* retention problem instead of two. It is worth noting that
   `request-state` has the identical unbounded-growth problem today and nothing prunes it either, so
   unification does not add the problem; it makes there be one place to fix it. If measurement later
   shows a high-rate source is untenable, the honest fix is a retention rule on `sent/`, not a second
   store.

2. **R11 loses structural enforcement.** Discussed in §4. A default filter is weaker than a separate
   directory. The mitigation (typed frontmatter carried into the digest-protected immutable row) is
   real but is not the same guarantee.

3. **Declaration locality.** `serves "demo"` puts a source's existence in `principals/` rather than in
   the agent's own declaration, so reading `agent.kdl` no longer tells you everything attached to that
   agent. The lifecycle prototype's `pipe` node had better locality. Recoverable via `st2 tasks
   --json` and `st2 agents`, but it is a genuine ergonomic loss and a reasonable person could go the
   other way.

4. **`request status`'s reply correlation is thinner than it looks.** `status` scans the principal's
   inbox and archive for a matching reply. Generalizing it to `message status` for every actor means
   that scan runs on agent-sized mailboxes. Bounded by the mailbox, not by history, but unmeasured.

5. **`+1` DING glyph is a string every agent sees.** Changing the notice vocabulary is a fleet-wide
   behavior change even though no VRS document specifies it.

6. **The `serves` requirement is a real restriction.** A source with no owning agent has no lifecycle
   owner and is refused. That is the right v1 answer — a second lifecycle owner is exactly what the
   lifecycle prototype argued against — but it means a fan-out-only source must nominate an arbitrary
   agent as its supervisor, which is slightly dishonest.

7. **Two rules for one question, in the spike and to be avoided in the implementation.** The `»`
   branch in `relationship_marker` matches a local bare identity by string surgery
   (`strip_prefix(this_host)` then `strip_prefix('.')`), while `actor::resolve_service` matches on
   `principal.host == this_host && principal.identity == id`. They agree today, but the DING marker
   and the delivery path can drift apart. In the target model the marker must call the actor table
   rather than re-derive it — the whole point of having one table. Likewise, the service arm of
   `with_resolved_state_dir` ignores the `create` parameter the agent arm honors via
   `open_message_box`; harmless for the spike's callers, and not to be inherited silently.

8. **Scope.** This is a refactor across `message.rs`, `request.rs`, `ding/mod.rs`, `main.rs`,
   `eval_run.rs`, `agent-spec`, and `reconcile.rs`, touching two INVARIANTS rows. The staging in §6
   makes each step independently shippable and reversible, but the total is much larger than "add a
   `pipe` node".

---

## 8. Steelman for the opposite pole

The differentiation argument deserves its strongest form, because it is not weak.

`request.rs` has its own tiny `publish_once` **for a reason**: a CI poller needs a reserved filename
and nothing else. It does not need a hash chain, a coverage tag, a per-sender flock, or the concept
of "complete sender history". The ledger is 400 lines of machinery serving a requirement — *an agent
must be able to prove what it sent* — that no machine producer has. Unification does not delete that
machinery; it **imposes** it on every producer, and then §7.1 has to argue away the consequences.

Meanwhile, the shipped principal path already satisfies both boundaries the ingress prototype cared
about (R10 and MESSAGE-R11), and its defects are four small, local fixes: add `--subject`, add a
marker glyph, add `to` to the receipt, and put the recipient in the state key. That is perhaps 100
lines against a multi-module refactor that amends R10, re-expresses MESSAGE-R11, and merges two
INVARIANTS rows. On a pure cost-to-fix-the-known-defects basis, differentiation wins outright.

**Why I still recommend unification.** Two reasons the differentiated path cannot answer:

- Fixing the state key to `(principal, recipient, key)` is not a small fix — it is
  *re-deriving MESSAGE-R07 in a second place*. The second mechanism does not stay small; it converges
  on the first one defect by defect. The four "small local fixes" are each a step of that
  convergence, and the endpoint of that process is two implementations of the same thing.
- The lifecycle prototype's top recommendation is unreachable from the differentiated pole. A pipe
  source needs a sender ledger *and* recipient-scoped idempotency *and* an honest `from`. Give
  `request-state` all three and it **is** `sent/`. Differentiation ships a third mechanism (`pipe`
  node, `pipe run`, JSON-line protocol, content-hash dedup) to avoid admitting that.

The objective function was "a design that deletes two mechanisms and adds one is winning; a design
that adds a clean-looking third while the old two survive is losing." Differentiation is, by
construction, the second one.

---

## 9. Decisions that remain genuinely human

Not derivable — taste, policy, or product:

1. **The glyph.** `»` is a proposal. It is a string every agent in the fleet reads.
2. **Declaration locality (§7.3).** `principal { serves }` versus `agent { source }`. Both work; the
   tradeoff is single-source-of-truth for identity versus locality when reading an agent.
3. **Does R11's purpose survive a default filter?** §4 argues yes. This is a judgment about what
   "honest Sent history" means, and it is Johannes's and Nathan's call, not a derivation.
4. **Is one ledger shape acceptable for a high-rate source (§7.1)?** The alternative re-splits the
   store. This is the one place where the design's central claim could be traded away for
   performance, and it should be traded knowingly or not at all.
5. **Do pipe events belong in the agent's ordinary inbox?** Unchanged from both prototypes' open
   questions. A busy CI source changes how an inbox feels, and #238's bounded-body delivery makes the
   budget question sharper.
6. **What happens to events while an agent is suspended?** The source is torn down with its agent, so
   events during suspension are not observed. Correct for a live CI feed, wrong for an audit trail.
7. **Is a source-supplied event id mandatory?** This design makes it *optional and honest*: no key
   means at-least-once, rather than the prototype's unsound content-hash dedup. Refusing such a source
   outright is safer and more annoying.

---

## 10. Reproduce

```sh
nix develop -c cargo test --test actor_unification
nix develop -c cargo test --lib actor::
nix develop -c cargo test --lib -- ding::tests::a_declared_service_actor ding::tests::an_invalid_catalog_cannot_promote
nix develop -c cargo test --test message --test message_cli --test request_cli --test invariants
```

## VRS Impact

None yet. This is a spike record. It carries no requirement, no ontology term, and no INVARIANTS
entry. It is evidence for DQ1's KDL-shape, event-inbox, deduplication-boundary, and
execution-receipt items, and it proposes — but does not land — amendments to R10, MESSAGE-A01, and
MESSAGE-R11 that a human must accept before any of them are written down.

## Method

Design exploration from the unification pole with an executable spike of the
riskiest seam: §1–§4 derive the target model and its delta from the code and
VRS on `main` (8ff140e); §5 documents the spike (a service actor owning the
durable sender ledger, crash injection at every publication checkpoint,
fan-out, DING marker rendering), run as 13 tests in the exploration worktree.

## Result

See §2 for the concept-count table and §5 for the spike outcomes: 13 tests
green, crash recovery proven at all nine publication checkpoints for a
non-agent sender, fan-out fixed by construction, `»` marker rendered with a
fail-closed gate; one self-claim disproved (`message ls`/`read` for service
actors remains unextended). Full-workspace failure set identical to baseline.

## Conclusion

Unification is safe and flattens every concept axis, but it imposes the
sender ledger's prove-what-I-said machinery on machine producers; §8's
steelman concedes the misfit for high-volume sources. Decision 0004 selected
the differentiation pole's record model while adopting this exploration's
identity convergence and fan-out fix.
