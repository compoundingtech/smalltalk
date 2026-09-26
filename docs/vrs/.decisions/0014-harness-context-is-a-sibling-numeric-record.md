# Harness context is a sibling numeric record

Status: accepted

Design decision made by Johannes on 2026-08-29 (an interview over four research
reports and one throwaway spike, all recorded in
[`08-harness-context/.experiments/2026-08-29-context-signals-and-write-placement.md`](../08-harness-context/.experiments/2026-08-29-context-signals-and-write-placement.md)).
Interview handles q1–q11. The write policy (q6) was deliberately deferred at
interview to a benchmark and settled on it —
[`08-harness-context/.experiments/2026-08-29-write-policy-benchmark.md`](../08-harness-context/.experiments/2026-08-29-write-policy-benchmark.md).
That benchmark also found that neither driver record would replicate at all;
q11 settles how (the transport's include list names them, rather than the
records moving). Merge and acceptance approval required: upstream maintainers.

## Context

st2 exposes what an agent declares (presence), and — since
[decision 0006](./0006-observed-harness-state-is-a-driver-written-catalog-record.md) —
what its harness is observed *doing*. It exposes nothing about how full that
harness's context window is. The motivating failure mode is context saturation:
a runtime fills its window and compacts, or wedges, with nobody watching.

Nothing in the fleet watches it today. The dotfiles status-line renderer already
parses Claude's `used_percentage` and prints it without persisting anything; the
dotfiles hook chain maps `PreCompact` to a transient PTY event with no counter;
tokenlens — the fleet's registered token and cost producer — has a
transcript-derived `compaction_count` and no context-window column at all, and
lists context replay as future work. No st2 supervisor, reconcile, or flapping
path reads any harness record.

The 2026-08-29 measurements established that all five maintained harnesses
publish occupancy on a channel st2's drivers already sit on, that no two compute
it the same way, and that two of them positively report "I do not know" where a
naive producer would report zero. What remained was where to put the numbers,
how often to write them, what vocabulary to publish, and what anyone is allowed
to do with them.

## Options

