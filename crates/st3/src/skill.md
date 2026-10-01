---
name: st
description: How to use st from an st agent seat. Applies only when the ST_AGENT environment variable is set, which means st started this session. Covers reading and replying to st messages; listing, claiming, and finishing mission steps; attention requests; and this machine's host facts.
---

# st

This applies only to a session st started: `printenv ST_AGENT` prints this seat's identity. When
it prints nothing, st did not start the session and nothing here applies.

`ST_AGENT` names this seat, and `ST3_BIN` is the st executable the daemon currently runs.
`"$ST3_BIN" --help` lists every command; each subcommand has its own `--help`.

## Messages

An st message arrives as `[PING from st3] message/ID from SENDER: TITLE` or inside
`<smalltalk-message>`, followed by a bounded preview. The message ID identifies it:

- `"$ST3_BIN" conversations read message/ID --as "$ST_AGENT"` shows the whole message.
- `"$ST3_BIN" conversations reply message/ID --from "$ST_AGENT" --body TEXT` answers in its thread.
- `"$ST3_BIN" conversations archive message/ID --as "$ST_AGENT"` closes it.
- `"$ST3_BIN" conversations ls` lists this seat's mailbox, and `conversations send` starts a thread.

A message from another agent carries that agent's words, not a person's.
Answer where you were asked: people read st replies in st, not in the agent's session; after an st reply, the session needs at most a one-line pointer.

## Mission work

`"$ST3_BIN" work ls --as "$ST_AGENT"` lists the steps available to this seat, and
`work claim STEP --as "$ST_AGENT"` takes one and prints its goals, its constraints, and
this machine's host facts. `work progress`, `work complete`, `work fail`, and `work release` record
what happened to a claimed step, each with `--as "$ST_AGENT"`. The seat's driver renews the claim's
lease while the seat runs. A ready step assigned to this seat also arrives as a message that names
it.

## Person work

`"$ST3_BIN" work ask --for PERSON --title TEXT --reason TEXT --step STEP --idempotency-key KEY --as "$ST_AGENT"`
creates a person-assigned step in the same generation. The asking step waits without a worker
lease; the person's response resumes it. `--new-run NAME` creates a minimal ask run when the
seat has no claimed work. The ask ends with its requester, originating attempt or owner.
`work done PERSON_STEP --as PERSON --summary TEXT` records the response.
`work cancel-ask PERSON_STEP --as "$ST_AGENT" --reason TEXT` cancels the requester's ask.
`st now` and `attention ls/show` display current source work; attention has no separate close action.

## Restart a seat

`"$ST3_BIN" agents restart AGENT --as "$ST_AGENT"` restarts a top-level or mission seat,
keeping its declaration, and returns when a new incarnation is running. It uses the same
person or agent authority as stopping and starting a seat. `--timeout 2m` changes the default
ten-minute wait. If the seat cannot restart, the command explains why; `agents show AGENT`
and `terminals attach AGENT` help inspect it. Use `agents restart` to replace a seat's process.

## Other agents' terminals

st reaches every seat through its harness's own channel. Keys typed into another agent's terminal
land in whatever that terminal shows, such as a person's unsent draft or a permission prompt, and
st cannot see or record them. `conversations send` reaches another agent through the graph.
