# Pipes: a first-class event model (differentiation pole)

Date: 2026-08-20

Branch: `worktree-agent-ae190cd91778e7149`, based on `8ff140e` (the tip of
`schickling/2026-08-20-pipes`, which is held by another worktree)

## Question

Who or what may put a durable item into an agent's inbox, and how is the producer
identified across the whole system?

This document starts from the hypothesis that **conversational messages and
world-events are genuinely different concepts** and that the global maximum keeps
them distinct. It tests that hypothesis against the two prototype spikes, the
shipped code, and the sacred invariant set, and reports where the split earns its
keep and where it does not.

Companion explorations: the ingress prototype
(`.../agent-ae38fe3de554a4a01/docs/vrs/.experiments/2026-08-20-pipes-ingress-prototype.md`)
and the lifecycle prototype
(`.../agent-a540e72fcb3229f96/docs/vrs/.experiments/2026-08-20-pipes-lifecycle-prototype.md`).
This is the differentiation-pole design; a unification-pole design was explored
independently.

## Summary of the verdict

The differentiation hypothesis survives on two axes and loses decisively on four.
The recommended model **differentiates the record kind and the producer identity**
— stream identity, supersession, bounded retention, a distinct notice — and
**unifies the transport, the storage location, the read commands, and the reply
path**. The pure differentiation design, with events in their own durable store
behind their own DING notice path, is measured and rejected in §7.

Concretely:

- keeps ordinary agent↔agent messages **completely untouched**;
- collapses today's five producer identity kinds into **two** — *agent* and
  *source*;
- makes an **event** a distinct record kind carried in the ordinary inbox, with
  `(from, stream, event-id)` identity, producer-side supersession, and *bounded*
  dedup retention that ordinary messages deliberately do not have;
- **absorbs service-principal requests into the event model**, deleting the typed
  request/reply envelope pair, `request-state/replies/`, and two CLI commands;
- gives events their own DING glyph, so `?` stops doubling as "legitimate machine
  producer" and goes back to meaning only "st2 could not resolve this".