| Option | Result | Reason |
| --- | --- | --- |
| Sibling record `<agent-dir>/harness-context` with its own lock, write policy, and 60-min horizon (q4) | Selected | The numeric axis wants a movement guard; the state record's guard is an equality test over a categorical tuple and cannot express one without redefining `sinceMs` and `transitions`. The sibling also keeps the numbers readable when the state derives `unknown` — the wedge case — and the delivery watcher's allowlist makes it inert with no production change. Accepted cost: ~65% more production code than folding it in, and one more file per agent. |
| `context` sub-object inside `harness-state`, coalesced onto state transitions and the 5-min refresh | Rejected | Zero extra writes and ~40% less code, but measured to report the number the turn *started with* for up to five minutes; needs its own `context.observedAtMs` because `heartbeat()` re-stamps without re-reading; needs a merge rule because context-blind observations would erase the numbers; and drops them on all seven `unknown` derivations. Defensible for "roughly how full is this agent", wrong for "warn me before it compacts". |
| Inside `harness-state`, every reading a transition (A-strict) | Rejected | Measured `2 + 2K` writes per turn (32 for a 15-tool-call turn), and it silently redefines two shipped fields: `sinceMs` becomes a token-delta stamp and `transitions` a turn counter. |
| Inside `harness-state`, every reading a "quiet" write that opens no transition | Rejected | Same write count as A-strict by construction, and it adds a third write class ("changed but not a transition") to the module whose invariant is that a landed write *is* a transition or a refresh. Named in the spike, not implemented. |
| Harness-native `usedTokens` / `windowTokens` / `usedPercent` (q2) | Selected | `usedPercent` is the number the harness itself displays, so it agrees with what the operator sees; raw tokens ride along for anyone doing their own arithmetic. `harness` is the discriminator for which rule produced it. |
| One normalized st2 formula across harnesses | Rejected | One physical meaning, but it disagrees with every harness's own UI — by the 12,000-token baseline on Codex (36% vs the displayed 33%), and by output plus cache between omp and pi. That disagreement is the first thing a human files as a bug. |
| Percent only, no raw tokens | Rejected | Smallest record, but it cannot show that a window *shrank* (a model switch), cannot be recomputed, and hides the join OpenCode's producer has to make anyway. |
| Compaction as a counter plus last time and trigger in the record (q3) | Selected | One write per compaction (rare); "how many times has this runtime compacted" is answerable from the roster. Per-compaction detail is not recorded. |
| Compaction as an event on the agent's stream, record carries nothing | Deferred | Full history with per-event detail, but the roster cannot show a count without folding the stream, and the hook-subprocess producers have no stream-append plumbing. Kept as the roadmap direction, together with the deferred state history (`OHS-T03`, `DQ-C6`). |
| Provenance-only fencing: `incarnation` carried, the relaunch claim removes the record (q7) | Selected | A stale token count says only "this number is older than you think", which the origin timestamp already says. One extra file operation on the claim path buys "no context yet" after a relaunch instead of the previous incarnation's fill. |
| Reuse the state record's incarnation + seq + claim + floor sidecar | Rejected | Roughly doubles the sibling's production code to duplicate a protocol whose only justification is preventing a dangerous *state* lie. |
| Advisory only, mirroring `OHS-A02` (q1) | Selected | A wrong-but-fresh number can mislead an operator; it cannot misdeliver or end a runtime. Supervisor actionability would first have to settle `DQ-H5`, unmet and one of the reasons decision 0006 ships Draft. |
| Supervisor may branch on the number now | Rejected | Requires bounded-staleness semantics for a remote reader (`DQ-H5`) and an action vocabulary, in this effort. An action taken on a stale reading is not undone by a spec edit. |
| Also carry `model`, `costUsd`, `rateLimits`, `sessionTotalTokens` (q9) | Selected | Each arrives free on a channel already being read. `model` is what explains a mid-session window change; cost was explicitly in scope; the two contested fields are carried with their limits documented rather than dropped. |
| Fixed 1% quantization + compaction edge + 300 s heartbeat as the write policy (q6, benchmarked) | Selected | The only measured policy with zero missed high-occupancy warnings at every threshold from 80% to 97%; error bounded at one bucket unconditionally; writes per window fill capped at 100 by construction, so producer chattiness cannot inflate cost and one constant serves all five harnesses. |
| The spike's movement guard (≥5% move bypassing a 60 s floor) | Rejected | Dominated on both axes, and structurally rather than by mistuning: its time floor, not its movement clause, sets its rate (1%, 5%, and 10% all yield 59 writes/active-hour at T=60 s), so cost tracks harness event cadence and would need per-harness constants. Misses 23% of warnings at 90% occupancy, 57% at 94%. |
| A coarser 5% bucket (2.6× cheaper) | Rejected | Perfect at multiples of five and worse than the movement guard between them — 51–79% of warnings missed at 91–94% — which couples the write policy to wherever a future reader sets its alarm. Cost is not binding anywhere in the candidate set, so buying that coupling away is the right trade. Named as the fallback if wire cost ever binds, at 2%. |
| Write every distinct reading | Rejected | Most real-time and still only 1/110 of the rate `DQ-H2` recorded as a failure — cost genuinely does not forbid it — but it is 3× the recommended policy for no measured accuracy gain, and it reinstates exactly the restatement pattern `DQ-H2` caught. |
| Write only at turn boundaries | Rejected | 48.8% p95 error, 92% of warnings missed. The wedge scenario is a single long turn, which this policy is silent through. |
| Name the driver records in the transport's include list, keeping them at the agent-directory root (q11) | Selected | The records matched none of the transport's globs and would silently never replicate, defeating the reason decision 0006 chose a catalog record at all. Naming them fixes the **already-shipped** `harness-state` too, with no migration of a live record, and keeps driver records out of a directory whose meaning is the Resource-binding realization surface. Cost: a cross-repository change, and an include list st2 does not own. |
| Move the record under `resources/` to match the existing globs | Rejected | Fixes only the new record and leaves `harness-state` unreplicated; puts driver runtime state on the Resource-binding realization surface; and contorts a record's location to fit a glob written for other purposes. |
| Chain the status-line tee to the operator's renderer (`DQ-C3`, captured) | Selected | Measured precedence is `.claude/settings.local.json` > `.claude/settings.json` > `~/.claude/settings.json`, and the winner *replaces* rather than merges — one slot, one command. Since the st2 materializer writes the winning file, not chaining would silently remove the operator's status line on every managed agent. |
| Doctor warns at a named st2 threshold, advisory-only (q10) | Selected | The record's whole purpose is that a human notices a filling window; a roster field nobody reads is not that. Advisory-only keeps it inside HC-A02 — Doctor already treats the categorical axis this way, and an exit-code failure would make an unfenced, advisory number gate a health check. |
| Doctor threshold set to each harness's own compaction point | Rejected | It would make the warning a prediction st2 cannot make: the point depends on harness, model, and operator settings (Codex's baseline plus a separate auto-compact buffer, pi's `reserveTokens`, omp's idle threshold, Claude's overridable window), and it would silently change meaning on a harness bump. |
| No Doctor output; roster only (the earlier `DQ-C4` position) | Rejected | Leaves the saturation signal to whoever thinks to read the roster, which is the invisibility this record exists to end. |
| New subsystem `08-harness-context/` with its own triple (q8) | Selected | A clean ontology boundary between the categorical and numeric axes, and a fresh requirements file rather than an edit to a protected one. `08` is the next unused prefix (`06` and `07` already carry duplicate prefixes). |
| Extend `05-harness-state` with `OHS-R16+` and amend decision 0006 | Rejected | One home for "what a driver observes about its harness", with the diagnostic record as precedent — but it would keep a subsystem named "harness state" carrying three records with three different guards, and needs an edit to a protected requirements file. |

