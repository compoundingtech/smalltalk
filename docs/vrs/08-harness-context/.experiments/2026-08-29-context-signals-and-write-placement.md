# Context signals across five harnesses, and where the numbers should be written

## Question

Two questions, measured together on 2026-08-29 because neither answers alone:

1. **What does each maintained harness positively publish about its own context
   window and its compactions**, and by what arithmetic does it turn that into
   the number it shows its operator?
2. **Where should st2 keep those numbers** — folded into the existing
   `harness-state` record, or in a sibling record with its own guard — and what
   does either cost in writes, in code, and in fields that already mean
   something?

A third question rode along: does anything in the fleet already detect context
saturation, so that a new st2 record would be a second source of truth?

## Method

Five harness surfaces, four labs, one throwaway spike, and one read-only survey.
All work was done on this host; nothing was committed.

**Claude Code 2.1.250.** Live status-line capture with zero model turns: a
throwaway settings file whose `statusLine.command` wrote its stdin to a file,
run under `script` from an already-trusted worktree (a fresh scratch directory
is blocked by the folder-trust dialog). Hook and status-line payload schemas
were read out of the shipped bundle by grepping the bundled zod literals.
Transcript shape was read from 1,184 local transcripts under `~/.claude/projects`.

**codex-cli 0.150.1.** Protocol shapes from `codex app-server
generate-json-schema`; the arithmetic from the openai/codex source at tag
`rust-v0.150.1` (the packaged binary is a prebuilt tarball with no vendored
source); live cadence and payloads from a real rollout JSONL written that day.

**pi 0.84.2.** A credential-free lab: a local OpenAI-completions SSE server
emitting usage-bearing chunks, a `registerProvider` fake provider with a
4,000-token window, and a probe extension logging `ctx.getContextUsage()` and
the event payload at every lifecycle event. Compaction was forced by overflowing
the small window.

**omp 18.0.9 and 18.0.3.** The same lab, ported unchanged (`pi.registerProvider`
works on omp), run against both store paths; plus one real-credential
`omp -p "say hi"` turn against a 272,000-token window, and a byte-level scan of
both binaries.

**OpenCode 1.18.25.** A headless `opencode serve` under isolated XDG
directories with a free anonymous model, its `GET /event` SSE stream captured to
a file, `GET /doc` (472 schemas), and `POST /session/{id}/summarize` to force a
compaction.

**Placement spike.** A throwaway worktree in which both placements were built
end to end through the Claude hook path and their writes counted:
*Option A*, a `context` sub-object inside `harness-state` with a knob selecting
whether a token delta opens a transition; *Option B*, a sibling
`harness-context` record with its own lock, guard, and horizon. Both compile and
their tests pass; the sandbox carries 12–13 pre-existing unrelated failures
(pty/network) in the full suite, and `--lib` alone is green. Write counts are
byte-distinct landed writes per simulated Claude turn.

**Prior-art survey.** Read-only grep across tokenlens, the fleet's Grafana
dashboards and OTel conventions, the dotfiles Claude hook and status-line chain,
the fractal TUI, and st2's own consumers.

## Result

### 1. Every harness answers, and no two answer the same way

| Harness (version) | Numerator | Denominator | Percent as the harness shows it |
| --- | --- | --- | --- |
| Claude Code 2.1.250 | `input + cache_creation + cache_read` of the last response | `context_window_size` (status line only) | integer, clamped 0..100 |
| codex-cli 0.150.1 | `last.totalTokens` − 12,000 | `modelContextWindow` − 12,000 | rounded "% context left" |
| pi 0.84.2 | `getContextUsage().tokens` = last assistant `totalTokens` | `.contextWindow` | float; `null` right after compaction |
| omp 18.0.9 | `getContextUsage().tokens` = last assistant `input` | `.contextWindow` | float |
| OpenCode 1.18.25 | last non-summary assistant `tokens.total` | `GET /config/providers` `limit.context` | none shown |

Verbatim, from the live Claude status-line capture:

```json
"context_window": { "total_input_tokens": 0, "total_output_tokens": 0,
                    "context_window_size": 1000000,
                    "current_usage": null, "used_percentage": null,
                    "remaining_percentage": null },
"cost": { "total_cost_usd": 0, "total_duration_ms": 36418, ... },
"model": { "id": "claude-fable-5", "display_name": "Fable 5" },
"rate_limits": { "five_hour": { "used_percentage": 31, ... },
                 "seven_day": { "used_percentage": 55.0, ... } }
```

