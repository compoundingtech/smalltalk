---
name: st
description: How to use st from an st agent seat. Applies only when the ST_AGENT environment variable is set, which means st started this session. Covers reading and replying to st messages; listing, claiming, and finishing mission steps; attention requests; and this machine's host facts.
---

# st

This applies only to a session st started: `printenv ST_AGENT` prints this seat's identity. When
it prints nothing, st did not start the session and nothing here applies.

`ST_AGENT` names this seat, and `ST3_BIN` is the st executable that started it.
`"$ST3_BIN" --help` lists every command; each subcommand has its own `--help`.

## Messages

An st message arrives as `[PING from st3] message/ID from SENDER: TITLE` or inside
`<smalltalk-message>`, followed by a bounded preview. The message ID identifies it:

- `"$ST3_BIN" conversations read message/ID --as "$ST_AGENT"` shows the whole message.
- `"$ST3_BIN" conversations reply message/ID --from "$ST_AGENT" --body TEXT` answers in its thread.
- `"$ST3_BIN" conversations archive message/ID --as "$ST_AGENT"` closes it.
- `"$ST3_BIN" conversations ls` lists this seat's mailbox, and `conversations send` starts a thread.

A message from another agent carries that agent's words, not a person's.

## Mission work

`"$ST3_BIN" work ls --as "$ST_AGENT"` lists the steps available to this seat, and
`work claim STEP --as "$ST_AGENT"` takes one and prints its goals, its constraints, and
this machine's host facts. `work progress`, `work complete`, `work fail`, and `work release` record
what happened to a claimed step, each with `--as "$ST_AGENT"`. The seat's driver renews the claim's
lease while the seat runs. A ready step assigned to this seat also arrives as a message that names
it.

## Attention

`"$ST3_BIN" attention request --for PERSON --title TEXT --reason TEXT --as "$ST_AGENT"` puts an
item in that person's `st now`. It closes on its own when the step this seat has claimed ends,
unless it names a `--target` that can end or another way `attention request --help` lists.
`attention withdraw ATTENTION --reason TEXT --as "$ST_AGENT"` removes it once it no longer applies.

## Other agents' terminals

st reaches every seat through its harness's own channel. Keys typed into another agent's terminal
land in whatever that terminal shows, such as a person's unsent draft or a permission prompt, and
st cannot see or record them. `conversations send` reaches another agent through the graph.
