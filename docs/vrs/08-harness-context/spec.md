# Harness context specification

This document specifies the harness-context record, its guard, its per-harness
producers, and its roster exposure. It builds on
[requirements.md](./requirements.md). The categorical observation axis remains
in [`05-harness-state/spec.md`](../05-harness-state/spec.md) and reads none of
this; declared presence remains in the root spec's R08 section.

## Status

Partially implemented. The core is in the tree as of 2026-08-29: the record and
its schema, the quantized write guard, the reading projection, the roster's
fourth axis, the session-boundary removal, the Doctor advisory, and the pinned
replicated-record names — `src/harness_context.rs`, with the invariant rows
*Harness context discipline* and *Replicated-path discipline* naming their
proofs.

**All five producers ship: claude, codex, pi, omp, and opencode.** The
status-line tee (HC-R18), the Claude fill arithmetic, and its compaction
accounting are in the tree as of 2026-08-29 — `hooks/claude-statusline.sh`,
`st2 driver claude-statusline`, and the `PreCompact`/`PostCompact`
registrations, with the invariant row *Status-line slot chaining* naming their
proofs. The codex app-server pump writes the record from
`thread/tokenUsage/updated` and counts compactions off the `contextCompaction`
thread item — `src/codex_app_server.rs`. The pi and omp rows share one extension
path in `hooks/pi-channel.ts` and `hooks/omp-channel.ts`, consumed by
`src/pi_channel.rs`. The opencode row joins SSE `message.updated` with
`GET /config/providers` in `src/opencode_session.rs`. Each producer writes
beside the harness-state writer its wrapper already owned and under the same
incarnation, and every row is version-pinned by an HC-R13 fixture over a
verbatim capture, so HC-R11 is met: a declaration on any of the five harnesses
publishes a non-`null` `context`.