and the bundle's own arithmetic, verbatim:

```js
let r = e.input_tokens + e.cache_creation_input_tokens + e.cache_read_input_tokens,
    o = Math.round(r / t * 100), u = Math.min(100, Math.max(0, o));
return { used: u, remaining: 100 - u }
```

Codex's, from `protocol.rs` and the TUI crate (the same constant and the same
function body duplicated in both):

```rust
const BASELINE_TOKENS: i64 = 12000;   // "Includes prompts, tools and space to call compact."
let effective_window = context_window - BASELINE_TOKENS;
let used = (self.tokens_in_context_window() - BASELINE_TOKENS).max(0);
```

Recomputed against the measured rollout (window 258,400, `last.totalTokens`
92,283): 67% left, 33% used. A naive `last.inputTokens / window` gives ~36% —
close enough to pass review and wrong by construction.

**The measured traps, one per harness:**

- **Codex:** `total_token_usage.total_tokens` reached 2,235,329 against a
  258,400-token window in one session. It is lifetime spend; anything dividing
  it by the window reports >800%.
- **pi:** after a compaction `getContextUsage()` returns
  `{tokens: null, contextWindow: 4000, percent: null}` and stays null across a
  process restart until the next assistant usage arrives. pi positively says it
  does not know.
- **omp:** the same call, a different quantity. Against a fake provider
  reporting prompt tokens of 900 / 9,900 / 22,500, `tokens` returned exactly
  those figures — prompt-only input, where pi returns `totalTokens`.
- **OpenCode:** `summary` is truthy on user messages (an object `{diffs: []}`),
  so `if (m.summary)` counts every user turn; and after a compaction the newest
  assistant message is the summarizer's own, whose `tokens.total` was 1,511 —
  the cost of summarizing, not the new context size.
- **Claude:** `used_percentage` is `null` until the first API response, while
  the window is populated from the start.

Compaction edges, all captured or read from the shipped schema:

| Harness | Edge | Carries | Count source |
| --- | --- | --- | --- |
| Claude 2.1.250 | `PreCompact` / `PostCompact` hooks; `SessionStart source=compact` | `trigger: manual \| auto`; no sizes | st2 must count |
| Codex 0.150.1 | `contextCompaction` thread item (`thread/compacted` deprecated) | nothing | st2 must count |
| pi 0.84.2 | `session_compact` | `reason: manual \| threshold \| overflow`, `tokensBefore` | `sessionManager.getEntries()`, durable |
| omp 18.0.9 | `session_compact` | timestamp only, **no reason** | `sessionManager.getEntries()`, durable |
| OpenCode 1.18.25 | `session.compacted` | `{sessionID}` only | assistant messages with `summary === true` |

The Claude transcript's `compact_boundary` line does carry `preTokens` /
`postTokens`, but zero of 1,184 local transcripts contain one — the shape is
verified from the binary, not observed in this corpus.

### 2. Where the numbers should live: the guard decides, not the placement

Byte-distinct landed writes per simulated Claude turn:

| Tool calls in the turn | Today | A, coalesced | A, strict | B, sibling |
| --- | --- | --- | --- | --- |
| 0 | 2 | 2 | 2 | 1 |
| 5 | 2 | 2 | 12 | 1 |
| 15 | 2 | 2 | 32 | 3 |

Five findings, in the order they constrain the choice:

1. **A-strict corrupts two existing fields.** A transition resets `sinceMs`
   ("when the current state was entered") and increments `transitions`. Making
   every token delta a transition turns both into turn counters, so "idle for 40
   minutes" becomes unrecoverable. A-strict is not actually available.
2. **A-coalesced reports the number the turn started with.** Across 40 readings
   inside one turn the record still held the first, until the five-minute
   refresh — measured, not argued. Acceptable for "roughly how full is this
   agent", wrong for "warn me before it compacts".
3. **A's freshness needs a second stamp.** `heartbeat()` re-stamps the record
   without re-reading the source, so a context field inside it would look fresh
   while being arbitrarily old. The fix is a `context.observedAtMs` — which is
   B's separate horizon, moved inside A's record.
4. **A drops the numbers on every indeterminate read** — the seven `unknown`
   derivations (stale, session-dead, malformed, claimed, unfenced, future-skew,
   unsupported-schema) each build a fresh value that loses `context`. That is
   exactly the wedge case: an agent at 190k of 200k whose state has gone
   indeterminate reports nothing. It is a cost rather than a wall — seven return
   sites have the parsed record in hand and could thread the field through — but
   it is work A pays and B gets for free.
