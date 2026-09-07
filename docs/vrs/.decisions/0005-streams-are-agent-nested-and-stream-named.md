# Streams are agent-nested declarations, and "stream" anchors the vocabulary

Status: accepted

Design decision made by Johannes on 2026-08-20 (interview over four executable
prototypes; vocabulary designed against Kafka/Unix prior art and the existing
ontology). Merge and acceptance approval required: upstream maintainers.

## Context

A subscription that turns external-world changes into inbox events needs a
declaration site and a name. The lifecycle prototype nested a `pipe` node
inside the agent; both design explorations independently placed a top-level
producer declaration beside agents (`sources/` / `principals/`) for
identity-grammar reasons, and both flagged the locality as a human call. The
working name "pipes" came from the branch, `pipe` from Unix and the Claude
Code Monitor lineage; issue #137 left the command name deliberately open.

## Decision

The declaration is **agent-nested**: `agent { stream "gh-ci" { command … } }`.
The subscription is visible on its subscriber, the recipient is implicitly the
owner, lifecycle coupling is the already-proven derived-companion behavior
(launch with the agent; stop on suspend, retire, park), and an agent
self-subscribes by editing its own declaration through the existing serialized
catalog-authoring path — R25's self/descendant authority covers it unchanged.
Because the stream belongs to its agent, no non-agent identity is introduced
and R10 survives untouched.

The vocabulary is **stream-centric**: *stream* (the named declared event
producer feeding an agent), *event* (one item on a stream), *key* (optional
grouping axis; supersession collapses unread events per `(stream, key)` — the
borrowed intuition is Kafka log compaction), *event-id* (producer-supplied
dedup identity), *adapter* (the world-specific program a stream runs), and
*stream task* (the derived companion, an implementation-level follower). A
stream declared with either `command` or `argv` runs that adapter as a
supervised stream task. Only a stream with neither adapter form is an external
ingress endpoint: outside producers emit into it via `st2 event emit`, which
natively covers issue #137's bootstrap-timer case. "pipe" is retired as a
working title. Self-authoring exposes the same choice as mutually exclusive
`--command <shell>` and direct `-- <program> [<arg>...]` forms.

## Options

| Option | Result | Reason |
| --- | --- | --- |
| Agent-nested declaration: `agent { stream "…" { command … } }` or `argv …` | Selected | The subscription is visible on its subscriber, lifecycle coupling is the proven derived-companion behavior, and self-authoring stays inside R25's existing authority. Accepted cost: two agents watching one feed run two adapters until a top-level generalization exists. |
| Top-level `source` declarations beside agents | Rejected for v1 | An agent's declaration no longer shows what can wake it, self-subscription becomes authoring a foreign declaration outside R25, and subscriber lifecycle coupling was the one part neither design spike built. Reserved as the future shared-feed generalization. |
| *pipe*-anchored vocabulary | Rejected | Unix "pipe" connotes a byte transport between two processes, not a durable deduplicated feed, and it forces two anchor-weight words for one family. |
| *source*-anchored vocabulary | Rejected | Producer-perspective ("the agent's sources" says where events come from, not that they flow to you) and collides subtly with R13's "source" classification axis. |

## Evidence and Argument

The lifecycle prototype proved the nested form end to end: 27 tests over the
derived-companion seam with zero changes to run, flapping, park, or task
inventory, covering one-pass launch, suspend/retire teardown, independent
parking with supervisor surfacing, and honest `st2 tasks --json`. Both design
explorations specified top-level declarations but left subscriber lifecycle
coupling unbuilt and flagged locality as a human call — so the only fully
proven locality is the nested one. On vocabulary, the borrowed intuition
decides: supersession per `(stream, key)` is exactly Kafka log compaction, a
mental model operators already hold, while "pipe" and "source" each import a
weaker or colliding intuition. The adapter-less stream unifies external
ingress into the same concept, which lets issue #137's bootstrap-timer case
run natively without a second producer-identity mechanism.

## Consequences

- Homograph to manage: R13–R15 use "event"/"event streams" for filesystem
  watcher machinery. The ontology qualifies those as *watcher events*; future
  requirements edits keep the qualification.
- Issue #49's "topic" remains a message-side axis; the event-side grouping
  axis is *key*. The two must not be merged by name.
- Suspension semantics follow locality: the stream task is an owned task, so
  R41 tears it down with the agent — eyes closed, no accumulation, resume
  re-observes; the dedup ring makes re-emission of still-current state safe.
- Ingress authority follows the same locality: `event emit` runs only when its
  resolved logical owner host and catalog lock match the unsynchronized
  machine-local binding established by the active supervisor. This supports
  declared/`--host` logical aliases that differ from the OS hostname without
  letting caller-controlled resolution grant authority. Synchronized remote
  catalogs cannot publish; cross-host observations need an owner-domain
  forwarding adapter/transport, which v1 does not provide.
- Public removal is lifecycle-first: under catalog then exact-runtime
  serialization, `stream rm` stops/retires an adapter companion and durably
  proves process absence before publishing declaration removal. A failure
  before source publish leaves the declaration intact for reconcile to
  relaunch; adapter-less external ingress skips runtime teardown. Direct KDL
  edits are not the self-authoring operation and carry no orphan-free promise.
