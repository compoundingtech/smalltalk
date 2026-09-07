# Pipes ingress and delivery prototype

Date: 2026-08-20

## Question

A "pipe" lets an agent subscribe to external events — GitHub CI updates for a
PR, timers, monitoring alerts — and be woken by each one through st2's existing
durable inbox and DING delivery. Two representations can carry that event into
`resources/inbox/`:

- **Rep 1 "message path"** — the event is an ordinary bus message with an
  idempotency key. This is the shape of draft PR
  [#138](https://github.com/compoundingtech/st2/pull/138) against issue
  [#137](https://github.com/compoundingtech/st2/issues/137).
- **Rep 2 "principal path"** — the event is a typed service-principal request
  published by `src/request.rs` from a declared
  `principals/<host>/<identity>/principal.kdl`.

Which one should own external-event ingress, and what does each actually do at
the inbox, the DING notice, and the provenance boundary?

## Method

Both representations were driven end to end on current HEAD through the real
CLI and the real library, in temporary catalogs, by
`tests/pipe_ingress.rs` (16 tests). Rep 1 needed two small additions to be
comparable at all (below). Rep 2 needed no code change. A no-network fake
GitHub CI poller, `examples/pipes/fake-gh-ci-poller.sh`, drives the
`pending → running → failure → success` transitions from a local state file and
is exercised by `fake_github_ci_poller_walks_pending_to_success_with_idempotent_polls`.

### What PR #138 contributes

Nothing that survives. The fetched diff (`gh pr diff 138`) adds only
`--idempotency-key`, not `--source`/`--event-id`, and implements deduplication
by scanning the recipient inbox and then the archive under an inbox flock —
"if the archived message is deleted, st2 forgets the key". HEAD already exposes
`--idempotency-key` on `st2 message send` (`src/main.rs:701`) backed by the
strictly stronger sender-owned key ledger (`src/message.rs::publish_key`,
`keyed_record`), which is scoped by `(canonical sender, canonical recipient,
key)` per MESSAGE-R07 and survives recipient deletion. #138 is superseded and
should be closed rather than rebased. Its `--source`/`--event-id` intent, which
lives in issue #137 and not in the PR, is what this spike reimplemented.

### Spike code

| Change | Where |
| --- | --- |
| `Message.source` / `Message.event_id` frontmatter fields | `src/message.rs:144` |
| `Provenance` (idempotency key + source + event id) and `SendOutcome` (`to`, `filename`, `deduplicated`) | `src/message.rs:156`, `src/message.rs:174` |
| Frontmatter render/parse for `source:` / `event-id:` | `src/message.rs:279`, `src/message.rs:345` |
| `send_to_resolved_inbox` returns `SendOutcome`; `keyed_record` and recovered-intent hits report `deduplicated: true` | `src/message.rs:1431`, `src/message.rs:1553`, `src/message.rs:1564` |
| `st2 message send --source --event-id --json`, key derived as `<source>#<event-id>` | `src/main.rs:703`, `src/main.rs:2198` |
| `SendReceiptJson`; `source` / `eventId` in `message ls --json`, `message read --json`, `message read` human output | `src/main.rs:2506`, `src/main.rs:2647`, `src/main.rs:2696`, `src/main.rs:2423` |
| Evidence suite | `tests/pipe_ingress.rs` |
| Fake CI poller demo | `examples/pipes/fake-gh-ci-poller.sh` |

`src/ding/mod.rs` delivery logic is untouched; only its test fixture gained the
two new `Message` fields.

## Result

### The two representations on disk

One CI failure event, emitted both ways into the same inbox:

```
--- 1787206735473-6ktatz.md          (Rep 1)
---
from: h.pipe-agent
subject: CI failure on PR #42
source: github:ci:pr-42
event-id: failure
idempotency-key: github:ci:pr-42#failure
---
build job failed

--- 1787206735485-bn4myf.md          (Rep 2)
---
from: h.pipe-gh-ci
subject: request github:ci:pr-42#failure
tags: st2-request
---
{"version":1,"idempotencyKey":"github:ci:pr-42#failure","from":"h.pipe-gh-ci",
 "to":"h.worker","replyTo":"h.pipe-gh-ci","tags":{"event-id":"failure",
 "source":"github:ci:pr-42"},"body":{"pr":42,"state":"failure"}}
```

### 0. Sender identity — the constraint that decides the rest

`send_to_resolved_inbox` resolves `from` against Agent Specs and refuses
anything else. The message path therefore cannot carry a non-agent producer
identity in a real catalog:

```
$ st2 message send h.worker --as h.pipe-gh-ci --source github:ci --event-id run-1 ...
Error: no agent 'h.pipe-gh-ci' found in catalog /tmp/…
```

Proof: `rep1_refuses_a_non_agent_sender_but_rep2_accepts_a_declared_principal`.
The same principal is accepted by Rep 2 and produces exactly one inbox item.
Making Rep 1 work requires declaring the producer as a full Agent Spec, which
buys a roster row, a presence record, a derived DING sidecar under "Derived
companion lifecycle", and doctor health checks that a CI poller never wanted —
directly against root R10's agent-only identity intent and MESSAGE-A01.

The residual authoring state after one event makes the asymmetry concrete:

```
h/pipe-agent/agent.kdl
h/pipe-agent/resources/sent/.lock
h/pipe-agent/resources/sent/commits/9ca9a2ad….json
h/pipe-agent/resources/sent/index.json
h/pipe-agent/resources/sent/keys/dd99760d….json
h/pipe-agent/resources/sent/messages/1787206735473-6ktatz.md.json

principals/h/pipe-gh-ci/principal.kdl
principals/h/pipe-gh-ci/resources/request-state/outgoing/07897efd….json
```

### 1. One event → exactly one durable inbox item, including concurrent replay

Both representations pass. One sequential emission plus 12 threads replaying the
same `(source, event-id)`: every invocation returns the same filename, every
replay reports `deduplicated: true`, and the inbox holds exactly one item.

- `rep1_concurrent_and_sequential_replays_publish_exactly_one_inbox_item`
- `rep2_concurrent_and_sequential_replays_publish_exactly_one_inbox_item`

Reusing one event identity with different content fails on both rather than
publishing a second item —
`both_representations_refuse_conflicting_reuse_of_one_event_identity`. Rep 2's
message is the sharper one: `idempotency key reused with different request`.

The mechanisms differ. Rep 1 reserves nothing at the recipient; it consults the
sender's `sent/keys/<sha256>.json` under an exclusive flock (MESSAGE-R08). Rep 2
atomically hard-links one `request-state/outgoing/<sha256>.json` record that
pins the message filename in advance, then finishes an interrupted publication
with `materialize_message_once`. Rep 2's record is what makes a producer *crash*
between reservation and publication safe; Rep 1 gets the same property from its
pending-intent recovery.

### 1b. Fan-out — one event, several agents

The obvious pipe shape is one CI event waking more than one agent, and it is the
one axis where the principal path is worse out of the box
(`one_event_fans_out_on_rep1_but_collides_on_rep2`):

- **Rep 1 fans out correctly.** `key_path(root, to, key)` includes the canonical
  recipient, so the same `(source, event-id)` addressed to `h.worker-a` and
  `h.worker-b` is two operations and two messages, each still exactly-once.
- **Rep 2 refuses the second recipient.** `publish_once` hashes only the state
  key into the principal's `request-state/outgoing`, then rejects the reused
  record because `record.to != to`:

  ```
  Error: idempotency key reused with different request
  ```

The producer workaround is to fold the recipient into the key
(`run-5@h.worker-b`), which the test confirms works and which is exactly what
Rep 1's MESSAGE-R07 scoping does for free. The proper fix is a one-line change
to Rep 2's state key — the same `(caller, recipient, key)` scoping the message
path already uses. This is a real defect in the recommended path, not a reason
to prefer the other one.

### 2. A duplicated event after archive

Neither re-notifies, but they get there differently, and the difference is
visible on raw disk.

**Rep 1 never re-creates the file.** `keyed_record` returns before
`deliver_record`, so a replay of an archived event touches only the sender
ledger. Asserted on raw disk before any logical read:
`rep1_replay_after_archive_never_recreates_the_inbox_file`.

**Rep 2 does re-create it.** `materialize_message_once` consults only the inbox,
so a post-archive replay re-links the canonical filename into
`resources/inbox/`. The "Exactly-once-safe native bus" archive receipt is what
makes that harmless: `list_inbox` shadows the restored replica, deletes it, and
`ding::new_arrivals` returns empty. Both halves asserted in
`rep2_replay_after_archive_recreates_the_file_but_the_archive_receipt_suppresses_it`.

Rep 2 therefore depends on an existing invariant for correctness where Rep 1 is
correct by construction. That is a real difference, though the invariant is
load-bearing anyway and proven by `archive_receipt_suppresses_and_idempotently_cleans_a_restored_inbox_copy`.

### 3. What DING actually renders

`ding::poke_text` rendered over messages that were really emitted, in a catalog
where `worker` and the Rep 1 producer are siblings under one supervisor
(`ding_notice_rendering_differs_across_representations`):

```
REP1 DING: [DING] ← h.pipe-agent: CI failure on PR #42 [id:vvje5g]
REP2 DING: [DING] ? h.pipe-gh-ci: request github:ci#run-11 [id:ykndpr]
RAW  DING: [DING] ? pipe:gh-ci: CI failure on PR #42 [id:y5za7n]
```

Three findings:

- **Rep 2 renders `?`.** `relationship_marker` resolves the claimed sender
  against Agent Specs only; a principal is not one. This matches the existing
  `nightly-timer` case in `src/ding/mod.rs`. `?` currently means *both* "not a
  declared agent" and "the catalog is unreadable / the chain is broken" — it
  cannot say "this is a machine producer, not a peer".
- **Rep 2's subject is useless.** `request::publish` hardcodes
  `format!("request {idempotency_key}")`, so the CI state never reaches the
  notice. The poller run makes the cost obvious — four events, four notices, no
  human-readable content beyond the key:

  ```
  POLLER DING: [DING] ? h.pipe-gh-ci: request github:ci:pr-42#pending [id:xaw0pe]
  POLLER DING: [DING] ? h.pipe-gh-ci: request github:ci:pr-42#running [id:37e4fd]
  POLLER DING: [DING] ? h.pipe-gh-ci: request github:ci:pr-42#failure [id:mxhpt4]
  POLLER DING: [DING] ? h.pipe-gh-ci: request github:ci:pr-42#success [id:fmz6sn]
  ```

  Because the key is doing double duty as the only visible label, this is
  legible only by accident of how the poller composes it.
- **Rep 1 renders a lie.** Because the producer had to be declared as an Agent
  Spec, the marker resolves to a real hierarchy glyph (`←` for a sibling). A
  machine event is presented to the agent as peer-agent traffic.

### 4. Burst behavior

Ten rapid distinct events (a CI status flap) through each representation land
completely — nothing coalesces, nothing is dropped
(`a_ten_event_burst_lands_completely_and_drains_fifo_by_filename_order`):

```
REP1 burst: 0 same-millisecond collisions among 10 events
REP1 burst: drain order matches emission order
REP2 burst: 0 same-millisecond collisions among 10 events
REP2 burst: drain order matches emission order
```

In-process emission costs more than a millisecond per event in both
representations, so a realistic burst preserves order. But the wire format
cannot *represent* sub-millisecond order: `list_dir` sorts by
`(ts_ms, filename)` and the `<rand6>` suffix is random. Constructed
deterministically in
`same_millisecond_events_drain_by_random_suffix_not_emission_order`, two events
sharing a millisecond drain in random-suffix order, not emission order. A
higher-rate producer than this one would need a stronger ordering token.

DING drains FIFO, one notice per delivery: `new_arrivals` yields the unread set
in `(ts_ms, filename)` order and `flush_pending` works the front of a
`VecDeque` with a single staged payload, popping only on `Delivered`.

**Where "coalesce to latest per stream-key" would have to live.** Not in DING:
`flush_pending` sees one notice at a time and holds exactly one staged payload,
and the only place with the full unread set is the protected enqueue path.
Adding a stream-key rule there would also mean DING deleting inbox items, which
it deliberately does not do. The remaining place is the **producer**: archive
the superseded item for the stream key before publishing the new one. Proven in
`producer_side_supersede_collapses_a_stream_to_its_latest_event` — four CI
transitions leave exactly one unread item (`success`) and three durable archived
predecessors, because the archive receipt guarantees the superseded notices are
not resurrected. This needs no new st2 concept, only a documented producer
pattern.

### 5. Provenance

| Question | Rep 1 | Rep 2 |
| --- | --- | --- |
| Which producer? | `from:` frontmatter — but it is an Agent Spec identity, indistinguishable from a real agent | `from:` is the declared principal; `principals/` membership proves it is not an agent |
| Which external event id? | `source:` / `event-id:` frontmatter, visible in `message read` and `message ls --json` | `tags.source` / `tags.event-id` inside the JSON envelope, via `st2 request read --json` |
| Created vs deduplicated? | `--json` receipt: `{"status":"published","to":"h.worker","filename":"…","deduplicated":false,"idempotencyKey":"github:ci:pr-42#failure","source":"github:ci:pr-42","eventId":"failure"}` (spike addition) | `--json` receipt: `{"status":"published","idempotencyKey":"github:ci:pr-42#failure","filename":"…","deduplicated":false}` (already shipped) |
| Receipt after the fact? | none — the producer must re-derive the key and re-send, or read `message sent --json` | `st2 request status --idempotency-key … --json` returns the tagged union `pending \| replied`, including the agent's reply body |
| Readable without a decoder? | yes, ordinary markdown + `message read`/`ls`/`thread` | no, the body is an opaque JSON envelope |

Proofs: `rep1_provenance_is_readable_frontmatter_on_an_ordinary_message`,
`rep2_provenance_is_a_typed_envelope_plus_a_principal_side_receipt`.

Rep 2's `status` channel is the only one that closes the loop: the test
publishes an event, observes `pending`, has the agent `request reply`, and then
reads `replied` with the agent's body. Rep 1 has no analogue keyed by event id.

### MESSAGE-R11, measured

Five events each way. `st2 message sent h.pipe-agent --json` reports 5 rows —
Rep 1 writes one sender-owned Sent row per pipe event into the producer's
ordinary Agent Sent history. The principal has no Agent Sent index at all and
the command fails for it. Proof: `rep1_pollutes_agent_sent_history_but_rep2_does_not`.
MESSAGE-R11 explicitly keeps request publication state out of Sent history; Rep 1
puts machine ingress squarely inside it.

### Cost

Ten sequential in-process emissions
(`report_sequential_emission_cost_of_each_representation`):

```
REP1 10 sequential in-process emissions: 58.2ms
REP2 10 sequential in-process emissions: 70.9ms
```

Rep 2 is the slower of the two despite having no sender lock: it runs catalog
discovery twice per publish (`resolve_principal` then `resolve_agent`) and
hashes the state key. Rep 1's cost is an exclusive flock plus a hash-chain
commit. The important structural difference is not the mean but the contention:
Rep 1 serializes every pipe event through the producing agent's single
`SentLock::exclusive` (MESSAGE-R08), so a chatty pipe competes with that agent's
own outgoing messages. Rep 2 has no shared lock.

## Comparison

| Axis | Rep 1 — message path | Rep 2 — principal path |
| --- | --- | --- |
| Idempotency scope | `(canonical sender, canonical recipient, key)`, sender-owned ledger (MESSAGE-R07) | `(principal, key)` in the principal's `request-state/outgoing`; the recipient is validated but not part of the key |
| Fan-out to several agents | works: the recipient is part of the key | **refused** — the second recipient collides; needs the recipient in the key |
| Concurrent replay | one item, one filename (12 threads) | one item, one filename (12 threads) |
| Post-archive replay | file never re-created; no re-notify | file re-linked, then shadowed and cleaned by the archive receipt; no re-notify |
| Crash between reserve and publish | resumable pending intent | reserved filename + envelope, replay finishes the same publication |
| Provenance | readable frontmatter on an ordinary message | typed JSON envelope + principal-side receipt |
| Outcome channel | one-shot receipt only | `request status` → `pending \| replied`, with the agent's reply |
| Authoring burden | producer must be a declared Agent Spec: roster row, presence, derived DING, doctor checks | three lines of KDL; no task, presence, persona, or authority |
| Marker rendering | resolves to a real hierarchy glyph — machine traffic looks like a peer agent | `?`, the same glyph as "unknown/broken", with a `request <key>` subject |
| Fit with MESSAGE-R11 | violates it: one Agent Sent row per pipe event | satisfies it by construction |
| Fit with R10 / MESSAGE-A01 agent-only identity | breaks it: a non-agent producer must impersonate or become an agent | preserves it: a principal is explicitly not an Agent Spec identity |
| Consumer ergonomics | `message ls/read/thread` work unchanged | needs `request read`; the body is opaque to ordinary message tooling |
| CLI surface today | `st2 message send --source --event-id --json` (spike) | `st2 request send --idempotency-key --tag --json` (shipped) |

## Recommendation

**Adopt Rep 2, the principal path, as the ingress representation for pipes, and
fix its notice.**

The deciding evidence is not idempotency — both representations are
exactly-once-safe under concurrent replay and neither re-notifies after archive.
It is identity and separation, and both are executable:

1. Rep 1 cannot express a non-agent producer at all
   (`rep1_refuses_a_non_agent_sender_but_rep2_accepts_a_declared_principal`).
   Every pipe would either impersonate an existing agent or inflate the roster
   with fake agents that acquire presence, a derived DING sidecar, and doctor
   health obligations. This is precisely what the service-principal transport was
   introduced to avoid.
2. Rep 1 writes machine ingress into the producer's Agent Sent history
   (`rep1_pollutes_agent_sent_history_but_rep2_does_not`), which MESSAGE-R11
   already rules out for typed requests. Pipes have the same character.

Rep 2's weaknesses are all in presentation, and all fixable without touching
delivery:

- Let `request send` take an optional `--subject` (or derive one from a
  well-known tag) so the notice carries the CI state instead of the key.
- Give the DING marker a distinct glyph for a declared non-agent principal, so
  `?` keeps meaning "unknown" rather than doubling as "machine".
- Add `to` and an echo of the caller's tags to the request receipt so it is as
  self-describing as the spike's message receipt.
- Scope the request state key by `(principal, recipient, key)` so one event can
  fan out to several agents, matching MESSAGE-R07.

Keep the message path's spike additions only as a comparison artifact. The one
piece worth keeping independently is the `SendOutcome`/`deduplicated` receipt:
a keyed `message send` currently prints a filename and no outcome, so an
ordinary agent retrying a keyed send cannot tell what happened either.

For burst control, adopt **producer-side supersede** as a documented pattern
rather than a DING feature: archive the previous item for the stream key before
publishing the next. It is one extra CLI call, it keeps the superseded events as
durable history, and the archive receipt already guarantees the retired notice
cannot come back.

## Frictions

- **No principal-authoring CLI.** A principal is created by hand-writing
  `principals/<host>/<identity>/principal.kdl` with content that must exactly
  match its path. `st2 agent-author`-style tooling has no principal analogue, so
  a Nix module or a human writes the file. Trivial to add, and its absence makes
  Rep 2 look harder to adopt than it is.
- **Hardcoded request subject.** `request::publish` bakes
  `format!("request {idempotency_key}")` into `rendered_message`, which is part
  of the durable publication record. It cannot be changed after the fact and it
  is the only thing DING shows.
- **The `?` marker is overloaded.** It means "unknown sender", "unreadable
  catalog", and "broken supervisor chain" already; a principal makes it also
  mean "legitimate machine producer".
- **`message send` had no outcome.** Before this spike, a keyed retry and a
  fresh send printed the same thing.
- **`send_to_resolved_inbox` has 10 arguments** and grew an eleventh concept.
  `Provenance` collapsed three of them; a real change should go further.
- **Request receipts omit the recipient.** `PublishReceipt` has no `to`, so a
  producer emitting to several agents cannot correlate receipts without tracking
  its own call sites.
- **Same-millisecond ordering is unrecoverable** in the frozen filename grammar.
  Fine for a poller, not for a webhook fan-in.

## Open questions for a human

1. Should a pipe be a *declared* catalog object (`pipe "gh-ci" { … }`) that st2
   supervises on a cadence, or does st2 stop at the ingress boundary and leave
   scheduling to systemd/launchd? Issue #137 puts "declare and supervise a
   recurring shell-driven source" in scope; this spike only proves the ingress
   half, and the poller is driven externally.
2. Is `(source, event-id)` the right event identity, or should the dedup key be
   `(stream-key, revision)` so supersede is a first-class outcome rather than a
   producer convention? The CI case wants "latest wins per PR"; the timer case
   wants "one per day, never superseded". This also decides the fan-out shape:
   is one event to three agents one operation with three deliveries, or three
   operations? Rep 1 answers "three" by scoping the key to the recipient; Rep 2
   currently answers "one" and then refuses the second delivery.
3. Should the DING marker distinguish a declared principal from an unknown
   sender, and if so with what glyph? This changes a string every agent sees.
4. What retention rule applies to `request-state/outgoing` records after the
   inbox message is archived? They are the only thing preventing a much later
   replay from re-publishing, and nothing prunes them today.
5. Does a pipe event deserve a reply channel at all? Rep 2 gives one for free
   via `request status`; if pipes are fire-and-forget, that is unused machinery,
   and if they are not, `pending | replied` may be too thin for a long-running
   subscription.
6. Should `st2 message send --json` and its `deduplicated` outcome ship
   independently of pipes? It is a small, generally useful honesty fix.

## Conclusion

Both representations satisfy issue #137's exactly-once, concurrent-replay, and
archive-receipt requirements. The message path fails on identity: it cannot
carry a non-agent producer without inventing a fake agent, and it writes machine
ingress into Agent Sent history that MESSAGE-R11 deliberately keeps out. The
principal path preserves both boundaries and already ships a durable outcome
channel; its only real defects are that the DING notice says `request <key>`
instead of what happened, and that `?` is an overloaded marker. Pipes should be
built on the service-principal transport, with a subject on the request and a
distinct marker for declared machine producers.

## VRS Impact

None yet. This is a throwaway spike carrying no requirement, no ontology term,
and no INVARIANTS entry. It is evidence for DQ1's executable-eval bar and for
whether issue #137's ingress boundary belongs on `message send` or
`request send`.

## Reproduce

```sh
nix develop -c cargo test --test pipe_ingress -- --nocapture
```

16 tests, all green. Regression suites run alongside, covering the named
INVARIANTS proofs that touch the message wire format: `--test message` (6),
`--test message_cli` (17), `--test request_cli` (9), `--test invariants` (1),
`--test doctor` (9), `--test run` (47), `--test pty` (5), `--test status_agents`
(10), `--test predecessor_ding_migration` (3), and `cargo test --lib` (325).

Pre-existing failures on this HEAD, reproduced on a clean stash and unrelated to
this spike:

- `tests/eval_run_e2e.rs::canonical_agents_freeze_the_admitted_route_across_post_boot_catalog_mutation`
  — fails identically with the spike stashed.
- `src/codex_app_server/tests.rs::runtime_owner_lock_is_nonblocking_and_released_on_close`
  — flaky under parallel `cargo test --lib` when the machine is also compiling;
  green in 4/4 subsequent full runs and green in isolation.
- `tests/native_only.rs::tracked_product_surface_contains_only_native_names` —
  trips on `docs/vrs/spec.md:584` ("st2 removes it from the launch
  environment"), a file this spike never touched, last changed by e4e9d56.
- `tests/native_only.rs::clean_path_supports_help_validate_env_and_doctor` —
  spawns `/bin/sleep`, which does not exist on this NixOS host.
- `cargo fmt --check` reports diffs in 43 files this spike never touched, so
  formatting is not a gate here.