## Evidence and Argument

**The guard is the discriminator, not the placement.** The spike built both
placements end to end and counted byte-distinct landed writes per simulated
Claude turn: 1 / 1 / 3 for the sibling at 0 / 5 / 15 tool calls, against 2 / 2 /
2 for a coalesced sub-object and 2 / 12 / 32 for a strict one, on a baseline of
2. What the measurement says is right — write when the number moved
meaningfully, at most once a minute otherwise, always on a compaction — needs a
prior-value comparison, an elapsed-since-reading check, and a move-fraction test
folded into `observe_inner`, the most invariant-dense function in that module,
on top of the byte-distinctness, ownership-sequence, and refresh-cadence logic
already there. The sibling expresses the same policy in a twenty-line guard with
nothing else in scope.

**The wedge case is exactly where the sub-object goes blind.** The state
record's reader routes stale, future-skew, unsupported-schema, literal-unknown,
claimed, unfenced, and session-dead through one `unknown` constructor that
builds a fresh value. A runtime at 190k of a 200k window whose state has gone
indeterminate would report no numbers at all — the one moment they matter. The
sibling has no `unknown` to derive: a stale reading is returned with its age.

**No single formula exists.** Claude computes
`(input + cache_creation + cache_read) / context_window_size` as a clamped
integer; Codex subtracts a hardcoded 12,000-token baseline from both numerator
and denominator of `last.totalTokens / modelContextWindow`; pi returns last
assistant `totalTokens` and omp — on a byte-identical API — returns last
assistant `input` alone; OpenCode publishes no percent at all and needs a
two-source join. Both harness constants are properties of a build, not of a
documented contract, which is why they inherit the omp version-gate discipline
and a per-harness pinned fixture (HC-R13).

**Two harnesses model honest ignorance already.** pi returns
`{tokens: null, percent: null}` after a compaction and holds it across a
restart; Claude's status line reports null percentages until the first API
response. Both map directly onto the discipline `OHS-R02` sets for the
categorical axis, and both are why HC-R03 forbids substituting a value for a
harness-declared null.

