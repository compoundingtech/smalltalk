---
date: 2026-06-28
audience: humans + agents trying to understand the shape of smalltalk
purpose: name the actor-model framing the system is built on, so design decisions stop being ad-hoc
---

# Smalltalk as an actor model

The project's new name is a nod to the original Smalltalk — Alan Kay's
language built around objects-that-pass-messages. That framing is also
the right framing for what this project actually is: **a network of
agent-actors that coordinate exclusively by passing messages through
each other's mailboxes.** No shared mutable state, no RPC, no remote
function calls. Just files in folders, observed and rewritten by their
owners.

If you've internalized the actor model, the rest of the system shape
follows mechanically. This doc names the mapping.

## The mapping

| Actor concept             | Smalltalk realization                                    |
|---------------------------|----------------------------------------------------------|
| Actor                     | An identity — `<root>/<name>/`                           |
| Mailbox                   | `<name>/inbox/`                                          |
| Processed history         | `<name>/archive/`                                        |
| State visible to peers    | `<name>/status` (a single token: available, busy, …)     |
| Message send              | Write a file into the recipient's `inbox/`              |
| Message receive           | Read a file out of your own `inbox/`                    |
| Message ack / "consumed"  | `mv inbox/X.md archive/X.md` (rename, never delete)     |
| Reply                     | Send a new message with `in-reply-to: <filename>` FM    |
| Encapsulation             | "Across identities, only `inbox/` is writable."         |
| Location transparency     | rsync (or any folder-sync) moves bytes between machines |

That's the whole system. Every behavioral property below is downstream
of this mapping.

## The encapsulation rule

> Across identities, only `inbox/` is writable.

This single rule is the actor encapsulation invariant. An identity's
`archive/`, `status`, and any future-private state is **owned**: only
that identity rewrites it. Peers read freely (via sync), but can never
mutate. The system has no permission daemon — the rule is a convention,
enforced by the agents themselves and verified post-hoc by sync
behavior (rsync replays whatever the owner wrote, ignoring outside
edits if they're configured to).

What this buys:

- **No locks.** Two senders writing to the same recipient never
  collide — each write is a fresh `<unix-ms>-<rand6>.md` filename.
  Receiver-side processing is single-writer by definition.
- **No authorization model.** "Can agent X mutate agent Y's state?"
  is a non-question — only Y mutates Y. The only authorization
  question is "can you write to anyone's inbox?" which is currently
  "yes if you can reach the filesystem."
- **Sandboxable identities.** Mount `<root>/<identity>/` into a
  container and that's the whole API surface the identity needs.
  No broker, no daemon, no shared key.
- **rsync is the transport.** Replicating an identity is `rsync -a`.
  Anything that surfaces a writable folder works as transport —
  rsync, Syncthing, NFS, S3-via-ZeroFS, a shared volume in a
  multi-container compose file.

## Asynchrony is the default

Actors don't synchronize. They send a message, then move on. The
sender doesn't block on the receiver reading or replying. This system
inherits that: `coord_msg_send` returns the moment the file is
written; the recipient sees it whenever sync delivers and their
process notices. There is **no synchronous "wait for reply"** — if
you want a reply, you send a message and either keep going or pause
on something else (Claude Code's channel notifications, a tail, a
human reading) until the reply arrives.

This is why "Coord threads stay on coord" matters (see
[`src/mcp/capabilities.ts`](../src/mcp/capabilities.ts) →
`CHANNEL_INSTRUCTIONS`): replying via `coord_msg_reply` keeps the
correspondence in the actor channel. Replying via the REPL (where
nobody is listening by default) breaks the actor abstraction — you
shouted into a room with no microphone.

## What's NOT in the actor model (yet)

The original Smalltalk had supervision, become-style hot upgrades,
and rich message-pattern matching. We have none of that yet. The
system today is the bare minimum that satisfies the encapsulation
rule:

- **No supervision tree.** Agents don't restart each other. `pty`
  supervises sessions, but the actor-vs-actor relationship is flat
  — no parent owns a child.
- **No selective receive.** An agent reads its own inbox in
  filename (= timestamp) order; there's no pattern-match dispatch.
- **No code update message.** "Become a new version of yourself"
  is `pty kill && pty up`, not a message.

These are deliberate omissions. The minimum was where Smalltalk
started in 1972 too — the project gets to add primitives only when
the workload demands them.

## Why the framing matters

Whenever a design question comes up — "should agent X be able to do
Y to agent Z?", "where should state Q live?", "what's the right
shape for new feature R?" — the actor framing answers it:

- **If it's shared mutable state, it can't exist as-is.** Either it
  becomes a single actor's owned state that others query by sending
  messages, or it becomes a sidecar inside one actor's folder.
- **If it's an RPC, it should be two messages.** Don't add `await
  callRemoteAgent(...)` — send a message, let them reply.
- **If it touches a peer's folder, it's wrong.** Re-shape it as
  something the peer does to itself in response to a message.

The few times we've broken this rule in the codebase, we've eaten
real bugs (cross-agent file rewrites during sync convergence). The
rule is load-bearing. Keep the framing in mind and most design
choices become obvious.

## Further reading

- Carl Hewitt's original actor papers (1973, 1977). The Wikipedia
  page on the actor model is a fine summary.
- [`LAYOUT.md`](../LAYOUT.md) — the data shape the actor mapping
  produces.
- [`IDEA.md`](../IDEA.md) — the older philosophy notes that this
  doc consolidates and renames.
- [`notes/agent-roles.md`](agent-roles.md) — how actors are
  organized into manager/worker roles in practice.