5. **The sibling is inert where it matters.** `src/watch.rs` is an allowlist
   (inbox subtree plus the presence record), so a new sibling file wakes no
   delivery pump with no production change, and the existing watcher test stays
   green untouched. Only the invariant row's prose, which enumerates siblings by
   name, would need editing.

Costs on the other side of the ledger, also measured: B is roughly 432
production lines against A's ~261 (~65% more), and adds one file per agent.
Both options pay the same unrelated cost of registering one new Claude hook
(`PreCompact`), which changes two expansion snapshots and the maintained example
declaration.

The discriminator is the guard, not the placement. `observe_inner`'s suppression
test is an equality check over a categorical tuple; the two policies it can
express are "write on no delta" and "write on every delta". What the measurement
says is right — write when the number moved meaningfully, and at most once a
minute otherwise — needs a prior-value comparison, an elapsed-since-reading
check, and a move-fraction test folded into the most invariant-dense function in
that module. The sibling expresses it in a twenty-line guard with nothing else
in scope.

### 3. Nobody else is watching, and the one adjacent owner is retrospective

A repo-wide survey found no context-saturation detection anywhere in the fleet.
The dotfiles Claude status-line renderer already parses
`context_window.used_percentage` and prints it to stdout without persisting
anything; the dotfiles hook chain maps `PreCompact` to a transient
`status=compacting` PTY event with no counter. tokenlens — the fleet's
registered token and cost producer — has a `compaction_count` column derived
retrospectively from transcripts and **no context-window column at all**, and
its own documentation lists context replay under future work. Its Grafana
dashboard's "Context Overhead Share" panel is a cache-waste heuristic, not
window fill. Nothing scrapes st2.

So this record is a first source, not a second — with one overlap to manage: a
live st2 compaction counter and tokenlens's transcript-derived one would be two
answers to one question.

### 4. Correction: omp 18.0.3 always had `getContextUsage`

The 18.0.3 capture in
[`06-omp-driver/.experiments/2026-08-25-omp-harness-integration.md`](../../06-omp-driver/.experiments/2026-08-25-omp-harness-integration.md)
records that the event-handler `ctx` exposes `{ui}` only. Running the 2026-08-29
probe unchanged against the 18.0.3 binary returns the identical `ctx` key set as
18.0.9 and a working call (`session_start {"tokens":2078,"contextWindow":4000,
"percent":51.95}`), and `getContextUsage` occurs 35 times in each of the two
binaries. The earlier enumeration missed it: a probe artifact, not a version
change. `signal` and `agent_settled` are genuinely absent from both, exactly as
that capture says.

## Conclusion

The five harnesses answer the occupancy question well enough to build on, and
badly enough that a single st2 formula is not available: the numerator differs
(last-response total, prompt-only input, an st2-computed join), the denominator
differs (raw window versus a window with a fixed 12,000-token baseline removed),
and two harnesses positively report "I do not know" where a naive producer would
report zero. Publishing each harness's own number, discriminated by the
`harness` field, is the only shape that agrees with what an operator sees.

For placement, the numeric axis wants a different write guard than the
categorical one, and the categorical record's guard cannot express it without
redefining `sinceMs` and `transitions`. The sibling record costs ~65% more
production code and one more file per agent, and buys a guard that fits the
axis, numbers that survive an indeterminate state, and an independent staleness
horizon.

Both harness-coupled constants — Codex's 12,000 baseline and omp's input-only
semantics — are properties of a build, not of an API contract, and belong under
the same version-gate discipline the omp driver already uses.

## VRS Impact

- Establishes [`08-harness-context/`](../requirements.md) and its producer table
  (HC-R11), the harness-native vocabulary (HC-A04, HC-R02), the withholding rule
  (HC-R03), and the compaction accounting table (HC-R12).
- Grounds the placement and guard decision recorded in
  [decision 0014](../../.decisions/0014-harness-context-is-a-sibling-numeric-record.md),
  including the write-amplification table and finding 5 (the delivery watcher's
  allowlist), which is why HC-R08 costs no production change.
- Sets the provisional constants in the spec's write guard and opens `DQ-C1`:
  the table above is one harness on one host, not a fleet benchmark.
- Supplies the version pins HC-R13's fixtures must assert: Claude Code 2.1.250,
  codex-cli 0.150.1, pi 0.84.2, omp 18.0.9, OpenCode 1.18.25.