**The Claude window has exactly one source.** Hook payloads carry no token
fields, and the transcript carries per-message `usage` but no window size — so a
transcript-only producer would have to guess the denominator from a model table
that cannot distinguish a 200k tier from a 1M tier for one model id. The
`statusLine` settings slot carries `context_window_size`, `used_percentage`,
`cost.total_cost_usd`, `model.id`, and `rate_limits` in one payload, and there
is exactly one such slot — hence the tee, and hence the cross-repo contract with
dotfiles that owns it today (`DQ-C2`, `DQ-C3`).

**The sibling costs nothing at the watcher.** The delivery watcher is an
allowlist over the inbox subtree and the presence record, so a new sibling file
wakes no pump with no production change; only the invariant row's prose, which
enumerates siblings by name, moves.

## Decision

1. **A sibling record** `<agent-dir>/harness-context`, schema
   `st2.harness-context.v1`, its own lock, its own staleness and future-skew
   constants, sharing only an extracted lock-and-atomic-write helper with the
   state record (q4; replication per item 11).
2. **Harness-native vocabulary**: `usedTokens`, `windowTokens`, and a
   `usedPercent` that is the number the harness itself displays, discriminated
   by `harness`; a producer that cannot obtain a window withholds the percent
   rather than guessing (q2).
3. **Compaction as a counter**: `compactions`, `lastCompactionMs`, and
   `lastCompactionTrigger` over the closed union
   `manual | auto | threshold | overflow | idle | unknown`, with the count's
   scope (incarnation or harness-durable) stated per harness. History is
   deferred to a follow-up that answers it once for this axis and the
   categorical one together (q3).
4. **Stale readings are returned**, marked stale and carrying their age, and the
   numbers survive every `observedState: unknown` derivation (q4).
5. **A fourth roster axis** `context` in `st2 agents --json`, always emitted and
   `null` when absent, following the roster's existing null convention (q4).
6. **st2 owns the Claude `statusLine` slot** for driver-declared agents: a tee
   that records the payload and then execs the operator's downstream renderer;
   compaction edges keep coming from hooks, with `PreCompact` routed through the
   observe path in addition to the existing pre-compact stub (q5).
7. **Provenance-only fencing**: `incarnation` is carried and never consulted as
   a fence; the relaunch claim on the state record also removes the context
   record (q7).
8. **Advisory only** (q1), mirroring `OHS-A02`.
9. **Adjacent facts carried**: `model`, `costUsd`, `rateLimits`, and
   `sessionTotalTokens`, each documented as harness-reported (q9).
10. **Fixed quantization is the write policy** (q6, settled by benchmark): write
    on a change of `floor(used / (HARNESS_CONTEXT_BUCKET_PERCENT% of window))`,
    on any compaction edge, or when the record is older than
    `HARNESS_CONTEXT_HEARTBEAT` — 1% and 300 s, one constant pair for all five
    harnesses. The spike's movement guard is retired.
11. **The record is made transport-visible by naming, not by moving** (q11): it
    stays at `<agent-dir>/harness-context`, the transport's include list gains
    both driver records, st2 pins those names in a test, and staging files are
    written outside the replicated subtree so a temporary name never becomes a
    durable replicated key.
12. **Doctor warns, advisory-only** (q10), at or above
    `HARNESS_CONTEXT_WARN_PERCENT` (80) and on a stale record beside a
    `running` desired state, never changing its exit status. The threshold is
    st2's own attention number, documented as not being the harness's
    compaction point.
13. **VRS lives in [`08-harness-context/`](../08-harness-context/requirements.md)**
    (q8), with the roadmap recording the event-driven direction this design
    deliberately does not take yet.

## Consequences

- The transport's include list must gain both driver records before either
  replicates. That is a fleet-side change in another repository, so st2 can
  publish a record it believes remote readers see while they do not; nothing
  errors. st2's half is a test pinning the names it expects (HC-T08).
- The already-shipped `harness-state` record is fixed by the same change,
  without migrating a live record — which is why naming was chosen over moving.
  `DQ-C11` closes with it.
