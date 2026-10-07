# Harness context requirements

**Status:** Accepted — confirmed by Johannes on 2026-08-29

## Context

This subsystem defines the **harness context record**: the driver-written
numeric record of how full an agent's harness context window is, how often that
harness has compacted, and the adjacent facts the same channel carries (model,
harness-reported session cost, harness-reported rate limits, cumulative session
tokens). It is a sibling of
[observed harness state](../05-harness-state/requirements.md), which owns the
categorical axis (`state`, `blockedOn`, `ask`, `inputBuffer`) and keeps its own
fencing, freshness, and derivation rules. Where this file and
[`05-harness-state`](../05-harness-state/requirements.md) or the
[root requirements](../requirements.md) disagree, they win and this file is
wrong.

The motivation is the context-saturation failure mode: an agent runtime that
fills its window and compacts (or wedges) with nobody watching. Nothing in the
fleet detects it today — dotfiles' Claude hook chain maps `PreCompact` to a
transient `status=compacting` PTY event and persists nothing, tokenlens records
a retrospective `compaction_count` from transcripts and has no context-window
column at all, and no st2 supervisor, reconcile, or flapping path reads any
harness record.

Provider-specific arithmetic belongs to harness drivers, exactly as
[`OHS-R05`](../05-harness-state/requirements.md) assigns categorical
classification to them: st2 core owns the envelope, its guard, its freshness,
and its exposure. The record-shape and placement decisions are recorded in
[`.decisions/0014`](../.decisions/0014-harness-context-is-a-sibling-numeric-record.md).
Delivery gating is not this subsystem's concern and no delivery path reads this
record.

## Assumptions

- **HC-A01 Every maintained harness publishes its own fill number:** each of the
  five drivers has a positive channel carrying occupancy — Claude's `statusLine`
  stdin payload, Codex's `thread/tokenUsage/updated` app-server notification,
  pi's and omp's `ctx.getContextUsage()` inside the injected extension, and
  OpenCode's `message.updated` SSE frame joined with a `GET /config/providers`
  pull. Where a channel yields no number the driver writes nothing, and the
  absence of a record is honest ("never observed"), never a fabricated zero.
- **HC-A02 Advisory surface:** consumers are humans, a roster, a TUI, and
  Doctor. The record authorizes nothing: no delivery, no lifecycle action, no
  reconciliation, and no supervisor branch. A wrong-but-fresh number can mislead
  an operator; it cannot misdeliver a message or end a runtime. This mirrors
  [`OHS-A02`](../05-harness-state/requirements.md) deliberately — supervisor
  actionability is a separate decision that must first settle
  [`DQ-H5`](../05-harness-state/open-questions.md).
- **HC-A03 Trusted writers:** the record is unauthenticated catalog state under
  the trusted-fleet model (root `A02`). The writers are the owning session's
  driver processes — the wrapper, its channel, and the hook or status-line
  subprocesses it shares its incarnation token with; nothing verifies that
  claim.
- **HC-A04 No single formula reproduces five harnesses:** measured 2026-08-29,
  every harness computes "context used" differently — different numerator
  (last-response total vs prompt-only input vs an st2-computed join),
  different denominator (a raw window vs a window with a fixed baseline
  subtracted), and different display rounding. An st2-normalized number would
  disagree with what the operator sees in the harness's own UI, and that
  disagreement is the first thing a human files as a bug.

## Acceptable Tradeoffs

- **HC-T01 Percent is an operator view, not a physical quantity:** because
  `usedPercent` is harness-native (HC-A04), comparing it across harnesses
  compares "what each operator would see", not one measured ratio. The record's
  `harness` field is what tells a reader which arithmetic produced the number;
  the per-harness rule is stated in the spec's producer table.
- **HC-T02 No history in v1:** the record carries a compaction counter and one
  timestamp, so a second compaction overwrites the first's detail. Per-event
  history is deferred, not deleted, and is expected to be settled together with
  observed harness state's deferred transition history
  ([`OHS-T03`](../05-harness-state/requirements.md)) rather than separately.
- **HC-T03 Version-coupled constants:** Codex's 12,000-token baseline and omp's
  input-only `tokens` semantics are properties of a specific harness build, not
  of a documented API contract. v1 accepts that a harness bump can silently
  change what the number means, bounded by a per-harness fixture test pinned to
  the verified version (HC-R13) under the omp version-gate discipline
  ([`OMP-R05`](../06-omp-driver/requirements.md)).
- **HC-T04 Numbers are unfenced:** the record carries no ownership sequence, no
  written claim, and no floor sidecar. A straggler from a superseded session can
  land a reading; all it says is "this number is older than you think", which
  the record's own origin timestamp already says, and the next reading
  overwrites it. This is the deliberate difference from
  [`OHS-R01`](../05-harness-state/requirements.md)'s fenced envelope, whose
  machinery exists to stop a straggler resurrecting a *live state* — a
  dangerous lie a stale number cannot tell.