- Resolves `DQ-C3`: the status-line precedence the Claude producer assumes is
  captured, and the consequence is ratified as HC-R18 (the tee must chain).
- Corrects the 18.0.3 `ctx` enumeration in
  [`06-omp-driver/.experiments/2026-08-25-omp-harness-integration.md`](../../06-omp-driver/.experiments/2026-08-25-omp-harness-integration.md),
  where the correction is appended. The same false claim also appears in
  [`2026-08-28-omp-18-0-9-admission.md`](../../06-omp-driver/.experiments/2026-08-28-omp-18-0-9-admission.md)
  (Result item 2) and in [`06-omp-driver/spec.md`](../../06-omp-driver/spec.md)
  (Channel section, "`ctx = {ui}`"); neither is edited here, because OMP-R05's
  admission checklist is that subsystem's to amend.

## Follow-up capture: status-line precedence, live (2026-08-29, second run)

The Claude producer section above left the settings precedence it depends on as
documented-but-uncaptured. It is now measured, and it turns a design risk into a
hard requirement.

**Method.** Four throwaway project directories, each driven through a real pty
that auto-accepts the folder-trust dialog, against Claude Code 2.1.250. **No
`--settings` flag**, so this measures real settings resolution rather than an
override. The host's `~/.claude/settings.json` declares a `statusLine` running
the operator's own renderer, which is what the control case proves would
otherwise render. No paid turns — the status line renders at startup.

| Case | Project files present | Which command ran | Global ran? |
| --- | --- | --- | --- |
| local | `.claude/settings.local.json` only | the project's | no |
| project | `.claude/settings.json` only | the project's | no |
| both | both files | `settings.local.json`'s — the other marker was never written | no |
| none (control) | none | the operator's global renderer | yes |

**Result.** Precedence is
`.claude/settings.local.json` > `.claude/settings.json` >
`~/.claude/settings.json`. The control establishes these are genuine overrides
rather than a missing global. The `both` case establishes the winner
**replaces** the loser outright: only one command runs per render, and the
losing case's marker file was never written. It is a single slot, not a merge or
a chain.

**Consequence.** `.claude/settings.local.json` is exactly the file the st2 materializer writes
for driver-declared agents, so an st2 `statusLine` entry there wins
unconditionally. Without chaining it silently removes the operator's status line
on every managed agent — on this host, one that already displays agent id,
model, context fill, and cost. This is why chaining is specified as a
requirement (HC-R18) rather than left to the tee's implementation.

The inverse is not solved by chaining: a `statusLine` a human sets in a managed
agent's `.claude/settings.local.json` is overwritten by st2's own
materialization of that file, whose merge owns only st2's hook entries. An
operator's renderer belongs in their own settings, with st2's tee chaining to
it.

**Limit.** `--settings <file>` also wins and was not compared against the
project files here; it is not how a managed agent runs.

## Superseded in part (2026-08-29)

Two of this record's conclusions were overtaken the same day by a replay
benchmark over 90 real sessions,
[`2026-08-29-write-policy-benchmark.md`](./2026-08-29-write-policy-benchmark.md).
The harness-signal survey above (the producer arithmetic, the traps, the
compaction edges, the omp correction) stands unchanged and is still the
grounding for the producer table. What no longer holds:

- **The movement guard is retired.** This file's write-amplification table
  measured one synthetic Claude turn and concluded the spike's guard — skip
  identical, write on a ≥5% move, otherwise at most once per 60 s — was the
  right policy. Replayed against real sessions it misses 23% of
  high-occupancy warnings at a 90% threshold and 57% at 94%, and its cost is set
  by its time floor rather than by its movement clause, so it tracks harness
  event cadence instead of agent behavior. The spec now specifies fixed 1%
  quantization plus a compaction edge and a 300 s heartbeat. The reasoning that
  survives is the part about the *categorical* record's guard: `observe_inner`
  still cannot express a numeric write policy, which is why the record is a
  sibling.
- **The record does not replicate at the path this file recommends** — nor does
  the shipped `harness-state` — because the replication transport's include list
  named only `**/resources/**` and `**/status` under an agent directory. The
  resolution keeps both records where they are and names them in that list
  instead, so finding 5 above (the delivery watcher's allowlist makes the record
  inert with no production change) holds exactly as tested here.
- **The staging file moved out of the agent directory.** The spike's sibling
  `tmp`-plus-rename is forbidden inside a replicated subtree, where the temporary
  name becomes a durable replicated key.
