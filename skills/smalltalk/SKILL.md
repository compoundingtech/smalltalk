---
name: smalltalk
description: The smalltalk message-bus + agent-status layer, driven by the `st` (long form `smalltalk`) CLI. Reach for it whenever you need to COMMUNICATE with another actor on this machine — send or read a message, reply to a `[DING]` poke, coordinate work, deliver a result, ask a blocker, or check who's around and their status — instead of printing to your own (unattended) terminal. Also covers your lossless-restart working state (context).
when_to_use: Use when a task involves talking to another agent or human on the bus (send/reply/ls/read/archive a message), reading or setting agent status, or reading/writing your own restart context. NOT for spawning agents (that is convoy) and NOT for wrapping a terminal session (that is pty).
---

# smalltalk (the `st` bus)

**What it is.** smalltalk is the file-folder message bus + agent-status layer
for humans and agents on this machine. The CLI is `st` (long form
`smalltalk`). A message is just a markdown file in `<agent>/inbox/`; sending
*is* writing that file. No server, no schema.

**When to reach for it.** Any time you need to *talk to another actor* —
deliver a result, ask a blocker, reply to a `[DING]` poke, check who's around
or a peer's status. Use `st`, not your own terminal output: your REPL is
unattended, so a message you never send is work that silently halts. (Spawning
agents is `convoy`; wrapping a session is `pty` — different tools.)

## The idiom

1. **Boot ritual** — set status, then drain your inbox:
   `st status $ST_AGENT --set available`, then `st message ls` and for each
   file `st message read <file>` → `st message reply <file> -m '<reply>'` if a
   reply is warranted → `st message archive <file>`.
2. **Reach a peer** — `echo 'hi bob' | st message send bob --subject hello`
   (or `st message send bob -m 'body'`).
3. **See who's around** — `st agents --status available`; **your status** —
   `st status --set busy`.
4. **Keep restart state** — `st context write` (rewrite `now.md` from stdin) /
   `st context append --decision '...' --why '...'`.

## Message economy

Sending a message wakes the recipient's whole agent loop — a full turn of
reading, reasoning, and acting, on both ends. Communicate what the work
needs — a blocker, a question you can't resolve yourself, a decision or
closure to hand off, info the recipient must have to act — then stop.
Batch related points into one message instead of a flurry. Skip pure acks
("got it" / "thanks"), status with no ask, and anything they already know.
A message that needs no action needs no reply — just archive it. The test:
would this change what the recipient does? If not, don't send it.

## Footguns (hard-won — these bite)

- **Backticks in `-m "..."` are shell command-substitution.** A double-quoted
  body containing backticks runs them as a command and mangles the message.
  Fix: single-quote the body (`-m '...'`), or pipe it via stdin / a here-doc /
  a body file.
- **A scripted send can hang on a blocking stdin.** When `-m` is omitted the
  body is read from stdin; a stdin that never reaches EOF (an inherited pipe)
  blocks forever. Append **`</dev/null`** to any scripted
  `st message send`/`reply` as cheap insurance. (Current builds add a timeout,
  but the habit costs nothing.)
- **Delivery is at-least-once.** The ding re-scans your inbox and can re-poke
  an item you haven't archived. **Archive the moment you act** on a message —
  not at the end of the task — or a restart will re-surface (and you may
  re-do) it.
- **Threads stay on the bus.** A thread that began from a `[DING]` or inbox
  message is answered *only* via `st message reply` — questions, blockers,
  "I think I'm done", all of it. Your correspondent is your interlocutor, not
  your REPL.

## The exact surface

Run `st --help` (lists every subcommand with a one-line purpose) and
`st <subcommand> --help` (usage, every flag, and a concrete example).
`st --version` prints `<semver>+<short-sha>`. See `LAYOUT.md` for the on-disk
data format.