- **HC-T05 Lag is bounded only by the heartbeat:** quantization bounds *error*
  unconditionally (a reading is never more than one bucket away from the truth)
  but leaves *lag* unbounded on its own — an agent that stops consuming window
  stops crossing buckets. The heartbeat is what bounds lag, and between
  heartbeats a reader may hold a reading several minutes old. This is tolerable
  only because the reader surfaces age rather than hiding it (HC-R06): unbounded
  lag is reported, not silent. If the age projection were ever dropped, this
  tradeoff would no longer hold and the policy would need a time floor.
- **HC-T08 Replication depends on a list this repository does not own:** the
  transport's include list lives in the fleet's own configuration, so st2 can
  publish a record it believes is replicated while the other side does not carry
  it. Nothing errors when that happens. v1 accepts the split ownership — the
  alternative, contorting the record's location to match a glob written for
  other purposes, buys nothing a named include entry does not — bounded by
  st2-side test that pins the names it expects (HC-R05), and by the fact that no
  correctness property here depends on the transport at all: everything works
  with no replication, and remote visibility is what is lost. Both records are
  published mode `0600` — as is every record st2's shared publication primitive
  writes, though not the bus message files, which stay at the writing process's
  umask — so a transport reading the catalog as a different unprivileged uid
  needs that decision taken deliberately rather than inherited from a default.
  No such transport runs today, which is also what leaves the wire-cost half of
  `DQ-C1` unmeasured.

## Requirements

### Must publish one numeric envelope

- **HC-R01 Context envelope:** each agent has at most one harness-context
  record, `<agent-dir>/harness-context`, schema `st2.harness-context.v1`,
  written only by the owning session's driver processes under its own lock, and
  replaced atomically. It carries the fill
  triple
  (`usedTokens`, `windowTokens`, `usedPercent`), the compaction triple
  (`compactions`, `lastCompactionMs`, `lastCompactionTrigger`), the observing
  harness, an origin timestamp, and the adjacent facts of HC-R16. Reads are
  additive-tolerant: a reader may be older than its writer, and unknown future
  enum words decode as indeterminate, never as a definite value.
- **HC-R02 Harness-native fill:** `usedPercent` is the number the harness itself
  displays to its operator, computed by that harness's own rule; `usedTokens`
  and `windowTokens` are the harness's own operands. A producer that cannot
  obtain a window withholds `usedPercent` rather than dividing by a table, an
  estimate, or a default. The value is carried raw and is never clamped by a
  producer or a reader — occupancy above 100% of the window is observed in
  practice and is precisely the condition worth surfacing; clamping belongs to
  whatever renders it.
- **HC-R03 Withheld, never fabricated:** where a harness positively reports that
  it does not know its own occupancy — pi after a compaction, Claude before the
  session's first API response — the producer withholds the value. No path
  substitutes zero, the previous reading, or a derived estimate for a
  harness-declared null.
- **HC-R04 Transport-safe freshness:** freshness lives in the record bytes: an
  embedded origin timestamp with its own staleness and future-skew constants,
  deliberately not aliases of the presence or harness-state constants. No read
  path consults file mtime, and every landed write is byte-distinct so a
  transport that carries content but not metadata always carries a refresh.

- **HC-R05 Transport-visible placement:** the record must be carried by the
  catalog's replication transport, and every temporary, partial, or staged file
  the writer produces must live **outside** the replicated subtree. Both
  failures are silent: a record the transport does not carry is invisible to
  exactly the remote readers a catalog record exists for, and a staged temporary
  name inside the replicated subtree becomes a durable replicated key that is
  restored after a later local delete. The record's own location stays beside
  `harness-state` at the agent-directory root, where the driver records belong;
  it is the transport's include list that names them, so this requirement is met
  by the two sides agreeing on that list rather than by moving the record. st2's
  side of that agreement is pinned by a test asserting the exact record names it
  expects the transport to carry, so a rename here cannot silently stop
  replicating.

### Must stay readable when the categorical axis is not

- **HC-R06 Stale readings are returned, not derived away:** past the staleness
  horizon a reader returns the reading it found, marked `stale` and carrying its
  derived age. There is no `unknown` vocabulary on this axis: an old number with
  a visible age is more useful to the operator this record exists for than an
  absence, and it cannot mislead a lifecycle action (HC-A02).
- **HC-R07 Independent of observed state:** the numbers survive every state the
  categorical record can derive. An `observedState` of `unknown` — stale,
  session-dead, malformed, or a fresh claim placeholder — does not blank, hide,
  or invalidate the context reading, because the wedge case this record exists
  for is exactly an agent runtime whose state has gone indeterminate at 190k of
  a 200k window.

### Must never wake what it informs

- **HC-R08 No delivery wake:** a write to the harness-context record wakes no
  delivery pump, no reconciliation pass, and no watcher owned by its own writer.
  Delivery pumps watch by allowlist — the `resources/inbox` subtree and the
  presence record — and the record is a sibling of neither, so it is ignored
  with no production change.