A "pure" differentiation design — events in their own durable store with their own
DING notice path — was evaluated and rejected on measured cost. See
[Rejected: a separate event store](#7-rejected-a-separate-event-store).

---

## 1. What exists today

### 1.1 Five things can already write an agent's inbox

R10 says "st2 models agents. Non-agent identities are unsupported." That
requirement is already carved out five ways in shipped code:

| # | Producer identity | Where | Ledger | DING glyph |
|---|---|---|---|---|
| 1 | Agent Spec bus id | `message::send_to_resolved_inbox` | `resources/sent/` (9 file kinds) | `↺ ↓ ↑ ←` |
| 2 | Service principal | `request::publish`, `principals/<host>/<id>/principal.kdl` | `resources/request-state/outgoing/` | `?` |
| 3 | Eval external requester | `message::ExternalInbox` (MESSAGE-A01 compat) | none | `?` |
| 4 | The runner, as `st2.<host>` | `src/run.rs:2107` `surface_crash_loop` → `send_to_inbox` | none | `?` |
| 5 | The literal string `st2` | `src/eval_run.rs:1031` → `send_to_inbox` | none | `?` |

The pipe prototypes add a sixth attempt and both fail on it:

- The **lifecycle prototype** could not give a runner-owned task identity a sender
  ledger, so a pipe sends *as its owning agent* and the agent reads
  `[DING] ↺ hetz.demo: pipe gh-ci: …` — an external CI event presented as the
  agent talking to itself. Its own top recommendation (f)(1) is to fix exactly
  this.
- The **ingress prototype** proved the message path *cannot* express a non-agent
  producer at all (`rep1_refuses_a_non_agent_sender_but_rep2_accepts_a_declared_principal`)
  and that forcing one into an Agent Spec buys a roster row, a presence record, a
  derived DING sidecar, and doctor health obligations that a CI poller never
  wanted.

So the honest framing of R10 is not whether to introduce a non-agent identity —
st2 already has four of them, three undeclared and unattributed. The design
question is **how few** identity kinds the system can have.

### 1.2 Requests are already an event plus a message thread

`src/request.rs:346 status()` is the sharpest single piece of evidence in this
document. Its implementation of the "typed reply channel" is:

```rust
for directory in [principal.inbox(), principal.archive()] {
    for candidate in message::list_dir(&directory)? {
        if candidate.in_reply_to.as_deref() != Some(record.filename.as_str()) { continue; }
        …
    }
}
```

That is a **thread scan for an ordinary message reply**, wrapped in JSON-envelope
validation. The `ReplyEnvelope`, the `st2-request-reply` tag, and
`request-state/replies/` exist only to re-derive facts that `in-reply-to` and
`from` already carry on the message file. The typed layer is not buying
routing — it is buying schema validation on a body, which the producer could do
itself.

Its measured costs are real:

- `request::publish` hardcodes `Some(&format!("request {idempotency_key}"))` as
  the subject, and the subject is the only thing DING shows. The ingress
  prototype's poller run produced four consecutive notices that say nothing:
  `[DING] ? h.pipe-gh-ci: request github:ci:pr-42#pending`.
- The body is an opaque JSON envelope, so `message read` / `message thread` are
  useless on it.
- The state key omits the recipient, so **one event cannot fan out to two
  agents** (`one_event_fans_out_on_rep1_but_collides_on_rep2`).
- Agents must know two reply verbs (`message reply`, `request reply`) and pick by
  inspecting the sender.

### 1.3 What the prototypes settled empirically

Treated as given, not relitigated:

- Delivery must ride the existing inbox → DING/MCP/app-server transports.
- Producer-side supersede is the only viable coalescing point: `flush_pending`
  sees one notice at a time and holds exactly one staged payload, and DING
  deliberately never deletes inbox items
  (`producer_side_supersede_collapses_a_stream_to_its_latest_event`).
- A source-supplied event id is the dedup contract; content hashing cannot
  distinguish replay from a legitimate repeat
  (`an_id_less_source_dedups_by_content_which_also_collapses_real_repeats`).
- Derived-companion lifecycle is generic over `Task::derived` and needed only a
  two-line gate extension to carry a new companion kind.

---

## 2. Target model

### 2.1 Two identity kinds

**Agent** — unchanged. A full Agent Spec: persona, tasks, presence, roster row,
derived DING, doctor obligations, sender-owned Sent ledger.

**Source** — a declared non-agent producer of world-events. It has a bus id, a
state folder, and an inbox (so an agent can reply to it). It has **no** persona,
presence record, roster row, derived DING, supervisor edge, or Sent ledger.

```kdl
// sources/hetz/gh-ci/source.kdl
source "gh-ci" {
  host "hetz"
  command "poll-gh-ci --json"        // optional
  restart { attempts 5 mode "delay" }
  desired-state "running"
}
```

The `command` is what unifies today's two shapes:

| `command` | Meaning | Replaces |
|---|---|---|
| absent | An endpoint only. Some external thing (systemd timer, webhook receiver, an eval harness) emits into it. No task, no supervision. | `principals/<host>/<id>/principal.kdl` |
| present | st2 supervises one long-running exec task `<host>.<name>` whose stdout is events. | the lifecycle prototype's `pipe "name" { … }` |

This is the load-bearing simplification. The lifecycle prototype's two sharpest
frictions both disappear:

- *"A task identity cannot be an idempotent bus sender."* A source's task **is**
  the source, so `ST_AGENT`-style runner identity and sender identity coincide.
  No impersonation, no `↺`, no `x-st2-pipe-task:` body line.
- *"The derived marker carries no payload."* A source's command is an ordinary
  authored task command. No marker-argv smuggling, no `pipe_name_of_task`, no
  invented task-name grammar, no `pipe-launch-missing` /
  `unsupported-pipe-interval` diagnostics, no `pipe`-vs-`exec` name-collision
  check.

**Why top-level rather than nested on an agent.** Fan-out alone does not decide
this: the ingress prototype's fan-out defect is *ledger scoping*, not declaration
placement, so a nested pipe that scopes its ledger by `(stream, event-id,
recipient)` can also feed several agents from one process. Three other things
decide it:

1. **Teardown coupling.** A nested pipe is a derived companion, and the *Derived
   companion lifecycle* invariant stops a companion whenever its agent is held,
   suspended, retired, or parked. Suspending agent A would therefore silently stop
   the CI feed that agents B and C depend on. That is a correctness bug, not an
   aesthetic one, and it appears the moment a source has more than one consumer.
2. **Arbitrary ownership.** A shared GitHub poller has no natural owning agent, so
   nesting picks one by accident and encodes it in the runtime id.
3. **It absorbs `principal`.** A nested pipe cannot be the same concept as
   `principals/<host>/<id>/principal.kdl`, which is inherently top-level, so the
   nested shape leaves the system with two declaration kinds for non-agent
   producers instead of one. The `command`-present/`command`-absent split is what
   collapses them, and it only works at top level.

The cost is real and is stated in §10: a top-level source is the one part of this
design that needs new reconciler work.

**Reserved sources.** `st2.<host>` becomes a *built-in* source identity — the
runner's crash-loop notice (`src/run.rs:2107`) and the eval harness notices
(`src/eval_run.rs:1031`, `:1471`) stop being unattributed strings and become
events from a declared reserved source. The eval external requester
(`ExternalInbox`) is a source whose `command` is absent and whose inbox is the
reply target: exactly the shape it already has, given a name.

### 2.2 Event identity: a three-part grammar

```
from       hetz.gh-ci            who produced it (a source bus id)
stream     github:ci:pr-42       which series of facts this belongs to
event-id   failure               which occurrence within that series
```

`(from, stream, event-id)` is the event's identity. The dedup ledger is scoped
by `(stream, event-id, recipient)` within one source, matching MESSAGE-R07's
`(sender, recipient, key)` scoping — which is what makes fan-out work and what
today's `request-state` gets wrong.

This grammar resolves an ambiguity in issue #137, which uses `--source` for what
is really the *stream* (`systemd:dev3-janitor-daily`) while the producer identity
stayed implicit. Splitting them means `from` is a declared, resolvable identity
the DING glyph can reason about, and `stream` is a free-form producer namespace
st2 never interprets — the same discipline R20 applies to Resource URIs.

### 2.3 The record on disk

An event is an ordinary message file in the recipient's `resources/inbox/`, with
the ordinary `<unix-ms>-<rand6>.md` grammar and the ordinary archive receipt.
Two new frontmatter keys:

```
---
from: hetz.gh-ci
subject: CI failed on PR #42
stream: github:ci:pr-42
event-id: failure
---
build job `test` failed on commit 9ca9a2a
```

There is deliberately **no** `kind: event` discriminant. An inbox item is an
event exactly when its `from` resolves to a declared source; `stream:` and
`event-id:` are its stream coordinates. Producer kind and record kind are the
same fact, and duplicating it would create a way for them to disagree.

`src/message.rs:242 parse_message` ignores unknown frontmatter keys
(`_ => {}`), and `crates/st2-wire` has no `deny_unknown_fields`, so both
directions of version skew are safe by construction — an older reader sees an
ordinary message with an unusual sender, and an older st2 binary reading a
newer file loses only the stream coordinates.

The body is whatever the producer wrote: markdown for a legible event, JSON for
a typed one. Typing moves from a mandatory st2 envelope to a producer choice,
which is what makes `message read`, `message ls --include-body` (#238), and
`message thread` work on events for free.

### 2.4 The durable event store: one file per (stream, recipient)

```text
sources/<host>/<name>/resources/
  inbox/<filename>.md            replies from agents (ordinary messages)
  archive/<filename>.md
  events/<sha256(stream\0to)>.json
  events/<sha256(stream\0to)>.lock
```

Version-1 stream record:

```json
{
  "version": 1,
  "stream": "github:ci:pr-42",
  "from": "hetz.gh-ci",
  "to": "hetz.worker",
  "pending": { "eventId": "failure", "filename": "1787…-6ktatz.md", "rendered": "---\n…" },
  "recent": [
    { "eventId": "running", "filename": "1787…-bn4myf.md" },
    { "eventId": "pending", "filename": "1787…-a1b2c3.md" }
  ]
}
```

`recent` is a **bounded ring**, newest first, capped at K (default 128,
per-source declarable). `recent[0]` is the stream head — the event supersede
retires. `pending` is the single in-flight reservation.

Publish transaction, under the per-(stream, recipient) flock:

1. Read the record. If `pending` names *this* `eventId`, resume it with its
   original filename and bytes. If `recent` already contains this `eventId`,
   return that filename with `deduplicated: true` and stop.
2. Atomically replace the record with `pending = {eventId, filename, rendered}`.
3. `message::materialize_message_once(inbox, filename, rendered)` — the same
   idempotent primitive `request::publish_once` already uses.
4. If `--supersede` and `recent[0]` names a different filename, archive that
   filename in the recipient's box. Repeated archive is idempotent by the
   *Exactly-once-safe native bus* invariant.
5. Atomically replace the record with `pending = null` and `recent` pushed.

Step 3 before step 4 is deliberate: the new head is unread before the old one is
retired, so a stream is never momentarily empty and a wakeup cannot be lost.

Three properties fall out:

- **Crash safety** with one durable file instead of eleven. A crash anywhere
  between 2 and 5 is finished by the next emit for the same `(stream, event-id)`
  with the identical filename and bytes. This is `request-state`'s reserve-then-
  materialize discipline, but the reservation and the ledger are the same object.
- **Constant write work and constant storage per stream.** MESSAGE-R08 requires
  constant history-dependent write work for the sender ledger; the event store
  achieves it *and* constant size, which the sender ledger deliberately does not.
- **No shared producer lock.** The ingress prototype's structural complaint about
  Rep 1 was that every pipe event serialises through the producing agent's single
  `SentLock::exclusive`. The event lock is per (stream, recipient), so two streams
  on one source never contend.

### 2.5 Bounded dedup retention — the differentiation that earns its keep

The ingress prototype's open question 4 is *"what retention rule applies to
`request-state/outgoing` records after the inbox message is archived? They are the
only thing preventing a much later replay from re-publishing, and nothing prunes
them today."* Nothing in st2 answers it, and an unbounded per-event ledger on a
chatty CI stream is an unbounded on-disk cost with no reclaim path.

The event model answers it by declaring a bound rather than a policy: **dedup is
guaranteed for the last K events of a stream, and is not guaranteed beyond that.**
Replaying an event id that has aged out of the ring publishes a new item.

This is exactly the tradeoff st2 already ratified as T01 — "a documented
unsupported case is preferable to a hidden distributed guarantee" — and it is the
concrete semantic difference between an event and a message that a unified model
would have to fake. A message's sender history is *history*: MESSAGE-R03 requires
exact traversal to genesis and refuses to weaken the coverage claim. An event
stream is *current state*: only the head matters, the tail is evidence, and the
system is allowed to forget. Forcing events into the Sent ledger would make a CI
poller's ten-per-minute flap a permanent, ledger-chained, hash-verified part of
some agent's conversational history.

### 2.6 Supersession

`--supersede` is per-emit, not per-stream, because the same stream can carry both
kinds of fact (a CI *status* supersedes; a CI *comment* does not).

A superseded event is archived, not deleted: it stays in `resources/archive/` as
durable evidence and its archive filename is the receipt that stops it being
resurrected. The recipient sees exactly one unread item per superseding stream.

Whether this races DING's staged ownership is the riskiest claim in this design
and is the subject of the spike in §5.

### 2.7 What happens to service-principal requests

They are absorbed. A request becomes:

| Today | Target |
|---|---|
| `st2 request send --idempotency-key K --tag k=v --json` | `st2 event emit <to> --stream S --event-id E --subject … --json` |
| `RequestEnvelope` JSON in the body, `st2-request` tag | the producer's own body; `stream`/`event-id` frontmatter |
| `st2 request read <file> --json` | `st2 message read <file>` (and `--json`) |
| `st2 request reply <file> --json …` | `st2 message reply <file>` |
| `ReplyEnvelope`, `st2-request-reply` tag, `request-state/replies/` | the ordinary reply's `in-reply-to` and `from` |
| `st2 request status --idempotency-key K --json` → `pending \| replied` | `st2 event status --stream S --event-id E --json` → `pending \| replied` |

`event status` runs the same algorithm `request::status` runs today: look up the
published filename in the stream record, then scan the source's inbox and archive
for a message whose `in-reply-to` is that filename. The envelope validation goes
away; the routing does not change at all.

Two consequences worth stating plainly:

- **An agent now has one reply verb.** It replies to a CI event exactly as it
  replies to a peer, and does not have to inspect the sender to choose a command.
  This is the concrete ergonomic payoff of unifying the *reply* path while
  differentiating the *ingress* path.
- **A reply to a source enters that agent's Sent history**, because the agent
  genuinely said something. MESSAGE-R11 is amended accordingly in §4.

### 2.8 How the notice renders

`relationship_marker` gains one branch: a `from` that resolves to a declared
source renders `~`.

```
[DING] ← hetz.reviewer: please look at PR #42            (peer agent)
[DING] ~ hetz.gh-ci: CI failed on PR #42                 (world event)
[DING] ? hetz.mystery: …                                 (cannot tell)
```

`?` currently means three different things at once — "not a declared agent", "the
catalog is unreadable", and "the supervisor chain is broken" — and the ingress
prototype showed a principal makes it mean a fourth, "legitimate machine
producer". Adding `~` gives `?` back a single meaning: *st2 could not resolve
this*. That is a strict gain in the glyph vocabulary's information content even
though the vocabulary grows by one symbol.

The glyph is a fixed st2-chosen literal, not producer-supplied text; the source's
bus id continues through `normalize_field`, so
`poke_text_normalizes_and_bounds_untrusted_fields` and
`malicious_controls_cannot_escape_the_single_paste_frame` are unaffected.

### 2.9 Cursors: deliberately not a thing

The lifecycle prototype notes that "nothing here addresses a cursor" and that a
restarted pipe re-reads its source from scratch and relies on dedup. That is the
right answer and the event model keeps it: a cursor is *the adapter's* state
about *the external world*, not st2's state about the bus. A GitHub poller's
cursor is a GitHub cursor; st2 storing it would mean st2 understanding GitHub,
which #137 lists as a non-goal.

What st2 owns is that a replayed event is not a second event, which the
`(stream, event-id)` ring provides. An adapter that wants a durable cursor writes
one in its own working directory.

---

## 3. Concept count: today vs target

| Dimension | Today | Target | Δ |
|---|---:|---:|---:|
| Producer identity kinds that can write an inbox | **5** (agent, principal, `ExternalInbox`, `st2.<host>`, literal `st2`) | **2** (agent, source) | −3 |
| Declaration nodes for producers | **2** (`principal`, prototype `pipe` + `command`/`argv`/reserved `every`) | **1** (`source`) | −1 |
| Durable producer stores | **3** (`sent/`, `request-state/outgoing/`, `request-state/replies/`) | **2** (`sent/`, `events/`) | −1 |
| Durable file kinds in those stores | **11** (9 in `sent/` + 2 request-state) | **10** (9 + 1) | −1 |
| Inbox record shapes | **3** (message, request envelope, reply envelope) | **2** (message, event) | −1 |
| Serialized wire types | **10** (`MessageRow`, `SentMessageRow`, `SentMessages`, `SentCoverage`, `RequestEnvelope`, `ReplyEnvelope`, `PublicationRecord`, `PublishReceipt`, `IncomingRequest`, `RequestStatus`) | **7** (`MessageRow`+2 fields, `SentMessageRow`, `SentMessages`, `SentCoverage`, `StreamRecord`, `EventReceipt`, `EventStatus`) | −3 |
| CLI commands | **11** (`message` ×7, `request` ×4) | **9** (`message` ×7, `event` ×2) | −2 |
| Reply verbs an agent must choose between | **2** | **1** | −1 |
| DING glyphs | **5** (`↺ ↓ ↑ ← ?`), `?` overloaded 4 ways | **6** (`↺ ↓ ↑ ← ~ ?`), `?` means one thing | +1 |
| INVARIANTS rows on the ingress path | **2** | **3** | +1 |
| Modules | `message.rs`, `request.rs` | `message.rs` (unchanged), `event.rs` | 0 |

Wire types are counted as top-level serialized shapes; `StreamRecord`'s two inline
sub-structs (`StreamEntry`, `StreamPending`) are counted with it, the same way
`PublicationRecord` is counted as one today.

Every axis moves down except two, and both increases are the earned semantics:
the glyph that stops `?` lying, and the invariant row that states the stream
bound. Nothing in the message subsystem changes.

The shape of the win is worth naming: this design **differentiates the record and
the producer identity** and **unifies the transport, the store location, the read
commands, and the reply path**. It is not the pure differentiation pole — that
design is measured and rejected in §7 — and it is not unification either, because
the identity split and the stream semantics are load-bearing. It is the factoring
that puts the split on the axis that carries meaning and takes it off the axis
that does not.

---

## 4. Requirement and invariant delta

### R10 Agent-only identity — amended, as a reduction

Current text: *"st2 models agents. Non-agent identities are unsupported."*

Proposed: *"st2 models agents and event sources. An **agent** is the actor st2
models: it has a persona, tasks, presence, a roster row, a supervisor edge, and
durable sender-owned history. A **source** is a declared non-agent producer of
world-events: it has a bus id, an event store, and an inbox for replies, and it
has none of an agent's lifecycle, presence, or authority. No other identity may
address the bus."*

This is not a new exception. It replaces four undeclared exceptions (`principals/`,
`ExternalInbox`, `st2.<host>`, the literal `st2`) with one declared one, and makes
the vision's *"not a general messaging or identity platform for people, services,
or arbitrary non-agent actors"* enforceable for the first time — today an
unattributed `send_to_inbox` call can write any inbox with any `from` string.

### R15 Bounded event coalescing — extended

R15 today governs *filesystem watcher* streams. The event model puts a second
coalescing point in the system (producer-side supersede), and it must be bounded
in the same spirit. Proposed addition: *"A superseding event stream leaves exactly
one unread item per (stream, recipient), and delivery attempts for that stream are
bounded by DING's existing backoff, not by the producer's emission rate."*

The exposure is real and is what the spike measures: a stream that supersedes
faster than DING drains must not turn each supersede into a PTY probe. The
existing **Bounded DING PTY probe churn** invariant
(`deferred_delivery_backoff_bounds_short_lived_pty_attempts`) is the mechanism
that has to hold.

### MESSAGE-R01…R11 — R11 amended, the rest untouched

MESSAGE-R01 through R10 are unaffected: ordinary sends, the sender ledger, the
commit chain, coverage, and the `st2-wire` shapes do not change. `MessageRow`
gains two optional fields, which MESSAGE-R04's "optional metadata … preserve
absent versus empty values" already accommodates and which the
`a_field_this_reader_does_not_know_is_ignored_not_rejected` test already protects.

MESSAGE-R11 today: *"Service-principal request publication state does not appear
in ordinary Agent Sent history."*

Proposed: *"Event publication state does not appear in ordinary Agent Sent
history; a source has no Sent index. An agent's **reply** to an event is an
ordinary send and does appear, because the agent genuinely authored it."*

The inbound half — the half MESSAGE-R11 exists for, keeping machine ingress out of
conversational history — is preserved exactly. The outbound half is deliberately
relaxed, and the relaxation is an honesty improvement: today an agent's answer to
a service is durably invisible in its own Sent history.

MESSAGE-A01 ("declared senders … the eval-owned external requester remains an
explicit compatibility capability") loses its exception clause: the eval requester
becomes an ordinary source.

### INVARIANTS: *Idempotent service requests* → *Idempotent event ingress*

Replacement row, written to the same standard:

> **Idempotent event ingress** — A declared non-agent source publishes one exact
> event per `(stream, event-id, recipient)` into a canonical Agent Spec inbox.
> Concurrent or crash-replayed publication reuses the reserved filename and bytes;
> conflicting reuse of one event identity fails. One event fans out to several
> recipients as several independent exactly-once operations. A reply routes to the
> source's canonical inbox as an ordinary message, without an Agent Spec identity
> or an orphan mailbox.
>
> Proof: `tests/event_e2e.rs::stable_event_identity_publishes_exactly_one_canonical_message`;
> `tests/event_e2e.rs::concurrent_replays_publish_exactly_one_event`;
> `tests/event_e2e.rs::one_event_fans_out_to_several_recipients_without_collision`;
> `tests/event_e2e.rs::a_fan_out_with_one_unknown_recipient_publishes_nothing`;
> `tests/event_e2e.rs::conflicting_reuse_of_one_event_identity_fails`;
> `tests/event_e2e.rs::a_reply_routes_to_the_source_and_status_is_a_tagged_json_union`;
> `tests/event_e2e.rs::event_api_rejects_agent_impersonation_and_unknown_sources`

### INVARIANTS: new row *Bounded event streams*

> **Bounded event streams** — A superseding stream leaves exactly one unread item
> in the recipient's inbox and its retired predecessors as durable archived
> evidence; a superseded item is never resurrected and never re-notified. The
> producer's per-stream dedup record is constant-size: exactly-once is guaranteed
> for the last K event ids of a stream and explicitly not beyond.
>
> Proof: `tests/event_e2e.rs::supersede_collapses_a_stream_to_one_unread_head_with_archived_evidence`;
> `tests/event_e2e.rs::supersede_is_idempotent_and_crash_replay_safe`;
> `tests/event_e2e.rs::the_stream_record_is_constant_size_and_forgets_beyond_its_bound`

### INVARIANTS: *Fail-closed observed native DING* and *Exactly-once-safe native bus* — unchanged

Both rows keep their text and all ~18 + 3 named proofs. Events use the same
filename grammar, the same `list_inbox` shadowing, and the same archive receipts,
so DING cannot tell an event from a message except through `relationship_marker`.
The spike in §5 exists to prove that supersede does not violate either row rather
than to amend them.

### R23, R31, *Derived companion lifecycle*, *Runner-owned task identity*

A source's task is an ordinary non-derived exec task, so it inherits restart
policy, flapping, parking, crash-loop surfacing, and `st2 tasks --json` with no
change to those contracts — the lifecycle prototype proved this for the derived
shape, and the non-derived shape is strictly simpler. *Derived companion
lifecycle* keeps its exact text and eleven proofs, because a source is not a
companion.

### Issue #137's acceptance criteria

| Criterion | Status |
|---|---|
| A shell command emits an event to a catalog identity using stdin for the payload | Library-complete (`event::emit`); the CLI verb is unbuilt and its name is open in #137 |
| A native recurring shell-driven source emits on a declared cadence, so a bootstrap timer can be removed | **Not addressed.** A `command`-bearing source removes the *supervision* half; the *cadence* half is deferred to `schedule` (open question 2) |
| The producer supplies a stable source and event ID | `(from, stream, event-id)`, with `from` a declared identity rather than a free string |
| Replaying the same identity is idempotent, including concurrent retries | `concurrent_replays_publish_exactly_one_event` (12 threads) |
| JSON output includes recipient, receipt id, and created vs deduplicated | `EventReceipt { to, filename, deduplicated, superseded }` |
| A new event follows inbox → DING; a duplicate does not DING again | `stable_event_identity_publishes_exactly_one_canonical_message` + the archive receipt; DING's `new_arrivals` never re-emits a filename it has seen |
| The on-disk representation stays compatible with normal list/read/archive/thread | By construction — it *is* a normal message file |
| Tests cover retry, concurrent duplicate emission, and a producer crash/retry boundary | `supersede_is_idempotent_and_crash_replay_safe` covers the crash boundary |

#137's own open questions get concrete answers: extend frontmatter rather than
keep a separate receipt index (the receipt index would be a second store to keep
consistent with an immutable message); deduplication lives at the **producer**,
scoped per recipient, not per recipient-catalog or at a shared bus scope; the
retention rule is the bounded ring in §2.5; and `event emit` beats `pipe` as the
verb because `pipe` names the *source*, not the act.

### DQ1

The event model supplies three of DQ1's four unspecified items: the KDL shape
(`source`), the event inbox (the ordinary inbox, differentiated by producer
identity and stream frontmatter), and the deduplication boundary
(`(from, stream, event-id)` with a declared bound). Execution receipts are the
`EventReceipt`/`EventStatus` pair. The *cadence* half — #137's "a native recurring
shell-driven source can emit on a declared cadence … allowing a bootstrap system
timer to be removed" — is deliberately left to the reserved `schedule` node; see
the open questions.

---

## 5. Spike: the riskiest seam

See §6 for what was built and measured.

The seam chosen is **producer-side supersede against DING's staged ownership**,
because it is the only place where a genuinely new semantic (supersession)
collides with a sacred invariant, and because if it fails the differentiation
argument collapses — supersession is the main semantic a unified model would
have to fake.

The three claims under test:

1. When a producer supersedes (archives) event *N* **while DING has already
   staged N's notice into a composer**, event *N+1* still delivers and *N* is
   never re-pasted.
2. The archived staged head does not stall FIFO indefinitely.
3. A stream that supersedes faster than DING drains does not turn each supersede
   into a PTY probe (the R15 / *Bounded DING PTY probe churn* exposure).

---

## 6. Spike results

### What was built

Strictly additive: `src/request.rs` is untouched and every `tests/request_cli.rs`
proof — including the four named by the *Idempotent service requests* invariant —
stays green. The absorption in §2.7 is a design proposal with a written
replacement invariant row, not a deletion performed here.

`INVARIANTS.md` is deliberately **unedited**. The replacement and new rows in §4
are proposed text with their expected proof names, which is the right state for a
two-pole exploration: amending a sacred row is a decision for whichever design is
adopted, and `tests/invariants.rs::qualified_proof_references_resolve` would fail
against a row whose proofs live in an unadopted branch.

| Area | File | What |
|---|---|---|
| Source declaration | `src/event.rs` | `sources/<host>/<name>/source.kdl`, `discover_sources`, `resolve_source`, agent-collision refusal |
| Event record | `src/event.rs::render_event` | `stream:` / `event-id:` frontmatter on an ordinary message |
| Durable store | `src/event.rs::StreamRecord` | one constant-size file per `(stream, recipient)`: bounded `recent` ring + single `pending` reservation + per-stream flock |
| Publish | `src/event.rs::emit` | reserve → materialize → supersede → advance, fan-out over N recipients |
| Reply channel | `src/event.rs::status`, `src/message.rs::DeliveryEndpoint::Source` | `pending \| replied` from ordinary `in-reply-to`; an agent replies with `message reply` through its own Sent ledger |
| Notice | `src/ding/mod.rs::SOURCE_MARKER`, `RelationshipResolver::is_source` | `~` for a declared source |
| Race proofs | `src/ding/mod.rs` tests | supersede against staged payload ownership |
| Ingress proofs | `tests/event_e2e.rs` | 12 tests |

### Claim 1 — supersede of a *staged* head never re-pastes, and the successor delivers

`src/ding/mod.rs::a_producer_supersede_of_a_staged_event_never_repastes_and_the_successor_delivers`

Real catalog, real source, real emits. DING stages the `failure` notice and owns
it; the producer then emits `success` with supersede, which materializes `success`
and archives `failure` **under DING's feet**. The measured result:

- `failure` appears in the fresh-paste log exactly once, ever;
- the only staged retry is `failure`, and it is inspection-only — the archived,
  positively-`NotRetained` head releases FIFO without a second paste;
- `success` is then pasted once and delivered; the queue drains empty.

**No change to `flush_pending` or `prune_archived_pending` was needed.** The rules
that already exist — "staged ownership survives archive and never repastes" and "a
maintained adapter's positive `NotRetained` observation releases only an already
archived staged head" — turn out to be exactly the rules a producer-side supersede
needs. That is the single most important result in this spike: supersession is
*already* safe against the DING invariant that governs it, because archiving under
DING is a case the invariant was designed for.

### Claim 2 — a superseded head that is still retained blocks FIFO instead of leaking a paste

`src/ding/mod.rs::a_superseded_but_still_retained_staged_event_keeps_ownership_without_repasting`

The pessimistic branch: the adapter still sees the superseded notice in the
composer. Ownership is retained, the successor is **not** pasted on top of a live
payload, and later FIFO work stays blocked — which is the correct fail-closed
behaviour and the one the invariant demands. Supersede therefore cannot be used to
force a second paste into an occupied composer.

### Claim 3 — a fast-superseding stream does not multiply PTY delivery attempts

`tests/event_e2e.rs::a_fast_superseding_stream_does_not_multiply_pty_delivery_attempts`

An end-to-end `run_ding` loop over a real inbox, with a `Poker` whose every
attempt defers (the worst case: the notice is never retired). 24 supersedes fired
into the watched inbox — 48 filesystem mutations, each one a watcher wake:

```
CHURN: 24 supersedes in 145.739138ms -> 1 fresh poke(s), 0 staged retry/retries, 1 unread
```

**One** PTY delivery attempt for 24 events, and the agent is left with exactly one
unread item rather than 24. The existing `DELIVERY_RETRY_BACKOFF` decouples
delivery attempts from producer rate, so *Bounded DING PTY probe churn* holds
against the new coalescing point without a new bound. R15's proposed extension in
§4 is therefore a statement of what already happens, not a new mechanism.

### Ingress, fan-out, retention, reply — `tests/event_e2e.rs` (12 tests, all green)

| Test | What it proved |
|---|---|
| `stable_event_identity_publishes_exactly_one_canonical_message` | One `(stream, event-id, recipient)` → one item; three replays return the original filename with `deduplicated: true`; the item is an ordinary readable message with the producer's own body and subject — no envelope, no `request <key>` subject |
| `concurrent_replays_publish_exactly_one_event` | 12 racing threads observe one canonical filename; one inbox item |
| `one_event_fans_out_to_several_recipients_without_collision` | Three recipients, three exactly-once operations, one copy each; replaying the fan-out deduplicates all three. This is the defect the ingress prototype measured in the principal path, fixed by scoping the ledger the way MESSAGE-R07 already does |
| `conflicting_reuse_of_one_event_identity_fails` | Different bytes under one identity is a hard failure, never a second item |
| `a_reply_routes_to_the_source_and_status_is_a_tagged_json_union` | An agent answers with **`message reply`**; the reply lands in the source's inbox; `status` flips `pending` → `replied` by `in-reply-to` alone. The reply is in the agent's Sent history and the source has no `resources/sent` — MESSAGE-R11's inbound half preserved, outbound half deliberately relaxed |
| `event_api_rejects_agent_impersonation_and_unknown_sources` | An undeclared producer is refused; a source colliding with an Agent Spec identity is refused; an agent still wins as a recipient, so a source cannot intercept peer traffic |
| `supersede_collapses_a_stream_to_one_unread_head_with_archived_evidence` | Four CI transitions → exactly one unread (`success`) and three durable archived predecessors; replaying a superseded event returns its filename and never resurrects it |
| `a_fan_out_with_one_unknown_recipient_publishes_nothing` | R19 admission: one bad address in a fan-out refuses before any write and reserves no producer state, so a caller can never lose the receipt for a partially published fan-out |
| `supersede_is_idempotent_and_crash_replay_safe` | Both crash windows. Reserve→materialize: the replay publishes under the *reserved* filename. Materialize→advance (the likelier one): the replay **adopts the already-materialized file** rather than minting a second one, then finishes the supersede and the ring advance. Repeating either emit supersedes nothing new; the stream is never momentarily empty |
| `the_stream_record_is_constant_size_and_forgets_beyond_its_bound` | 129 events → **one** record file with `recent.len() == 128`; the aged-out id is gone, and replaying it is asserted to be a *new* publication rather than silently claimed as exactly-once |
| `a_source_event_renders_its_own_marker_and_question_mark_keeps_one_meaning` | `[DING] ~ hetz.gh-ci: CI failed on PR #42 [id:…]`; an unresolvable sender is still `?`; an agent is never mistaken for a source |
| `a_fast_superseding_stream_does_not_multiply_pty_delivery_attempts` | see claim 3 |

Plus `src/event.rs` unit proofs that one declaration node covers both a supervised
and an externally driven source
(`one_declaration_covers_a_supervised_and_an_externally_driven_source`), that
content must match its canonical path, and that the ring is bounded and
newest-first.

### What the spike disproved

Nothing in the design, but two things I expected to have to build turned out to be
unnecessary, and both make the design *smaller* than drafted:

- **No DING change was needed for supersession.** The draft anticipated at least a
  `prune_archived_pending` adjustment. There is none. The only DING edit in the
  whole spike is the `~` glyph branch.
- **No new bound was needed for R15.** `DELIVERY_RETRY_BACKOFF` already decouples
  probes from producer rate by two orders of magnitude in the measured case.

### Regression status

`nix develop -c cargo test --workspace --no-fail-fast`, with the spike applied:
**40 suites green, 7 suites failing, 9 failing tests.** The same seven suites fail
byte-identically at the unmodified base commit `8ff140e` — same test names, same
pass counts — so every one is pre-existing:

| Suite | Failing test(s) |
|---|---|
| `catalog_diff` | `classification_only_and_nested_agent_filename_changes_are_exact` |
| `eval_run_e2e` | `canonical_agents_freeze_the_admitted_route_across_post_boot_catalog_mutation` |
| `eval_up` | `st2_up_once_atomically_respawns_a_hard_killed_agent`, `st2_up_boots_a_specs_team`, `st2_down_tears_down_a_spec_fleet`, `st2_up_spec_supervises_and_respawns_a_killed_agent` |
| `materialize` | `up_materialize_only_writes_the_overlay_without_needing_pty` |
| `native_only` | `clean_path_supports_help_validate_env_and_doctor`, `tracked_product_surface_contains_only_native_names` (names only `docs/vrs/spec.md`, untouched here) |
| `targeted_reconcile` | `targeted_once_real_pty_preserves_sibling_generation_across_selected_lifecycle` |
| `task_inventory_cli` | `completed_catalog_aba_during_runtime_observation_is_incomplete` |

This set matches the lifecycle prototype's independently recorded pre-existing
failures exactly.

**Every INVARIANTS-named suite is green**, including the ones this spike touches
directly: `--lib` (339, covering all 18 *Fail-closed observed native DING* proofs,
the *Exactly-once-safe native bus* proofs, and the two new supersede-race tests),
`--test request_cli` (all four *Idempotent service requests* proofs),
`--test message`, `--test message_cli`, `--test invariants`, `--test run`,
`--test nomad_survival`, `--test doctor`, `--test reconcile`, `--test pty`,
`--test exec_backend`, `--test status_agents`, `--test transport_isolation`,
`--test predecessor_ding_migration`, and the new `--test event_e2e` (12).

### What the spike did *not* cover

- **Supervising a standalone source's task.** §10 cost 1 is unmeasured. The
  lifecycle prototype proved the *derived companion* shape works with a two-line
  gate change; a non-derived, agent-less top-level task is a different and larger
  change to `src/reconcile.rs`'s target loop, and this spike deliberately spent its
  budget on the seam that could invalidate the design rather than the one that is
  mechanical.
- **CLI surface.** `st2 event emit` / `st2 event status` are library functions
  here, not `main.rs` subcommands. #137 leaves the command name open.
- **`ExternalInbox` migration** to a source, and the reserved `st2.<host>` source.
  Both are proposed in §2.1 and neither is implemented.

---

## 7. Rejected: a separate event store

The pure differentiation design — events in `resources/events/` on the recipient
with their own notice path — was evaluated and rejected. Its measured cost:

- **Five inbox readers** would each need a second source and a merge rule:
  `src/agents.rs:147` (roster counts), `src/claude_mcp.rs:96`, `src/pi_channel.rs:109`,
  `src/codex_app_server.rs` (native delivery), `src/main.rs:2341` (`message ls`).
  #238's bounded-body delivery envelope would need to span both.
- **DING's `PendingNotice` becomes a four-variant enum** and `flush_pending`'s
  single-staged-payload FIFO becomes a two-queue merge with an ordering rule. That
  lands inside a machine with ~18 named invariant proofs; `prune_archived_pending`
  and `new_arrivals` would both need a second archive semantics.
- **The *Exactly-once-safe native bus* invariant's archive-receipt guarantee would
  have to be reimplemented** for the second store, or events would lose the very
  property that makes supersede safe.

What it buys: retention policy independence, and events not appearing in
`message ls`. Both are obtainable at near-zero cost from a `stream:` frontmatter
key plus a filter, and the retention story in §2.5 lives at the *producer*, where
it belongs, not at the recipient.

Coalescing is not on the list of benefits: the ingress prototype already proved
coalescing cannot live in DING regardless of where events are stored, because
`flush_pending` sees one notice at a time.

Verdict: a separate store adds one durable store, one notice path, one enum
variant set, and five reader merges to buy two things a frontmatter key already
buys. It loses by every axis of the objective function.

---

## 8. The strongest argument against this design

Steelmanned unification: *after the factoring in §2, a message and an event
differ only in two optional frontmatter keys, one glyph, and a producer-side
ledger shape. All three differences are producer-side or presentation-side. The
record is the same record, in the same directory, with the same filename grammar,
read by the same commands, delivered by the same transport. Calling that a
"distinct concept" is naming, not architecture. The honest version of this design
is "one inbox item, two producer identity kinds", and `st2 event emit` should just
be `st2 message send --stream --event-id --supersede`, which is exactly what issue
#137's shipped-direction PR #138 converged on and what its comment thread
proposed: "adds optional `--source` and `--event-id` flags and frontmatter to
ordinary `st2 message send`. Without those flags, message behavior and output do
not change."*

This is a strong argument and it is partly correct. Three things answer it:

1. **The identity split is load-bearing independently of the record split.** A CI
   poller must not acquire a roster row, a presence record, a derived DING
   sidecar, or doctor health obligations. That is true whether or not events and
   messages share a record shape, and it is the finding both prototypes
   independently reached. The unified position must still declare sources.
2. **The publication transactions genuinely differ, and cannot merge.** An
   ordinary send takes the agent's single `SentLock::exclusive`, writes a pending
   record, a row, an immutable commit node, and an atomically replaced head, and
   MESSAGE-R03 then requires exact traversal to genesis. An event takes a
   per-stream lock and replaces one constant-size record. Routing an event through
   `send_to_resolved_inbox` means either giving every CI flap a ledger-chained,
   hash-verified permanent place in an agent's conversational history, or adding a
   bypass flag to the send path — and a bypass flag is a second mechanism wearing
   the first one's name.
3. **Supersede and bounded retention have no meaning for a message.** "Archive the
   thing I said last time before saying this" is incoherent for speech and correct
   for a status fact. "Exactly-once for the last 128, then not" is unacceptable for
   an agent's own history and right for a CI stream.

Where the unification argument *wins*, and this design concedes it: the reply
path, the transport, the storage location, the read commands, and the DING
machinery. Those are unified here, and the concessions are why this design deletes
more than it adds.

---

## 9. Migration and compatibility

Per the guidance, fresh namespaces over in-place migration.

| Thing | Verdict |
|---|---|
| `principals/<host>/<id>/principal.kdl` | Fresh namespace `sources/<host>/<name>/source.kdl`. Existing catalogs: `principals/` continues to be discovered as sources with no `command` for one release, with a `validate` warning naming the new path. No state moves; a principal's inbox/archive are already at `resources/`. |
| `resources/request-state/outgoing/` | Left in place, no longer written. A one-shot `st2 event import-request-state` is *not* proposed: dedup records only prevent re-publication of events that have already been delivered, so the worst case of ignoring them is one re-delivery of an event whose last emission predates the upgrade. |
| `resources/request-state/replies/` | Left in place, no longer written or read. |
| `st2 request …` | Kept as a deprecated alias for one release: `request send` → `event emit` with `--stream <key> --event-id <key>` derived from the idempotency key, `request reply` → `message reply`, `request status` → `event status`. `request read` is dropped; `message read` supersedes it. |
| In-flight typed requests across the upgrade | An unreplied request published by the old code is an ordinary inbox message with a JSON body; `message read` shows it, `message reply` answers it, and the old `request status` alias still finds the reply because both scan `in-reply-to`. No fencing needed. |
| `MessageRow` consumers | Additive optional fields only; `crates/st2-wire` has no `deny_unknown_fields` and `a_field_this_reader_does_not_know_is_ignored_not_rejected` is the standing proof. |
| `RequestEnvelope` / `ReplyEnvelope` / `PublicationRecord` | All three carry `deny_unknown_fields`. They are **not** extended — they are retired with the code that reads them. |
| The `~` glyph | Every agent sees this string. It is additive: no existing glyph changes meaning, and `?` becomes strictly more precise. |

---

## 10. Costs and risks, honestly

1. **Sources are a new top-level declaration kind in the reconciler.** A source
   with a `command` compiles to a `TaskTarget` that is not owned by an `AgentSpec`.
   Everything downstream of `TaskTarget` (flapping, park, `exec_backend`,
   `task_inventory`) is already generic, but `src/reconcile.rs`'s target loop
   (`:860`), materialization, doctor, and `st2 tasks --json`'s agent join all
   assume an `AgentSpec` owner. This is the largest implementation cost in the
   design and it is *not* what the spike measured. Estimated 150–250 lines across
   `reconcile.rs`, `run.rs`, `doctor`, and `task_inventory.rs`, plus discovery.
   The lifecycle prototype's nested `pipe` shape avoids it entirely at the cost of
   losing fan-out and re-introducing marker-argv payload smuggling.
2. **`?` regressions during rollout.** Until a catalog declares its sources, a
   pipe adapter's events render `?` — the same as today, but the design's headline
   benefit is invisible until the catalog is authored.
3. **The bounded ring is a real semantic loss** relative to today's unbounded
   `request-state`. A source that replays an event id from more than K events ago
   publishes a duplicate. K must be chosen so that no realistic replay window
   exceeds it; 128 is a guess, not a measurement.
4. **Two writers to one recipient inbox now take different locks.** An agent's
   send holds `resources/sent/.lock`; a source's emit holds its own per-stream
   lock. They do not exclude each other. This is already true today
   (`request::publish_once` takes no sender lock at all) and is safe because
   `materialize_message_once` is atomic on a fresh random filename, but it means
   inbox writes are not globally serialised and never were.
5. **`event status` is O(inbox + archive) on the source's boxes**, exactly like
   `request::status` today. A source that receives many replies will want the
   MESSAGE-R10 treatment eventually.
6. **Source discovery must fail closed, and in the spike it does not.** The spike's
   `lookup_source` swallows a discovery error (`.ok()?`) and
   `RelationshipResolver::read` uses `.unwrap_or_default()`, so a malformed
   `source.kdl` makes that source silently *invisible*: a reply to it reports "no
   agent … found", and its events render `?` instead of `~`. That is the opposite
   of how the agent side behaves — `RelationshipResolver::valid` goes false on any
   discovery error and forces `?` everywhere on purpose — and it is against R23 and
   *Tracked workspaces fail closed*. The shipped version must surface a source
   declaration error as a `validate` diagnostic and as an explicit unresolved
   state, never as absence. Noted here rather than fixed, because it is a
   correctness rule for the real implementation, not a question the spike answers.
7. **Same-millisecond ordering is still unrecoverable.** The ingress prototype
   proved two events sharing a millisecond drain in random-suffix order. A
   high-rate webhook fan-in needs a stronger ordering token than the frozen
   `<unix-ms>-<rand6>` grammar, and this design does not supply one.

---

## 11. Open questions only a human can settle

1. **Do sources declare their recipients, or does each emit choose?** This design
   says each emit chooses, which keeps the declaration minimal and lets a source
   route by content. The cost is that an agent's declaration does not say which
   sources can wake it. The alternative — a `subscribe "gh-ci"` node on the agent —
   adds a declaration node and a second place recipients are written.
2. **Cadence.** #137 explicitly wants a declared cadence so a systemd timer can be
   removed. This design leaves `every` to the reserved `schedule` node rather than
   putting a sleep loop inside a source, following the lifecycle prototype's
   reasoning that an interval-respawn loop makes the source a second lifecycle
   owner. Whether `schedule` targets a source, or a source grows `every` handled by
   the supervisor's restart policy, is unsettled.
3. **Is `~` the right glyph?** It is one character every agent sees on every
   machine event, forever.
4. **Is the reserved `st2.<host>` source worth it**, or should the runner's
   crash-loop notice stay an unattributed `send_to_inbox`? Making it a source is
   what takes the identity count from 3 to 2, but it puts the supervisor's own
   escalation path behind a declaration.
5. **K, the dedup ring bound.** 128 is a guess. The right value depends on how far
   back a real adapter replays after a restart.
6. **Does `st2 event emit` also want #277's `steer` / `queue` field?** Three
   separate proposals now want a field on the send envelope — #137's provenance
   pair, #49's thread/topic identity, and #277's delivery mode. `stream` is
   arguably #49's "topic" for machine facts; whether the two should be one concept
   is a real question this design does not answer.
7. **What happens to events while an agent is suspended?** A source is
   independently supervised here, so unlike the nested-pipe shape it keeps running
   and keeps filling a suspended agent's inbox. That is right for an audit trail
   and wrong for a live feed; today's nested prototype makes the opposite choice.

## Method

Design exploration from the differentiation pole with an executable spike of
the riskiest seam: §1–§4 derive the target event model (stream identity,
producer-side supersession, bounded per-stream state, request absorption) from
the code and VRS on `main` (8ff140e); §5–§6 document the spike — supersession
racing DING staged-payload ownership, ingress idempotency under 12-way
concurrency, fan-out, retention bounds, reply routing — run as
`tests/event_e2e.rs` (12 tests) plus DING-side proofs in the exploration
worktree.

## Result

See §3 for the concept-count table and §6 for spike outcomes: supersession
needs zero DING changes (staged head pasted at most once ever; 24 supersedes
in 145.7 ms → 1 fresh poke, 0 staged retries, 1 unread), R15 holds, the
stream record stays constant-size, and `?` regains a single meaning via the
new event marker. Full-workspace failure set identical to baseline.

## Conclusion

Events earn a distinct record kind on the shared transport: the semantics CI
actually needs — supersession, bounded retention, mandatory event identity —
are incoherent inside the append-only sender ledger, while the transport,
storage location, read commands, and reply path unify. Decision 0004 adopted
this model; decision 0005-streams-are-agent-nested-and-stream-named moved the declaration onto the agent and renamed
the vocabulary stream-centric (this document's per-emit "stream" axis became
`key`, and its top-level `source` declaration became the reserved future
generalization).

## VRS Impact

Realized as the `04-stream` subsystem: STREAM-R03..R07 carry this document's
ingress, retention, and supersession claims; spec DQ-S1..DQ-S7 carry its open
questions; `.decisions/0004` records the record-kind choice with this
exploration as primary evidence.
