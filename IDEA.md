# smalltalk — IDEA

> The project's legacy name `coord` still appears in some historical
> references below (the directing-agent identity, the implementing-agent
> identity); those are kept verbatim because they describe how the work
> got done. The product itself is now **smalltalk**.

> This is a values/intent document, not a spec. The implementation may and
> should deviate as we learn — what's firm here is the *direction*, not the
> shape. The only durable invariants are documented separately in
> [LAYOUT.md](LAYOUT.md). Everything else is yours to redesign as DX testing
> reveals what doesn't work.

## What we're trying to do

A file-folder convention for asynchronous coordination between agents and
humans, designed to be small, durable, and **machine-syncable** so it can
scale across devices without anyone running a service. Producers write,
consumers read at their own pace. The folder is the API.

The motivating use case: when several AI agents and one human are
collaborating on related work across multiple projects, today they coordinate
by interrupting each other (one agent calls into another's terminal
mid-task). That's noisy and lossy. We want a queue-shaped alternative where
senders drop messages and recipients drain at their own rate, including
across machines without any sync service.

## How we work

We are *not* writing a spec ahead of implementation. We're building the
simplest possible thing, testing the DX manually, and iterating. The risk of
spec-first is that early bad decisions ossify and every later iteration has
to fight against them — and that's exactly what happened in the first round of
this repo. Lesson learned. From here on:

1. The folder convention is captured tersely in [LAYOUT.md](LAYOUT.md). That
   is the only document with binding force.
2. Implementation, CLI, README usage docs, sync mechanics — all of that is
   built minimally and iterated based on real DX testing.
3. The directing-agent (in the `pty` repo) drives DX testing in `/tmp` after
   each round and surfaces what's broken or awkward.
4. smalltalk-claude (formerly `coord-claude`, the implementing agent
   here) takes those findings and ships the smallest fix that
   addresses them.
5. No big PLAN.md. No big README written as a third-party spec. Those are
   premature.

## Values (firm)

These are the values that should guide every implementation choice. They are
not behavioral guarantees and they may have edge cases — but if a design
violates one of them, the design is suspect.

### Make conflicts impossible by construction

Every operation should be representable as a unique-filename create, a
rename, or an idempotent overwrite. Don't have two writers race for the same
path. Don't modify files in place except where the user explicitly opts into
it (e.g. a `.status` file with a single writer).

A corollary: **the signal channel must be separate from the content
channel.** Self-reference on shared channels is a footgun.

### Append-only and rename-only

Producers create new files with globally unique names. Consumers move files
between sub-folders (e.g. inbox → archive). Trim is a separate slow operation
that's the only place where outright deletion happens.

### The folder is the API

The convention should be readable enough that `cat`, `ls`, `mv`, `find` are
all you need to participate. Any language can implement a producer or
consumer. The CLI is ergonomics, not contract.

### Sync engine swappable

The producer/consumer code must not depend on which sync engine moves files
between machines. v0 targets plain bidirectional `rsync` (with a
post-rsync sweep step to handle renames — see LAYOUT.md). Future versions
might use Syncthing, rclone bisync, a CRDT, whatever. Every
sync-engine-specific assumption inside the convention itself is a bug.

### Single-device-per-identity

A given identity (`alice`, `pty-relay-claude`, etc.) is hosted on exactly one
machine. Multi-device-per-identity is out of scope. If we ever observe two
machines claiming the same identity, that's a bug to surface, not merge.

### Status is informational, not contractual

A consumer can publish a status (`available` / `busy` / `dnd`). Producers may
consult it to decide whether to fragment vs. batch. The protocol does not
block on status.

## Non-goals (v0)

- Not a daemon, server, or library. The folder is the API.
- Not real-time delivery. Sync is periodic.
- Not handling untrusted writers. All participants are cooperating peers.
- Not encryption at rest. Out of scope until we have a concrete need.
- Not a UI. CLI only; agents and humans use existing tools.
- Not multi-writer-per-identity.

## Pre-1.0

Pre-1.0. The convention can change between commits. The implementation
*will* change as we learn. Don't depend on stability yet.
