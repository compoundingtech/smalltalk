# Observed harness state specification

This document specifies the observed-harness-state record. It builds on
[requirements.md](./requirements.md). Declared presence remains in the root
spec's R08 section; terminal delivery remains in
[`01-ding/spec.md`](../01-ding/spec.md) and reads none of this.

## Status

Draft. The envelope (`src/harness_state.rs`), all four producers, the scoped
delivery-input watcher, the native-driver diagnostic snapshot, and the
roster/Doctor exposure are implemented; the former DELTA-005 fence is
resolved and deleted, and the DQ-H1 and DQ-H6
captures are taken and folded in. Draft status remains for the genuinely
unmet residuals: root `DQ3`'s supervisor-following gate (`DQ-H5`) and
Claude's eventless deny path (the remaining `DQ-H1` window). Open questions
are tracked in [open-questions.md](./open-questions.md).

The `agent` field's immutable-ID meaning is the accepted target, and it ships
as a new record version, `st2.harness-state.v2`: this record's version suffix
is its read contract, so changing an existing field's meaning reserves the next
version rather than reusing `v1`. Records and producers stay on
`st2.harness-state.v1`, whose `agent` remains a bus identity, until
[DELTA-003](../.delta/DELTA-003-agent-address-not-implemented.md) closes.
Version 2 is otherwise identical to the shape below. The reader-first rollout
accepts both versions — narrowing the `unsupported-schema` rule below to
versions outside that pair — before any version-2 writer activates.

## Scope