### Must bound its own write rate

- **HC-R09 Quantized writes:** a reading is written when it enters a different
  bucket of the window, when a compaction occurs, or when the record is older
  than a named heartbeat interval — whichever comes first. The bucket width and
  the heartbeat are named constants. Quantization is required over a
  time-floored movement guard because it ties write cost to *information*
  rather than to producer chattiness: writes per window fill are capped at
  `100 / bucket-percent` no matter how often a harness emits a reading, so no
  future producer can inflate the write rate by restating, and no per-harness
  tuning is needed.
- **HC-R10 Threshold-agnostic resolution:** the bucket width is chosen so that a
  reader's value always shares the truth's bucket at every alarm threshold, not
  only at multiples of the bucket width. A policy whose accuracy depends on
  where a future consumer sets its alarm couples the write policy to that
  consumer; this requirement forbids that coupling.

### Must be produced by every maintained harness

- **HC-R11 All-harness coverage:** Claude, Codex, pi, omp, and OpenCode each
  ship a producer. Each producer's numerator, denominator, percent rule, and
  withholding conditions are stated per harness in the spec; a harness with no
  producer would leave its declarations reading `null` indefinitely and
  re-create the two-tier observability this record exists to close.
- **HC-R12 Compaction accounting:** the record carries a compaction count, the
  time of the last compaction, and a trigger drawn from the closed vocabulary
  `manual | auto | threshold | overflow | idle | unknown`. The spec names, per
  harness, which edge produces the count, which trigger words that harness can
  yield, and whether the count is scoped to the session incarnation or is
  harness-durable across restarts.
- **HC-R13 Version-pinned producer fixture:** each producer's arithmetic is
  proved by a fixture captured verbatim from the named harness version, and the
  version is asserted in the test. A harness bump that changes the numerator,
  the denominator, or a magic constant must fail that fixture rather than
  silently publish a differently-meaning number.

### Must be readable beside the other axes

- **HC-R14 Roster join:** `st2 agents --json` carries `context` as a fourth
  independent top-level axis beside `status`, `observedState`, and
  `driverDiagnostic`, always emitted and `null` when no record exists, following
  the roster's existing null convention. It never rewrites presence, desired
  lifecycle, observed state, or `lastActivity`. The pinned roster wire
  assertions and the *Stable roster JSON* invariant wording change deliberately,
  in the same change, with the new proof named.
- **HC-R15 Session-boundary reset:** the relaunch claim that supersedes the
  harness-state record also removes the harness-context record, so a new
  incarnation reads "no context yet" rather than the previous incarnation's
  fill. `incarnation` is carried in the record as provenance — evidence of which
  session produced a number — and is never consulted as a fence.
- **HC-R16 Adjacent facts, one owner each:** where the same channel supplies
  them, the record carries the model identifier, the harness-reported cost at
  the scope the harness reports, the harness-reported account rate limits, and
  the cumulative session token total. Each is carried as what the harness
  reported and nothing more; absent facts are `null`, never zero. The cumulative
  total is named so it cannot be mistaken for occupancy, and it is never the
  numerator of any percent.
- **HC-R17 Doctor advisory:** Doctor surfaces a warning for an agent it owns
  when the reading is at or above a named st2 warning threshold, and when the
  record is stale beside a `running` desired state. Both are advisory output and
  neither ever changes Doctor's exit status, matching how Doctor already treats
  the categorical axis (`OHS-R10`). The threshold is st2's own number for "worth
  a human's attention", explicitly **not** a prediction of where the harness will
  compact — that point is harness-, model-, and setting-specific, and a number
  claiming to know it would be wrong in both directions.
- **HC-R18 The status-line tee must chain:** where st2 occupies a harness's
  single status-line slot to read a producer channel, it must invoke the
  operator's own renderer and pass the payload on. The slot is single-valued and
  the highest-precedence declaration replaces the others outright — nothing
  merges — so occupying it without chaining silently removes whatever the
  operator had, on every managed agent, with no warning. Where no downstream
  renderer resolves, and wherever a resolved renderer fails, the tee writes
  nothing to stdout: the degraded case is an EMPTY status line, and its
  diagnostic goes to stderr, which the harness does not render. Silence is
  required rather than merely permitted, because the payload on that channel is
  machine-readable JSON — session id, transcript path, model and usage blocks —
  and a slot that echoes it paints that JSON across the operator's terminal at
  the refresh cadence. That is strictly worse for them than a blank row and
  carries nothing they can act on, so "transparency over silence" is the wrong
  trade for this surface even though it is the right one for a channel a human
  reads. Which arm runs never affects recording; only the human-facing line is
  at stake. The inverse also holds and is not solved by chaining: a renderer a
  human sets in a file st2 materializes is not preserved by st2's merge, which
  owns only its own hook entries.
