# Running st with omp

An omp seat is an ordinary st agent. Declare its workspace, harness, model, and effort in KDL:

```kdl
agent "worker" {
  workspace "${ST_WORKSPACE}"
  harness "omp" {
    model "openai-codex/MODEL"
    effort "medium"
  }
}
```

st validates the installed omp version before launch and supplies the channel extension, session
directory, and boot prompt. The launcher runs under `bun`, which must be available in the daemon's
login-shell environment. The seat uses the provider login of the user running the daemon. Confirm
the selected model in the session transcript; omp may resolve an unavailable model name to another
one.

The session JSONL is stored in st's driver state for the seat. The `model_change` entry and each
assistant turn identify the selected provider and model. omp's Python `eval` tool has a filtered
environment; run st commands from its shell tool so `ST_AGENT`, `ST3_BIN`, and `ST3_ENDPOINT` are
available.

An authored `args "--resume" "/absolute/path/<time>_<uuid>.jsonl"` (or `--resume=…`) keeps its
original argv and transcript. On a fresh native omp/pi driver start, st links an absent or empty
managed `provider-sessions` directory to the transcript's parent after checking the filename UUID
against the session header in the first two lines. It never copies the transcript: omp continues
appending to the authored path, and the channel must see those same bytes to publish its native
session binding. Already-correct links and inventories containing the same inode are left alone.
Relative or missing paths, invalid filenames or headers, non-empty inventories and foreign
(including dangling) directory links are left untouched; the driver log records a typed
`authored_resume_link_skipped` reason. A legacy directory may contain several sessions; the channel
still looks up the reported UUID. This does not backfill an already-running driver's binding.
Conversation readers use the current incarnation's exact native binding (session ID and path),
not the newest sibling in that directory. An unreadable bound transcript, or a linked inventory
without a current binding, stays unavailable rather than displaying another seat's conversation.

Messages delivered during a running turn are held until the current tool batch returns. A tool call
that runs longer than the hold limit can still be backgrounded. Read the exact graph message with
`st conversations read` before acting on it, and archive it after the related action completes.

The channel reports idle only after `ctx.isIdle()` proves that the native turn has settled.
It keeps sampling through slow final unwind rather than abandoning the idle edge after a
timeout. New activity, session replacement, or channel replacement retires the old sampler;
an outstanding human ask or approval keeps its blocking observation, including on reconnect.

## Channel telemetry

The managed channel forwards `timeline` and `context` frames to the shared pi-family
normalizers. Response usage comes from `timeline` / `message_end`, including the provider's
token buckets, model, and reported cost; context occupancy is a separate measurement.
A `turn` frame carries a terminal result, not usage: provider errors become timeline errors.
`session` and `ready` frames report the native session ID and transcript for resume.

Native transcript tool results use the message-level `toolCallId` and `isError` to produce
one typed result paired with its exact call, preserving composite IDs and failure status.
Text blocks within that result remain its content, not separate plain tool messages.

Visible native OMP `custom_message` records are content entries with
`body.media_type = "application/vnd.omp.event+json"` and `body.text` containing this JSON envelope:

```json
{
  "_tag": "OmpEvent",
  "version": 1,
  "kind": "irc:incoming",
  "attribution": "agent",
  "content": "An incoming message",
  "details": {"from": "worker", "message": "An incoming message", "id": "message-id"}
}
```

`kind`, `attribution`, `content`, and `details` retain the native values; missing values are
`null`. The timeline role remains `system` as a transport role. Consumers derive provenance
from the envelope, not display-text heuristics: IRC senders come from `details.from`; async
task identities stay in `details.jobs` with their native `type`, `label`, and `id`. Mixed or
multiple jobs remain one native event unless their body has an authoritative partition.
Human, daemon, system, and absent attribution are not inferred to be agents.
Structured resource observations retain their content and details in the same envelope.
`display: false` stays hidden; an absent `display` retains the native reader's visible behavior.
The existing native-value byte bound applies separately to content and details; oversized
fields become marked truncated text without corrupting the envelope's JSON.
Timeline page limits are item-count upper bounds: a page can contain fewer entries to stay
within the gateway response budget. Pagination preserves each complete envelope and advances
the continuation cursor by the number actually returned; following it does not skip entries.
Consumers must opt into this media type. Legacy conversation adapters may omit these system
entries rather than render their JSON.
Pi's existing plain custom-message projection is unchanged.