This specification defines the observed-state record, its freshness and
derivation rules, the per-harness producers, the adjacent native-driver
diagnostic record, and their roster/Doctor exposure. It does not define:
transition history or a `--watch` surface (deferred, OHS-T03); idle thresholds,
escalation, or notification policy (#173's, per root `R20`); the withdrawn
host-local hot tier; the cut PTY screen observer; or any `AGENT-SPEC.md`
change (separate authority, root `DQ3`).

## Overview

```text
   codex app-server        claude hooks +          pi extension        opencode
   control stream          wrapper child poll      (evented)           server surface
        |                        |                      |                  |
        v                        v                      v                  v
   [driver-owned projection: idle/active/child/ended × blockedOn (+ask) × inputBuffer]
        |
        v                                          declared axis (unchanged)
   <agent-dir>/harness-state                       <agent-dir>/status
   st2.harness-state.v1                            presence, agent-authored
   transition writes + 5-min heartbeat                     |
        |                                                  |
        +--------------------+-----------------------------+
                             v
              st2 agents --json  (status ∥ observedState)
              st2 doctor         (advisory)
              downstream TUI     (spinner, blocked-on-you)
```

## Record (OHS-R01, OHS-R03)

One JSON object, atomically written (tmp sibling + rename), newline-terminated:

```json
{
  "schema": "st2.harness-state.v1",
  "agent": "<identity in v1; agent ID in v2>",
  "harness": "codex | claude | pi | opencode",
  "state": "idle | active | child | ended",
  "blockedOn": "none | human",
  "ask": "none | permission | question | review (reserved; no producer emits it)",
  "inputBuffer": "empty | nonempty | unknown",
  "reason": "<diagnostic, optional>",
  "exit": "<ended only: e.g. 'exit 0', 'signal 9', optional>",
  "ptySession": "<the wrapper's runtime/task ID; required for live states>",
  "incarnation": "<the writing session's token>",
  "seq": 7,
  "sinceMs": 1787690000000,
  "writtenAtMs": 1787690300000,
  "transitions": 41
}
```

Field rules, matching `src/harness_state.rs`:

- `state` on disk is never `unknown` (OHS-R02) and never `child` today
  (`DQ-H3`). Readers decode both, plus unrecognized future words as `unknown`.
- `blockedOn` unrecognized words decode indeterminate, never `none` — a v2
  axis value must not read as "not blocked".
- `ask` names what kind of human ask holds the harness, machine-readably —
  the axis consumers filter on (`reason` stays diagnostic). Meaningful only
  while `blockedOn` is `human`; writers emit `none` otherwise, records from
  writers predating the axis default to `none`, and unrecognized future words
  decode indeterminate.
- `incarnation` is the writing session's token and `seq` its monotonic
  ownership sequence. A claim is a WRITTEN act under the record lock: the
  session starting up writes an exitless `ended (superseded)` takeover
  record carrying its token and a sequence one above the highest this seat
  has seen — the maximum of the on-disk record's sequence and the
  `.harness-state.seq` floor sidecar, a sibling file (written
  stage-and-rename under the same lock on every claim) that keeps claims
  monotonic even when the record itself is unreadable. Racing
  claimers therefore mint distinct sequences, and a predecessor's
  still-fresh live record is superseded at relaunch, where the
  pty-name-based probe cannot tell sessions apart; readers derive
  indeterminate (`claimed`) from the fresh placeholder — a fence is not an
  observation — until the session's first real observation. Ownership
  — coalescing, heartbeat eligibility, terminal suppression (which applies
  only to exit-bearing terminal records, never the claim placeholder) — is
  token equality with the claim's direction: a straggler whose claim is
  below the on-disk sequence is refused in live and terminal paths alike,
  and a token-only writer never claims — it adopts its own session's
  records, starts virgin ones, and is refused against foreign tokens.
  Sibling writer processes of one session share the claimer's exported
  token and sequence; records predating either field decode with an empty
  token and sequence zero, which no session owns and any claim supersedes.
  The residual: with the record unreadable AND the floor sidecar missing or
  damaged, a claim restarts at sequence one and a lingering predecessor
  holding a higher sequence could fence it — accepted, because it takes
  both files independently damaged, and refusing the claim instead would
  wedge the seat permanently.
  Every landed write carries a strictly monotonic per-record stamp (never
  inherited from beyond the future-skew trust bound) so it stays
  byte-distinct even against a same-millisecond predecessor.
- `reason` is diagnostic only; no consumer branches on it.
- `ptySession` fences every live state: a writer refuses a live observation
  that names no session (only `ended` may omit it), because a live record the
  probe cannot check would stay definite straight through an external SIGKILL.
- `sinceMs` is when the current state was entered and survives heartbeat
  re-stamps; `writtenAtMs` is the heartbeat. `transitions` is a monotonic
  counter continued across writer restarts; with `writtenAtMs` it keeps every
  write byte-distinct.
- Writes are session-owned by incarnation token, and ownership has a
  DIRECTION through `seq`. A claim is a WRITTEN act under the record lock —
  an exitless `ended (superseded)` takeover record carrying the new token
  and a sequence one above the highest the seat has seen (the on-disk
  record's or the `.harness-state.seq` floor sidecar's, whichever is
  higher, so an unreadable record does not restart the sequence) — so
  racing claimers serialize and mint
  DISTINCT sequences (no tie exists), and a predecessor's still-fresh live
  record is superseded at relaunch; readers derive indeterminate (`claimed`)
  from the fresh placeholder until the session's first real observation. A
  straggler from a superseded session (its claim below the on-disk sequence)
  is refused in live and terminal paths alike, a token-only writer never
  claims (it adopts its own session's records, starts virgin ones, and is
  refused against foreign tokens), and terminal suppression applies only to
  exit-bearing records — never the claim placeholder. `sinceMs` never spans
  a restart; a lingering predecessor can neither heartbeat nor overwrite its
  successor's record.
  Heartbeats and coalescing never touch a record whose schema or token the
  writer does not own; a foreign record is left byte-identical by heartbeats
  and replaced only by a claiming observation. One residual, stated:
  Claude's exported ownership pair is visible to its tool children like
  every other hook-environment value (`ST_AGENT` included) — pi stashes and
  unexports instead because pi fronts its own environment onto every bash
  child, an architectural difference, not an oversight.
- Deserialization is additive-tolerant (no `deny_unknown_fields`): a reader
  may be older than its writer.

Constants, deliberately not aliases of the presence constants (OHS-R03):
`HARNESS_STATE_STALE` 15 min, `HARNESS_STATE_REFRESH` 5 min,
`HARNESS_STATE_FUTURE_SKEW` 60 s.

## Derivation (OHS-R02, OHS-R07)

What a reader reports, in evaluation order:

| Evidence | Reads as | Reason |
| --- | --- | --- |
| No record file | no observation (`null`) | never observed ≠ `unknown` |
| File exists but cannot be read | `unknown` | `unreadable-record`; an IO error is indeterminate, never absence |
| Unparseable bytes, or bytes not shaped like an accepted version | `unknown` | `malformed-record`; never falls back to mtime |
| `schema` is neither `st2.harness-state.v1` nor, once tolerant readers ship, `st2.harness-state.v2` | `unknown` | `unsupported-schema`; a future schema's words may be spelled like this version's while meaning something else |
| `writtenAtMs` > now + 60 s | `unknown` | `future-skew` |
| `writtenAtMs` ≤ now − 15 min | `unknown` | `stale` |
| Literal `unknown` state (never written by this crate) | `unknown` | `literal-unknown` |
| Live state, same-host probe proves `ptySession` dead | `unknown` | `session-dead` |
| Live state, probe available, record names no `ptySession` | `unknown` | `unfenced-record`; nothing to check is not the same as checked |
| Live state, probe indeterminate | the recorded state | unprovable evidence downgrades nothing |
| Fresh claim placeholder (`ended`, exitless, reason `superseded`) | `unknown` | `claimed`; a fence is not an observation |
| `ended`, any probe result | `ended` | a terminal record outlives its writer |
| Otherwise | the recorded tuple | — |

Every `unknown` row routes through one constructor and blanks every axis;
there is no path from any absence to a definite state.

Two reader-side limits, stated before anyone finds them: `pty kill` removes
the session pidfile, so the liveness probe reads *indeterminate* rather than
*dead* after it — a seat that was `active` when killed genuinely reads
`active` (measured), with Doctor silent, until the staleness horizon or the
next session's written claim supersedes the orphan at relaunch, whichever
comes first. The cross-check is a narrowing of the ungraceful-death window
(provably dead sessions: pidfile present, process gone), not its closure —
OHS-T04/OHS-R07 say exactly this, and no death tombstone is attempted: the
kill that removes the registry entry leaves nothing behind to prove death
with, and fabricating evidence is the one thing this design never does.

Each controlled Codex startup generates the installed app-server schema. st2
checks the required methods, response fields, blocking flags, and data shapes
that native delivery uses. A compatible patch or minor release starts without
a source change. A missing required element or a changed critical shape stops
before the app-server starts. The wrapper sends one idempotent rejection report
to the agent's declared supervisor. The version string is diagnostic data, not
an admission proxy.

Additive item kinds and server-request methods do not stop startup. A listed
element is reviewed and safe to ignore. An unlisted element creates an
`UnknownProtocol` delivery hold. The next safe thread status releases the hold.
This fail-closed runtime rule keeps the agent present and prevents a silent
delivery into a new hold that st2 does not understand.

## Codex producer (OHS-R05)

The projection reads the state the control pump already maintains; it adds no
observation path. `Held` never enters the published vocabulary — it is the
complement of steerable, a delivery predicate (decision 0001's boundary).

| `CodexObservedState` | state | blockedOn | ask | reason |
| --- | --- | --- | --- | --- |
| `AwaitingStatus` | *withhold* | — | — | no evidence yet; the record ages |
| `Idle` | `idle` | `none` | `none` | |
| `Active { turnId }` | `active` | `none` | `none` | |
| `TerminalError { systemError }` | `ended` | `none` | `none` | `systemError` |
| `TerminalError { providerAuthRejected }` | `ended` | `none` | `none` | `providerAuth` — a failed turn whose typed error names `unauthorized`; the same word the OpenCode producer publishes |
| `Held { ActiveWithoutTurn }` | `active` | `none` | `none` | `activeWithoutTurn` — Codex said active; st2 merely cannot name a steerable turn |
| `Held { ConflictingTurn }` | `active` | `none` | `none` | `conflictingTurn` — two turns believed live is maximally active |
| `Held { Review }` | `active` | `none` | `none` | `review` — review's enter and exit are model-emitted items inside a running turn; nothing awaits a human |
| `Held { Compaction }` | `active` | `none` | `none` | `compaction` |
| `Held { UnknownProtocol }` | `active` | `none` | `none` | `unknownProtocol` — an additive protocol element blocks delivery until a safe thread status arrives |
| `Held { WaitingOnApproval }` | `active` | `human` | `permission` | `waitingOnApproval` |
| `Held { WaitingOnUserInput }` | `active` | `human` | `question` | `waitingOnUserInput` |
| `Held { NotLoaded }` | *withhold* | — | — | thread not loaded proves nothing about work |
| `Held { SystemError }` | *withhold* | — | — | see #264's catch-all defect |
| `Held { UnknownStatus }` | *withhold* | — | — | an unrecognized future status is not a terminal `systemError` and cannot authorize delivery |

`inputBuffer` is `unknown` from this producer: the control stream does not see
the composer. The projection test must be behavioral — a table that would pass
with every row mapped to `unknown` is not an oracle (#268 §B). The wrapper
honors st2's stop signal through every startup phase (socket connect,
initialize, thread binding), not only the bound monitor loop: a stop before
the TUI exists ends the launch gracefully and leaves no record — nothing was
observed — while a stop after it exits through the ordinary terminal-write
path — and the stop handler installs before ANY child is spawned, the
hook-trust preflight's detached app-server included, with the stop flag
polled between short socket timeouts through every startup wait (connect,
the initialize handshake, thread binding, and the preflight's projection
read), so a stop in that window cannot leak a server around a dead wrapper
or sit out a 30-second handshake. Observability never kills a
launch: a claim that cannot be written degrades to a token-only writer with
a warning, a TUI that fails to spawn writes a real terminal record (ended,
launch-error) over the claim placeholder, and a projected transition whose
record write fails does not count as evidence: it is retained as pending and
retried on EVERY pump pass (only the heartbeat is presence-cadence work), so
a stale on-disk state is never kept fresh in contradiction of the latest
observation.

## Claude producer (OHS-R05, OHS-R06)

Two cooperating writers. The hook side classifies turn lifecycle: a submitted
prompt or tool activity writes `active`; `Stop` writes `idle`; `StopFailure`
— which Claude fires *instead of* `Stop` when an API error ended the turn —
also writes `idle`, because the turn is over at the same lifecycle point, with
reason `providerAuth` for the credential class and `apiError` for every other
word in that closed vocabulary. It is deliberately not `ended`: the TUI is
still live, a human can re-login and carry on, and the wrapper owns this seat's
terminal record (OHS-T04). The event carries a second registered command, the
pre-existing wedge reporter, so both jobs run on one edge;
`PermissionRequest` writes `active` + `blockedOn: human` with its ask kind
classified from the payload's `tool_name` (`AskUserQuestion` → `question`,
anything else → `permission`) — (its meaning is
specifically "a human is about to be asked" — it fires only under permission
modes that ask), classifying its ask kind from the payload's `tool_name`:
`AskUserQuestion` is a `question`, anything else a `permission`. Events carrying `agent_id` are subagent-nested and never move
top-level state. The blocked *exit* edge is the next `PreToolUse`,
`PostToolUse`, or `Stop` — measured-correct, not merely conservative: the
2026-08-23 batched-permission capture (`DQ-H1`) shows tool execution
serializes around an open permission prompt, so no event can clear the
blocked state early. The residual limit is the eventless deny path pinned in
`DQ-H1`. The wrapper side owns liveness: it re-stamps the heartbeat on its
existing presence cadence while the child is alive — through a fresh writer
each time, so it never clobbers a state a hook process wrote in between —
and writes the terminal record from its `try_wait` reap and its SIGTERM
path, before any SIGKILL escalation into its own process group, which no
in-process write survives (OHS-T04). Hook registration has one canonical
shape (`hooks::claude_settings_registration`): the maintained example
declaration (`examples/native/agent-claude.kdl`) carries it by hand, and
`expand_claude` renders the same `.claude/settings.local.json` upsert for
driver-declared seats, so both surfaces register identical hooks and a test
fails if they drift. A live seat converges without disruption: render output
is not part of the launch fingerprint, the merged settings land on the next
materialization pass, and the hooks take effect at the next session start.
The registration merges with `arrays="union"`: user-declared hook entries
survive every materialization, while st2's own prior entries — recognizable
by the hook root they reference — are superseded on a hook-set upgrade
rather than accumulated. Hook writes carry the wrapper's exported
incarnation token, so a hook finishing after the wrapper reaped Claude
cannot replace the terminal record. Legacy `deliver "mcp"` seats render the same
canonical registration — they run under claude-session too, and a wrapper
that claims and ends a record nobody transitions would be worse than no
record. A hooks-only seat (the maintained hand-authored example launches
claude directly) gets transitions and blocked-on-you but no heartbeat owner
and no terminal record: its `SessionStart` performs the written claim so
session succession works — eligibility and the takeover are ONE act under
the record lock, so a hooks-only SessionStart racing a wrapper's startup
cannot steal its sequence, and a wrapper's fresh claim placeholder counts
as owned (an abandoned one ages into claimability); a wrapperless claimer
otherwise supersedes only fellow wrapperless tokens, real exit-bearing
terminal records, and staleness — while a live-but-idle seat still ages to
`unknown` and an exit leaves its last state to age out — indeterminate,
never wrong.

## pi producer (OHS-R05, OHS-R08)

The injected extension observes `agent_start`/`agent_settled` — a positive,
evented, in-process signal; the idle edge is deliberately `agent_settled`,
because `ctx.isIdle()` is still false through `agent_end` and a queued
follow-up turn starts exactly at that boundary (measured against the repo's
own pi captures). Ownership splits by phase rather than living in one
process: the channel owns the live record and its heartbeat, because it is
the one process that sees pi's turn events and its stdio EOF bounds its
evidence, while the wrapper owns only the terminal record — through a terminal-only
observer that never heartbeats but does write `ended` *before* the stop
path's SIGKILL escalation of its own group, the one write that makes an
escalated stop observable at all — because it is the
one process that sees the provider die — and the channel drops queued live
frames once a terminal record is on disk, so the wrapper's write is the
incarnation's last word. This is the first producer that satisfies root
`DQ2`'s "stronger evented signal" clause for any harness. pi's composer
offers nothing to scrape, so `inputBuffer` stays `unknown`.

## OpenCode producer (OHS-R08)

OpenCode's interactive TUI is also a server, so the driver needs no screen
observation at all (measured on 1.18.19 —
`.experiments/2026-08-23-opencode-surface.md`). The `opencode-session` wrapper
allocates a loopback port and a per-seat password, launches the TUI bound to
them, owns the presence lease and the observed-state record, and projects the
`/event` SSE stream:

| Signal | state | blockedOn | ask | reason |
|---|---|---|---|---|
| `session.status {type: busy}` | `active` | `none` | `none` | — |
| `session.status {type: retry}` | `active` | `none` | `none` | `retry` |
| `session.status {type: idle}` / `session.idle` | `idle` | `none` | `none` | — |
| `permission.asked` … `permission.replied` (same ask id; spelled `id` on entry, `requestID` on exit — measured) | `active` | `human` | `permission` | `permission` |
| `question.asked` … `question.replied\|rejected` (same ask id, same spelling split) | `active` | `human` | `question` | `question` |
| `session.error {ProviderAuthError}` | `ended` | `none` | `none` | `providerAuth` |
| `session.error` (other arms) | `idle` | `none` | `none` | `error:<name>` |
| child exit / stop path | `ended` | `none` | `none` | exit status |

A dedicated seat aggregates across the server's sessions: any busy session is
activity, any open ask is a human block, and idle requires positive level
evidence (`/session/status` omits idle sessions, so an empty map over a live
server is the idle proof, re-read on every SSE (re)connect). Asks open across
a reconnect are recovered from both pending listings — `GET /permission` and
`GET /question`, each measured on 1.18.19 — with their ids kept so the
id-matched exits still release them; the seed builds a fresh projection and
swaps it in whole only when the status level (which must be the documented
object shape — null or an array proves nothing), every status word, and both
listings all read cleanly — a listing entry without a readable id is a
pending ask the id-matched exit could never release, so it fails the seed
rather than being skipped. A mid-seed failure leaves nothing half-seeded, a
successful re-seed clears stale entries whose exits passed during the
outage, and otherwise evidence stays off and the seed retries. An
unrecognized `session.status` word on ANY session poisons the projection
outright — a tracked-busy entry can no longer be trusted to clear, and an
untracked session in a state this version cannot read makes standing idle
evidence a fabrication — so non-terminal observations are withheld (a
sticky terminal still outranks the poison) and evidence drops until a fresh
seed replaces the picture. An SSE drop marks the observation
stream interrupted, so the first post-reseed observation opens a fresh
transition rather than claiming continuity across the outage. The `/doc`
subset gate names every consumed arm, exit events and pending listings
included, so a release renaming an exit is refused up front rather than
holding `blockedOn: human` forever. A dropped stream stops the heartbeat, the
stream carries a silence horizon of at least twice the measured heartbeat
cadence so a stalled socket surfaces as a disconnect instead of keeping
evidence alive forever, and `inputBuffer` stays `unknown` — the `/tui/*`
surface is write-only.

The native delivery transport mirrors the Codex FIFO discipline: an
`Attempted` receipt persisted before transport, `POST
/session/<id>/prompt_async` with a stable caller `messageID` derived from
(identity, session, filename), and acceptance only when the exact message
reads back durably from the server. The `/tui/append-prompt` and
`/tui/submit-prompt` endpoints acknowledge input even with no TUI attached
(measured) and are therefore never a receipt. Delivery is fail-closed behind
two gates: a `SUPPORTED_OPENCODE_VERSIONS` pin and a live `/doc` OpenAPI
subset check naming every arm st2 consumes; observation runs behind the
`/doc` check alone, since its vocabulary already degrades to indeterminate.
Deliveries target the most recently observed session; a seat whose TUI has
not yet created one waits rather than creating sessions itself, and a session
that settled before the observer connected — invisible to events and to
`/session/status` alike — is recovered from the session listing when work is
pending. The status seed trusts exactly the pinned words (`busy`, `retry`,
`idle`) and fails closed on anything else, and the stop path rewrites its
pre-signal escalation cover with the exit the grace-window reap actually
observed.

## Native driver diagnostic snapshot (OHS-R11–OHS-R16)

```text
version gate -> API gate -> SSE -> seed -> provider auth -> delivery -> read-back
      \______________ typed failure/recovery transitions ______________/
                                |
                                v
              <agent-dir>/driver-diagnostic
                  st2.driver-diagnostic.v1
                                |
                 +--------------+---------------+
                 v                              v
       agents --json.driverDiagnostic     Doctor advisory
```

The native driver core, not OpenCode prose and not either consumer, owns the
closed model. The persisted newline-terminated JSON object is atomically
stage-and-renamed:

```json
{
  "schema": "st2.driver-diagnostic.v1",
  "driver": "opencode",
  "stage": "readBack",
  "reason": "notDurable",
  "source": "messageReadBack",
  "producerVersion": "1.18.19",
  "support": "supported",
  "observedAt": 1787690300000,
  "recovery": "clearsOnStageRecovery"
}
```

`producerVersion` is omitted when the version probe cannot produce one.
`support` is `supported | unsupported | unknown`; an unrecognized future word
is distinct from the literal supported vocabulary and makes the record
indeterminate. Unknown additive object fields are ignored. Unknown enum words,
a foreign schema/recovery contract, an empty driver, or a known reason paired
with the wrong stage/source are never accepted as failure evidence.

The closed stage/reason/source matrix is:

| Stage | Reasons | Sources |
| --- | --- | --- |
| `versionGate` | `versionProbeFailed`, `unsupportedVersion` | `versionProbe` |
| `apiGate` | `apiUnavailable`, `incompatibleApi` | `openApiDocument` |
| `sse` | `sseConnectFailed`, `sseDisconnected`, `unknownEvent` | `eventStream` |
| `seed` | `statusUnavailable`, `malformedStatus`, `unknownStatus` | `statusSnapshot` |
| `seed` | `permissionUnavailable`, `malformedPermissions`, `missingAskId` | `permissionSnapshot` |
| `seed` | `questionUnavailable`, `malformedQuestions`, `missingAskId` | `questionSnapshot` |
| `providerAuth` | `providerAuthRejected` | `turnResult` |
| `delivery` | `deliveryUnavailable`, `deliveryRejected` | `promptTransport` |
| `readBack` | `readBackUnavailable`, `notDurable` | `messageReadBack` |

One in-process publisher retains at most one current failure per stage and
persists the earliest stage in the table's execution order. Re-publishing the
same tuple is a no-op; it does not refresh `observedAt` or increment telemetry.
A stage success clears only that stage and atomically reveals the next
outstanding failure. Clearing the final failure removes the record. At a new
session, a measured stage success may clear a matching persisted failure from
the predecessor, so recovery never requires an st2 restart beyond the restart
that was itself needed to cross a static version/API gate.

OpenCode publishes and clears at the actual boundary, not at a catch-all error
renderer:

1. `opencode --version` resolves version support before launch.
2. `/doc` reachability and exact subset compatibility resolve the API gate.
3. `/event` connect/disconnect/silence and unknown event status resolve SSE.
4. `/session/status`, `/permission`, and `/question` resolve the atomic seed.
5. `prompt_async` transport reachability/status resolves delivery.
6. Exact message GET `200`/`404`/indeterminate resolves read-back.

Diagnostic persistence errors log and do not escape into those operations.
The existing attempted-before-transport receipt, same-message retry,
indeterminate-read-back no-resend rule, durable acceptance, and archive
behavior are unchanged.

Claude, Codex, and omp publish exactly one of those stages — `providerAuth` —
from their own typed turn-failure signal, and nothing else: every earlier
boundary is already fail-closed at admission for them (an incompatible Codex
protocol refuses the launch rather than degrading into an observation, an
unadmitted omp MINOR refuses it too under OMP-R05, and st2 gates no Claude
version at all). Every edge comes from the signal that ends a turn, so no
driver reads provider prose to decide this:

| Driver | Rejection | Recovery | `producerVersion` / `support` |
| --- | --- | --- | --- |
| Claude | `StopFailure` hook with `error: authentication_failed` — Claude's own name for every 401/403 provider response, fired *instead of* `Stop` | the next `Stop`, i.e. a turn that reached its ordinary end | omitted / `unknown` — a hook payload names no version and st2 gates none |
| Codex | `turn/completed` with `turn.status: failed` and `turn.error.codexErrorInfo: unauthorized` | `turn/completed` with `turn.status: completed` | the version the protocol gate admitted / `supported` |
| omp | a `type: "turn"` channel frame whose error carries omp's own `errorId` classification with `AuthFailed` set and `UsageLimit`, `AccountPolicy`, and `Transient` clear | a `type: "turn"` frame with no error, i.e. an `agent_end` whose last assistant message did not stop on an error | omitted / `unknown` — the wrapper, not the channel, owns the version gate |

No driver borrows the word for a neighbouring class: Claude's `rate_limit`,
`overloaded`, `oauth_org_not_allowed`, `account_on_hold` and `billing_error`,
Codex's `usageLimitExceeded` and `rateLimitExceeded`, and omp's `UsageLimit`,
`AccountPolicy` and `Transient` flags are capacity, policy, or account state
that a re-login cannot fix. Claude's `StopFailure` still writes `idle` with
reason `apiError` for them — the turn did end — Codex keeps its `systemError`
terminal, and omp's frame still writes `active` with omp's own bounded prose,
which is the only place a reader learns that a 403 was about credits. The Codex
startup gate pins `unauthorized` and both quota words present in
`CodexErrorInfo`, so a release that merged them refuses the launch instead of
letting st2 report an exhausted allowance as a rejected credential; omp needs no
such gate because its flags are separate bits that cannot merge, but it does
need the `Class` bit checked, because an unclassified `errorId` is a bare HTTP
status. Because these three publish only on rejection, an absent record is their
healthy steady state and Doctor advises nothing for it; OpenCode, which resolves
its version gate on every launch, still advises on absence.

The roster projection always has one fixed shape. `failure` fills every
evidence field; `absent` and `indeterminate` preserve the same keys with null
evidence and stable reader recovery text:

```json
"driverDiagnostic": {
  "status": "failure | absent | indeterminate",
  "driver": "opencode | null",
  "stage": "readBack | null",
  "reason": "notDurable | malformedRecord | unsupportedSchema | unknownVocabulary | futureSkew | null",
  "source": "messageReadBack | null",
  "producerVersion": "1.18.19 | null",
  "support": "supported | unsupported | unknown",
  "observedAt": 1787690300000,
  "evidenceAgeMs": 250,
  "recovery": "clearsOnStageRecovery | publishFailureOrClearOnStageRecovery | replaceWithValidRecordOrClearOnStageRecovery"
}
```

Doctor consumes the same `Observed` value and `repair_text` function for
declarations whose native driver is expected to publish this record (OpenCode
in this version). It emits an advisory for `failure`, `absent`, and
`indeterminate`; none changes Doctor's exit status. Roster retains the
explicit object for every declaration.

## Exposure (OHS-R09, OHS-R10, OHS-R14)

`st2 agents --json` (both forms) appends one field per row:

```json
"observedState": {
  "state": "active",
  "blockedOn": "human",
  "inputBuffer": "unknown",
  "ask": "permission",
  "harness": "codex",
  "since": 1787690000000,
  "reason": "waitingOnApproval",
  "exit": null
}
```

`null` when no record exists. The derivation above is already applied — a
consumer never re-implements staleness. Roster reads pass the same-host
liveness probe only for agents whose resolved host is this host, resolving
the pty root exactly as the runner does (`PTY_ROOT`, else the catalog's own
pty root; the legacy `PTY_SESSION_DIR` is deliberately not honored — the
runner never uses it, and probing a directory st2-managed sessions never
touch turns provable deaths into indeterminate reads). `status`,
`desiredState`, and `lastActivity` keep their exact meanings; the three
full-string pinned assertions and the stable-roster invariant wording are
updated deliberately in the change that adds the field. Doctor prints an
advisory (not a failure) for an owned agent whose record is stale,
session-dead, or `ended` while desired state is `running`.

## Verification plan

The invariant rows this subsystem must add or amend when implementation lands,
each only once a real test proves it (per `CLAUDE.md`):

- **Scoped delivery-input watching** — a write to `harness-state` does not
  wake the Codex delivery pump; an inbox write still does. Test in the shape
  of `src/ding/mod.rs::idle_ding_does_not_spin_on_its_own_inbox_reads`.
- **Stable roster JSON** (existing row) — wording gains the third axis;
  `src/agents.rs::agents_json_has_stable_wire_shape` is edited in the same
  change and stays the proof.
- **Observed harness state discipline** — derived-only `unknown` with distinct
  reasons, byte-distinct writes, no-mtime staleness, terminal-write-before-
  escalation. Proving tests live in `src/harness_state.rs` today (11 tests)
  plus the planned per-producer suites; the SIGKILL-mid-turn test (`ended`,
  not `active`) gates the teardown row.
- **Native-driver diagnostic discipline** — exhaustive closed vocabulary,
  wrong-pair/unknown/malformed/additive decoding, stage-priority recovery,
  OpenCode delivery/read-back recovery, exact roster wire, Doctor agreement,
  and bounded telemetry labels are proved by focused unit/integration tests in
  `src/driver_diagnostic.rs`, `src/opencode_session.rs`, `src/agents.rs`,
  `src/metrics.rs`, and `tests/doctor.rs`.

## Open design questions

Tracked with context in [open-questions.md](./open-questions.md): DQ-H1 Claude
blocked-exit edge, DQ-H2 transport cost, DQ-H3 `child` producer, DQ-H4
ungraceful-death coverage, DQ-H5 supervisor-following, DQ-H6 OpenCode state
source.