- st2 occupying Claude's status-line slot now carries a hard obligation rather
  than a courtesy: the slot is single-valued, the winner replaces rather than
  merges, and the st2 materializer writes the winning file, so a tee that does not chain removes
  the operator's status line on every managed agent (HC-R18). The inverse also
  holds — a human-set renderer in that file is not preserved by st2's merge —
  so an operator's renderer belongs in their own settings.
- A second per-agent record joins `status` and `harness-state`. Two invariant
  rows change wording when implementation lands: *Stable roster JSON* gains the
  fourth axis, and *Scoped delivery-input wakeups* gains `harness-context` in
  its enumeration of records that must never wake a pump.
- The pinned full-string roster assertions change deliberately, in the change
  that adds the axis, with the new proof named — the same discipline decision
  0006 applied to `observedState`.
- `usedPercent` is not comparable across harnesses as a physical quantity. Any
  consumer that aggregates it is aggregating operator views, and the ontology
  pins that.
- Codex's 12,000 baseline and omp's input-only semantics become version-coupled
  constants under the omp gate's discipline: a harness bump must re-verify them,
  and a per-harness fixture pinned to the measured version is what fails if they
  move.
- st2 becomes a live producer of a compaction count that tokenlens also derives
  retrospectively from transcripts. Two sources of truth for one quantity now
  exist; reconciling them is a tokenlens-side design, not an st2 record change.
- `rateLimits` under an agent-scoped record repeats across every runtime sharing
  an account. tokenlens remains the quota authority and nothing here reconciles
  the two.
- The Claude producer creates a cross-repo dependency: the st2 materializer owns the
  `statusLine` slot must not break the operator's own renderer, and the naming
  of that hand-off is unsettled (`DQ-C2`), as is the settings precedence the
  design assumes (`DQ-C3`).
- Bare *context* is already taken in st2's language for R09's working state. The
  wire key is `context` while the canonical term is *harness context record*;
  the ontology carries the collision rule.

## Amendment 1 — 2026-08-30: the degraded status-line arm is silent

The chaining obligation (q5, HC-R18) stands unchanged. What is withdrawn is the
*fallback* the original decision paired with it: that where no downstream
renderer resolves, the tee passes the payload through unchanged, "transparency
rather than silence", so the degraded case is still a status line.

That reasoning was wrong for this surface, and observably so. The status-line
payload is machine JSON, not prose — session id, transcript path, model and
usage blocks — and the slot repaints every five seconds. A seat that resolves no
renderer therefore renders a wall of
`{"session_id":…,"transcript_path":…}` in place of its status line: worse for
the operator than a blank row, and carrying nothing they can act on.
Transparency is the right default for a channel a human reads, and the original
argument applied it to a channel that carries a machine's serialization.

The failure was live rather than theoretical, and was observed on `dev3` on
2026-08-30. Neither resolution path was present there — the renderer file is managed by the dotfiles
generation and had not been switched, and the environment variable comes from
the login-shell session variables while seats launch from a user service — so
every managed Claude seat on that host displayed raw JSON as its status line.

Amended: where no renderer resolves, and wherever a resolved renderer fails, the
tee writes nothing to stdout and puts its diagnostic on stderr, which the
harness routes to its debug log and never to the rendered row. The diagnostic
names both resolution paths, because absent both is precisely the diagnosis. The
hook script's own outermost fallback follows the same rule, draining stdin so
the harness does not take an EPIPE at the refresh cadence.

Recording is untouched by any of this: the reading lands whether or not anything
is drawn, so the amendment trades no telemetry for the quieter line. Two
consequences follow. The *Status-line slot chaining* invariant row changes
wording, and its degraded proofs now assert an empty stdout with the stderr
diagnostic as their positive evidence — an empty stdout alone is also what a tee
that crashed instantly would leave. And a blank status line becomes a state an
operator can reach without an error anywhere; stderr is the only place that says
why, which is why the diagnostic is required rather than optional.