The observed administrative entry types `model_usage`, `title_change`, `session_init`, and
`ttsr_injection` use the same envelope rather than an unrecognized-entry text fallback.
Their `kind` is the native entry type, `content` is `null`, and `details` contains the bounded
native record, including its IDs and timestamp. Native attribution is retained when present;
otherwise it stays `null`. The timeline timestamp uses the same native/predecessor rule as
conversation entries. `display: false` remains hidden.
Existing bookkeeping (`session`, `model_change`, `custom`, `credential_pin`, and `title`)
stays hidden; compaction still contributes its summary. This whitelist applies only to OMP:
Pi and genuinely unknown entry types retain their existing behavior.

`crates/st3/fixtures/omp-admin-events.jsonl` preserves one native shape per administrative kind
from a session resumed on 2026-10-04. Native IDs and timestamps remain exact, but every payload
string and numeric usage value is redacted. No bootstrap prompt, injected rule, task, model
label, or tool name from that capture is published.

`crates/st3/fixtures/omp-native-events.jsonl` contains a redacted 2026-10-01 OMP capture:
the native tool call/result IDs and error flag, plus an incoming IRC event's envelope.
Tool arguments, result text, sender name and incoming text are redacted; no credentials,
machine paths, or host metadata are retained. Task-job and structured-resource boundary
cases are synthetic regression inputs, not additional claims about that capture.

Unsupported frames, including `pre_compact`, produce a structured debug event with
`frame_type` and `observer_enabled`; frame payloads are not logged. When the managed observation
outbox is unavailable, unforwarded `timeline`, `context`, and `turn` frames produce the same
diagnostic. The managed route does not perform the standalone channel's pre-compaction context
recovery stub.

## Interrupted ask bridge

Native session continuation belongs to the shared driver mechanism: the daemon names a relaunch's
session in `ST3_NATIVE_CONTINUE_SESSION`, and `native_resume::continued()` selects that exact
session when available. Suspension resume uses `native_resume::requested()` and refuses if it cannot
bind the named session. The bridge below inspects only the OMP session successfully selected by
one of those paths. Without a selected session, including a `native-continue-unavailable` fallback,
there is no bridge; it never chooses a transcript or session itself.

