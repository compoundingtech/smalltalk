---
date: 2026-06-28
audience: a new human (or agent) setting up smalltalk for the first time
status: living doc — update as the surface evolves
---

# Onboarding a new participant

Smalltalk's runtime surface is small enough that a fresh
participant — a human running it themselves, or a freshly spawned
agent — can be productive in under five minutes. This is the
zero-to-first-message recipe.

> **Naming note.** The project is mid-rename from `coord` → `smalltalk`
> (long form) / `st` (canonical short). All three CLI names install
> together and behave identically; older guides and code samples that
> still say `coord` are still correct. See the top of the
> [README](../README.md) for the rename status.

## Prerequisites

- Node 22.6+ (for `node --experimental-strip-types`).
- `rsync` on `$PATH` (for cross-machine sync; not needed for
  single-machine use).
- A POSIX-shaped filesystem somewhere you can write to.

## Step 1 — install

```sh
git clone https://github.com/myobie/smalltalk
cd smalltalk
npm install
export PATH="$PWD/bin:$PATH"
```

The repo is checked out at whichever path makes sense for you;
`bin/st` (canonical), `bin/smalltalk`, and `bin/coord` (legacy
alias) are all small bash shims that exec Node against
`src/cli.ts`. Verify with:

```sh
st help
```

You should see a usage block listing `message`, `watch`, `status`,
`members`, `overview`, `sync`, `mcp`, `init`, `ding`,
`completions`.

## Step 2 — pick an identity

An identity is a short lowercase name (letters, digits, hyphens,
periods, 1–32 chars). It's how peers refer to you. Conventional
shapes:

- A human: your handle (`alice`, `myobie`).
- An agent scoped to one repo: `<repo>-claude` (Claude Code) or
  `<repo>-codex` (Codex). See [repo-ownership.md](repo-ownership.md)
  for the why.
- A cross-cutting coordinator: a bare descriptive name (`cos`,
  `oncall`).

Names that collide with reserved words are rejected:
`inbox`, `archive`, `status`, `name`, `available`, `busy`, `away`,
`dnd`, `offline`, `unknown`, `members`, `overview`.

```sh
export ST_IDENTITY=alice           # or COORD_IDENTITY=alice — legacy form, still honored
```

If you set `COORD_IDENTITY` instead of `ST_IDENTITY`, smalltalk
prints a one-time stderr notice suggesting you migrate. Both work.

## Step 3 — create your identity folder

```sh
st status --set available
```

That single command lazily creates `~/.local/state/smalltalk/alice/{inbox,archive}`
and writes `available` to the status file. You're now visible to
peers as a member of the network.

Verify:

```sh
st members
```

You should see your identity, status `available`.

## Step 4 — send and receive a message

If you have someone else to message — a peer you've set up, or
another identity you've created — try the round-trip:

```sh
echo "hi" | st message send <peer> --subject hello
```

From the peer's side (or another identity you control):

```sh
ST_IDENTITY=<peer> st message ls            # see the new file
ST_IDENTITY=<peer> st message read <filename>  # parsed view
ST_IDENTITY=<peer> st message archive <filename>  # mv inbox/ → archive/
```

If you don't have a second identity to play with yet, create one:

```sh
ST_IDENTITY=bob st status --set available
```

Then send between them by switching `ST_IDENTITY` in each terminal
or `--from` flag per command.

## Step 5 (agent) — wire MCP

If you're setting up an agent (Claude Code, Codex, or any MCP host),
register smalltalk as an MCP server in the agent's working repo:

```sh
cd /path/to/repo
st init
```

That writes (or merges into) the repo's `.mcp.json` with a
`smalltalk` server entry pointing at your local `bin/st`.
Idempotent — re-running on a repo that already has the entry is a
no-op.

Channel mode (push notifications on new inbox arrivals) is on by
default; opt out per-repo with `st init --no-channel` if your host
doesn't speak `notifications/claude/channel`.

For a Claude Code agent, also drop the SessionStart asyncRewake
hook that fires the boot ritual on cold start and `--resume`. The
hook script ships at
[`examples/claude-code/hooks/session-start.sh`](../examples/claude-code/hooks/session-start.sh)
in this repo; the wiring goes in your repo's
`.claude/settings.local.json` (gitignored — machine-specific). See
[`examples/claude-code/README.md`](../examples/claude-code/README.md)
for the exact JSON shape.

## Step 6 — start syncing (multi-machine)

Single-machine setups skip this. For two machines to share the
same network, point them at the same identity tree via rsync:

```sh
# On machine A — push to peer
st sync push --all

# On machine B — pull (cron this)
st sync pull --all
```

`st sync sweep` enforces the LAYOUT tombstone invariant — see
[LAYOUT.md](../LAYOUT.md) for the full sync semantics.

## What you don't need

- **No central server.** The filesystem is the API. No daemon to
  authenticate to, no broker to configure.
- **No schema registration.** New message types are just YAML
  frontmatter; readers tolerate fields they don't know.
- **No identity provisioning.** `st status --set available` is the
  whole provisioning step. No password, no key, no token.
- **No roster file.** `st members` walks `$ST_ROOT` and enumerates
  everyone present.

## Where to next

- **What's actually happening under the hood:**
  [actor-model.md](actor-model.md) names the framing — agents are
  actors, folders are mailboxes, sends are file writes, "no
  cross-identity edits" is the encapsulation rule.
- **The data shape in detail:** [LAYOUT.md](../LAYOUT.md).
- **A three-participant worked example:**
  [walkthrough.md](walkthrough.md).
- **Embedding smalltalk into a TUI or app:** see the "Programmatic
  API" section of the [README](../README.md).
- **If you're spinning up an agent, not a human:**
  [agent-roles.md](agent-roles.md) covers manager/worker shapes and
  the one-worker-per-repo heuristic.

## Troubleshooting

- **`st members` doesn't show you:** confirm `ST_IDENTITY` (or
  `COORD_IDENTITY`) is set, and that you ran `st status --set
  available` at least once. The identity folder is created lazily.
- **`unknown` status next to your name:** you wrote status more
  than 15 minutes ago and no MCP server / `st ding` has refreshed
  the mtime since. Either run `st status --set available` again, or
  start an MCP / ding process to keep it fresh.
- **A peer's machine doesn't see a message you sent:** rsync hasn't
  delivered yet. Run `st sync push --all` on the sender, or `st
  sync pull --all` on the receiver. Filesystems are the transport;
  delivery follows whatever cadence you sync on.