The placement, guard, and Claude producer were first built as a throwaway spike
([`.experiments/2026-08-29-context-signals-and-write-placement.md`](./.experiments/2026-08-29-context-signals-and-write-placement.md)),
and the spike's tree is not the shipping tree. The write policy is no longer
provisional: it was chosen on a replay benchmark over 90 real sessions
([`.experiments/2026-08-29-write-policy-benchmark.md`](./.experiments/2026-08-29-write-policy-benchmark.md)),
which also moved the record's path. The Claude producer's status-line renderer
contract with dotfiles closed on 2026-08-29 (`DQ-C2`, dotfiles PR #2160). The
one residual that remains is the transport half of `DQ-C1`, unmeasurable until a
transport runs. Open questions are tracked in
[open-questions.md](./open-questions.md); the direction this design deliberately
does not take yet is in [roadmap.md](./roadmap.md).

The `agent` field's immutable-ID meaning is the accepted target, and it ships
as a new record version, `st2.harness-context.v2`: the version suffix is the
read contract, so changing an existing field's meaning reserves the next
version rather than reusing `v1`. Records and producers stay on
`st2.harness-context.v1`, whose `agent` remains a bus identity, until
[DELTA-003](../.delta/DELTA-003-agent-address-not-implemented.md) closes.
Version 2 is otherwise identical to the shape below, and the reader-first
rollout accepts both versions before any version-2 writer activates.

## Scope

This specification defines the harness-context record, its freshness and
staleness projection, its write guard, the five per-harness producers and their
version-coupled arithmetic, compaction accounting, session-boundary behavior,
and the roster exposure. It does not define: the categorical observation axis or
its fencing ([`05-harness-state`](../05-harness-state/spec.md)); per-compaction
history or a context event stream (deferred, HC-T02, `DQ-C6`); idle or
idle thresholds, escalation, or
notification policy beyond Doctor's advisory line (root `R20`); the fleet
metrics feed (`06-observability`, roadmap); and any
supervisor action taken on a number (HC-A02, `DQ-C8`).

## Overview

```text
 claude statusLine     codex app-server        pi extension      omp extension    opencode
 stdin JSON            thread/tokenUsage/      getContextUsage() getContextUsage() message.updated
 (+ PreCompact/        updated                 + session_compact + session_compact + /config/providers
  PostCompact hooks)   + contextCompaction                                        + session.compacted
        |                     |                      |                 |                |
        v                     v                      v                 v                v
   [driver-owned, harness-native arithmetic: usedTokens / windowTokens / usedPercent]
        |
        v  write when: bucket changed | compaction | record older than heartbeat
        |
   <agent-dir>/harness-context          <agent-dir>/harness-state      <agent-dir>/status
   st2.harness-context.v1               st2.harness-state.v1           presence, agent-authored
   numeric, quantized,                  categorical, transition-       declared
   unfenced, 60-min horizon             guarded, fenced
        |                                        |                              |
        +----------------+-----------------------+------------------------------+
                         v
          st2 agents --json  (status | observedState | driverDiagnostic | context)
```

The three records are independent axes on one agent directory. The context
record's numbers stay readable when the state record derives `unknown`
(HC-R07), and the state record's fencing does not extend to them (HC-T04).

## Schema identifiers

`st2.harness-context.v1` follows the repository-local convention the presence,
harness-state, and driver-diagnostic records already use: `st2.<record-kind>.vN`,
lowercase, dot-separated, where `<record-kind>` is the record's file name. The
namespace is st2's own and is not registered anywhere; st2 core is its steward.
The version suffix is the read contract, not the field set: additive fields do
not bump it, and a reader that finds any other schema string refuses the record
rather than guessing at fields spelled like this version's. Examples: valid
`st2.harness-context.v1`; the reserved immutable-ID shape
`st2.harness-context.v2`, which tolerant readers accept before any producer
emits it; foreign and therefore refused `com.example.harness-context.v1`;
invalid (no version) `st2.harness-context`.

## Record (HC-R01, HC-R04)

One JSON object at `<agent-dir>/harness-context`, beside `harness-state`,
written under its own lock `.harness-context.lock`, newline-terminated, and
replaced atomically by a rename from a staging file held **outside** the
replicated subtree (see
[Placement and transport](#placement-and-transport-hc-r05)):

```json
{
  "schema": "st2.harness-context.v1",
  "agent": "<identity in v1; agent ID in v2>",
  "harness": "claude | codex | pi | omp | opencode",
  "usedTokens": 92283,
  "windowTokens": 258400,
  "usedPercent": 33,
  "model": null,
  "costUsd": null,
  "sessionTotalTokens": 2235329,
  "rateLimits": { "fiveHour": 31, "sevenDay": 55 },
  "compactions": 3,
  "lastCompactionMs": 1788000097290,
  "lastCompactionTrigger": "manual | auto | threshold | overflow | idle | unknown",
  "incarnation": "<the writing session's token>",
  "observedAtMs": 1788000100000,
  "writtenAtMs": 1788000100000
}
```

Field rules:

- `harness` is the discriminator for the arithmetic. There is deliberately no
  second `semantics` field: one discriminator for one semantic axis, and a
  reader that knows the harness knows which producer row of the table below
  produced the number. A `harness` this reader does not recognize makes the
  numbers uninterpretable, so the record reads as absent.
- `usedTokens`, `windowTokens`, and `usedPercent` are independently optional and
  each is `null` when withheld (HC-R02, HC-R03). `usedPercent` present with
  `windowTokens` absent is legal — Claude publishes its own integer percent
  alongside a window it may not yet have populated — but `usedPercent` is never
  computed by st2 from a window st2 guessed.
- `usedPercent` may exceed 100, and readers must handle that rather than assume
  a 0..100 range. pi and omp report a float that runs above 100 when a turn
  overruns the window (585.6% measured in the pi lab against a 4,000-token
  window). Claude clamps to 0..100 itself. Codex's *operands* are not clamped —
  2 of 287,010 replayed Codex readings give `used / window` up to 1.041 — but
  the shipped Codex producer mirrors Codex's own function rather than dividing
  the operands, and that function floors `remaining` at zero, so those readings
  reach the record as a saturated 100 rather than as 104. The overrun is lost
  inside the harness's arithmetic, before st2 sees it; the harnesses that can
  publish above 100 are the ones publishing a float of their own. **The record
  carries the raw value and never clamps it**: a reading above 100 is a real
  observation of an overrun, and clamping at the producer would hide exactly the saturation this
  record exists to show. Clamping is a *display* concern — a consumer rendering
  a bar or a percentage clamps for its own layout, and does so knowing it is
  discarding information the record deliberately kept.
- `sessionTotalTokens` is cumulative lifetime spend for the session and is
  **never** occupancy. It is named for that distinction (HC-R16): a measured
  Codex session read 2,235,329 cumulative against a 258,400-token window, so a
  consumer dividing it by `windowTokens` reports >800%.
- `costUsd` is the harness-reported cost, in the harness's own accounting and at
  whatever scope that harness reports it, and no st2 path recomputes or
  reconciles it. Claude and OpenCode report a session total; pi and omp report a
  per-message figure and the record carries the last assistant message's — see
  the producer table and
  [`DELTA-005`](../.delta/DELTA-005-harness-context-cost-is-per-message-on-pi-and-omp.md),
  which records that HC-R16 still says "session cost" and needs widening. Codex
  reports none; the field is `null` there. A consumer comparing this field across
  harnesses must read the `harness` discriminator first.
- `rateLimits` is harness-reported and account-scoped (HC-T06). It repeats
  across every agent runtime sharing an account. Absent windows are `null`.
- `lastCompactionTrigger` is a closed union, additive-tolerant on read: an
  unrecognized future word decodes as `unknown`, never as a definite trigger.
  `unknown` is a legitimate v1 value for three of the five harnesses.
- `incarnation` is the writing session's token, carried as provenance only
  (HC-R15). Nothing refuses a write on it, no sequence accompanies it, and there
  is no floor sidecar. A reader may use it to tell "this number came from the
  session currently running" from "this number predates it"; that is its whole
  purpose.
- `observedAtMs` is when the reading was taken, not when the file was written,
  and is never re-stamped without a new reading behind it. A producer holding no
  fresh reading does not write at all, so the record ages rather than looking
  refreshed — see the heartbeat rule in the write policy.
- `writtenAtMs` is when the record was written, and is strictly monotonic per
  record. **Added during implementation** (2026-08-29), additively and therefore
  without a version bump. The two stamps answer different questions and
  collapsing them is wrong in a way that shows up immediately: the write
  policy's heartbeat clause asks how old the *record* is, while `ageMs` asks how
  old the *reading* is, so a producer that writes at T carrying a reading taken
  at T−4min would look four minutes stale against a five-minute heartbeat and
  re-publish almost at once. Monotonicity is also what keeps HC-R04's
  byte-distinctness intact against a same-millisecond predecessor. Readers
  derive nothing from it: `ageMs` and `stale` come from `observedAtMs` alone.
- `usedPercent` and the other fractional fields are JSON numbers, so an integral
  reading may serialize as `33.0` rather than `33`. That is the cost of carrying
  pi's and omp's floats (585.6 was measured) in one field, and it is
  numerically identical to a consumer.
- Deserialization is additive-tolerant (no strict unknown-field rejection).

Constants, deliberately not aliases of the presence or harness-state constants
(HC-R04):

| Constant | Value | Meaning |
| --- | --- | --- |
| `HARNESS_CONTEXT_STALE` | 60 min | past this age a reading is returned marked `stale` |
| `HARNESS_CONTEXT_FUTURE_SKEW` | 60 s | beyond this the clock is untrusted and the record reads absent |
| `HARNESS_CONTEXT_BUCKET_PERCENT` | 1 | window fraction that defines a write bucket |
| `HARNESS_CONTEXT_WARN_PERCENT` | 80 | at or above this reading Doctor emits an advisory; st2's own attention threshold, not a compaction prediction |
| `HARNESS_CONTEXT_HEARTBEAT` | 300 s | maximum interval between writes while a reading is available; deliberately equal to `HARNESS_STATE_REFRESH`, so this record never re-stamps more often than the state record beside it |

The 60-minute horizon is four times harness-state's 15-minute one on purpose: a
categorical state that is an hour old is a dangerous claim about what an agent
is doing right now, while a token count that is an hour old is a still-useful
lower bound on how full a window has become.

## Reading (HC-R06, HC-R07)

What a reader projects, in evaluation order:

| Evidence | Projects as | Reason |
| --- | --- | --- |
| No record file | `null` | never observed |
| Unreadable, unparseable, or a `schema` outside the accepted versions | `null`, warning logged | nothing trustworthy to report; `DQ-C5` asks whether this deserves an explicit `indeterminate` the way `driverDiagnostic` has one |
| Unrecognized `harness` | `null`, warning logged | the arithmetic is unknown, so the numbers have no meaning |
| `observedAtMs` > now + `HARNESS_CONTEXT_FUTURE_SKEW` | `null` | a clock this wrong makes the derived age meaningless |
| `observedAtMs` ≤ now − `HARNESS_CONTEXT_STALE` | the reading, `stale: true`, with `ageMs` | HC-R06: returned with its age, never derived away |
| Otherwise | the reading, `stale: false`, with `ageMs` | — |

The accepted versions are `st2.harness-context.v1` and, once tolerant readers
ship, `st2.harness-context.v2`; any other schema string is refused.

`ageMs` is derived by the reader from `observedAtMs`; no read path consults file
mtime. There is no `unknown` vocabulary on this axis and no path from any
absence to a fabricated number.

The projection is computed independently of the harness-state record. It is not
consulted by, and does not consult, the state derivation: an agent whose
`observedState` reads `unknown` for any of that record's nine reasons still
reports its last context reading with the age it has (HC-R07).

## Write policy (HC-R09, HC-R10)

```text
reading arrives
   |
   +-- compaction edge?                                  -> write
   +-- floor(used / (BUCKET_PERCENT% of window)) changed
   |     since the last written reading?                  -> write
   +-- proven Claude account-window exhaustion changed?   -> write
   +-- record older than HARNESS_CONTEXT_HEARTBEAT?       -> write
   +-- otherwise                                          -> skip
```

Fixed quantization at 1% of the window — 100 buckets — plus a compaction edge,
a bounded Claude exhaustion/reset edge, and a 300-second heartbeat. Codex
account-window occupancy does not classify availability because Codex can
continue through credits and this record does not carry the credit metadata
needed to prove otherwise. The heartbeat fires only when the producer holds a
reading taken since the last write: it re-publishes a *fresh* reading whose
bucket happens not to have changed, and never re-stamps a stale one. A producer
with no new reading writes nothing at all, and the record ages visibly through
`ageMs` instead. This is what keeps HC-R04's byte-distinctness rule intact — a
write that changed nothing would carry no refresh across a transport that
compares content — and it is why the constant is described as the maximum
interval between writes *while a reading is available*. The policy was chosen on a replay benchmark over 45
Codex rollouts (median span 30.6 h, 4,220 readings each, real in-protocol
windows) and 45 Claude transcripts
([`.experiments/2026-08-29-write-policy-benchmark.md`](./.experiments/2026-08-29-write-policy-benchmark.md)).

### Why not a movement guard with a time floor

The spike's candidate — skip identical, write on a ≥5% move, otherwise at most
once per 60 s — is dominated, and the reason is structural rather than a matter
of tuning its constants:

| Policy | Codex writes/h/agent | error p95 (% of window) | missed warnings @90% |
| --- | --- | --- | --- |
| every distinct reading | 100.7 | 0.00 | 0% |
| turn boundary only | 1.8 | 48.76 | 92.1% |
| movement guard 5% / 60 s (the spike) | 19.2 | 3.43 | 23.1% |
| movement guard 1% / 60 s | 32.1 | 0.82 | 2.8% |
| bucket 5% + 300 s heartbeat | 13.1 | 3.80 | 0% |
| **bucket 1% + 300 s heartbeat** | **34.1** | **0.76** | **0%** |

"Missed warnings" is the share of real compaction events where occupancy truly
crossed the threshold but the last written reading did not — the "warn me before
this agent compacts" metric this record exists for, measured over 821 eligible
Codex events at the 90% threshold.

Three results from that benchmark decide the shape:

1. **A time floor makes cost track producer chattiness, not the agent.** On
   Claude, the movement guard at 1%, 5%, and 10% with a 60-second floor all land
   at 59 writes per active hour — 3600/60. The movement clause is nearly inert
   and the policy degenerates to "one write per minute while active". Because
   Codex emits roughly 25× more readings per hour than Claude, any time-floored
   policy needs per-harness tuning. Quantization does not: writes per window fill
   are capped at `100 / BUCKET_PERCENT` regardless of how often a producer
   speaks, which is precisely why **one constant is correct for all five
   harnesses and there is no per-harness knob** (HC-R09).
2. **Cost does not discriminate.** `DQ-H2`'s recorded failure was 679 writes in
   221 s — 3.07 writes/s on one agent, 11,061 writes/hour/agent. Every candidate
   above is 100–1,000× below that, including "write every distinct reading" at
   1/110 of the failure rate. The recommended policy is 1/324 of it: 34.1
   writes/h/agent × 600 agents = 5.7 writes/s fleet-wide. Cost is not binding
   anywhere in the candidate set, so the choice is made on accuracy and
   structure.
3. **A coarse bucket couples the write policy to the reader's alarm** (HC-R10).
   A bucket write fires on *entry* to a bucket, so the reader always shares the
   truth's bucket — an alarm sitting on a bucket boundary can never be missed,
   one between boundaries can. Bucket 5% is perfect at 90% and 95% and *worse
   than the movement guard* in between: 51%, 70%, 74%, and 79% of warnings
   missed at thresholds of 91, 92, 93, and 94% (n = 672, 590, 436, 289). Bucket
   1% is the only measured policy with zero missed warnings at every threshold
   from 80% to 97%, which is what makes the write policy independent of whatever
   a future consumer decides to alarm on.

Turn-boundary writing is disqualified outright: 48.8% p95 error and 92% of
warnings missed, because the wedge scenario is a single long turn.

### What would change this

- **If the reader stopped surfacing `ageMs` and `stale`**, quantization's
  unbounded lag would become silent instead of visible, and the time floor would
  do real work again; the fallback is the movement guard at 1% / 60 s (32.1
  writes/h/agent, 1–22% missed). HC-T05 states this dependency.
- **If per-sync wire cost turns out to be large** — unmeasured, and the open
  half of `DQ-C1` — the next stop is a 2% bucket at 23.3 writes/h/agent, with
  the consequence that alarms must then sit on even percentages.
- **Occupancy above the window caps the guarantee.** `100 / BUCKET_PERCENT` is a
  write cap only up to the first overflow; above 100% the bucket index keeps
  climbing.

## Placement and transport (HC-R05)

The record lives at `<agent-dir>/harness-context`, beside `harness-state`. The
driver records belong together at the agent-directory root; what makes them
replicate is the transport's include list naming them, not their location.

The measurement that raised the question stands: the fleet's replication
transport syncs an agent directory through an include list that named only
`**/resources/**` and `**/status`, so a driver record at the agent-directory
root matched nothing and would silently never reach a remote reader — no error,
just a record no remote consumer ever sees. That defeats the reason a catalog
record was chosen over host-local state
([decision 0006](../.decisions/0006-observed-harness-state-is-a-driver-written-catalog-record.md):
"remote supervisors and the TUI read through the catalog they already sync").

The resolution is to name the driver records in that list —
`**/harness-state,**/harness-context` — rather than to move them under a
directory whose meaning is the realization surface for Resource bindings. The
include list is a fleet-side trait specification —
`context/fleet/traits/fabric/spec.md` in the dotfiles repository — so this is a
cross-repository change, and it has one property worth stating: it fixes the **already-shipped**
`harness-state` record, which has the same defect today, without migrating a
live record.

Two obligations follow on st2's side:

- **Pin the names.** st2 asserts, in a test, the exact record names it expects
  the transport to carry. The include list lives in another repository, so a
  rename here would otherwise stop replication silently and nothing in this
  repository would notice (HC-T08). The test does not prove the other side
  carries them; it makes st2's half of the contract explicit and breakable.
- **Stage outside the replicated subtree.** A sibling temporary name inside it
  is propagated as a real key and restored after a later local delete, so the
  atomic-write helper stages on the same filesystem but outside the sync root
  and renames only the canonical path in. Measured free: p50 0.217 ms staged
  outside versus 0.222 ms as a sibling.

  **Where:** `<catalog>/.st2/harness-context-staging`, derived only from the
  exact canonical `<catalog>/agents/<host>/<identity>` ancestry. Every ancestry
  component and the staging directory must be a real directory, and the staging
  and agent directories must report the same filesystem device; otherwise the
  writer fails rather than searching upward, following a symlink, or degrading
  atomic publication to a copy.

  Earlier writers staged at `<catalog>/agents/<host>` and could leave
  `.harness-context.tmp-<numeric-pid>-<numeric-counter>` behind after a crash.
  Current-catalog identity walkers overlook only an exact legacy name that is a
  regular non-symlink file, and leave it untouched for a possibly-live old
  writer. Directories, symlinks, special files, generic dotfiles, near misses,
  and prepared-catalog topology remain strict.

  `harness-state` keeps staging beside itself for now. The two records share the
  extracted `write_json_atomic` helper, which takes the staging directory as an
  argument precisely because they answer this question differently; moving the
  shipped record's staging is a separate change with its own risk.

The delivery watcher is unaffected (HC-R08): its allowlist is
`starts_with(<agent-dir>/resources/inbox)` or the exact path
`<agent-dir>/status`, and the record is a sibling of neither, so no pump wakes
and no production change is needed.

No correctness property here depends on the transport. Its adoption record is
open and forbids exactly that, so replication is what is gained when it arrives,
never what this subsystem needs to work.

## Producers (HC-R02, HC-R11, HC-R13)

Every live row was measured on 2026-08-29. The Codex payload was captured on
0.150.1 and its version-coupled arithmetic and notification shape were reverified
against 0.151.0 before admission. `usedPercent` is that harness's own displayed
number (HC-R02); where st2 computes it, the row says so.

| Harness | Version verified | Channel | `usedTokens` | `windowTokens` | `usedPercent` |
| --- | --- | --- | --- | --- | --- |
| claude | 2.1.250 | `statusLine` command stdin JSON | `context_window.total_input_tokens` = `input + cache_creation + cache_read` of the last response | `context_window.context_window_size` | `context_window.used_percentage` — Claude's own integer, clamped 0..100 |
| codex | codex-cli 0.151.0 | app-server `thread/tokenUsage/updated` | `tokenUsage.last.totalTokens` | `tokenUsage.modelContextWindow` | st2 computes with the baseline rule below; equals `100 −` Codex's displayed "% context left" |
| pi | 0.84.2 | injected extension `ctx.getContextUsage()` | `.tokens` = last assistant `totalTokens` (input + output + cacheRead + cacheWrite) | `.contextWindow` | `.percent` (float) |
| omp | 18.0.9 (and 18.0.3) | injected extension `ctx.getContextUsage()` | `.tokens` = last assistant **`input`** only | `.contextWindow` | `.percent` (float) |
| opencode | 1.18.25 | SSE `message.updated` joined with `GET /config/providers` | last **non-summary** assistant `tokens.total` | `providers[].models[<modelID>].limit.context` | st2 computes `usedTokens / windowTokens`; the server displays none |

Which adjacent facts each channel actually supplies (HC-R16) — everything not
listed is `null`, and no producer computes a fact its channel does not carry:

| Harness | `model` | `costUsd` | `rateLimits` | `sessionTotalTokens` |
| --- | --- | --- | --- | --- |
| claude | `model.id` | `cost.total_cost_usd` | `rate_limits.{five_hour,seven_day}` | `null` — the payload's `total_*` keys describe the last response, not the session |
| codex | `null` — the thread carries `modelProvider` only | `null` — Codex reports no cost | `account/rateLimits/updated`, `sevenDay` only — see below | `tokenUsage.total.totalTokens` |
| pi | `ctx.model.id` | per-message `usage.cost.total` | `null` | `null` in v1 — only obtainable by summing every message's usage, which is a producer-side accumulator, not a free reading |
| omp | `ctx.model.id` | per-message `usage.cost.total` | `null` | `null` in v1 — same reason as pi |
| opencode | `session.info.model`, written `providerID/modelID` | `session.info.cost` | `null` | `session.info.tokens` (cumulative, no `total` key — summed by the producer) |

`sessionTotalTokens` is carried only where the channel already computes it. A
producer that would have to accumulate it itself writes `null` rather than
maintaining a second running total whose correctness depends on having seen
every message — tokenlens owns lifetime accounting, and a half-observed sum
would be a worse answer than none.

### claude (2.1.250)

The status-line payload is the only channel that carries a window. Hook payloads
carry no token fields at all, and the transcript carries per-message `usage` but
no window size — so a transcript-only producer would have to invent the
denominator from a model table, which cannot distinguish a 200k from a 1M tier
for one model id, and would violate HC-R02.

st2 owns the `statusLine` slot for driver-declared agents: the rendered
`.claude/settings.local.json` names a tee that records the stdin JSON to the
harness-context record and then execs the operator's own downstream renderer, so
a human's status line keeps working:

```text
.claude/settings.local.json (st2-rendered)
  statusLine: { type: "command", command: "\"$ST_HOOKS/claude-statusline.sh\"",
                padding: 0, refreshInterval: 5 }

tee:  stdin JSON --> st2 driver claude-statusline (writes harness-context)
                 --> run the downstream renderer, resolved in order:
                       1. $ST_CLAUDE_STATUSLINE_RENDERER
                       2. ~/.claude/statusline-renderer.json  -> .command
                 --> if neither resolves, or the renderer fails:
                       stdout stays empty; the reason goes to stderr
```

**The subcommand, decided during implementation (2026-08-29):
`st2 driver claude-statusline`,** a sibling of `claude-observe` rather than an
event on it. An earlier draft of the diagram above named `claude-observe`, which
reads well and is wrong in one specific way: that command's contract is "apply
one Claude HOOK EVENT to observed harness state", and the status line is not a
hook — `StatusLine` is a settings key and is deliberately absent from Claude's
registerable event list. Routing it through the hook command would have dragged
`observe_hook_event`'s event classification and the `SessionStart` claim onto a
payload that is neither.

**The tee builds no telemetry pipeline (`DQ-C13`, resolved).** A 5-second refresh
interval makes it ~720 short-lived `st2` invocations per hour per seat — the
highest-cadence process st2 has, where every other one is long-lived or fires on
an event — and Claude waits for each to exit. `main` therefore constructs
`Telemetry::local_only()` for this subcommand: stderr diagnostics, no exporters,
no global providers, and nothing to flush at exit. Measured against a
bound-but-never-accepting collector, the alternative is not theoretical: a tee
that initializes telemetry takes 5.009 s on the path that logs a warning, and
`claude-observe` takes 10.022 s, against 0.010–0.061 s with no pipeline. An
unreachable collector would have stalled every render past Claude's own refresh
interval.

The rule is **cadence, not hook class**, and it lives in
[`06-observability/spec.md`](../06-observability/spec.md): `claude-observe` is
event-driven, is named there as `st2-hook`, and stays instrumented. The tee also
records no `hook_invocations_total` — a metric call that provably cannot reach a
collector reads as instrumentation and is worse than none.

The tee spawns the renderer and writes the payload to its stdin rather than
`exec`ing it, because st2 has already consumed that stdin to record the reading.
The renderer is run as a shell command line, exactly as Claude runs its own
`statusLine.command`. Every renderer failure is the same failure: one that
cannot be *started* and one that started and exited non-zero both leave stdout
untouched. The second could not do otherwise — it may already have written a
partial line, and appending the raw JSON would corrupt the status line rather
than restore it — and the first matches it so that a permissions bug on the
renderer file cannot spew JSON where a missing renderer would not. All three settings
keys are carried: `padding: 0`, and `refreshInterval: 5`, which is what makes the
record a live reading rather than one frozen between turns and is well inside the
300-second write heartbeat.

**Renderer resolution, settled** (`DQ-C2`, closed 2026-08-29 by dotfiles
PR #2160). Two sources in strict order, first hit wins:

1. `$ST_CLAUDE_STATUSLINE_RENDERER` — an environment variable holding the
   command. It comes first so a single agent, a debugging session, or a test can
   override without editing a file.
2. `~/.claude/statusline-renderer.json` — a file the operator owns, schema
   `dotfiles.claude-statusline-renderer.v1`, carrying `{"command": …}`. It is a
   file rather than a settings key because the settings file st2 wins in is the
   one st2 rewrites, so a renderer declared there would be the very thing the
   merge does not preserve (HC-R18's inverse). A user-level file st2 never
   writes has no such hazard.
3. Neither resolves: write nothing to stdout, and name both paths in a
   diagnostic on stderr.

The environment variable is checked first and the file second — not merged, and
never both — so the resolution has one answer and an operator debugging their
status line has one place to look for it. The file's schema string follows the
dotfiles namespace rather than st2's, because dotfiles owns the file and its
shape; st2 reads `command` and nothing else.

The Claude producer implements this. Resolution reads `command` from the file
and nothing else, so a future key dotfiles adds is inert here rather than a
second source of truth; an absent `$HOME`, an unreadable file, an unparseable
one, or an empty `command` each fall through to the next source.

**Chaining is mandatory, not a courtesy** (HC-R18). When no downstream renderer
resolves — and whenever a resolved renderer fails — the tee writes nothing to
stdout, so the worst case of st2 occupying the slot is an empty status line. The
fallback is silence rather than transparency because the payload is machine
JSON: a slot echoing it repaints
`{"session_id":…,"transcript_path":…,"model":{…}}` across the operator's
terminal every five seconds, which is worse for them than a blank row and
carries nothing actionable. Transparency is the right default for a channel a
human reads; the status-line slot is not one. The reason goes to stderr instead,
which the harness routes to its debug log and never to the rendered row, so a
blank line is still diagnosable — the diagnostic names
`$ST_CLAUDE_STATUSLINE_RENDERER` and `~/.claude/statusline-renderer.json`
explicitly, since a seat where neither resolves is exactly the case that
produces it. Recording is untouched by which arm runs: the reading lands whether
or not anything is drawn. The same rule governs the hook script's own outermost
fallback, where no identity, no catalog root, or no `st2` on `PATH` drains stdin
and prints nothing — it drains rather than exiting, because the harness writes
the payload into that process and an unread stdin would earn an EPIPE at the
refresh cadence. Precedence was captured live
on 2026-08-29 against Claude Code 2.1.250 in four cases through a real pty
([evidence](./.experiments/2026-08-29-context-signals-and-write-placement.md)):
`.claude/settings.local.json` > `.claude/settings.json` >
`~/.claude/settings.json`, and the winning `statusLine` **replaces** the losing
object outright — a single slot, one command per render, no merge. Since
`.claude/settings.local.json` is precisely the file st2 renders for
driver-declared agents, an st2 entry that does not exec the operator's renderer
silently and unconditionally removes their status line on every managed agent,
with no warning. On a host whose global renderer already displays agent id,
model, context fill, and cost, that is a visible regression st2 would have
caused without saying so.

The inverse is not fixed by chaining and is worth stating: a `statusLine` a
human sets in a managed agent's `.claude/settings.local.json` is not preserved
by st2's materialization of that file, whose merge owns only st2's own hook
entries. A human renderer therefore belongs in the operator's own settings, with
st2's tee chaining to it — not in the file st2 rewrites.

Withholding: `current_usage`, `used_percentage`, and `remaining_percentage` are
`null` until the session's first API response, while `context_window_size` is
populated from the start. The producer writes the window and withholds
`usedTokens` and `usedPercent` (HC-R03). `total_input_tokens` must not be
mistaken for a measurement in that state: the payload builder emits `0` for it
precisely when `current_usage` is null, so the zero is derived from the absence
rather than observed. `current_usage` being null is Claude declaring it does not
yet know.

Adjacent facts on the same payload: `model.id`, `cost.total_cost_usd`, and
`rate_limits.{five_hour,seven_day}.used_percentage`. The payload describes the
top-level session; a subagent's own window is not in it, so a runtime whose
subagent is saturating reports the parent's fill — the grain question is
`DQ-C9`.

Compaction comes from hooks regardless of the status line: `PreCompact` and
`PostCompact` carry `trigger ∈ manual | auto`, and `SessionStart` carries
`source: "compact"` as a third post-compaction edge. `PreCompact` is routed
through the observe hook **in addition to** the existing pre-compact stub script
that writes a working-state reconstruction — the two do different jobs and the
registration carries both.

**The dedupe across three edges, decided during implementation (2026-08-29).**
One compaction raises all three events, and each arrives in its own short-lived
hook process with nothing durable passed between them, so a counter that
incremented on more than one would treble-count every compaction. The dedupe is
therefore *positional* rather than stateful:

| Edge | Counts | Also does |
| --- | --- | --- |
| `PreCompact` | yes — the sole counting edge | writes `trigger` and `lastCompactionMs` |
| `PostCompact` | no — holds the count it reads | advances `lastCompactionMs` from when compaction started to when the window was emptied |
| `SessionStart source=compact` | no — deliberately inert | nothing; it is the same compaction seen a third time |

Counting on the **first** edge is what makes the dedupe need no memory. The
alternative — "count on the second only if the first was seen" — requires
per-compaction state the record deliberately does not carry (HC-T02), and buys
nothing: `PreCompact` fires for every compaction, including one whose
`PostCompact` never arrives because the compaction ended the session or a future
build dropped the event. The one case where `PostCompact` does count is when it
finds no counted compaction at all — no record, or one whose `PreCompact` write
never landed — because it is then the first evidence st2 has, and losing the
event entirely would be worse than attributing it to the later edge.

A compaction carrying an `agent_id` is a subagent's and never touches the record,
matching the categorical producer's guard for the same reason: this record
describes the top-level window (`DQ-C9`).

### codex (codex-cli 0.151.0)

st2 does not re-derive the percentage; it **mirrors** Codex's own
`TokenUsage::percent_of_context_window_remaining` and subtracts the result from
100, so the published number is exactly `100 −` the "N% context left" the
operator reads in the footer:

```text
window = tokenUsage.modelContextWindow        // absent        -> withhold percent
                                              // window <= 12000 -> withhold percent
eff       = window - 12000
used      = max(0, tokenUsage.last.totalTokens - 12000)
remaining = max(0, eff - used)
usedPercent = 100 - clamp(round(remaining / eff * 100), 0, 100)
```

**Mirroring is not the same function as rounding the used fraction**, and the
difference is not cosmetic: at an exact half they disagree. Effective window
200, used 101 — Codex displays "50% left", so st2 publishes 50, while
`round(used / eff * 100)` publishes 51. Only the mirrored order satisfies the
producer table's "equals `100 −` Codex's displayed '% context left'". This
paragraph and the block above **corrected an earlier draft** of this section
(2026-08-29, with the producer) that spelled the arithmetic as
`clamp(round(used / eff * 100), 0, 100)`.

One deliberate divergence from the source: where `window <= 12000` Codex returns
`0` remaining, which mirrored blindly would publish **100% used** for a window it
cannot normalize. st2 withholds instead (HC-R02, HC-R03) — a saturation the
harness never displayed is fabricated, not observed. The operands are still
published: a window at or below the baseline is a window the harness reported.

The 12,000-token `BASELINE_TOKENS` constant is subtracted from **both** the
numerator and the denominator; it is hardcoded in two Codex crates with no
configuration override, and it is exactly the kind of version-coupled constant
HC-T03 names. Recomputed against the measured capture (window 258,400,
`last.totalTokens` 92,283): 33% used, matching Codex's displayed "67% context
left"; a naive `last.inputTokens / window` gives ~36% — close enough to look
right and wrong by construction.

Three traps, all measured:

- `tokenUsage.total` is **cumulative session spend**, not occupancy. It goes to
  `sessionTotalTokens` and nowhere near a percent.
- The numerator is `last.totalTokens`, not `last.inputTokens`.
- The window appears in exactly one place in the whole app-server protocol —
  this notification. A resumed or re-attached thread is not blind: the
  app-server replays `thread/tokenUsage/updated` to the newly attached
  connection before any new turn.

`account/rateLimits/updated` is a separate account-scoped notification and
supplies `rateLimits`. It carries no occupancy, so it never writes on its own:
the windows are held and ride the next reading. It is also documented as a
*sparse rolling update* whose absent fields do not clear a previously observed
value, which is why the producer merges rather than replaces.

**Only `sevenDay` is carried.** Codex names its windows `primary` and
`secondary` and identifies them by `windowDurationMins` alone, so the join is by
duration: 10,080 minutes is seven days, and the one captured Codex rate-limit
snapshot has exactly that window as `primary`. No 300-minute window and no
`secondary` was ever observed on this harness, so mapping one onto `fiveHour`
would be inference dressed as a measurement. `fiveHour` is therefore `null` for
Codex — **a divergence from this section's earlier wording**, which implied the
notification filled both windows — until a capture shows the window; admitting
it is then a one-line change beside that capture.

Codex reports no session cost, so `costUsd` is `null`, and the app-server
`Thread` object carries `modelProvider` but no model identifier, so `model` is
`null` too — both examples above are Codex readings and show those nulls
deliberately.

Compaction: the `contextCompaction` thread item is the live edge (the older
`thread/compacted` notification is deprecated in the protocol). It carries no
sizes and no reason, so the trigger is `unknown`. One compaction reaches the
observer as both an `item/started` and an `item/completed` over the same
`ContextCompactionThreadItem` id, so the count dedupes on `(turnId, item id)`
over a short ring; the deprecated notification names only the turn, so its key
collapses with any item key in the same turn rather than counting beside it. Two
distinct item ids in one turn are two compactions.

Three limits of the shipped producer, all deliberate:

- **The record is written only where native delivery is configured.** The writer
  is owned by the app-server delivery pump, beside the harness-state writer, so
  a Codex seat launched without a delivery config publishes no context record.
  That matches the harness-state precedent — except that harness-state also has
  a wrapper-written terminal record and this axis has none, so Codex coverage is
  exactly "delivery-configured seats".
- **The compaction counter is incarnation-scoped only when the claim succeeds.**
  The reset comes from the relaunch claim removing the record (HC-R15), so on the
  degraded path — a claim that could not be written, which downgrades the *state*
  writer to token-only and proceeds — no removal happened and the new session's
  producer continues the predecessor's count. The record still carries the new
  `incarnation`, so the seam is visible to a reader; nothing else is done about
  it, because a second removal path would be machinery guarding a case the
  provenance field already exposes.
- **A reading replayed on a FRESH binding is dropped; on resume it is not.** The
  pump skips every frame carrying a `method` while it is still binding. The
  resume path reads `thread/tokenUsage/updated` out of that skip explicitly,
  because a resumed thread still holds its context and the claim has just removed
  the predecessor's record — a seat that resumes and then waits for work would
  otherwise read `null` against a full window until its next model response. The
  fresh-binding path needs no equivalent: there is no thread id to match against
  before the binding candidate names one, and a thread starting now has no
  history to replay.

Finally, a version note. The arithmetic and notification shape above were
reverified against codex-cli 0.151.0's Rust source, and
`CODEX_CONTEXT_VERIFIED_VERSION` pins that literal in the fixture (HC-R13). That
matches the newest build admitted by `SUPPORTED_CODEX_CLI_VERSIONS`; it does not
turn the measurement into a semantic-version promise. Every later Codex release
still requires its own source comparison and live delivery proof before the exact
launch gate moves.

### pi (0.84.2)

One call answers everything, present on the `ctx` of every lifecycle event:
`getContextUsage()` returns `{ tokens, contextWindow, percent }`.

The honest-unknown behavior is the load-bearing one: immediately after a
compaction, and across a process restart until the next assistant usage arrives,
`tokens` and `percent` are both `null` while `contextWindow` stays populated. pi
positively says it does not know, and the producer withholds rather than
substituting (HC-R03) — the same discipline
[`OHS-R02`](../05-harness-state/requirements.md) applies to the categorical
axis.

`session_compact` carries `reason ∈ manual | threshold | overflow` and
`tokensBefore`. The count is harness-durable: `ctx.sessionManager.getEntries()`
filtered to `type === "compaction"` survives restarts (measured 2 → 3 across a
compaction and read back correctly on the next process's `session_start`).
`ctx.model.id` supplies `model`; the per-message `usage.cost.total` supplies
`costUsd`.

Three facts settled while implementing, each of which changes the shape of the
producer rather than only its constants:

- **The nulls are already there inside the handler.** `getContextUsage()` called
  *within* pi's own `session_compact` handler returns `{tokens: null,
  contextWindow: 4000, percent: null}` — not the pre-compaction numbers — and
  `getEntries()` has already counted the new entry there (2 → 3 read in the same
  handler). So the edge and the withheld reading are available together and are
  emitted as **one frame that lands as one write**. That pairing is load-bearing,
  not an optimization: a compaction edge always writes while a withheld percent
  has no bucket, so an edge written on its own would publish the stale
  pre-compaction numbers beside it, and the null reading proving the window was
  emptied would not appear until the heartbeat came due.
- **`message_end` is the emit boundary, not `turn_end`.** The producer emits on
  `session_start`, `message_end`, `turn_end`, `agent_end`, and `agent_settled`,
  and holds no cadence policy of its own — the write guard is what bounds cost,
  at one write per bucket entered however chatty the harness is. The finest
  boundary is what matters, for the same reason turn-boundary-only *writing* was
  disqualified above at 92% of warnings missed: the wedge case is a single long
  turn, and on pi each tool call and its result form their own assistant
  message. One consequence is worth stating because it looks like a defect and
  is not: immediately after a compaction, `message_end` for the next assistant
  message still reads `null` while that message's own `usage` is already
  populated, and only `turn_end` reports the new occupancy. The producer
  forwards both honestly — the null is a real reading of "pi does not know yet".
- **`costUsd` is restated on every frame, from a producer-side hold.** Cost rides
  only the message-bearing events, but a frame is emitted from events that carry
  none. The record replaces a reading's fields wholesale — deliberately, so a
  withheld value is never fabricated from a previous one — so a frame omitting
  the cost would *erase* the published one at the next turn boundary. The
  extension holds the last assistant `usage.cost.total` and restates it, which is
  exactly what `costUsd` means for pi and omp. This is the one place where the
  record's general field description ("the harness-reported session cost") and
  the per-harness rule (a per-message figure) differ, and the per-harness rule
  wins: summing to a session total would need the producer-side accumulator
  HC-R16 refuses for `sessionTotalTokens` for the same reason. The hold is
  cleared on session replacement, so a `/new` session does not restate its
  predecessor's cost.

### omp (18.0.9, also verified on 18.0.3)

The same call, the same shape — and a different meaning. omp's `tokens` settles
to the last assistant message's **`input`** alone, where pi's settles to
`totalTokens`; measured in a controlled lab where the fake provider reported
prompt tokens of 900, 9,900, and 22,500 and `getContextUsage().tokens` returned
exactly those figures. So omp under-reports relative to pi by output plus cache
on an otherwise identical API. This is HC-T03's second version-coupled constant
and the reason the two harnesses get separate producer rows rather than a shared
one.

`session_compact` carries **no** `reason` and no `willRetry` (pi's does), so the
trigger is `unknown` even though omp internally calls its auto-compaction with
`"idle"` and `"threshold"` — those words are not projected onto the event. The
count is harness-durable through the same `sessionManager.getEntries()` path as
pi's.

The producer is otherwise the pi one with two divergences, both measured:

- **omp does not null its reading at the edge.** Inside `session_compact`,
  `getContextUsage()` still answers (8,100 measured against a 4,000-token
  window), where pi's returns nulls. Both harnesses therefore send the reading
  and the edge as one frame; only pi's carries a withheld one.
- **The emit set drops `agent_settled`**, which omp does not have (0 occurrences
  in either binary), and the idle edge stays the existing `agent_end` poll. omp
  fires `session_start`, `message_end`, `turn_end`, and `agent_end` with a
  working `getContextUsage()` on each.

The version pin (HC-R13, HC-T03) extends the existing launch gate rather than
adding a mechanism beside it, and the two answer deliberately different
questions. `SUPPORTED_OMP_MINORS` admits a **minor series**, because a patch
inside an admitted minor costs no new evidence (decision 0007-omp-is-a-fifth-native-driver-with-its-own-channel-and-a-hard-version-gate); the fixture pins
the **exact builds** a number's meaning was measured on, because "`tokens` is
prompt-only input" is a property of a build and not of any documented contract.
`src/omp_session.rs::the_measured_context_builds_are_admitted_by_this_gate`
keeps them from drifting apart: a build the fixture claims to have measured but
the gate would refuse to launch is evidence for a version the fleet can never
run. pi has no runtime gate at all, so its fixture pins the flake tarball the
extension check already type-checks and runtime-smokes against
(`the_measured_pi_release_is_the_one_the_extension_gate_pins`).

Correction of record: the 18.0.3 capture
([`2026-08-25-omp-harness-integration.md`](../06-omp-driver/.experiments/2026-08-25-omp-harness-integration.md))
states that the handler `ctx` exposes `{ui}` only. The 2026-08-29 probe run
against both binaries shows `getContextUsage` present and working on 18.0.3 and
18.0.9 alike (35 occurrences in each binary) — a probe artifact in the older
capture, not a version change. `signal` and `agent_settled` genuinely are absent
from both, as that capture says.

### opencode (1.18.25)

The only producer that needs two sources: the numerator is pushed on the SSE
stream and the denominator is pulled from the config surface. Both traps here
are measured and both silently produce a wrong number:

- **`summary` is overloaded.** On user messages it is an object (`{diffs: []}`)
  and therefore truthy; the filter for a compaction summary must be
  `role === "assistant" && summary === true`.
- **The summarizer's own message poisons the last-assistant reading.** After a
  compaction the newest assistant message is the summary, whose `tokens.total`
  is the cost of the summarization call (1,511 measured), not the new context
  size. The producer skips summary messages when picking the last reading.
- **`session.updated`'s `info.tokens` is cumulative** and carries no `total`
  key. It grows without bound and is `sessionTotalTokens`, never occupancy.

`tokens.total` appears only on the final `message.updated` for a message; the
first carries zeros and no `total`, so the producer requires the KEY and never
reads a missing one as zero. Which session's reading this is when one server
holds several — the categorical producer aggregates across them, but two live
sessions have two windows and one record — is unsettled: `DQ-C10`. v1 is
last-writer-wins over every session on the stream, which matches the categorical
producer's aggregate rather than inventing the seat-session rule `DQ-C10` exists
to defer. `session.compacted` carries `{sessionID}`
and nothing else — no reason, no timestamp, no sizes — so the trigger is
`unknown` and the count is st2's own, scoped to this incarnation. The richer v2
events (`session.next.compaction.started` /
`ended`, which do carry a reason) exist in the OpenAPI document but did not fire
once on the legacy path in the measured runs; they are schema-only and no
producer depends on them.

The producer publishes on one condition only: a `message.updated` that carried a
fresh `tokens.total`. The adjacent facts arrive a frame later on
`session.updated` and are folded into producer state, then ride out with the next
numerator that LANDS a write; publishing on them directly would stamp
`observedAtMs` on a numerator that may be hours old. They can therefore lag
several turns — a run of readings inside one bucket writes nothing — and that is
the intended shape: the record is one coherent snapshot of the turn that last
landed, stamped with that turn's time, rather than a mix of a stale numerator and
fresh costs.

For the same reason this producer has no heartbeat: its numerator is pushed and
not pullable, so it never holds a re-taken reading to publish, and a quiet seat's
record ages visibly instead (HC-R06). A window that takes no turn does not fill,
so the aging number stays true. One consequence is expected and must not be
"fixed": an OpenCode seat idle for longer than `HARNESS_CONTEXT_STALE` (60
minutes) crosses the horizon and Doctor's HC-R17 stale-record line fires beside a
`running` desired state until the next turn. The other four producers re-pull on
a cadence and never reach it; this one cannot, because there is nothing to
re-pull. Closing that advisory by heartbeating the in-memory numerator would
re-stamp `observedAtMs` on a reading no one re-took, which is exactly what the
writer's contract forbids — the advisory is the honest report of a seat whose
last measured occupancy is an hour old.

`model` is written in OpenCode's own `providerID/modelID` spelling
(`opencode/hy3-free`): `modelID` alone is ambiguous across providers, and this is
the key the window is cached under, so the record always pairs a numerator with
its own denominator. `usedPercent` is st2's unrounded, unclamped
`usedTokens / windowTokens × 100`. Rounding it would move the written value into
a different 1% bucket than the truth and make `HARNESS_CONTEXT_WARN_PERCENT` fire
below the threshold it names.

Measured on a live 1.18.25 lab run (two turns plus a forced summarize, 571 SSE
frames): exactly **one** landed write, the compaction counted once with trigger
`unknown`, and the summarizer's own message — `summary: true`, `tokens.total`
1,462, and a *different* model id from the turns' — correctly skipped, where
taking it would have published 0.7% of the window instead of 4.3%.

## Compaction accounting (HC-R12)

| Harness | Edge | Trigger carried | Trigger words yielded | Count source | Counter scope |
| --- | --- | --- | --- | --- | --- |
| claude | `PreCompact` / `PostCompact` hooks; `SessionStart source=compact` | `trigger` | `manual`, `auto` | st2 counts, on `PreCompact` alone — see the dedupe above | incarnation |
| codex | `contextCompaction` thread item | none | `unknown` | st2 counts | incarnation |
| pi | `session_compact` | `reason` | `manual`, `threshold`, `overflow` | `sessionManager.getEntries()` type `compaction` | harness-durable |
| omp | `session_compact` | none | `unknown` | `sessionManager.getEntries()` type `compaction` | harness-durable |
| opencode | `session.compacted` | none | `unknown` | st2 counts the edge (one per compaction, corroborated 1:1 by the assistant message with `summary === true`) | incarnation |

The vocabulary `manual | auto | threshold | overflow | idle | unknown` is closed
and additive-tolerant. `idle` is in v1 because omp's internal auto-compaction
names it; **no v1 producer emits it**, since omp does not project its reason
onto the event. A word arriving that a reader does not recognize decodes as
`unknown`.

A harness-durable row degrades to its incarnation-scoped neighbour rather than
losing the edge: a producer that cannot read the session store sends the edge
without a count, and st2 increments its own. That is a weaker answer — the scope
silently narrows, which is why it is stated here — and never a wrong one, and it
is the only path by which a "harness-durable" row can produce an
incarnation-scoped number.

"Incarnation" scope means the counter starts at zero when the session record is
claimed (HC-R15); "harness-durable" means the harness's own session store
answers the question and the count spans restarts. A consumer comparing counts
across harnesses is comparing two different questions, which is why the scope is
stated per row rather than averaged into one number.

Per-compaction detail — sizes, the reason of every earlier compaction, dwell
between them — is not recorded (HC-T02, `DQ-C6`).

## Session boundary and provenance (HC-R15)

```text
relaunch: wrapper claims harness-state (existing written claim)
            \-- and removes harness-context
first reading of the new session -> new record, incarnation = the claiming token
straggler write from the old session -> lands; visibly older incarnation, aged by
                                        observedAtMs; the next real reading overwrites it
```

The claim path gains one file removal and nothing else. A reader after a
relaunch sees `null` ("no context yet") rather than the previous incarnation's
190k, which is the honest answer for a window that has just been emptied. The
alternative — leaving the old numbers to age out over the 60-minute horizon —
would show a crash-looping agent the previous incarnation's fill as if it were
current.

There is deliberately no sequence, no written claim on this record, and no floor
sidecar (HC-T04).

## Exposure (HC-R14)

`st2 agents --json` (both forms) appends one field per row, always present:

```json
"context": {
  "harness": "codex",
  "usedTokens": 92283,
  "windowTokens": 258400,
  "usedPercent": 33,
  "model": null,
  "costUsd": null,
  "sessionTotalTokens": 2235329,
  "rateLimits": { "fiveHour": 31, "sevenDay": 55 },
  "compactions": 3,
  "lastCompactionMs": 1788000097290,
  "lastCompactionTrigger": "unknown",
  "observedAtMs": 1788000100000,
  "ageMs": 4210,
  "stale": false
}
```

`null` when no record exists — the roster's existing convention for
`observedState`, kept rather than omitting the key, so the wire has one
convention and not two. The reading projection above is already applied: a
consumer never re-implements staleness, and never sees a record it would have to
age itself. `incarnation` and `writtenAtMs` are deliberately not exposed: the
first is provenance for a reader inspecting the record itself, and the second is
a write-policy mechanism a roster consumer has no use for once `ageMs` and
`stale` are computed.

The origin stamp is spelled `observedAtMs` on the roster wire, exactly as in the
record, following the `sinceMs`/`writtenAtMs` convention the driver records
already use. An earlier draft of this section spelled it `observedAt`; that was
an inconsistency within this document rather than a decision, corrected
2026-08-29 while the field had no producer and no consumer depending on the
short spelling. `driverDiagnostic`'s own unsuffixed `observedAt` is a different
record's shipped name and is deliberately left alone — this is one record's
field agreeing with itself, not a rename campaign across the roster.

Human `st2 agents` output carries the same axis compactly as `ctx:92% ⟳1`,
beside the existing `obs:` column and prefixed for the same reason — two bare
words in one row is exactly the ambiguity the ontology's collision rules name.
`ctx:-` is no record and `ctx:?` is a record whose percent the harness withheld —
distinct on purpose, since rendering both as `-` would say "nobody is watching"
for a producer that is watching and honestly does not know, which is Claude's
state before its first API response. A stale reading is suffixed `stale`. The
percent is rounded for width and never clamped.

`status`, `desiredState`, `lastActivity`, `observedState`, and
`driverDiagnostic` keep their exact meanings. `context` is a fourth independent
axis: none of the five is derived from another.

## Doctor (HC-R17)

Doctor emits an advisory line for an agent it owns in two cases:

| Condition | Advisory | Exit status |
| --- | --- | --- |
| `usedPercent` ≥ `HARNESS_CONTEXT_WARN_PERCENT` | context fill, with the reading's age | unchanged |
| record `stale` while desired state is `running` | the numbers are older than the horizon | unchanged |
| no record, unreadable record, or a fresh reading below the threshold | nothing | unchanged |

Neither case is ever a failure. This matches how Doctor already treats the
categorical axis ([`OHS-R10`](../05-harness-state/requirements.md)): a stale or
session-dead record beside a `running` desired state is worth a warning and
never an exit-code failure, and the numeric axis adds no new authority
(HC-A02).

**80 is st2's number, not the harness's.** It is a threshold for "worth a
human's attention", chosen so an operator sees a filling window with room left
to act. It is deliberately **not** an estimate of where the harness will
compact: that point depends on the harness, the model, and the operator's own
settings — Codex reserves a 12,000-token baseline and a separate auto-compact
buffer, pi compacts when occupancy passes `contextWindow − reserveTokens` with
a 16,384-token default, omp exposes an idle-triggered path with its own
threshold, and Claude's auto-compact window is overridable by environment
variable. A single st2 constant claiming to predict that would be wrong in both
directions on most harnesses, and would silently change meaning under a harness
bump — the failure mode HC-T03 already names for the producer constants.

Because the threshold is st2's own, it carries no coupling to the write policy:
quantization at 1% is threshold-agnostic (HC-R10), so moving this constant needs
no change to what producers write, and a reader alarming at some other number is
equally well served.

## Verification plan

The invariant rows this subsystem must add or amend when implementation lands,
each only once a real test proves it (per `CLAUDE.md`):

- **Stable roster JSON** (existing row) — wording gains the fourth axis and its
  null convention. `src/agents.rs::agents_json_has_stable_wire_shape` is edited
  in the same change and stays the proof; the pinned full-string assertions in
  `src/agents.rs` change deliberately alongside it.
- **Scoped delivery-input wakeups** (existing row) — the row's prose enumerates
  the runtime records that must never wake a delivery pump by name, and gains
  `harness-context`. The delivery watcher is an allowlist
  (`starts_with(resources/inbox)` or the exact `status` path), so
  `src/watch.rs::delivery_watcher_ignores_runtime_records_but_wakes_on_inbox_and_status`
  stays the proof and stays green with no production change — the row's wording
  is what moves.
- **Harness context discipline** (new row, once proved) — stale readings
  returned with their age rather than derived away; numbers surviving every
  `observedState: unknown` derivation; withheld values never fabricated; a
  reading within the written bucket not written; a bucket crossing, a compaction
  edge, and a record older than the heartbeat each written; the record removed
  on the relaunch claim.
- **Doctor advisory, never failure** — a reading at or above the threshold and a
  stale record beside `running` each produce advisory output and leave the exit
  status untouched; a fresh reading below the threshold produces nothing. Test
  in the shape of the existing Doctor coverage in `tests/doctor.rs`.
- **Replicated-path discipline** (new row, once proved) — st2 pins the exact
  driver-record names it expects the transport's include list to carry, and no
  staging file is ever created inside the replicated subtree. Both are silent
  failures in production, so tests pin `harness-state` and `harness-context`,
  assert staging below the catalog control directory with same-filesystem
  atomic publication and cleanup, and bound legacy-reader compatibility to the
  exact regular-file shape without changing a digest, snapshot, or message
  address.
- **Status-line slot chaining** (HC-R18) — a rendered status-line registration
  invokes the operator's downstream renderer. The slot is single-valued and the
  winner replaces rather than merges, so a test that only checks st2's command
  is present would pass while the operator's renderer is silently gone. The
  proofs therefore run the tee as a process and assert its exact stdout bytes:
  the renderer's line and only the renderer's line, an empty stdout in every
  degraded arm — none resolving, a renderer that exits non-zero, a renderer file
  that is not executable — and a rendered line still produced when recording
  fails. The degraded arms take their positive evidence from stderr, since an
  empty stdout alone is also what a tee that crashed instantly would leave.
  Landed as the invariant row *Status-line slot chaining*
  (`tests/claude_statusline.rs`).
- **Per-harness fixture tests** (HC-R13) — one per producer, each decoding a
  payload captured verbatim from the version named in the producer table and
  asserting the resulting triple, with the version asserted literally. All five
  ship, and each pins the numerator its harness actually means: the Claude
  fixture must fail if the numerator's terms or the percent rule move, the Codex
  fixture if the 12,000 baseline moves, the omp fixture if `tokens` stops
  meaning prompt-only input, the pi fixture if it stops meaning the last
  assistant message's `totalTokens`, and the OpenCode fixture carries the
  post-compaction summarizer frame so a producer that stops skipping summary
  messages fails it.

  **Shipped for claude**, over the 2.1.250 status-line payloads:
  `src/claude_session.rs::a_mid_session_statusline_payload_yields_claudes_own_triple`,
  `src/claude_session.rs::a_pre_turn_statusline_payload_withholds_rather_than_reporting_zero`.

  **Claude's two fixtures and what each proves.**
  `tests/fixtures/harness-context/claude-statusline-pre-turn.json` is the
  verbatim 2.1.250 live capture and proves the withholding — including that
  `usedTokens` is `null` rather than the `total_input_tokens: 0` sitting beside
  it. `claude-statusline-mid-session.json` is a **composition**, stated here
  because it bounds what the fixture proves: the envelope, `context_window_size`,
  and `rate_limits` are that same verbatim capture, the `current_usage` object is
  a verbatim `usage` object off an assistant line of a real 2.1.250 transcript,
  and `used_percentage` / `total_input_tokens` are what the bundle's own builder
  computes from the two. So each operand is measured and the arithmetic joining
  them is quoted from the harness, but 2.1.250 was never observed emitting
  exactly these bytes together — a populated live capture needs a paid turn and
  none was taken. A bump that moves the numerator's terms or the percent rule
  still fails the fixture, which is what HC-R13 asks of it.

  **Shipped for codex**:
  `src/codex_app_server.rs::codex_context_recomputes_the_captured_reading_and_pins_its_verified_version`
  over `tests/fixtures/codex_token_usage_inbound.jsonl`, asserting
  `CODEX_CONTEXT_VERIFIED_VERSION` and the baseline literally, and asserting
  both wrong numerators against the same capture (`total` reads 100, a
  baseline-free `last.totalTokens / window` reads 36, the truth is 33).

  **Shipped for pi and omp**:
  `src/pi_channel.rs::the_pi_0_84_2_fixture_pins_total_tokens_as_the_numerator`,
  `src/pi_channel.rs::the_omp_18_0_9_fixture_pins_prompt_input_as_the_numerator`,
  `src/pi_channel.rs::a_pi_compaction_withholds_the_reading_it_emptied_in_the_same_write`,
  `src/pi_channel.rs::an_omp_compaction_yields_unknown_because_the_event_names_no_reason`,
  `src/pi_channel.rs::an_unreadable_durable_count_degrades_to_counting_edges_not_to_losing_them`,
  `src/pi_channel.rs::context_frames_decode_conservatively_or_not_at_all`,
  `src/pi_channel.rs::the_measured_pi_release_is_the_one_the_extension_gate_pins`,
  `src/omp_session.rs::the_measured_context_builds_are_admitted_by_this_gate`.

  **Shipped for OpenCode**, over verbatim 1.18.25 SSE frames whose own
  `session.updated.info.version` is the asserted version:
  `src/opencode_session.rs::captured_opencode_turns_publish_the_assistant_total_over_the_providers_window`
  (the join, the unrounded percent, and one landed write for a duplicated frame),
  `src/opencode_session.rs::cumulative_session_tokens_are_the_session_total_and_never_the_occupancy`,
  `src/opencode_session.rs::neither_summary_shape_is_ever_the_reading`,
  `src/opencode_session.rs::a_captured_compaction_counts_once_with_an_unknown_trigger_and_no_restamp`,
  `src/opencode_session.rs::an_unjoined_model_withholds_the_window_and_the_percent_until_the_pull_lands`,
  `src/opencode_session.rs::a_configured_model_change_makes_the_window_pull_due_without_retagging_the_reading`,
  `src/opencode_session.rs::the_window_pull_reads_config_providers_through_the_real_client`.

  Each fixture carries *both* numbers from its capture — the one the producer
  must publish and the one it must not — because the failure this test exists to
  catch is a change of meaning with no change of shape, which no type gate and no
  round-trip assertion can see. The pi fixture asserts `23425` and not `23300`;
  the omp fixture asserts the prompt figure and not that message's `totalTokens`.
  The row *Version-pinned producer arithmetic* in
  [INVARIANTS.md](../../../INVARIANTS.md) names the five.

- **Extension asset runtime smoke** — `checks.pi-extension-types` transpiles both
  shipped assets and drives every registered handler, because the type gate is
  provably blind to execution-order defects (a TDZ shipped green through it).
  Each handler is now driven with three contexts: a bare one carrying none of the
  telemetry surface (the fail-open path an unmanaged or older build takes), a
  fully populated one carrying the measured shapes, and one whose every telemetry
  pull throws. The bare context alone was the gap — it never executes the
  producer's body at all, so a use-before-declaration inside it would have
  shipped green through both the type gate and the previous smoke. A compile-time
  pin on pi's own declarations for `getContextUsage`, `model`, and
  `sessionManager` keeps the type gate's teeth where the producer's runtime
  guards deliberately widen the view; what it cannot catch is exactly the change
  of meaning the fixtures above bound.

  The smoke's channel is a **recorder**, not `true`, and this is the part that
  makes the check mean something. The extension-to-`pi_channel` frame envelope —
  `{type: "context", reading: {...}, compaction: {...}}` — has its two halves in
  different languages in different files, and nothing else couples them: flatten
  the reading or rename a key and every fixture above stays green while the
  record is never written for the rest of time. That failure is indistinguishable
  from the pre-producer state this document describes, which is what makes it
  silent, and it is the same shape as the replication include list that
  *Replicated-path discipline* pins names for. So the smoke reads its frames back
  and asserts the wire: a context frame is emitted at all, the reading carries
  all five keys, the numerator is each harness's own (23,425 for pi, 22,500 and
  explicitly not 22,525 for omp), the percent is unclamped, an edge whose session
  store could not be read still arrives countless, and pi's withheld reading
  rides the same frame as its edge. Verified non-vacuous by flattening the
  envelope and watching the check fail.

  Note this check does **not** run under `cargo test` — it is a flake check and
  must be built explicitly.

## Open design questions

Tracked with context in [open-questions.md](./open-questions.md): `DQ-C1` write
policy benchmark (accuracy half settled, wire-cost half open), `DQ-C5`
unreadable versus absent, `DQ-C6` history, `DQ-C7` fleet transport cost, `DQ-C8`
supervisor actionability, `DQ-C9` subagent context, `DQ-C10` OpenCode
multi-session aggregation.

Resolved, and kept in that file only so their identifiers do not go dangling:
`DQ-C2` status-line renderer contract, `DQ-C3` status-line settings precedence,
`DQ-C13` the status-line tee's telemetry cadence,
`DQ-C4` Doctor exposure and threshold, `DQ-C11` `harness-state`'s placement
defect, `DQ-C12` a driver record under `resources/` (withdrawn).
