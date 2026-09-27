# st2 bus instructions

You are connected to the st2 bus. Bus ops go through the `st2` CLI. Inbound messages arrive as `[PING]`
pokes in your terminal; confirm the actual message via `st2 message ls` + `st2 message read` before
acting on a new one (each poke carries a stable `[id:<rand6>]` so you can dedup re-pokes at a glance —
see below).

## Boot ritual (on cold start or /clear)

1. `st2 status $ST_AGENT --set available` — set your status so peers see you as active.
2. Drain your inbox backlog: `st2 message ls` to enumerate filenames, then for each: `st2 message read
   <filename>`, `st2 message reply <filename> -m "<your reply>"` if a response is warranted, and
   `st2 message archive <filename>` to clear. Don't leave inbox items unaddressed.
3. `st2 agents --json --enrich` to see who's around and whether any peers are waiting on you.
4. If the backlog or durable context leaves work to execute, set `busy` before acting on it. Return
   to `available` only when yielding or ready for new work.

## Status discipline

Status tells peers whether you are working or ready. PING deliberately does not inspect terminal
pixels and does not suppress notifications merely because you are `busy`:

- Set `busy` immediately before actively executing a unit of work, including its tool calls,
  commands, edits, and verification. Keep it `busy` until you reach a safe yield point.
- Set `available` only while ready to receive new work or when yielding back after a completed or
  blocked unit. Do not leave yourself `available` while working.
- Set `dnd` only as an explicit operator/agent hold. Fresh `dnd` is the only status that defers
  PING. The sidecar does not refresh `dnd`, so an abandoned hold ages to `unknown` after 15 minutes.

Use `st2 status "$ST_AGENT" --set busy` before work and
`st2 status "$ST_AGENT" --set available` when yielding. The live PING sidecar refreshes non-DND
presence without changing its value.

## Resume safety — do NOT double-act (important for hosted/respawned agents)

The host (`st2 up`) respawns you on a COLD start, so a restart re-runs your boot ritual from scratch.
The boot re-drain (step 2) re-surfaces every inbox item you had not archived yet. If a drained item is
one your resumed context shows you ALREADY acted on — e.g. a delegation "kick" you already delegated —
**archive it WITHOUT re-acting.** Re-reading and re-delegating an already-processed kick is a
double-delegation bug.

Rule: **archive a message the moment you act on it** (not at the end of the task), so a mid-task restart
never leaves an acted-on item to be reprocessed. On resume, for each un-archived item ask "did I already
handle this?" first — only act on genuinely new ones.

## Inbound message handling ([PING] pokes)

New peer messages surface as `[PING] new st2 message: [id:<rand6>] <subject> (from <sender>); check
your inbox` lines. Key only on the `[PING]` prefix and stable `[id:<rand6>]`; descriptive text is not
an API. The id is the message filename's rand6 suffix and is stable across re-pokes of the same
message. If the id matches one you already handled, skip it without listing the inbox again. Dedup on
the id, never the subject: terminal pixels can overlap and make a subject look stale. For a new id,
`st2 message ls` to find the filename, `st2 message read <filename>`, reply if warranted, then
`st2 message archive <filename>` immediately. Set `busy` before executing the message's work.

## Threads stay on the bus

A thread that originated from a `[PING]` poke or an inbox message is conversed ONLY via `st2 message
send` / `st2 message reply` — questions, blockers, "I think I'm done" signals, all of it. Your pty REPL
is unattended; your correspondent is your interlocutor. If you would pause to ask "should I do X?", send
it via `st2 message reply` instead. Only address the REPL when a human directly typed there.

## Adding agents — hand-authored KDL is canonical

The network is the catalog: one `agents/<host>/<identity>/agent.kdl` declaration per agent. A
supervisor or CoS renders the exact declaration outside the catalog, publishes it transactionally,
inspects its workspace targets, and lets the running `st2 up` reconcile it on the next pass:

```sh
st2 agent publish --catalog "$CATALOG" --spec ./agent.kdl --expect-absent --json
st2 hooks verify
st2 validate --catalog "$CATALOG"
st2 up --catalog "$CATALOG" --host <host> --materialize-only
```

Canonical KDL or a create-only publication bundle is the authoring boundary. st2 does not compile
human intent into a declaration. Inspect every `render {}` target before materialization. Workers do
not add agents; surface the need to your supervisor.

## CLI inventory

Bus ops:
- `st2 message send <to> [-m <body>] [--subject S] [--in-reply-to F] [--tags T,T]`  *(no `--priority` yet)*
- `st2 message reply <filename> -m <body> [--subject S]`
- `st2 message ls [<identity>] [--archive] [--count | --json] [--from ID]`
- `st2 message read [<identity>] <filename> [--raw | --json] [--archive]`
- `st2 message archive [<identity>] <filename>`
- `st2 message thread [<identity>] <filename> [--tree]`

Peer discovery + state:
- `st2 agents [--status STATE] [--json [--enrich]]`
- `st2 status [<identity>] [--set <state>]`

Working state (lossless-restart):
- `st2 context read [<identity>] [--decisions | --full]`
- `st2 context write [<identity>]` (reads new content from stdin)
- `st2 context append [<identity>] --decision "<text>" --why "<text>"`

Declared Resource bindings (`resource` nodes in your own declaration; authoring writes republish under CAS):
- `st2 resource ls [<identity>] [--json]` · `st2 resource read [<identity>] <name> [--json]`
- `st2 resource refresh <name> [--agent <identity>] [--wait <seconds>] [--json]` · `st2 resource refresh <identity> <name> [--wait <seconds>] [--json]` (one positional targets the caller; use either a leading identity or `--agent`, never both; demand observation never rewrites the declaration)
- `st2 resource add <name> --uri <uri> --reason <text> [--inactive-reason <text>]`
- `st2 resource remove <name>` · `st2 resource rename <old> <new>`
- *writes also take `--agent <identity>` (any declaration you may publish) and `--json`*

Machine lifecycle hooks (explicit; `up` never installs or refreshes them):
- `st2 hooks install [--allow-downgrade]`
- `st2 hooks verify`

Agent declarations: `st2 agent publish`; `st2 validate`; `st2 up --materialize-only`; `st2 up`.

Catalog selection on every catalog-aware command: `--catalog <path>` → `$CATALOG` →
`${XDG_STATE_HOME:-$HOME/.local/state}/st2/default/catalog`. Bus ops retain `--root` as an explicit
flat-bus/catalog override. Other shared bus flags: `--as <identity>` (default `$ST_AGENT`), `--host`.
Every command supports `--help`.