OMP 18.4.10 needs a temporary, token-free recovery bridge for an open `ask` picker:
`arn:lmig:smalltalk:2026-10-02-omp-ask-resume-bridge`
([migration registry](https://app.notion.com/p/OMP-interrupted-ask-resume-bridge-st3-3ede3d41f4a3818a9e37ec160c006bbf)).
When the selected transcript has an unresolved last ask, st first runs a bounded RPC heal pass
with closed stdin, then launches the interactive session with `ST3_OMP_PENDING_ASK` naming its
pending toolCall id. `--no-extensions` disables discovery, but a fleet launcher can still prepend
explicit `-e` extensions. The heal clears `PTY_SESSION`, `PTY_ROOT`, `PTY_SESSION_DIR`, and inherited
`ST3_*`, `ST2_*`, and `ST_*` environment variables (except the launcher's required `ST_AGENT`
identity) so those extensions cannot reach the seat's PTY or st channel. The bridge's transcript
read logs I/O failures, fails open, and decodes bytes lossily; read, heal, retry, or diagnostic
failures do not stop native continuation.
The extension holds mailbox delivery, waits for its ready frame and first idle, and requests one
raw F5 from the Rust channel. For OMP's native interrupted result, the same toolCall id, questions,
and options reopen without a model turn or human action. Delivery resumes when that ask starts
again, or after a 120-second timeout with a diagnostic. Both exits send `delivery_ready` to release
the Rust channel's first-idle delivery gate independently of activity; a reopened picker remains
blocked on the person, not falsely idle. Shared native continuation does not depend on the bridge.

The contraction criterion is behavioral, not a version equality assertion. The standalone
`crates/st3/fixtures/omp-resume/native-reopen-probe.py` copies its adjacent public canary fixture into
a disposable native session directory, renders native resume in a PTY under an empty HOME with no
auth or extensions, and never sends F5 or a prompt. It requires `OMP_BIN` pointing at the raw pinned
OMP executable (not a fleet launcher); `--omp` overrides that variable. Optional `--fixture`,
`--timeout` (seconds, default 30), and `--screen-out` select the capture and output. Exit codes:
**0** means the F5-to-retry UI rendered without native reopening (bridge still needed);
**1** means the original picker reopened natively (contract this migration);
**2** means probe error, not evidence either way.

Smalltalk does not provide OMP in its checks: the native Rust test runs this probe only with
`OMP_BIN` set and otherwise prints an explicit skip. The hermetic gate belongs to the dotfiles
OMP pin, `flakes/external/omp`: run the same script and fixture verbatim as a flake check on
**every** OMP bump, including patches. That is where native reopening must fail the bridge-retaining
check with `arn:lmig:smalltalk:2026-10-02-omp-ask-resume-bridge`; remove the marked bridge when it
fires. The fixture keeps the canary prompt, actual tool arguments and interruption records, but
no machine paths, hosts, credentials or usage/account metadata.

The detector treats an error tool result for the pending toolCall id as interrupted when its text
starts with `Previous OMP process exited before this tool returned`, or when it follows a
`custom/session_exit` whose `pendingToolCalls` lists that id, with no intervening user or
non-aborted assistant message. Other results, including a person's cancellation without that exit
context, answer the ask. The bridge does not change shutdown timing.

OMP 18.4.10 can persist `Ask input was cancelled` after a signal ends a picker which F5 already
reopened. Direct `--resume` plus F5, both with and without a preceding heal pass, has been observed
to reopen that cancelled ask under the same toolCall id with no model turn.

The isolated managed smoke on 2026-10-03 still reported `Nothing to retry` on the second relaunch,
with policy, PTY-events, and status explicitly loaded and the heal's PTY/channel environment
cleared. Its transcript ended on the cancelled toolResult **without a following aborted assistant
boundary**. Bare `--resume` plus F5 on an unchanged copy failed the same way: OMP 18.4.10's retry
predicate requires a failed/aborted assistant tail, looking past synthetic results. In contrast,
standalone SIGTERM runs, both bare and with those fleet extensions, persisted an aborted assistant
boundary after the cancellation. The same missing-boundary shape occurred with the st3 channel
loaded when only the native OMP PID received SIGTERM, so it is not solely the heal environment or
managed process-group stop. A standalone extension registering only an async, no-op `context`
handler reproduced the missing boundary; a no-op `tool_result` observer did not. In OMP 18.4.10,
[`emitContext`](https://github.com/can1357/oh-my-pi/blob/v18.4.10/packages/coding-agent/src/extensibility/extensions/runner.ts#L1935-L2008)
throws on the aborted signal when context handlers exist. That abort occurs before the next
provider stream; the aborted-error catch in
[`Agent`](https://github.com/can1357/oh-my-pi/blob/v18.4.10/packages/agent/src/agent.ts#L2060-L2064)
emits only `agent_end`, not the `message_end` needed to persist the aborted assistant boundary.
The st3 channel registers such a context handler for read receipts. These captures establish a
context-handler-dependent native persistence failure, not a general limit on reopening cancelled
asks. No transcript rewrite, new toolCall id, model turn, context-handler removal, or
shutdown-timing workaround is applied.
